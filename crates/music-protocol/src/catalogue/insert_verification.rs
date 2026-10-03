//! Independent catalogue checks after a single additive transfer.
//! Audio and artwork byte verification remain separate transport evidence.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::{
    catalogue::{
        presentation::{Presentation, presentation},
        sqlite::{Album, Catalog, ComposerReference, PlaybackStatistics, Track},
    },
    media::metadata::{Metadata, ScalarMetadata},
};

pub struct ExpectedInsert<'a> {
    pub track_id: u64,
    pub album_id: u64,
    pub artist_id: u64,
    pub path: &'a str,
    pub file_bytes: u64,
    pub metadata: ExpectedSource<'a>,
    /// Whether the insert sent the sort names and comment of
    /// [`presentation`] against the catalogue before it. When false, each
    /// sort name was sent as its name with no comment, and they are not
    /// checked.
    pub presented: bool,
}

/// What an inserted row must say: an ALAC source's own tags and PCM duration,
/// or the tags another codec's source projects with the display duration its
/// insert sent (an MP3's coded duration).
#[derive(Clone, Copy)]
pub enum ExpectedSource<'a> {
    Alac(&'a Metadata),
    Projected {
        tags: ScalarMetadata<'a>,
        total_time_ms: u64,
    },
}

impl<'a> From<&'a Metadata> for ExpectedSource<'a> {
    fn from(metadata: &'a Metadata) -> Self {
        Self::Alac(metadata)
    }
}

impl<'a> ExpectedSource<'a> {
    pub fn projected(&self) -> Result<ScalarMetadata<'a>> {
        match *self {
            Self::Alac(metadata) => metadata.projected(),
            Self::Projected { tags, .. } => Ok(tags),
        }
    }

    pub fn ensure_compilation_profile(&self) -> Result<()> {
        match self {
            Self::Alac(metadata) => metadata.ensure_compilation_profile(),
            Self::Projected { tags, .. } => {
                ensure!(
                    tags.compilation != Some(true)
                        || tags
                            .album_artist
                            .is_some_and(|value| !value.trim().is_empty()),
                    "compilation insertion requires one unambiguous nonempty explicit album artist"
                );
                Ok(())
            }
        }
    }

    pub fn total_time_ms(&self) -> Result<u64> {
        match self {
            Self::Alac(metadata) => Ok(metadata
                .timing
                .pcm_samples_per_channel()
                .checked_mul(1000)
                .context("duration overflow")?
                / u64::from(metadata.sample_rate)),
            Self::Projected { total_time_ms, .. } => Ok(*total_time_ms),
        }
    }
}

#[derive(Serialize)]
pub struct VerifiedInsert {
    pub schema: &'static str,
    pub track_id: String,
    pub media_anchor: u32,
    pub before_database_sha256: String,
    pub before_wal_sha256: Option<String>,
    pub after_database_sha256: String,
    pub after_wal_sha256: Option<String>,
    pub supported_metadata_matches: bool,
    pub prior_tracks_preserved: usize,
    pub prior_track_field_scope: &'static str,
    pub prior_recent_counter_resets: Vec<RecentCounterReset>,
    /// Prior tracks whose playback statistics the caller's listening rule
    /// accepted; see [`verify_insert_after_listening`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prior_tracks_listened_between_runs: Vec<String>,
    /// Prior rows and groups the phone renamed to the inserted track's
    /// spelling.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub case_renamed: Vec<CaseRename>,
    pub prior_artwork_registrations_preserved: bool,
    pub audio_and_artwork_bytes_verified_by_this_check: bool,
    pub normalized_fields: Vec<NormalizedField>,
    pub composer_sort_policy: &'static str,
    pub composer_sort_delta: Option<ComposerSortDelta>,
    pub unverified_fields: [&'static str; 2],
}

/// A name-matched group the phone renamed to the spelling an inserted track
/// declared; the two names differ only in ASCII case. `kind` is `track_artist`,
/// `track_album` or `track_album_artist` for a prior track's row, `album` or
/// `album_artist` for the group itself.
#[derive(Debug, Serialize)]
pub struct CaseRename {
    pub kind: &'static str,
    pub id: String,
    pub before: String,
    pub after: String,
}

/// The spellings the present targets declare for the name-matched groups.
#[derive(Default)]
struct Declared<'a> {
    artists: BTreeSet<&'a str>,
    albums: BTreeSet<&'a str>,
    album_artists: BTreeSet<&'a str>,
}

impl<'a> Declared<'a> {
    fn of<'b>(targets: impl IntoIterator<Item = &'b ExpectedInsert<'a>>) -> Result<Self>
    where
        'a: 'b,
    {
        let mut declared = Self::default();
        for target in targets {
            let s = target.metadata.projected()?;
            declared.artists.insert(s.artist);
            declared.albums.insert(s.album);
            declared
                .album_artists
                .insert(s.album_artist.unwrap_or(s.artist));
        }
        Ok(declared)
    }
}

/// The phone matches artist, album and composer names ignoring ASCII case.
fn same_name(a: &str, b: &str) -> bool {
    a == b || (a.is_ascii() && b.is_ascii() && a.eq_ignore_ascii_case(b))
}

/// A name as the phone matches it.
fn folded(name: &str) -> String {
    if name.is_ascii() {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    }
}

/// Whether the phone renamed a group from `old` to `new`, a spelling an
/// inserted track declared: a target naming an existing artist or album in
/// another ASCII case gives the group its spelling.
fn case_renamed(old: &str, new: &str, declared: &BTreeSet<&str>) -> bool {
    old != new
        && old.is_ascii()
        && new.is_ascii()
        && old.eq_ignore_ascii_case(new)
        && declared.contains(new)
}

/// Every permitted positive-to-zero recent-counter change for a prior track.
/// Lifetime counts and all other typed raw statistics remain exact.
#[derive(Debug, Serialize)]
pub struct RecentCounterReset {
    pub track_id: String,
    pub play_count_recent_before: u32,
    pub play_count_recent_after: u32,
    pub skip_count_recent_before: u32,
    pub skip_count_recent_after: u32,
}

#[derive(Serialize)]
pub struct NormalizedField {
    pub field: &'static str,
    pub requested: String,
    pub stored: String,
    pub existing_lookup_id: i64,
    pub prior_reference_count: usize,
}
#[derive(Serialize)]
pub struct ComposerSortDelta {
    pub lookup_id: i64,
    pub stored_name: String,
    pub before_sort: String,
    pub after_sort: String,
    pub prior_reference_count: usize,
    pub after_reference_count: usize,
    pub order_changes: Vec<ComposerOrderChange>,
}
#[derive(Serialize)]
pub struct ComposerOrderChange {
    pub track_id: String,
    pub before: i64,
    pub after: i64,
}

pub fn verify_insert(
    before: &Catalog,
    after: &Catalog,
    expected: &ExpectedInsert<'_>,
) -> Result<VerifiedInsert> {
    verify_insert_with_policy(before, after, expected, false, None)
}

/// For a later run's capture, after the phone may have been used. `listening`
/// decides whether a prior track's playback statistics changed only by plays,
/// skips and recent-counter resets; every other Track field stays exact. The
/// typed raw statistics of such tracks (dates included) are not compared here,
/// so the caller MUST have proved them with a full SQL comparison.
pub fn verify_insert_after_listening(
    before: &Catalog,
    after: &Catalog,
    expected: &ExpectedInsert<'_>,
    listening: &dyn Fn(&PlaybackStatistics, &PlaybackStatistics) -> Result<()>,
) -> Result<VerifiedInsert> {
    verify_insert_with_policy(before, after, expected, false, Some(listening))
}

/// For an insert serialized with no existing composer, whose composer became
/// its own sort label. This accepts the observed shared composer-sort
/// transition and emits every prior order delta. A caller MUST additionally
/// pin an independent full-SQL audit; this receipt does not claim those known
/// changed values were preserved and cannot authorize replay.
pub fn verify_insert_legacy_composer_sort(
    before: &Catalog,
    after: &Catalog,
    expected: &ExpectedInsert<'_>,
) -> Result<VerifiedInsert> {
    verify_insert_with_policy(before, after, expected, true, None)
}

fn verify_insert_with_policy(
    before: &Catalog,
    after: &Catalog,
    expected: &ExpectedInsert<'_>,
    legacy_composer_sort: bool,
    listening: Option<&dyn Fn(&PlaybackStatistics, &PlaybackStatistics) -> Result<()>>,
) -> Result<VerifiedInsert> {
    ensure!(
        before.database_pid() == after.database_pid()
            && before.primary_container_pid() == after.primary_container_pid(),
        "catalogue identity changed across insertion"
    );
    ensure!(
        after.persisted_media_anchor()
            == before
                .persisted_media_anchor()
                .checked_add(1)
                .context("media anchor exhausted")?,
        "post-transfer media anchor differs from the single prepared operation"
    );
    ensure!(
        !before.represented_sync_ids().contains(&expected.track_id),
        "expected inserted track already existed"
    );
    ensure!(
        after.tracks().len() == before.tracks().len() + 1,
        "post-transfer track count differs from one insertion"
    );
    let after_tracks: BTreeMap<_, _> = after.tracks().iter().map(|t| (t.sync_id, t)).collect();
    ensure!(
        after_tracks.len() == after.tracks().len(),
        "post-transfer track IDs alias"
    );
    let declared = Declared::of([expected])?;
    let (prior_recent_counter_resets, prior_tracks_listened_between_runs, mut case_renamed) =
        if legacy_composer_sort {
            preserve_tracks(before.tracks(), &after_tracks)?;
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            preserve_insert_tracks(
                before,
                after,
                &after_tracks,
                &BTreeSet::from([expected.track_id]),
                listening,
                &declared,
            )?
        };
    let inserted = after_tracks
        .get(&expected.track_id)
        .context("inserted identity absent from fresh catalogue")?;
    let normalized_fields = check_inserted(inserted, expected, before)?;
    let targets = std::slice::from_ref(expected);
    let presented = presentations(before, targets)?;
    let composer_sort_delta = verify_composers(
        before,
        after,
        expected.track_id,
        expected.metadata.projected()?.composer,
        presented[0].map(|p| p.sort_composer),
        legacy_composer_sort,
    )?;
    verify_presentation(
        before,
        after,
        targets,
        &presented,
        &BTreeSet::from([expected.track_id]),
    )?;
    let old_albums: BTreeSet<_> = before.albums().values().map(|a| a.sync_id).collect();
    let old_artists: BTreeSet<_> = before.artists().values().map(|a| a.sync_id).collect();
    let expected_albums = old_albums
        .union(&BTreeSet::from([expected.album_id]))
        .copied()
        .collect::<BTreeSet<_>>();
    let expected_artists = old_artists
        .union(&BTreeSet::from([expected.artist_id]))
        .copied()
        .collect::<BTreeSet<_>>();
    ensure!(
        after
            .albums()
            .values()
            .map(|a| a.sync_id)
            .collect::<BTreeSet<_>>()
            == expected_albums,
        "post-transfer album identities differ from the planned grouping"
    );
    ensure!(
        after
            .artists()
            .values()
            .map(|a| a.sync_id)
            .collect::<BTreeSet<_>>()
            == expected_artists,
        "post-transfer artist identities differ from the planned grouping"
    );
    for (id, old) in before.albums() {
        let new = after.albums().get(id).context("prior album disappeared")?;
        ensure!(
            old.sync_id == new.sync_id
                && group_name(
                    &mut case_renamed,
                    "album",
                    old.sync_id,
                    &old.name,
                    &new.name,
                    &declared.albums
                )
                && old.artist_internal_id == new.artist_internal_id
                && old.artist_sync_id == new.artist_sync_id
                && old.all_compilations == new.all_compilations
                && compilation_members_kept(old, new)
                && new.live_track_count
                    == old.live_track_count + usize::from(old.sync_id == expected.album_id),
            "prior album changed outside the expected track membership increment"
        );
    }
    for (id, old) in before.artists() {
        let new = after
            .artists()
            .get(id)
            .context("prior artist disappeared")?;
        ensure!(
            old.sync_id == new.sync_id
                && group_name(
                    &mut case_renamed,
                    "album_artist",
                    old.sync_id,
                    &old.name,
                    &new.name,
                    &declared.album_artists
                )
                && new.live_track_count
                    == old.live_track_count + usize::from(old.sync_id == expected.artist_id),
            "prior artist changed outside the expected track membership increment"
        );
    }
    for (id, cache) in before.artwork().track_cache_ids() {
        ensure!(
            after.artwork().track_cache_ids().get(id) == Some(cache),
            "prior track artwork registration changed"
        );
    }
    for (id, cache) in before.artwork().album_cache_ids() {
        ensure!(
            after.artwork().album_cache_ids().get(id) == Some(cache),
            "prior album artwork registration changed"
        );
    }
    ensure!(
        before
            .artwork()
            .occupied_cache_ids()
            .is_subset(after.artwork().occupied_cache_ids()),
        "prior artwork namespace lost an ID"
    );
    Ok(VerifiedInsert {
        schema: "music-protocol.insert-catalog-verification",
        track_id: format!("{:016x}", expected.track_id),
        media_anchor: after.persisted_media_anchor(),
        before_database_sha256: hex(&before.source_hashes().database),
        before_wal_sha256: before.source_hashes().wal.map(|v| hex(&v)),
        after_database_sha256: hex(&after.source_hashes().database),
        after_wal_sha256: after.source_hashes().wal.map(|v| hex(&v)),
        supported_metadata_matches: true,
        prior_tracks_preserved: before.tracks().len(),
        prior_track_field_scope: if legacy_composer_sort {
            "Track DTO fields are exact; composer lookup/sort/order preservation or explicit legacy deltas are reported separately"
        } else if listening.is_some() {
            "Prior Track DTO fields are exact except reported recent-counter resets and the playback statistics of reported listened tracks, which the caller's listening rule accepted; listened tracks' other item_stats columns are exact and their play and skip dates move forward only with new plays and skips, all others are exact; composer lookup/sort/order preservation is reported separately"
        } else {
            "Prior Track DTO fields are exact except explicitly reported positive-to-zero recent play/skip counter resets; all other typed raw item_stats columns are exact; composer lookup/sort/order preservation is reported separately"
        },
        prior_recent_counter_resets,
        prior_tracks_listened_between_runs,
        case_renamed,
        prior_artwork_registrations_preserved: true,
        audio_and_artwork_bytes_verified_by_this_check: false,
        normalized_fields,
        composer_sort_policy: if legacy_composer_sort {
            "legacy_source_sort"
        } else {
            "preserve"
        },
        composer_sort_delta,
        unverified_fields: [
            "Other unprojected SQL columns and lookup grouping keys require independent full-row audit",
            "Permanent audio/artwork bytes and protocol terminal are separate evidence",
        ],
    })
}

fn preserve_insert_tracks(
    before: &Catalog,
    after: &Catalog,
    after_tracks: &BTreeMap<u64, &Track>,
    inserted: &BTreeSet<u64>,
    listening: Option<&dyn Fn(&PlaybackStatistics, &PlaybackStatistics) -> Result<()>>,
    declared: &Declared<'_>,
) -> Result<(Vec<RecentCounterReset>, Vec<String>, Vec<CaseRename>)> {
    let prior_ids = before
        .tracks()
        .iter()
        .map(|t| t.sync_id)
        .collect::<BTreeSet<_>>();
    let before_stats = before.stats_except_recent_play_and_skip();
    let after_stats = after.stats_except_recent_play_and_skip();
    ensure!(
        before_stats.keys().copied().collect::<BTreeSet<_>>() == prior_ids,
        "prior typed statistics do not cover exactly the prior track identities"
    );
    let expected_ids = prior_ids
        .iter()
        .chain(inserted)
        .copied()
        .collect::<BTreeSet<_>>();
    ensure!(
        after_stats.keys().copied().collect::<BTreeSet<_>>() == expected_ids,
        "post-insert typed statistics identities differ from prior tracks plus the inserted targets"
    );
    let mut resets = Vec::new();
    let mut listened = Vec::new();
    let mut renamed = Vec::new();
    for old in before.tracks() {
        let mut new = after_tracks
            .get(&old.sync_id)
            .copied()
            .with_context(|| format!("prior track {:016x} disappeared", old.sync_id))?
            .clone();
        // A group the phone renamed shows its new spelling in every member's
        // row; the rest of the row stays exact.
        for (kind, before_name, after_name, names) in [
            (
                "track_artist",
                &old.metadata.artist,
                &mut new.metadata.artist,
                &declared.artists,
            ),
            (
                "track_album",
                &old.metadata.album,
                &mut new.metadata.album,
                &declared.albums,
            ),
            (
                "track_album_artist",
                &old.metadata.album_artist,
                &mut new.metadata.album_artist,
                &declared.album_artists,
            ),
        ] {
            if case_renamed(before_name, after_name, names) {
                renamed.push(CaseRename {
                    kind,
                    id: format!("{:016x}", old.sync_id),
                    before: before_name.clone(),
                    after: std::mem::replace(after_name, before_name.clone()),
                });
            }
        }
        let recent_matches =
            |previous: u32, current: u32| previous == current || (previous > 0 && current == 0);
        // The session's own positive-to-zero recent-counter reset stays a
        // reset, not listening.
        let reset_only = {
            let mut kept = new.playback.clone();
            kept.play_count_recent = old.playback.play_count_recent;
            kept.skip_count_recent = old.playback.skip_count_recent;
            kept == old.playback
                && recent_matches(
                    old.playback.play_count_recent,
                    new.playback.play_count_recent,
                )
                && recent_matches(
                    old.playback.skip_count_recent,
                    new.playback.skip_count_recent,
                )
        };
        if let Some(rule) = listening.filter(|_| old.playback != new.playback && !reset_only) {
            rule(&old.playback, &new.playback)
                .with_context(|| format!("prior track {:016x} playback", old.sync_id))?;
            let mut comparable = new.clone();
            comparable.playback = old.playback.clone();
            ensure!(
                comparable == *old,
                "prior track {:016x} changed outside listening",
                old.sync_id
            );
            ensure!(
                before.stats_except_playback().get(&old.sync_id).is_some()
                    && before.stats_except_playback().get(&old.sync_id)
                        == after.stats_except_playback().get(&old.sync_id),
                "prior track {:016x} statistics changed outside playback",
                old.sync_id
            );
            let dates = |catalog: &Catalog| {
                catalog
                    .playback_dates()
                    .get(&old.sync_id)
                    .copied()
                    .unwrap_or_default()
            };
            let (was, now) = (dates(before), dates(after));
            for (field, prior_count, count, prior, later) in [
                (
                    "date_played",
                    old.playback.play_count_user,
                    new.playback.play_count_user,
                    was.played,
                    now.played,
                ),
                (
                    "date_skipped",
                    old.playback.skip_count_user,
                    new.playback.skip_count_user,
                    was.skipped,
                    now.skipped,
                ),
            ] {
                ensure!(
                    if count > prior_count {
                        later > prior
                    } else {
                        prior.to_bits() == later.to_bits()
                    },
                    "{field} of prior track {:016x} is not explained by its plays and skips",
                    old.sync_id
                );
            }
            listened.push(format!("{:016x}", old.sync_id));
            continue;
        }
        ensure!(
            recent_matches(
                old.playback.play_count_recent,
                new.playback.play_count_recent
            ) && recent_matches(
                old.playback.skip_count_recent,
                new.playback.skip_count_recent
            ),
            "prior track {:016x} recent counters changed outside positive-to-zero reset",
            old.sync_id
        );
        let mut comparable = new.clone();
        comparable.playback.play_count_recent = old.playback.play_count_recent;
        comparable.playback.skip_count_recent = old.playback.skip_count_recent;
        ensure!(
            comparable == *old,
            "prior track {:016x} changed outside recent-counter reset",
            old.sync_id
        );
        ensure!(
            before_stats.get(&old.sync_id) == after_stats.get(&old.sync_id),
            "prior track {:016x} typed statistics changed outside recent-counter reset",
            old.sync_id
        );
        if old.playback.play_count_recent != new.playback.play_count_recent
            || old.playback.skip_count_recent != new.playback.skip_count_recent
        {
            resets.push(RecentCounterReset {
                track_id: format!("{:016x}", old.sync_id),
                play_count_recent_before: old.playback.play_count_recent,
                play_count_recent_after: new.playback.play_count_recent,
                skip_count_recent_before: old.playback.skip_count_recent,
                skip_count_recent_after: new.playback.skip_count_recent,
            });
        }
    }
    Ok((resets, listened, renamed))
}

/// Whether a prior group kept its name or the phone renamed it to a declared
/// spelling (`case_renamed`), which is recorded.
fn group_name(
    renamed: &mut Vec<CaseRename>,
    kind: &'static str,
    id: u64,
    before: &str,
    after: &str,
    declared: &BTreeSet<&str>,
) -> bool {
    if before == after {
        return true;
    }
    let allowed = case_renamed(before, after, declared);
    if allowed {
        renamed.push(CaseRename {
            kind,
            id: format!("{id:016x}"),
            before: before.into(),
            after: after.into(),
        });
    }
    allowed
}

/// A live album keeps whether it has a compilation member. An emptied album
/// whose `all_compilations` is set may be refilled with compilation tracks, as
/// a new compilation album would be; the summary itself must not change.
fn compilation_members_kept(old: &Album, new: &Album) -> bool {
    old.has_compilation_tracks == new.has_compilation_tracks
        || old.live_track_count == 0 && old.all_compilations
}

fn preserve_tracks(before: &[Track], after: &BTreeMap<u64, &Track>) -> Result<()> {
    for old in before {
        ensure!(
            after.get(&old.sync_id).copied() == Some(old),
            "prior track {:016x} disappeared or its inspected fields changed",
            old.sync_id
        );
    }
    Ok(())
}
fn check_inserted(
    track: &Track,
    expected: &ExpectedInsert<'_>,
    before: &Catalog,
) -> Result<Vec<NormalizedField>> {
    let s = expected.metadata.projected()?;
    expected.metadata.ensure_compilation_profile()?;
    let m = &track.metadata;
    ensure!(
        track.sync_id == expected.track_id
            && track.album_sync_id == expected.album_id
            && track.artist_sync_id == expected.artist_id,
        "inserted track/group identities differ from the plan"
    );
    ensure!(
        track.afc_path.as_deref() == Some(expected.path) && m.file_size == expected.file_bytes,
        "inserted file path or size differs from verified audio"
    );
    ensure!(
        !track.availability.is_placeholder
            && track.availability.is_library_added
            && track.availability.has_non_purgeable_asset
            && track.availability.is_supported_type,
        "inserted track is not a supported available local asset"
    );
    let optional = |value: &Option<String>, text: Option<&str>| {
        value.as_deref().filter(|v| !v.is_empty()) == text.filter(|v| !v.is_empty())
    };
    // The phone files a track under an existing artist, album or composer
    // whose name differs only in ASCII case; an artist or album takes the
    // spelling of the last track filed under it (`case_renamed`), a composer
    // keeps its first spelling.
    let same_group = same_name;
    let same_optional = |value: &Option<String>, text: Option<&str>| match (
        value.as_deref().filter(|v| !v.is_empty()),
        text.filter(|v| !v.is_empty()),
    ) {
        (Some(stored), Some(declared)) => same_name(stored, declared),
        (stored, declared) => stored == declared,
    };
    let existing_composer = s
        .composer
        .map(|name| before.composer_for_label(name))
        .transpose()?
        .flatten();
    let expected_composer = existing_composer
        .map(|row| row.name.as_str())
        .or(s.composer);
    ensure!(
        m.title == s.title
            && same_group(&m.artist, s.artist)
            && same_group(&m.album, s.album)
            && same_group(&m.album_artist, s.effective_artist())
            && optional(&m.genre, s.genre)
            && same_optional(&m.composer, expected_composer)
            && optional(&m.copyright, s.copyright),
        "inserted text metadata differs from the supported source projection"
    );
    let duration = expected.metadata.total_time_ms()?;
    ensure!(
        m.total_time_ms as u64 == duration
            && m.year == s.year.unwrap_or(0)
            && m.track_number == s.track.unwrap_or(0)
            && m.track_count == s.track_total.unwrap_or(0)
            // The phone stores disc 1 when an insert names none.
            && m.disc_number == s.disc.unwrap_or(1)
            && m.disc_count == s.disc_total.unwrap_or(0)
            && m.content_rating == u32::from(s.content_rating.unwrap_or(0))
            && m.content_rating_level == 0
            && m.is_compilation == s.compilation.unwrap_or(false),
        "inserted numeric metadata differs from the supported source projection"
    );
    let mut normalized = Vec::new();
    if let (Some(requested), Some(row)) = (s.composer, existing_composer)
        && requested != row.name
    {
        normalized.push(NormalizedField {
            field: "composer",
            requested: requested.to_owned(),
            stored: row.name.clone(),
            existing_lookup_id: row.internal_id,
            prior_reference_count: before
                .track_composers()
                .values()
                .filter(|r| r.lookup_id == Some(row.internal_id))
                .count(),
        });
    }
    Ok(normalized)
}

/// The presentation each target was sent with, if it was presented.
fn presentations<'c>(
    before: &'c Catalog,
    expected: &[ExpectedInsert<'c>],
) -> Result<Vec<Option<Presentation<'c>>>> {
    expected
        .iter()
        .map(|target| {
            target
                .presented
                .then(|| {
                    presentation(
                        &target.metadata.projected()?,
                        before,
                        target.album_id,
                        target.artist_id,
                    )
                })
                .transpose()
        })
        .collect()
}

/// The sort names a group was sent, keyed as the phone files it; `None` once
/// a target that was not presented joined it, whose sort is not checked.
type Sent<'s, K> = BTreeMap<K, Option<BTreeSet<&'s str>>>;

fn note<'s, K: Ord>(sent: &mut Sent<'s, K>, key: K, sort: Option<&'s str>) {
    let entry = sent.entry(key).or_insert_with(|| Some(BTreeSet::new()));
    match (entry.as_mut(), sort) {
        (Some(sorts), Some(sort)) => {
            sorts.insert(sort);
        }
        _ => *entry = None,
    }
}

/// A group's sort name after the insert: a prior group keeps it (a target
/// joining it sent it back), a new one holds one its members sent.
fn sort_kept_or_sent(
    what: &str,
    old: Option<&Option<String>>,
    new: &Option<String>,
    sent: Option<&Option<BTreeSet<&str>>>,
) -> Result<()> {
    match (old, sent) {
        (_, Some(None)) => {}
        (Some(old), _) => ensure!(new == old, "prior {what} changed its sort name"),
        (None, Some(Some(sorts))) => ensure!(
            new.as_deref().is_some_and(|sort| sorts.contains(sort)),
            "new {what} sort name differs from what was sent"
        ),
        (None, None) => {}
    }
    Ok(())
}

/// The comments and sort names an insert left. Each presented target in
/// `located` stores its comment (NULL for none) and title sort. Albums, album
/// artists and track artists keep their sort names unless a target that was not
/// presented joined them; a new one holds a sort name one of its members sent
/// (the album artist's, or the track artist's when the insert named no album
/// artist). Composer sorts are checked with their lookups. Prior tracks keep
/// their comments and title sorts.
fn verify_presentation(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    presented: &[Option<Presentation<'_>>],
    located: &BTreeSet<u64>,
) -> Result<()> {
    for old in before.tracks() {
        ensure!(
            after.comment(old.sync_id) == before.comment(old.sync_id)
                && after.title_sort(old.sync_id) == before.title_sort(old.sync_id),
            "prior track {:016x} changed its comment or title sort",
            old.sync_id
        );
    }
    let mut albums: Sent<'_, u64> = BTreeMap::new();
    let mut album_artists: Sent<'_, u64> = BTreeMap::new();
    let mut artists: Sent<'_, i64> = BTreeMap::new();
    for (target, presentation) in expected.iter().zip(presented.iter().copied()) {
        note(
            &mut albums,
            target.album_id,
            presentation.map(|p| p.sort_album),
        );
        note(
            &mut album_artists,
            target.artist_id,
            presentation.map(|p| p.sort_album_artist.unwrap_or(p.sort_artist)),
        );
        if let Some(row) = after.track_item_artist(target.track_id) {
            note(
                &mut artists,
                row.internal_id,
                presentation.map(|p| p.sort_artist),
            );
        }
        if let Some(p) = presentation
            && located.contains(&target.track_id)
        {
            ensure!(
                after.comment(target.track_id) == Some(&p.comment.map(str::to_owned))
                    && after.title_sort(target.track_id) == Some(&Some(p.sort_title.to_owned())),
                "inserted track {:016x} comment or title sort differs from what was sent",
                target.track_id
            );
        }
    }
    let prior_albums: BTreeMap<_, _> = before.albums().values().map(|a| (a.sync_id, a)).collect();
    for new in after.albums().values() {
        sort_kept_or_sent(
            "album",
            prior_albums.get(&new.sync_id).map(|old| &old.sort_name),
            &new.sort_name,
            albums.get(&new.sync_id),
        )?;
    }
    let prior_artists: BTreeMap<_, _> = before.artists().values().map(|a| (a.sync_id, a)).collect();
    for new in after.artists().values() {
        sort_kept_or_sent(
            "album artist",
            prior_artists.get(&new.sync_id).map(|old| &old.sort_name),
            &new.sort_name,
            album_artists.get(&new.sync_id),
        )?;
    }
    ensure!(
        before
            .item_artists()
            .keys()
            .all(|id| after.item_artists().contains_key(id)),
        "a prior track artist disappeared"
    );
    for (id, new) in after.item_artists() {
        sort_kept_or_sent(
            "track artist",
            before.item_artists().get(id).map(|old| &old.sort_name),
            &new.sort_name,
            artists.get(id),
        )?;
    }
    Ok(())
}

/// `sent_sort` is the sort name a presented insert sent with the composer;
/// `None` when the insert was not presented, which sent the name as its sort.
fn verify_composers(
    before: &Catalog,
    after: &Catalog,
    target: u64,
    desired: Option<&str>,
    sent_sort: Option<Option<&str>>,
    legacy: bool,
) -> Result<Option<ComposerSortDelta>> {
    let target_ref = after
        .track_composers()
        .get(&target)
        .context("inserted composer reference absent")?;
    ensure!(
        after.track_composers().len() == before.track_composers().len() + 1,
        "composer reference count differs"
    );
    let prior = desired
        .map(|name| before.composer_for_label(name))
        .transpose()?
        .flatten();
    let current = target_ref
        .lookup_id
        .map(|id| {
            after
                .composers()
                .get(&id)
                .context("inserted composer lookup absent")
        })
        .transpose()?;
    match (desired, prior, current) {
        (None, None, None) => {}
        (Some(name), None, Some(row)) => ensure!(
            row.name == name
                && row.sort_name.as_deref() == sent_sort.unwrap_or(Some(name))
                && !before.composers().contains_key(&row.internal_id),
            "new composer lookup differs from source"
        ),
        (Some(_), Some(old), Some(new)) => ensure!(
            old.internal_id == new.internal_id
                && old.sync_id == new.sync_id
                && old.name == new.name,
            "reused composer identity or label changed"
        ),
        _ => anyhow::bail!("inserted composer join differs from source and prior lookup"),
    }
    let expected_ids: BTreeSet<_> = before
        .composers()
        .keys()
        .copied()
        .chain(target_ref.lookup_id)
        .collect();
    ensure!(
        after.composers().keys().copied().collect::<BTreeSet<_>>() == expected_ids,
        "unexpected composer lookup creation or removal"
    );
    let mut delta = None;
    for (id, old) in before.composers() {
        let new = &after.composers()[id];
        if old == new {
            continue;
        }
        ensure!(
            legacy && prior.is_some_and(|row| row.internal_id == *id),
            "prior composer lookup changed"
        );
        let requested = desired.context("legacy composer request absent")?;
        ensure!(
            old.name.is_ascii()
                && requested.is_ascii()
                && old.name != requested
                && old.name.eq_ignore_ascii_case(requested)
                && old.internal_id == new.internal_id
                && old.sync_id == new.sync_id
                && old.name == new.name
                && old.sort_name.as_deref() == Some(old.name.as_str())
                && new.sort_name.as_deref() == Some(requested),
            "legacy composer sort delta is not the qualified ASCII label transition"
        );
        let count = before
            .track_composers()
            .values()
            .filter(|r| r.lookup_id == Some(*id))
            .count();
        ensure!(count > 0, "legacy composer has no prior live references");
        delta = Some(ComposerSortDelta {
            lookup_id: *id,
            stored_name: old.name.clone(),
            before_sort: old.sort_name.clone().unwrap(),
            after_sort: requested.to_owned(),
            prior_reference_count: count,
            after_reference_count: count + 1,
            order_changes: Vec::new(),
        });
    }
    for (id, old) in before.track_composers() {
        let new = after
            .track_composers()
            .get(id)
            .context("prior composer reference disappeared")?;
        ensure!(
            old.lookup_id == new.lookup_id,
            "prior composer reference changed"
        );
        // Outside a legacy transition, orders may only be spaced out (below).
        if old.order != new.order && legacy {
            let change = delta
                .as_mut()
                .context("prior composer order changed outside explicit legacy qualification")?;
            ensure!(
                old.lookup_id == Some(change.lookup_id)
                    && old.order > 0
                    && new.order > 0
                    && new.order == target_ref.order,
                "legacy composer order changed outside qualified shared group"
            );
            change.order_changes.push(ComposerOrderChange {
                track_id: format!("{id:016x}"),
                before: old.order,
                after: new.order,
            });
        }
    }
    if legacy {
        let change = delta
            .as_ref()
            .context("explicit legacy composer transition absent")?;
        ensure!(
            change.order_changes.len() == change.prior_reference_count
                && change
                    .order_changes
                    .iter()
                    .map(|r| r.before)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == 1,
            "legacy composer order changes do not cover exactly the prior shared group"
        );
    } else {
        // The phone may space every sort key out again, keeping their order,
        // as after a large batch (seen after a long run of single inserts).
        ensure!(
            keeps_order(before.track_composers(), after.track_composers()),
            "prior composer order changed"
        );
    }
    Ok(delta)
}
/// Catalogue checks after one session inserted several ALAC tracks.
#[derive(Serialize)]
pub struct VerifiedInsertBatch {
    pub schema: &'static str,
    pub track_ids: Vec<String>,
    pub media_anchor: u32,
    pub before_database_sha256: String,
    pub before_wal_sha256: Option<String>,
    pub after_database_sha256: String,
    pub after_wal_sha256: Option<String>,
    pub supported_metadata_matches: bool,
    pub prior_tracks_preserved: usize,
    pub prior_recent_counter_resets: Vec<RecentCounterReset>,
    /// Prior tracks played or skipped between the session and a later run's
    /// check; see [`verify_insert_batch_after_listening`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prior_tracks_listened_between_runs: Vec<String>,
    /// Prior rows and groups the phone renamed to a target's spelling.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub case_renamed: Vec<CaseRename>,
    pub prior_artwork_registrations_preserved: bool,
    pub new_albums: Vec<String>,
    pub new_artists: Vec<String>,
    pub new_composers: Vec<String>,
    pub normalized_fields: Vec<InsertedFields>,
    /// Targets the phone created without their audio (no file location).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locationless_track_ids: Vec<String>,
    /// Targets the phone created and deleted again in the same session.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_track_ids: Vec<String>,
    /// Album registrations whose album was already gone and that the session
    /// deleted.
    #[serde(skip_serializing_if = "is_zero")]
    pub dropped_orphan_album_registrations: usize,
    /// Cache IDs that left the namespace with the album registration they had
    /// no cached object for, when the album took a new member's cover.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub released_album_cache_ids: Vec<u32>,
    /// Targets inserted with their cover, and their albums' registrations.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub covers: Vec<InsertedCover>,
    pub audio_and_artwork_bytes_verified_by_this_check: bool,
}

#[derive(Serialize)]
pub struct InsertedCover {
    pub track_id: String,
    pub cache_id: u32,
    pub album_id: String,
    pub album_cache_id: Option<u32>,
}

#[derive(Serialize)]
pub struct InsertedFields {
    pub track_id: String,
    pub normalized_fields: Vec<NormalizedField>,
}

/// The single-insert checks for every target of one session: the anchor moves
/// once, prior tracks, groups and artwork registrations are exact apart from
/// the added memberships, and each new track matches its source. Tracks of the
/// same new album, artist or composer share one new row.
pub fn verify_insert_batch(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
) -> Result<VerifiedInsertBatch> {
    verify_insert_batch_with(before, after, expected, &BTreeSet::new())
}

/// The same checks for a session that ended before some of its audio arrived:
/// each `locationless` target has its planned row and groups but no file
/// location, and its other fields are not checked (it awaits removal).
pub fn verify_insert_batch_with(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    locationless: &BTreeSet<u64>,
) -> Result<VerifiedInsertBatch> {
    verify_insert_batch_covered(
        before,
        after,
        expected,
        locationless,
        &BTreeSet::new(),
        &BTreeMap::new(),
    )
}

/// The same checks for a session whose inserts may name covers: `covers` maps
/// a target track to its new cache ID. A covered target registers exactly that
/// ID (a locationless one may lack it), an uncovered one none. An album keeps
/// its registration or takes the cover of one of its new members, as the phone
/// does for a single cover (independent representative); a new album takes
/// one of its members' covers or none. An album without a covered member, and
/// every other registration, stays exact; only planned cache IDs appear. The
/// caller sends covers only for albums whose own image may change.
///
/// Each `removed` target is one the phone created and deleted again within the
/// session (observed when the host abandoned a session mid-transfer: the
/// in-flight rows were deleted, the others kept without a location). Its row,
/// references and track registration are gone; an album, artist or composer
/// planned for it may stay without members, and its album may hold its cover.
/// The session may also delete album registrations whose album was already
/// gone, never create one.
pub fn verify_insert_batch_covered(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    locationless: &BTreeSet<u64>,
    removed: &BTreeSet<u64>,
    covers: &BTreeMap<u64, u32>,
) -> Result<VerifiedInsertBatch> {
    verify_batch(before, after, expected, locationless, removed, covers, None)
}

/// The same checks for a later run's capture, after the phone may have been
/// played: a prior track's playback may differ as `listening` accepts, its
/// play or skip date only forwards and only with new plays or skips, and
/// every other statistic and field stays exact. The session's own
/// recent-counter resets are accepted as before.
pub fn verify_insert_batch_after_listening(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    locationless: &BTreeSet<u64>,
    removed: &BTreeSet<u64>,
    covers: &BTreeMap<u64, u32>,
    listening: &dyn Fn(&PlaybackStatistics, &PlaybackStatistics) -> Result<()>,
) -> Result<VerifiedInsertBatch> {
    verify_batch(
        before,
        after,
        expected,
        locationless,
        removed,
        covers,
        Some(listening),
    )
}

fn verify_batch(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    locationless: &BTreeSet<u64>,
    removed: &BTreeSet<u64>,
    covers: &BTreeMap<u64, u32>,
    listening: Option<&dyn Fn(&PlaybackStatistics, &PlaybackStatistics) -> Result<()>>,
) -> Result<VerifiedInsertBatch> {
    ensure!(!expected.is_empty(), "insert batch has no targets");
    ensure!(
        before.database_pid() == after.database_pid()
            && before.primary_container_pid() == after.primary_container_pid(),
        "catalogue identity changed across insertion"
    );
    ensure!(
        after.persisted_media_anchor()
            == before
                .persisted_media_anchor()
                .checked_add(1)
                .context("media anchor exhausted")?,
        "post-transfer media anchor differs from the one prepared operation"
    );
    let inserted: BTreeSet<u64> = expected.iter().map(|e| e.track_id).collect();
    ensure!(
        inserted.len() == expected.len(),
        "insert batch names a track twice"
    );
    ensure!(
        locationless.is_subset(&inserted),
        "a locationless track is not a batch target"
    );
    ensure!(
        removed.is_subset(&inserted) && removed.is_disjoint(locationless),
        "a removed track is not a present batch target"
    );
    let present: BTreeSet<u64> = inserted.difference(removed).copied().collect();
    ensure!(
        inserted
            .iter()
            .all(|id| !before.represented_sync_ids().contains(id)),
        "expected inserted track already existed"
    );
    ensure!(
        after.tracks().len() == before.tracks().len() + present.len(),
        "post-transfer track count differs from the batch"
    );
    let after_tracks: BTreeMap<_, _> = after.tracks().iter().map(|t| (t.sync_id, t)).collect();
    ensure!(
        after_tracks.len() == after.tracks().len(),
        "post-transfer track IDs alias"
    );
    let declared = Declared::of(expected.iter().filter(|e| present.contains(&e.track_id)))?;
    let (prior_recent_counter_resets, prior_tracks_listened_between_runs, mut case_renamed) =
        preserve_insert_tracks(before, after, &after_tracks, &present, listening, &declared)?;
    let mut normalized_fields = Vec::new();
    for target in expected {
        if removed.contains(&target.track_id) {
            ensure!(
                !after_tracks.contains_key(&target.track_id),
                "removed track {:016x} still exists",
                target.track_id
            );
            continue;
        }
        let track = after_tracks
            .get(&target.track_id)
            .context("inserted identity absent from fresh catalogue")?;
        if locationless.contains(&target.track_id) {
            ensure!(
                track.album_sync_id == target.album_id
                    && track.artist_sync_id == target.artist_id
                    && track.afc_path.is_none(),
                "locationless track {:016x} differs from its planned row",
                target.track_id
            );
            continue;
        }
        let normalized = check_inserted(track, target, before)?;
        if !normalized.is_empty() {
            normalized_fields.push(InsertedFields {
                track_id: format!("{:016x}", target.track_id),
                normalized_fields: normalized,
            });
        }
    }
    let presented = presentations(before, expected)?;
    let (new_composers, composer_renames) =
        verify_batch_composers(before, after, expected, &presented, removed)?;
    case_renamed.extend(composer_renames);
    let located: BTreeSet<u64> = present.difference(locationless).copied().collect();
    verify_presentation(before, after, expected, &presented, &located)?;
    let count = |pick: fn(&ExpectedInsert<'_>) -> u64, id: u64| {
        expected
            .iter()
            .filter(|e| !removed.contains(&e.track_id) && pick(e) == id)
            .count()
    };
    let old_albums: BTreeSet<_> = before.albums().values().map(|a| a.sync_id).collect();
    let old_artists: BTreeSet<_> = before.artists().values().map(|a| a.sync_id).collect();
    let after_albums: BTreeSet<_> = after.albums().values().map(|a| a.sync_id).collect();
    let after_artists: BTreeSet<_> = after.artists().values().map(|a| a.sync_id).collect();
    // Groups of present targets must exist; those planned only for removed
    // targets may.
    let grouped = |pick: fn(&ExpectedInsert<'_>) -> u64, kept: bool| -> BTreeSet<u64> {
        expected
            .iter()
            .filter(|e| !kept || !removed.contains(&e.track_id))
            .map(pick)
            .collect()
    };
    let within =
        |after: &BTreeSet<u64>, old: &BTreeSet<u64>, pick: fn(&ExpectedInsert<'_>) -> u64| {
            let kept: BTreeSet<u64> = old.union(&grouped(pick, true)).copied().collect();
            let planned: BTreeSet<u64> = old.union(&grouped(pick, false)).copied().collect();
            kept.is_subset(after) && after.is_subset(&planned)
        };
    ensure!(
        within(&after_albums, &old_albums, |e| e.album_id),
        "post-transfer album identities differ from the planned grouping"
    );
    ensure!(
        within(&after_artists, &old_artists, |e| e.artist_id),
        "post-transfer artist identities differ from the planned grouping"
    );
    for (id, old) in before.albums() {
        let new = after.albums().get(id).context("prior album disappeared")?;
        ensure!(
            old.sync_id == new.sync_id
                && group_name(
                    &mut case_renamed,
                    "album",
                    old.sync_id,
                    &old.name,
                    &new.name,
                    &declared.albums
                )
                && old.artist_internal_id == new.artist_internal_id
                && old.artist_sync_id == new.artist_sync_id
                && old.all_compilations == new.all_compilations
                && compilation_members_kept(old, new)
                && new.live_track_count
                    == old.live_track_count + count(|e| e.album_id, old.sync_id),
            "prior album changed outside the expected track membership increment"
        );
    }
    for new in after.albums().values() {
        if !old_albums.contains(&new.sync_id) {
            ensure!(
                new.live_track_count == count(|e| e.album_id, new.sync_id),
                "new album membership differs from the batch"
            );
        }
    }
    for (id, old) in before.artists() {
        let new = after
            .artists()
            .get(id)
            .context("prior artist disappeared")?;
        ensure!(
            old.sync_id == new.sync_id
                && group_name(
                    &mut case_renamed,
                    "album_artist",
                    old.sync_id,
                    &old.name,
                    &new.name,
                    &declared.album_artists
                )
                && new.live_track_count
                    == old.live_track_count + count(|e| e.artist_id, old.sync_id),
            "prior artist changed outside the expected track membership increment"
        );
    }
    for new in after.artists().values() {
        if !old_artists.contains(&new.sync_id) {
            ensure!(
                new.live_track_count == count(|e| e.artist_id, new.sync_id),
                "new artist membership differs from the batch"
            );
        }
    }
    for (id, cache) in before.artwork().track_cache_ids() {
        ensure!(
            after.artwork().track_cache_ids().get(id) == Some(cache),
            "prior track artwork registration changed"
        );
    }
    let released = released_album_cache_ids(before, after, expected, covers);
    let covers = verify_batch_covers(before, after, expected, locationless, removed, covers)?;
    ensure!(
        before
            .artwork()
            .occupied_cache_ids()
            .difference(&released)
            .all(|id| after.artwork().occupied_cache_ids().contains(id)),
        "prior artwork namespace lost an ID"
    );
    let orphans = before.artwork().orphan_album_cache_ids();
    ensure!(
        after
            .artwork()
            .orphan_album_cache_ids()
            .iter()
            .all(|(album, cache)| orphans.get(album) == Some(cache)),
        "an album registration lost its album during insertion"
    );
    let ids = |set: BTreeSet<u64>, old: &BTreeSet<u64>| {
        set.difference(old).map(|id| format!("{id:016x}")).collect()
    };
    Ok(VerifiedInsertBatch {
        schema: "music-protocol.insert-batch-catalog-verification",
        track_ids: expected
            .iter()
            .map(|e| format!("{:016x}", e.track_id))
            .collect(),
        media_anchor: after.persisted_media_anchor(),
        before_database_sha256: hex(&before.source_hashes().database),
        before_wal_sha256: before.source_hashes().wal.map(|v| hex(&v)),
        after_database_sha256: hex(&after.source_hashes().database),
        after_wal_sha256: after.source_hashes().wal.map(|v| hex(&v)),
        supported_metadata_matches: true,
        prior_tracks_preserved: before.tracks().len(),
        prior_recent_counter_resets,
        prior_tracks_listened_between_runs,
        case_renamed,
        prior_artwork_registrations_preserved: true,
        new_albums: ids(after_albums, &old_albums),
        new_artists: ids(after_artists, &old_artists),
        new_composers,
        normalized_fields,
        locationless_track_ids: locationless.iter().map(|id| format!("{id:016x}")).collect(),
        removed_track_ids: removed.iter().map(|id| format!("{id:016x}")).collect(),
        released_album_cache_ids: released
            .into_iter()
            .filter(|id| !after.artwork().occupied_cache_ids().contains(id))
            .collect(),
        dropped_orphan_album_registrations: orphans.len()
            - after.artwork().orphan_album_cache_ids().len(),
        covers,
        audio_and_artwork_bytes_verified_by_this_check: false,
    })
}

/// Prior album cache IDs that may leave the namespace. An album that an
/// interrupted batch pointed at a cover that never arrived registers an ID with
/// no cached object; when the album takes a new member's cover, that ID leaves
/// with its last registration, as for a single cover update.
fn released_album_cache_ids(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    covers: &BTreeMap<u64, u32>,
) -> BTreeSet<u32> {
    let (old, new) = (before.artwork(), after.artwork());
    old.album_cache_ids()
        .iter()
        .filter(|(album, prior)| {
            new.album_cache_ids().get(*album).is_some_and(|now| {
                now != *prior
                    && expected
                        .iter()
                        .any(|e| e.album_id == **album && covers.get(&e.track_id) == Some(now))
            }) && !old.cached_ids().contains(*prior)
                && !old.track_cache_ids().values().any(|id| id == *prior)
                && !old.orphan_album_cache_ids().values().any(|id| id == *prior)
                && old
                    .album_cache_ids()
                    .values()
                    .filter(|id| id == prior)
                    .count()
                    == 1
        })
        .map(|(_, prior)| *prior)
        .collect()
}

/// The cover rules of `verify_insert_batch_covered`; prior track registrations
/// were checked by the caller.
fn verify_batch_covers(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    locationless: &BTreeSet<u64>,
    removed: &BTreeSet<u64>,
    covers: &BTreeMap<u64, u32>,
) -> Result<Vec<InsertedCover>> {
    let (old, new) = (before.artwork(), after.artwork());
    ensure!(
        covers
            .keys()
            .all(|track| expected.iter().any(|e| e.track_id == *track)),
        "a cover names a track outside the batch"
    );
    let planned: BTreeSet<u32> = covers.values().copied().collect();
    ensure!(
        planned.len() == covers.len() && planned.iter().all(|c| *c >= 100),
        "batch covers need distinct unreserved cache IDs"
    );
    ensure!(
        planned.is_disjoint(old.occupied_cache_ids()),
        "a planned cover cache ID was already occupied"
    );
    ensure!(
        new.occupied_cache_ids()
            .difference(old.occupied_cache_ids())
            .all(|c| planned.contains(c)),
        "an unplanned artwork cache ID appeared"
    );
    let mut album_covers: BTreeMap<u64, BTreeSet<u32>> = BTreeMap::new();
    for target in expected {
        let registered = new.track_cache_ids().get(&target.track_id);
        match covers.get(&target.track_id) {
            _ if removed.contains(&target.track_id) => ensure!(
                registered.is_none(),
                "removed track {:016x} kept a cover registration",
                target.track_id
            ),
            Some(cache) if locationless.contains(&target.track_id) => ensure!(
                registered.is_none_or(|r| r == cache),
                "locationless track {:016x} registered another cover",
                target.track_id
            ),
            Some(cache) => ensure!(
                registered == Some(cache),
                "inserted track {:016x} does not hold its cover",
                target.track_id
            ),
            None => ensure!(
                registered.is_none()
                    && !new
                        .tracks_with_artwork_registration()
                        .contains(&target.track_id),
                "inserted track {:016x} gained an unplanned cover",
                target.track_id
            ),
        }
        if let Some(cache) = covers.get(&target.track_id) {
            album_covers
                .entry(target.album_id)
                .or_default()
                .insert(*cache);
        }
    }
    let takes_member_cover = |album: &u64, cache: &u32| {
        album_covers
            .get(album)
            .is_some_and(|caches| caches.contains(cache))
    };
    for (album, cache) in old.album_cache_ids() {
        let now = new.album_cache_ids().get(album);
        ensure!(
            now == Some(cache) || now.is_some_and(|c| takes_member_cover(album, c)),
            "prior album artwork registration changed"
        );
    }
    for (album, cache) in new.album_cache_ids() {
        ensure!(
            old.album_cache_ids().contains_key(album) || takes_member_cover(album, cache),
            "album {album:016x} gained an unplanned cover registration"
        );
    }
    Ok(expected
        .iter()
        .filter_map(|e| {
            covers.get(&e.track_id).map(|cache| InsertedCover {
                track_id: format!("{:016x}", e.track_id),
                cache_id: *cache,
                album_id: format!("{:016x}", e.album_id),
                album_cache_id: new.album_cache_ids().get(&e.album_id).copied(),
            })
        })
        .collect())
}

/// Each present target references the existing composer of its name, or the
/// one new composer its name created in this batch; prior composers and
/// references are exact. A removed target has no reference, and a new
/// composer of its name may stay without members.
fn verify_batch_composers(
    before: &Catalog,
    after: &Catalog,
    expected: &[ExpectedInsert<'_>],
    presented: &[Option<Presentation<'_>>],
    removed: &BTreeSet<u64>,
) -> Result<(Vec<String>, Vec<CaseRename>)> {
    ensure!(
        after.track_composers().len() + removed.len()
            == before.track_composers().len() + expected.len(),
        "composer reference count differs"
    );
    // New lookups by folded name, and the spellings of the present targets
    // that joined each lookup.
    let mut created: BTreeMap<String, i64> = BTreeMap::new();
    let mut joined: BTreeMap<i64, BTreeSet<&str>> = BTreeMap::new();
    // The sort names sent to each new lookup: a presented target's, or the
    // name.
    let mut sorted: BTreeMap<i64, BTreeSet<&str>> = BTreeMap::new();
    let mut leftover = BTreeSet::new();
    for (target, presentation) in expected.iter().zip(presented.iter().copied()) {
        if removed.contains(&target.track_id) {
            ensure!(
                !after.track_composers().contains_key(&target.track_id),
                "removed track {:016x} kept a composer reference",
                target.track_id
            );
            if let Some(name) = target.metadata.projected()?.composer {
                leftover.insert(name.to_owned());
            }
            continue;
        }
        let reference = after
            .track_composers()
            .get(&target.track_id)
            .context("inserted composer reference absent")?;
        let desired = target.metadata.projected()?.composer;
        let prior = desired
            .map(|name| before.composer_for_label(name))
            .transpose()?
            .flatten();
        let current = reference
            .lookup_id
            .map(|id| {
                after
                    .composers()
                    .get(&id)
                    .context("inserted composer lookup absent")
            })
            .transpose()?;
        match (desired, prior, current) {
            (None, None, None) => {}
            (Some(name), None, Some(row)) => {
                ensure!(
                    !before.composers().contains_key(&row.internal_id),
                    "new composer lookup differs from source"
                );
                let shared = *created.entry(folded(name)).or_insert(row.internal_id);
                ensure!(
                    shared == row.internal_id,
                    "one new composer name created several lookups"
                );
                joined.entry(row.internal_id).or_default().insert(name);
                sorted
                    .entry(row.internal_id)
                    .or_default()
                    .insert(presentation.and_then(|p| p.sort_composer).unwrap_or(name));
            }
            (Some(name), Some(old), Some(new)) => {
                ensure!(
                    old.internal_id == new.internal_id
                        && old.sync_id == new.sync_id
                        && old.name == new.name,
                    "reused composer identity or label changed"
                );
                joined.entry(new.internal_id).or_default().insert(name);
            }
            _ => anyhow::bail!("inserted composer join differs from source and prior lookup"),
        }
    }
    // Members naming one composer in different ASCII case share its lookup:
    // its name and sort name are each one of their spellings (seen: the name
    // of the first member, the sort name of a later one).
    let spelled = |id: &i64, text: Option<&str>| {
        text.is_some_and(|text| joined.get(id).is_some_and(|names| names.contains(text)))
    };
    for id in created.values() {
        let row = &after.composers()[id];
        ensure!(
            spelled(id, Some(&row.name))
                && row
                    .sort_name
                    .as_deref()
                    .is_some_and(|sort| sorted.get(id).is_some_and(|sent| sent.contains(sort))),
            "new composer lookup differs from source"
        );
    }
    let expected_ids: BTreeSet<_> = before
        .composers()
        .keys()
        .copied()
        .chain(created.values().copied())
        .collect();
    let after_ids: BTreeSet<_> = after.composers().keys().copied().collect();
    let mut stray = BTreeSet::new();
    for id in after_ids.difference(&expected_ids) {
        let row = &after.composers()[id];
        ensure!(
            leftover.contains(&row.name)
                && !created.contains_key(&folded(&row.name))
                && before.composer_for_label(&row.name)?.is_none()
                && stray.insert(row.name.clone()),
            "unexpected composer lookup creation or removal"
        );
    }
    ensure!(
        expected_ids.is_subset(&after_ids),
        "unexpected composer lookup creation or removal"
    );
    // A prior lookup keeps its name; a target naming it in another ASCII case
    // may give it that spelling as its sort name.
    let mut renamed = Vec::new();
    for (id, old) in before.composers() {
        let new = &after.composers()[id];
        let sorted = new.internal_id == old.internal_id
            && new.sync_id == old.sync_id
            && new.name == old.name
            && new.sort_name != old.sort_name
            && new
                .sort_name
                .as_deref()
                .is_some_and(|sort| same_name(sort, &old.name))
            && spelled(id, new.sort_name.as_deref());
        ensure!(*new == *old || sorted, "prior composer lookup changed");
        if sorted {
            renamed.push(CaseRename {
                kind: "composer_sort",
                id: format!("{:016x}", old.sync_id),
                before: old.sort_name.clone().unwrap_or_default(),
                after: new.sort_name.clone().unwrap_or_default(),
            });
        }
    }
    for (id, old) in before.track_composers() {
        let new = after
            .track_composers()
            .get(id)
            .context("prior composer reference disappeared")?;
        ensure!(
            old.lookup_id == new.lookup_id,
            "prior composer reference changed"
        );
    }
    // A large session may space every sort key out again, keeping their order
    // (seen in a 200-track insert, for each prior track's orders).
    ensure!(
        keeps_order(before.track_composers(), after.track_composers()),
        "prior composer order changed"
    );
    let names = created
        .values()
        .map(|id| after.composers()[id].name.clone())
        .collect();
    Ok((names, renamed))
}

/// Whether the references `before` holds keep their order in `after`: equal
/// orders stay equal and lower ones stay lower.
fn keeps_order(
    before: &BTreeMap<u64, ComposerReference>,
    after: &BTreeMap<u64, ComposerReference>,
) -> bool {
    let mut orders: Vec<_> = before
        .iter()
        .filter_map(|(id, old)| Some((old.order, after.get(id)?.order)))
        .collect();
    orders.sort_unstable();
    orders.windows(2).all(|pair| {
        if pair[0].0 == pair[1].0 {
            pair[0].1 == pair[1].1
        } else {
            pair[0].1 < pair[1].1
        }
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compilation_source(letter: &str, compilation: bool) -> Metadata {
        let mut source = crate::media::metadata::read(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/silence-alac.m4a"),
        )
        .unwrap();
        source.title = vec![format!("Fixture {letter}")];
        source.artist = vec![format!("Guest {letter}")];
        source.album = vec!["Compilation fixture album".into()];
        source.album_artist = vec!["CLI Compilation Σύνολο".into()];
        source.genre.clear();
        source.composer.clear();
        source.copyright.clear();
        source.year = None;
        source.track = None;
        source.track_total = None;
        source.disc = None;
        source.disc_total = None;
        source.content_rating = None;
        source.compilation = Some(compilation);
        source
    }

    fn compilation_state(stage: u32, change: &str) -> Catalog {
        compilation_state_at(stage, 12 + stage, change)
    }

    fn compilation_state_at(stage: u32, anchor: u32, change: &str) -> Catalog {
        let mut sql = String::new();
        if stage >= 1 {
            sql.push_str(
                "INSERT INTO album_artist VALUES(121,8000,'CLI Compilation Σύνολο',NULL);
                INSERT INTO album VALUES(131,9000,'Compilation fixture album',121,1,NULL);
                INSERT INTO item_artist VALUES(141,0,'Guest A',NULL);
                INSERT INTO item VALUES(2,8,131,121,141,0,0,51,0,0,1,1,0);
                INSERT INTO item_extra VALUES(2,'Fixture A','NEWA.m4a',50.0,1000,0,0,0,NULL,0,0,NULL,NULL);
                INSERT INTO item_store VALUES(2,10000);
                INSERT INTO item_state VALUES(2,0,1,1,1);
                INSERT INTO item_stats VALUES(2,0,0,0,0,0,0,0,0);",
            );
        }
        if stage >= 2 {
            sql.push_str(
                "INSERT INTO item_artist VALUES(142,0,'Guest B',NULL);
                INSERT INTO item VALUES(3,8,131,121,142,0,0,51,0,0,1,0,0);
                INSERT INTO item_extra VALUES(3,'Fixture B','NEWB.m4a',50.0,1000,0,0,0,NULL,0,0,NULL,NULL);
                INSERT INTO item_store VALUES(3,10001);
                INSERT INTO item_state VALUES(3,0,1,1,1);
                INSERT INTO item_stats VALUES(3,0,0,0,0,0,0,0,0);",
            );
        }
        sql.push_str(change);
        crate::test_support::catalogue::modified_collection_catalog(anchor, &sql)
    }

    fn compilation_expected(source: &Metadata, first: bool) -> ExpectedInsert<'_> {
        ExpectedInsert {
            track_id: if first { 10000 } else { 10001 },
            album_id: 9000,
            artist_id: 8000,
            path: if first {
                "iTunes_Control/Music/F39/NEWA.m4a"
            } else {
                "iTunes_Control/Music/F39/NEWB.m4a"
            },
            file_bytes: 1000,
            metadata: source.into(),
            presented: false,
        }
    }

    #[test]
    fn explicit_compilation_then_ordinary_member_preserves_catalogue_and_aggregates() {
        let before = compilation_state(0, "");
        let first = compilation_state(1, "");
        let second = compilation_state(2, "");
        let a = compilation_source("A", true);
        let b = compilation_source("B", false);
        assert_eq!(
            verify_insert(&before, &first, &compilation_expected(&a, true))
                .unwrap()
                .prior_tracks_preserved,
            1
        );
        assert_eq!(
            verify_insert(&first, &second, &compilation_expected(&b, false))
                .unwrap()
                .prior_tracks_preserved,
            2
        );
        let album = &second.albums()[&131];
        assert_eq!(album.live_track_count, 2);
        assert!(album.all_compilations && album.has_compilation_tracks);
        assert_eq!(
            second
                .tracks()
                .iter()
                .filter(|t| t.album_sync_id == 9000)
                .map(|t| t.metadata.is_compilation)
                .collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn an_emptied_source_tag_is_expected_absent_from_the_inserted_row() {
        let first = compilation_state(1, "");
        let removed = compilation_source("B", false);
        let mut emptied = compilation_source("B", false);
        for field in [
            &mut emptied.genre,
            &mut emptied.composer,
            &mut emptied.copyright,
            &mut emptied.sort_composer,
        ] {
            *field = vec![String::new()];
        }
        let verified = |source: &Metadata, after: &Catalog| {
            verify_insert(&first, after, &compilation_expected(source, false))
                .map(|receipt| serde_json::to_value(receipt).unwrap())
        };
        // The row the insert left, with no copyright, composer or genre,
        // verifies as it does for a source without those tags.
        let second = compilation_state(2, "");
        assert_eq!(
            verified(&emptied, &second).unwrap(),
            verified(&removed, &second).unwrap()
        );
        // A row holding a copyright the emptied tag does not name is refused.
        let named = compilation_state(
            2,
            "UPDATE item_extra SET copyright='Artist X' WHERE item_pid=3;",
        );
        let error = verified(&emptied, &named).unwrap_err();
        assert!(
            error.to_string().contains("inserted text metadata differs"),
            "{error:#}"
        );
    }

    #[test]
    fn a_single_insert_may_space_prior_composer_orders_out() {
        // A single insert after a long run of others gave every prior track
        // new sort keys in the same order, as a large batch does.
        let first = compilation_state(1, "");
        let source = compilation_source("B", false);
        let b = compilation_expected(&source, false);
        let spaced = "UPDATE item SET composer_order=composer_order*3+5 WHERE item_pid IN (1,2)";
        verify_insert(&first, &compilation_state(2, spaced), &b).unwrap();
        // Track 2 (order 0) after track 1 (order 100) refuses.
        let crossed = "UPDATE item SET composer_order=item_pid WHERE item_pid IN (1,2)";
        let error = verify_insert(&first, &compilation_state(2, crossed), &b)
            .err()
            .expect("crossed composer orders verified");
        assert!(
            error.to_string().contains("prior composer order changed"),
            "{error:#}"
        );
    }

    #[test]
    fn one_session_inserts_two_members_of_one_new_album() {
        let before = compilation_state(0, "");
        let after = compilation_state_at(2, 13, "");
        let a = compilation_source("A", true);
        let b = compilation_source("B", false);
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        let checked = verify_insert_batch(&before, &after, &expected).unwrap();
        assert_eq!(checked.prior_tracks_preserved, 1);
        assert_eq!(checked.track_ids, ["0000000000002710", "0000000000002711"]);
        assert_eq!(checked.new_albums, ["0000000000002328"]);
        assert_eq!(checked.new_artists, ["0000000000001f40"]);
        // A missing member, a second anchor step or a changed prior track refuse.
        assert!(verify_insert_batch(&before, &compilation_state_at(1, 13, ""), &expected).is_err());
        assert!(verify_insert_batch(&before, &compilation_state(2, ""), &expected).is_err());
        assert!(
            verify_insert_batch(
                &before,
                &compilation_state_at(
                    2,
                    13,
                    "UPDATE item_stats SET play_count_user=1 WHERE item_pid=1"
                ),
                &expected
            )
            .is_err()
        );
        assert!(verify_insert_batch(&before, &after, &expected[..1]).is_err());
        assert!(verify_insert_batch(&before, &after, &[]).is_err());
        // The phone may space the prior track's sort keys out again.
        let spaced = "UPDATE item SET composer_order=composer_order*3+5 WHERE item_pid=1";
        verify_insert_batch(&before, &compilation_state_at(2, 13, spaced), &expected).unwrap();
    }

    #[test]
    fn a_session_that_also_removes_tracks_is_checked_against_them_gone() {
        // Both members of the album go and a new one refills it, in place.
        let before = compilation_state(2, "");
        let after = compilation_state_at(
            0,
            15,
            "INSERT INTO album_artist VALUES(121,8000,'CLI Compilation Σύνολο',NULL);
            INSERT INTO album VALUES(131,9000,'Compilation fixture album',121,1,NULL);
            INSERT INTO item_artist VALUES(143,0,'Guest C',NULL);
            INSERT INTO item VALUES(4,8,131,121,143,0,0,51,0,0,1,0,0);
            INSERT INTO item_extra VALUES(4,'Fixture C','NEWC.m4a',50.0,1000,0,0,0,NULL,0,0,NULL,NULL);
            INSERT INTO item_store VALUES(4,10002);
            INSERT INTO item_state VALUES(4,0,1,1,1);
            INSERT INTO item_stats VALUES(4,0,0,0,0,0,0,0,0);",
        );
        let c = compilation_source("C", false);
        let expected = [ExpectedInsert {
            track_id: 10002,
            path: "iTunes_Control/Music/F39/NEWC.m4a",
            ..compilation_expected(&c, true)
        }];
        let removed = BTreeSet::from([10000, 10001]);
        let projected = before.without_tracks(&removed, &after).unwrap();
        let checked = verify_insert_batch(&projected, &after, &expected).unwrap();
        assert_eq!(checked.prior_tracks_preserved, 1);
        assert!(checked.new_albums.is_empty());
        assert!(verify_insert_batch(&before, &after, &expected).is_err());
        // A removal that kept one of them is not the session that ran.
        let kept = before
            .without_tracks(&BTreeSet::from([10000]), &after)
            .unwrap();
        assert!(verify_insert_batch(&kept, &after, &expected).is_err());
    }

    #[test]
    fn prior_composer_orders_may_move_but_keep_their_order() {
        let references = |orders: &[i64]| -> BTreeMap<u64, ComposerReference> {
            (1..)
                .zip(orders)
                .map(|(id, &order)| {
                    let lookup_id = Some(72);
                    (id, ComposerReference { lookup_id, order })
                })
                .collect()
        };
        let before = references(&[10, 20, 20, 30]);
        assert!(keeps_order(&before, &before));
        assert!(keeps_order(&before, &references(&[40, 90, 90, 200])));
        // Two orders crossing, an equal pair split or two meeting refuse.
        assert!(!keeps_order(&before, &references(&[40, 90, 90, 80])));
        assert!(!keeps_order(&before, &references(&[40, 90, 91, 200])));
        assert!(!keeps_order(&before, &references(&[90, 90, 90, 200])));
    }

    /// The sort names and comment two presented members of a new album sent,
    /// as `after` holds them.
    const PRESENTED: &str = "UPDATE item_extra SET sort_title=title WHERE item_pid=3;
        UPDATE item_extra SET sort_title='Fixture A, sorted',comment='Line one
Line two' WHERE item_pid=2;
        UPDATE album SET sort_album='Compilation fixture album' WHERE album_pid=131;
        UPDATE album_artist SET sort_album_artist='CLI Compilation Σύνολο' WHERE album_artist_pid=121;
        UPDATE item_artist SET sort_item_artist=item_artist WHERE item_artist_pid IN (141,142);";

    #[test]
    fn presented_members_store_their_comment_and_sort_names() {
        let before = compilation_state(0, "");
        let after = compilation_state_at(2, 13, PRESENTED);
        let member_a = || {
            let mut a = compilation_source("A", true);
            a.comment = vec!["Line one\nLine two".into()];
            a.sort_title = vec!["Fixture A, sorted".into()];
            a
        };
        let (a, b) = (member_a(), compilation_source("B", false));
        let presented = |a, b| {
            let mut expected = [
                compilation_expected(a, true),
                compilation_expected(b, false),
            ];
            for target in &mut expected {
                target.presented = true;
            }
            expected
        };
        verify_insert_batch(&before, &after, &presented(&a, &b)).unwrap();
        // An insert that sent nothing new is not checked for them.
        verify_insert_batch(
            &before,
            &after,
            &[
                compilation_expected(&a, true),
                compilation_expected(&b, false),
            ],
        )
        .unwrap();
        // A comment or sort name other than the one sent refuses.
        let mut other = member_a();
        other.comment.clear();
        assert!(verify_insert_batch(&before, &after, &presented(&other, &b)).is_err());
        let mut other = member_a();
        other.sort_title.clear();
        assert!(verify_insert_batch(&before, &after, &presented(&other, &b)).is_err());
        // A new album holds a sort name one of its members sent.
        let (mut other_a, mut other_b) = (member_a(), compilation_source("B", false));
        other_b.sort_album = vec!["Elsewhere".into()];
        verify_insert_batch(&before, &after, &presented(&a, &other_b)).unwrap();
        other_a.sort_album = vec!["Elsewhere".into()];
        assert!(verify_insert_batch(&before, &after, &presented(&other_a, &other_b)).is_err());
        let mut other = compilation_source("B", false);
        other.sort_artist = vec!["B, Guest".into()];
        assert!(verify_insert_batch(&before, &after, &presented(&a, &other)).is_err());
        // A prior group or track that changed its sort name or comment refuses.
        for change in [
            "UPDATE album SET sort_album='Moved' WHERE album_pid=31;",
            "UPDATE item_artist SET sort_item_artist='Moved' WHERE item_artist_pid=41;",
            "UPDATE item_extra SET comment='Moved' WHERE item_pid=1;",
            "UPDATE item_extra SET sort_title='Moved' WHERE item_pid=1;",
        ] {
            let after = compilation_state_at(2, 13, &format!("{PRESENTED}{change}"));
            assert!(
                verify_insert_batch(&before, &after, &presented(&a, &b)).is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn a_presented_member_joining_a_group_leaves_its_sort_name() {
        // The insert sends a joined group's stored sort name back.
        let stored = "UPDATE album SET sort_album='Album, sorted' WHERE album_pid=131;
            UPDATE album_artist SET sort_album_artist='Artist, sorted' WHERE album_artist_pid=121;
            UPDATE item_artist SET sort_item_artist='Guest A' WHERE item_artist_pid=141;";
        let before = compilation_state(1, stored);
        let after = compilation_state(
            2,
            &format!(
                "{stored}UPDATE item_extra SET sort_title=title WHERE item_pid=3;
                UPDATE item_artist SET sort_item_artist='Guest B' WHERE item_artist_pid=142;"
            ),
        );
        let b = compilation_source("B", false);
        let mut expected = compilation_expected(&b, false);
        expected.presented = true;
        verify_insert(&before, &after, &expected).unwrap();
        let moved = compilation_state(
            2,
            &format!(
                "{stored}UPDATE item_extra SET sort_title=title WHERE item_pid=3;
                UPDATE item_artist SET sort_item_artist='Guest B' WHERE item_artist_pid=142;
                UPDATE album SET sort_album='Compilation fixture album' WHERE album_pid=131;"
            ),
        );
        assert!(verify_insert(&before, &moved, &expected).is_err());
    }

    #[test]
    fn an_artist_matched_by_name_ignoring_case_is_the_same_group() {
        // On the phone, a track tagged `Ca$h` was filed under the artist
        // `CA$H`, the spelling of a later member of the same batch.
        let before = compilation_state(0, "");
        let after = compilation_state_at(2, 13, "");
        let a = compilation_source("A", true);
        let mut b = compilation_source("B", false);
        b.artist = vec!["GUEST b".into()];
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        assert!(verify_insert_batch(&before, &after, &expected).is_ok());
        b.artist = vec!["Guest C".into()];
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        assert!(verify_insert_batch(&before, &after, &expected).is_err());
    }

    #[test]
    fn a_group_the_phone_renamed_to_a_targets_spelling_is_recorded() {
        // On the phone, a member tagged `Ca$h` renamed the artist `CA$H` an
        // earlier batch had left, and its prior member shows it.
        let before = compilation_state(1, "");
        let renamed = "UPDATE item_artist SET item_artist='GUEST A' WHERE item_artist_pid=141;
            UPDATE item SET item_artist_pid=141 WHERE item_pid=3;
            DELETE FROM item_artist WHERE item_artist_pid=142;";
        let after = compilation_state_at(2, 14, renamed);
        let mut b = compilation_source("B", false);
        b.artist = vec!["GUEST A".into()];
        let checked =
            verify_insert_batch(&before, &after, &[compilation_expected(&b, false)]).unwrap();
        assert_eq!(checked.case_renamed.len(), 1);
        let rename = &checked.case_renamed[0];
        assert_eq!(
            (
                rename.kind,
                rename.id.as_str(),
                rename.before.as_str(),
                rename.after.as_str()
            ),
            ("track_artist", "0000000000002710", "Guest A", "GUEST A")
        );
        // A spelling no target declared is a changed prior track.
        b.artist = vec!["guest a".into()];
        assert!(verify_insert_batch(&before, &after, &[compilation_expected(&b, false)]).is_err());
    }

    #[test]
    fn members_naming_one_new_composer_in_different_case_share_its_lookup() {
        // On the phone, `Alpha;Beta` then `ALPHA;BETA` in one batch made one
        // lookup named by the first and sorted by the second.
        let shared = |name: &str, sort: &str| {
            format!(
                "INSERT INTO composer VALUES(72,0,'{name}','{sort}');
                UPDATE item SET composer_pid=72 WHERE item_pid IN (2,3);"
            )
        };
        let before = compilation_state(0, "");
        let mut a = compilation_source("A", true);
        a.composer = vec!["Writer X".into()];
        let mut b = compilation_source("B", false);
        b.composer = vec!["WRITER X".into()];
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        let after = compilation_state_at(2, 13, &shared("Writer X", "WRITER X"));
        let checked = verify_insert_batch(&before, &after, &expected).unwrap();
        assert_eq!(checked.new_composers, ["Writer X"]);
        // A spelling no member declared refuses.
        let after = compilation_state_at(2, 13, &shared("Writer X", "writer x"));
        assert!(verify_insert_batch(&before, &after, &expected).is_err());
    }

    /// Artwork rows: one cached object per ID, then (entity, ID, artwork type,
    /// entity type) registrations, tracks by item and albums by album row.
    fn art(objects: &[u32], tokens: &[(i64, u32, i64, i64)]) -> String {
        let mut sql = String::new();
        for id in objects {
            sql.push_str(&format!(
                "INSERT INTO artwork VALUES('{id}',300,6,0,'aa/00000000000000000000000000000000000000');"
            ));
        }
        for (entity, id, kind, entity_type) in tokens {
            sql.push_str(&format!(
                "INSERT INTO artwork_token VALUES({entity},'{id}',300,{kind},{entity_type},0);"
            ));
        }
        sql
    }

    #[test]
    fn covered_members_register_their_covers_and_may_cover_their_album() {
        let a = compilation_source("A", true);
        let b = compilation_source("B", false);
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        let covers = BTreeMap::from([(10000, 140), (10001, 141)]);
        let none = BTreeSet::new();
        let tracks = [(2, 140, 1, 0), (3, 141, 1, 0)];
        let after = |album: Option<u32>, tracks: &[(i64, u32, i64, i64)], objects: &[u32]| {
            let mut tokens = tracks.to_vec();
            tokens.extend(album.map(|id| (131, id, 6, 4)));
            compilation_state_at(2, 13, &art(objects, &tokens))
        };
        let check = |before: &Catalog, after: &Catalog| {
            verify_insert_batch_covered(before, after, &expected, &none, &none, &covers)
        };
        // A new album takes one of its members' covers, or none.
        let new = compilation_state(0, "");
        for album in [Some(140), Some(141), None] {
            let checked = check(&new, &after(album, &tracks, &[140, 141])).unwrap();
            assert_eq!(checked.covers.len(), 2);
            assert_eq!(checked.covers[0].album_cache_id, album);
        }
        for (album, tracks, objects, message) in [
            (
                Some(150),
                &tracks[..],
                &[140, 141, 150][..],
                "unplanned artwork cache ID",
            ),
            (
                None,
                &[(2, 141, 1, 0), (3, 140, 1, 0)][..],
                &[140, 141][..],
                "does not hold its cover",
            ),
            (
                None,
                &tracks[..1],
                &[140, 141][..],
                "does not hold its cover",
            ),
        ] {
            let error = check(&new, &after(album, tracks, objects))
                .err()
                .expect(message);
            assert!(
                format!("{error:#}").contains(message),
                "{message}: {error:#}"
            );
        }
        // Without covers nothing may register.
        let error =
            verify_insert_batch_with(&new, &after(None, &tracks, &[140, 141]), &expected, &none)
                .err()
                .expect("an uncovered batch registered covers");
        assert!(
            format!("{error:#}").contains("unplanned artwork cache ID"),
            "{error:#}"
        );
        // A prior album (here emptied earlier) keeps its registration or takes
        // a new member's cover. Its members are ordinary tracks here: an empty
        // album has no compilation member, and that aggregate must not change.
        let ordinary = compilation_source("A", false);
        let refill = [
            compilation_expected(&ordinary, true),
            compilation_expected(&b, false),
        ];
        let emptied = "INSERT INTO album_artist VALUES(121,8000,'CLI Compilation Σύνολο',NULL);
            INSERT INTO album VALUES(131,9000,'Compilation fixture album',121,1,NULL);";
        let before =
            compilation_state(0, &format!("{emptied}{}", art(&[120], &[(131, 120, 6, 4)])));
        let refilled = |album: Option<u32>| {
            let mut tokens = tracks.to_vec();
            tokens.extend(album.map(|id| (131, id, 6, 4)));
            let sql = format!(
                "UPDATE item SET is_compilation=0 WHERE item_pid=2;{}",
                art(&[120, 140, 141], &tokens)
            );
            verify_insert_batch_covered(
                &before,
                &compilation_state_at(2, 13, &sql),
                &refill,
                &none,
                &none,
                &covers,
            )
        };
        for album in [120, 140, 141] {
            refilled(Some(album)).unwrap();
        }
        assert!(refilled(None).is_err());
        // An album without a covered member keeps its own registration.
        let live = compilation_state(0, &art(&[120], &[(31, 120, 6, 4)]));
        let moved = compilation_state_at(
            2,
            13,
            &art(
                &[120, 140, 141],
                &[(2, 140, 1, 0), (3, 141, 1, 0), (31, 140, 6, 4)],
            ),
        );
        let error = check(&live, &moved)
            .err()
            .expect("a live album's cover moved");
        assert!(format!("{error:#}").contains("prior album artwork registration changed"));
    }

    #[test]
    fn an_album_taking_a_member_cover_may_release_its_dangling_cache_id() {
        let a = compilation_source("A", false);
        let b = compilation_source("B", false);
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        let covers = BTreeMap::from([(10000, 140), (10001, 141)]);
        let none = BTreeSet::new();
        let emptied = "INSERT INTO album_artist VALUES(121,8000,'CLI Compilation Σύνολο',NULL);
            INSERT INTO album VALUES(131,9000,'Compilation fixture album',121,1,NULL);";
        // The emptied album points at 120, which has no cached object.
        let dangling = compilation_state(0, &format!("{emptied}{}", art(&[], &[(131, 120, 6, 4)])));
        let cached =
            compilation_state(0, &format!("{emptied}{}", art(&[120], &[(131, 120, 6, 4)])));
        let after = |album: u32, objects: &[u32]| {
            let tokens = [(2, 140, 1, 0), (3, 141, 1, 0), (131, album, 6, 4)];
            compilation_state_at(
                2,
                13,
                &format!(
                    "UPDATE item SET is_compilation=0 WHERE item_pid=2;{}",
                    art(objects, &tokens)
                ),
            )
        };
        let check = |before: &Catalog, after: &Catalog| {
            verify_insert_batch_covered(before, after, &expected, &none, &none, &covers)
        };
        let checked = check(&dangling, &after(141, &[140, 141])).unwrap();
        assert_eq!(checked.released_album_cache_ids, [120]);
        // An album that keeps its registration releases nothing; a cached ID
        // never leaves.
        let kept = check(&dangling, &after(120, &[140, 141])).unwrap();
        assert!(kept.released_album_cache_ids.is_empty());
        let error = check(&cached, &after(141, &[140, 141]))
            .err()
            .expect("a cached ID left");
        assert!(
            format!("{error:#}").contains("namespace lost an ID"),
            "{error:#}"
        );
    }

    #[test]
    fn removed_targets_leave_no_row_and_may_leave_their_groups() {
        let a = compilation_source("A", false);
        let b = compilation_source("B", false);
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        let before = compilation_state(0, "");
        let none = BTreeSet::new();
        let first = BTreeSet::from([10000]);
        let both = BTreeSet::from([10000, 10001]);
        let without_a = |extra: &str| {
            compilation_state_at(
                2,
                13,
                &format!(
                    "DELETE FROM item_extra WHERE item_pid=2; DELETE FROM item_store WHERE item_pid=2;
                    DELETE FROM item_state WHERE item_pid=2; DELETE FROM item_stats WHERE item_pid=2;
                    DELETE FROM item WHERE item_pid=2;{extra}"
                ),
            )
        };
        let check = |after: &Catalog, removed: &BTreeSet<u64>, covers: &BTreeMap<u64, u32>| {
            verify_insert_batch_covered(&before, after, &expected, &none, removed, covers)
        };
        // A was dropped; B is complete in the album both planned.
        let checked = check(&without_a(""), &first, &BTreeMap::new()).unwrap();
        assert_eq!(checked.removed_track_ids, ["0000000000002710"]);
        assert_eq!(checked.new_albums, ["0000000000002328"]);
        // Both were dropped: their new groups may stay without members, or not.
        let emptied = "INSERT INTO album_artist VALUES(121,8000,'CLI Compilation Σύνολο',NULL);
            INSERT INTO album VALUES(131,9000,'Compilation fixture album',121,1,NULL);";
        for after in [
            compilation_state_at(0, 13, emptied),
            compilation_state_at(0, 13, ""),
        ] {
            let checked = check(&after, &both, &BTreeMap::new()).unwrap();
            assert_eq!(checked.removed_track_ids.len(), 2);
        }
        // The dropped member's cover may stay as its album's; it holds none.
        let covers = BTreeMap::from([(10000, 140), (10001, 141)]);
        for album in [Some(140), Some(141), None] {
            let mut tokens = vec![(3, 141, 1, 0)];
            tokens.extend(album.map(|id| (131, id, 6, 4)));
            check(&without_a(&art(&[140, 141], &tokens)), &first, &covers).unwrap();
        }
        // A row that still exists, or one both removed and locationless, refuses.
        let error = check(&compilation_state_at(2, 13, ""), &first, &BTreeMap::new())
            .err()
            .expect("a removed track's row exists");
        assert!(
            format!("{error:#}").contains("track count differs"),
            "{error:#}"
        );
        let error = verify_insert_batch_covered(
            &before,
            &without_a(""),
            &expected,
            &first,
            &first,
            &BTreeMap::new(),
        )
        .err()
        .expect("a track was both removed and locationless");
        assert!(
            format!("{error:#}").contains("not a present batch target"),
            "{error:#}"
        );
        // A present member's groups must exist.
        let error = check(&compilation_state_at(0, 13, ""), &first, &BTreeMap::new())
            .err()
            .expect("the kept member lost its row");
        assert!(
            format!("{error:#}").contains("track count differs"),
            "{error:#}"
        );
    }

    #[test]
    fn a_batch_may_drop_album_registrations_whose_album_was_gone() {
        let a = compilation_source("A", false);
        let b = compilation_source("B", false);
        let expected = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        let orphan = art(&[120], &[(999, 120, 6, 4)]);
        let kept = art(&[120], &[]);
        let ordinary = "UPDATE item SET is_compilation=0 WHERE item_pid=2;";
        let none = BTreeSet::new();
        let check = |before: &str, after: &str| {
            verify_insert_batch_covered(
                &compilation_state(0, before),
                &compilation_state_at(2, 13, &format!("{ordinary}{after}")),
                &expected,
                &none,
                &none,
                &BTreeMap::new(),
            )
        };
        assert_eq!(
            check(&orphan, &kept)
                .unwrap()
                .dropped_orphan_album_registrations,
            1
        );
        assert_eq!(
            check(&orphan, &orphan)
                .unwrap()
                .dropped_orphan_album_registrations,
            0
        );
        let error = check(&kept, &orphan)
            .err()
            .expect("a new orphan registration");
        assert!(format!("{error:#}").contains("lost its album"), "{error:#}");
    }

    #[test]
    fn compilation_verification_refuses_old_member_or_aggregate_drift() {
        let before = compilation_state(1, "");
        let b = compilation_source("B", false);
        for change in [
            "UPDATE item SET is_compilation=0 WHERE item_pid=2",
            "UPDATE item_extra SET title='Changed' WHERE item_pid=2",
            "UPDATE item_stats SET play_count_user=1 WHERE item_pid=1",
            "UPDATE album SET all_compilations=0 WHERE album_pid=131",
            "UPDATE album SET all_compilations=1 WHERE album_pid=31",
            "UPDATE album_artist SET album_artist='Changed' WHERE album_artist_pid=121",
            "UPDATE item SET is_compilation=1 WHERE item_pid=3",
        ] {
            assert!(
                verify_insert(
                    &before,
                    &compilation_state(2, change),
                    &compilation_expected(&b, false)
                )
                .is_err(),
                "{change}"
            );
        }
        let a = compilation_source("A", true);
        assert!(
            verify_insert(
                &compilation_state(0, ""),
                &compilation_state(1, "UPDATE item SET is_compilation=0 WHERE item_pid=2"),
                &compilation_expected(&a, true)
            )
            .is_err()
        );
    }

    #[test]
    fn first_compilation_aggregate_transition_is_not_inferred_by_verifier() {
        let ordinary = "UPDATE item SET is_compilation=0 WHERE item_pid=2; UPDATE album SET all_compilations=0 WHERE album_pid=131;";
        let before = compilation_state(1, ordinary);
        let after = compilation_state(
            2,
            &format!("{ordinary} UPDATE item SET is_compilation=1 WHERE item_pid=3;"),
        );
        let source = compilation_source("B", true);
        let error = verify_insert(&before, &after, &compilation_expected(&source, false))
            .err()
            .unwrap();
        assert!(error.to_string().contains("prior album changed"));
    }

    #[test]
    fn an_emptied_compilation_album_is_refilled_by_a_compilation_track() {
        // `clear` can leave an album so: no members, summary still set.
        let emptied = |summary: u8| {
            compilation_state(
                0,
                &format!(
                    "INSERT INTO album_artist VALUES(121,8000,'CLI Compilation Σύνολο',NULL);
                    INSERT INTO album VALUES(131,9000,'Compilation fixture album',121,{summary},NULL);"
                ),
            )
        };
        let before = emptied(1);
        let a = compilation_source("A", true);
        let b = compilation_source("B", false);
        let first = compilation_state(1, "");
        verify_insert(&before, &first, &compilation_expected(&a, true)).unwrap();
        let both = [
            compilation_expected(&a, true),
            compilation_expected(&b, false),
        ];
        verify_insert_batch(&before, &compilation_state_at(2, 13, ""), &both).unwrap();
        // The summary itself stays, and an emptied ordinary album takes no
        // first compilation.
        let cleared = "UPDATE album SET all_compilations=0 WHERE album_pid=131";
        for (before, after) in [
            (&before, compilation_state(1, cleared)),
            (&emptied(0), compilation_state(1, cleared)),
        ] {
            let error = verify_insert(before, &after, &compilation_expected(&a, true))
                .err()
                .expect("an unmodelled album aggregate verified");
            assert!(
                error.to_string().contains("prior album changed"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn shared_composer_sort_delta_requires_explicit_legacy_policy() {
        let before = crate::test_support::catalogue::composer_transition_catalog(
            "ACME",
            Some("ACME"),
            100,
            false,
        );
        let after = crate::test_support::catalogue::composer_transition_catalog(
            "ACME",
            Some("Acme"),
            200,
            true,
        );
        assert!(verify_composers(&before, &after, 1000, Some("Acme"), None, false).is_err());
        let proof = verify_composers(&before, &after, 1000, Some("Acme"), None, true)
            .unwrap()
            .unwrap();
        assert_eq!(proof.lookup_id, 71);
        assert_eq!(proof.before_sort, "ACME");
        assert_eq!(proof.after_sort, "Acme");
        assert_eq!(proof.prior_reference_count, 1);
        assert_eq!(proof.after_reference_count, 2);
        assert_eq!(proof.order_changes.len(), 1);
        assert_eq!(proof.order_changes[0].before, 100);
        assert_eq!(proof.order_changes[0].after, 200);
        for (name, sort, order, request) in [
            ("ACME", "ACME", 100, "Acme"), // no legacy effect
            ("ACME", "Acme", 100, "Acme"), // unknown sort-only effect
            ("ACME", "Other", 200, "Acme"),
            ("ACME", "Acme", 200, "Other"),
            ("ACME", "Acme", 200, "Acme "),
            ("Acme", "Acme", 200, "Acme"), // display label changed
        ] {
            let wrong = crate::test_support::catalogue::composer_transition_catalog(
                name,
                Some(sort),
                order,
                true,
            );
            assert!(verify_composers(&before, &wrong, 1000, Some(request), None, true).is_err());
        }
    }
    #[test]
    fn strict_composer_reuse_preserves_independent_sort_and_reference_order() {
        let before = crate::test_support::catalogue::composer_transition_catalog(
            "ACME",
            Some("An independent sort"),
            100,
            false,
        );
        let after = crate::test_support::catalogue::composer_transition_catalog(
            "ACME",
            Some("An independent sort"),
            100,
            true,
        );
        assert!(
            verify_composers(&before, &after, 1000, Some("Acme"), None, false)
                .unwrap()
                .is_none()
        );
        assert!(verify_composers(&before, &after, 1000, Some("ACME"), None, false).is_ok());
        assert!(verify_composers(&before, &after, 1000, Some("acme "), None, false).is_err());
        assert!(verify_composers(&before, &after, 1000, None, None, false).is_err());
    }
    #[test]
    fn preservation_rejects_missing_path_metadata_and_availability_changes() {
        let catalog = crate::test_support::catalogue::collection_catalog(13);
        let original = &catalog.tracks()[0];
        assert!(preserve_tracks(catalog.tracks(), &BTreeMap::new()).is_err());
        assert!(
            preserve_tracks(
                catalog.tracks(),
                &BTreeMap::from([(original.sync_id, original)])
            )
            .is_ok()
        );
        for variant in 0..6 {
            let mut changed = original.clone();
            match variant {
                0 => changed.afc_path = None,
                1 => changed.metadata.title = "changed".into(),
                2 => changed.availability.is_placeholder = true,
                3 => changed.playback.play_count_user += 1,
                4 => changed.metadata.content_rating += 1,
                _ => changed.metadata.is_compilation = true,
            };
            assert!(
                preserve_tracks(
                    catalog.tracks(),
                    &BTreeMap::from([(changed.sync_id, &changed)])
                )
                .is_err()
            );
        }
    }
    // Regression reduced from a retained insert capture: this prior track's
    // recent play count drained 10 -> 0 while lifetime counts, dates and every
    // other inspected/raw stats value remained exact. This is only a catalogue
    // verifier test; transport, complete readback and journal acceptance are
    // separately required evidence.
    #[test]
    fn insert_capture_recent_counters_reset_preserves_prior_track() {
        let prior_stats = "ALTER TABLE item_stats ADD COLUMN date_played REAL; \
            ALTER TABLE item_stats ADD COLUMN date_accessed REAL; \
            ALTER TABLE item_stats ADD COLUMN bookmark_time_ms REAL; \
            UPDATE item_store SET sync_id=-1234567890123456789 WHERE item_pid=1; \
            UPDATE item_stats SET play_count_user=10,play_count_recent=10, \
            date_played=750208100.121234,date_accessed=750153587.007244, \
            bookmark_time_ms=0.0 WHERE item_pid=1;";
        let before = compilation_state(0, prior_stats);
        let after = compilation_state(
            1,
            &format!(
                "{prior_stats} \
                 UPDATE item_stats SET play_count_recent=0 WHERE item_pid=1; \
                 UPDATE item SET is_compilation=0 WHERE item_pid=2; \
                 UPDATE album SET all_compilations=0 WHERE album_pid=131;"
            ),
        );
        let source = compilation_source("A", false);
        let proof = verify_insert(&before, &after, &compilation_expected(&source, true))
            .expect("the qualified recent-counter reset preserves the prior track");
        assert_eq!(proof.prior_tracks_preserved, 1);
        assert_eq!(before.tracks()[0].sync_id, 0xeeddef0b82167eeb);
        assert_eq!(before.tracks()[0].playback.play_count_recent, 10);
        assert_eq!(after.tracks()[0].playback.play_count_recent, 0);
    }

    #[test]
    fn insert_capture_recent_counter_reset_rejects_every_other_prior_change() {
        let prior_stats = "ALTER TABLE item_stats ADD COLUMN date_played REAL; \
            ALTER TABLE item_stats ADD COLUMN date_accessed REAL; \
            ALTER TABLE item_stats ADD COLUMN bookmark_time_ms REAL; \
            UPDATE item_store SET sync_id=-1234567890123456789 WHERE item_pid=1; \
            UPDATE item_stats SET play_count_user=10,play_count_recent=10, \
            date_played=750208100.121234,date_accessed=750153587.007244, \
            bookmark_time_ms=0.0 WHERE item_pid=1;";
        let before = compilation_state(0, prior_stats);
        let source = compilation_source("A", false);
        for change in [
            "UPDATE item_stats SET play_count_user=9 WHERE item_pid=1;",
            "UPDATE item_stats SET play_count_user=11 WHERE item_pid=1;",
            "UPDATE item_stats SET skip_count_user=1 WHERE item_pid=1;",
            "UPDATE item_stats SET user_rating=1 WHERE item_pid=1;",
            "UPDATE item_stats SET has_been_played=1 WHERE item_pid=1;",
            "UPDATE item_stats SET user_rating_is_derived=1 WHERE item_pid=1;",
            "UPDATE item_stats SET liked_state=1 WHERE item_pid=1;",
            "UPDATE item_stats SET date_played=750208101.121234 WHERE item_pid=1;",
            "UPDATE item_stats SET date_accessed=750153588.007244 WHERE item_pid=1;",
            "UPDATE item_stats SET bookmark_time_ms=1.0 WHERE item_pid=1;",
            "ALTER TABLE item_stats ADD COLUMN opaque_value;",
            "UPDATE item_stats SET play_count_recent=9 WHERE item_pid=1;",
            "UPDATE item_stats SET play_count_recent=11 WHERE item_pid=1;",
            "UPDATE item_stats SET skip_count_recent=1 WHERE item_pid=1;",
            "UPDATE item_extra SET title='Changed' WHERE item_pid=1;",
            "UPDATE item_extra SET location='LOST.m4a' WHERE item_pid=1;",
            "UPDATE item_state SET is_placeholder=1 WHERE item_pid=1;",
            "UPDATE item_store SET sync_id=123 WHERE item_pid=1;",
            "DELETE FROM item_stats WHERE item_pid=1; DELETE FROM item_state WHERE item_pid=1; DELETE FROM item_store WHERE item_pid=1; DELETE FROM item_extra WHERE item_pid=1; DELETE FROM item WHERE item_pid=1;",
        ] {
            let after = compilation_state(
                1,
                &format!(
                    "{prior_stats} \
                    UPDATE item_stats SET play_count_recent=0 WHERE item_pid=1; \
                    UPDATE item SET is_compilation=0 WHERE item_pid=2; \
                    UPDATE album SET all_compilations=0 WHERE album_pid=131; {change}"
                ),
            );
            assert!(
                verify_insert(&before, &after, &compilation_expected(&source, true)).is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn insert_capture_recent_play_and_skip_resets_are_explicit_and_other_stats_stay_exact() {
        let stats = "ALTER TABLE item_stats ADD COLUMN date_played REAL; \
            ALTER TABLE item_stats ADD COLUMN bookmark_time_ms; \
            UPDATE item_stats SET play_count_user=10,play_count_recent=10, \
            date_played=750208100.121234,bookmark_time_ms=1 WHERE item_pid=1; \
            UPDATE item_stats SET skip_count_user=5,skip_count_recent=3, \
            date_played=750208101.121234,bookmark_time_ms=2 WHERE item_pid=2;";
        let before = compilation_state(1, stats);
        let source = compilation_source("B", false);
        let reset = "UPDATE item_stats SET play_count_recent=0 WHERE item_pid=1; \
            UPDATE item_stats SET skip_count_recent=0 WHERE item_pid=2;";
        let after = compilation_state(2, &format!("{stats}{reset}"));
        let proof = verify_insert(&before, &after, &compilation_expected(&source, false)).unwrap();
        assert_eq!(proof.prior_tracks_preserved, 2);
        assert_eq!(proof.prior_recent_counter_resets.len(), 2);
        let play = proof
            .prior_recent_counter_resets
            .iter()
            .find(|r| r.play_count_recent_before == 10)
            .unwrap();
        assert_eq!(play.play_count_recent_after, 0);
        assert_eq!(play.skip_count_recent_before, play.skip_count_recent_after);
        let skip = proof
            .prior_recent_counter_resets
            .iter()
            .find(|r| r.skip_count_recent_before == 3)
            .unwrap();
        assert_eq!(skip.skip_count_recent_after, 0);
        assert_eq!(skip.play_count_recent_before, skip.play_count_recent_after);
        for change in [
            "UPDATE item_stats SET skip_count_user=4 WHERE item_pid=2;",
            "UPDATE item_stats SET skip_count_user=6 WHERE item_pid=2;",
            "UPDATE item_stats SET skip_count_recent=2 WHERE item_pid=2;",
            "UPDATE item_stats SET skip_count_recent=4 WHERE item_pid=2;",
            "UPDATE item_stats SET bookmark_time_ms=1.0 WHERE item_pid=1;",
            "UPDATE item_stats SET bookmark_time_ms=NULL WHERE item_pid=1;",
            "UPDATE item_stats SET date_played=750208101.121234 WHERE item_pid=1;",
        ] {
            let wrong = compilation_state(2, &format!("{stats}{reset}{change}"));
            assert!(
                verify_insert(&before, &wrong, &compilation_expected(&source, false)).is_err(),
                "{change}"
            );
        }
        let unchanged = compilation_state(2, stats);
        assert!(
            verify_insert(&before, &unchanged, &compilation_expected(&source, false))
                .unwrap()
                .prior_recent_counter_resets
                .is_empty()
        );
        assert!(
            verify_insert_legacy_composer_sort(
                &before,
                &after,
                &compilation_expected(&source, false)
            )
            .is_err()
        );
    }

    #[test]
    fn listening_rule_decides_prior_playback_and_every_other_field_stays_exact() {
        let stats = "ALTER TABLE item_stats ADD COLUMN date_played REAL; \
            ALTER TABLE item_stats ADD COLUMN date_accessed REAL; \
            UPDATE item_stats SET play_count_user=10,play_count_recent=1, \
            date_played=750208100.121234 WHERE item_pid=1;";
        let before = compilation_state(0, stats);
        let source = compilation_source("A", false);
        let expected = compilation_expected(&source, true);
        let insert = "UPDATE item SET is_compilation=0 WHERE item_pid=2; \
            UPDATE album SET all_compilations=0 WHERE album_pid=131;";
        // Two later plays, counted from zero after the write cleared the
        // recent counter.
        let played = "UPDATE item_stats SET play_count_user=12,play_count_recent=2, \
            date_played=750282000.5 WHERE item_pid=1;";
        let after = compilation_state(1, &format!("{stats}{insert}{played}"));
        assert!(verify_insert(&before, &after, &expected).is_err());
        let grows = |a: &PlaybackStatistics, b: &PlaybackStatistics| -> Result<()> {
            ensure!(
                b.play_count_user >= a.play_count_user && b.user_rating == a.user_rating,
                "not listening"
            );
            Ok(())
        };
        let proof = verify_insert_after_listening(&before, &after, &expected, &grows).unwrap();
        assert_eq!(
            proof.prior_tracks_listened_between_runs,
            vec![format!("{:016x}", before.tracks()[0].sync_id)]
        );
        assert!(proof.prior_recent_counter_resets.is_empty());
        let unchanged = compilation_state(1, &format!("{stats}{insert}"));
        let proof = verify_insert_after_listening(&before, &unchanged, &expected, &grows).unwrap();
        assert!(proof.prior_tracks_listened_between_runs.is_empty());
        for change in [
            "UPDATE item_stats SET user_rating=1 WHERE item_pid=1;",
            "UPDATE item_stats SET play_count_user=9 WHERE item_pid=1;",
            "UPDATE item_extra SET title='Changed' WHERE item_pid=1;",
            "UPDATE item_extra SET location='LOST.m4a' WHERE item_pid=1;",
            // A statistic listening never writes, and a play dated earlier.
            "UPDATE item_stats SET date_accessed=750282000.5 WHERE item_pid=1;",
            "UPDATE item_stats SET date_played=750208000.0 WHERE item_pid=1;",
        ] {
            let wrong = compilation_state(1, &format!("{stats}{insert}{played}{change}"));
            assert!(
                verify_insert_after_listening(&before, &wrong, &expected, &grows).is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn a_later_run_accepts_a_batch_whose_prior_tracks_were_played_since() {
        let stats = "ALTER TABLE item_stats ADD COLUMN date_played REAL; \
            ALTER TABLE item_stats ADD COLUMN date_accessed REAL; \
            UPDATE item_stats SET play_count_user=10,play_count_recent=1, \
            date_played=750208100.121234 WHERE item_pid=1;";
        let before = compilation_state(0, stats);
        let source = compilation_source("A", false);
        let expected = [compilation_expected(&source, true)];
        let insert = "UPDATE item SET is_compilation=0 WHERE item_pid=2; \
            UPDATE album SET all_compilations=0 WHERE album_pid=131;";
        let played = "UPDATE item_stats SET play_count_user=12,play_count_recent=2, \
            date_played=750282000.5 WHERE item_pid=1;";
        let none = BTreeSet::new();
        let covers = BTreeMap::new();
        let grows = |a: &PlaybackStatistics, b: &PlaybackStatistics| -> Result<()> {
            ensure!(b.play_count_user >= a.play_count_user, "not listening");
            Ok(())
        };
        let later = |after: &Catalog| {
            verify_insert_batch_after_listening(
                &before, after, &expected, &none, &none, &covers, &grows,
            )
        };
        let after = compilation_state(1, &format!("{stats}{insert}{played}"));
        // The session's own check stays exact.
        assert!(
            verify_insert_batch_covered(&before, &after, &expected, &none, &none, &covers).is_err()
        );
        let proof = later(&after).unwrap();
        assert_eq!(
            proof.prior_tracks_listened_between_runs,
            vec![format!("{:016x}", before.tracks()[0].sync_id)]
        );
        let unchanged = compilation_state(1, &format!("{stats}{insert}"));
        let proof = later(&unchanged).unwrap();
        assert!(proof.prior_tracks_listened_between_runs.is_empty());
        // The session's own reset is reported as one, not as listening.
        let reset = "UPDATE item_stats SET play_count_recent=0 WHERE item_pid=1;";
        let proof = later(&compilation_state(1, &format!("{stats}{insert}{reset}"))).unwrap();
        assert!(proof.prior_tracks_listened_between_runs.is_empty());
        assert_eq!(proof.prior_recent_counter_resets.len(), 1);
        for change in [
            "UPDATE item_stats SET play_count_user=9 WHERE item_pid=1;",
            "UPDATE item_extra SET title='Changed' WHERE item_pid=1;",
            "UPDATE item_stats SET date_accessed=750282000.5 WHERE item_pid=1;",
            "UPDATE item_stats SET date_played=750208000.0 WHERE item_pid=1;",
        ] {
            let wrong = compilation_state(1, &format!("{stats}{insert}{played}{change}"));
            assert!(later(&wrong).is_err(), "{change}");
        }
        // A date that moved without a play is not listening either.
        let moved = "UPDATE item_stats SET date_played=750282000.5 WHERE item_pid=1;";
        let wrong = compilation_state(1, &format!("{stats}{insert}{moved}"));
        assert!(later(&wrong).is_err());
    }

    #[test]
    fn unchanged_catalog_cannot_complete_an_insertion() {
        let catalog = crate::test_support::catalogue::collection_catalog(13);
        let metadata = crate::media::metadata::read(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/silence-alac.m4a"),
        )
        .unwrap();
        let expected = ExpectedInsert {
            track_id: 42,
            album_id: 43,
            artist_id: 44,
            path: "iTunes_Control/Music/F00/TEST.m4a",
            file_bytes: 100,
            metadata: (&metadata).into(),
            presented: false,
        };
        assert!(verify_insert(&catalog, &catalog, &expected).is_err());
    }

    /// The phone may finish an earlier session's evaluation of an album in a
    /// later one: keep_local_status 0 to 2, its revision moved, its stats
    /// revision row gone (seen in a metadata edit). The catalogue the
    /// insert and cover verifiers compare holds none of those cells, so an
    /// insert batch, a single insert or a cover accepts such a session.
    #[test]
    fn an_album_evaluation_leaves_the_compared_catalogue_unchanged() {
        use rusqlite::Connection;
        use std::os::unix::fs::PermissionsExt;
        let fixture = crate::test_support::catalogue::NativeFixture::new();
        std::fs::set_permissions(&fixture.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Connection::open(&fixture.before)
            .unwrap()
            .execute_batch(
                "UPDATE album SET keep_local_status=0 WHERE album_pid=10;
                INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,10,0,4,0),(6,10,0,4,1);",
            )
            .unwrap();
        let evaluated = fixture.dir.join("evaluated.sqlite");
        std::fs::copy(&fixture.before, &evaluated).unwrap();
        Connection::open(&evaluated)
            .unwrap()
            .execute_batch(
                "UPDATE album SET keep_local_status=2 WHERE album_pid=10;
                UPDATE entity_revision SET revision=11 WHERE entity_pid=10 AND class=4 AND revision_type=0;
                DELETE FROM entity_revision WHERE entity_pid=10 AND class=4 AND revision_type=1;",
            )
            .unwrap();
        let read = |path: &std::path::Path| {
            crate::catalogue::sqlite::read_snapshot(path, None, &fixture.dir).unwrap()
        };
        let (before, after) = (read(&fixture.before), read(&evaluated));
        assert_eq!(before.tracks(), after.tracks());
        assert!(before.albums() == after.albums());
        assert!(before.artists() == after.artists());
        assert_eq!(
            before.persisted_media_anchor(),
            after.persisted_media_anchor()
        );
        assert_eq!(
            before.artwork().album_cache_ids(),
            after.artwork().album_cache_ids()
        );
    }
}
