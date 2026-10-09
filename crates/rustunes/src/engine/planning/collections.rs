//! Offline Music collection selection from a copied live catalogue.
//! No random IDs, device calls, source edits or hidden reservations occur here.
//! Explicit-aART compilation/mixed groups use the same album/artist linkage;
//! first-compilation transitions in ordinary albums remain unsupported.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::engine::planning::source_collection::mp3::preparation::PreparedSource;
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::metadata::Metadata;

#[derive(Clone, Copy, Debug)]
pub struct CandidateIds {
    pub track: u64,
    /// Used only if the selected group does not already exist.
    pub album: Option<u64>,
    pub artist: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    database: [u8; 32],
    wal: Option<[u8; 32]>,
    database_pid: u64,
    primary_container_pid: u64,
    anchor: u32,
}

impl Snapshot {
    fn of(catalog: &Catalog) -> Self {
        Self {
            database: catalog.source_hashes().database,
            wal: catalog.source_hashes().wal,
            database_pid: catalog.database_pid(),
            primary_container_pid: catalog.primary_container_pid(),
            anchor: catalog.persisted_media_anchor(),
        }
    }
}

/// Bound to source wire metadata and one exact catalogue snapshot. Transferring
/// audio still requires the caller's full source fingerprint and fresh Ready.
#[derive(Clone, Debug)]
pub struct PlannedTrack {
    track: u64,
    album: u64,
    artist: u64,
    new_collection_ids: BTreeSet<u64>,
    pending_collection_ids: BTreeSet<u64>,
    snapshot: Snapshot,
    source: [u8; 32],
}

impl PlannedTrack {
    pub fn track_pid(&self) -> u64 {
        self.track
    }
    pub fn album_pid(&self) -> u64 {
        self.album
    }
    pub fn artist_pid(&self) -> u64 {
        self.artist
    }
    /// Only this step's newly allocated roles; exclude reused live or planned IDs.
    pub fn new_collection_ids(&self) -> &BTreeSet<u64> {
        &self.new_collection_ids
    }
    /// Groups introduced earlier in the same plan. Separate live sessions must
    /// reconcile/replan after each success rather than reuse a stale snapshot.
    pub fn pending_collection_ids(&self) -> &BTreeSet<u64> {
        &self.pending_collection_ids
    }

    pub(crate) fn check(&self, source: &Metadata, catalog: &Catalog) -> Result<()> {
        self.check_bound(source_binding(source)?, catalog)
    }

    pub(crate) fn check_mpeg(&self, source: &PreparedSource, catalog: &Catalog) -> Result<()> {
        self.check_bound(mpeg_binding(source)?, catalog)
    }
    fn check_bound(&self, source_binding: [u8; 32], catalog: &Catalog) -> Result<()> {
        ensure!(
            self.snapshot == Snapshot::of(catalog),
            "collection plan catalogue changed; replan from fresh state"
        );
        ensure!(
            self.source == source_binding,
            "source wire metadata changed after collection planning"
        );
        ensure!(
            !catalog.represented_sync_ids().contains(&self.track),
            "track PID already exists in fresh catalogue"
        );
        for id in self
            .new_collection_ids
            .iter()
            .chain(&self.pending_collection_ids)
        {
            ensure!(
                !catalog.represented_sync_ids().contains(id),
                "planned collection PID now exists; reconcile before insertion"
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Artist {
    name: String,
    pid: u64,
    pending: bool,
}
#[derive(Clone)]
struct Album {
    name: String,
    pid: u64,
    artist: u64,
    compilation_group: bool,
    has_compilation_tracks: bool,
    pending: bool,
}

pub struct Planner {
    snapshot: Snapshot,
    occupied: BTreeSet<u64>,
    artists: Vec<Artist>,
    albums: Vec<Album>,
    planned_tracks: usize,
}

impl Planner {
    /// Catalog's reader already requires local item.media_type8 rows. An
    /// emptied group (no live tracks) is reused like any other: the phone
    /// matches groups by name and keeps the row's ID when that ID is sent
    /// (docs/05-receiver-operations.md, section 1, "Existing, emptied and
    /// renamed groups"). An emptied album keeps its `all_compilations`; when
    /// that is set, a compilation track may refill it as it would a new
    /// compilation album (`clear` can leave an album so, section 3). A group
    /// no sync made (sync ID 0: the phone's own, for an entry outside the
    /// library) is matched by name only to refuse the track, as a metadata
    /// edit refuses to join one.
    pub fn new(catalog: &Catalog, reserved_ids: &BTreeSet<u64>) -> Self {
        Self {
            snapshot: Snapshot::of(catalog),
            occupied: catalog
                .represented_sync_ids()
                .union(reserved_ids)
                .copied()
                .collect(),
            artists: catalog
                .artists()
                .values()
                .chain(catalog.unsynced_artists().values())
                .map(|a| Artist {
                    name: a.name.clone(),
                    pid: a.sync_id,
                    pending: false,
                })
                .collect(),
            albums: catalog
                .albums()
                .values()
                .chain(catalog.unsynced_albums().values())
                .map(|a| Album {
                    name: a.name.clone(),
                    pid: a.sync_id,
                    artist: a.artist_sync_id,
                    compilation_group: a.all_compilations || a.has_compilation_tracks,
                    has_compilation_tracks: a.has_compilation_tracks
                        || a.live_track_count == 0 && a.all_compilations,
                    pending: false,
                })
                .collect(),
            planned_tracks: 0,
        }
    }

    /// Failed planning changes no reservations or groups. Supplied candidates
    /// for already-existing roles are unused and never reserved.
    pub fn plan(&mut self, source: &Metadata, candidates: CandidateIds) -> Result<PlannedTrack> {
        let scalar = source.projected()?;
        source.ensure_compilation_profile()?;
        self.plan_fields(
            GroupFields {
                album: scalar.album,
                artist: scalar.effective_artist(),
                album_artist: scalar.album_artist,
                compilation: scalar.compilation,
            },
            source_binding(source)?,
            candidates,
        )
    }

    /// Genuine MPEG metadata enters the same collection selector without an
    /// invented ALAC configuration. This returns a proposal, never authority.
    pub(crate) fn plan_mpeg(
        &mut self,
        source: &PreparedSource,
        candidates: CandidateIds,
    ) -> Result<PlannedTrack> {
        let (metadata, _) = source.coded_insert_fields()?;
        self.plan_fields(
            GroupFields {
                album: &metadata.album,
                artist: metadata.album_artist.as_deref().unwrap_or(&metadata.artist),
                album_artist: metadata.album_artist.as_deref(),
                compilation: metadata.compilation,
            },
            mpeg_binding(source)?,
            candidates,
        )
    }

    fn plan_fields(
        &mut self,
        scalar: GroupFields<'_>,
        binding: [u8; 32],
        candidates: CandidateIds,
    ) -> Result<PlannedTrack> {
        ensure!(
            self.planned_tracks < 50_000,
            "collection plan track bound exceeded"
        );
        let mut allocated = BTreeSet::new();
        let allocate = |pid: u64, allocated: &mut BTreeSet<u64>| -> Result<u64> {
            ensure!(
                pid >= 1000 && !self.occupied.contains(&pid) && allocated.insert(pid),
                "candidate identity is reserved, repeated or collides with current/planned IDs"
            );
            Ok(pid)
        };
        let track = allocate(candidates.track, &mut allocated)?;
        let matching_artists: Vec<_> = self
            .artists
            .iter()
            .filter(|a| same_name(&a.name, scalar.artist))
            .collect();
        ensure!(
            matching_artists.iter().all(|a| a.pid != 0),
            "album artist {:?} has no sync id and cannot be joined",
            scalar.artist
        );
        ensure!(
            matching_artists.len() <= 1,
            "ambiguous artist grouping; no new identity allocated"
        );
        let (artist, new_artist, pending_artist) = if let Some(found) = matching_artists.first() {
            (found.pid, false, found.pending)
        } else {
            (
                allocate(
                    candidates
                        .artist
                        .context("new artist candidate PID required")?,
                    &mut allocated,
                )?,
                true,
                false,
            )
        };
        let matching_albums: Vec<_> = self
            .albums
            .iter()
            .filter(|a| a.artist == artist && same_name(&a.name, scalar.album))
            .collect();
        ensure!(
            matching_albums.iter().all(|a| a.pid != 0),
            "album {:?} by this album artist has no sync id and cannot be joined",
            scalar.album
        );
        ensure!(
            matching_albums.len() <= 1,
            "ambiguous album grouping; no new identity allocated"
        );
        let (album, new_album, pending_album) = if let Some(found) = matching_albums.first() {
            ensure!(
                !found.compilation_group || scalar.album_artist.is_some(),
                "compilation or mixed album reuse requires explicit album artist"
            );
            ensure!(
                scalar.compilation != Some(true) || found.has_compilation_tracks,
                "first compilation in an ordinary album requires a qualified aggregate transition"
            );
            (found.pid, false, found.pending)
        } else {
            (
                allocate(
                    candidates
                        .album
                        .context("new album candidate PID required")?,
                    &mut allocated,
                )?,
                true,
                false,
            )
        };
        let mut new_collection_ids = BTreeSet::new();
        let mut pending_collection_ids = BTreeSet::new();
        for (id, is_new, pending) in [
            (artist, new_artist, pending_artist),
            (album, new_album, pending_album),
        ] {
            if is_new {
                new_collection_ids.insert(id);
            }
            if pending {
                pending_collection_ids.insert(id);
            }
        }
        if new_artist {
            self.artists.push(Artist {
                name: scalar.artist.into(),
                pid: artist,
                pending: true,
            });
        }
        if new_album {
            self.albums.push(Album {
                name: scalar.album.into(),
                pid: album,
                artist,
                compilation_group: scalar.compilation == Some(true),
                has_compilation_tracks: scalar.compilation == Some(true),
                pending: true,
            });
        }
        self.occupied.extend(allocated);
        self.planned_tracks += 1;
        Ok(PlannedTrack {
            track,
            album,
            artist,
            new_collection_ids,
            pending_collection_ids,
            snapshot: self.snapshot.clone(),
            source: binding,
        })
    }
}

struct GroupFields<'a> {
    album: &'a str,
    artist: &'a str,
    album_artist: Option<&'a str>,
    compilation: Option<bool>,
}

fn mpeg_binding(source: &PreparedSource) -> Result<[u8; 32]> {
    let (metadata, timing) = source.coded_insert_fields()?;
    let mut h = Sha256::new();
    h.update(b"rustunes/mpeg-collection-source\0");
    h.update(serde_json::to_vec(&(
        metadata,
        timing,
        &source.source().fingerprint,
    ))?);
    Ok(h.finalize().into())
}

/// A documented subset of native IsEqualUText: never split delimiters, apply
/// Unicode case folding, or discard whitespace. Case aliases match only ASCII.
fn same_name(left: &str, right: &str) -> bool {
    left == right || (left.is_ascii() && right.is_ascii() && left.eq_ignore_ascii_case(right))
}

pub(crate) fn source_binding(source: &Metadata) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(&(
        source.projected()?,
        source.sample_rate,
        source.bit_depth,
        source.channels,
        source.timing.timescale,
        source.timing.duration_ticks,
        source.timing.packets,
    ))?;
    Ok(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Metadata {
        music_protocol::media::metadata::read(
            &crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a"),
        )
        .unwrap()
    }
    fn planner() -> Planner {
        Planner {
            snapshot: Snapshot {
                database: [1; 32],
                wal: None,
                database_pid: 5000,
                primary_container_pid: 6000,
                anchor: 13,
            },
            occupied: BTreeSet::from([2000, 3000, 5000, 6000, 7000]),
            artists: vec![Artist {
                name: "First artist".into(),
                pid: 2000,
                pending: false,
            }],
            albums: vec![Album {
                name: "Example album".into(),
                pid: 3000,
                artist: 2000,
                compilation_group: false,
                has_compilation_tracks: false,
                pending: false,
            }],
            planned_tracks: 0,
        }
    }
    fn candidates(track: u64) -> CandidateIds {
        CandidateIds {
            track,
            album: Some(9000),
            artist: Some(8000),
        }
    }

    #[test]
    fn existing_groups_reuse_role_ids_and_only_track_is_new() {
        let mut p = planner();
        let mut s = source();
        s.artist = vec![
            "FIRST ARTIST".into(),
            "Additional scalar value ignored".into(),
        ];
        let plan = p.plan(&s, candidates(u64::MAX)).unwrap();
        assert_eq!(
            (plan.track_pid(), plan.artist_pid(), plan.album_pid()),
            (u64::MAX, 2000, 3000)
        );
        assert!(plan.new_collection_ids().is_empty());
        assert!(!p.occupied.contains(&8000) && !p.occupied.contains(&9000));
    }

    #[test]
    fn explicit_album_artist_groups_guests_and_new_groups_are_shared() {
        let mut p = planner();
        let mut s = source();
        s.artist = vec!["Guest one".into()];
        s.album_artist = vec!["New ensemble".into()];
        let first = p.plan(&s, candidates(10000)).unwrap();
        assert_eq!(first.new_collection_ids(), &BTreeSet::from([8000, 9000]));
        s.artist = vec!["Guest two".into()];
        let second = p
            .plan(
                &s,
                CandidateIds {
                    track: 10001,
                    album: None,
                    artist: None,
                },
            )
            .unwrap();
        assert_eq!(
            (first.album_pid(), first.artist_pid()),
            (second.album_pid(), second.artist_pid())
        );
        assert!(second.new_collection_ids().is_empty());
        assert_eq!(
            second.pending_collection_ids(),
            &BTreeSet::from([8000, 9000])
        );
    }

    #[test]
    fn compilation_first_shares_explicit_group_with_later_ordinary_members() {
        let mut p = planner();
        let mut s = source();
        s.album_artist = vec!["CLI Compilation Σύνολο".into()];
        s.compilation = Some(true);
        let first = p.plan(&s, candidates(10000)).unwrap();
        for (index, flag) in [Some(false), None, Some(true)].into_iter().enumerate() {
            s.artist = vec![format!("Guest {index}")];
            s.compilation = flag;
            let next = p
                .plan(
                    &s,
                    CandidateIds {
                        track: 10001 + index as u64,
                        album: None,
                        artist: None,
                    },
                )
                .unwrap();
            assert_eq!(
                (next.album_pid(), next.artist_pid()),
                (first.album_pid(), first.artist_pid())
            );
            assert!(next.new_collection_ids().is_empty());
            assert_eq!(next.pending_collection_ids(), &BTreeSet::from([8000, 9000]));
        }
        s.compilation = None;
        s.artist = s.album_artist.clone();
        s.album_artist.clear();
        assert!(p.plan(&s, candidates(11000)).is_err());
        assert!(!p.occupied.contains(&11000));
    }

    #[test]
    fn first_compilation_in_live_or_pending_ordinary_album_refuses_atomically() {
        let mut s = source();
        s.album_artist = vec!["First artist".into()];
        s.compilation = Some(true);
        let mut p = planner();
        assert!(
            p.plan(&s, candidates(10000))
                .unwrap_err()
                .to_string()
                .contains("aggregate transition")
        );
        assert!(!p.occupied.contains(&10000));
        s.album_artist = vec!["New ensemble".into()];
        s.compilation = Some(false);
        p.plan(&s, candidates(10000)).unwrap();
        s.compilation = Some(true);
        assert!(
            p.plan(&s, candidates(10001))
                .unwrap_err()
                .to_string()
                .contains("aggregate transition")
        );
        assert!(!p.occupied.contains(&10001));
    }

    #[test]
    fn live_compilation_and_mixed_groups_reuse_only_unambiguous_supported_linkage() {
        let mut s = source();
        s.album_artist = vec!["Synthetic Artist".into()];
        s.album = vec!["Synthetic Album".into()];
        for summary in [0, 1] {
            let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
                12,
                &format!(
                    "UPDATE item SET is_compilation=1; UPDATE album SET all_compilations={summary} WHERE album_pid=31"
                ),
            );
            let mut p = Planner::new(&catalog, &BTreeSet::new());
            for (index, flag) in [Some(true), Some(false), None].into_iter().enumerate() {
                s.compilation = flag;
                let plan = p.plan(&s, candidates(10000 + index as u64)).unwrap();
                assert_eq!(
                    (plan.album_pid(), plan.artist_pid()),
                    ((-31i64) as u64, (-21i64) as u64)
                );
                assert!(plan.new_collection_ids().is_empty());
            }
        }
        s.compilation = Some(true);
        for sql in [
            "UPDATE item SET is_compilation=1; INSERT INTO album VALUES(33,-33,'Synthetic Album',21,1,NULL)",
            "UPDATE item SET is_compilation=1; INSERT INTO album_artist VALUES(23,-23,'Synthetic Artist',NULL)",
            "UPDATE album SET all_compilations=1 WHERE album_pid=31", // summary alone does not prove a compilation member
        ] {
            let catalog =
                music_protocol::test_support::catalogue::modified_collection_catalog(12, sql);
            let mut p = Planner::new(&catalog, &BTreeSet::new());
            assert!(p.plan(&s, candidates(10000)).is_err());
            assert!(!p.occupied.contains(&10000));
        }
        // An emptied ordinary album takes no first compilation either.
        s.album = vec!["Emptied Album".into()];
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            12,
            "INSERT INTO album VALUES(33,-33,'Emptied Album',21,0,NULL)",
        );
        let error = Planner::new(&catalog, &BTreeSet::new())
            .plan(&s, candidates(10000))
            .unwrap_err();
        assert!(
            error.to_string().contains("aggregate transition"),
            "{error:#}"
        );
        s.album = vec!["Synthetic Album".into()];
        s.album_artist = vec!["Orphan Artist".into()];
        s.album = vec!["Orphan Album".into()];
        assert!(
            Planner::new(
                &music_protocol::test_support::catalogue::collection_catalog(12),
                &BTreeSet::new()
            )
            .plan(&s, candidates(10000))
            .is_err()
        );
    }

    #[test]
    fn groups_no_sync_made_are_never_joined() {
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            12,
            "INSERT INTO album_artist VALUES(23,0,'Streamed Artist',NULL);
             INSERT INTO album VALUES(33,0,'Streamed Album',23,0,NULL);
             INSERT INTO album VALUES(34,0,'Streamed Album',21,0,NULL);",
        );
        let mut p = Planner::new(&catalog, &BTreeSet::new());
        let mut s = source();
        s.compilation = None;
        s.album = vec!["Streamed Album".into()];
        for (artist, refusal) in [
            (
                "STREAMED ARTIST",
                "album artist \"STREAMED ARTIST\" has no sync id",
            ),
            (
                "Synthetic Artist",
                "album \"Streamed Album\" by this album artist",
            ),
        ] {
            s.album_artist = vec![artist.into()];
            let error = p.plan(&s, candidates(10000)).unwrap_err();
            assert!(error.to_string().contains(refusal), "{error:#}");
            assert!(!p.occupied.contains(&10000));
        }
        s.album = vec!["Synthetic Album".into()];
        let plan = p.plan(&s, candidates(10000)).unwrap();
        assert_eq!(
            (plan.album_pid(), plan.artist_pid()),
            ((-31i64) as u64, (-21i64) as u64)
        );
    }

    #[test]
    fn emptied_compilation_album_takes_compilation_and_ordinary_members() {
        let mut s = source();
        s.album_artist = vec!["Synthetic Artist".into()];
        s.album = vec!["Emptied Album".into()];
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            12,
            "INSERT INTO album VALUES(33,-33,'Emptied Album',21,1,NULL)",
        );
        let mut p = Planner::new(&catalog, &BTreeSet::new());
        for (index, flag) in [Some(true), Some(false), Some(true)]
            .into_iter()
            .enumerate()
        {
            s.compilation = flag;
            let plan = p.plan(&s, candidates(10000 + index as u64)).unwrap();
            assert_eq!(
                (plan.album_pid(), plan.artist_pid()),
                ((-33i64) as u64, (-21i64) as u64)
            );
            assert!(plan.new_collection_ids().is_empty());
        }
        // Its compilation summary still demands an explicit album artist.
        s.album_artist.clear();
        s.artist = vec!["Synthetic Artist".into()];
        s.compilation = None;
        let mut p = Planner::new(&catalog, &BTreeSet::new());
        assert!(p.plan(&s, candidates(10000)).is_err());
    }

    #[test]
    fn existing_artist_new_album_allocates_only_album_and_missing_candidates_are_atomic() {
        let mut p = planner();
        let mut s = source();
        s.album = vec!["New album".into()];
        assert!(
            p.plan(
                &s,
                CandidateIds {
                    track: 10000,
                    album: None,
                    artist: None
                }
            )
            .is_err()
        );
        assert!(!p.occupied.contains(&10000));
        let plan = p.plan(&s, candidates(10000)).unwrap();
        assert_eq!(plan.artist_pid(), 2000);
        assert_eq!(plan.new_collection_ids(), &BTreeSet::from([9000]));
    }

    #[test]
    fn collisions_ambiguous_groups_compilation_and_orphans_never_allocate() {
        let s = source();
        for pid in [0, 999, 2000, 7000] {
            assert!(planner().plan(&s, candidates(pid)).is_err());
        }
        let mut p = planner();
        p.artists.push(Artist {
            name: "FIRST ARTIST".into(),
            pid: 2001,
            pending: false,
        });
        assert!(p.plan(&s, candidates(10000)).is_err());
        assert!(!p.occupied.contains(&10000));
        let mut s = source();
        s.compilation = Some(true);
        assert!(planner().plan(&s, candidates(10000)).is_err());
        s.compilation = Some(false);
        s.artist = vec!["New".into()];
        assert!(
            planner()
                .plan(
                    &s,
                    CandidateIds {
                        track: 10000,
                        album: Some(8000),
                        artist: Some(8000)
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn names_never_split_delimiters_or_apply_unicode_casefold() {
        assert!(same_name("Example", "EXAMPLE"));
        assert!(same_name("Ελλάδα", "Ελλάδα"));
        for (a, b) in [("A;B", "A, B"), ("ABC", " ABC"), ("Σ", "σ"), ("É", "é")] {
            assert!(!same_name(a, b));
        }
    }
}
