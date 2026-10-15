//! `sync`'s one-track sessions: apply the additive plan it saved, exactly
//! reconstructed, one journaled track at a time.
//! Each accepted poststate becomes the next step's prestate. No deletion API is
//! reachable here, and interrupted Started attempts are never retried.
use std::{
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::{
    engine::execution::device,
    engine::execution::insert,
    engine::execution::native_insert::{self, Control, FreshState, Insertion},
    engine::planning::additive::Decision,
    engine::planning::collections::{CandidateIds, Planner},
    engine::planning::saved_plan,
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::state::current::{self, hex},
    engine::state::journal::{AttemptKind, CompletionEvidence, Content, Journal, PreparedAttempt},
    engine::{ApplyResult, Engine, Event},
};
use music_protocol::catalogue::insert_verification::{self, ExpectedInsert};
use music_protocol::protocol::templates;
use music_protocol::protocol::trace::private_write;

mod sequence;

pub struct Options<'a> {
    pub plan: &'a Path,
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub trace: &'a Path,
    pub host_name: &'a str,
    pub dry_run: bool,
}

struct ApplyState {
    journal: Journal,
    current: current::Current,
    current_directory: std::path::PathBuf,
}
struct StepContext<'a> {
    engine: &'a Engine,
    validated: &'a saved_plan::Validated,
    options: &'a Options<'a>,
    scope: [u8; 32],
    additions: usize,
    long_control: &'a Control,
}

// Retain exact transferred tag/container bytes once before input, intent or
// native dispatch. The transfer still reads and checks the caller's original
// path.
fn retain_alac_source(
    source_path: &Path,
    fingerprint: &music_protocol::media::fingerprint::SourceFingerprint,
    step: &Path,
    control: &Control,
) -> Result<()> {
    control.check()?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(step)?;
    let held_directory = directory.metadata()?;
    ensure!(
        held_directory.permissions().mode() & 0o777 == 0o700,
        "ALAC retention step is not private"
    );
    let retained = step.join("retained-source.m4a");
    let mut bound = native_insert::BoundSource::open(source_path, fingerprint)?;
    control.check()?;
    let bytes = bound.read_for_retention()?;
    control.check()?;
    private_write(&retained, &bytes)?;
    control.check()?;
    let retained_bound = native_insert::BoundSource::open(&retained, fingerprint)?;
    control.check()?;
    ensure!(
        music_protocol::media::fingerprint::read(&retained)? == *fingerprint,
        "source changed while retaining ALAC original bytes"
    );
    control.check()?;
    bound.check()?;
    finish_alac_retention(
        step,
        &directory,
        &held_directory,
        &bound,
        &retained_bound,
        control,
    )
}

// Finalize the same retention operation after the typed fingerprint read.
fn finish_alac_retention(
    step: &Path,
    directory: &File,
    held_directory: &fs::Metadata,
    bound: &native_insert::BoundSource,
    retained_bound: &native_insert::BoundSource,
    control: &Control,
) -> Result<()> {
    let retained = step.join("retained-source.m4a");
    let retained_metadata = fs::symlink_metadata(&retained)?;
    ensure!(
        retained_metadata.is_file()
            && retained_metadata.nlink() == 1
            && retained_metadata.permissions().mode() & 0o777 == 0o600,
        "retained ALAC original is not a private unaliased file"
    );
    let current_directory = fs::symlink_metadata(step)?;
    ensure!(
        current_directory.is_dir()
            && current_directory.dev() == held_directory.dev()
            && current_directory.ino() == held_directory.ino(),
        "ALAC retention step changed"
    );
    directory.sync_all()?;
    bound.check()?;
    retained_bound.check()?;
    control.check()?;
    Ok(())
}

// Keep this boundary ahead of trace creation and any device capture: it keeps
// an MP3 source out of the ALAC-only apply body.
/// Every MP3 addition goes in an insert batch. Apply sends one track per
/// session, so a real run refuses it; the preflight of `sync --dry-run` checks
/// it against the insert-batch profile.
fn ensure_supported_additions(
    rows: &[crate::engine::planning::additive::SourceDecision],
    sources: &crate::engine::planning::source_collection::SourceCollection,
    dry_run: bool,
) -> Result<()> {
    for row in rows {
        if !matches!(row.decision, Decision::Add { .. }) {
            continue;
        }
        if row.mp3.is_some()
            || row.mp3_insertion.is_some()
            || sources
                .mp3_tracks
                .iter()
                .any(|source| source.relative_path == row.relative_path)
        {
            let source = sources
                .mp3_tracks
                .iter()
                .find(|s| s.relative_path == row.relative_path)
                .context("planned MPEG source missing")?;
            ensure!(
                row.mp3.as_ref() == Some(&source.source),
                "planned MPEG inspection differs from selected source"
            );
            ensure!(
                dry_run,
                "{} goes in an insert batch, which sync sends; apply sends one track per session",
                row.relative_path
            );
            PreparedSource::read_expected(
                &sources.base_directory.join(&source.relative_path),
                &source.source,
            )?
            .ensure_batch_insertion_allowed()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod codec_gate_tests {
    use super::*;
    use crate::engine::planning::additive::{ArtworkDecision, Group, SourceDecision};
    #[test]
    fn mpeg_add_cannot_route_through_alac_apply_even_without_report_fields() {
        let path = std::env::temp_dir().join(format!(
            "apply-mpeg-gate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        fs::write(path.join("source.mp3"), [frame.clone(), frame].concat()).unwrap();
        let sources = crate::engine::planning::source_collection::inspect(
            &crate::engine::planning::source_collection::select(
                &path,
                &crate::engine::planning::source_collection::SelectionOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let group = || Group {
            id: "0000000000001234".into(),
            wire_id: "4660".into(),
            status: "new",
        };
        let mut row = SourceDecision {
            relative_path: "source.mp3".into(),
            managed_audio_replacement: None,
            source_file_bytes: None,
            source_file_sha256: None,
            encoded_audio_sha256: None,
            projected: None,
            mp3: None,
            mp3_insertion: None,
            flac_to_alac: false,
            validation_issues: Vec::new(),
            matches: Vec::new(),
            artwork: ArtworkDecision::NoSourceCoverPreserveExisting,
            native_composer: None,
            decision: Decision::Add {
                track_id: "0000000000001235".into(),
                wire_id: "4661".into(),
                album: group(),
                artist: group(),
                path: "iTunes_Control/Music/F00/TEST.mp3".into(),
                pending_collection_ids: Vec::new(),
                fresh_state_required_before_execution: true,
            },
        };
        assert!(ensure_supported_additions(std::slice::from_ref(&row), &sources, false).is_err());
        // An omitted or mislabelled codec field cannot bypass actual selection.
        row.mp3 = Some(sources.mp3_tracks[0].source.clone());
        assert!(ensure_supported_additions(std::slice::from_ref(&row), &sources, false).is_err());
        row.decision = Decision::Keep;
        assert!(ensure_supported_additions(std::slice::from_ref(&row), &sources, false).is_ok());
        fs::remove_dir_all(path).unwrap();
    }
}

fn verify_planned_composer(
    operation: &[u8],
    row: &crate::engine::planning::additive::SourceDecision,
    source: &music_protocol::media::metadata::Metadata,
) -> Result<()> {
    let plist = plist::Value::from_reader(std::io::Cursor::new(operation))?;
    let item = plist
        .as_dictionary()
        .and_then(|r| r.get("operations"))
        .and_then(plist::Value::as_array)
        .and_then(|ops| ops.last())
        .and_then(plist::Value::as_dictionary)
        .and_then(|r| r.get("item"))
        .and_then(plist::Value::as_dictionary)
        .context("generated insertion item missing")?;
    let source_composer = source.projected()?.composer;
    let (label, sort) = row
        .native_composer
        .as_ref()
        .map(|p| (Some(p.wire_label.as_str()), Some(p.wire_sort.as_str())))
        .unwrap_or((source_composer, source_composer));
    ensure!(
        item.get("composer").and_then(plist::Value::as_string) == label
            && item.get("sort_composer").and_then(plist::Value::as_string) == sort,
        "composer grouping changed after planning; no alternate metadata projection will be substituted"
    );
    Ok(())
}

fn insertion_state(current: &current::Current, input_hash: [u8; 32]) -> Result<FreshState> {
    let mut state = FreshState::new(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        input_hash,
    )?;
    state.bind_copied_cdb(&current.cdb)?;
    Ok(state)
}

struct OrdinaryInsert<'a> {
    engine: &'a Engine,
    current_directory: &'a Path,
    step: &'a Path,
    sources: &'a crate::engine::planning::source_collection::SourceCollection,
    source: &'a crate::engine::planning::source_collection::SourceTrack,
    source_path: &'a Path,
    prepared: &'a mut PreparedAttempt,
    input: serde_json::Value,
    row: &'a crate::engine::planning::additive::SourceDecision,
    selected: &'a crate::engine::planning::collections::PlannedTrack,
    timestamp: SystemTime,
    host_name: &'a str,
}

#[cfg(test)]
mod basic_tests;
