use super::*;
use idevice::{
    Idevice,
    afc::{
        MAGIC,
        opcode::{AfcFopenMode, AfcOpcode},
        packet::{AfcPacket, AfcPacketHeader},
    },
};
use music_protocol::protocol::afc::copy;
use std::sync::Arc;

const AUDIO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/silence-alac.m4a"
));
const MUSIC: &str = "iTunes_Control/Music/F39/TEST.m4a";
const DATABASE: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb";
const WAL: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal";
const SHM: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb-shm";

#[derive(Clone, Copy)]
enum Scenario {
    Normal,
    InitialWalTruncated,
    InitialWalMissing,
    InitialMalformedWal,
    InitialMalformedShm,
    InitialWalIo,
    InitialCancel,
    InitialStall,
    InitialHashGuardAttributeDrift,
    MissingAudio,
    ChangedAudio,
    FinalCatalogDrift,
    Eof,
    BadClose,
    CancelRead,
    StallRead,
    UnsupportedAudio,
    ChangedAfterFingerprint,
    /// The phone trimmed the file's allocation after it was fingerprinted.
    TrimmedAfterFingerprint,
    Mixed,
    /// The audio file was rewritten since an earlier pass: a new mtime.
    Rewritten,
}

fn identity() -> IndexIdentity {
    IndexIdentity {
        device_identifier_sha256: [1; 32],
        library_identity_sha256: [2; 32],
    }
}
fn binding() -> CatalogBinding {
    CatalogBinding {
        database_sha256: Sha256::digest(b"database").into(),
        database_bytes: 8,
        wal_sha256: Some(Sha256::digest(b"wal").into()),
        wal_bytes: Some(3),
        persisted_media_anchor: 13,
    }
}
fn plan(bytes: u64) -> Vec<Target> {
    vec![Target {
        sync_id: 1,
        path: MUSIC.into(),
        bytes,
    }]
}

async fn reply(
    peer: &mut Idevice,
    request: &AfcPacket,
    op: AfcOpcode,
    header: Vec<u8>,
    data: Vec<u8>,
) {
    let header_len = AfcPacketHeader::LEN + header.len() as u64;
    let packet = AfcPacket {
        header: AfcPacketHeader {
            magic: MAGIC,
            entire_len: header_len + data.len() as u64,
            header_payload_len: header_len,
            packet_num: request.header.packet_num,
            operation: op,
        },
        header_payload: header,
        payload: data,
    };
    peer.send_raw(&packet.serialize()).await.unwrap();
}

fn attributes(size: usize, mtime: u32, atime: u32) -> Vec<u8> {
    format!(
        "st_ifmt\0S_IFREG\0st_size\0{size}\0st_mtime\0{mtime}\0st_birthtime\01\0st_atime\0{atime}\0"
    )
    .into_bytes()
}

fn endpoint(
    scenario: Scenario,
    cancelled: Arc<AtomicBool>,
) -> (AfcClient, tokio::task::JoinHandle<Vec<AfcOpcode>>) {
    let (client, peer) = tokio::io::duplex(copy::CHUNK * 2);
    let afc = AfcClient::new(Idevice::new(Box::new(client), "offline-index-test"));
    let task = tokio::spawn(async move {
        let mut peer = Idevice::new(Box::new(peer), "offline-index-peer");
        let mut files = BTreeMap::from([
            (DATABASE.to_owned(), b"database".to_vec()),
            (
                WAL.to_owned(),
                if matches!(
                    scenario,
                    Scenario::InitialWalTruncated
                        | Scenario::InitialMalformedWal
                        | Scenario::InitialMalformedShm
                ) {
                    Vec::new()
                } else {
                    b"wal".to_vec()
                },
            ),
            (SHM.to_owned(), vec![0; 8]),
            (
                MUSIC.to_owned(),
                if matches!(scenario, Scenario::Mixed) {
                    let mut frame = vec![0; 417];
                    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
                    [frame.clone(), frame].concat()
                } else if matches!(scenario, Scenario::UnsupportedAudio) {
                    b"invalid".to_vec()
                } else {
                    AUDIO.to_vec()
                },
            ),
        ]);
        files.insert("iTunes_Control/Music/F39/OTHER.m4a".into(), AUDIO.to_vec());
        let mut stats = BTreeMap::<String, u32>::new();
        let mut handles = BTreeMap::<u64, (String, usize)>::new();
        let mut next_handle = 1u64;
        let mut operations = Vec::new();
        loop {
            let Ok(request) = AfcPacket::read(&mut peer).await else {
                break;
            };
            operations.push(request.header.operation);
            assert!(request.payload.is_empty());
            match request.header.operation {
                AfcOpcode::GetFileInfo => {
                    let path = std::str::from_utf8(&request.header_payload)
                        .unwrap()
                        .to_owned();
                    let count = stats.entry(path.clone()).or_default();
                    *count += 1;
                    if path == WAL && matches!(scenario, Scenario::InitialStall) {
                        assert!(peer.read_raw(1).await.is_err());
                        break;
                    }
                    if path == WAL && matches!(scenario, Scenario::InitialCancel) {
                        cancelled.store(true, Ordering::Relaxed);
                    }
                    if path == WAL
                        && matches!(
                            scenario,
                            Scenario::InitialWalMissing | Scenario::InitialWalIo
                        )
                    {
                        let status = if matches!(scenario, Scenario::InitialWalMissing) {
                            8u64
                        } else {
                            1
                        };
                        reply(
                            &mut peer,
                            &request,
                            AfcOpcode::Status,
                            status.to_le_bytes().to_vec(),
                            vec![],
                        )
                        .await;
                        continue;
                    }
                    if (path == WAL && matches!(scenario, Scenario::InitialMalformedWal))
                        || (path == SHM && matches!(scenario, Scenario::InitialMalformedShm))
                    {
                        reply(
                            &mut peer,
                            &request,
                            AfcOpcode::Data,
                            vec![],
                            b"st_ifmt\0S_IFREG\0st_size\00\0".to_vec(),
                        )
                        .await;
                        continue;
                    }
                    if path == MUSIC && matches!(scenario, Scenario::MissingAudio) {
                        reply(
                            &mut peer,
                            &request,
                            AfcOpcode::Status,
                            8u64.to_le_bytes().to_vec(),
                            vec![],
                        )
                        .await;
                        continue;
                    }
                    if path == DATABASE
                        && *count == 3
                        && matches!(scenario, Scenario::FinalCatalogDrift)
                    {
                        files.get_mut(DATABASE).unwrap()[0] ^= 1;
                    }
                    let mtime = if (path == DATABASE
                        && *count == 2
                        && matches!(scenario, Scenario::InitialHashGuardAttributeDrift))
                        || (path == MUSIC
                            && ((*count == 2 && matches!(scenario, Scenario::ChangedAudio))
                                || (*count == 3
                                    && matches!(scenario, Scenario::ChangedAfterFingerprint))))
                    {
                        2
                    } else if path == MUSIC && matches!(scenario, Scenario::Rewritten) {
                        3
                    } else {
                        1
                    };
                    let mut info = attributes(files[&path].len(), mtime, *count);
                    if path == MUSIC && matches!(scenario, Scenario::TrimmedAfterFingerprint) {
                        let blocks = if *count >= 3 { 8 } else { 16 };
                        info.extend(format!("st_blocks\0{blocks}\0").bytes());
                    }
                    reply(&mut peer, &request, AfcOpcode::Data, vec![], info).await;
                }
                AfcOpcode::FileOpen => {
                    assert_eq!(
                        &request.header_payload[..8],
                        &(AfcFopenMode::RdOnly as u64).to_le_bytes()
                    );
                    let path = std::str::from_utf8(&request.header_payload[8..])
                        .unwrap()
                        .to_owned();
                    assert!(files.contains_key(&path));
                    handles.insert(next_handle, (path, 0));
                    reply(
                        &mut peer,
                        &request,
                        AfcOpcode::FileOpenRes,
                        next_handle.to_le_bytes().to_vec(),
                        vec![],
                    )
                    .await;
                    next_handle += 1;
                }
                AfcOpcode::Read => {
                    assert_eq!(request.header_payload.len(), 16);
                    let handle =
                        u64::from_le_bytes(request.header_payload[..8].try_into().unwrap());
                    let want = u64::from_le_bytes(request.header_payload[8..].try_into().unwrap())
                        as usize;
                    assert!((1..=copy::CHUNK).contains(&want));
                    let (path, offset) = handles.get_mut(&handle).unwrap();
                    if path == MUSIC
                        && matches!(scenario, Scenario::CancelRead | Scenario::StallRead)
                    {
                        if matches!(scenario, Scenario::CancelRead) {
                            cancelled.store(true, Ordering::Relaxed);
                        }
                        assert!(peer.read_raw(1).await.is_err());
                        break;
                    }
                    let count =
                        if path == MUSIC && *offset != 0 && matches!(scenario, Scenario::Eof) {
                            0
                        } else {
                            want.min(128).min(files[path].len() - *offset)
                        };
                    let data = files[path][*offset..*offset + count].to_vec();
                    *offset += count;
                    reply(&mut peer, &request, AfcOpcode::Data, vec![], data).await;
                }
                AfcOpcode::FileClose => {
                    let handle = u64::from_le_bytes(request.header_payload[..].try_into().unwrap());
                    let (path, _) = handles.remove(&handle).unwrap();
                    let status = if path == MUSIC && matches!(scenario, Scenario::BadClose) {
                        1u64
                    } else {
                        0
                    };
                    reply(
                        &mut peer,
                        &request,
                        AfcOpcode::Status,
                        status.to_le_bytes().to_vec(),
                        vec![],
                    )
                    .await;
                }
                other => panic!("forbidden read-only-index opcode {other:?}"),
            }
        }
        operations
    });
    (afc, task)
}

/// Exercise the real initial and final AFC guards with a WAL truncated at the
/// handoff. The second fresh binding independently expects the empty WAL hash.
pub(crate) async fn truncated_wal_attempt(
    fresh_binding: bool,
    parent: &Path,
    control: &ReadControl<'_>,
    mut progress: impl FnMut(IndexProgress) -> Result<()>,
) -> Result<()> {
    let (afc, server) = endpoint(
        Scenario::InitialWalTruncated,
        Arc::new(AtomicBool::new(false)),
    );
    let mut expected = binding();
    if fresh_binding {
        expected.wal_bytes = Some(0);
        expected.wal_sha256 = Some(Sha256::digest([]).into());
    }
    let result = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        expected,
        parent,
        control,
        &mut progress,
    )
    .await
    .map(|_| ());
    let operations = server.await.unwrap();
    if !fresh_binding {
        assert_eq!(operations, vec![AfcOpcode::GetFileInfo; 3]);
    }
    result
}

/// A pass over one track whose catalogue changes before the final guard.
pub(crate) async fn late_drift_attempt(
    parent: &Path,
    control: &ReadControl<'_>,
    mut progress: impl FnMut(IndexProgress) -> Result<()>,
) -> Result<()> {
    let (afc, server) = endpoint(
        Scenario::FinalCatalogDrift,
        Arc::new(AtomicBool::new(false)),
    );
    let result = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        parent,
        control,
        &mut progress,
    )
    .await
    .map(|_| ());
    server.await.unwrap();
    result
}

#[tokio::test]
async fn complete_index_binds_exact_bytes_and_cleans_only_owned_spool() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let unrelated = parent.directory.join("unrelated.txt");
    fs::write(&unrelated, b"preserve").unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::Normal, cancelled.clone());
    let mut rows = Vec::new();
    let mut bytes = 0;
    let index = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |event| {
            match event {
                IndexProgress::Fingerprinted(row) => rows.push(row),
                IndexProgress::FileBytes {
                    file_bytes_read, ..
                } => bytes = file_bytes_read,
                _ => {}
            }
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(index.tracks().len(), 1);
    assert!(rows[0] == index.tracks()[&1]);
    assert_eq!(
        rows[0].fingerprint.full_file_sha256,
        <[u8; 32]>::from(Sha256::digest(AUDIO))
    );
    assert_eq!(rows[0].fingerprint.audio.timescale, 44100);
    assert_eq!(bytes, AUDIO.len() as u64);
    assert!(!rows[0].attributes.contains_key("st_atime"));
    assert_eq!(fs::read(&unrelated).unwrap(), b"preserve");
    assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 1);
    let operations = server.await.unwrap();
    assert_eq!(
        operations
            .iter()
            .filter(|op| **op == AfcOpcode::FileOpen)
            .count(),
        5
    );
    fs::remove_file(unrelated).unwrap();
}

#[tokio::test]
async fn mixed_catalog_is_complete_only_with_disjoint_confirmed_codec_maps() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::Mixed, cancelled.clone());
    let targets = vec![
        Target {
            sync_id: 1,
            path: "iTunes_Control/Music/F39/OTHER.m4a".into(),
            bytes: AUDIO.len() as u64,
        },
        Target {
            sync_id: 2,
            path: MUSIC.into(),
            bytes: 834,
        },
    ];
    let mut alac_events = 0;
    let mut other_events = 0;
    let index = run(
        afc,
        targets,
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |event| {
            match event {
                IndexProgress::Fingerprinted(_) => alac_events += 1,
                IndexProgress::OtherFingerprinted(_) => other_events += 1,
                _ => {}
            }
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (
            index.tracks().len(),
            index.other_audio().len(),
            alac_events,
            other_events
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(index.other_audio()[&2].file_bytes, 834);
    // The test MP3 deliberately has an .m4a path: classification uses its bytes.
    assert!(matches!(
        index.other_audio()[&2].codec,
        OtherCodec::Mpeg1Layer3 { frame_count: 2, .. }
    ));
    assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn later_catalog_drift_retains_typed_progress_but_refuses_complete_index() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::FinalCatalogDrift, cancelled.clone());
    let mut completed = Vec::new();
    let result = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |event| {
            if let IndexProgress::Fingerprinted(row) = event {
                completed.push(row);
            }
            Ok(())
        },
    )
    .await;
    let error = result.err().unwrap();
    assert!(error.to_string().contains("catalog bytes differ"));
    assert!(error.downcast_ref::<InitialCatalogGuardDrift>().is_none());
    assert!(error.downcast_ref::<LateCatalogGuardDrift>().is_some());
    assert_eq!(completed.len(), 1);
    assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn late_audio_mutation_invalidates_previously_fingerprinted_record() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::ChangedAfterFingerprint, cancelled.clone());
    let mut completed = 0;
    let result = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |event| {
            if matches!(event, IndexProgress::Fingerprinted(_)) {
                completed += 1;
            }
            Ok(())
        },
    )
    .await;
    let error = result.err().unwrap();
    assert!(error.to_string().contains("changed later in the pass"));
    assert!(error.downcast_ref::<InitialCatalogGuardDrift>().is_none());
    assert!(error.downcast_ref::<LateCatalogGuardDrift>().is_none());
    assert_eq!(completed, 1);
    server.await.unwrap();
}

#[tokio::test]
async fn late_allocation_trim_alone_keeps_the_pass_complete() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::TrimmedAfterFingerprint, cancelled.clone());
    let index = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |_| Ok(()),
    )
    .await
    .unwrap();
    // The record keeps the allocation the file had when it was fingerprinted.
    assert_eq!(index.tracks()[&1].attributes["st_blocks"], "16");
    server.await.unwrap();
}

#[tokio::test]
async fn initial_wal_handoff_drift_is_typed_before_progress_or_audio_reads() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::InitialWalTruncated, cancelled.clone());
    let error = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |_| panic!("initial guard failure must retain empty progress"),
    )
    .await
    .err()
    .unwrap();
    let operations = server.await.unwrap();
    assert_eq!(operations, vec![AfcOpcode::GetFileInfo; 3]);
    assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
    assert!(
        error.downcast_ref::<InitialCatalogGuardDrift>().is_some(),
        "valid initial WAL handoff drift must be typed: {error:#}"
    );
}

#[tokio::test]
async fn initial_catalog_mismatch_prevents_audio_reads_and_progress() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::Normal, cancelled.clone());
    let mut expected = binding();
    expected.database_sha256 = [3; 32];
    let result = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        expected,
        &parent.directory,
        &control,
        &mut |_| panic!("no progress before initial catalog binding passes"),
    )
    .await;
    let error = result.err().unwrap();
    assert!(error.to_string().contains("catalog bytes differ"));
    assert!(error.downcast_ref::<InitialCatalogGuardDrift>().is_some());
    assert_eq!(
        server
            .await
            .unwrap()
            .iter()
            .filter(|op| **op == AfcOpcode::FileOpen)
            .count(),
        1
    );
    assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
}

#[tokio::test]
async fn valid_initial_wal_absence_and_hash_guard_attribute_drift_are_typed() {
    for scenario in [
        Scenario::InitialWalMissing,
        Scenario::InitialHashGuardAttributeDrift,
    ] {
        let parent = Spool::new(&std::env::temp_dir()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
        };
        let (afc, server) = endpoint(scenario, cancelled.clone());
        let error = run(
            afc,
            plan(AUDIO.len() as u64),
            identity(),
            binding(),
            &parent.directory,
            &control,
            &mut |_| panic!("no progress before initial guard passes"),
        )
        .await
        .err()
        .unwrap();
        assert!(
            error.downcast_ref::<InitialCatalogGuardDrift>().is_some(),
            "{error:#}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_initial_evidence_io_cancellation_and_deadline_are_not_retryable() {
    for scenario in [
        Scenario::InitialMalformedWal,
        Scenario::InitialMalformedShm,
        Scenario::InitialWalIo,
        Scenario::InitialCancel,
        Scenario::InitialStall,
    ] {
        let parent = Spool::new(&std::env::temp_dir()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_millis(100),
            cancelled: &cancelled,
        };
        let (afc, server) = endpoint(scenario, cancelled.clone());
        let error = run(
            afc,
            plan(AUDIO.len() as u64),
            identity(),
            binding(),
            &parent.directory,
            &control,
            &mut |_| panic!("invalid initial evidence must stop before progress"),
        )
        .await
        .err()
        .unwrap();
        assert!(
            error.downcast_ref::<InitialCatalogGuardDrift>().is_none(),
            "{error:#}"
        );
        let operations = server.await.unwrap();
        assert!(operations.iter().all(|op| *op == AfcOpcode::GetFileInfo));
        assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
    }
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::InitialWalTruncated, cancelled.clone());
    let mut invalid = binding();
    invalid.wal_sha256 = None;
    let error = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        invalid,
        &parent.directory,
        &control,
        &mut |_| panic!("invalid binding must stop before progress"),
    )
    .await
    .err()
    .unwrap();
    assert!(error.downcast_ref::<InitialCatalogGuardDrift>().is_none());
    assert!(server.await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_changed_truncated_unsupported_or_unclosed_audio_never_completes() {
    for scenario in [
        Scenario::MissingAudio,
        Scenario::ChangedAudio,
        Scenario::Eof,
        Scenario::BadClose,
        Scenario::UnsupportedAudio,
    ] {
        let parent = Spool::new(&std::env::temp_dir()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
        };
        let (afc, server) = endpoint(scenario, cancelled.clone());
        let mut completed = 0;
        let bytes = if matches!(scenario, Scenario::UnsupportedAudio) {
            7
        } else {
            AUDIO.len() as u64
        };
        let result = run(
            afc,
            plan(bytes),
            identity(),
            binding(),
            &parent.directory,
            &control,
            &mut |event| {
                if matches!(event, IndexProgress::Fingerprinted(_)) {
                    completed += 1;
                }
                Ok(())
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(completed, 0);
        assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn a_missing_file_is_listed_as_gone_only_when_the_pass_allows_it() {
    for gone in [true, false] {
        let parent = Spool::new(&std::env::temp_dir()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
        };
        let (afc, server) = endpoint(Scenario::MissingAudio, cancelled.clone());
        let result = run_checked(
            afc,
            plan(AUDIO.len() as u64),
            identity(),
            binding(),
            &parent.directory,
            &control,
            &mut |_| Ok(()),
            gone,
        )
        .await;
        match result {
            Ok(index) => {
                assert!(gone);
                assert!(index.tracks().is_empty() && index.other_audio().is_empty());
                assert_eq!(index.gone(), &BTreeSet::from([1]));
            }
            Err(error) => {
                assert!(!gone);
                let error = format!("{error:#}");
                assert!(
                    error.contains(&format!("{MUSIC} of track 0000000000000001 is missing")),
                    "{error}"
                );
            }
        }
        server.await.unwrap();
    }
}

// Paused time: the deadline expires only once the scripted phone is stalled,
// not while the machine is too busy to reach the read.
#[tokio::test(start_paused = true)]
async fn cancellation_and_deadline_discard_uncertain_stream_without_close_or_retry() {
    for scenario in [Scenario::CancelRead, Scenario::StallRead] {
        let parent = Spool::new(&std::env::temp_dir()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_millis(200),
            cancelled: &cancelled,
        };
        let (afc, server) = endpoint(scenario, cancelled.clone());
        let result = run(
            afc,
            plan(AUDIO.len() as u64),
            identity(),
            binding(),
            &parent.directory,
            &control,
            &mut |_| Ok(()),
        )
        .await;
        assert!(result.is_err());
        let operations = server.await.unwrap();
        assert_eq!(operations.last(), Some(&AfcOpcode::Read));
        assert_eq!(
            operations
                .iter()
                .filter(|op| **op == AfcOpcode::FileOpen)
                .count(),
            3
        );
        assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn durable_progress_failure_aborts_without_completing_or_restarting() {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(Scenario::Normal, cancelled.clone());
    let result = run(
        afc,
        plan(AUDIO.len() as u64),
        identity(),
        binding(),
        &parent.directory,
        &control,
        &mut |event| {
            if matches!(event, IndexProgress::Fingerprinted(_)) {
                bail!("durable checkpoint failed");
            }
            Ok(())
        },
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("durable checkpoint failed")
    );
    assert_eq!(
        server
            .await
            .unwrap()
            .iter()
            .filter(|op| **op == AfcOpcode::FileOpen)
            .count(),
        3
    );
    assert_eq!(fs::read_dir(&parent.directory).unwrap().count(), 0);
}

#[test]
fn path_bounds_refuse_unsafe_or_ambiguous_inputs() {
    assert!(safe_path(MUSIC));
    for path in [
        "iTunes_Control/Music/F39/../TEST.m4a",
        "iTunes_Control/Music/F3/TEST.m4a",
        "iTunes_Control/Music/F39/a%2fb.m4a",
        "/iTunes_Control/Music/F39/TEST.m4a",
        "iTunes_Control/Music/F39/é.m4a",
    ] {
        assert!(!safe_path(path));
    }
}

#[test]
fn preflight_refuses_unavailable_missing_unsafe_or_oversized_tracks() {
    use music_protocol::catalogue::sqlite::{Availability, TrackMetadata};
    let mut track = Track {
        sync_id: 1,
        internal_id: 1,
        album_sync_id: 2,
        artist_sync_id: 3,
        playback: Default::default(),
        metadata: TrackMetadata {
            content_rating: 0,
            content_rating_level: 0,
            is_compilation: false,
            title: "Synthetic".into(),
            artist: "Synthetic".into(),
            album: "Synthetic".into(),
            album_artist: "Synthetic".into(),
            genre: None,
            composer: None,
            copyright: None,
            track_number: 1,
            track_count: 1,
            disc_number: 1,
            disc_count: 1,
            year: 2026,
            total_time_ms: 50,
            file_size: AUDIO.len() as u64,
        },
        afc_path: Some(MUSIC.into()),
        availability: Availability {
            is_placeholder: false,
            is_library_added: true,
            has_non_purgeable_asset: true,
            is_supported_type: true,
        },
    };
    assert_eq!(target(&track).unwrap().bytes, AUDIO.len() as u64);
    track.availability.is_placeholder = true;
    assert!(target(&track).is_err());
    track.availability.is_placeholder = false;
    track.availability.has_non_purgeable_asset = false;
    assert!(target(&track).is_err());
    track.availability.has_non_purgeable_asset = true;
    track.availability.is_supported_type = false;
    assert!(target(&track).is_err());
    track.availability.is_supported_type = true;
    track.availability.is_library_added = false;
    assert!(target(&track).is_err());
    track.availability.is_library_added = true;
    track.afc_path = None;
    assert!(target(&track).is_err());
    track.afc_path = Some("iTunes_Control/Music/F39/../wrong.m4a".into());
    assert!(target(&track).is_err());
    track.afc_path = Some(MUSIC.into());
    for size in [0, MAX_FILE_BYTES + 1] {
        track.metadata.file_size = size;
        assert!(target(&track).is_err());
    }
    track.metadata.file_size = AUDIO.len() as u64;
    track.sync_id = 0;
    assert!(target(&track).is_err());
}

#[test]
fn only_allowed_rows_without_audio_are_left_out_of_the_index() {
    let fixture = music_protocol::test_support::catalogue::Fixture::new(false);
    // The row an interrupted insertion leaves: library-added and supported,
    // with no location, size or local asset.
    fixture
        .connection
        .execute_batch(
            "INSERT INTO item VALUES(2,8,31,21,41,61,71,0,0,7,1,0,100);
            INSERT INTO item_extra VALUES(2,'Unfilled','',6000.0,0,10,0,2026,NULL,0,0,'Unfilled',NULL);
            INSERT INTO item_store VALUES(2,-2);
            INSERT INTO item_state VALUES(2,0,1,0,1);
            INSERT INTO item_stats VALUES(2,0,0,0,0,0,0,0,0);",
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    let (filled, unfilled) = ((-1i64) as u64, (-2i64) as u64);
    assert!(targets_allowing(&catalog, &BTreeSet::new()).is_err());
    assert!(required_bytes_allowing(&catalog, &BTreeSet::new()).is_err());
    for allowed in [
        BTreeSet::from([unfilled]),
        BTreeSet::from([filled, unfilled]),
    ] {
        let (targets, found) = targets_allowing(&catalog, &allowed).unwrap();
        assert_eq!(found, BTreeSet::from([unfilled]));
        assert_eq!(
            targets.iter().map(|t| t.sync_id).collect::<Vec<_>>(),
            vec![filled]
        );
        assert_eq!(required_bytes_allowing(&catalog, &allowed).unwrap(), 12345);
    }
    assert!(targets_allowing(&catalog, &BTreeSet::from([filled])).is_err());
    // A capture that accepts any such row finds exactly that one.
    let any = Absent {
        any_unfilled: true,
        ..Absent::NONE
    };
    assert_eq!(any.rows(&catalog), BTreeSet::from([unfilled]));
    assert!(Absent::NONE.rows(&catalog).is_empty());
    // A row outside the library is not one an insertion left unfilled.
    fixture
        .connection
        .execute(
            "UPDATE item_state SET is_library_added=0 WHERE item_pid=2",
            [],
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    assert!(targets_allowing(&catalog, &BTreeSet::from([unfilled])).is_err());
    assert!(any.rows(&catalog).is_empty());
}

fn cache_config(mode: cache::Mode) -> (Spool, cache::Config) {
    let root = Spool::new(&std::env::temp_dir()).unwrap();
    let config = cache::Config::new(root.directory.join("fingerprints"), mode);
    cache::select_for_tests(&config, &identity().device_identifier_sha256);
    (root, config)
}
fn mixed_targets() -> Vec<Target> {
    vec![
        Target {
            sync_id: 1,
            path: "iTunes_Control/Music/F39/OTHER.m4a".into(),
            bytes: AUDIO.len() as u64,
        },
        Target {
            sync_id: 2,
            path: MUSIC.into(),
            bytes: 834,
        },
    ]
}
/// One pass inside a cache scope: (index, audio files opened, which files
/// reused a cached fingerprint). The fake phone refuses GetFileHash.
async fn cached_pass(
    config: &cache::Config,
    scenario: Scenario,
    targets: Vec<Target>,
) -> (Result<DeviceAudioIndex>, usize, Vec<bool>) {
    let parent = Spool::new(&std::env::temp_dir()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let (afc, server) = endpoint(scenario, cancelled.clone());
    let mut reused = Vec::new();
    let result = cache::scope(
        Some(config.clone()),
        run(
            afc,
            targets,
            identity(),
            binding(),
            &parent.directory,
            &control,
            &mut |event| {
                match event {
                    IndexProgress::FileStarted { .. } => reused.push(false),
                    IndexProgress::Reused { .. } => *reused.last_mut().unwrap() = true,
                    _ => {}
                }
                Ok(())
            },
        ),
    )
    .await;
    let operations = server.await.unwrap();
    // Two catalogue guard opens precede the audio and two more follow a
    // completed pass; the rest are audio copies.
    let guards = if result.is_ok() { 4 } else { 2 };
    let opened = operations
        .iter()
        .filter(|op| **op == AfcOpcode::FileOpen)
        .count()
        - guards;
    (result, opened, reused)
}

#[tokio::test]
async fn cached_pass_reuses_fingerprints_by_file_stat_without_copying_audio() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    let (first, opened, reused) = cached_pass(&config, Scenario::Mixed, mixed_targets()).await;
    let first = first.unwrap();
    assert_eq!((opened, reused), (2, vec![false, false]));
    let (second, opened, reused) = cached_pass(&config, Scenario::Mixed, mixed_targets()).await;
    let second = second.unwrap();
    assert_eq!((opened, reused), (0, vec![true, true]));
    assert!(first.tracks() == second.tracks() && first.other_audio() == second.other_audio());
    assert_eq!(
        second.tracks()[&1].fingerprint.full_file_sha256,
        <[u8; 32]>::from(Sha256::digest(AUDIO))
    );
    assert!(matches!(
        second.other_audio()[&2].codec,
        OtherCodec::Mpeg1Layer3 { frame_count: 2, .. }
    ));
}

#[tokio::test]
async fn full_readback_refreshes_the_cache_but_never_reuses_it() {
    let (_root, config) = cache_config(cache::Mode::Refresh);
    for _ in 0..2 {
        let (index, opened, reused) =
            cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
        index.unwrap();
        assert_eq!((opened, reused), (1, vec![false]));
    }
    assert_eq!(cache_rows(&config), 1);
}

#[tokio::test]
async fn a_rewritten_file_is_copied_again_and_replaces_its_entry() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    let (_, opened, reused) =
        cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
    assert_eq!((opened, reused), (1, vec![false]));
    for expected in [(1, vec![false]), (0, vec![true])] {
        let (index, opened, reused) =
            cached_pass(&config, Scenario::Rewritten, plan(AUDIO.len() as u64)).await;
        index.unwrap();
        assert_eq!((opened, reused), expected);
    }
    assert_eq!(cache_rows(&config), 1);
}

#[tokio::test]
async fn unreadable_or_mismatched_cache_entry_is_a_miss_and_is_rewritten() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    let (index, _, _) = cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
    let expected = index.unwrap();
    let database = rusqlite::Connection::open(cache::database_for_tests(&config)).unwrap();
    let body = || -> Vec<u8> {
        database
            .query_row("SELECT body FROM entries", [], |r| r.get(0))
            .unwrap()
    };
    let original = body();
    let mut wrong_size: serde_json::Value = serde_json::from_slice(&original).unwrap();
    wrong_size["file_bytes"] = (AUDIO.len() as u64 + 1).into();
    for corrupt in [b"{".to_vec(), serde_json::to_vec(&wrong_size).unwrap()] {
        database
            .execute("UPDATE entries SET body=?1", [&corrupt])
            .unwrap();
        let (index, opened, reused) =
            cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
        assert!(index.unwrap().tracks() == expected.tracks());
        assert_eq!((opened, reused), (1, vec![false]));
        assert_eq!(body(), original);
    }
}

#[tokio::test]
async fn an_mpeg_entry_without_its_frame_digest_is_a_miss_and_is_rewritten() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    cached_pass(&config, Scenario::Mixed, mixed_targets())
        .await
        .0
        .unwrap();
    let database = rusqlite::Connection::open(cache::database_for_tests(&config)).unwrap();
    let body = || -> serde_json::Value {
        let bytes: Vec<u8> = database
            .query_row("SELECT body FROM entries WHERE path=?1", [MUSIC], |r| {
                r.get(0)
            })
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    let original = body();
    assert!(original["other"]["frame_bytes_sha256"].is_string());
    let mut missing = original.clone();
    missing["other"]["frame_bytes_sha256"] = serde_json::Value::Null;
    database
        .execute(
            "UPDATE entries SET body=?1 WHERE path=?2",
            rusqlite::params![serde_json::to_vec(&missing).unwrap(), MUSIC],
        )
        .unwrap();
    let (index, opened, reused) = cached_pass(&config, Scenario::Mixed, mixed_targets()).await;
    index.unwrap();
    assert_eq!((opened, reused), (1, vec![true, false]));
    assert_eq!(body(), original);
}

#[tokio::test]
async fn an_entry_stored_under_another_composite_definition_is_reused_with_this_builds() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    let (index, _, _) = cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
    let expected = index.unwrap();
    let database = rusqlite::Connection::open(cache::database_for_tests(&config)).unwrap();
    let mut body: serde_json::Value = serde_json::from_slice(
        &database
            .query_row("SELECT body FROM entries", [], |r| r.get::<_, Vec<u8>>(0))
            .unwrap(),
    )
    .unwrap();
    // As a build with another hash domain would have stored it.
    body["fingerprint"]["audio"]["sha256"] = "05".repeat(32).into();
    database
        .execute(
            "UPDATE entries SET body=?1",
            [serde_json::to_vec(&body).unwrap()],
        )
        .unwrap();
    let (index, opened, reused) =
        cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
    assert!(index.unwrap().tracks() == expected.tracks());
    assert_eq!((opened, reused), (0, vec![true]));
}

#[tokio::test]
async fn unreadable_or_unfamiliar_database_is_set_aside_and_rebuilt() {
    for damage in [&b"not a database at all, just bytes"[..], b"unfamiliar"] {
        let (_root, config) = cache_config(cache::Mode::Reuse);
        let directory = config_directory(&config);
        fs::create_dir_all(&directory).unwrap();
        let database = cache::database_for_tests(&config);
        if damage == b"unfamiliar" {
            let c = rusqlite::Connection::open(&database).unwrap();
            c.execute_batch("CREATE TABLE other(x);").unwrap();
        } else {
            fs::write(&database, damage.repeat(200)).unwrap();
        }
        let (index, opened, reused) =
            cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
        index.unwrap();
        assert_eq!((opened, reused), (1, vec![false]));
        assert!(directory.join("fingerprints.sqlite.unreadable").exists());
        assert_eq!(cache_rows(&config), 1);
        let (_, opened, reused) =
            cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
        assert_eq!((opened, reused), (0, vec![true]));
    }
}

#[tokio::test]
async fn unavailable_cache_never_fails_a_pass_and_counts_nothing_stored() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    // A file where the cache directory belongs: the database cannot be opened.
    fs::write(config_directory(&config), b"not a directory").unwrap();
    for _ in 0..2 {
        let (index, opened, reused) =
            cached_pass(&config, Scenario::Normal, plan(AUDIO.len() as u64)).await;
        index.unwrap();
        assert_eq!((opened, reused), (1, vec![false]));
    }
    assert_eq!(cache::counts_for_tests(&config), (0, 0));
    assert!(config_directory(&config).is_file());
}

#[tokio::test]
async fn statistics_count_use_and_prune_removes_only_unseen_entries() {
    let (_root, config) = cache_config(cache::Mode::Reuse);
    let directory = config_directory(&config);
    let empty = cache::statistics(&directory, 30).unwrap();
    assert_eq!(empty.entries, 0);
    assert!(!directory.exists(), "statistics never creates the cache");
    cached_pass(&config, Scenario::Mixed, mixed_targets())
        .await
        .0
        .unwrap();
    cached_pass(&config, Scenario::Mixed, mixed_targets())
        .await
        .0
        .unwrap();
    let report = cache::statistics(&directory, 30).unwrap();
    assert_eq!(
        (report.entries, report.hits, report.writes, report.unseen),
        (2, 2, 2, 0)
    );
    // Age one entry, as for a track deleted from the phone 40 days ago.
    let c = rusqlite::Connection::open(cache::database_for_tests(&config)).unwrap();
    c.execute(
        "UPDATE entries SET last_seen=last_seen-40*86400 WHERE file_bytes=834",
        [],
    )
    .unwrap();
    assert_eq!(cache::statistics(&directory, 30).unwrap().unseen, 1);
    assert_eq!(cache::prune(&directory, 30, true).unwrap(), 1);
    assert_eq!(cache_rows(&config), 2);
    assert_eq!(cache::prune(&directory, 30, false).unwrap(), 1);
    assert_eq!(cache_rows(&config), 1);
    // The pruned file is copied again; the kept one is still reused.
    let (_, opened, reused) = cached_pass(&config, Scenario::Mixed, mixed_targets()).await;
    assert_eq!((opened, reused), (1, vec![true, false]));
}

fn config_directory(config: &cache::Config) -> PathBuf {
    config.directory().to_owned()
}
fn cache_rows(config: &cache::Config) -> i64 {
    rusqlite::Connection::open(cache::database_for_tests(config))
        .unwrap()
        .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_file_whose_allocation_alone_changed_is_the_same_file() {
    let audio = music_protocol::media::fingerprint::EncodedAlac {
        sha256: [1; 32],
        alac_config_sha256: [2; 32],
        sample_sizes_sha256: [3; 32],
        media_sha256: [4; 32],
        timescale: 44100,
        duration_ticks: 132300,
        media_bytes: 800,
    };
    let track = |blocks: &str, mtime: &str| TrackAudio {
        sync_id: 7,
        afc_path: MUSIC.into(),
        attributes: Attributes::from([
            ("st_ifmt".into(), "S_IFREG".into()),
            ("st_size".into(), "20814357".into()),
            ("st_blocks".into(), blocks.into()),
            ("st_mtime".into(), mtime.into()),
        ]),
        fingerprint: SourceFingerprint {
            file_bytes: 20814357,
            full_file_sha256: [5; 32],
            audio: audio.clone(),
        },
    };
    let index = |tracks: Vec<TrackAudio>| DeviceAudioIndex {
        identity: identity(),
        catalog: binding(),
        tracks: tracks.into_iter().map(|t| (t.sync_id, t)).collect(),
        other_audio: BTreeMap::new(),
        unfilled: BTreeSet::new(),
        gone: BTreeSet::new(),
    };
    // Seen after an insert: the phone trimmed a recently written file's spare
    // allocation, 40904 -> 40656 blocks, and nothing else.
    let before = index(vec![track("40904", "9")]);
    let trimmed = index(vec![track("40656", "9")]);
    assert!(track("40904", "9").same_file(&track("40656", "9")));
    assert!(before.same_files(&trimmed) && before.kept_in(&trimmed, &BTreeSet::new()));
    assert_eq!(before.allocation_changed(&trimmed), vec![7]);
    assert!(before.allocation_changed(&before).is_empty());
    // Any other change is still a changed file.
    let touched = index(vec![track("40904", "10")]);
    assert!(!before.same_files(&touched) && !before.kept_in(&touched, &BTreeSet::new()));
    assert!(before.allocation_changed(&touched).is_empty());
    let mut other = track("40904", "9");
    other.fingerprint.full_file_sha256 = [6; 32];
    assert!(!track("40904", "9").same_file(&other));
    other = track("40904", "9");
    other.afc_path = "iTunes_Control/Music/F39/OTHER.m4a".into();
    assert!(!track("40904", "9").same_file(&other));
    // A file the after state lacks is not kept, unless excepted.
    let empty = index(Vec::new());
    assert!(!before.kept_in(&empty, &BTreeSet::new()) && !before.same_files(&empty));
    assert!(before.kept_in(&empty, &BTreeSet::from([7])));
    // An extra file in the after state breaks equality but not keeping.
    let mut extra = track("1", "1");
    extra.sync_id = 8;
    let grown = index(vec![track("40656", "9"), extra]);
    assert!(before.kept_in(&grown, &BTreeSet::new()) && !before.same_files(&grown));
}

/// A peer that takes every request of a window before answering any, then
/// answers each in order (or, with `skew`, with the wrong packet number).
fn pipelined_peer(paths: usize, skew: bool) -> (AfcClient, tokio::task::JoinHandle<()>) {
    let (client, peer) = tokio::io::duplex(64 * 1024);
    let afc = AfcClient::new(Idevice::new(Box::new(client), "offline-stat-test"));
    let task = tokio::spawn(async move {
        let mut peer = Idevice::new(Box::new(peer), "offline-stat-peer");
        let mut requests = Vec::new();
        for _ in 0..paths.min(music_protocol::protocol::afc::stat::STAT_WINDOW) {
            requests.push(AfcPacket::read(&mut peer).await.unwrap());
        }
        for mut request in requests {
            assert_eq!(request.header.operation, AfcOpcode::GetFileInfo);
            if skew {
                request.header.packet_num += 1;
            }
            if request.header_payload.ends_with(b"absent.m4a") {
                reply(
                    &mut peer,
                    &request,
                    AfcOpcode::Status,
                    8u64.to_le_bytes().to_vec(),
                    vec![],
                )
                .await;
            } else {
                reply(
                    &mut peer,
                    &request,
                    AfcOpcode::Data,
                    vec![],
                    attributes(5, 1, 1),
                )
                .await;
            }
        }
    });
    (afc, task)
}

#[tokio::test]
async fn many_stats_are_pipelined_and_answered_in_order() {
    let cancelled = AtomicBool::new(false);
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(3),
        cancelled: &cancelled,
    };
    let paths = [
        "iTunes_Control/Music/F00/a.m4a",
        "iTunes_Control/Music/F00/absent.m4a",
        "iTunes_Control/Music/F00/c.m4a",
    ];
    let (mut afc, peer) = pipelined_peer(paths.len(), false);
    let stats = stat_all(&mut afc, &paths, &control).await.unwrap();
    peer.await.unwrap();
    assert_eq!(
        stats
            .iter()
            .map(|s| s.as_ref().map(|a| a["st_size"].clone()))
            .collect::<Vec<_>>(),
        [Some("5".to_owned()), None, Some("5".to_owned())]
    );

    // A reply carrying another request's number discards the connection.
    let (mut afc, peer) = pipelined_peer(paths.len(), true);
    let error = stat_all(&mut afc, &paths, &control).await.unwrap_err();
    assert!(format!("{error:#}").contains("out of order"), "{error:#}");
    drop(afc);
    peer.await.unwrap();
}
