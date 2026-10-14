//! Read-only catalog capture and CSV export. AFC source bounds do not bound the
//! pinned transport's allocation for a malformed frame. Stable attributes are
//! checked across all three SQLite files; this is not an atomic device snapshot.
use anyhow::{Context, Result, ensure};
use idevice::afc::AfcClient;
use music_protocol::protocol::afc::copy;
use music_protocol::protocol::afc::stat::{self as afc_stat, regular_size, unchanged};
use music_protocol::protocol::trace::private_write;
use music_protocol::protocol::transfer::{CappedMetadata, CaptureControl, bounded_error};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{DirBuilder, File, OpenOptions},
    future::Future,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    time::{Duration, SystemTime},
};
use tokio::time::{Instant, timeout_at};

const FILES: [&str; 3] = [
    "MediaLibrary.sqlitedb",
    "MediaLibrary.sqlitedb-wal",
    "MediaLibrary.sqlitedb-shm",
];
const COPY_CAP: u64 = 32 * 1024 * 1024;
/// The untested user_version last warned about, so that a command capturing
/// the catalogue several times warns once.
static WARNED_VERSION: AtomicI64 = AtomicI64::new(0);
const MAX_READS: usize = 4096;
type Attributes = BTreeMap<String, String>;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Only a fully read, closed capture with valid cross-file attribute drift is
/// retryable. Transport, parser, filesystem and unsupported-shape errors never
/// acquire this type, even when their message mentions changing files.
#[derive(Debug)]
pub(crate) struct CatalogCaptureDrift {
    changed_files: Vec<&'static str>,
}
impl CatalogCaptureDrift {
    pub(crate) fn changed_files(&self) -> &[&'static str] {
        &self.changed_files
    }
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self {
            changed_files: vec![FILES[1]],
        }
    }
}
impl std::fmt::Display for CatalogCaptureDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("catalog files changed across capture; no catalog exported")
    }
}
impl std::error::Error for CatalogCaptureDrift {}

struct CaptureDirectory {
    path: PathBuf,
    retain: bool,
}
impl CaptureDirectory {
    fn create(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_owned(),
            None => std::env::temp_dir().join(format!(
                "iphone-music-catalog-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_nanos(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            )),
        };
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .context("new private catalog capture directory required")?;
        Ok(Self {
            path,
            retain: explicit.is_some(),
        })
    }
}

struct Captured {
    database: PathBuf,
    wal: Option<PathBuf>,
    hashes: Vec<(String, u64, [u8; 32])>,
}
fn check_control(control: Option<&CaptureControl>) -> Result<()> {
    if let Some(control) = control {
        control.check()?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn parse_copied_snapshot(
    database: &Path,
    wal: Option<&Path>,
) -> Result<music_protocol::catalogue::sqlite::Catalog> {
    parse_copied_snapshot_options(database, wal, None)
}
/// SQLite's NOFOLLOW rejects /proc/self/fd paths. Retained source files may use
/// held descriptors, but SQLite receives only its usual disposable copies in
/// a separate private canonical scratch directory. Parser and hash errors are
/// terminal: they never become `CatalogCaptureDrift`.
fn parse_copied_snapshot_options(
    database: &Path,
    wal: Option<&Path>,
    control: Option<&CaptureControl>,
) -> Result<music_protocol::catalogue::sqlite::Catalog> {
    check_control(control)?;
    let scratch = CaptureDirectory::create(None)?;
    let path = std::fs::canonicalize(&scratch.path)?;
    let held = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&path)?;
    let before = held.metadata()?;
    check_control(control)?;
    let result = if let Some(control) = control {
        music_protocol::catalogue::sqlite::read_snapshot_current_with_control(
            database,
            wal,
            &path,
            control.deadline.into_std(),
            control.cancelled.clone(),
        )
    } else {
        music_protocol::catalogue::sqlite::read_snapshot(database, wal, &path)
    };
    let after = std::fs::symlink_metadata(&path)?;
    ensure!(
        after.is_dir() && before.dev() == after.dev() && before.ino() == after.ino(),
        "private catalog analysis directory changed; retained without cleanup"
    );
    // read_snapshot has dropped its own bounded child scratch before returning.
    // Only remove this empty parent, never a recursive pathname selected later.
    std::fs::remove_dir(&path).context("remove empty private catalog analysis directory")?;
    check_control(control)?;
    result
}

async fn capture_client(
    mut afc: AfcClient,
    trace: &Path,
    deadline: Instant,
    control: Option<&CaptureControl>,
) -> Result<Captured> {
    check_control(control)?;
    let before = attributes(&mut afc, trace, "before", deadline, control).await?;
    let database_size = selected_size(&before[0], true)?.unwrap();
    let wal_size = selected_size(&before[1], false)?;
    if let Some(attrs) = &before[2] {
        regular_size(attrs)?;
    }
    let mut hashes = Vec::new();
    for (index, size) in [Some(database_size), wal_size].into_iter().enumerate() {
        let Some(size) = size else { continue };
        check_control(control)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(trace.join(FILES[index]))?;
        let mut copied_output = HashedFile {
            file,
            digest: Sha256::new(),
            bytes: 0,
        };
        let (copied, closed) = copy::read_file(
            &mut afc,
            &remote(FILES[index]),
            &mut copied_output,
            size,
            copy::CopyLimits {
                maximum_size: COPY_CAP,
                maximum_reads: MAX_READS,
            },
            deadline,
            control.map(|control| control.cancelled.clone()),
            "open catalog file read-only",
        )
        .await?;
        let mut status = Dictionary::new();
        status.insert("Name".into(), FILES[index].into());
        status.insert("CopiedBytes".into(), copied_output.bytes.into());
        status.insert(
            "Copy".into(),
            copied
                .as_ref()
                .map(|_| "complete".to_owned())
                .unwrap_or_else(|failure| {
                    if control.is_some() {
                        bounded_error(&failure.error)
                    } else {
                        format!("{:#}", failure.error)
                    }
                })
                .into(),
        );
        status.insert(
            "Close".into(),
            match &closed {
                Some(Ok(())) => "acknowledged".to_owned(),
                Some(Err(failure)) => format!("{:#}", failure.error),
                None => "not attempted: uncertain transport discarded".to_owned(),
            }
            .into(),
        );
        save_plist_options(
            &trace.join(format!("copy-{index}-status.plist")),
            &Value::Dictionary(status),
            control,
        )?;
        copied.map_err(|failure| failure.error)?;
        closed
            .context("catalog handle close was not attempted")?
            .map_err(|failure| failure.error)?;
        ensure!(copied_output.bytes == size, "copied catalog size mismatch");
        check_control(control)?;
        copied_output.file.sync_all()?;
        check_control(control)?;
        let digest: [u8; 32] = copied_output.digest.finalize().into();
        hashes.push((FILES[index].to_owned(), size, digest));
    }
    let after = attributes(&mut afc, trace, "after", deadline, control).await?;
    let receipts = hashes
        .iter()
        .map(|(name, size, digest)| {
            let mut row = Dictionary::new();
            row.insert("Name".into(), name.clone().into());
            row.insert("Bytes".into(), (*size).into());
            row.insert("SHA256".into(), hex(digest).into());
            Value::Dictionary(row)
        })
        .collect();
    save_plist_options(
        &trace.join("copies.plist"),
        &Value::Array(receipts),
        control,
    )?;
    check_control(control)?;
    check_capture_stability(&before, &after)?;
    Ok(Captured {
        database: trace.join(FILES[0]),
        wal: wal_size.map(|_| trace.join(FILES[1])),
        hashes,
    })
}

fn check_capture_stability(
    before: &[Option<Attributes>],
    after: &[Option<Attributes>],
) -> Result<()> {
    ensure!(
        before.len() == FILES.len() && after.len() == FILES.len(),
        "invalid catalog attribute set"
    );
    for set in [before, after] {
        selected_size(&set[0], true)?;
        selected_size(&set[1], false)?;
        if let Some(attrs) = &set[2] {
            regular_size(attrs)?;
        }
    }
    let changed_files: Vec<_> = FILES
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let same = match (&before[index], &after[index]) {
                (Some(before), Some(after)) => unchanged(before, after),
                (None, None) => true,
                _ => false,
            };
            (!same).then_some(*name)
        })
        .collect();
    if !changed_files.is_empty() {
        return Err(CatalogCaptureDrift { changed_files }.into());
    }
    Ok(())
}

async fn attributes(
    afc: &mut AfcClient,
    trace: &Path,
    phase: &str,
    deadline: Instant,
    control: Option<&CaptureControl>,
) -> Result<Vec<Option<Attributes>>> {
    let mut captured = Vec::new();
    for (index, name) in FILES.iter().enumerate() {
        let path = remote(name);
        let result = if control.is_some() {
            read_attributes_options(afc, &path, deadline, control).await
        } else {
            read_attributes(afc, &path, deadline).await
        };
        let mut record = Dictionary::new();
        record.insert("Path".into(), path.into());
        match &result {
            Ok(Some(attrs)) => {
                record.insert(
                    "Attributes".into(),
                    Value::Dictionary(
                        attrs
                            .iter()
                            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                            .collect(),
                    ),
                );
            }
            Ok(None) => {
                record.insert("Status".into(), "ObjectNotFound".into());
            }
            Err(error) => {
                record.insert(
                    "Error".into(),
                    (if control.is_some() {
                        bounded_error(error)
                    } else {
                        format!("{error:#}")
                    })
                    .into(),
                );
            }
        }
        save_plist_options(
            &trace.join(format!("{phase}-{index}-attributes.plist")),
            &Value::Dictionary(record),
            control,
        )?;
        captured.push(result?);
    }
    Ok(captured)
}

async fn read_attributes(
    afc: &mut AfcClient,
    path: &str,
    deadline: Instant,
) -> Result<Option<Attributes>> {
    read_attributes_options(afc, path, deadline, None).await
}
async fn read_attributes_options(
    afc: &mut AfcClient,
    path: &str,
    deadline: Instant,
    control: Option<&CaptureControl>,
) -> Result<Option<Attributes>> {
    check_control(control)?;
    afc_stat::attributes(
        afc,
        path,
        deadline,
        control.map(|control| control.cancelled.clone()),
        "read catalog attributes",
    )
    .await
}

struct HashedFile {
    file: File,
    digest: Sha256,
    bytes: u64,
}
impl Write for HashedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.file.write(bytes)?;
        self.digest.update(&bytes[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
fn selected_size(attrs: &Option<Attributes>, required: bool) -> Result<Option<u64>> {
    let Some(attrs) = attrs else {
        ensure!(!required, "device MediaLibrary database is absent");
        return Ok(None);
    };
    let size = regular_size(attrs)?;
    ensure!(
        size <= COPY_CAP && (!required || size > 0),
        "catalog database/WAL size unsupported (32 MiB cap per file)"
    );
    Ok(Some(size))
}
fn remote(name: &str) -> String {
    format!("iTunes_Control/iTunes/{name}")
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn save_plist(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = Vec::new();
    value.to_writer_xml(&mut bytes)?;
    private_write(path, &bytes)
}
fn save_plist_options(path: &Path, value: &Value, control: Option<&CaptureControl>) -> Result<()> {
    if control.is_some() {
        let mut bytes = CappedMetadata::new();
        value.to_writer_xml(&mut bytes)?;
        private_write(path, bytes.bytes())
    } else {
        save_plist(path, value)
    }
}
fn write_csv(
    output: &mut impl Write,
    catalog: &music_protocol::catalogue::sqlite::Catalog,
) -> Result<()> {
    csv_row(
        output,
        &[
            "wire_id",
            "title",
            "artist",
            "album",
            "album_artist",
            "track_number",
            "track_count",
            "disc_number",
            "disc_count",
            "year",
            "duration_ms",
            "size_bytes",
            "path",
        ],
    )?;
    for track in catalog.tracks() {
        let meta = &track.metadata;
        csv_row(
            output,
            &[
                &(track.sync_id as i64).to_string(),
                &meta.title,
                &meta.artist,
                &meta.album,
                &meta.album_artist,
                &meta.track_number.to_string(),
                &meta.track_count.to_string(),
                &meta.disc_number.to_string(),
                &meta.disc_count.to_string(),
                &meta.year.to_string(),
                &meta.total_time_ms.to_string(),
                &meta.file_size.to_string(),
                track.afc_path.as_deref().unwrap_or(""),
            ],
        )?;
    }
    Ok(())
}
fn csv_row(output: &mut impl Write, fields: &[&str]) -> Result<()> {
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        if field.contains([',', '"', '\r', '\n']) {
            output.write_all(b"\"")?;
            for part in field.split_inclusive('"') {
                output.write_all(part.as_bytes())?;
                if part.ends_with('"') {
                    output.write_all(b"\"")?;
                }
            }
            output.write_all(b"\"")?;
        } else {
            output.write_all(field.as_bytes())?;
        }
    }
    output.write_all(b"\r\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use idevice::{
        Idevice,
        afc::{
            MAGIC,
            opcode::AfcOpcode,
            packet::{AfcPacket, AfcPacketHeader},
        },
    };
    use std::sync::atomic::AtomicBool;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(CaptureDirectory::create(None).unwrap().path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn csv_quotes_unicode_commas_quotes_and_newlines_without_losing_fields() {
        let mut output = Vec::new();
        csv_row(
            &mut output,
            &[
                "-42",
                "Νύχτα, Night",
                "A \"B\"",
                "first\r\nsecond",
                "",
                "plain",
            ],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "-42,\"Νύχτα, Night\",\"A \"\"B\"\"\",\"first\r\nsecond\",,plain\r\n"
        );
    }

    fn attrs(size: u64) -> Attributes {
        Attributes::from([
            ("st_ifmt".into(), "S_IFREG".into()),
            ("st_size".into(), size.to_string()),
            ("st_mtime".into(), "123".into()),
            ("st_birthtime".into(), "100".into()),
        ])
    }

    #[test]
    fn sizes_and_cross_file_identity_are_guarded_but_access_time_may_change() {
        assert!(selected_size(&None, true).is_err());
        assert_eq!(selected_size(&None, false).unwrap(), None);
        assert_eq!(selected_size(&Some(attrs(0)), false).unwrap(), Some(0));
        assert!(selected_size(&Some(attrs(0)), true).is_err());
        assert!(selected_size(&Some(attrs(COPY_CAP + 1)), false).is_err());
        let before = attrs(5);
        let mut after = before.clone();
        after.insert("st_atime".into(), "456".into());
        assert!(unchanged(&before, &after));
        after.insert("st_birthtime".into(), "101".into());
        assert!(!unchanged(&before, &after));
        for key in ["st_size", "st_mtime", "st_ifmt"] {
            let mut invalid = before.clone();
            invalid.remove(key);
            assert!(regular_size(&invalid).is_err());
        }
    }

    #[test]
    fn only_valid_completed_cross_file_drift_gets_retry_type() {
        let before = vec![
            Some(attrs(2_609_152)),
            Some(attrs(53_592)),
            Some(attrs(32768)),
        ];
        let mut after = before.clone();
        after[1] = Some(attrs(61_832));
        after[1]
            .as_mut()
            .unwrap()
            .insert("st_mtime".into(), "124".into());
        let error = check_capture_stability(&before, &after).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<CatalogCaptureDrift>()
                .unwrap()
                .changed_files(),
            &["MediaLibrary.sqlitedb-wal"]
        );
        for field in ["st_ifmt", "st_size", "st_mtime"] {
            let mut invalid = after.clone();
            invalid[1].as_mut().unwrap().remove(field);
            assert!(
                check_capture_stability(&before, &invalid)
                    .unwrap_err()
                    .downcast_ref::<CatalogCaptureDrift>()
                    .is_none()
            );
        }
        let mut oversized = after.clone();
        oversized[1] = Some(attrs(COPY_CAP + 1));
        assert!(
            check_capture_stability(&before, &oversized)
                .unwrap_err()
                .downcast_ref::<CatalogCaptureDrift>()
                .is_none()
        );
        let mut gone = after.clone();
        gone[1] = None;
        assert_eq!(
            check_capture_stability(&before, &gone)
                .unwrap_err()
                .downcast_ref::<CatalogCaptureDrift>()
                .unwrap()
                .changed_files(),
            &["MediaLibrary.sqlitedb-wal"]
        );
        let mut missing = after.clone();
        missing[0] = None;
        assert!(
            check_capture_stability(&before, &missing)
                .unwrap_err()
                .downcast_ref::<CatalogCaptureDrift>()
                .is_none()
        );
        let mut accessed = before.clone();
        accessed[1]
            .as_mut()
            .unwrap()
            .insert("st_atime".into(), "456".into());
        check_capture_stability(&before, &accessed).unwrap();
    }

    fn pair() -> (AfcClient, Idevice) {
        let (client, peer) = tokio::io::duplex(copy::CHUNK * 2);
        (
            AfcClient::new(Idevice::new(Box::new(client), "offline-only")),
            Idevice::new(Box::new(peer), "offline-peer"),
        )
    }
    async fn reply(
        peer: &mut Idevice,
        request: &AfcPacket,
        operation: AfcOpcode,
        header_payload: Vec<u8>,
        payload: Vec<u8>,
    ) {
        let header_payload_len = AfcPacketHeader::LEN + header_payload.len() as u64;
        let packet = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: header_payload_len + payload.len() as u64,
                header_payload_len,
                packet_num: request.header.packet_num,
                operation,
            },
            header_payload,
            payload,
        };
        peer.send_raw(&packet.serialize()).await.unwrap();
    }
    async fn stat_replies(peer: &mut Idevice, present_wal: bool, changed: bool) {
        for (index, name) in FILES.iter().enumerate() {
            let request = AfcPacket::read(peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::GetFileInfo);
            assert_eq!(request.header_payload, remote(name).as_bytes());
            assert!(request.payload.is_empty());
            if index == 1 && !present_wal {
                reply(
                    peer,
                    &request,
                    AfcOpcode::Status,
                    8u64.to_le_bytes().to_vec(),
                    vec![],
                )
                .await;
            } else {
                let mut values = attrs([8, 3, 32768][index]);
                if changed && index == 1 {
                    values.insert("st_mtime".into(), "124".into());
                }
                let payload = values
                    .iter()
                    .flat_map(|(key, value)| {
                        [key.as_bytes(), &[0], value.as_bytes(), &[0]].concat()
                    })
                    .collect();
                reply(peer, &request, AfcOpcode::Data, vec![], payload).await;
            }
        }
    }
    async fn open_reply(peer: &mut Idevice, index: usize) {
        let request = AfcPacket::read(peer).await.unwrap();
        assert_eq!(request.header.operation, AfcOpcode::FileOpen);
        assert_eq!(
            request.header_payload,
            [
                1u64.to_le_bytes().as_slice(),
                remote(FILES[index]).as_bytes()
            ]
            .concat()
        );
        assert!(request.payload.is_empty());
        reply(
            peer,
            &request,
            AfcOpcode::FileOpenRes,
            42u64.to_le_bytes().to_vec(),
            vec![],
        )
        .await;
    }
    async fn copy_reply(peer: &mut Idevice, index: usize) {
        open_reply(peer, index).await;
        let chunks = if index == 0 {
            vec![b"abc".as_slice(), b"defgh".as_slice()]
        } else {
            vec![b"wal".as_slice()]
        };
        let mut remaining = if index == 0 { 8 } else { 3 };
        for bytes in chunks {
            let request = AfcPacket::read(peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::Read);
            assert_eq!(
                request.header_payload,
                [42u64.to_le_bytes(), (remaining as u64).to_le_bytes()].concat()
            );
            reply(peer, &request, AfcOpcode::Data, vec![], bytes.to_vec()).await;
            remaining -= bytes.len();
        }
        let request = AfcPacket::read(peer).await.unwrap();
        assert_eq!(request.header.operation, AfcOpcode::FileClose);
        assert_eq!(request.header_payload, 42u64.to_le_bytes());
        reply(peer, &request, AfcOpcode::Status, vec![0; 8], vec![]).await;
    }

    #[tokio::test]
    async fn capture_binds_short_chunk_bytes_hashes_and_cross_file_attributes() {
        for (present_wal, changed) in [(true, false), (false, false), (true, true)] {
            let temp = Temp::new();
            let (afc, mut peer) = pair();
            let server = tokio::spawn(async move {
                stat_replies(&mut peer, present_wal, false).await;
                copy_reply(&mut peer, 0).await;
                if present_wal {
                    copy_reply(&mut peer, 1).await;
                }
                stat_replies(&mut peer, present_wal, changed).await;
                assert!(peer.read_raw(1).await.is_err());
            });
            let result =
                capture_client(afc, &temp.0, Instant::now() + Duration::from_secs(2), None).await;
            if changed {
                let error = result.err().unwrap();
                assert!(error.to_string().contains("changed across capture"));
                assert!(error.downcast_ref::<CatalogCaptureDrift>().is_some());
            } else {
                let snapshot = result.unwrap();
                assert_eq!(snapshot.hashes[0].1, 8);
                assert_eq!(
                    snapshot.hashes[0].2,
                    <[u8; 32]>::from(Sha256::digest(b"abcdefgh"))
                );
                assert_eq!(snapshot.wal.is_some(), present_wal);
                assert_eq!(snapshot.hashes.len(), if present_wal { 2 } else { 1 });
            }
            assert_eq!(std::fs::read(temp.0.join(FILES[0])).unwrap(), b"abcdefgh");
            assert!(temp.0.join("after-2-attributes.plist").is_file());
            assert!(temp.0.join("copies.plist").is_file());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn timeout_discards_open_connection_and_retains_partial_capture_without_close() {
        let temp = Temp::new();
        let (afc, mut peer) = pair();
        let server = tokio::spawn(async move {
            stat_replies(&mut peer, true, false).await;
            open_reply(&mut peer, 0).await;
            let request = AfcPacket::read(&mut peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::Read);
            reply(
                &mut peer,
                &request,
                AfcOpcode::Data,
                vec![],
                b"abc".to_vec(),
            )
            .await;
            let request = AfcPacket::read(&mut peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::Read);
            assert!(peer.read_raw(1).await.is_err());
        });
        let error = capture_client(
            afc,
            &temp.0,
            Instant::now() + Duration::from_millis(100),
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(error.downcast_ref::<CatalogCaptureDrift>().is_none());
        assert_eq!(std::fs::read(temp.0.join(FILES[0])).unwrap(), b"abc");
        let status = Value::from_file(temp.0.join("copy-0-status.plist")).unwrap();
        assert_eq!(
            status.as_dictionary().unwrap()["Close"].as_string(),
            Some("not attempted: uncertain transport discarded")
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn controlled_catalogue_partial_copy_original_arc_cancellation_has_no_close() {
        let temp = Temp::new();
        let (afc, mut peer) = pair();
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let control = CaptureControl {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: flag.clone(),
        };
        let server = tokio::spawn(async move {
            stat_replies(&mut peer, true, false).await;
            open_reply(&mut peer, 0).await;
            let request = AfcPacket::read(&mut peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::Read);
            reply(
                &mut peer,
                &request,
                AfcOpcode::Data,
                vec![],
                b"abc".to_vec(),
            )
            .await;
            let request = AfcPacket::read(&mut peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::Read);
            flag.store(true, Ordering::Relaxed);
            assert!(peer.read_raw(1).await.is_err());
        });
        let result = capture_client(afc, &temp.0, control.deadline, Some(&control)).await;
        assert!(result.is_err());
        server.await.unwrap();
        assert_eq!(std::fs::read(temp.0.join(FILES[0])).unwrap(), b"abc");
        assert!(!temp.0.join(FILES[1]).exists());
        let status = Value::from_file(temp.0.join("copy-0-status.plist")).unwrap();
        assert_eq!(
            status.as_dictionary().unwrap()["Close"].as_string(),
            Some("not attempted: uncertain transport discarded")
        );
        for entry in std::fs::read_dir(&temp.0).unwrap() {
            let entry = entry.unwrap();
            if entry.path().extension().is_some_and(|ext| ext == "plist") {
                assert!(
                    entry.metadata().unwrap().len()
                        <= music_protocol::protocol::transfer::CURRENT_METADATA_LIMIT as u64
                );
            }
        }
    }

    #[tokio::test]
    async fn controlled_complete_catalogue_still_qualifies_only_real_cross_file_drift() {
        let temp = Temp::new();
        let (afc, mut peer) = pair();
        let control = CaptureControl {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: std::sync::Arc::new(AtomicBool::new(false)),
        };
        let server = tokio::spawn(async move {
            stat_replies(&mut peer, true, false).await;
            copy_reply(&mut peer, 0).await;
            copy_reply(&mut peer, 1).await;
            stat_replies(&mut peer, true, true).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        let error = capture_client(afc, &temp.0, control.deadline, Some(&control))
            .await
            .err()
            .unwrap();
        server.await.unwrap();
        assert!(error.downcast_ref::<CatalogCaptureDrift>().is_some());
        assert!(temp.0.join("copies.plist").is_file());
        assert!(temp.0.join("copy-0-status.plist").is_file());
        assert!(temp.0.join("copy-1-status.plist").is_file());
    }
}
