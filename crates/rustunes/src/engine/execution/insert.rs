//! Insert operations, prepared offline before any device write: a single ALAC
//! insert, and the ALAC and MP3 members of an insert batch.
//!
//! The ALAC field policy follows the captured native 44.1 kHz / 16-bit stereo
//! ALAC inserts. The catalog update repeats only the existing database and
//! primary-container identities from the copied catalogue. No playlist,
//! existing-track, artwork-cache or deletion operation is emitted. Caller must
//! separately prove current library ownership and anchor against the live
//! handshake.

use std::{collections::BTreeSet, time::SystemTime};

use anyhow::{Context, Result, ensure};
#[cfg(test)]
use plist::Value;

use crate::engine::planning::source_collection::mp3::preparation::PreparedSource;
use music_protocol::media::metadata::Metadata;

/// The insert policy: reuse the exact existing composer label AND its independent
/// stored sort label, and send the presentation (sort names and comment) of
/// `music_protocol::catalogue::presentation`. This changes only wire metadata;
/// source files and embedded tags stay intact.
pub fn prepare_live(
    source: &Metadata,
    plan: &crate::engine::planning::collections::PlannedTrack,
    catalog: &music_protocol::catalogue::sqlite::Catalog,
    imported_at: SystemTime,
) -> Result<Vec<u8>> {
    plan.check(source, catalog)?;
    ensure!(
        plan.pending_collection_ids().is_empty(),
        "single insert depends on earlier planned collections"
    );
    prepare_planned(source, plan, catalog, imported_at)
}

/// The same operation for one member of an insert batch. Collections an
/// earlier member of the same batch creates are allowed: the phone creates
/// each once, in operation order, and later members join it.
pub(crate) fn prepare_batch_member(
    source: &Metadata,
    plan: &crate::engine::planning::collections::PlannedTrack,
    catalog: &music_protocol::catalogue::sqlite::Catalog,
    imported_at: SystemTime,
    earlier: &BTreeSet<u64>,
) -> Result<Vec<u8>> {
    plan.check(source, catalog)?;
    ensure!(
        plan.pending_collection_ids().is_subset(earlier),
        "batch member depends on collections planned outside its batch"
    );
    prepare_planned(source, plan, catalog, imported_at)
}

/// The insert of one MP3 member of an insert batch, under the same collection
/// rule. Its fields come from the MPEG encoder; the batch joins the two codecs'
/// inserts as they are.
pub(crate) fn prepare_batch_mpeg_member(
    source: &PreparedSource,
    plan: &crate::engine::planning::collections::PlannedTrack,
    catalog: &music_protocol::catalogue::sqlite::Catalog,
    imported_at: SystemTime,
    earlier: &BTreeSet<u64>,
) -> Result<Vec<u8>> {
    use music_protocol::protocol::mpeg::{Envelope, Ids};
    source.ensure_batch_insertion_allowed()?;
    plan.check_mpeg(source, catalog)?;
    ensure!(
        plan.pending_collection_ids().is_subset(earlier),
        "batch member depends on collections planned outside its batch"
    );
    source.operation_with_metadata(
        Envelope {
            database_pid: catalog.database_pid(),
            primary_container_pid: catalog.primary_container_pid(),
            before_anchor: catalog.persisted_media_anchor(),
            timestamp: imported_at,
        },
        Ids {
            track: plan.track_pid(),
            album: plan.album_pid(),
            artist: plan.artist_pid(),
        },
        catalog,
    )
}

fn prepare_planned(
    source: &Metadata,
    plan: &crate::engine::planning::collections::PlannedTrack,
    catalog: &music_protocol::catalogue::sqlite::Catalog,
    imported_at: SystemTime,
) -> Result<Vec<u8>> {
    let scalar = source.projected()?;
    let existing = scalar
        .composer
        .map(|name| catalog.composer_for_label(name))
        .transpose()?
        .flatten();
    let composer = existing
        .map(|row| -> Result<(&str, &str)> {
            let sort = row
                .sort_name
                .as_deref()
                .context("existing composer has no qualified sort label")?;
            for text in [&row.name, sort] {
                ensure!(
                    !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control),
                    "invalid existing composer label or sort label"
                );
            }
            Ok((&row.name, sort))
        })
        .transpose()?;
    let presentation = music_protocol::catalogue::presentation::presentation(
        &scalar,
        catalog,
        plan.album_pid(),
        plan.artist_pid(),
    )?;
    prepare_operation_with_composer(
        source,
        &NewTrackIds {
            track: plan.track_pid(),
            album: plan.album_pid(),
            artist: plan.artist_pid(),
        },
        Catalog {
            database_pid: catalog.database_pid(),
            primary_container_pid: catalog.primary_container_pid(),
            media_anchor: catalog.persisted_media_anchor(),
        },
        imported_at,
        composer,
        &presentation,
    )
}

/// The new track's identity and the album and artist identities it names.
pub struct NewTrackIds {
    track: u64,
    album: u64,
    artist: u64,
}

use music_protocol::protocol::alac::Catalog;

fn prepare_operation_with_composer(
    source: &Metadata,
    ids: &NewTrackIds,
    catalog: Catalog,
    imported_at: SystemTime,
    existing_composer: Option<(&str, &str)>,
    presentation: &music_protocol::catalogue::presentation::Presentation<'_>,
) -> Result<Vec<u8>> {
    music_protocol::protocol::alac::serialize(
        source,
        &music_protocol::protocol::alac::Ids {
            track: ids.track,
            album: ids.album,
            artist: ids.artist,
        },
        catalog,
        imported_at,
        existing_composer,
        Some(presentation),
    )
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

    fn catalog(anchor: u32) -> Catalog {
        Catalog {
            database_pid: 5555,
            primary_container_pid: 6666,
            media_anchor: anchor,
        }
    }

    /// The serializer as `prepare_live` calls it, with no existing composer and
    /// the presentation planned against a library without these collections.
    fn prepare_operation(
        source: &Metadata,
        ids: &NewTrackIds,
        catalog: Catalog,
        imported_at: SystemTime,
    ) -> Result<Vec<u8>> {
        let scalar = source.projected()?;
        let library = music_protocol::test_support::catalogue::collection_catalog(13);
        let presentation = music_protocol::catalogue::presentation::presentation(
            &scalar, &library, ids.album, ids.artist,
        )?;
        prepare_operation_with_composer(source, ids, catalog, imported_at, None, &presentation)
    }

    #[test]
    fn prepare_live_preserves_existing_label_and_independent_sort() {
        let mut source = source();
        source.composer = vec!["Acme".into()];
        let before = music_protocol::test_support::catalogue::composer_transition_catalog(
            "ACME",
            Some("Independent sort"),
            100,
            false,
        );
        let mut planner =
            crate::engine::planning::collections::Planner::new(&before, &BTreeSet::new());
        let planned = planner
            .plan(
                &source,
                crate::engine::planning::collections::CandidateIds {
                    track: 2000,
                    album: Some(3000),
                    artist: Some(4000),
                },
            )
            .unwrap();
        let new = prepare_live(&source, &planned, &before, SystemTime::UNIX_EPOCH).unwrap();
        let new: Value = plist::from_bytes(&new).unwrap();
        let item = new.as_dictionary().unwrap()["operations"]
            .as_array()
            .unwrap()[1]
            .as_dictionary()
            .unwrap()["item"]
            .as_dictionary()
            .unwrap();
        assert_eq!(item["composer"].as_string(), Some("ACME"));
        assert_eq!(item["sort_composer"].as_string(), Some("Independent sort"));
        assert_eq!(source.composer, ["Acme"]);
        let missing = music_protocol::test_support::catalogue::composer_transition_catalog(
            "ACME", None, 100, false,
        );
        let mut planner =
            crate::engine::planning::collections::Planner::new(&missing, &BTreeSet::new());
        let planned = planner
            .plan(
                &source,
                crate::engine::planning::collections::CandidateIds {
                    track: 2000,
                    album: Some(3000),
                    artist: Some(4000),
                },
            )
            .unwrap();
        assert!(prepare_live(&source, &planned, &missing, SystemTime::UNIX_EPOCH).is_err());
    }

    #[test]
    fn prepare_live_sends_sort_names_without_a_leading_article_and_the_comment() {
        let mut source = source();
        source.title = vec!["The Example Title".into()];
        source.album = vec!["An Album".into()];
        source.album_artist = vec!["The Album Artist".into()];
        source.comment = vec!["Processed by SoX".into()];
        source.sort_artist = vec!["Artist, Sorted".into()];
        let before = music_protocol::test_support::catalogue::collection_catalog(13);
        let mut planner =
            crate::engine::planning::collections::Planner::new(&before, &BTreeSet::new());
        let planned = planner
            .plan(
                &source,
                crate::engine::planning::collections::CandidateIds {
                    track: 2000,
                    album: Some(3000),
                    artist: Some(4000),
                },
            )
            .unwrap();
        let item = |bytes: &[u8]| {
            let value: Value = plist::from_bytes(bytes).unwrap();
            value.as_dictionary().unwrap()["operations"]
                .as_array()
                .unwrap()[1]
                .as_dictionary()
                .unwrap()["item"]
                .as_dictionary()
                .unwrap()
                .clone()
        };
        let new = item(&prepare_live(&source, &planned, &before, SystemTime::UNIX_EPOCH).unwrap());
        let text = |key: &str| new.get(key).and_then(Value::as_string);
        assert_eq!(
            [
                "sort_name",
                "sort_album",
                "sort_artist",
                "sort_album_artist",
                "comment"
            ]
            .map(text),
            [
                Some("Example Title"),
                Some("Album"),
                Some("Artist, Sorted"),
                Some("Album Artist"),
                Some("Processed by SoX")
            ]
        );
    }

    #[test]
    fn isolated_insert_preserves_pid_bits_types_timing_and_source_text() {
        let mut source = source();
        source.title = vec!["Track W".into()];
        source.artist = vec!["Artist A, Artist B".into()];
        source.timing.duration_ticks = 6_482_890;
        let ids = NewTrackIds {
            track: u64::MAX,
            album: 2000,
            artist: 3000,
        };
        let bytes = prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).unwrap();
        assert!(bytes.starts_with(b"bplist00"));
        let root: Value = plist::from_bytes(&bytes).unwrap();
        let root = root.as_dictionary().unwrap();
        assert_eq!(root["revision"].as_unsigned_integer(), Some(11));
        let ops = root["operations"].as_array().unwrap();
        assert_eq!(ops.len(), 2);
        let db = ops[0].as_dictionary().unwrap();
        assert_eq!(db["operation"].as_string(), Some("update_db_info"));
        assert_eq!(db["pid"].as_unsigned_integer(), Some(5555));
        assert_eq!(
            db["db_info"].as_dictionary().unwrap()["primary_container_pid"].as_unsigned_integer(),
            Some(6666)
        );
        let op = ops[1].as_dictionary().unwrap();
        assert_eq!(op["operation"].as_string(), Some("insert_track"));
        assert_eq!(op["pid"].as_signed_integer(), Some(-1));
        let item = op["item"].as_dictionary().unwrap();
        assert_eq!(item["artist"].as_string(), Some("Artist A, Artist B"));
        assert_eq!(item["total_time_ms"].as_unsigned_integer(), Some(147004));
        assert_eq!(item["artwork_checksum"].as_unsigned_integer(), Some(0));
        assert!(!item.contains_key("artwork_cache_id"));
        assert!(!item.contains_key("album_artist"));
        let audio = op["avformat_info"].as_dictionary().unwrap();
        assert_eq!(audio["sample_rate"].as_real(), Some(44100.0));
        assert_eq!(audio["duration"].as_unsigned_integer(), Some(6_482_890));
        assert!(!audio.contains_key("volume_normalization_energy"));
        assert_eq!(op["location"].as_dictionary().unwrap().len(), 1);
    }

    #[test]
    fn rejects_revision_overflow_before_serialization() {
        let ids = NewTrackIds {
            track: 2001,
            album: 3000,
            artist: 4000,
        };
        assert!(
            prepare_operation(&source(), &ids, catalog(u32::MAX), SystemTime::UNIX_EPOCH).is_err()
        );
    }

    #[test]
    fn first_value_projection_preserves_source_and_rejects_unsupported_audio() {
        let ids = NewTrackIds {
            track: 2000,
            album: 3000,
            artist: 4000,
        };
        let mut source = source();
        source.artist.push("Second artist".into());
        let bytes = prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).unwrap();
        let value: Value = plist::from_bytes(&bytes).unwrap();
        assert_eq!(
            value.as_dictionary().unwrap()["operations"]
                .as_array()
                .unwrap()[1]
                .as_dictionary()
                .unwrap()["item"]
                .as_dictionary()
                .unwrap()["artist"]
                .as_string(),
            Some("First artist")
        );
        assert_eq!(source.artist.len(), 2);
        source.artist.pop();
        // A 96 kHz 24-bit source sends its own PCM rate; six channels refuse.
        source.sample_rate = 96000;
        source.bit_depth = 24;
        let bytes = prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).unwrap();
        let value: Value = plist::from_bytes(&bytes).unwrap();
        let audio = value.as_dictionary().unwrap()["operations"]
            .as_array()
            .unwrap()[1]
            .as_dictionary()
            .unwrap()["avformat_info"]
            .as_dictionary()
            .unwrap()
            .clone();
        assert_eq!(audio["bit_rate"].as_unsigned_integer(), Some(4608));
        assert_eq!(audio["sample_rate"].as_real(), Some(96000.0));
        source.channels = 6;
        assert!(prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).is_err());
    }

    #[test]
    fn compilation_wire_is_boolean_true_and_false_absence_are_byte_identical() {
        let ids = NewTrackIds {
            track: 2000,
            album: 3000,
            artist: 4000,
        };
        let mut source = source();
        source.album_artist = vec!["CLI Compilation Σύνολο".into()];
        let absent = prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).unwrap();
        source.compilation = Some(false);
        assert_eq!(
            absent,
            prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).unwrap()
        );
        source.compilation = Some(true);
        let compiled: Value = plist::from_bytes(
            &prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).unwrap(),
        )
        .unwrap();
        let mut expected: Value = plist::from_bytes(&absent).unwrap();
        let item = expected
            .as_dictionary_mut()
            .unwrap()
            .get_mut("operations")
            .unwrap()
            .as_array_mut()
            .unwrap()[1]
            .as_dictionary_mut()
            .unwrap()
            .get_mut("item")
            .unwrap()
            .as_dictionary_mut()
            .unwrap();
        assert!(!item.contains_key("is_compilation"));
        assert_eq!(
            item["album_artist"].as_string(),
            Some("CLI Compilation Σύνολο")
        );
        assert_eq!(item["sort_album_artist"], item["album_artist"]);
        item.insert("is_compilation".into(), Value::Boolean(true));
        assert_eq!(compiled, expected);
        assert_eq!(source.album_artist, ["CLI Compilation Σύνολο"]);
        source.album_artist.clear();
        assert!(prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).is_err());
        source.album_artist = vec!["One".into(), "Two".into()];
        assert!(prepare_operation(&source, &ids, catalog(10), SystemTime::UNIX_EPOCH).is_err());
    }
}
