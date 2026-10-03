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
