//! Ordinary recovery's independent original-effect and later-playback proofs.
//! Only the caller's fresh physical checks can make these completion evidence.
use super::*;
use crate::{
    engine::recovery::retained::playback::Window, engine::verification::journal,
    engine::verification::sql_tables::Capture,
};
use music_protocol::catalogue::insert_verification;

pub(super) struct CatalogProof {
    pub(super) evidence: Json,
    pub(super) managed_preservation_required: bool,
}

pub(super) fn verify_catalog(
    attempt: &Attempt,
    retained: &RetainedRun,
    current: &Current,
    source: &metadata::Metadata,
    expected: &ExpectedInsert<'_>,
    options: &Options<'_>,
    control: &Control,
    attributes: &music_protocol::protocol::transfer::Attributes,
) -> Result<CatalogProof> {
    control.check()?;
    let fresh_dir = options.current_state_dir;
    directory(fresh_dir)?;
    let fresh_dir = fs::canonicalize(fresh_dir)?;
    let original_sql = |catalog: &Catalog, directory: &Path, output: &Path, window: Window| {
        let before = retained.before.source_hashes();
        let after = catalog.source_hashes();
        let db = directory.join("catalog/MediaLibrary.sqlitedb");
        let wal = directory.join("catalog/MediaLibrary.sqlitedb-wal");
        ordinary_sql::verify(
            &attempt.prepared,
            source,
            &retained.before,
            Capture {
                database: &retained.before_database,
                database_sha256: before.database,
                wal: retained.before_wal.as_deref().zip(before.wal),
            },
            Capture {
                database: &db,
                database_sha256: after.database,
                wal: after.wal.map(|hash| (wal.as_path(), hash)),
            },
            output,
            window,
            control,
            attributes,
        )
    };
    let load = |directory: &Path, receipt_sha256: String| {
        crate::engine::recovery::saved::load_saved_current_with_control(
            &crate::engine::recovery::saved::SavedCurrent {
                directory: directory.to_owned(),
                receipt_sha256,
            },
            &current.device,
            options.trace,
            control.deadline.into_std(),
            Arc::clone(&control.cancelled),
        )
    };
    // Successful strict-current recovery requires no optional after artifact.
    // The fresh capture is a later run's, so listening and phone housekeeping
    // since the write are absorbed under the full SQL comparison. Only a typed,
    // well-formed semantic mismatch enters the separate bridge.
    let refusal = match original_sql(
        &current.catalog,
        &fresh_dir,
        &options.trace.join("ordinary-full-sql"),
        Window::LaterRun,
    ) {
        Ok(sql) => {
            let listened = sql
                .get("between_runs_playback")
                .is_some_and(playback::between_runs::listened);
            let projection = if listened {
                // As the bridge does, the saved fresh evidence must be the
                // captured Current in every scope the SQL proof does not read.
                let fresh = load(&fresh_dir, hex(&current.receipt_sha256))?;
                ensure_loaded_fresh(current, &fresh.current)?;
                insert_verification::verify_insert_after_listening(
                    &retained.before,
                    &current.catalog,
                    expected,
                    &playback::between_runs::catalogue_listening,
                )?
            } else {
                insert_verification::verify_insert(&retained.before, &current.catalog, expected)?
            };
            // As after the bridge, listened survivors' physical preservation
            // is checked by the managed pass rather than assumed.
            return Ok(CatalogProof {
                evidence: json!({"projection":projection,"all45_typed_sql":sql}),
                managed_preservation_required: listened,
            });
        }
        Err(error) if error.is::<ordinary_sql::PlaybackEffectMismatch>() => error,
        Err(error) => return Err(error),
    };
    control.check()?;
    durable_write(
        options.trace,
        "ordinary-strict-refusal.json",
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.ordinary-strict-refusal",
            "reason":format!("{refusal:#}"),"classification":"playback_effect_mismatch",
            "phone_writes":0,"journal_completion_authorized":false,
        }))?,
    )?;
    let immediate_dir = retained.step.join("after");
    match fs::symlink_metadata(&immediate_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(refusal),
        Err(error) => return Err(error.into()),
        Ok(_) => directory(&immediate_dir)?,
    }
    let immediate_dir = fs::canonicalize(immediate_dir)?;
    let receipt = read_bounded(&immediate_dir.join("state-receipt.json"), SMALL)?;
    control.check()?;
    let immediate = load(&immediate_dir, hex(&Sha256::digest(&receipt)))?;
    let fresh = load(&fresh_dir, hex(&current.receipt_sha256))?;
    ensure_loaded_fresh(current, &fresh.current)?;
    playback::same_ordinary_scope(&immediate.current, current)?;
    let sql = original_sql(
        &immediate.current.catalog,
        &immediate_dir,
        &options.trace.join("ordinary-immediate-full-sql"),
        Window::SameRun,
    )?;
    let projection =
        insert_verification::verify_insert(&retained.before, &immediate.current.catalog, expected)?;
    let bridge = playback::verify_ordinary(
        &immediate.current,
        &immediate_dir,
        current,
        &fresh_dir,
        &options.trace.join("ordinary-playback-bridge"),
        control,
    )?;
    durable_write(
        options.trace,
        "ordinary-retained-immediate-state.json",
        &immediate.receipt,
    )?;
    control.check()?;
    Ok(CatalogProof {
        evidence: json!({"schema":"rustunes.ordinary-recovery-two-stage",
            "original_insertion":{"projection":projection,"all45_typed_sql":sql},
            "verified_against":"matched_step_immediate_after","playback_bridge":bridge.evidence(),
            "all_managed_physical_preservation_still_required":true,"operation_replayed":false}),
        managed_preservation_required: true,
    })
}

pub(super) fn ensure_loaded_fresh(current: &Current, loaded: &Current) -> Result<()> {
    playback::same_ordinary_scope(current, loaded)?;
    let expected = current.catalog.source_hashes();
    let actual = loaded.catalog.source_hashes();
    ensure!(
        current.artwork_db == loaded.artwork_db
            && current.receipt_sha256 == loaded.receipt_sha256
            && expected.database == actual.database
            && expected.database_bytes == actual.database_bytes
            && expected.wal == actual.wal
            && expected.wal_bytes == actual.wal_bytes,
        "saved fresh ordinary evidence differs from the captured Current"
    );
    Ok(())
}

/// Consume only the report returned by the current read-only All verification.
/// No saved report, partial selection or provisional per-file result is loaded.
pub(super) fn require_complete_preservation(
    current: &Current,
    bindings: &[crate::engine::state::journal::Binding],
    report: &journal::Report,
) -> Result<()> {
    let artwork_count = bindings
        .iter()
        .filter(|b| b.content.artwork.is_some())
        .count();
    ensure!(
        report.schema == "rustunes.journal-verification"
            && report.verification_completed
            && report.failure.is_none()
            && !report.atomic_snapshot_claimed
            && report.phone_writes == 0
            && report.journal_mutations == 0
            && report.device_identifier_sha256 == hex(&current.identity.device_digest())
            && report.library_identity_sha256 == hex(&current.identity.library_digest())
            && report.input_receipt_sha256 == hex(&current.receipt_sha256)
            && report.media_anchor == current.catalog.persisted_media_anchor()
            && report.selected_bindings == bindings.len()
            && report.total_verified_journal_bindings == bindings.len()
            && report.tracks.len() == bindings.len()
            && report.completed_batches == report.planned_batches
            && report.planned_artwork_tracks == artwork_count
            && report.completed_artwork_tracks == artwork_count
            && (artwork_count == 0 || report.permanent_artwork_verified)
            && if bindings.is_empty() {
                report.status == "no_bindings"
            } else {
                report.status == "audio_checks_complete" && report.all_selected_audio_verified
            },
        "ordinary recovery requires complete fresh All managed audio/artwork preservation"
    );
    let mut seen = BTreeSet::new();
    for binding in bindings {
        let content = &binding.content;
        let id = format!("{:016x}", content.track_pid);
        ensure!(seen.insert(id.clone()), "duplicate active managed binding");
        let matches: Vec<_> = report.tracks.iter().filter(|t| t.track_id == id).collect();
        ensure!(
            matches.len() == 1,
            "managed physical report has missing or duplicate track"
        );
        let track = matches[0];
        ensure!(
            track.path == content.path
                && track.audio_bytes == content.source.file_bytes
                && track.expected_audio_sha256 == hex(&content.source.full_file_sha256)
                && track.encoded_audio_sha256 == hex(&content.source.audio.sha256)
                && track.audio_observed_sha256.as_deref()
                    == Some(track.expected_audio_sha256.as_str())
                && track.audio_status == journal::AudioStatus::VerifiedTransferredContainer
                && track.audio_attributes.as_ref().is_some_and(|attributes| {
                    music_protocol::protocol::transfer::regular_size(
                        attributes,
                        content.source.file_bytes,
                    )
                    .is_ok()
                })
                && track.finding.is_none()
                && track.artwork.expected_cache_id == content.artwork.as_ref().map(|a| a.cache_id)
                && track.artwork.expected_source_cover_sha256
                    == content.artwork.as_ref().map(|a| hex(&a.sha256))
                && (content.artwork.is_none()
                    || (track.artwork.permanent_cache_proof.is_some()
                        && track.artwork.track_registration_matches == Some(true)
                        && track.artwork.observed_track_cache_id
                            == content.artwork.as_ref().map(|a| a.cache_id)
                        && track.artwork.permanent_cache_failure.is_none())),
            "managed physical audio/artwork proof differs from active binding"
        );
    }
    Ok(())
}
