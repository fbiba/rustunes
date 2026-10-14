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

pub(crate) mod recent_counters;
pub(crate) use music_protocol::catalogue::sqlite::tables::values as sql_values;
pub(crate) mod catalogue_capture;
pub(crate) mod removal_sql;
pub(crate) mod sql_tables;
