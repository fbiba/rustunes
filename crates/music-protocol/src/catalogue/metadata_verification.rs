//! Pure verification of field-scoped scalar or composed source updates in projected SQL state.
//! Does not verify unprojected lookup/sort rows, phone audio bytes or a protocol
//! terminal. A caller must bind those separately before journal completion.
use crate::{
    catalogue::metadata_update::{self, ScalarPatch},
    catalogue::sqlite::{Catalog, Track},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::BTreeMap;

mod source;
pub use source::{ProjectedSourceMetadataVerification, verify_source_metadata};

#[derive(Serialize)]
pub struct VerifiedMetadata {
    pub schema: &'static str,
    pub target_track_id: String,
    pub media_anchor: u32,
    pub requested: ScalarPatch,
    pub changed_fields: Vec<String>,
    pub tracks_preserved: usize,
    pub typed_album_artist_groups_preserved: bool,
    pub projected_artwork_preserved: bool,
    pub before_database_sha256: String,
    pub before_wal_sha256: Option<String>,
    pub after_database_sha256: String,
    pub after_wal_sha256: Option<String>,
    pub audio_bytes_verified: bool,
    pub receiver_qualification_required: bool,
    pub unverified_fields: [&'static str; 4],
}
pub fn verify(
    before: &Catalog,
    after: &Catalog,
    target: u64,
    patch: &ScalarPatch,
) -> Result<VerifiedMetadata> {
    let original = metadata_update::target_track(before, target)?;
    metadata_update::reject_shared_case_alias(before, original, patch)?;
    let (expected, fields) = metadata_update::apply(original, patch)?;
    verify_identity_and_anchor(before, after)?;
    verify_tracks(before.tracks(), after.tracks(), &expected)?;
    verify_groups_and_artwork(before, after)?;
    Ok(VerifiedMetadata {
        schema: "music-protocol.scalar-metadata-verification",
        target_track_id: format!("{target:016x}"),
        media_anchor: after.persisted_media_anchor(),
        requested: patch.clone(),
        changed_fields: fields.keys().cloned().collect(),
        tracks_preserved: before.tracks().len(),
        typed_album_artist_groups_preserved: true,
        projected_artwork_preserved: true,
        before_database_sha256: hex(&before.source_hashes().database),
        before_wal_sha256: before.source_hashes().wal.as_ref().map(|h| hex(h)),
        after_database_sha256: hex(&after.source_hashes().database),
        after_wal_sha256: after.source_hashes().wal.as_ref().map(|h| hex(h)),
        audio_bytes_verified: false,
        receiver_qualification_required: true,
        unverified_fields: [
            "Unprojected SQL columns including sort keys, playback dates and bookmarks",
            "Composer/genre lookup rows and grouping keys beyond each track's projected text",
            "Foreign or unsupported artwork relationship payloads beyond exposed counts",
            "Permanent audio/image bytes, source ownership and protocol terminal",
        ],
    })
}
/// Check the factual catalogue identity and exactly-next anchor transition.
/// This observation alone does not establish application completion.
pub fn verify_identity_and_anchor(before: &Catalog, after: &Catalog) -> Result<()> {
    ensure!(
        before.persisted_media_anchor().checked_add(1) == Some(after.persisted_media_anchor()),
        "metadata update anchor is not exactly next"
    );
    ensure!(
        before.database_pid() == after.database_pid()
            && before.primary_container_pid() == after.primary_container_pid(),
        "metadata update changed catalog identity"
    );
    Ok(())
}
pub(crate) fn verify_groups_and_artwork(before: &Catalog, after: &Catalog) -> Result<()> {
    ensure!(
        before.albums() == after.albums()
            && before.artists() == after.artists()
            && before.represented_sync_ids() == after.represented_sync_ids(),
        "metadata update changed mapped collection identities or rows"
    );
    verify_artwork_preserved(before, after)
}
/// Compare independently parsed artwork registrations and occupied IDs.
/// Permanent artwork payload bytes are outside this projected SQL check.
pub fn verify_artwork_preserved(before: &Catalog, after: &Catalog) -> Result<()> {
    let a = before.artwork();
    let b = after.artwork();
    let other_counts = |inventory: &crate::catalogue::sqlite::ArtworkInventory| {
        inventory
            .other_source_rows()
            .iter()
            .map(|(source, count)| (*source, count.cached_rows, count.registration_rows))
            .collect::<Vec<_>>()
    };
    ensure!(
        a.occupied_cache_ids() == b.occupied_cache_ids()
            && a.cached_ids() == b.cached_ids()
            && a.cached_objects() == b.cached_objects()
            && a.track_cache_ids() == b.track_cache_ids()
            && a.tracks_with_artwork_registration() == b.tracks_with_artwork_registration()
            && a.album_cache_ids() == b.album_cache_ids()
            && a.source300_rows().cached_rows == b.source300_rows().cached_rows
            && a.source300_rows().registration_rows == b.source300_rows().registration_rows
            && a.unmapped_source300_token_rows() == b.unmapped_source300_token_rows()
            && a.orphan_album_cache_ids() == b.orphan_album_cache_ids()
            && other_counts(a) == other_counts(b),
        "metadata update changed projected artwork"
    );
    Ok(())
}
pub(crate) fn verify_tracks(before: &[Track], after: &[Track], expected: &Track) -> Result<()> {
    let mut old: BTreeMap<_, _> = before.iter().map(|t| (t.sync_id, t)).collect();
    let new: BTreeMap<_, _> = after.iter().map(|t| (t.sync_id, t)).collect();
    ensure!(
        old.len() == before.len() && new.len() == after.len(),
        "duplicate metadata verification identity"
    );
    ensure!(
        old.contains_key(&expected.sync_id),
        "metadata target missing"
    );
    old.insert(expected.sync_id, expected);
    ensure!(
        old == new,
        "metadata update changed an unrequested field, identity, audio path/size, availability or statistic"
    );
    Ok(())
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compares_exact_allowed_target_and_preserves_all_other_fields() {
        let c = crate::test_support::catalogue::collection_catalog(12);
        let mut before = c.tracks().to_vec();
        let mut sibling = before[0].clone();
        sibling.sync_id = 123;
        sibling.internal_id = 2;
        before.push(sibling);
        let patch = ScalarPatch {
            composer: Some("New literal composer".into()),
            disc_number: Some(2),
            ..Default::default()
        };
        let (expected, _) = metadata_update::apply(&before[0], &patch).unwrap();
        let mut after = before.clone();
        after[0] = expected.clone();
        assert!(verify_tracks(&before, &after, &expected).is_ok());
        for variant in 0..10 {
            let mut wrong = after.clone();
            match variant {
                0 => wrong[0].metadata.artist.push('!'),
                1 => wrong[0].album_sync_id += 1,
                2 => wrong[0].artist_sync_id += 1,
                3 => wrong[0].playback.play_count_user += 1,
                4 => wrong[0].metadata.file_size += 1,
                5 => wrong[0].afc_path = Some("other".into()),
                6 => wrong[1].metadata.composer = expected.metadata.composer.clone(),
                7 => wrong[0].metadata.content_rating += 1,
                8 => wrong[0].availability.is_placeholder = true,
                _ => {
                    wrong.pop();
                }
            }
            assert!(verify_tracks(&before, &wrong, &expected).is_err());
        }
    }
    #[test]
    fn rejects_unmodified_target_and_wrong_anchor_before_claiming_success() {
        let before = crate::test_support::catalogue::collection_catalog(12);
        let after = crate::test_support::catalogue::collection_catalog(13);
        let patch = ScalarPatch {
            year: Some(2025),
            ..Default::default()
        };
        assert!(verify(&before, &after, u64::MAX, &patch).is_err());
        assert!(verify(&before, &before, u64::MAX, &patch).is_err());
        assert!(verify(&before, &after, 123, &patch).is_err());
    }
}
