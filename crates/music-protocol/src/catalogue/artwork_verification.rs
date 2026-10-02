//! Independent checks for one artwork-only change between read-only catalogs.
//! This verifies projected SQL state, not JPEG pixels, cache files, or audio bytes.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::catalogue::sqlite::{
    Album, Artist, CachedArtwork, Catalog, PlaybackDates, PlaybackStatistics, Track,
};

/// Both recent counters are preserved exactly, or every positive recent play
/// and skip count is reset to zero together. No lifetime/date/other exception.
pub const RECENT_COUNT_IMPORT_RULE: &str = "all_positive_recent_play_and_skip_counts_to_zero";

/// Receipt label for the reset of positive recent play counts; when recent
/// skip counts reset too, `RECENT_COUNT_IMPORT_RULE` is reported beside it.
/// It does not itself authorize a skip-counter change.
pub const RECENT_PLAY_IMPORT_RULE: &str = "all_positive_play_count_recent_to_zero";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecentPlayCountReset {
    pub track_id: String,
    pub before: u32,
    pub after: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecentSkipCountReset {
    pub track_id: String,
    pub before: u32,
    pub after: u32,
}

pub fn possible_recent_skip_count_resets(catalog: &Catalog) -> Vec<RecentSkipCountReset> {
    recent_skip_reset_projection(catalog.tracks())
}
fn recent_skip_reset_projection(tracks: &[Track]) -> Vec<RecentSkipCountReset> {
    let mut result: Vec<_> = tracks
        .iter()
        .filter(|t| t.playback.skip_count_recent > 0)
        .map(|t| RecentSkipCountReset {
            track_id: format!("{:016x}", t.sync_id),
            before: t.playback.skip_count_recent,
            after: 0,
        })
        .collect();
    result.sort_by(|a, b| a.track_id.cmp(&b.track_id));
    result
}

/// Exact current counters potentially consumed by an artwork import. This is
/// disclosure of a qualified side effect, not a claim that a future import ran.
pub fn possible_recent_play_count_resets(catalog: &Catalog) -> Vec<RecentPlayCountReset> {
    recent_reset_projection(catalog.tracks())
}

fn recent_reset_projection(tracks: &[Track]) -> Vec<RecentPlayCountReset> {
    let mut result: Vec<_> = tracks
        .iter()
        .filter(|t| t.playback.play_count_recent > 0)
        .map(|track| RecentPlayCountReset {
            track_id: format!("{:016x}", track.sync_id),
            before: track.playback.play_count_recent,
            after: 0,
        })
        .collect();
    result.sort_by(|a, b| a.track_id.cmp(&b.track_id));
    result
}

/// SQL transition only. The verification policy determines whether the album
/// representative must also equal the target track's requested cover.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumRegistrationDisposition {
    NewCache,
    UnchangedPreviousCache,
}

#[derive(Serialize)]
pub struct VerifiedArtwork {
    pub schema: &'static str,
    pub target_track_id: String,
    pub target_album_id: String,
    pub prior_album_cache_id: Option<u32>,
    pub current_album_cache_id: u32,
    pub album_registration_disposition: AlbumRegistrationDisposition,
    pub permanent_album_original_equality_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_verification_policy: Option<&'static str>,
    pub cache_id: u32,
    pub media_anchor: u32,
    pub before_database_sha256: String,
    pub before_wal_sha256: Option<String>,
    pub after_database_sha256: String,
    pub after_wal_sha256: Option<String>,
    pub prior_tracks_preserved: usize,
    pub prior_albums_preserved: usize,
    pub prior_artists_preserved: usize,
    pub target_track_and_album_registrations_match: bool,
    pub other_supported_artwork_registrations_preserved: bool,
    pub occupied_cache_ids_before: usize,
    pub occupied_cache_ids_after: usize,
    pub cached_sql_ids_before: usize,
    pub cached_sql_ids_after: usize,
    pub existing_cached_artwork_objects_preserved: bool,
    pub unmapped_source300_token_rows_before: usize,
    pub unmapped_source300_token_rows_after: usize,
    pub audio_and_artwork_bytes_verified_by_this_check: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_play_count_resets: Vec<RecentPlayCountReset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_play_count_import_rule: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_skip_count_resets: Vec<RecentSkipCountReset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_count_import_rule: Option<&'static str>,
    /// Tracks played or skipped on the phone between the write and a later
    /// run's capture (the later-run profile only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tracks_listened_between_runs: Vec<ListenedTrack>,
    pub unverified_fields: [&'static str; 4],
}

/// One track's playback between the write and a later capture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListenedTrack {
    pub track_id: String,
    pub plays: u32,
    pub skips: u32,
    pub play_count_recent: [u32; 2],
    pub skip_count_recent: [u32; 2],
}

/// Accepts one track's playback change between runs (lifetime counts only
/// grow, recent counters kept or restarted, rating and like state exact).
pub type Listening<'a> = &'a dyn Fn(&PlaybackStatistics, &PlaybackStatistics) -> Result<()>;

/// Requires one fresh cache ID and exactly the expected local source300 track
/// registration. The target album may use that new ID or its exact prior ID.
/// Caller must prove the permanent album original equals the requested cover,
/// and separately verify source ownership,
/// reservation, transfer completion, full audio, JPEG and pixel/cache evidence.
pub fn verify(
    before: &Catalog,
    after: &Catalog,
    target_sync_id: u64,
    cache_id: u32,
) -> Result<VerifiedArtwork> {
    verify_policy(before, after, target_sync_id, cache_id, true, None)
}

/// The same strict catalog transition, with an independent shared album image.
/// The target track's exact permanent original must still be verified. Albums
/// protected by add-only policy require a separate before/after byte proof;
/// this SQL check grants no authority to replace their representative image.
pub fn verify_independent_representative(
    before: &Catalog,
    after: &Catalog,
    target_sync_id: u64,
    cache_id: u32,
) -> Result<VerifiedArtwork> {
    verify_policy(before, after, target_sync_id, cache_id, false, None)
}

/// The independent-representative transition observed by a later run, after
/// the phone may have been played: each track's playback may differ from
/// `before` only as `listening` accepts, and its play or skip date only when
/// that count grew, and later. Every other statistic, typed field and
/// registration is compared exactly; the write's own recent-counter reset is
/// one of the changes `listening` accepts.
pub fn verify_independent_representative_after_listening(
    before: &Catalog,
    after: &Catalog,
    target_sync_id: u64,
    cache_id: u32,
    listening: Listening<'_>,
) -> Result<VerifiedArtwork> {
    verify_policy(
        before,
        after,
        target_sync_id,
        cache_id,
        false,
        Some(listening),
    )
}

fn verify_policy(
    before: &Catalog,
    after: &Catalog,
    target_sync_id: u64,
    cache_id: u32,
    album_equality_required: bool,
    listening: Option<Listening<'_>>,
) -> Result<VerifiedArtwork> {
    ensure!(
        before.composers() == after.composers()
            && before.track_composers() == after.track_composers(),
        "artwork update changed composer lookup, sort label or track order"
    );
    ensure!(
        before
            .tracks()
            .iter()
            .all(|t| before.title_sort(t.sync_id) == after.title_sort(t.sync_id)),
        "artwork update changed an exact title sort value"
    );
    let (album_id, recent_play_count_resets, recent_skip_count_resets, listened) = check_views(
        &View::from(before),
        &View::from(after),
        target_sync_id,
        cache_id,
        listening,
    )?;
    Ok(VerifiedArtwork {
        schema: "music-protocol.artwork-catalog-verification",
        target_track_id: format!("{target_sync_id:016x}"),
        target_album_id: format!("{album_id:016x}"),
        prior_album_cache_id: before.artwork().album_cache_ids().get(&album_id).copied(),
        current_album_cache_id: after.artwork().album_cache_ids()[&album_id],
        album_registration_disposition: if after.artwork().album_cache_ids()[&album_id] == cache_id
        {
            AlbumRegistrationDisposition::NewCache
        } else {
            AlbumRegistrationDisposition::UnchangedPreviousCache
        },
        permanent_album_original_equality_required: album_equality_required,
        album_verification_policy: (!album_equality_required)
            .then_some("independent_representative"),
        cache_id,
        media_anchor: after.persisted_media_anchor(),
        before_database_sha256: hex(&before.source_hashes().database),
        before_wal_sha256: before.source_hashes().wal.map(|value| hex(&value)),
        after_database_sha256: hex(&after.source_hashes().database),
        after_wal_sha256: after.source_hashes().wal.map(|value| hex(&value)),
        prior_tracks_preserved: before.tracks().len(),
        prior_albums_preserved: before.albums().len(),
        prior_artists_preserved: before.artists().len(),
        target_track_and_album_registrations_match: after.artwork().album_cache_ids()[&album_id]
            == cache_id,
        other_supported_artwork_registrations_preserved: true,
        occupied_cache_ids_before: before.artwork().occupied_cache_ids().len(),
        occupied_cache_ids_after: after.artwork().occupied_cache_ids().len(),
        cached_sql_ids_before: before.artwork().cached_ids().len(),
        cached_sql_ids_after: after.artwork().cached_ids().len(),
        existing_cached_artwork_objects_preserved: true,
        unmapped_source300_token_rows_before: before.artwork().unmapped_source300_token_rows(),
        unmapped_source300_token_rows_after: after.artwork().unmapped_source300_token_rows(),
        audio_and_artwork_bytes_verified_by_this_check: false,
        recent_play_count_import_rule: (!recent_play_count_resets.is_empty())
            .then_some(RECENT_PLAY_IMPORT_RULE),
        recent_play_count_resets,
        recent_count_import_rule: (!recent_skip_count_resets.is_empty())
            .then_some(RECENT_COUNT_IMPORT_RULE),
        recent_skip_count_resets,
        tracks_listened_between_runs: listened,
        unverified_fields: [
            "SQL columns not projected by Catalog, except exact item_stats preservation fingerprints",
            "Foreign artwork sources and unsupported source300 relationship shapes",
            "AFC artwork files, dimensions, JPEG bytes and rendered pixels",
            "Source ownership, reservation, protocol completion and full audio bytes",
        ],
    })
}

// A borrowed view makes the same checks testable without exposing a constructor
// for Catalog or accepting caller-supplied fields in the public verifier API.
struct View<'a> {
    database: u64,
    container: u64,
    anchor: u32,
    tracks: &'a [Track],
    stats_except_recent_play: &'a BTreeMap<u64, [u8; 32]>,
    stats_except_recent_play_and_skip: &'a BTreeMap<u64, [u8; 32]>,
    stats_except_playback: &'a BTreeMap<u64, [u8; 32]>,
    playback_dates: &'a BTreeMap<u64, PlaybackDates>,
    albums: &'a BTreeMap<i64, Album>,
    artists: &'a BTreeMap<i64, Artist>,
    represented: &'a BTreeSet<u64>,
    occupied: &'a BTreeSet<u32>,
    cached: &'a BTreeSet<u32>,
    cached_objects: &'a BTreeMap<(u32, i64), CachedArtwork>,
    track_art: &'a BTreeMap<u64, u32>,
    album_art: &'a BTreeMap<u64, u32>,
}

impl<'a> From<&'a Catalog> for View<'a> {
    fn from(catalog: &'a Catalog) -> Self {
        Self {
            database: catalog.database_pid(),
            container: catalog.primary_container_pid(),
            anchor: catalog.persisted_media_anchor(),
            tracks: catalog.tracks(),
            stats_except_recent_play: catalog.stats_except_recent_play(),
            stats_except_recent_play_and_skip: catalog.stats_except_recent_play_and_skip(),
            stats_except_playback: catalog.stats_except_playback(),
            playback_dates: catalog.playback_dates(),
            albums: catalog.albums(),
            artists: catalog.artists(),
            represented: catalog.represented_sync_ids(),
            occupied: catalog.artwork().occupied_cache_ids(),
            cached: catalog.artwork().cached_ids(),
            cached_objects: catalog.artwork().cached_objects(),
            track_art: catalog.artwork().track_cache_ids(),
            album_art: catalog.artwork().album_cache_ids(),
        }
    }
}

type Verified = (
    u64,
    Vec<RecentPlayCountReset>,
    Vec<RecentSkipCountReset>,
    Vec<ListenedTrack>,
);

#[cfg(test)]
fn verify_views(
    before: &View<'_>,
    after: &View<'_>,
    target: u64,
    cache: u32,
) -> Result<(u64, Vec<RecentPlayCountReset>, Vec<RecentSkipCountReset>)> {
    let (album, resets, skip_resets, _) = check_views(before, after, target, cache, None)?;
    Ok((album, resets, skip_resets))
}

fn check_views(
    before: &View<'_>,
    after: &View<'_>,
    target: u64,
    cache: u32,
    listening: Option<Listening<'_>>,
) -> Result<Verified> {
    ensure!(
        before.database == after.database && before.container == after.container,
        "catalog identity changed across artwork update"
    );
    ensure!(
        after.anchor
            == before
                .anchor
                .checked_add(1)
                .context("Media anchor exhausted")?,
        "artwork update did not advance the Media anchor by exactly one"
    );
    ensure!(
        cache >= 100 && !before.occupied.contains(&cache),
        "artwork cache ID is reserved or already occupied"
    );
    let old: BTreeMap<_, _> = before
        .tracks
        .iter()
        .map(|track| (track.sync_id, track))
        .collect();
    let new: BTreeMap<_, _> = after
        .tracks
        .iter()
        .map(|track| (track.sync_id, track))
        .collect();
    ensure!(
        old.len() == before.tracks.len()
            && new.len() == after.tracks.len()
            && old.keys().eq(new.keys()),
        "artwork update added, removed or duplicated a track"
    );
    let (resets, skip_resets, listened) = match listening {
        Some(listening) => (
            Vec::new(),
            Vec::new(),
            listened_tracks(before, after, &old, &new, listening)?,
        ),
        None => {
            let (resets, skip_resets) = exact_tracks(before, after, &old, &new)?;
            (resets, skip_resets, Vec::new())
        }
    };
    rest_of_views(
        before,
        after,
        &old,
        target,
        cache,
        resets,
        skip_resets,
        listened,
    )
}

/// Every track and statistic exact, except the qualified recent-count reset.
fn exact_tracks(
    before: &View<'_>,
    after: &View<'_>,
    old: &BTreeMap<u64, &Track>,
    new: &BTreeMap<u64, &Track>,
) -> Result<(Vec<RecentPlayCountReset>, Vec<RecentSkipCountReset>)> {
    let skip_unchanged = old
        .iter()
        .all(|(id, t)| t.playback.skip_count_recent == new[id].playback.skip_count_recent);
    let (old_stats, new_stats) = if skip_unchanged {
        (
            before.stats_except_recent_play,
            after.stats_except_recent_play,
        )
    } else {
        (
            before.stats_except_recent_play_and_skip,
            after.stats_except_recent_play_and_skip,
        )
    };
    ensure!(
        old_stats == new_stats && old_stats.keys().eq(old.keys()),
        "artwork update changed statistics outside the qualified recent-count reset"
    );
    let recent_unchanged = skip_unchanged
        && old
            .iter()
            .all(|(id, t)| t.playback.play_count_recent == new[id].playback.play_count_recent);
    let recent_all_zero = new
        .values()
        .all(|t| t.playback.play_count_recent == 0 && t.playback.skip_count_recent == 0);
    ensure!(
        recent_unchanged || recent_all_zero,
        "artwork import must preserve both recent counters or reset every positive play and skip count to zero"
    );
    for (id, track) in old {
        let mut expected = (**track).clone();
        if !recent_unchanged {
            expected.playback.play_count_recent = 0;
            expected.playback.skip_count_recent = 0;
        }
        ensure!(
            &expected == new[id],
            "artwork update changed a typed track field outside the qualified recent-count reset"
        );
    }
    let resets = if recent_unchanged {
        Vec::new()
    } else {
        recent_reset_projection(before.tracks)
    };
    let skip_resets = if recent_unchanged {
        Vec::new()
    } else {
        recent_skip_reset_projection(before.tracks)
    };
    Ok((resets, skip_resets))
}

/// Every track and statistic exact except listening between runs, which also
/// covers the write's own recent-counter reset.
fn listened_tracks(
    before: &View<'_>,
    after: &View<'_>,
    old: &BTreeMap<u64, &Track>,
    new: &BTreeMap<u64, &Track>,
    listening: Listening<'_>,
) -> Result<Vec<ListenedTrack>> {
    ensure!(
        before.stats_except_playback == after.stats_except_playback
            && before.stats_except_playback.keys().eq(old.keys()),
        "artwork update changed statistics outside playback"
    );
    let mut listened = Vec::new();
    for (id, track) in old {
        let (was, now) = (&track.playback, &new[id].playback);
        let dates = |view: &View<'_>| view.playback_dates.get(id).copied().unwrap_or_default();
        let (was_dates, now_dates) = (dates(before), dates(after));
        let mut expected = (*track).clone();
        expected.playback = now.clone();
        ensure!(
            &expected == new[id],
            "artwork update changed a typed track field outside playback"
        );
        if was == now && was_dates == now_dates {
            continue;
        }
        listening(was, now).with_context(|| format!("track {id:016x}"))?;
        let plays = now.play_count_user - was.play_count_user;
        let skips = now.skip_count_user - was.skip_count_user;
        for (field, events, prior, later) in [
            ("date_played", plays, was_dates.played, now_dates.played),
            ("date_skipped", skips, was_dates.skipped, now_dates.skipped),
        ] {
            ensure!(
                if events == 0 {
                    prior.to_bits() == later.to_bits()
                } else {
                    later > prior
                },
                "{field} of track {id:016x} is not explained by its plays and skips"
            );
        }
        listened.push(ListenedTrack {
            track_id: format!("{id:016x}"),
            plays,
            skips,
            play_count_recent: [was.play_count_recent, now.play_count_recent],
            skip_count_recent: [was.skip_count_recent, now.skip_count_recent],
        });
    }
    Ok(listened)
}

/// Target availability, collections, registrations and cached objects.
#[allow(clippy::too_many_arguments)]
fn rest_of_views(
    before: &View<'_>,
    after: &View<'_>,
    old: &BTreeMap<u64, &Track>,
    target: u64,
    cache: u32,
    resets: Vec<RecentPlayCountReset>,
    skip_resets: Vec<RecentSkipCountReset>,
    listened: Vec<ListenedTrack>,
) -> Result<Verified> {
    let target = old
        .get(&target)
        .context("artwork target is absent or ambiguous")?;
    ensure!(
        target.afc_path.is_some()
            && !target.availability.is_placeholder
            && target.availability.is_library_added
            && target.availability.has_non_purgeable_asset
            && target.availability.is_supported_type,
        "artwork target is not an available supported local track"
    );
    ensure!(
        before.albums == after.albums
            && before.artists == after.artists
            && before.represented == after.represented,
        "artwork update changed a collection, membership count or mapped identity"
    );
    let mut expected_track_art = before.track_art.clone();
    expected_track_art.insert(target.sync_id, cache);
    let observed_album_cache = *after
        .album_art
        .get(&target.album_sync_id)
        .context("target album artwork registration missing")?;
    ensure!(
        observed_album_cache == cache
            || before.album_art.get(&target.album_sync_id) == Some(&observed_album_cache),
        "target album cache is neither the new ID nor its unchanged prior ID"
    );
    let mut expected_album_art = before.album_art.clone();
    expected_album_art.insert(target.album_sync_id, observed_album_cache);
    ensure!(
        *after.track_art == expected_track_art && *after.album_art == expected_album_art,
        "artwork registrations differ from the target track and album update"
    );
    let mut expected_occupied = before.occupied.clone();
    expected_occupied.insert(cache);
    // A track or album an interrupted session pointed at a cover that never
    // arrived registers an ID with no cached object; when every track and
    // album registering it takes the new cover, that ID leaves the namespace
    // with its last registration.
    let album_moved = observed_album_cache == cache;
    let track_prior = before.track_art.get(&target.sync_id);
    let album_prior = before
        .album_art
        .get(&target.album_sync_id)
        .filter(|_| album_moved);
    for prior in [track_prior, album_prior].into_iter().flatten() {
        if *prior != cache
            && !before.cached.contains(prior)
            && before
                .track_art
                .iter()
                .all(|(track, id)| id != prior || *track == target.sync_id)
            && before
                .album_art
                .iter()
                .all(|(album, id)| id != prior || (*album == target.album_sync_id && album_moved))
        {
            expected_occupied.remove(prior);
        }
    }
    ensure!(
        *after.occupied == expected_occupied,
        "artwork namespace lost a prior ID or gained an unexpected ID"
    );
    let mut expected_cached = before.cached.clone();
    expected_cached.insert(cache);
    ensure!(
        *after.cached == expected_cached,
        "cached SQL artwork IDs differ from the one new cache object"
    );
    ensure!(
        after.cached_objects.len() == before.cached_objects.len() + 1,
        "artwork cached-object row count differs from one new object"
    );
    for (key, old) in before.cached_objects {
        ensure!(
            after.cached_objects.get(key) == Some(old),
            "pre-existing cached artwork row changed or disappeared"
        );
    }
    let added = after
        .cached_objects
        .get(&(cache, 0))
        .context("new source300 cached artwork object missing")?;
    ensure!(
        added.cache_id == cache && added.variant == 0 && added.artwork_type == 6,
        "new cached artwork object has unsupported shape"
    );
    Ok((target.album_sync_id, resets, skip_resets, listened))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_refuses_any_composer_lookup_or_order_change() {
        let before = crate::test_support::catalogue::composer_transition_catalog(
            "ACME",
            Some("ACME"),
            100,
            false,
        );
        for (sort, order) in [("Acme", 100), ("ACME", 101)] {
            let after = crate::test_support::catalogue::composer_transition_catalog(
                "ACME",
                Some(sort),
                order,
                false,
            );
            let error = verify(&before, &after, u64::MAX, 100)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("composer lookup"));
        }
    }

    struct Fixture {
        before: Catalog,
        after: Catalog,
        track_art: BTreeMap<u64, u32>,
        album_art: BTreeMap<u64, u32>,
        occupied: BTreeSet<u32>,
        cached: BTreeSet<u32>,
        cached_objects: BTreeMap<(u32, i64), CachedArtwork>,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                before: crate::test_support::catalogue::collection_catalog(13),
                after: crate::test_support::catalogue::collection_catalog(14),
                track_art: BTreeMap::from([(u64::MAX, 100)]),
                album_art: BTreeMap::from([((-31i64 as u64), 100)]),
                occupied: BTreeSet::from([100]),
                cached: BTreeSet::from([100]),
                cached_objects: BTreeMap::from([(
                    (100, 0),
                    CachedArtwork {
                        cache_id: 100,
                        artwork_type: 6,
                        variant: 0,
                        relative_path: "aa/00000000000000000000000000000000000000".into(),
                    },
                )]),
            }
        }
        fn views(&self) -> (View<'_>, View<'_>) {
            let before = View::from(&self.before);
            let mut after = View::from(&self.after);
            after.track_art = &self.track_art;
            after.album_art = &self.album_art;
            after.occupied = &self.occupied;
            after.cached = &self.cached;
            after.cached_objects = &self.cached_objects;
            (before, after)
        }
    }

    #[test]
    fn accepts_only_expected_artwork_transition_and_preserves_retained_cache_ids() {
        let fixture = Fixture::new();
        let (mut before, mut after) = fixture.views();
        assert!(verify_views(&before, &after, u64::MAX, 100).is_ok());
        let previous_ids = BTreeSet::from([101]);
        let updated_ids = BTreeSet::from([100, 101]);
        before.occupied = &previous_ids;
        before.cached = &previous_ids;
        let old_track_art = BTreeMap::from([(u64::MAX, 101)]);
        let old_album_art = BTreeMap::from([((-31i64 as u64), 101)]);
        before.track_art = &old_track_art;
        before.album_art = &old_album_art;
        after.occupied = &updated_ids;
        after.cached = &updated_ids;
        let old_object = CachedArtwork {
            cache_id: 101,
            artwork_type: 6,
            variant: 0,
            relative_path: "aa/00000000000000000000000000000000000000".into(),
        };
        let old_objects = BTreeMap::from([((101, 0), old_object.clone())]);
        let mut updated_objects = fixture.cached_objects.clone();
        updated_objects.insert((101, 0), old_object);
        before.cached_objects = &old_objects;
        after.cached_objects = &updated_objects;
        // Shared suffixes are allowed: the distinct cache IDs may name the same JPEG.
        assert!(verify_views(&before, &after, u64::MAX, 100).is_ok());
        after.cached = &fixture.cached;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        after.cached = &updated_ids;
        after.occupied = &fixture.occupied;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
    }

    #[test]
    fn rejects_wrong_anchor_identity_target_and_existing_or_reserved_cache() {
        let fixture = Fixture::new();
        let (before, mut after) = fixture.views();
        for anchor in [13, 15] {
            after.anchor = anchor;
            assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        }
        after.anchor = 14;
        after.database ^= 1;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        after.database = before.database;
        after.container ^= 1;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        after.container = before.container;
        assert!(verify_views(&before, &after, 555, 100).is_err());
        assert!(verify_views(&before, &after, u64::MAX, 99).is_err());
        let mut before = before;
        before.occupied = &fixture.occupied;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        before.anchor = u32::MAX;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
    }

    #[test]
    fn rejects_audio_metadata_availability_playback_and_membership_changes() {
        let fixture = Fixture::new();
        for variant in 0..7 {
            let mut tracks = fixture.after.tracks().to_vec();
            match variant {
                0 => tracks[0].metadata.title.push('!'),
                1 => tracks[0].metadata.is_compilation = true,
                2 => tracks[0].metadata.content_rating = 1,
                3 => tracks[0].playback.skip_count_recent = 1,
                4 => tracks[0].afc_path = None,
                5 => tracks[0].availability.is_placeholder = true,
                _ => tracks.push(tracks[0].clone()),
            }
            let (before, mut after) = fixture.views();
            after.tracks = &tracks;
            assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        }
        let (before, mut after) = fixture.views();
        let empty_albums = BTreeMap::new();
        after.albums = &empty_albums;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
        let (before, mut after) = fixture.views();
        let empty_artists = BTreeMap::new();
        after.artists = &empty_artists;
        assert!(verify_views(&before, &after, u64::MAX, 100).is_err());
    }
}
