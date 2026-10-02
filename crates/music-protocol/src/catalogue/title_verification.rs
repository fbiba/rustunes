//! Projected verification only for the explicit native title/sort profile.
//! Raw SQL/search/order rows, permanent bytes and protocol terminal are not
//! verified here. This receipt cannot authorize journal completion.
use crate::{
    catalogue::metadata_update::{self, TitleBinding, TitlePatch},
    catalogue::metadata_verification::{
        hex, verify_groups_and_artwork, verify_identity_and_anchor, verify_tracks,
    },
    catalogue::sqlite::Catalog,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

#[derive(Serialize)]
pub struct ProjectedTitleVerification {
    pub schema: &'static str,
    pub target_track_id: String,
    pub unchanged_album_id: String,
    pub media_anchor: u32,
    pub before: TitlePatch,
    pub requested: TitlePatch,
    pub changed_fields: Vec<&'static str>,
    pub total_tracks: usize,
    pub other_tracks_preserved: usize,
    pub other_title_sorts_preserved: usize,
    pub typed_groups_and_composers_preserved: bool,
    pub projected_artwork_preserved: bool,
    pub before_database_sha256: String,
    pub before_wal_sha256: Option<String>,
    pub after_database_sha256: String,
    pub after_wal_sha256: Option<String>,
    pub raw_sql_verified: bool,
    pub permanent_bytes_verified: bool,
    pub protocol_terminal_verified: bool,
    pub journal_completion_authorized: bool,
    pub receiver_qualification_required: bool,
    pub unverified_fields: [&'static str; 4],
}

pub fn verify(
    before: &Catalog,
    after: &Catalog,
    binding: &TitleBinding,
    patch: &TitlePatch,
) -> Result<ProjectedTitleVerification> {
    metadata_update::validate_title_change(before, binding, patch)?;
    verify_identity_and_anchor(before, after)?;
    let target = binding.target();
    let mut expected = metadata_update::target_track(before, target)?.clone();
    expected.metadata.title = patch.title.clone();
    verify_tracks(before.tracks(), after.tracks(), &expected)?;
    verify_groups_and_artwork(before, after)?;
    ensure!(
        before.composers() == after.composers()
            && before.track_composers() == after.track_composers(),
        "title update changed composer lookup or reference state"
    );
    for track in before.tracks() {
        let old_sort = before
            .title_sort(track.sync_id)
            .context("before title-sort row absent")?;
        let new_sort = after
            .title_sort(track.sync_id)
            .context("after title-sort row absent")?;
        if track.sync_id == target {
            ensure!(
                new_sort.as_deref() == Some(patch.sort_name.as_str()),
                "target title sort does not match the explicit request"
            );
        } else {
            ensure!(
                old_sort == new_sort,
                "title update changed another track's sort"
            );
        }
    }
    let mut changed_fields = Vec::new();
    if binding.before().title != patch.title {
        changed_fields.push("title");
    }
    if binding.before().sort_name != patch.sort_name {
        changed_fields.push("sort_name");
    }
    Ok(ProjectedTitleVerification {
        schema: "music-protocol.title-metadata-projection",
        target_track_id: format!("{target:016x}"),
        unchanged_album_id: format!("{:016x}", binding.album_pid()),
        media_anchor: after.persisted_media_anchor(),
        before: binding.before().clone(),
        requested: patch.clone(),
        changed_fields,
        total_tracks: before.tracks().len(),
        other_tracks_preserved: before.tracks().len() - 1,
        other_title_sorts_preserved: before.tracks().len() - 1,
        typed_groups_and_composers_preserved: true,
        projected_artwork_preserved: true,
        before_database_sha256: hex(&before.source_hashes().database),
        before_wal_sha256: before.source_hashes().wal.as_ref().map(|h| hex(h)),
        after_database_sha256: hex(&after.source_hashes().database),
        after_wal_sha256: after.source_hashes().wal.as_ref().map(|h| hex(h)),
        raw_sql_verified: false,
        permanent_bytes_verified: false,
        protocol_terminal_verified: false,
        journal_completion_authorized: false,
        receiver_qualification_required: true,
        unverified_fields: [
            "Raw SQL rows/schema including title search, order/section, sort maps, dates and revisions",
            "Foreign/unsupported artwork relationship payloads beyond projected registrations/counts",
            "Permanent audio/images, raw ArtworkDB, source ownership and journal history",
            "Signed operation, same-session Ready/manifest/terminal and native receiver qualification",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND_AND_ART: &str = "
        INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
        INSERT INTO item_extra SELECT 2,'Sibling','SIBLING.m4a',total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,NULL,comment FROM item_extra WHERE item_pid=1;
        INSERT INTO item_store VALUES(2,123);
        INSERT INTO item_state SELECT 2,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1;
        INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;
        INSERT INTO artwork VALUES('100',300,6,0,'aa/00000000000000000000000000000000000000');
        INSERT INTO artwork_token VALUES(1,'100',300,1,0,0);
        INSERT INTO artwork_token VALUES(31,'100',300,6,4,0);
        INSERT INTO artwork_token VALUES(1,'opaque',500,1,0,0);";
    const CHANGE: &str =
        "UPDATE item_extra SET title='Changed title',sort_title='Changed sort' WHERE item_pid=1;";

    fn catalog(anchor: u32, sql: &str) -> Catalog {
        crate::test_support::catalogue::modified_collection_catalog(
            anchor,
            &format!("{SECOND_AND_ART}\n{sql}"),
        )
    }
    fn patch() -> TitlePatch {
        TitlePatch {
            title: "Changed title".into(),
            sort_name: "Changed sort".into(),
        }
    }

    #[test]
    fn accepts_exact_title_sort_and_reports_only_projected_verification() {
        let before = catalog(12, "");
        let binding = metadata_update::bind_title(&before, u64::MAX).unwrap();
        let after = catalog(13, CHANGE);
        let result = verify(&before, &after, &binding, &patch()).unwrap();
        assert_eq!(result.target_track_id, "ffffffffffffffff");
        assert_eq!(result.unchanged_album_id, "ffffffffffffffe1");
        assert_eq!(result.media_anchor, 13);
        assert_eq!(result.changed_fields, ["title", "sort_name"]);
        assert_eq!(result.total_tracks, 2);
        assert_eq!(result.other_tracks_preserved, 1);
        assert_eq!(result.other_title_sorts_preserved, 1);
        assert_eq!(after.title_sort(123), Some(&None));
        let json = serde_json::to_value(result).unwrap();
        for key in [
            "raw_sql_verified",
            "permanent_bytes_verified",
            "protocol_terminal_verified",
            "journal_completion_authorized",
        ] {
            assert_eq!(json[key], false, "{key}");
        }
        assert_eq!(json["receiver_qualification_required"], true);
        assert_eq!(json["unverified_fields"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn rejects_unrequested_track_identity_group_composer_and_art_changes() {
        let before = catalog(12, "");
        let binding = metadata_update::bind_title(&before, u64::MAX).unwrap();
        for sql in [
            "UPDATE item_extra SET year=2000 WHERE item_pid=1;",
            "UPDATE item_extra SET location='OTHER.m4a' WHERE item_pid=1;",
            "UPDATE item_extra SET file_size=file_size+1 WHERE item_pid=1;",
            "UPDATE item SET is_compilation=1 WHERE item_pid=1;",
            "UPDATE item_stats SET play_count_user=1 WHERE item_pid=2;",
            "UPDATE item_state SET is_placeholder=1 WHERE item_pid=1;",
            "UPDATE item_store SET sync_id=456 WHERE item_pid=1;",
            "UPDATE album SET album='Changed orphan' WHERE album_pid=32;",
            "UPDATE album SET all_compilations=1 WHERE album_pid=31;",
            "UPDATE album_artist SET album_artist='Changed orphan' WHERE album_artist_pid=22;",
            "UPDATE composer SET sort_composer='Changed composer sort';",
            "UPDATE item SET composer_order=101 WHERE item_pid=2;",
            "INSERT INTO composer VALUES(72,0,'Orphan Composer','Orphan Composer');",
            "UPDATE artwork SET relative_path='bb/00000000000000000000000000000000000000';",
            "UPDATE artwork_token SET entity_pid=2 WHERE artwork_source_type=500;",
        ] {
            let after = catalog(13, &format!("{CHANGE}{sql}"));
            assert!(
                verify(&before, &after, &binding, &patch()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn rejects_nullable_or_unrequested_sort_and_wrong_target_title() {
        let before = catalog(12, "");
        let binding = metadata_update::bind_title(&before, u64::MAX).unwrap();
        for sql in [
            "UPDATE item_extra SET sort_title=NULL WHERE item_pid=1;",
            "UPDATE item_extra SET sort_title='' WHERE item_pid=1;",
            "UPDATE item_extra SET sort_title='Other' WHERE item_pid=1;",
            "UPDATE item_extra SET title='Other' WHERE item_pid=1;",
            "UPDATE item_extra SET sort_title='' WHERE item_pid=2;",
            "UPDATE item_extra SET sort_title='Other' WHERE item_pid=2;",
        ] {
            let after = catalog(13, &format!("{CHANGE}{sql}"));
            assert!(
                verify(&before, &after, &binding, &patch()).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn rejects_wrong_anchor_identity_noop_and_a_stale_before_binding() {
        let before = catalog(12, "");
        let binding = metadata_update::bind_title(&before, u64::MAX).unwrap();
        for anchor in [12, 14] {
            assert!(verify(&before, &catalog(anchor, CHANGE), &binding, &patch()).is_err());
        }
        let other = catalog(13, &format!("{CHANGE}UPDATE db_info SET db_pid=-100;"));
        assert!(verify(&before, &other, &binding, &patch()).is_err());
        assert!(verify(&before, &catalog(13, ""), &binding, binding.before()).is_err());
        let stale = catalog(
            12,
            "UPDATE item_extra SET sort_title='Another original' WHERE item_pid=1;",
        );
        assert!(verify(&stale, &catalog(13, CHANGE), &binding, &patch()).is_err());
    }

    #[test]
    fn restored_projection_requires_the_changed_binding_and_preserves_nullable_sibling() {
        let original = catalog(12, "");
        let old = metadata_update::bind_title(&original, u64::MAX).unwrap();
        let changed = catalog(13, CHANGE);
        let current = metadata_update::bind_title(&changed, u64::MAX).unwrap();
        let restored = catalog(14, "");
        assert!(verify(&changed, &restored, &old, old.before()).is_err());
        let receipt = verify(&changed, &restored, &current, old.before()).unwrap();
        assert_eq!(receipt.media_anchor, 14);
        assert_eq!(receipt.requested, *old.before());
        assert_eq!(restored.title_sort(123), Some(&None));
        assert!(!receipt.journal_completion_authorized);
    }
}
