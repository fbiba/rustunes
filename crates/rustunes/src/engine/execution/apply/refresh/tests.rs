use super::*;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "insert-refresh-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn control() -> Control {
    Control {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
#[test]
fn refresh_requires_exact_projected_catalogue_and_artwork_despite_new_raw_hashes() {
    let before = music_protocol::test_support::catalogue::collection_catalog(89);
    let fresh = music_protocol::test_support::catalogue::modified_collection_catalog(
        89,
        "INSERT INTO _MLDatabaseProperties VALUES('MLArtistHeroImageImportDatabaseRevision','83559');",
    );
    assert_ne!(
        before.source_hashes().database,
        fresh.source_hashes().database
    );
    ensure_catalog(&before, &fresh).unwrap();
    for sql in [
        "UPDATE item_extra SET title='foreign change'",
        "UPDATE item_extra SET sort_title=NULL",
        "UPDATE item_extra SET location='DIFF.m4a'",
        "UPDATE item_stats SET play_count_recent=1",
        "UPDATE album SET album='changed'",
        "UPDATE composer SET sort_composer='different'",
        "INSERT INTO artwork VALUES('123',300,6,0,'aa/00000000000000000000000000000000000000')",
    ] {
        let changed = music_protocol::test_support::catalogue::modified_collection_catalog(89, sql);
        assert!(ensure_catalog(&before, &changed).is_err(), "{sql}");
    }
    assert!(
        ensure_catalog(
            &before,
            &music_protocol::test_support::catalogue::collection_catalog(90)
        )
        .is_err()
    );
}
#[test]
fn refresh_reselects_all_sources_and_refuses_added_missing_edited_or_excluded_entries() {
    let temp = Temp::new();
    let path = temp.0.join("source.m4a");
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/silence-alac.m4a"
    ));
    fs::write(&path, bytes).unwrap();
    let options = source_collection::SelectionOptions::default();
    let frozen =
        source_collection::inspect(&source_collection::select(&temp.0, &options).unwrap()).unwrap();
    ensure_sources(&frozen, &control()).unwrap();
    fs::write(temp.0.join("new.m4a"), bytes).unwrap();
    assert!(ensure_sources(&frozen, &control()).is_err());
    fs::remove_file(temp.0.join("new.m4a")).unwrap();
    fs::write(temp.0.join("corrupt.m4a"), b"invalid selected data").unwrap();
    assert!(ensure_sources(&frozen, &control()).is_err());
    fs::remove_file(temp.0.join("corrupt.m4a")).unwrap();
    fs::rename(&path, temp.0.join("renamed.m4a")).unwrap();
    assert!(ensure_sources(&frozen, &control()).is_err());
    fs::rename(temp.0.join("renamed.m4a"), &path).unwrap();
    fs::write(temp.0.join("new-excluded.txt"), b"new exclusion").unwrap();
    assert!(ensure_sources(&frozen, &control()).is_err());
    fs::remove_file(temp.0.join("new-excluded.txt")).unwrap();
    let mut changed = bytes.to_vec();
    changed[20] ^= 1;
    fs::write(&path, changed).unwrap();
    assert!(ensure_sources(&frozen, &control()).is_err());
}
#[test]
fn before_copy_is_bounded_hash_checked_nonreplacing_and_symlink_safe() {
    let temp = Temp::new();
    let source = temp.0.join("before");
    fs::write(&source, b"bound capture").unwrap();
    let h = Sha256::digest(b"bound capture").into();
    let copy = temp.0.join("copy");
    copy_file(&source, &copy, h, 13, &control()).unwrap();
    assert_eq!(fs::read(&source).unwrap(), fs::read(&copy).unwrap());
    assert!(copy_file(&source, &copy, h, 13, &control()).is_err());
    assert!(copy_file(&source, &temp.0.join("wrong"), [0; 32], 13, &control()).is_err());
    let link = temp.0.join("link");
    std::os::unix::fs::symlink(&source, &link).unwrap();
    assert!(copy_file(&link, &temp.0.join("linked"), h, 13, &control()).is_err());
}
#[tokio::test]
async fn refresh_capture_shares_deadline_and_cancellation_without_polling_after_expiry() {
    let mut c = control();
    c.deadline = Instant::now();
    assert!(
        controlled(
            async {
                panic!("expired capture polled");
                #[allow(unreachable_code)]
                Ok(())
            },
            &c
        )
        .await
        .is_err()
    );
    let c = control();
    c.cancelled.store(true, Ordering::Relaxed);
    assert!(
        controlled(
            async {
                panic!("cancelled capture polled");
                #[allow(unreachable_code)]
                Ok(())
            },
            &c
        )
        .await
        .is_err()
    );
    let c = control();
    let cancel = Arc::clone(&c.cancelled);
    let task = async move {
        cancel.store(true, Ordering::Relaxed);
        std::future::pending::<Result<()>>().await
    };
    assert!(controlled(task, &c).await.is_err());
}

#[test]
fn only_one_typed_pre_session_refresh_and_no_error_text_fallback() {
    let c = control();
    let e = anyhow::Error::from(PreSessionStateDrift::fixture())
        .context("ordinary pre-session attempt");
    assert!(may_refresh(&e, 1, &c).unwrap());
    assert!(!may_refresh(&e, 2, &c).unwrap());
    for text in [
        "unexpected file size",
        "read-only pre-session drift",
        "Ready state changed",
        "Started transfer failed",
    ] {
        assert!(!may_refresh(&anyhow::anyhow!(text), 1, &c).unwrap());
    }
    c.cancelled.store(true, Ordering::Relaxed);
    assert!(may_refresh(&e, 1, &c).is_err());
}

fn current() -> Current {
    let mut prefs = vec![0; 1232];
    prefs[..4].copy_from_slice(b"frpd");
    prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
    prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
    prefs[8] = 1;
    prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
    prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
    let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
    Current {
        device: "fixture".into(),
        identity: crate::engine::state::journal::Identity::new("fixture", &library).unwrap(),
        library,
        catalog: music_protocol::test_support::catalogue::collection_catalog(89),
        raw_preferences: prefs.clone(),
        plist_preferences: prefs,
        cdb: vec![1, 2, 3],
        artwork_db: None,
        receipt_sha256: [7; 32],
    }
}
#[test]
fn refresh_does_not_replace_library_prefs_cdb_or_artwork_identity() {
    let before = current();
    ensure_current(&before, &current()).unwrap();
    for change in 0..6 {
        let mut fresh = current();
        match change {
            0 => fresh.device = "other".into(),
            1 => fresh.raw_preferences[20] ^= 1,
            2 => fresh.plist_preferences[20] ^= 1,
            3 => fresh.cdb.push(4),
            4 => fresh.artwork_db = Some(vec![1]),
            _ => {
                fresh.identity =
                    crate::engine::state::journal::Identity::new("other", &fresh.library).unwrap()
            }
        }
        assert!(ensure_current(&before, &fresh).is_err());
    }
}

#[test]
fn refreshed_planner_cannot_substitute_original_ids_path_content_or_operation() {
    use crate::{
        engine::planning::collections::{CandidateIds, Planner},
        engine::state::journal::{AttemptKind, Content, PreparedAttempt},
    };
    use std::collections::BTreeSet;
    let path = crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a");
    let make_source = || {
        let mut metadata = music_protocol::media::metadata::read(&path).unwrap();
        metadata.title = vec!["New fixture".into()];
        metadata.artist = vec!["New artist".into()];
        metadata.album = vec!["New album".into()];
        metadata.album_artist = vec!["New artist".into()];
        crate::engine::planning::source_collection::SourceTrack {
            relative_path: "source.m4a".into(),
            fingerprint: music_protocol::media::fingerprint::read(&path).unwrap(),
            metadata,
        }
    };
    let source = make_source();
    let before = music_protocol::test_support::catalogue::collection_catalog(89);
    let fresh = music_protocol::test_support::catalogue::modified_collection_catalog(
        89,
        "INSERT INTO _MLDatabaseProperties VALUES('MLArtistHeroImageImportDatabaseRevision','83559');",
    );
    let selected = Planner::new(&before, &BTreeSet::new())
        .plan(
            &source.metadata,
            CandidateIds {
                track: 10000,
                album: Some(10001),
                artist: Some(10002),
            },
        )
        .unwrap();
    let time = std::time::UNIX_EPOCH + Duration::from_secs(123456789);
    let operation =
        crate::engine::execution::insert::prepare_live(&source.metadata, &selected, &before, time)
            .unwrap();
    let make_intent = || PreparedAttempt {
        kind: AttemptKind::Insert,
        content: Content {
            source: (source.fingerprint.clone()).into(),
            track_pid: 10000,
            path: "iTunes_Control/Music/F39/FIXT.m4a".into(),
            artwork: None,
            collection: Some([4; 32]),
        },
        new_album_pid: Some(10001),
        new_artist_pid: Some(10002),
        current_media_anchor: 89,
        operation: operation.clone(),
        input_receipt_sha256: [7; 32],
    };
    let frozen = FrozenIntent::new(&make_intent(), &selected, time);
    let mut normal = make_intent();
    normal.input_receipt_sha256 = [8; 32];
    assert_eq!(
        frozen
            .revalidate(&normal, &source, &fresh, &BTreeSet::new(), &control())
            .unwrap(),
        operation
    );
    for change in 0..6 {
        let mut changed = make_intent();
        match change {
            0 => changed.content.track_pid += 1,
            1 => changed.content.path = "iTunes_Control/Music/F39/DIFF.m4a".into(),
            2 => changed.operation[20] ^= 1,
            3 => changed.new_album_pid = Some(10003),
            4 => changed.content.collection = Some([5; 32]),
            _ => changed.current_media_anchor += 1,
        }
        assert!(
            frozen
                .revalidate(&changed, &source, &fresh, &BTreeSet::new(), &control())
                .is_err()
        );
    }
    let mut edited = make_source();
    edited.metadata.title = vec!["changed title".into()];
    assert!(
        frozen
            .revalidate(&normal, &edited, &fresh, &BTreeSet::new(), &control())
            .is_err()
    );
    assert!(
        frozen
            .revalidate(
                &normal,
                &source,
                &fresh,
                &BTreeSet::from([10000]),
                &control()
            )
            .is_err()
    );
    let mut expired = control();
    expired.deadline = Instant::now();
    assert!(
        frozen
            .revalidate(&normal, &source, &fresh, &BTreeSet::new(), &expired)
            .is_err()
    );
}
