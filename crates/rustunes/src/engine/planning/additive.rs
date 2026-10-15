//! Exact additive dry-run decisions from complete, identity-bound readback.
//! Planning reserves nothing and authorizes no phone mutation or deletion.
//! A proposed add still needs a durable journal preparation, fresh catalog/Ready,
//! and fresh AFC namespace checks. Rebind each sequential step without silently
//! replacing its proposed identities, fields, or permanent destination.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    engine::planning::collections::{CandidateIds, PlannedTrack, Planner},
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::planning::source_collection::{SourceCollection, SourceTrack},
    engine::planning::source_validation,
    engine::state::audio_index::{DeviceAudioIndex, IndexIdentity},
    engine::state::journal::{AttemptState, Binding, Identity, Journal},
};
use music_protocol::catalogue::sqlite::{Catalog, Track};

pub struct Candidate {
    pub ids: CandidateIds,
    pub path: String,
}

#[derive(Clone, Copy)]
pub struct JournalEvidence<'a> {
    pub journal: &'a Journal,
    pub identity: &'a Identity,
}

#[derive(Serialize)]
pub struct SelectionScope {
    pub base_directory: String,
    pub recursive: bool,
    pub include: BTreeSet<String>,
    pub exclude: BTreeSet<String>,
    pub selected_files: usize,
    pub selected_bytes: u64,
}

#[derive(Serialize)]
pub struct ExcludedSource {
    pub relative_path: String,
    pub reason: &'static str,
    pub descendants_unscanned: bool,
}
