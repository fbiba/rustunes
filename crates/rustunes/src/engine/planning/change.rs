//! Saved plans for the syncs that remove tracks before adding: `--remove-missing`
//! and `--add-remove` remove the tracks this tool inserted whose source file is
//! gone, then sync; `--replace-changed` and `--replace-library` remove tracks
//! and add their sources again. A plan names every track it removes, why, and
//! the source that comes back in its place; the additions and updates of the
//! sync that follows; what the run costs; and everything that blocks it.
//!
//! The steps depend on each other. The removal goes first, and the sync after
//! it plans again from a fresh capture, since the removal changes the
//! catalogue; new tracks get their identities then, so the plan names none.
//! The plan is built from saved state only and grants nothing: `apply`
//! rebuilds it from the same snapshot, sources and journal and requires the
//! same bytes, then pins each step to it (`Pinned`). The removal must select
//! exactly its tracks and the sync after it must add and update exactly its
//! sources, each checked against fresh state before that step sends anything.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use super::saved_plan;
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;

pub const SCHEMA: &str = "rustunes.change-plan";
/// A session's operations by default, besides its `update_db_info`: all one
/// operation file holds, for every command that sends.
pub(crate) const DEFAULT_BATCH: usize = MAX_BATCH;
/// The largest `--batch-size`: a session's operations besides its
/// `update_db_info`, which one operation file holds.
pub(crate) const MAX_BATCH: usize = music_protocol::protocol::operations::MAX_OPERATIONS - 1;
const LOSSES: &str =
    "removed tracks lose their identities, play and skip counts, ratings and playlist memberships";
const DEPENDENCIES: &str = "the removal goes first; the sync after it plans again from a fresh capture and must add and update exactly these sources, and new tracks get their identities then";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Remove the tracks whose source file is gone, then sync.
    RemoveMissing,
    /// The same with an add-only sync (`--add-remove`).
    AddRemove,
    /// Remove the tracks whose source audio changed, or whose source cleared a
    /// field an edit cannot clear, then add their sources again.
    ReplaceChanged,
    /// Remove every track, then add the whole selection again.
    ReplaceLibrary,
}
impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RemoveMissing => "remove_missing",
            Self::AddRemove => "add_remove",
            Self::ReplaceChanged => "replace_changed",
            Self::ReplaceLibrary => "replace_library",
        }
    }
    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "remove_missing" => Self::RemoveMissing,
            "add_remove" => Self::AddRemove,
            "replace_changed" => Self::ReplaceChanged,
            "replace_library" => Self::ReplaceLibrary,
            _ => anyhow::bail!("unknown saved plan action {text:?}"),
        })
    }
    /// Whether the sync after the removal keeps the audio and metadata of the
    /// tracks already on the phone.
    pub fn add_only(self) -> bool {
        !matches!(self, Self::RemoveMissing)
    }
    pub fn replaces(self) -> bool {
        matches!(self, Self::ReplaceChanged | Self::ReplaceLibrary)
    }
}

#[derive(Serialize)]
pub struct ChangePlan {
    pub schema: &'static str,
    pub action: &'static str,
    /// `--flac-to-alac`: the sync sends each new FLAC as an ALAC.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub flac_to_alac: bool,
    pub source_root: String,
    pub selection: Scope,
    pub device_sha256: String,
    pub library_sha256: String,
    pub database_sha256: String,
    pub media_anchor: u32,
    pub device_track_count: usize,
    pub removals: Vec<Removal>,
    /// Tracks this tool inserted whose source path the journal does not pin;
    /// they stay (`--remove-missing` only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_path_unknown: Vec<Value>,
    pub additions: Vec<Addition>,
    pub updates: Vec<Update>,
    /// Sources the sync leaves out without holding the rest
    /// (`additive::skip`), with why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// Covers of this tool's earlier additions the sync finishes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_covers: Vec<String>,
    /// Tracks on the phone the plan neither removes nor updates.
    pub untouched_tracks: usize,
    pub costs: Costs,
    pub order: [&'static str; 2],
    pub dependencies: &'static str,
    pub losses: &'static str,
    /// Empty when `apply` may run the plan.
    pub blockers: Vec<String>,
    pub fresh_state_required_before_execution: bool,
    pub execution_authorized_by_plan: bool,
}

#[derive(Serialize)]
pub struct Scope {
    pub recursive: bool,
    pub include: BTreeSet<String>,
    pub exclude: BTreeSet<String>,
    pub selected_files: usize,
}

#[derive(Serialize)]
pub struct Removal {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub plays: u32,
    pub skips: u32,
    pub file_bytes: Option<u64>,
    /// `source_gone`, `audio_changed` (or a cleared field) or `whole_library`.
    pub cause: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_source_path: Option<String>,
    /// The source (relative path) added again in its place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<String>,
    pub blockers: Vec<String>,
}

#[derive(Serialize)]
pub struct Addition {
    pub relative_path: String,
    pub file_bytes: Option<u64>,
    /// The bytes the sync sends.
    pub source_file_sha256: Option<String>,
    pub codec: &'static str,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub has_cover: bool,
    /// Why a source that holds pictures goes without a cover.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_left_out: Option<String>,
    /// The removed track it takes the place of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

#[derive(Serialize)]
pub struct Update {
    pub relative_path: String,
    /// The sync plan's decision for the row, exactly.
    pub decision: Value,
}

#[derive(Serialize)]
pub struct Skipped {
    pub relative_path: String,
    pub code: &'static str,
    /// Why, in plain words, and what to do.
    pub reason: String,
    /// The planner's full refusal.
    pub detail: String,
}

#[derive(Serialize)]
pub struct Costs {
    pub removal_sessions: usize,
    pub removal_batch_size: usize,
    pub upload_bytes: u64,
    pub plays_lost: u64,
    pub skips_lost: u64,
}

impl ChangePlan {
    pub fn ready(&self) -> bool {
        self.blockers.is_empty()
    }
    /// What `apply` holds each step to.
    pub fn pinned(&self) -> Result<Pinned> {
        let id = |text: &str| u64::from_str_radix(text, 16).context("planned track id");
        Ok(Pinned {
            removals: self
                .removals
                .iter()
                .map(|r| id(&r.track_id))
                .collect::<Result<_>>()?,
            replaced_by: self
                .removals
                .iter()
                .filter_map(|r| {
                    r.replaced_by
                        .as_ref()
                        .map(|s| Ok((id(&r.track_id)?, s.clone())))
                })
                .collect::<Result<_>>()?,
            additions: self
                .additions
                .iter()
                .map(|a| (a.relative_path.clone(), a.source_file_sha256.clone()))
                .collect(),
            updates: self
                .updates
                .iter()
                .map(|u| (u.relative_path.clone(), u.decision.clone()))
                .collect(),
        })
    }
}

/// The actions a saved plan holds each step to: the removal selects exactly
/// `removals` (for a changed-audio replacement, each replaced by its source),
/// and the sync after it adds exactly `additions` (each source path with the
/// SHA-256 of its file) and sends exactly `updates`, each with no conflict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pinned {
    pub removals: BTreeSet<u64>,
    pub replaced_by: BTreeMap<u64, String>,
    pub additions: BTreeMap<String, Option<String>>,
    pub updates: BTreeMap<String, Value>,
}

impl Pinned {
    /// The removal step's selection against fresh state, before it sends.
    pub(crate) fn check_removals(&self, fresh: &BTreeSet<u64>) -> Result<()> {
        ensure!(
            *fresh == self.removals,
            "the saved plan removes {} track(s) and the phone's current state selects {} ({}); nothing was sent; take a fresh snapshot and plan again",
            self.removals.len(),
            fresh.len(),
            differences(
                &self
                    .removals
                    .iter()
                    .map(|id| format!("{id:016x}"))
                    .collect(),
                &fresh.iter().map(|id| format!("{id:016x}")).collect()
            )
        );
        Ok(())
    }
    /// A changed-audio replacement's tracks and the sources that replace
    /// them, as (track id, source relative path), against fresh state.
    pub(crate) fn check_replacements(&self, fresh: &[(String, String)]) -> Result<()> {
        let planned: BTreeSet<String> = self
            .replaced_by
            .iter()
            .map(|(id, source)| format!("{id:016x} → {source}"))
            .collect();
        let fresh: BTreeSet<String> = fresh
            .iter()
            .map(|(id, source)| format!("{id} → {source}"))
            .collect();
        ensure!(
            planned == fresh,
            "the saved plan replaces {} track(s) and the phone's current state replaces {} ({}); nothing was sent; take a fresh snapshot and plan again",
            planned.len(),
            fresh.len(),
            differences(&planned, &fresh)
        );
        Ok(())
    }
}

/// Up to five entries on each side of two sets that differ.
fn differences(planned: &BTreeSet<String>, fresh: &BTreeSet<String>) -> String {
    let list = |set: Vec<&String>| {
        let mut text = set
            .iter()
            .take(5)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if set.len() > 5 {
            text.push_str(&format!(" and {} more", set.len() - 5));
        }
        text
    };
    let only_planned: Vec<_> = planned.difference(fresh).collect();
    let only_fresh: Vec<_> = fresh.difference(planned).collect();
    match (only_planned.is_empty(), only_fresh.is_empty()) {
        (true, true) => "the same".into(),
        (false, true) => format!("only in the plan: {}", list(only_planned)),
        (true, false) => format!("only now: {}", list(only_fresh)),
        (false, false) => format!(
            "only in the plan: {}; only now: {}",
            list(only_planned),
            list(only_fresh)
        ),
    }
}

pub struct Planned {
    pub plan: ChangePlan,
    pub serialized: Vec<u8>,
}

/// Whether the file at `path` is a saved change plan (not an additive one).
pub fn is_change_plan(path: &Path) -> Result<bool> {
    let saved = saved_plan::unique_value(&saved_plan::read_stable(path)?)
        .context("reading saved JSON plan with unique object keys")?;
    Ok(saved["schema"] == SCHEMA)
}

fn selectors(value: &Value) -> Result<BTreeSet<String>> {
    let values = value.as_array().context("missing source selectors")?;
    ensure!(values.len() <= 4096, "source selector bound exceeded");
    let mut result = BTreeSet::new();
    for value in values {
        ensure!(
            result.insert(value.as_str().context("selector must be text")?.to_owned()),
            "duplicate source selector"
        );
    }
    Ok(result)
}
