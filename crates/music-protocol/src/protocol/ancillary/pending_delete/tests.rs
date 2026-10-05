use super::super::tests::manifest;
use super::*;
use crate::test_support::catalogue::modified_collection_catalog;
use std::time::UNIX_EPOCH;

const DEST: &str = "iTunes_Control/Music/F39/OMJJ.m4a";
const PENDING: &str = "UPDATE item SET base_location_id=0;
    UPDATE item_extra SET location='',file_size=0;
    UPDATE item_state SET has_non_purgeable_asset=0;";
const FOREIGN: &str = "INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,51,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
    INSERT INTO item_extra SELECT 2,title,'FOREIGN.m4a',total_time_ms,12345,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1;
    INSERT INTO item_store VALUES(2,-2); INSERT INTO item_state VALUES(2,0,1,1,1);
    INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;
    INSERT INTO artwork VALUES('100',300,6,0,'aa/00000000000000000000000000000000000000');
    INSERT INTO artwork_token VALUES(2,'100',300,1,0,0);";
fn identity(a: u64, b: u64) -> LibraryIdentity {
    let mut raw = [0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4..6].copy_from_slice(&1u16.to_le_bytes());
    raw[6..8].copy_from_slice(&25u16.to_le_bytes());
    raw[8] = 1;
    raw[12..20].copy_from_slice(&a.to_le_bytes());
    raw[20..28].copy_from_slice(&b.to_le_bytes());
    crate::catalogue::identity::parse(&raw).unwrap()
}
fn fixture(sql: &str) -> (Catalog, LibraryIdentity, EligibleSnapshot, PendingDelete) {
    let catalog = modified_collection_catalog(140, &format!("{PENDING}{FOREIGN}{sql}"));
    let own = catalog
        .tracks()
        .iter()
        .find(|track| track.sync_id == u64::MAX)
        .unwrap();
    let deletion = PendingDelete::prepare(&catalog, own, DEST, UNIX_EPOCH).unwrap();
    let id = identity(1, 2);
    let snapshot = EligibleSnapshot::new(&catalog, &id);
    (catalog, id, snapshot, deletion)
}

/// Session-test fixture: the policy negotiates `library_id` at `anchor`.
/// Own row "-1" is locationless; foreign "-2" has a registered artwork token.
pub(crate) fn session_fixture(
    library_id: &str,
    anchor: u32,
) -> (Catalog, LibraryIdentity, EligibleSnapshot, PendingDelete) {
    let catalog = modified_collection_catalog(anchor, &format!("{PENDING}{FOREIGN}"));
    let own = catalog
        .tracks()
        .iter()
        .find(|track| track.sync_id == u64::MAX)
        .unwrap();
    let deletion = PendingDelete::prepare(&catalog, own, DEST, UNIX_EPOCH).unwrap();
    let id = identity(1, 2);
    let mut snapshot = EligibleSnapshot::new(&catalog, &id);
    snapshot.library_id = library_id.to_owned();
    (catalog, id, snapshot, deletion)
}

#[test]
fn pending_delete_policy_uses_genuine_catalogue_and_refuses_installed_metadata_conversion() {
    let (catalog, id, snapshot, deletion) = fixture("");
    assert!(snapshot.metadata("-1").is_err());
    assert!(snapshot.audio("-1").is_err());
    assert!(snapshot.artwork("-1").is_err());
    let policy = snapshot.pending_delete(&catalog, &id, &deletion).unwrap();
    assert_eq!(policy.own_asset_id(), "-1");
    policy.ensure_session(id.library_id(), 140).unwrap();
    assert!(policy.ensure_session(id.library_id(), 141).is_err());
    assert!(policy.ensure_session("other", 140).is_err());
    assert!(
        policy
            .validate_manifest(&manifest(&[]), 2)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pending_delete_policy_rejects_every_own_asset_mask_atomically() {
    let (catalog, id, snapshot, deletion) = fixture("");
    let policy = snapshot.pending_delete(&catalog, &id, &deletion).unwrap();
    for part in [0, 1, 2, 3, 4, 5, 6, 7, u64::MAX] {
        assert!(
            policy
                .validate_manifest(&manifest(&[("-1", part)]), 2)
                .is_err()
        );
        // No valid foreign prefix may escape a later forbidden own request.
        assert!(
            policy
                .validate_manifest(&manifest(&[("-2", 2), ("-1", part)]), 2)
                .is_err()
        );
    }
}

#[test]
fn pending_delete_policy_foreign_requests_require_actual_registered_eligible_rows() {
    let (catalog, id, snapshot, deletion) = fixture("");
    let policy = snapshot.pending_delete(&catalog, &id, &deletion).unwrap();
    for part in [2, 4] {
        assert_eq!(
            policy
                .validate_manifest(&manifest(&[("-2", part)]), 2)
                .unwrap(),
            ["-2"]
        );
    }
    for records in [
        vec![("-2", 1)],
        vec![("-2", 3)],
        vec![("-2", 5)],
        vec![("-2", 2), ("-2", 4)],
        vec![("-21", 4)],
        vec![("-12345", 4)],
        vec![("-02", 4)],
        vec![("0", 4)],
    ] {
        assert!(policy.validate_manifest(&manifest(&records), 2).is_err());
    }
    let (catalog, id, snapshot, deletion) = fixture("DELETE FROM artwork_token;");
    let policy = snapshot.pending_delete(&catalog, &id, &deletion).unwrap();
    assert!(
        policy
            .validate_manifest(&manifest(&[("-2", 2)]), 2)
            .is_err()
    );
    assert_eq!(
        policy
            .validate_manifest(&manifest(&[("-2", 4)]), 2)
            .unwrap(),
        ["-2"]
    );
}

#[test]
fn pending_delete_policy_rejects_changed_snapshot_library_and_anchor() {
    let (catalog, id, snapshot, deletion) = fixture("");
    assert!(
        snapshot
            .pending_delete(&catalog, &identity(4, 7), &deletion)
            .is_err()
    );
    let (changed, _, fresh, _) =
        fixture("UPDATE item_stats SET play_count_recent=1 WHERE item_pid=2;");
    assert!(snapshot.pending_delete(&changed, &id, &deletion).is_err());
    assert!(fresh.pending_delete(&changed, &id, &deletion).is_err());
}

#[test]
fn pending_delete_policy_rejects_non_manifest_filecomplete_and_wrong_session() {
    let (catalog, id, snapshot, deletion) = fixture("");
    let policy = snapshot.pending_delete(&catalog, &id, &deletion).unwrap();
    assert!(policy.validate_manifest(&manifest(&[]), 0).is_err());
    assert!(policy.validate_manifest(&manifest(&[]), 3).is_err());
    for command in ["FileComplete", "AssetReady", "SyncFinished"] {
        let mut wrong = manifest(&[]);
        wrong
            .as_dictionary_mut()
            .unwrap()
            .insert("Command".into(), command.into());
        assert!(policy.validate_manifest(&wrong, 2).is_err());
    }
    let mut extra = manifest(&[]);
    extra
        .as_dictionary_mut()
        .unwrap()
        .insert("Unexpected".into(), true.into());
    assert!(policy.validate_manifest(&extra, 2).is_err());
}
