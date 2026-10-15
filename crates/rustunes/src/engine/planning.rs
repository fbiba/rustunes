//! Saved-only planning. The returned serialized bytes are exactly the bytes
//! persisted for later reconstruction; a plan never grants write authority.
use super::{Engine, Event, SelectionOptions};
use crate::{engine::state::journal, engine::state::snapshot};
use anyhow::{Result, ensure};
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;
use std::{fs::File, path::Path, time::Instant};

pub struct PlanRequest<'a> {
    pub source: &'a Path,
    pub snapshot: &'a Path,
    pub journal: Option<&'a Path>,
    pub output: Option<&'a Path>,
    pub selection: &'a SelectionOptions,
    pub add_only: bool,
    /// `--flac-to-alac`: plan FLAC additions as ALACs converted from them.
    pub flac_to_alac: bool,
}
pub struct PlanResult {
    pub add_only: bool,
    pub plan: additive::Plan,
    pub serialized: Vec<u8>,
    pub additions: usize,
    pub matched: usize,
    pub updates: usize,
    pub conflicts: usize,
    /// Sources the plan leaves out (`additive::skip`); the rest goes on
    /// without them.
    pub skipped: usize,
    pub source_exclusions: usize,
}
impl PlanResult {
    pub fn ready(&self) -> bool {
        !self.plan.has_conflicts()
    }
}

pub(crate) mod additive;
pub(crate) mod candidates;
pub(crate) mod collections;
pub(crate) mod removal;
pub(crate) mod saved_plan;
pub(crate) mod source_collection;
pub(crate) mod source_reads;
pub(crate) mod source_validation;
