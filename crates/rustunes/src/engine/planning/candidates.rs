//! Ephemeral candidate allocation for a reviewable plan. Nothing is reserved
//! until execution records immutable intent in the journal.
use crate::{
    engine::planning::additive::Candidate, engine::planning::collections::CandidateIds,
    engine::planning::source_collection::SourceCollection,
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::state::journal::Journal,
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::format::Format;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
};

pub fn generate(
    sources: &SourceCollection,
    catalog: &Catalog,
    journal: Option<&Journal>,
) -> Result<BTreeMap<String, Candidate>> {
    let (mut random, mut occupied, mut paths, folders) = allocation_context(catalog, journal)?;
    let mut candidates = BTreeMap::new();
    for source in &sources.tracks {
        let ids = CandidateIds {
            track: next_id(&mut random, &mut occupied)?,
            album: Some(next_id(&mut random, &mut occupied)?),
            artist: Some(next_id(&mut random, &mut occupied)?),
        };
        let path = next_path(&mut random, &folders, &mut paths, ".m4a")?;
        ensure!(
            candidates
                .insert(source.relative_path.clone(), Candidate { ids, path })
                .is_none(),
            "source path repeated during allocation"
        );
    }
    if let Some(journal) = journal
        && journal.outstanding()?.is_none()
    {
        for source in &sources.mp3_tracks {
            let prepared = PreparedSource::read_expected(
                &sources.base_directory.join(&source.relative_path),
                &source.source,
            )?;
            if prepared.ensure_batch_insertion_allowed().is_err() {
                continue;
            }
            // Uses the same in-memory occupied sets, including earlier ALAC
            // candidates. Nothing is reserved and Keep rows ignore proposals.
            let ids = CandidateIds {
                track: next_id(&mut random, &mut occupied)?,
                album: Some(next_id(&mut random, &mut occupied)?),
                artist: Some(next_id(&mut random, &mut occupied)?),
            };
            // A FLAC converted to ALAC lands as an M4A.
            let format = match prepared.source().format() {
                Format::Flac if sources.flac_to_alac => Format::Alac,
                format => format,
            };
            let path = next_path(&mut random, &folders, &mut paths, format.extension())?;
            ensure!(
                candidates
                    .insert(source.relative_path.clone(), Candidate { ids, path })
                    .is_none(),
                "source path repeated during allocation"
            );
        }
    }
    Ok(candidates)
}
type AllocationContext = (File, BTreeSet<u64>, BTreeSet<String>, Vec<String>);
fn allocation_context(catalog: &Catalog, journal: Option<&Journal>) -> Result<AllocationContext> {
    let mut occupied = catalog.represented_sync_ids().clone();
    let mut paths: BTreeSet<_> = catalog
        .music_paths()?
        .into_iter()
        .map(|p| p.to_ascii_lowercase())
        .collect();
    if let Some(journal) = journal {
        occupied.extend(journal.reserved_pids()?);
        paths.extend(journal.reserved_paths()?);
    }
    let known: Vec<_> = catalog
        .music_paths()?
        .iter()
        .map(|p| {
            p.rsplit_once('/')
                .map(|(folder, _)| folder.to_owned())
                .context("catalogue path lacks a music folder")
        })
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .chain(catalog.music_folders().iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let random = File::open("/dev/urandom")?;
    Ok((
        random,
        occupied,
        paths,
        destination_folders(known, catalog.full_model_shape()),
    ))
}

/// The folders new tracks go in. On a phone with no folder (one `rustunes
/// init` prepared), and on any library whose shape the full SQL model of
/// recovery does not cover (not `modelled`: a library the phone built new,
/// the shape `init` prepares on iOS 27.2, or iOS 26's), Apple's own set
/// `F00` to `F49` (docs/02 §12) besides the folders in use, so tracks spread
/// over them as Apple's sync spreads them, not into the one folder a first
/// track went in. The phone creates a missing folder and registers it in
/// `base_location` when it moves the first file in (observed on iOS 27.2,
/// docs/04 §4.4). A modelled library keeps to the folders it uses: the
/// recovery model of a single insertion (`ordinary_sql`) expects its folder
/// registered already. The planner accepts the same folders.
pub(crate) fn destination_folders(known: Vec<String>, modelled: bool) -> Vec<String> {
    if !known.is_empty() && modelled {
        return known;
    }
    let apple = (0..50).map(|group| format!("iTunes_Control/Music/F{group:02}"));
    known
        .into_iter()
        .chain(apple)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

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
    fn a_phone_without_music_folders_or_an_unmodelled_library_gets_apples_fifty() {
        for modelled in [true, false] {
            let folders = destination_folders(Vec::new(), modelled);
            assert_eq!(folders.len(), 50);
            assert_eq!(folders[0], "iTunes_Control/Music/F00");
            assert_eq!(folders[49], "iTunes_Control/Music/F49");
        }
        // A modelled (migrated iOS 27) library keeps to its folders.
        let known = vec!["iTunes_Control/Music/F07".to_owned()];
        assert_eq!(destination_folders(known.clone(), true), known);
        // A library built new, or iOS 26's, spreads over Apple's fifty
        // besides its own.
        let folders = destination_folders(known, false);
        assert_eq!(folders.len(), 50);
        assert!(folders.contains(&"iTunes_Control/Music/F07".to_owned()));
        let odd = vec!["iTunes_Control/Music/F77".to_owned()];
        let folders = destination_folders(odd, false);
        assert_eq!(folders.len(), 51);
        assert_eq!(folders[50], "iTunes_Control/Music/F77");
    }

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
