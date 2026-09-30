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

/// Only the exact current writer's 1.117 raw / 2.117 zlib formats are accepted.
pub fn parse(input: &[u8]) -> Result<Inventory> {
    ensure!(
        input.len() <= MAX_INPUT_BYTES,
        "database exceeds 64 MiB input limit"
    );
    ensure!(input.len() >= ROOT_HEADER, "truncated database root");
    let order = match &input[..4] {
        b"mhbd" => Order::Little,
        b"dbhm" => Order::Big,
        _ => bail!("unsupported database magic"),
    };
    ensure!(
        order.u32(input, 4)? as usize == ROOT_HEADER,
        "unsupported root header length"
    );
    ensure!(
        order.u32(input, 8)? as usize == input.len(),
        "declared database length differs from input"
    );
    ensure!(order.u32(input, 16)? == 117, "unsupported minor version");
    let count = order.u32(input, 20)?;
    ensure!(
        (1..=MAX_SECTIONS).contains(&count),
        "section count absent or exceeds limit"
    );
    let inflated;
    let sections = match (order.u32(input, 12)?, input[0xa8]) {
        (1, 0) => &input[ROOT_HEADER..],
        (2, 1) => {
            inflated = inflate(&input[ROOT_HEADER..], MAX_SECTION_BYTES)?;
            &inflated
        }
        _ => bail!("unsupported version/compression combination"),
    };
    let mut reader = Reader {
        order,
        rows: 0,
        children: 0,
        text_bytes: 0,
    };
    let mut output = Inventory {
        database_pid: order.u64(input, 0x48)?,
        primary_container_pid: 0,
        tracks: BTreeMap::new(),
        albums: BTreeMap::new(),
        artists: BTreeMap::new(),
        track_pids: BTreeSet::new(),
        all_mapped_pids: BTreeSet::new(),
        playlist_pids: BTreeSet::new(),
        saved_media_anchor: None,
    };
    ensure!(output.database_pid != 0, "zero database PID");
    let mut primary_by_dataset = BTreeMap::new();
    let (mut kinds, mut offset) = (BTreeSet::new(), 0);
    for _ in 0..count {
        let section = sections
            .get(offset..)
            .context("section offset exceeds input")?;
        ensure!(section.len() >= SECTION_HEADER, "truncated section header");
        order.magic(section, b"mhsd")?;
        ensure!(
            order.u32(section, 4)? as usize == SECTION_HEADER,
            "unsupported section header length"
        );
        let length = order.u32(section, 8)? as usize;
        ensure!(
            (SECTION_HEADER..=section.len()).contains(&length),
            "section length exceeds bounds"
        );
        let kind = order.u32(section, 12)?;
        ensure!(kinds.insert(kind), "duplicate dataset type");
        let body = &section[SECTION_HEADER..length];
        match kind {
            1 | 6 | 10 => {
                output.tracks.insert(kind, reader.tracks(body, kind)?);
            }
            4 => output.albums = reader.collections(body, true)?,
            8 => output.artists = reader.collections(body, false)?,
            2 | 3 | 5 => {
                let (pids, primary) = reader.playlists(body)?;
                output.playlist_pids.extend(pids);
                if let Some(primary) = primary {
                    primary_by_dataset.insert(kind, primary);
                }
            }
            11 => {
                ensure!(body.len() == 48, "unsupported saved-anchor dataset length");
                output.saved_media_anchor = Some(order.u32(body, 0)?);
            }
            _ => {}
        }
        offset += length;
    }
    ensure!(
        offset == sections.len(),
        "trailing database sections or bytes"
    );
    ensure!(
        output.tracks.contains_key(&1),
        "primary track dataset 1 absent"
    );
    ensure!(
        output.saved_media_anchor.is_some(),
        "saved-anchor dataset 11 absent"
    );
    output.primary_container_pid = *primary_by_dataset
        .get(&3)
        .context("dataset 3 primary container absent")?;
    ensure!(
        primary_by_dataset
            .values()
            .all(|pid| *pid == output.primary_container_pid),
        "conflicting primary containers across datasets"
    );
    resolve_tracks(&mut output)?;
    output.all_mapped_pids = output
        .track_pids
        .iter()
        .copied()
        .chain(output.albums.values().map(|row| row.pid))
        .chain(output.artists.values().map(|row| row.pid))
        .chain(output.playlist_pids.iter().copied())
        .chain(std::iter::once(output.database_pid))
        .collect();
    Ok(output)
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

fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = Decompress::new(true);
    let mut output = Vec::new();
    let mut chunk = [0; 32 * 1024];
    loop {
        let (before_in, before_out) = (decoder.total_in(), decoder.total_out());
        let available = (limit - output.len()).saturating_add(1).min(chunk.len());
        let status = decoder
            .decompress(
                &input[before_in as usize..],
                &mut chunk[..available],
                FlushDecompress::None,
            )
            .context("invalid database zlib stream")?;
        let produced = (decoder.total_out() - before_out) as usize;
        ensure!(
            produced <= limit - output.len(),
            "inflated database exceeds limit"
        );
        output
            .try_reserve_exact(produced)
            .context("allocating bounded database sections")?;
        output.extend_from_slice(&chunk[..produced]);
        if status == Status::StreamEnd {
            ensure!(
                decoder.total_in() == input.len() as u64,
                "trailing bytes or multiple zlib streams"
            );
            return Ok(output);
        }
        ensure!(
            decoder.total_in() > before_in || produced > 0,
            "truncated or stalled zlib stream"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    fn u32_at(data: &mut [u8], at: usize, value: u32, order: Order) {
        data[at..at + 4].copy_from_slice(&match order {
            Order::Little => value.to_le_bytes(),
            Order::Big => value.to_be_bytes(),
        });
    }
    fn u64_at(data: &mut [u8], at: usize, value: u64, order: Order) {
        data[at..at + 8].copy_from_slice(&match order {
            Order::Little => value.to_le_bytes(),
            Order::Big => value.to_be_bytes(),
        });
    }
    fn magic(data: &mut [u8], value: &[u8; 4], order: Order) {
        u32_at(data, 0, u32::from_le_bytes(*value), order);
    }
    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
    fn text(kind: u32, value: &str, order: Order, utf8: bool) -> Vec<u8> {
        let payload: Vec<u8> = if utf8 {
            value.as_bytes().to_vec()
        } else {
            value
                .encode_utf16()
                .flat_map(|unit| match order {
                    Order::Little => unit.to_le_bytes(),
                    Order::Big => unit.to_be_bytes(),
                })
                .collect()
        };
        let mut data = vec![0; 40];
        data.extend_from_slice(&payload);
        let len = data.len() as u32;
        magic(&mut data, b"mhod", order);
        for (at, value) in [
            (4, 24),
            (8, len),
            (12, kind),
            (24, if utf8 { 2 } else { 1 }),
            (28, payload.len() as u32),
        ] {
            u32_at(&mut data, at, value, order);
        }
        data[32] = 1;
        data
    }
    fn track(pid: u64, id: u32, joins: bool, children: &[Vec<u8>], order: Order) -> Vec<u8> {
        let mut data = vec![0; 624];
        data.extend(children.concat());
        let len = data.len() as u32;
        magic(&mut data, b"mhit", order);
        for (at, value) in [
            (4, 624),
            (8, len),
            (12, children.len() as u32),
            (16, id),
            (0x120, if joins { 23 } else { 0 }),
            (0x1e0, if joins { 29 } else { 0 }),
            (0x28, 234567),
            (0x2c, 2),
            (0x30, 12),
            (0x5c, 1),
            (0x60, 2),
            (0x34, 2020),
        ] {
            u32_at(&mut data, at, value, order);
        }
        data[0x1e] = 1;
        u64_at(&mut data, 0x70, pid, order);
        u64_at(&mut data, 0x12c, 0x100000001, order);
        data
    }
    fn collection(album: bool, pid: u64, id: u32, order: Order) -> Vec<u8> {
        let header = if album { 88 } else { 80 };
        let mut data = vec![0; header];
        data.extend(text(
            if album { 200 } else { 300 },
            if album { "Album" } else { "Artist" },
            order,
            false,
        ));
        let len = data.len() as u32;
        magic(&mut data, if album { b"mhia" } else { b"mhii" }, order);
        for (at, value) in [(4, header as u32), (8, len), (12, 1), (16, id)] {
            u32_at(&mut data, at, value, order);
        }
        u64_at(&mut data, 20, pid, order);
        data[28] = 2;
        data
    }
    fn section(kind: u32, body: &[u8], order: Order) -> Vec<u8> {
        let mut data = vec![0; 96];
        data.extend_from_slice(body);
        let len = data.len() as u32;
        magic(&mut data, b"mhsd", order);
        for (at, value) in [(4, 96), (8, len), (12, kind)] {
            u32_at(&mut data, at, value, order);
        }
        data
    }
    fn list(kind: u32, rows: &[Vec<u8>], order: Order) -> Vec<u8> {
        let mut body = vec![0; 92];
        magic(
            &mut body,
            match kind {
                4 => b"mhla",
                8 => b"mhli",
                _ => b"mhlt",
            },
            order,
        );
        u32_at(&mut body, 4, 92, order);
        u32_at(&mut body, 8, rows.len() as u32, order);
        body.extend(rows.concat());
        section(kind, &body, order)
    }
    fn database(sections: &[Vec<u8>], order: Order, compressed: bool) -> Vec<u8> {
        let mut data = vec![0; 244];
        let body = sections.concat();
        data.extend(if compressed { zlib(&body) } else { body });
        let len = data.len() as u32;
        magic(&mut data, b"mhbd", order);
        for (at, value) in [
            (4, 244),
            (8, len),
            (12, if compressed { 2 } else { 1 }),
            (16, 117),
            (20, sections.len() as u32),
        ] {
            u32_at(&mut data, at, value, order);
        }
        data[0xa8] = u8::from(compressed);
        u64_at(&mut data, 0x48, 47, order);
        data
    }
    fn playlists(kind: u32, rows: &[(u64, u8)], order: Order) -> Vec<u8> {
        let mut body = vec![0; 92];
        magic(&mut body, b"mhlp", order);
        u32_at(&mut body, 4, 92, order);
        u32_at(&mut body, 8, rows.len() as u32, order);
        for (pid, flag) in rows {
            let mut row = vec![0; 184];
            magic(&mut row, b"mhyp", order);
            u32_at(&mut row, 4, 184, order);
            u32_at(&mut row, 8, 184, order);
            row[20] = *flag;
            u64_at(&mut row, 28, *pid, order);
            body.extend(row);
        }
        section(kind, &body, order)
    }
    fn fixture(order: Order, compressed: bool) -> Vec<u8> {
        let track = track(
            0x1122334455667788,
            19,
            true,
            &[
                text(1, "Synthetic 🎵", order, false),
                text(3, "Album", order, false),
                text(4, "Artist", order, true),
                text(22, "Album artist", order, true),
            ],
            order,
        );
        let mut anchor = [0; 48];
        u32_at(&mut anchor, 0, 17, order);
        database(
            &[
                list(1, &[track], order),
                list(4, &[collection(true, 31, 23, order)], order),
                list(8, &[collection(false, 37, 29, order)], order),
                section(11, &anchor, order),
                playlists(3, &[(41, 1)], order),
            ],
            order,
            compressed,
        )
    }
    fn simple(rows: &[Vec<u8>], extra: &[Vec<u8>]) -> Vec<u8> {
        let mut sections = vec![
            list(1, rows, Order::Little),
            section(11, &[0; 48], Order::Little),
            playlists(3, &[(41, 1)], Order::Little),
        ];
        sections.extend_from_slice(extra);
        database(&sections, Order::Little, false)
    }
    fn rejected(data: &[u8]) {
        assert!(parse(data).is_err());
    }

    #[test]
    fn reads_current_containers_both_orders_with_unicode_and_joins() {
        for order in [Order::Little, Order::Big] {
            for compressed in [false, true] {
                let state = parse(&fixture(order, compressed)).unwrap();
                let row = &state.primary_tracks()[0];
                assert_eq!(row.title(), Some("Synthetic 🎵"));
                assert_eq!(row.artist(), Some("Artist"));
                assert_eq!(row.album(), Some("Album"));
                assert_eq!(row.album_artist(), Some("Album artist"));
                assert_eq!((row.album_pid, row.artist_pid), (Some(31), Some(37)));
                assert_eq!(row.metadata.file_size, 0x100000001);
                assert_eq!(state.saved_media_anchor(), 17);
                assert_eq!(state.all_mapped_pids().len(), 5);
                assert_eq!(state.database_pid(), 47);
                assert_eq!(state.primary_container_pid(), 41);
            }
        }
    }
    #[test]
    fn preserves_absent_fields_zero_joins_and_empty_primary() {
        let state = parse(&simple(&[], &[])).unwrap();
        assert!(state.primary_tracks().is_empty());
        assert_eq!(state.saved_media_anchor(), 0);
        let state = parse(&simple(&[track(17, 19, false, &[], Order::Little)], &[])).unwrap();
        let row = &state.primary_tracks()[0];
        assert!(row.title().is_none() && row.album_pid.is_none());
    }

    #[test]
    fn converts_only_conservative_native_music_components() {
        assert_eq!(
            music_path(":iTunes_Control:Music:F39:PXMR.m4a").unwrap(),
            "iTunes_Control/Music/F39/PXMR.m4a"
        );
        assert_eq!(
            music_path(":iTunes_Control:Music:F00:a_B-1.mp3").unwrap(),
            "iTunes_Control/Music/F00/a_B-1.mp3"
        );
        for bad in [
            "",
            "iTunes_Control:Music:F39:PXMR.m4a",
            "/iTunes_Control/Music/F39/PXMR.m4a",
            "\\iTunes_Control\\Music\\F39\\PXMR.m4a",
            "C::iTunes_Control:Music:F39:PXMR.m4a",
            ":Other:Music:F39:PXMR.m4a",
            ":iTunes_Control:Podcasts:F39:PXMR.m4a",
            ":iTunes_Control:Music:F9:a.m4a",
            ":iTunes_Control:Music:F999:a.m4a",
            ":iTunes_Control:Music:..:a.m4a",
            ":iTunes_Control:Music:F39:",
            ":iTunes_Control:Music:F39:.",
            ":iTunes_Control:Music:F39:..",
            ":iTunes_Control:Music:F39:a:b.m4a",
            ":iTunes_Control:Music:F39:a/b.m4a",
            ":iTunes_Control:Music:F39:a\\b.m4a",
            ":iTunes_Control:Music:F39:%2E%2E.m4a",
            ":iTunes_Control:Music:F39:α.m4a",
            ":iTunes_Control:Music:F39:a\0.m4a",
            ":iTunes_Control:Music:F39:a\n.m4a",
        ] {
            assert!(music_path(bad).is_err());
        }
        assert!(music_path(&format!(":iTunes_Control:Music:F39:{}", "A".repeat(256))).is_err());
    }

    #[test]
    fn music_path_set_requires_every_represented_track_location() {
        let missing = parse(&simple(&[track(17, 19, false, &[], Order::Little)], &[])).unwrap();
        assert!(missing.music_paths().is_err());
        let child = text(
            2,
            ":iTunes_Control:Music:F39:PXMR.m4a",
            Order::Little,
            false,
        );
        let row = track(17, 19, false, std::slice::from_ref(&child), Order::Little);
        let state = parse(&simple(&[row], &[])).unwrap();
        assert!(
            state
                .music_paths()
                .unwrap()
                .contains("iTunes_Control/Music/F39/PXMR.m4a")
        );
        rejected(&simple(
            &[track(17, 19, false, &[child.clone(), child], Order::Little)],
            &[],
        ));
        let unsafe_row = track(
            17,
            19,
            false,
            &[text(
                2,
                ":iTunes_Control:Music:F39:..",
                Order::Little,
                false,
            )],
            Order::Little,
        );
        assert!(
            parse(&simple(&[unsafe_row], &[]))
                .unwrap()
                .music_paths()
                .is_err()
        );
    }

    #[test]
    fn music_path_union_preserves_distinct_locations_for_shared_track_pid() {
        let first = track(
            17,
            19,
            false,
            &[text(
                2,
                ":iTunes_Control:Music:F39:A.m4a",
                Order::Little,
                false,
            )],
            Order::Little,
        );
        let second = track(
            17,
            19,
            false,
            &[text(
                2,
                ":iTunes_Control:Music:F39:B.m4a",
                Order::Little,
                true,
            )],
            Order::Little,
        );
        let state = parse(&simple(&[first], &[list(10, &[second], Order::Little)])).unwrap();
        assert_eq!(state.track_pids().len(), 1);
        assert_eq!(state.music_paths().unwrap().len(), 2);
        let missing = track(17, 19, false, &[], Order::Little);
        let primary = track(
            17,
            19,
            false,
            &[text(
                2,
                ":iTunes_Control:Music:F39:A.m4a",
                Order::Little,
                false,
            )],
            Order::Little,
        );
        assert!(
            parse(&simple(&[primary], &[list(10, &[missing], Order::Little)]))
                .unwrap()
                .music_paths()
                .is_err()
        );
    }
    #[test]
    fn unions_datasets_without_losing_instance_metadata() {
        let first = track(
            17,
            19,
            false,
            &[text(1, "A", Order::Little, false)],
            Order::Little,
        );
        let mut other = track(17, 19, false, &[], Order::Little);
        u32_at(&mut other, 0x28, 1, Order::Little);
        let extra = list(
            6,
            &[track(23, 29, false, &[], Order::Little)],
            Order::Little,
        );
        let state = parse(&simple(
            &[first],
            &[extra, list(10, &[other], Order::Little)],
        ))
        .unwrap();
        assert_eq!(state.track_pids().len(), 2);
        assert_eq!(state.primary_tracks().len(), 1);
        assert_eq!(state.track_datasets()[&10][0].metadata.total_time_ms, 1);
        assert!(state.track_datasets()[&10][0].title().is_none());
    }
    #[test]
    fn rejects_duplicate_track_and_collection_ids() {
        let row = track(17, 19, false, &[], Order::Little);
        rejected(&simple(&[row.clone(), row], &[]));
        rejected(&simple(
            &[
                track(17, 19, false, &[], Order::Little),
                track(17, 23, false, &[], Order::Little),
            ],
            &[],
        ));
        rejected(&simple(
            &[
                track(17, 19, false, &[], Order::Little),
                track(23, 19, false, &[], Order::Little),
            ],
            &[],
        ));
        let col = collection(true, 31, 23, Order::Little);
        rejected(&simple(&[], &[list(4, &[col.clone(), col], Order::Little)]));
        rejected(&simple(
            &[],
            &[list(
                4,
                &[collection(true, 31, 0, Order::Little)],
                Order::Little,
            )],
        ));
        rejected(&simple(&[track(0, 19, false, &[], Order::Little)], &[]));
    }
    #[test]
    fn rejects_dangling_joins_and_cross_dataset_conflicts() {
        rejected(&simple(&[track(17, 19, true, &[], Order::Little)], &[]));
        let first = track(17, 19, false, &[], Order::Little);
        rejected(&simple(
            std::slice::from_ref(&first),
            &[list(
                10,
                &[track(17, 23, false, &[], Order::Little)],
                Order::Little,
            )],
        ));
        let a = track(
            17,
            19,
            false,
            &[text(1, "A", Order::Little, false)],
            Order::Little,
        );
        let b = track(
            17,
            19,
            false,
            &[text(1, "B", Order::Little, false)],
            Order::Little,
        );
        rejected(&simple(
            &[first],
            &[list(6, &[a], Order::Little), list(10, &[b], Order::Little)],
        ));
    }
    #[test]
    fn rejects_root_versions_lengths_and_input_limit() {
        let data = fixture(Order::Little, false);
        for len in 0..244 {
            rejected(&data[..len]);
        }
        for (offset, value) in [
            (0, 0),
            (4, 243),
            (8, 1),
            (12, 3),
            (16, 116),
            (20, 0),
            (20, 4097),
        ] {
            let mut bad = data.clone();
            u32_at(&mut bad, offset, value, Order::Little);
            rejected(&bad);
        }
        let mut bad = data.clone();
        bad[0xa8] = 1;
        rejected(&bad);
        rejected(&vec![0; MAX_INPUT_BYTES + 1]);
    }
    #[test]
    fn rejects_sections_lists_and_records_with_wrong_bounds() {
        for (offset, value) in [
            (244, 0),
            (248, 95),
            (252, 95),
            (252, u32::MAX),
            (340, 0),
            (344, 91),
            (348, 0),
            (348, u32::MAX),
            (432, 0),
            (436, 623),
            (440, 623),
            (440, u32::MAX),
            (444, 4097),
        ] {
            let mut bad = fixture(Order::Little, false);
            u32_at(&mut bad, offset, value, Order::Little);
            rejected(&bad);
        }
        let mut bad = fixture(Order::Little, false);
        bad.push(0);
        let len = bad.len() as u32;
        u32_at(&mut bad, 8, len, Order::Little);
        rejected(&bad);
    }
    #[test]
    fn requires_one_primary_and_one_saved_anchor_dataset() {
        rejected(&database(
            &[list(1, &[], Order::Little)],
            Order::Little,
            false,
        ));
        rejected(&database(
            &[section(11, &[0; 48], Order::Little)],
            Order::Little,
            false,
        ));
        rejected(&simple(&[], &[list(1, &[], Order::Little)]));
        rejected(&simple(&[], &[section(11, &[0; 48], Order::Little)]));
        rejected(&database(
            &[
                list(1, &[], Order::Little),
                section(11, &[0; 47], Order::Little),
            ],
            Order::Little,
            false,
        ));
    }

    #[test]
    fn validates_current_database_and_primary_container_identity() {
        let state = parse(&simple(
            &[],
            &[playlists(5, &[(41, 1), (53, 0)], Order::Little)],
        ))
        .unwrap();
        assert_eq!(state.playlist_pids().len(), 2);
        assert!(state.all_mapped_pids().contains(&47));
        assert!(state.all_mapped_pids().contains(&53));
        rejected(&simple(&[], &[playlists(5, &[(59, 1)], Order::Little)]));
        rejected(&simple(
            &[],
            &[playlists(5, &[(41, 1), (53, 1)], Order::Little)],
        ));
        rejected(&simple(
            &[],
            &[playlists(5, &[(41, 0), (41, 0)], Order::Little)],
        ));
        rejected(&simple(&[], &[playlists(5, &[(0, 0)], Order::Little)]));
        rejected(&simple(&[], &[playlists(5, &[(53, 2)], Order::Little)]));
        let mut zero = simple(&[], &[]);
        u64_at(&mut zero, 0x48, 0, Order::Little);
        rejected(&zero);
        for rows in [vec![], vec![(41, 0)]] {
            rejected(&database(
                &[
                    list(1, &[], Order::Little),
                    section(11, &[0; 48], Order::Little),
                    playlists(3, &rows, Order::Little),
                ],
                Order::Little,
                false,
            ));
        }
        let mut malformed = playlists(5, &[(53, 0)], Order::Little);
        u32_at(&mut malformed, 96 + 92 + 8, u32::MAX, Order::Little);
        rejected(&simple(&[], &[malformed]));
    }
    #[test]
    fn rejects_text_headers_encodings_duplicates_and_unicode() {
        for (offset, value) in [
            (0, 0),
            (4, 23),
            (8, 23),
            (8, u32::MAX),
            (24, 3),
            (28, 0),
            (28, 1048577),
        ] {
            let mut child = text(1, "abc", Order::Little, false);
            u32_at(&mut child, offset, value, Order::Little);
            rejected(&simple(
                &[track(17, 19, false, &[child], Order::Little)],
                &[],
            ));
        }
        let mut child = text(1, "a", Order::Little, false);
        child[32] = 0;
        rejected(&simple(
            &[track(17, 19, false, &[child], Order::Little)],
            &[],
        ));
        let child = text(1, "a", Order::Little, false);
        rejected(&simple(
            &[track(17, 19, false, &[child.clone(), child], Order::Little)],
            &[],
        ));
        for (utf8, payload) in [(false, vec![0]), (false, vec![0, 0xd8]), (true, vec![0xff])] {
            let mut child = text(1, "", Order::Little, utf8);
            child.extend(&payload);
            let len = child.len() as u32;
            u32_at(&mut child, 8, len, Order::Little);
            u32_at(&mut child, 28, payload.len() as u32, Order::Little);
            rejected(&simple(
                &[track(17, 19, false, &[child], Order::Little)],
                &[],
            ));
        }
    }
    #[test]
    fn bounds_unknown_payloads_without_searching_inside() {
        let mut child = vec![0; 24];
        child.extend(b"mhit is opaque");
        let len = child.len() as u32;
        magic(&mut child, b"mhod", Order::Little);
        for (at, value) in [(4, 24), (8, len), (12, 999)] {
            u32_at(&mut child, at, value, Order::Little);
        }
        let row = track(17, 19, false, &[child], Order::Little);
        assert!(
            parse(&simple(
                &[row],
                &[section(77, b"mhit is opaque", Order::Little)]
            ))
            .is_ok()
        );
    }
    #[test]
    fn enforces_aggregate_limits_and_compilation_boolean() {
        let row = track(
            17,
            19,
            false,
            &[text(1, "A", Order::Little, false)],
            Order::Little,
        );
        let mut reader = Reader {
            order: Order::Little,
            rows: MAX_ROWS,
            children: 0,
            text_bytes: 0,
        };
        let section = list(1, std::slice::from_ref(&row), Order::Little);
        assert!(reader.tracks(&section[96..], 1).is_err());
        reader.children = MAX_TOTAL_CHILDREN;
        assert!(reader.text_children(&row, 624, &[1]).is_err());
        reader.children = 0;
        reader.text_bytes = MAX_TOTAL_TEXT_BYTES;
        assert!(reader.text_children(&row, 624, &[1]).is_err());
        let mut row = row;
        row[0x1e] = 2;
        rejected(&simple(&[row], &[]));
    }
    #[test]
    fn rejects_truncated_bad_checksum_and_extra_zlib_streams() {
        let good = fixture(Order::Little, true);
        let mut truncated = good[..good.len() - 1].to_vec();
        let mut trailing = good.clone();
        trailing.push(0);
        let mut multiple = good.clone();
        multiple.extend(zlib(b""));
        for mut bad in [truncated.clone(), trailing, multiple] {
            let len = bad.len() as u32;
            u32_at(&mut bad, 8, len, Order::Little);
            rejected(&bad);
        }
        truncated = good;
        *truncated.last_mut().unwrap() ^= 1;
        rejected(&truncated);
        let payload = vec![b'a'; 100_000];
        let compressed = zlib(&payload);
        assert_eq!(inflate(&compressed, payload.len()).unwrap(), payload);
        assert!(inflate(&compressed, payload.len() - 1).is_err());
    }
}
