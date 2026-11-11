//! What `init` plans for a phone, retries included, and how it reads the
//! phone back after the session.
use std::cell::Cell;

use super::*;
use crate::engine::verification::readiness::tests::{
    fresh, fresh_library, set, with_prefs, with_prefs_only, with_temporary,
};

const HOST: &str = "rustunes";

#[test]
fn a_fresh_phone_gets_both_prefs_files_and_the_missing_folders() {
    let record = InitRecord::mint().unwrap();
    let work = work_on(&fresh(), &record, HOST, None).unwrap();
    let prefs = work.prefs.as_ref().unwrap();
    assert_eq!(prefs.raw, record.prefs(HOST).unwrap().raw);
    assert_eq!(
        work.write_prefs,
        [device_files::PREFS, device_files::PREFS_PLIST]
    );
    assert!(work.leftovers.is_empty());
    assert_eq!(
        work.create_folders,
        ["iTunes_Control/Sync", "iTunes_Control/Sync/Media"]
    );
}

#[test]
fn a_retry_after_the_first_prefs_file_writes_only_the_plist_that_matches_it() {
    let record = InitRecord::mint().unwrap();
    // The stopped attempt ran with another --host-name than this one.
    let written = record.prefs("Mac mini").unwrap();
    let inspection = with_temporary(
        with_prefs_only(fresh(), &record, "Mac mini"),
        device_files::PREFS_PLIST_TEMPORARY,
        Some(written.plist[..40].to_vec()),
    );
    let work = work_on(&inspection, &record, HOST, None).unwrap();
    let prefs = work.prefs.as_ref().unwrap();
    assert_eq!(prefs.raw, written.raw);
    assert_eq!(prefs.plist, written.plist);
    assert_eq!(work.write_prefs, [device_files::PREFS_PLIST]);
    assert_eq!(
        work.leftovers,
        [(device_files::PREFS_PLIST_TEMPORARY, written.plist.clone())]
    );
}

#[test]
fn a_retry_before_the_first_prefs_file_removes_its_temporary_and_writes_both() {
    let record = InitRecord::mint().unwrap();
    let earlier = record.prefs("Mac mini").unwrap();
    let inspection = with_temporary(
        fresh(),
        device_files::PREFS_TEMPORARY,
        Some(earlier.raw.clone()),
    );
    let work = work_on(&inspection, &record, HOST, None).unwrap();
    assert_eq!(
        work.prefs.as_ref().unwrap().raw,
        record.prefs(HOST).unwrap().raw
    );
    assert_eq!(work.write_prefs.len(), 2);
    assert_eq!(
        work.leftovers,
        [(device_files::PREFS_TEMPORARY, earlier.raw.clone())]
    );
}

#[test]
fn a_retry_after_staging_removes_the_saved_pair_and_writes_no_prefs() {
    let record = InitRecord::mint().unwrap();
    let mut inspection = with_prefs(fresh(), &record);
    set(&mut inspection, "iTunes_Control/Sync");
    set(&mut inspection, SYNC_MEDIA);
    inspection.staged = Some(vec!["Sync_00000000.plist".into()]);
    let saved = (b"<plist>operation</plist>".to_vec(), [3_u8; 21]);
    let work = work_on(&inspection, &record, HOST, Some(&saved)).unwrap();
    assert!(work.prefs.is_none());
    assert!(work.write_prefs.is_empty());
    assert!(work.create_folders.is_empty());
    assert_eq!(
        work.leftovers,
        [(SIGNATURE, vec![3; 21]), (METADATA, saved.0.clone())]
    );
    // Staged files with no saved pair to check them against: refused.
    let error = work_on(&inspection, &record, HOST, None).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("no earlier init attempt saved one")
    );
}

#[test]
fn work_differs_when_the_phone_does() {
    let record = InitRecord::mint().unwrap();
    let before = work_on(&fresh(), &record, HOST, None).unwrap();
    assert!(before.same(&work_on(&fresh(), &record, HOST, None).unwrap()));
    let mut folders = fresh();
    set(&mut folders, "iTunes_Control/Sync");
    assert!(!before.same(&work_on(&folders, &record, HOST, None).unwrap()));
    let earlier = record.prefs("Mac mini").unwrap();
    let temporary = with_temporary(fresh(), device_files::PREFS_TEMPORARY, Some(earlier.raw));
    assert!(!before.same(&work_on(&temporary, &record, HOST, None).unwrap()));
}

fn drift() -> anyhow::Error {
    catalogue_capture::CatalogCaptureDrift::fixture().into()
}

/// The library after the first session.
fn initialised() -> LibraryState {
    let mut library = fresh_library();
    library.media_anchor = Some("1".into());
    library
}

fn shows(library: &LibraryState) -> bool {
    library.media_anchor.as_deref() == Some("1")
}

/// A phone whose database changes under the first `changing` captures, then
/// holds `then`. Returns the attempt numbers seen and when each began.
async fn read_changing(
    seconds: u64,
    changing: usize,
    then: Option<LibraryState>,
) -> (Result<ReadBack>, Vec<(usize, Duration)>) {
    let start = Instant::now();
    let seen = Cell::new(Vec::new());
    let read = read_back(start + Duration::from_secs(seconds), shows, |number| {
        let mut so_far = seen.take();
        so_far.push((number, start.elapsed()));
        seen.set(so_far);
        let then = then.clone();
        async move {
            if number <= changing {
                Err(drift())
            } else {
                Ok(then)
            }
        }
    })
    .await;
    (read, seen.take())
}

#[tokio::test(start_paused = true)]
async fn the_read_back_waits_for_the_phone_to_stop_writing() {
    // As on the phone: changed under two captures back to back.
    for changing in [1, 2, 6] {
        let (read, seen) = read_changing(120, changing, Some(initialised())).await;
        assert!(matches!(read.unwrap(), ReadBack::Shown(_)), "{changing}");
        assert_eq!(seen.len(), changing + 1);
        // A pause between attempts, 0.25 s and then doubling.
        assert_eq!(seen[1].1, Duration::from_millis(250));
        if changing >= 2 {
            assert_eq!(seen[2].1, Duration::from_millis(750));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_read_back_waits_while_a_stable_capture_does_not_show_the_result() {
    let count = Cell::new(0);
    let read = read_back(Instant::now() + Duration::from_secs(120), shows, |_| {
        count.set(count.get() + 1);
        let library = if count.get() < 4 {
            fresh_library()
        } else {
            initialised()
        };
        async move { Ok(Some(library)) }
    })
    .await;
    assert!(matches!(read.unwrap(), ReadBack::Shown(_)));
    assert_eq!(count.get(), 4);
}

#[tokio::test(start_paused = true)]
async fn the_read_back_stops_at_its_bound_or_at_any_other_failure() {
    // Still changing: given up within --seconds, the last error kept.
    let (read, seen) = read_changing(5, usize::MAX, None).await;
    let error = read.err().unwrap();
    assert!(
        error
            .downcast_ref::<catalogue_capture::CatalogCaptureDrift>()
            .is_some()
    );
    assert!(format!("{error:#}").contains(&format!("after {} readings", seen.len())));
    assert!(seen.last().unwrap().1 < Duration::from_secs(5));
    assert_eq!(seen.len(), 5);
    // And within READ_BACK_ATTEMPTS whatever the bound.
    let (read, seen) = read_changing(1800, usize::MAX, None).await;
    assert!(read.is_err());
    assert_eq!(seen.len(), READ_BACK_ATTEMPTS);

    // Never showing the result: the last stable capture is reported.
    let (read, seen) = read_changing(5, 0, Some(fresh_library())).await;
    match read.unwrap() {
        ReadBack::NotShown { last, attempts } => {
            assert!(last.is_some_and(|last| last.media_anchor.is_none()));
            assert_eq!(attempts, seen.len());
        }
        ReadBack::Shown(_) => panic!("shown"),
    }
    let (read, _) = read_changing(5, 0, None).await;
    assert!(matches!(
        read.unwrap(),
        ReadBack::NotShown { last: None, .. }
    ));

    let count = Cell::new(0);
    let other = read_back(Instant::now() + Duration::from_secs(120), shows, |_| {
        count.set(count.get() + 1);
        async { Err::<Option<LibraryState>, _>(anyhow::anyhow!("AFC permission denied")) }
    })
    .await;
    assert!(other.is_err());
    assert_eq!(count.get(), 1);
}
