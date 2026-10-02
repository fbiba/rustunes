//! Exact projected poststate for the serializer's accepted composed request.
//! A successful comparison supplies no receiver or journal completion authority.
use super::{hex, verify_groups_and_artwork, verify_identity_and_anchor, verify_tracks};
use crate::catalogue::{
    metadata_update::{self, SourceFieldRequest, SourceMetadataBinding, SourceMetadataPatch},
    sqlite::Catalog,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

#[derive(Serialize)]
pub struct ProjectedSourceMetadataVerification {
    pub schema: &'static str,
    pub target_track_id: String,
    pub unchanged_album_id: String,
    pub media_anchor: u32,
    pub requested: SourceMetadataPatch,
    pub changed_fields: Vec<String>,
    pub total_tracks: usize,
    pub other_tracks_preserved: usize,
    pub other_title_sorts_preserved: usize,
    pub typed_album_artist_groups_preserved: bool,
    pub projected_artwork_preserved: bool,
    pub all_track_statistics_preserved: bool,
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

/// Verify title/sort and supported scalar requests as one indivisible change.
/// Unrequested track values, every sibling, nullable title sorts, typed album/
/// artist groups, projected artwork and all typed track statistics must be preserved.
/// Permanent bytes and receiver-specific lookup/order changes need independent
/// engine evidence; this receipt does not authorize application or completion.
pub fn verify_source_metadata(
    before: &Catalog,
    after: &Catalog,
    binding: &SourceMetadataBinding,
    patch: &SourceMetadataPatch,
) -> Result<ProjectedSourceMetadataVerification> {
    let accepted = metadata_update::normalize_source_metadata(before, binding, patch)?;
    verify_identity_and_anchor(before, after)?;
    verify_tracks(before.tracks(), after.tracks(), &accepted.expected)?;
    verify_groups_and_artwork(before, after)?;
    // Track equality covers play_count_recent; the fingerprint covers all other
    // typed columns, including dates/bookmarks absent from the Track projection.
    ensure!(
        before.stats_except_recent_play() == after.stats_except_recent_play(),
        "source metadata update changed typed statistics"
    );
    let target = binding.target();
    for track in before.tracks() {
        let old_sort = before
            .title_sort(track.sync_id)
            .context("before title-sort row absent")?;
        let new_sort = after
            .title_sort(track.sync_id)
            .context("after title-sort row absent")?;
        if track.sync_id == target {
            ensure!(
                new_sort.as_deref() == Some(accepted.expected_sort.as_str()),
                "target title sort does not match the accepted source request"
            );
        } else {
            ensure!(
                old_sort == new_sort,
                "source metadata update changed a sibling title sort"
            );
        }
    }
    // Preserve the existing title-only contract when composer is not requested.
    // A composer request may change its lookup/reference representation; only
    // its projected text is qualified here, with sibling references retained.
    if matches!(&patch.composer, SourceFieldRequest::Preserve) {
        ensure!(
            before.composers() == after.composers()
                && before.track_composers() == after.track_composers(),
            "source metadata update changed unrequested composer lookup or references"
        );
    } else {
        for (track, reference) in before.track_composers() {
            if *track != target {
                ensure!(
                    after.track_composers().get(track) == Some(reference),
                    "source metadata update changed a sibling composer reference"
                );
            }
        }
    }
    Ok(ProjectedSourceMetadataVerification {
        schema: "music-protocol.source-metadata-projection",
        target_track_id: format!("{target:016x}"),
        unchanged_album_id: format!("{:016x}", binding.before().album_sync_id),
        media_anchor: after.persisted_media_anchor(),
        requested: patch.clone(),
        changed_fields: accepted.changed_fields,
        total_tracks: before.tracks().len(),
        other_tracks_preserved: before.tracks().len() - 1,
        other_title_sorts_preserved: before.tracks().len() - 1,
        typed_album_artist_groups_preserved: true,
        projected_artwork_preserved: true,
        all_track_statistics_preserved: true,
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
            "Raw SQL/search/order/revision rows and composer/genre lookup changes for requested fields",
            "Foreign/unsupported artwork relationship payloads beyond projected registrations/counts",
            "Permanent audio/images, raw ArtworkDB, source ownership and journal history",
            "Signed operation, same-session Ready/manifest/terminal and native receiver qualification",
        ],
    })
}

#[cfg(test)]
mod tests;
