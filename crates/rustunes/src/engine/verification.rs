//! Read-only current-state and retained-audio verification. This report does
//! not reconcile a Started attempt or reconstruct unretained source metadata.
use super::{Engine, Event};
use crate::{engine::execution::device, engine::state::current, engine::state::snapshot};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::identity;
use std::{fs, os::unix::fs::DirBuilderExt, path::Path, sync::Arc};

pub struct VerificationRequest<'a> {
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub trace: &'a Path,
    pub tracks: Vec<u64>,
    pub seconds: u64,
}

pub(super) async fn run(
    request: VerificationRequest<'_>,
    engine: &Engine,
) -> Result<journal::Report> {
    engine.check()?;
    ensure!(
        (1..=1800).contains(&request.seconds),
        "verification seconds must be between 1 and 1800"
    );
    let saved = snapshot::open(request.snapshot)?;
    let library = identity::parse(&saved.raw_preferences)?;
    let identity = crate::engine::state::journal::Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &library,
    )?;
    let journal =
        crate::engine::state::journal::Journal::open_read_only(request.journal, &identity)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(request.trace)
        .context("verification trace directory must be new")?;
    let control = engine.control(request.seconds);
    engine.emit(Event::VerificationStarting)?;
    let current = tokio::time::timeout_at(
        control.deadline,
        current::capture_controlled(
            &request.trace.join("state"),
            &identity,
            control.deadline,
            Arc::clone(&control.cancelled),
        ),
    )
    .await
    .context("verification state capture deadline")??;
    let selection = if request.tracks.is_empty() {
        journal::Selection::All
    } else {
        journal::Selection::TrackIds(request.tracks.into_iter().collect())
    };
    journal::verify(
        &journal,
        &current,
        selection,
        &request.trace.join("audio"),
        &journal::ReadControl {
            deadline: control.deadline,
            cancelled: control.cancelled,
        },
        device::provider,
    )
    .await
}

pub(crate) mod recent_counters;
pub(crate) use music_protocol::catalogue::sqlite::tables::values as sql_values;

pub(crate) mod artwork;
pub(crate) mod audio_capture;
pub(crate) mod catalogue_capture;
pub(crate) mod journal;
pub(crate) mod metadata_edit_sql;
pub(crate) mod removal_sql;
pub(crate) mod sql_tables;
