//! Bounded read-only inventory of the current iTunesDB/CDB format.
//!
//! This parses saved state, not a live anchor or deletion ownership. Evidence:
//! AMPDevicesAgent SHA256 f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9;
//! writers 0x47866c/0x476d78, 0x475684/0x4751e0, 0x4764bc/0x476200,
//! text 0x474c40, PID metadata loads 0x461b54/0x463086/0x464144.
//! Unknown payloads are bounded but not interpreted. Multiple occurrences of
//! one PID within a dataset are rejected as unsupported instance ambiguity.

use anyhow::{Context, Result, bail, ensure};
use flate2::{Decompress, FlushDecompress, Status};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SECTION_BYTES: usize = 256 * 1024 * 1024;
const MAX_SECTIONS: u32 = 4096;
const MAX_ROWS: usize = 200_000;
const MAX_ROW_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHILDREN: usize = 4096;
const MAX_TOTAL_CHILDREN: usize = 500_000;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_TEXT_BYTES: usize = 64 * 1024 * 1024;
const ROOT_HEADER: usize = 244;
const SECTION_HEADER: usize = 96;
const LIST_HEADER: usize = 92;
const TRACK_DATASETS: [u32; 3] = [1, 6, 10];

pub struct Inventory {
    database_pid: u64,
    primary_container_pid: u64,
    tracks: BTreeMap<u32, Vec<Track>>,
    albums: BTreeMap<u32, Collection>,
    artists: BTreeMap<u32, Collection>,
    track_pids: BTreeSet<u64>,
    all_mapped_pids: BTreeSet<u64>,
    playlist_pids: BTreeSet<u64>,
    saved_media_anchor: Option<u32>,
}

impl Inventory {
    pub fn database_pid(&self) -> u64 {
        self.database_pid
    }

    pub fn primary_container_pid(&self) -> u64 {
        self.primary_container_pid
    }

    pub fn playlist_pids(&self) -> &BTreeSet<u64> {
        &self.playlist_pids
    }

    /// All represented track locations, including references to missing files.
    /// Requires every row in datasets 1/6/10 to have a supported native colon
    /// path under iTunes_Control/Music/FNN. Returns exact case; callers may
    /// additionally compare ignoring ASCII case when checking a destination.
    pub fn music_paths(&self) -> Result<BTreeSet<String>> {
        self.tracks
            .values()
            .flatten()
            .map(|track| music_path(track.text.get(&2).context("track type2 location absent")?))
            .collect()
    }

    pub fn primary_tracks(&self) -> &[Track] {
        &self.tracks[&1]
    }

    pub fn track_datasets(&self) -> &BTreeMap<u32, Vec<Track>> {
        &self.tracks
    }

    pub fn albums(&self) -> &BTreeMap<u32, Collection> {
        &self.albums
    }

    pub fn artists(&self) -> &BTreeMap<u32, Collection> {
        &self.artists
    }

    /// Includes all represented track PIDs in datasets 1, 6 and 10.
    pub fn track_pids(&self) -> &BTreeSet<u64> {
        &self.track_pids
    }

    /// Includes tracks, collections, playlist headers and the database PID.
    /// Does not include IDs hidden in other unparsed payloads.
    pub fn all_mapped_pids(&self) -> &BTreeSet<u64> {
        &self.all_mapped_pids
    }

    /// Saved host state only. Parsing requires exactly one anchor dataset.
    pub fn saved_media_anchor(&self) -> u32 {
        self.saved_media_anchor
            .expect("parse requires anchor dataset")
    }
}

pub struct Track {
    pub dataset: u32,
    pub pid: u64,
    pub record_id: u32,
    pub album_local_id: u32,
    pub artist_local_id: u32,
    pub album_pid: Option<u64>,
    pub artist_pid: Option<u64>,
    pub metadata: TrackMetadata,
    /// Proven text types: 1 title, 2 native permanent file location,
    /// 3 album, 4 artist, 22 album_artist.
    pub text: BTreeMap<u32, String>,
}

impl Track {
    pub fn title(&self) -> Option<&str> {
        self.text.get(&1).map(String::as_str)
    }
    pub fn album(&self) -> Option<&str> {
        self.text.get(&3).map(String::as_str)
    }
    pub fn artist(&self) -> Option<&str> {
        self.text.get(&4).map(String::as_str)
    }
    pub fn album_artist(&self) -> Option<&str> {
        self.text.get(&22).map(String::as_str)
    }
}

pub struct TrackMetadata {
    pub total_time_ms: u32,
    pub track_number: u32,
    pub track_count: u32,
    pub disc_number: u32,
    pub disc_count: u32,
    pub year: u32,
    pub is_compilation: bool,
    pub file_size: u64,
}

pub struct Collection {
    pub local_id: u32,
    pub pid: u64,
    pub kind: u8,
    /// Album 200=name; 201/202 unresolved artist-family keys.
    /// Artist 300=name; 301 unresolved sort/group key. No reuse rule is inferred.
    pub text: BTreeMap<u32, String>,
}

#[derive(Clone, Copy)]
enum Order {
    Little,
    Big,
}

impl Order {
    fn u32(self, data: &[u8], at: usize) -> Result<u32> {
        let bytes: [u8; 4] = data
            .get(at..at.checked_add(4).context("field offset overflow")?)
            .context("truncated u32 field")?
            .try_into()
            .expect("checked field length");
        Ok(match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        })
    }

    fn u64(self, data: &[u8], at: usize) -> Result<u64> {
        let bytes: [u8; 8] = data
            .get(at..at.checked_add(8).context("field offset overflow")?)
            .context("truncated u64 field")?
            .try_into()
            .expect("checked field length");
        Ok(match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        })
    }

    fn magic(self, data: &[u8], expected: &[u8; 4]) -> Result<()> {
        ensure!(
            self.u32(data, 0)? == u32::from_le_bytes(*expected),
            "invalid database record magic"
        );
        Ok(())
    }
}

struct Reader {
    order: Order,
    rows: usize,
    children: usize,
    text_bytes: usize,
}

impl Reader {
    fn text_children(
        &mut self,
        row: &[u8],
        header: usize,
        known: &[u32],
    ) -> Result<BTreeMap<u32, String>> {
        let count = self.order.u32(row, 12)? as usize;
        ensure!(count <= MAX_CHILDREN, "row child count exceeds limit");
        self.children += count;
        ensure!(
            self.children <= MAX_TOTAL_CHILDREN,
            "aggregate child count exceeds limit"
        );
        ensure!(
            count <= (row.len() - header) / 24,
            "child count exceeds row bounds"
        );
        let mut offset = header;
        let mut text = BTreeMap::new();
        for _ in 0..count {
            let child = &row[offset..];
            ensure!(child.len() >= 24, "truncated child header");
            self.order.magic(child, b"mhod")?;
            ensure!(
                self.order.u32(child, 4)? == 24,
                "unsupported child header length"
            );
            let length = self.order.u32(child, 8)? as usize;
            let kind = self.order.u32(child, 12)?;
            ensure!(
                (24..=child.len()).contains(&length),
                "child length exceeds row bounds"
            );
            let child = &child[..length];
            if known.contains(&kind) {
                ensure!(!text.contains_key(&kind), "duplicate known text child");
                ensure!(length >= 40, "truncated text header");
                ensure!(child[32] == 1, "unsupported legacy text conversion flag");
                let size = self.order.u32(child, 28)? as usize;
                ensure!(size <= MAX_TEXT_BYTES, "text field exceeds limit");
                ensure!(
                    size == length - 40,
                    "text byte count differs from child length"
                );
                self.text_bytes += size;
                ensure!(
                    self.text_bytes <= MAX_TOTAL_TEXT_BYTES,
                    "aggregate text exceeds limit"
                );
                let value = match self.order.u32(child, 24)? {
                    1 => {
                        ensure!(size.is_multiple_of(2), "odd UTF-16 byte count");
                        let units: Vec<u16> = child[40..]
                            .chunks_exact(2)
                            .map(|chunk| match self.order {
                                Order::Little => u16::from_le_bytes([chunk[0], chunk[1]]),
                                Order::Big => u16::from_be_bytes([chunk[0], chunk[1]]),
                            })
                            .collect();
                        String::from_utf16(&units).context("invalid UTF-16 text")?
                    }
                    2 => std::str::from_utf8(&child[40..])
                        .context("invalid UTF-8 text")?
                        .to_owned(),
                    _ => bail!("unsupported text encoding"),
                };
                text.insert(kind, value);
            }
            offset += length;
        }
        ensure!(offset == row.len(), "unclaimed row bytes or children");
        Ok(text)
    }

    fn list_header(&mut self, body: &[u8], magic: &[u8; 4], row_header: usize) -> Result<usize> {
        ensure!(body.len() >= LIST_HEADER, "truncated list header");
        self.order.magic(body, magic)?;
        ensure!(
            self.order.u32(body, 4)? as usize == LIST_HEADER,
            "unsupported list header length"
        );
        let count = self.order.u32(body, 8)? as usize;
        self.rows = self.rows.checked_add(count).context("row count overflow")?;
        ensure!(self.rows <= MAX_ROWS, "aggregate row count exceeds limit");
        ensure!(
            count <= (body.len() - LIST_HEADER) / row_header,
            "row count exceeds list bounds"
        );
        Ok(count)
    }

    fn row<'a>(
        &self,
        body: &'a [u8],
        offset: usize,
        magic: &[u8; 4],
        header: usize,
    ) -> Result<&'a [u8]> {
        let row = body.get(offset..).context("record offset exceeds list")?;
        ensure!(row.len() >= header, "truncated record header");
        self.order.magic(row, magic)?;
        ensure!(
            self.order.u32(row, 4)? as usize == header,
            "unsupported record header length"
        );
        let length = self.order.u32(row, 8)? as usize;
        ensure!(
            (header..=row.len().min(MAX_ROW_BYTES)).contains(&length),
            "record length exceeds bounds"
        );
        Ok(&row[..length])
    }

    fn tracks(&mut self, body: &[u8], dataset: u32) -> Result<Vec<Track>> {
        let count = self.list_header(body, b"mhlt", 624)?;
        let mut output = Vec::with_capacity(count);
        let (mut ids, mut pids) = (BTreeSet::new(), BTreeSet::new());
        let mut offset = LIST_HEADER;
        for index in 0..count {
            let row = self
                .row(body, offset, b"mhit", 624)
                .with_context(|| format!("dataset {dataset} row {index}"))?;
            let pid = self.order.u64(row, 0x70)?;
            let record_id = self.order.u32(row, 0x10)?;
            ensure!(pid != 0, "zero track PID");
            ensure!(
                pids.insert(pid),
                "duplicate track PID within dataset; ambiguous instances"
            );
            ensure!(
                ids.insert(record_id),
                "duplicate track record ID within dataset"
            );
            ensure!(row[0x1e] <= 1, "invalid compilation boolean");
            output.push(Track {
                dataset,
                pid,
                record_id,
                album_local_id: self.order.u32(row, 0x120)?,
                artist_local_id: self.order.u32(row, 0x1e0)?,
                album_pid: None,
                artist_pid: None,
                metadata: TrackMetadata {
                    total_time_ms: self.order.u32(row, 0x28)?,
                    track_number: self.order.u32(row, 0x2c)?,
                    track_count: self.order.u32(row, 0x30)?,
                    disc_number: self.order.u32(row, 0x5c)?,
                    disc_count: self.order.u32(row, 0x60)?,
                    year: self.order.u32(row, 0x34)?,
                    is_compilation: row[0x1e] == 1,
                    file_size: self.order.u64(row, 0x12c)?,
                },
                text: self.text_children(row, 624, &[1, 2, 3, 4, 22])?,
            });
            offset += row.len();
        }
        ensure!(offset == body.len(), "unclaimed track list bytes or rows");
        Ok(output)
    }

    fn collections(&mut self, body: &[u8], album: bool) -> Result<BTreeMap<u32, Collection>> {
        let (list_magic, row_magic, header, known): (_, _, _, &[u32]) = if album {
            (b"mhla", b"mhia", 88, &[200, 201, 202])
        } else {
            (b"mhli", b"mhii", 80, &[300, 301])
        };
        let count = self.list_header(body, list_magic, header)?;
        let (mut output, mut pids) = (BTreeMap::new(), BTreeSet::new());
        let mut offset = LIST_HEADER;
        for _ in 0..count {
            let row = self.row(body, offset, row_magic, header)?;
            let local_id = self.order.u32(row, 0x10)?;
            let pid = self.order.u64(row, 0x14)?;
            ensure!(local_id != 0 && pid != 0, "zero collection local ID or PID");
            ensure!(
                !output.contains_key(&local_id),
                "duplicate collection local ID"
            );
            ensure!(pids.insert(pid), "duplicate collection PID");
            output.insert(
                local_id,
                Collection {
                    local_id,
                    pid,
                    kind: row[0x1c],
                    text: self.text_children(row, header, known)?,
                },
            );
            offset += row.len();
        }
        ensure!(
            offset == body.len(),
            "unclaimed collection list bytes or rows"
        );
        Ok(output)
    }

    fn playlists(&mut self, body: &[u8]) -> Result<(BTreeSet<u64>, Option<u64>)> {
        // Writer 0x47a984: mhlp header92/count+8; 0x479c7c: mhyp
        // header184, total+8, primary byte+20, PlaylistInfo PID+28.
        // Nested playlist content is bounded by total length, not interpreted.
        let count = self.list_header(body, b"mhlp", 184)?;
        let (mut pids, mut primary, mut offset) = (BTreeSet::new(), None, LIST_HEADER);
        for _ in 0..count {
            let row = self.row(body, offset, b"mhyp", 184)?;
            ensure!(row[20] <= 1, "invalid primary-container flag");
            let pid = self.order.u64(row, 28)?;
            ensure!(
                pid != 0 && pids.insert(pid),
                "zero or duplicate playlist PID within dataset"
            );
            if row[20] == 1 {
                ensure!(
                    primary.is_none(),
                    "multiple primary containers within dataset"
                );
                primary = Some(pid);
            }
            offset += row.len();
        }
        ensure!(
            offset == body.len(),
            "unclaimed playlist list bytes or rows"
        );
        Ok((pids, primary))
    }
}

fn resolve_tracks(inventory: &mut Inventory) -> Result<()> {
    // Store observed common fields independently: filling a missing field here
    // must never invent a value in the returned primary or instance records.
    type Identity = (u32, u32, u32, [u32; 6], BTreeMap<u32, String>);
    let mut seen: BTreeMap<u64, Identity> = BTreeMap::new();
    for dataset in TRACK_DATASETS {
        for row in inventory.tracks.get_mut(&dataset).into_iter().flatten() {
            row.album_pid = resolve_join(row.album_local_id, &inventory.albums)?;
            row.artist_pid = resolve_join(row.artist_local_id, &inventory.artists)?;
            let fields = [
                row.metadata.track_number,
                row.metadata.track_count,
                row.metadata.disc_number,
                row.metadata.disc_count,
                row.metadata.year,
                u32::from(row.metadata.is_compilation),
            ];
            if let Some(previous) = seen.get_mut(&row.pid) {
                ensure!(
                    previous.0 == row.record_id
                        && previous.1 == row.album_local_id
                        && previous.2 == row.artist_local_id
                        && previous.3 == fields,
                    "conflicting cross-dataset track identity"
                );
                for (kind, value) in &row.text {
                    // Type2 is a TrackInstance location and can differ between
                    // instances sharing one TrackInfo PID.
                    if *kind == 2 {
                        continue;
                    }
                    if let Some(old) = previous.4.get(kind) {
                        ensure!(old == value, "conflicting cross-dataset track text");
                    } else {
                        previous.4.insert(*kind, value.clone());
                    }
                }
            } else {
                seen.insert(
                    row.pid,
                    (
                        row.record_id,
                        row.album_local_id,
                        row.artist_local_id,
                        fields,
                        row.text
                            .iter()
                            .filter(|(kind, _)| **kind != 2)
                            .map(|(kind, value)| (*kind, value.clone()))
                            .collect(),
                    ),
                );
            }
            inventory.track_pids.insert(row.pid);
        }
    }
    Ok(())
}

fn resolve_join(local_id: u32, collections: &BTreeMap<u32, Collection>) -> Result<Option<u64>> {
    if local_id == 0 {
        return Ok(None);
    }
    Ok(Some(
        collections
            .get(&local_id)
            .context("nonzero collection join has no target")?
            .pid,
    ))
}

fn music_path(native: &str) -> Result<String> {
    // Current writer 0x474d9a -> 0x6b12bc converts UTF-16 backslashes
    // to colons for type2. Reader 0x480451 -> 0x47e260 reverses colon
    // separators, then 0x4804bd -> 0x370740 stores permanent file location.
    // This deliberately rejects drive paths, alternative separators, Unicode,
    // traversal, non-Music roots and unsupported directory layouts.
    ensure!(
        native.len() <= 1024 && native.is_ascii(),
        "unsupported native Music path text"
    );
    let relative = native
        .strip_prefix(":iTunes_Control:Music:")
        .context("native location is outside the supported colon Music root")?;
    let (folder, name) = relative
        .split_once(':')
        .context("native Music path lacks file component")?;
    ensure!(
        folder.len() == 3
            && folder.as_bytes()[0] == b'F'
            && folder.as_bytes()[1..].iter().all(u8::is_ascii_digit),
        "unsupported Music folder component"
    );
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "unsafe or ambiguous Music file component"
    );
    Ok(format!("iTunes_Control/Music/{folder}/{name}"))
}
