//! Apply one source cover to its already verified journal track. Audio is not
//! retransferred. Completion requires independent SQL and permanent-image proof.
use std::{
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::{
    engine::execution::device,
    engine::execution::native_artwork::{self, ArtworkUpdate, FreshArtworkState},
    engine::state::current::{self, hex},
    engine::state::journal::{
        Artwork, AttemptKind, CompletionEvidence, Identity, Journal, PreparedAttempt,
    },
    engine::state::snapshot,
    engine::{CoverResult, Engine, Event},
};
use music_protocol::catalogue::artwork_update;
use music_protocol::catalogue::artwork_verification;
use music_protocol::catalogue::identity;
use music_protocol::media::artwork;
use music_protocol::media::fingerprint;
use music_protocol::protocol::trace::private_write;

pub struct Options<'a> {
    pub file: &'a Path,
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub trace: &'a Path,
    pub host_name: &'a str,
    pub dry_run: bool,
    /// When invoked by sync, bind the cover to the exact reviewed source file.
    pub expected_source: Option<&'a fingerprint::SourceFingerprint>,
    /// An album present before an add-only sync must keep its existing cover bytes.
    pub preserve_album: Option<u64>,
    /// Add-only may finish only an owned first cover, never replace a cover
    /// introduced after the saved plan.
    pub pending_cover_only: bool,
}

pub(crate) async fn run(options: Options<'_>, engine: &Engine) -> Result<CoverResult> {
    engine.check()?;
    ensure!(
        !options.trace.try_exists()?,
        "artwork trace directory must be new"
    );
    let saved = snapshot::open(options.snapshot)?;
    let library = identity::parse(&saved.raw_preferences)?;
    let identity = Identity::for_saved_device(saved.identity.device_identifier_sha256, &library)?;
    let mut journal = Journal::open(options.journal, &identity)?;
    ensure!(
        journal.outstanding()?.is_none(),
        "unfinished journal operation requires reconciliation before an artwork update"
    );
    let desired = fingerprint::read(options.file)?;
    if let Some(expected) = options.expected_source {
        ensure!(
            &desired == expected,
            "source changed after sync planning; cover update refused"
        );
    }
    let binding = journal
        .lookup_active_verified(&desired.audio)?
        .context("source has no active verified journal binding; verify or sync its audio first")?;
    let source_art = artwork::read(options.file)?;
    ensure!(
        source_art.source_size == desired.file_bytes
            && source_art.source_sha256 == desired.full_file_sha256,
        "source changed during artwork inspection"
    );
    let cover = source_art
        .cover
        .context("source contains no supported embedded JPEG or PNG cover")?;
    require_decoder()?;
    fs::DirBuilder::new().mode(0o700).create(options.trace)?;
    File::open(
        options
            .trace
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    let capture_control = engine.control(120);
    let current = current::capture_with_artwork_controlled(
        &options.trace.join("before"),
        &identity,
        capture_control.deadline,
        Arc::clone(&capture_control.cancelled),
    )
    .await?;
    let artwork_db = current
        .artwork_db
        .as_deref()
        .context("artwork database capture absent")?;
    let target = current
        .catalog
        .tracks()
        .iter()
        .find(|t| t.sync_id == binding.content.track_pid)
        .context("verified track is absent from current catalogue")?;
    ensure!(
        target.afc_path.as_deref() == Some(binding.content.path.as_str())
            && target.metadata.file_size == binding.content.source.file_bytes,
        "verified target path or audio size changed"
    );
    if options.pending_cover_only {
        ensure!(
            crate::engine::planning::additive::owes_cover(
                &current.catalog,
                target.sync_id,
                &desired,
                &binding,
                false
            ),
            "add-only pending cover changed after planning; existing artwork is preserved"
        );
    }
    if let Some(existing) = binding
        .content
        .artwork
        .as_ref()
        .filter(|art| art.sha256 == cover.sha256 && art.bytes == cover.bytes.len() as u64)
        && current
            .catalog
            .artwork()
            .track_cache_ids()
            .get(&target.sync_id)
            == Some(&existing.cache_id)
    {
        let proof = crate::engine::verification::artwork::verify_track(
            &current,
            target.sync_id,
            existing.cache_id,
            &cover,
            &options.trace.join("existing-artwork"),
            &engine.control(120),
            device::provider,
        )
        .await?;
        private_write(
            &options.trace.join("unchanged.json"),
            &serde_json::to_vec_pretty(&proof)?,
        )?;
        File::open(options.trace)?.sync_all()?;
        engine.emit(Event::CoverFinished(CoverResult::Unchanged))?;
        return Ok(CoverResult::Unchanged);
    }
    let preserved_album = if let Some(album) = options.preserve_album {
        ensure!(
            target.album_sync_id == album,
            "protected album identity changed after planning"
        );
        Some(crate::engine::verification::artwork::verify_album_original(
            &current,album,&cover,&options.trace.join("preserved-album"),
            &engine.control(120),
            device::provider,
        ).await.context("add-only cover refused: existing album image could not be proven identical; verified audio is retained")?)
    } else {
        None
    };
    let cache = native_artwork::allocate_cache_id(&current.catalog, artwork_db, &journal)?;
    let operation = artwork_update::prepare(
        &current.catalog,
        target.sync_id,
        cache,
        &cover,
        SystemTime::now(),
    )?;
    let possible_recent_resets =
        artwork_verification::possible_recent_play_count_resets(&current.catalog);
    let mut input = serde_json::json!({"schema":"rustunes.artwork-input","device_sha256":hex(&identity.device_digest()),
        "library_sha256":hex(&identity.library_digest()),"current_state_receipt_sha256":hex(&current.receipt_sha256),
        "track_id":format!("{:016x}",target.sync_id),"album_id":format!("{:016x}",target.album_sync_id),"cache_id":cache,
        "desired_source_sha256":hex(&desired.full_file_sha256),"transferred_audio_sha256":hex(&binding.content.source.full_file_sha256),
        "cover_sha256":hex(&cover.sha256),"cover_bytes":cover.bytes.len(),"operation_sha256":hex(&Sha256::digest(&operation)),
        "media_revision":current.catalog.persisted_media_anchor()+1,"audio_retransfer":false,"phone_deletions":0,
        "artwork_verification":"exact_track_source_and_decoded_derivative",
        "album_policy":if preserved_album.is_some(){"preserve_existing_original_bytes"}else{"native_representative_new_or_prior_cache"},
        "preserved_album_original":preserved_album,
        "possible_recent_play_count_import_resets":possible_recent_resets,
        "recent_play_count_import_rule":artwork_verification::RECENT_PLAY_IMPORT_RULE});
    let possible_skip_resets =
        artwork_verification::possible_recent_skip_count_resets(&current.catalog);
    if !possible_skip_resets.is_empty() {
        input["possible_recent_skip_count_import_resets"] =
            serde_json::to_value(&possible_skip_resets)?;
        input["recent_count_import_rule"] =
            serde_json::json!(artwork_verification::RECENT_COUNT_IMPORT_RULE);
    }
    let bytes = serde_json::to_vec_pretty(&input)?;
    private_write(&options.trace.join("input.json"), &bytes)?;
    File::open(options.trace)?.sync_all()?;
    if !possible_skip_resets.is_empty() || !possible_recent_resets.is_empty() {
        engine.emit(Event::ArtworkRecentCountResets {
            plays: &possible_recent_resets,
            skips: &possible_skip_resets,
        })?;
    }
    if options.dry_run {
        let result = CoverResult::Planned {
            track_id: target.sync_id,
            cache_id: cache,
            bytes: cover.bytes.len(),
        };
        engine.emit(Event::CoverFinished(result))?;
        return Ok(result);
    }
    let input_hash = Sha256::digest(&bytes).into();
    let state = FreshArtworkState::new(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        artwork_db,
        &current.cdb,
        input_hash,
    )?;
    let mut content = binding.content.clone();
    content.artwork = Some(Artwork {
        cache_id: cache,
        bytes: cover.bytes.len() as u64,
        sha256: cover.sha256,
    });
    let track_id = content.track_pid;
    let attempt = journal.prepare(PreparedAttempt {
        kind: AttemptKind::Artwork,
        content,
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: current.catalog.persisted_media_anchor(),
        operation,
        input_receipt_sha256: input_hash,
    })?;
    engine.emit(Event::ArtworkStarted { track_id })?;
    let pending=native_artwork::execute(&mut journal,ArtworkUpdate{attempt,source:options.file,desired_source:&desired,state:&state,
        host_name:options.host_name,trace:&options.trace.join("transfer")},
        &engine.control(180),device::provider).await
        .context("artwork update did not reach independent verification; inspect the journal before retrying")?;
    let after_control = engine.control(120);
    let after = current::capture_with_artwork_controlled(
        &options.trace.join("after"),
        &identity,
        after_control.deadline,
        Arc::clone(&after_control.cancelled),
    )
    .await?;
    let catalog_proof = artwork_verification::verify_independent_representative(
        &current.catalog,
        &after.catalog,
        track_id,
        cache,
    )?;
    let file_proof = crate::engine::verification::artwork::verify_track(
        &after,
        track_id,
        cache,
        &cover,
        &options.trace.join("permanent-artwork"),
        &engine.control(120),
        device::provider,
    )
    .await?;
    let protected_album_after = if let Some(album) = options.preserve_album {
        Some(
            crate::engine::verification::artwork::verify_album_original(
                &after,
                album,
                &cover,
                &options.trace.join("preserved-album-after"),
                &engine.control(120),
                device::provider,
            )
            .await?,
        )
    } else {
        None
    };
    let receipt = serde_json::json!({"schema":"rustunes.verified-artwork","catalogue":catalog_proof,"permanent_files":file_proof,
        "preserved_album_original_after":protected_album_after,
        "transfer_receipt_sha256":hex(&pending.evidence.receipt_sha256),"after_state_receipt_sha256":hex(&after.receipt_sha256),
        "preserved_audio_sha256":hex(&pending.evidence.permanent_audio_sha256),"staged_jpeg_sha256":hex(&pending.evidence.staged_cover_sha256),
        "phone_deletions":0});
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    private_write(&options.trace.join("verified.json"), &bytes)?;
    File::open(options.trace)?.sync_all()?;
    journal.complete(
        pending.permit,
        CompletionEvidence {
            track_pid: track_id,
            path: binding.content.path,
            audio_bytes: binding.content.source.file_bytes,
            audio_file_sha256: pending.evidence.permanent_audio_sha256,
            artwork_sha256: Some(cover.sha256),
            media_anchor: after.catalog.persisted_media_anchor(),
            receipt_sha256: Sha256::digest(&bytes).into(),
        },
    )?;
    engine.emit(Event::CoverFinished(CoverResult::Verified))?;
    Ok(CoverResult::Verified)
}

/// Fail before any transfer when the required image decoder is unavailable.
pub fn require_decoder() -> Result<()> {
    let available = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            fs::metadata(directory.join("ffmpeg"))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    });
    ensure!(
        available,
        "FFmpeg is required for permanent artwork verification; install ffmpeg before syncing covers"
    );
    Ok(())
}
