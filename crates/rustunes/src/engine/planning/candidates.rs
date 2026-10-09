//! Ephemeral candidate allocation for a reviewable plan. Nothing is reserved
//! until execution records immutable intent in the journal.
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::format::Format;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
};
type AllocationContext = (File, BTreeSet<u64>, BTreeSet<String>, Vec<String>);

fn word(random: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    random.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
fn next_id(random: &mut impl Read, occupied: &mut BTreeSet<u64>) -> Result<u64> {
    for _ in 0..128 {
        let id = word(random)?;
        if id >= 1000 && occupied.insert(id) {
            return Ok(id);
        }
    }
    anyhow::bail!("unable to allocate a unique candidate identity within the attempt bound")
}

/// An unused `Fnn/XXXX` path with `extension` (its dot included).
fn next_path(
    random: &mut impl Read,
    folders: &[String],
    occupied: &mut BTreeSet<String>,
    extension: &str,
) -> Result<String> {
    ensure!(!folders.is_empty(), "no known destination folders");
    for _ in 0..128 {
        let mut bits = word(random)?;
        let folder = &folders[(bits % (folders.len() as u64)) as usize];
        bits /= folders.len() as u64;
        let mut name = String::with_capacity(4);
        for _ in 0..4 {
            name.push(char::from(b'A' + (bits % 26) as u8));
            bits /= 26;
        }
        let path = format!("{folder}/{name}{extension}");
        if occupied.insert(path.to_ascii_lowercase()) {
            return Ok(path);
        }
    }
    anyhow::bail!("unable to allocate an unused Music path within the attempt bound")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skips_reserved_ids_and_casefolded_paths_without_recycling() {
        let words = [0u64, 1000, 1001];
        let bytes: Vec<_> = words.into_iter().flat_map(u64::to_le_bytes).collect();
        let mut occupied = BTreeSet::from([1000]);
        assert_eq!(next_id(&mut bytes.as_slice(), &mut occupied).unwrap(), 1001);
        let folders = vec!["iTunes_Control/Music/F00".into()];
        let bytes: Vec<_> = [0u64, 1].into_iter().flat_map(u64::to_le_bytes).collect();
        let mut paths = BTreeSet::from(["itunes_control/music/f00/aaaa.m4a".into()]);
        assert_eq!(
            next_path(&mut bytes.as_slice(), &folders, &mut paths, ".m4a").unwrap(),
            "iTunes_Control/Music/F00/BAAA.m4a"
        );
        assert_eq!(paths.len(), 2);
        paths.insert("itunes_control/music/f00/aaaa.mp3".into());
        let bytes: Vec<_> = [0u64, 1].into_iter().flat_map(u64::to_le_bytes).collect();
        assert_eq!(
            next_path(&mut bytes.as_slice(), &folders, &mut paths, ".mp3").unwrap(),
            "iTunes_Control/Music/F00/BAAA.mp3"
        );
        assert!(paths.contains("itunes_control/music/f00/baaa.m4a"));
        assert!(paths.contains("itunes_control/music/f00/baaa.mp3"));
        assert!(next_path(&mut [0; 8].as_slice(), &[], &mut paths, ".m4a").is_err());
        let bytes: Vec<_> = [2u64].into_iter().flat_map(u64::to_le_bytes).collect();
        assert_eq!(
            next_path(&mut bytes.as_slice(), &folders, &mut paths, ".flac").unwrap(),
            "iTunes_Control/Music/F00/CAAA.flac"
        );
    }
}
