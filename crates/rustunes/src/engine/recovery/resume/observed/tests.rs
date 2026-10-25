use super::*;
use crate::engine::state::journal::{Artwork, test_support};
use music_protocol::test_support::catalogue::{
    artwork_catalog, collection_catalog, modified_collection_catalog,
};

/// A Started insert of the fixture's one track (sync ID -1), prepared at
/// anchor 13.
fn started_insert() -> (test_support::Temp, Attempt) {
    let temp = test_support::Temp::new();
    let mut journal = Journal::create(&temp.path(), &test_support::identity()).unwrap();
    let mut content = test_support::alac_content(1);
    content.track_pid = u64::MAX;
    let id = test_support::insert_started(&mut journal, content);
    let attempt = journal.attempt(id).unwrap();
    (temp, attempt)
}

fn started_artwork(cache_id: u32) -> (test_support::Temp, Attempt) {
    let (temp, mut attempt) = started_insert();
    attempt.prepared.kind = AttemptKind::Artwork;
    attempt.prepared.content.artwork = Some(Artwork {
        cache_id,
        bytes: 100,
        sha256: [5; 32],
    });
    (temp, attempt)
}

const LOCATIONLESS: &str = "UPDATE item SET base_location_id=0 WHERE item_pid=1;
    UPDATE item_extra SET location='' WHERE item_pid=1;
    UPDATE item_state SET has_non_purgeable_asset=0 WHERE item_pid=1;";

const NO_TRACK: &str = "DELETE FROM item; DELETE FROM item_extra; DELETE FROM item_store;
    DELETE FROM item_state; DELETE FROM item_stats;";

#[test]
fn an_unmoved_anchor_without_the_insert_is_unmoved() {
    let (_temp, attempt) = started_insert();
    let fresh = modified_collection_catalog(13, NO_TRACK);
    assert_eq!(classify(&attempt, &fresh, &fresh).unwrap(), Seen::Unmoved);
    // A Before at another revision proves nothing about the tracks.
    assert_eq!(
        classify(&attempt, &fresh, &collection_catalog(12)).unwrap(),
        Seen::Unmoved
    );
}

#[test]
fn an_unmoved_anchor_with_the_insert_or_changed_tracks_stays_started() {
    let (_temp, attempt) = started_insert();
    let present = collection_catalog(13);
    let error = classify(&attempt, &present, &present)
        .unwrap_err()
        .to_string();
    assert!(error.contains("stays Started"), "{error}");
    let locationless = modified_collection_catalog(13, LOCATIONLESS);
    assert!(classify(&attempt, &locationless, &locationless).is_err());
    let fresh = modified_collection_catalog(13, NO_TRACK);
    let before = modified_collection_catalog(
        13,
        &format!("{NO_TRACK} INSERT INTO item VALUES(2,8,31,21,41,61,71,51,0,6,1,0,100);
            INSERT INTO item_extra VALUES(2,'Other','OTHR.m4a',1.0,1,10,0,2026,NULL,0,0,'Other',NULL);
            INSERT INTO item_store VALUES(2,-2); INSERT INTO item_state VALUES(2,0,1,1,1);
            INSERT INTO item_stats VALUES(2,0,0,0,0,0,0,0,0);"),
    );
    let error = classify(&attempt, &fresh, &before).unwrap_err().to_string();
    assert!(error.contains("tracks changed"), "{error}");
}

#[test]
fn a_moved_anchor_compensates_only_an_insert_row_without_location() {
    let (_temp, attempt) = started_insert();
    let before = modified_collection_catalog(13, NO_TRACK);
    let locationless = modified_collection_catalog(14, LOCATIONLESS);
    assert_eq!(
        classify(&attempt, &locationless, &before).unwrap(),
        Seen::Locationless
    );
    for fresh in [
        collection_catalog(14),
        modified_collection_catalog(14, NO_TRACK),
        modified_collection_catalog(15, ""),
    ] {
        assert_eq!(classify(&attempt, &fresh, &before).unwrap(), Seen::Moved);
    }
    let (_temp, artwork) = started_artwork(300);
    assert_eq!(
        classify(&artwork, &locationless, &before).unwrap(),
        Seen::Moved
    );
}

#[test]
fn an_unmoved_cover_needs_its_track_and_no_trace_of_the_cache() {
    let (_temp, attempt) = started_artwork(300);
    // artwork_catalog registers its track and album caches at anchor 12.
    let mut attempt12 = started_artwork(300).1;
    attempt12.prepared.current_media_anchor = 12;
    let other = artwork_catalog(150, 151);
    assert_eq!(classify(&attempt12, &other, &other).unwrap(), Seen::Unmoved);
    for registered in [artwork_catalog(300, 151), artwork_catalog(150, 300)] {
        let error = classify(&attempt12, &registered, &registered)
            .unwrap_err()
            .to_string();
        assert!(error.contains("holds cover 300"), "{error}");
    }
    let without = modified_collection_catalog(13, NO_TRACK);
    assert!(classify(&attempt, &without, &without).is_err());
}

#[test]
fn a_cover_taken_one_anchor_on_without_its_image_is_imageless() {
    let (_temp, mut attempt) = started_artwork(300);
    let token = "INSERT INTO artwork_token VALUES(1,'300',300,1,0,0);";
    let before = collection_catalog(13);
    // A cover keeps its track where it is.
    attempt.prepared.content.path = before.tracks()[0].afc_path.clone().unwrap();
    let imageless = modified_collection_catalog(14, token);
    assert_eq!(
        classify(&attempt, &imageless, &before).unwrap(),
        Seen::Imageless
    );
    // Its image arrived, the anchor moved further, or the track kept its old
    // cover: the attempt's own route decides.
    for fresh in [
        modified_collection_catalog(
            14,
            &format!(
                "{token} INSERT INTO artwork VALUES('300',300,1,0,'aa/00000000000000000000000000000000000000');"
            ),
        ),
        modified_collection_catalog(15, token),
        collection_catalog(14),
    ] {
        assert_eq!(classify(&attempt, &fresh, &before).unwrap(), Seen::Moved);
    }
    let (_temp, insert) = started_insert();
    assert_eq!(classify(&insert, &imageless, &before).unwrap(), Seen::Moved);
}
