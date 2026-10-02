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
