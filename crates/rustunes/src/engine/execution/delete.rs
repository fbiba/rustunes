//! Frozen removal selection, removed in verified batches (one native session
//! each, see `removal_batch`). Explicit deletion selects named tracks; prune
//! uses full-source ownership rules; sync and `plan --remove-missing` select
//! the tracks this tool inserted whose source file is gone from the selection, by
//! the source path the journal pins; Clear selects every track under one
//! coordination that closes only when the phone's catalogue is empty: once no
//! track is left, it deletes every ordinary playlist in a last batch. Smart,
//! folder and phone-made playlists stay.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    num::NonZeroU64,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, atomic::Ordering},
    time::SystemTime,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::{
    engine::execution::device,
    engine::planning::additive::JournalEvidence,
    engine::planning::removal::{self, Decision, Origin, Request},
    engine::planning::source_collection::{self, Selection, SelectionOptions, SourceCollection},
    engine::state::audio_index::Absent,
    engine::state::journal::{
        BatchState, CoordinationId, CoordinationKind, Identity, Journal, PlaylistTarget,
    },
    engine::state::layout,
    engine::state::retained_files as io,
    engine::state::snapshot,
    engine::verification::audio_capture,
    engine::{
        execution::{insert_batch, removal_batch},
        planning::change::MAX_BATCH,
        verification::removal_sql,
    },
};
use music_protocol::catalogue::mutation;
use music_protocol::protocol::trace::private_write;

pub struct Options {
    /// Save an exact removal proposal without changing the phone or journal.
    pub dry_run: bool,
    /// Reuse a complete snapshot; execution rechecks current device state.
    pub snapshot: Option<PathBuf>,
    /// Existing per-device journal. Defaults to this tool's private state.
    pub journal: Option<PathBuf>,
    pub state_dir: PathBuf,
    /// Also save the full selection and disclosed side effects to this new file.
    pub output: Option<PathBuf>,
    pub host_name: String,
    /// Maximum seconds for each complete audio verification pass.
    pub seconds: u64,
    /// Tracks removed per native session.
    pub batch_size: usize,
    /// A sync follows (`Missing`): the removals that do not fill a batch are
    /// left to it (`Outcome::Completed::left`), and its first insert batch
    /// removes them in its session before its new tracks, as Apple's host
    /// sends a sync. They are all sent here when an ordinary playlist holds
    /// one: such a session removes no playlist member.
    pub leave_for_sync: bool,
    /// A saved plan's removals (`apply`): the selection must be exactly its
    /// tracks, or nothing is sent.
    pub pinned: Option<crate::engine::ChangePins>,
}

/// A nonzero catalogue track identifier, parsed from exactly 16 ASCII hex digits.
/// Parsing accepts either letter case; display uses 16 lowercase digits.
/// This identifies a track and grants no deletion authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(NonZeroU64);

impl TrackId {
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// A malformed or zero catalogue track identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseTrackIdError {
    InvalidFormat,
    Zero,
}

impl fmt::Display for ParseTrackIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidFormat => "track IDs require exactly 16 hexadecimal digits",
            Self::Zero => "zero or duplicate track ID",
        })
    }
}
impl std::error::Error for ParseTrackIdError {}

impl TryFrom<u64> for TrackId {
    type Error = ParseTrackIdError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(ParseTrackIdError::Zero)
    }
}

impl FromStr for TrackId {
    type Err = ParseTrackIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 16 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(ParseTrackIdError::InvalidFormat);
        }
        u64::from_str_radix(value, 16)
            .map_err(|_| ParseTrackIdError::InvalidFormat)?
            .try_into()
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.get())
    }
}

/// Supported removal requests.
pub enum Mode {
    /// Select validated nonzero track IDs; empty or duplicate selections fail.
    Delete(Vec<TrackId>),
    /// Remove owned tracks absent from this source directory, with its
    /// subfolders when `recursive`.
    Prune { source: PathBuf, recursive: bool },
    /// Remove the tracks this tool inserted whose source file is gone from
    /// this directory's selection (sync and `plan --remove-missing`).
    Missing {
        source: PathBuf,
        selection: SelectionOptions,
    },
    /// Remove every track. An interrupted Clear continues under its Active
    /// coordination when run again.
    Clear,
}

/// Frontends choose their own exit status from the supported removal outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Blocked,
    /// `sync`'s removals of `tracks` wait: a source this run cannot read may
    /// be one of theirs, moved. Nothing was removed.
    Held {
        tracks: usize,
    },
    Cancelled,
    Preview,
    Completed {
        removed: usize,
        deleted_playlists: usize,
        /// Tracks left to the sync that follows (`Options::leave_for_sync`).
        left: Vec<u64>,
    },
}
impl Outcome {
    pub fn ready(&self) -> bool {
        matches!(self, Self::Preview | Self::Completed { .. })
    }
}

/// A confirmation is requested only for an immutable, fully supported selection.
/// It never substitutes for per-operation qualification or a Started permit.
pub struct Confirmation<'a> {
    pub count: usize,
    /// Ordinary playlists a Clear deletes once its tracks are gone.
    pub playlists: usize,
    pub selection: &'a Path,
}
pub enum Event<'a> {
    InspectingSources {
        done: usize,
        total: usize,
    },
    Evidence(&'a Path),
    Selection {
        descriptor: &'a serde_json::Value,
        path: &'a Path,
    },
    Blocked,
    /// Every removal of `tracks` waits for these unreadable `sources`.
    Held {
        tracks: usize,
        sources: &'a [String],
    },
    DryRunComplete,
    Cancelled,
    Step {
        number: usize,
        total: usize,
    },
    /// An earlier run's unfinished removal batch, settled before selecting.
    Settled {
        batch: i64,
        state: &'static str,
    },
    Completed {
        removed: usize,
        deleted_playlists: usize,
        /// Removals left to the sync that follows.
        left: usize,
        evidence: Option<&'a Path>,
    },
}

pub async fn run(
    o: Options,
    mode: Mode,
    selected_device: Option<&str>,
    engine: &crate::engine::Engine,
    confirm: &mut impl FnMut(Confirmation<'_>) -> Result<bool>,
) -> Result<Outcome> {
    engine.check()?;
    ensure!(
        (1..=1800).contains(&o.seconds),
        "removal verification seconds must be between 1 and 1800"
    );
    ensure!(
        (1..=MAX_BATCH).contains(&o.batch_size),
        "batch size must be 1..{MAX_BATCH}"
    );
    let mut ids = match &mode {
        Mode::Delete(ids) => selection_ids(ids)?,
        Mode::Prune { .. } | Mode::Clear | Mode::Missing { .. } => BTreeSet::new(),
    };
    if let Some(path) = &o.output {
        ensure!(!path.try_exists()?, "plan output already exists");
    }
    let sources = if let Mode::Prune { source, recursive } = &mode {
        let options = SelectionOptions {
            recursive: *recursive,
            ..Default::default()
        };
        let selection = source_collection::select(source, &options)?;
        Some(inspect(&selection, engine)?)
    } else {
        None
    };
    // Its files are read only once a source is found gone.
    let missing_selection = match &mode {
        Mode::Missing { source, selection } => {
            let selected = source_collection::select(source, selection)?;
            ensure!(
                selected.root() == selected.base_directory(),
                "removing the tracks whose source is gone needs the library directory, not a file"
            );
            Some(selected)
        }
        _ => None,
    };
    let state = o.state_dir;
    let run = layout::new_run(&state)?;
    engine.emit(crate::engine::Event::Deletion(Event::Evidence(&run)))?;
    let cancelled = Arc::clone(&engine.control(o.seconds).cancelled);
    // The CLI retains the process's ordinary interrupt handling. Durable
    // Prepared/Started boundaries survive interruption; the next run settles
    // them (below) and never retries an outstanding operation.
    // A removal the phone rolled back may have deleted a target's file and
    // left its row: such a file may be missing, and once the journal is known
    // it must be one an earlier removal took (`removal_batch::taken`).
    let absent = Absent {
        gone: true,
        ..Absent::NONE
    };
    // An interrupted session leaves rows without audio; once the journal is
    // known they must be its insert batch's targets, which settling removes.
    let first = Absent {
        any_unfilled: true,
        ..absent
    };
    let fresh = o.snapshot.is_none();
    let snapshot_path = if let Some(path) = o.snapshot {
        std::fs::canonicalize(path)?
    } else {
        let path = run.join("snapshot");
        audio_capture::capture_allowing(&path, first, &engine.control(o.seconds), device::provider)
            .await?;
        path
    };
    let mut saved = snapshot::open_allowing(&snapshot_path, first)?;
    let library = music_protocol::catalogue::identity::parse(&saved.raw_preferences)?;
    let identity = Identity::for_saved_device(saved.identity.device_identifier_sha256, &library)?;
    ensure_selected_device(selected_device, &library, &identity)?;
    let journal_path = match o.journal {
        Some(path) => std::path::absolute(path)?,
        None => {
            crate::engine::execution::pull::current_journal(
                &state,
                &saved,
                missing_selection.as_ref().map(|s| s.root()),
                missing_selection
                    .as_ref()
                    .is_some_and(|s| s.options().recursive),
                &run,
                &engine.control(o.seconds),
            )
            .await?
        }
    };
    removal_batch::only_taken(&saved, &removal_batch::taken(&journal_path, &identity)?)?;
    only_unfinished(saved.index.unfilled(), &journal_path, &identity)?;
    // A batch an earlier run left unfinished (interrupted, or its observation
    // failed) is settled first, as resume would; the selection then starts
    // from fresh state. A dry run changes nothing, so it names what settles it.
    if o.dry_run {
        refuse_unsettled(
            &journal_path,
            &identity,
            &snapshot_path,
            &engine.command_options(),
        )?;
    } else {
        if let Some(removed) =
            settle_unfinished(&journal_path, &identity, &o.host_name, engine, o.seconds).await?
        {
            ids.retain(|id| !removed.contains(id));
            if fresh {
                let path = run.join("snapshot-settled");
                audio_capture::capture_allowing(
                    &path,
                    absent,
                    &engine.control(o.seconds),
                    device::provider,
                )
                .await?;
                saved = snapshot::open_allowing(&path, absent)?;
                let observed = Identity::for_saved_device(
                    saved.identity.device_identifier_sha256,
                    &music_protocol::catalogue::identity::parse(&saved.raw_preferences)?,
                )?;
                ensure!(
                    observed.device_digest() == identity.device_digest()
                        && observed.library_digest() == identity.library_digest(),
                    "the connected phone or library changed while the earlier batch was settled"
                );
                removal_batch::only_taken(
                    &saved,
                    &removal_batch::taken(&journal_path, &identity)?,
                )?;
            }
            // Every named track was removed by the batch just settled.
            if matches!(mode, Mode::Delete(_)) && ids.is_empty() {
                engine.emit(crate::engine::Event::Deletion(Event::Completed {
                    removed: 0,
                    deleted_playlists: 0,
                    left: 0,
                    evidence: None,
                }))?;
                return Ok(Outcome::Completed {
                    removed: 0,
                    deleted_playlists: 0,
                    left: Vec::new(),
                });
            }
        }
    }
    // Rows without audio this run did not settle (a dry run, or an insert
    // batch only resume settles) stop it, naming what settles them.
    if !saved.index.unfilled().is_empty() {
        crate::engine::execution::replace::ensure_no_unfinished_attempt(
            &journal_path,
            &identity,
            &state,
            &snapshot_path,
            &engine.command_options(),
        )?;
        bail!(
            "the phone holds tracks without audio that no unfinished insertion explains; nothing was sent"
        );
    }
    let journal_pin = journal_path
        .try_exists()?
        .then(|| io::file_hash(&journal_path.join("journal.sqlite"), 64 << 20).map(|(_, h)| h))
        .transpose()?;
    let journal = journal_pin
        .is_some()
        .then(|| Journal::open_read_only(&journal_path, &identity))
        .transpose()?;
    if let Some(hash) = journal_pin {
        ensure!(
            io::file_hash(&journal_path.join("journal.sqlite"), 64 << 20)?.1 == hash,
            "journal changed while opening the selection baseline"
        );
    }
    let origins = match (&missing_selection, &journal) {
        (Some(selected), Some(journal)) => {
            missing_origins(journal, &identity, selected, &engine.control(o.seconds))?
        }
        _ => BTreeMap::new(),
    };
    let missing_sources = match &missing_selection {
        Some(selected) if origins.values().any(|o| matches!(o, Origin::Missing(_))) => {
            Some(inspect(selected, engine)?)
        }
        _ => None,
    };
    let request = match &mode {
        Mode::Delete(_) => Request::Delete { track_ids: &ids },
        Mode::Missing { .. } => Request::Missing {
            sources: missing_sources.as_ref(),
            origins: &origins,
        },
        Mode::Prune { .. } => Request::Prune {
            sources: sources.as_ref().context("prune source inspection absent")?,
        },
        Mode::Clear => Request::Clear,
    };
    let plan = removal::build(
        &saved.catalog,
        &saved.index,
        &saved.identity,
        journal.as_ref().map(|journal| JournalEvidence {
            journal,
            identity: &identity,
        }),
        request,
    )?;
    let selected: Vec<u64> = plan
        .proposed_removal_ids
        .iter()
        .map(|id| u64::from_str_radix(id, 16))
        .collect::<std::result::Result<BTreeSet<_>, _>>()?
        .into_iter()
        .collect();
    if let Some(pinned) = &o.pinned {
        pinned.check_removals(&selected.iter().copied().collect())?;
    }
    // Every selected row must fit the general removal model on this exact
    // snapshot; each batch checks it again against fresh state before sending.
    let control = engine.control(o.seconds);
    let sql = removal_batch::materialize(&saved, &run.join("selection-sql"), &control)?;
    let batch_size = o.batch_size.max(1);
    let (steps, batch_blockers) =
        removal_batch::qualify_selection(&saved, &sql, &selected, batch_size)?;
    // A Clear deletes the ordinary playlists once no track is left. What can
    // be checked while the tracks remain is checked now; the full checks run
    // against fresh state before that session.
    let playlists = match &mode {
        Mode::Clear => removal_sql::clear_playlists(&sql.tables)?,
        _ => removal_sql::ClearPlaylists::default(),
    };
    let mut playlist_blockers = Vec::new();
    if !playlists.delete.is_empty() {
        let pids: Vec<i64> = playlists.delete.iter().map(|(pid, _)| *pid).collect();
        let containers: Vec<u64> = pids.iter().map(|pid| *pid as u64).collect();
        if let Err(error) = removal_sql::qualify_clear(&sql.tables, &pids).and_then(|()| {
            mutation::delete_playlists(&saved.catalog, &containers, SystemTime::now())
                .map(drop)
                .with_context(|| {
                    format!(
                        "a clear deletes its {} ordinary playlists in one session",
                        containers.len()
                    )
                })
        }) {
            playlist_blockers.push(format!("{error:#}"));
        }
    }
    let entire_selection_supported = !plan.has_blockers()
        && steps.iter().all(|s| s["blockers"] == json!([]))
        && batch_blockers.is_empty()
        && playlist_blockers.is_empty();
    let selected_set: BTreeSet<_> = selected.iter().copied().collect();
    let resets: Vec<String> = saved
        .catalog
        .tracks()
        .iter()
        .filter(|t| {
            !selected_set.contains(&t.sync_id)
                && (t.playback.play_count_recent != 0 || t.playback.skip_count_recent != 0)
        })
        .map(|t| format!("{:016x}", t.sync_id))
        .collect();
    let mut descriptor = json!({"schema":"rustunes.removal-selection","event":"plan",
        "proposal":plan,"recent_counter_resets_on_survivors":resets,
        "execution_order":selected.iter().map(|id|format!("{id:016x}")).collect::<Vec<_>>(),
        "batch_size":batch_size,"batches":selected.len().div_ceil(batch_size),
        "batch_blockers":batch_blockers,
        "sql_sequence_preview":{"blockers":[],"steps":steps},
        "entire_selection_supported":entire_selection_supported,
        "scope":"selected local Music tracks only","new_tracks_added_later_are_not_selected":true,
        "audio_path_cleanup":"the phone removes each target's file shortly after its session; no host unlink",
        "container_memberships":"library-container and Playback History rows of removed tracks stay on the phone; ordinary playlists lose them and close up, and an emptied playlist stays",
        "collection_representatives":"another surviving member, or zero for an emptied group",
        "qualification":"each batch is checked again against a fresh capture, then verified against the complete removal model"});
    if let Some(selected) = &missing_selection {
        let options = selected.options();
        descriptor["missing_sources"] = json!({
            "root":selected.root(),
            "selection":{"recursive":options.recursive,
                "include":options.include,"exclude":options.exclude},
            "source_path_unknown":origins.iter().filter_map(|(track, origin)| match origin {
                Origin::Unknown(reason) => Some(json!({"track_id":format!("{track:016x}"),"reason":reason})),
                _ => None,
            }).collect::<Vec<_>>(),
            "rule":"a track this tool inserted is removed when the selection reaches the source path the journal pins and no file is there, unless its encoded audio is in the selection under another path; unowned and unattributed tracks stay",
        });
    }
    if matches!(mode, Mode::Clear) {
        let listed = |list: &[(i64, String)]| -> Vec<serde_json::Value> {
            list.iter()
                .map(|(pid, name)| json!({"container":format!("{:016x}", *pid as u64),"name":name}))
                .collect()
        };
        descriptor["playlists"] = json!({
            "deleted_after_tracks":listed(&playlists.delete),
            "kept":listed(&playlists.kept),
            "blockers":playlist_blockers,
            "rule":"once no track is left, every ordinary playlist is deleted in one verified batch; smart, folder and phone-made playlists stay",
        });
    }
    let selection_file = run.join("selection.json");
    let bytes = serde_json::to_vec_pretty(&descriptor)?;
    io::write(&selection_file, &bytes)?;
    if let Some(path) = &o.output {
        write_selection_output(path, &bytes)?;
    }
    engine.emit(crate::engine::Event::Deletion(Event::Selection {
        descriptor: &descriptor,
        path: &selection_file,
    }))?;
    if !entire_selection_supported {
        if matches!(mode, Mode::Missing { .. }) && plan.held_by_unreadable_sources() {
            let tracks = plan
                .tracks
                .iter()
                .filter(|row| row.decision == Decision::Blocked)
                .count();
            let sources = missing_sources.as_ref().map(unreadable).unwrap_or_default();
            engine.emit(crate::engine::Event::Deletion(Event::Held {
                tracks,
                sources: &sources,
            }))?;
            return Ok(Outcome::Held { tracks });
        }
        engine.emit(crate::engine::Event::Deletion(Event::Blocked))?;
        return Ok(Outcome::Blocked);
    }
    if o.dry_run {
        engine.emit(crate::engine::Event::Deletion(Event::DryRunComplete))?;
        return Ok(Outcome::Preview);
    }
    let active_clear = match &journal {
        Some(journal) if matches!(mode, Mode::Clear) => journal
            .active_coordination()?
            .filter(|c| c.kind == CoordinationKind::Clear)
            .map(|c| c.id),
        _ => None,
    };
    if selected.is_empty() && active_clear.is_none() && playlists.delete.is_empty() {
        engine.emit(crate::engine::Event::Deletion(Event::Completed {
            removed: 0,
            deleted_playlists: 0,
            left: 0,
            evidence: None,
        }))?;
        return Ok(Outcome::Completed {
            removed: 0,
            deleted_playlists: 0,
            left: Vec::new(),
        });
    }
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "removal cancelled before confirmation"
    );
    // Closing an emptied Clear removes nothing and needs no confirmation.
    if !(selected.is_empty() && playlists.delete.is_empty())
        && !confirm(Confirmation {
            count: selected.len(),
            playlists: playlists.delete.len(),
            selection: &selection_file,
        })?
    {
        engine.emit(crate::engine::Event::Deletion(Event::Cancelled))?;
        return Ok(Outcome::Cancelled);
    }
    if let Some(journal) = journal {
        ensure!(
            journal.outstanding_removal_batch()?.is_none(),
            "an earlier removal batch is unfinished; run resume first"
        );
        drop(journal);
        if let Some(hash) = journal_pin {
            ensure!(
                io::file_hash(&journal_path.join("journal.sqlite"), 64 << 20)?.1 == hash,
                "journal changed after the selection was made"
            );
        }
    } else {
        // Explicit deletion may record an unowned target.
        // Prune has already refused a missing journal in the planning layer.
        drop(Journal::create(&journal_path, &identity)?);
    }
    // A source put back since the selection keeps its track.
    for row in plan
        .tracks
        .iter()
        .filter(|row| row.decision == Decision::ProposedRemoval)
    {
        if let Some(path) = &row.missing_source_path {
            ensure!(
                matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "{path} is back since the selection; nothing was removed, and running the command again selects afresh"
            );
        }
    }
    let coordination = match mode {
        Mode::Clear => Some(clear_coordination(
            &journal_path,
            &identity,
            &bytes,
            &run,
            saved.catalog.persisted_media_anchor(),
        )?),
        _ => None,
    };
    // The sync captures the phone with every file present.
    let left = match matches!(mode, Mode::Missing { .. }) && o.leave_for_sync {
        true => left_for_sync(&selected, batch_size, |tail| {
            Ok(tail.iter().any(|t| saved.index.gone().contains(t))
                || removal_sql::in_playlists(&sql.tables, tail)?)
        })?,
        false => &[][..],
    };
    let sent = &selected[..selected.len() - left.len()];
    let batches = sent.len().div_ceil(batch_size);
    let mut settled = removal_batch::remove_all(
        &journal_path,
        &identity,
        sent,
        batch_size,
        coordination,
        &run,
        &o.host_name,
        || engine.control(o.seconds),
        {
            let mut number = 0;
            move |_, _, _| {
                number += 1;
                engine.emit(crate::engine::Event::Deletion(Event::Step {
                    number,
                    total: batches,
                }))
            }
        },
    )
    .await
    .with_context(|| {
        format!(
            "removal stopped; running the same command again settles an unfinished batch and continues; evidence: {}",
            run.display()
        )
    })?;
    if let Some(id) = coordination {
        let batch = finish_clear(
            &journal_path,
            &identity,
            id,
            &run,
            &playlists.delete,
            &o.host_name,
            || engine.control(o.seconds),
        )
        .await
        .with_context(|| {
            format!(
                "Clear did not finish; running clear again continues it; evidence: {}",
                run.display()
            )
        })?;
        settled.extend(batch);
    }
    let removed: usize = settled.iter().map(|b| b.removed).sum();
    let deleted_playlists: usize = settled.iter().map(|b| b.deleted_playlists).sum();
    io::write(
        &run.join("complete.json"),
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.removal-completion","selection_sha256":io::hex(&io::sha(&bytes)),
            "batches":settled.iter().map(|b| json!({"batch":b.batch.as_i64(),"state":b.state.as_str(),
                "removed":b.removed,"deleted_playlists":b.deleted_playlists,"evidence":b.evidence}))
                .collect::<Vec<_>>(),
            "removed":removed,"deleted_playlists":deleted_playlists,"journal":journal_path,
            "left_for_sync":left.iter().map(|t| format!("{t:016x}")).collect::<Vec<_>>(),
            "operation_replays":0,"host_unlinks":0,
        }))?,
    )?;
    engine.emit(crate::engine::Event::Deletion(Event::Completed {
        removed,
        deleted_playlists,
        left: left.len(),
        evidence: Some(&run),
    }))?;
    Ok(Outcome::Completed {
        removed,
        deleted_playlists,
        left: left.to_vec(),
    })
}

/// The removals a sync's removal leaves it: those that do not fill a batch,
/// which its first insert batch removes in its session before its new
/// tracks. None when `alone` says they go by themselves: an ordinary playlist
/// holds one, which such a session does not remove, or one's file is already
/// gone, which the sync's capture refuses.
fn left_for_sync(
    selected: &[u64],
    batch_size: usize,
    alone: impl FnOnce(&[u64]) -> Result<bool>,
) -> Result<&[u64]> {
    let tail = &selected[selected.len() / batch_size * batch_size..];
    Ok(match !tail.is_empty() && alone(tail)? {
        true => &[],
        false => tail,
    })
}

/// The Active Clear coordination, or a new one. Any other Active
/// coordination must be finished first.
fn clear_coordination(
    journal_path: &Path,
    identity: &Identity,
    selection: &[u8],
    run: &Path,
    anchor: u32,
) -> Result<CoordinationId> {
    let mut journal = Journal::open(journal_path, identity)?;
    // An interrupted Clear continues under its own coordination.
    match journal.active_coordination()? {
        Some(active) => {
            ensure!(
                active.kind == CoordinationKind::Clear,
                "coordination {} is Active; run the command that began it again to finish it first",
                active.id.as_i64()
            );
            Ok(active.id)
        }
        None => journal.begin_coordination(CoordinationKind::Clear, selection, run, anchor),
    }
}

/// A Clear closes only when a fresh capture shows no tracks, no indexed audio
/// and no ordinary playlist. Once the tracks are gone, the ordinary playlists
/// (all empty by then) are deleted in one verified batch under the
/// coordination. Only `confirmed` playlists (the selection, by ID and name)
/// are deleted; one that appeared since refuses, and running clear again
/// selects it. Emptied albums and artists stay on the phone, as after any
/// removal, and so do smart, folder and phone-made playlists. Returns the
/// playlist batch, if one ran.
async fn finish_clear(
    journal_path: &Path,
    identity: &Identity,
    id: CoordinationId,
    run: &Path,
    confirmed: &[(i64, String)],
    host_name: &str,
    control: impl Fn() -> crate::engine::execution::native_insert::Control,
) -> Result<Option<removal_batch::Settled>> {
    let mut batch = None;
    let mut attempt = 1;
    let (found, after) = loop {
        let directory = run.join(match attempt {
            1 => "clear-playlists".to_owned(),
            n => format!("clear-playlists-attempt-{n}"),
        });
        let before = removal_batch::capture_before(&directory, identity, &control()).await?;
        no_tracks_left(&before.snapshot, id)?;
        let found = removal_sql::clear_playlists(&before.sql.tables)?;
        let unconfirmed: Vec<String> = found
            .delete
            .iter()
            .filter(|(pid, _)| !confirmed.iter().any(|(c, _)| c == pid))
            .map(|(pid, name)| format!("{:016x} {name:?}", *pid as u64))
            .collect();
        ensure!(
            unconfirmed.is_empty(),
            "ordinary playlists appeared after the selection ({}); nothing was deleted, coordination {} stays Active and running clear again selects them",
            unconfirmed.join(", "),
            id.as_i64()
        );
        if found.delete.is_empty() {
            break (found, before);
        }
        // A confirmed playlist the phone no longer has needs nothing; a
        // renamed one refuses in `run_playlists`.
        let targets: Vec<PlaylistTarget> = confirmed
            .iter()
            .filter(|(pid, _)| found.delete.iter().any(|(f, _)| f == pid))
            .map(|(pid, name)| PlaylistTarget {
                container: *pid as u64,
                name: name.clone(),
            })
            .collect();
        let control = control();
        let mut journal = Journal::open(journal_path, identity)?;
        let result = removal_batch::run_playlists(
            &mut journal,
            identity,
            &before,
            &targets,
            id,
            host_name,
            &control,
        )
        .await;
        drop(journal);
        // Rolled back or never sent: once more from fresh state.
        match result {
            Ok(settled) if settled.state == BatchState::Verified => {
                batch = Some(settled);
                let after =
                    removal_batch::capture_before(&run.join("clear-final"), identity, &control)
                        .await?;
                break (found, after);
            }
            Ok(_) | Err(_)
                if attempt < removal_batch::BATCH_ATTEMPTS
                    && removal_batch::may_try_again(journal_path, identity, &control)? =>
            {
                attempt += 1
            }
            Ok(settled) => bail!(
                "playlist batch {} did not apply on the phone, after {attempt} attempts; coordination {} stays Active and running clear again continues it",
                settled.batch.as_i64(),
                id.as_i64()
            ),
            Err(error) => return Err(error),
        }
    };
    no_tracks_left(&after.snapshot, id)?;
    let left = removal_sql::clear_playlists(&after.sql.tables)?.delete;
    ensure!(
        left.is_empty(),
        "the phone still has {} ordinary playlists; coordination {} stays Active and running clear again continues it",
        left.len(),
        id.as_i64()
    );
    let outcome = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.clear-outcome","anchor":after.anchor(),
        "after_index_sha256":io::hex(&after.snapshot.index_receipt_sha256),"tracks":0,
        "playlists_deleted":Journal::open_read_only(journal_path, identity)?
            .coordination_deleted_playlists(id)?,
        "playlists_kept":found.kept.iter().map(|(pid, name)| json!({
            "container":format!("{:016x}", *pid as u64),"name":name,
        })).collect::<Vec<_>>(),
    }))?;
    io::write(&run.join("clear-outcome.json"), &outcome)?;
    let mut journal = Journal::open(journal_path, identity)?;
    let revision = journal.coordination(id)?.revision;
    journal.complete_coordination(id, revision, &outcome)?;
    Ok(batch)
}

/// Read every selected source, reporting progress.
fn inspect(selection: &Selection, engine: &crate::engine::Engine) -> Result<SourceCollection> {
    source_collection::inspect_with_progress(selection, |done, total| {
        if done == 0 || done == total || done % 25 == 0 {
            engine.emit(crate::engine::Event::Deletion(Event::InspectingSources {
                done,
                total,
            }))?;
        }
        Ok(())
    })
}

/// The selected sources whose audio cannot be identified: those inspection
/// could not read, and MP3s whose frames do not check.
fn unreadable(sources: &source_collection::SourceCollection) -> Vec<String> {
    let mut paths: Vec<_> = sources
        .failures
        .iter()
        .map(|failed| failed.relative_path.clone())
        .chain(
            sources
                .mp3_tracks
                .iter()
                .filter(|source| source.source.transferred_identity().is_err())
                .map(|source| source.relative_path.clone()),
        )
        .collect();
    paths.sort();
    paths
}

/// Each track this tool inserted, against the selection: the source path the
/// journal pins is gone, still there, out of the selection's reach, or not
/// known.
pub(crate) fn missing_origins(
    journal: &Journal,
    identity: &Identity,
    selected: &Selection,
    control: &crate::engine::execution::native_insert::Control,
) -> Result<BTreeMap<u64, Origin>> {
    let paths = match crate::engine::planning::additive::source_paths(journal, identity, control) {
        Ok(paths) => paths,
        // Without the run history no source can be named, and every track stays.
        Err(error) => {
            let reason = format!("{error:#}");
            return Ok(journal
                .active_verified_bindings()?
                .into_iter()
                .map(|b| (b.content.track_pid, Origin::Unknown(reason.clone())))
                .collect());
        }
    };
    paths
        .into_iter()
        .map(|(track, path)| {
            let origin = match path {
                Err(reason) => Origin::Unknown(reason),
                Ok(path)
                    if !source_collection::reaches(selected.root(), selected.options(), &path) =>
                {
                    Origin::Outside
                }
                Ok(path) => match std::fs::symlink_metadata(&path) {
                    Ok(_) => Origin::Present,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Origin::Missing(path)
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("checking source {}", path.display()));
                    }
                },
            };
            Ok((track, origin))
        })
        .collect()
}

/// Refuse a dry run while the journal's removal batch is unfinished, naming
/// the two ways to settle it: the same command without `--dry-run`
/// (`settle_unfinished`), or `resume` with `snapshot`, this run's. Each ends
/// with `connection` (`Engine::command_options`).
fn refuse_unsettled(
    journal_path: &Path,
    identity: &Identity,
    snapshot: &Path,
    connection: &str,
) -> Result<()> {
    if !journal_path.try_exists()? {
        return Ok(());
    }
    let Some(batch) =
        Journal::open_read_only(journal_path, identity)?.outstanding_removal_batch()?
    else {
        return Ok(());
    };
    use crate::engine::execution::replace::{quoted, session_or_removal};
    bail!(
        "{} from an earlier run is unfinished; the same command without --dry-run settles it first, or rustunes resume --snapshot {} --journal {} --trace-dir NEW_DIRECTORY{connection} settles it alone; either observes the phone and never resends",
        session_or_removal(&batch),
        quoted(&snapshot.to_string_lossy()),
        quoted(&journal_path.to_string_lossy()),
    )
}

/// Settle the journal's unfinished removal batch, if any, as resume would: a
/// Prepared batch was never sent and is cancelled; a Started one is observed
/// and settled, never resent. A session's removal half settles with its
/// insert half (`insert_batch::resume`). Returns the tracks it removed when
/// there was one.
async fn settle_unfinished(
    journal_path: &Path,
    identity: &Identity,
    host_name: &str,
    engine: &crate::engine::Engine,
    seconds: u64,
) -> Result<Option<BTreeSet<u64>>> {
    if !journal_path.try_exists()? {
        return Ok(None);
    }
    let Some(batch) =
        Journal::open_read_only(journal_path, identity)?.outstanding_removal_batch()?
    else {
        return Ok(None);
    };
    let id = batch.id.as_i64();
    if let Some(insert) = batch.with_insert_batch {
        let record = Journal::open_read_only(journal_path, identity)?.insert_batch(insert)?;
        insert_batch::resume(
            journal_path,
            identity,
            &record,
            host_name,
            &engine.control(seconds),
        )
        .await
        .with_context(|| {
            format!(
                "the session of removal batch {id} and insert batch {} from an earlier run is not settled; nothing was sent",
                insert.as_i64()
            )
        })?;
        let state = Journal::open_read_only(journal_path, identity)?
            .removal_batch(batch.id)?
            .state;
        engine.emit(crate::engine::Event::Deletion(Event::Settled {
            batch: id,
            state: state.as_str(),
        }))?;
        return Ok(Some(match state {
            BatchState::Verified => batch.targets.iter().map(|t| t.track).collect(),
            _ => BTreeSet::new(),
        }));
    }
    let mut journal = Journal::open(journal_path, identity)?;
    let state = match batch.state {
        BatchState::Prepared => {
            journal.cancel_removal_batch(batch.id)?;
            BatchState::Cancelled
        }
        // A batch of an earlier run: the phone may have been played since.
        _ => {
            removal_batch::settle(
                &mut journal,
                identity,
                &batch,
                crate::engine::recovery::retained::playback::Window::LaterRun,
                &engine.control(seconds),
            )
            .await
            .with_context(|| {
                format!("removal batch {id} from an earlier run is not settled; nothing was sent")
            })?
            .state
        }
    };
    engine.emit(crate::engine::Event::Deletion(Event::Settled {
        batch: id,
        state: state.as_str(),
    }))?;
    let removed = match state {
        BatchState::Verified => batch.targets.iter().map(|t| t.track).collect(),
        _ => BTreeSet::new(),
    };
    Ok(Some(removed))
}

/// Every row without audio is a target of the journal's unfinished insert
/// batch: an interrupted insertion left it.
fn only_unfinished(
    unfilled: &BTreeSet<u64>,
    journal_path: &Path,
    identity: &Identity,
) -> Result<()> {
    if unfilled.is_empty() {
        return Ok(());
    }
    let targets: BTreeSet<u64> = match journal_path.try_exists()? {
        true => Journal::open_read_only(journal_path, identity)?
            .outstanding_insert_batch()?
            .map(|b| {
                b.targets
                    .iter()
                    .map(|t| t.target.content.track_pid)
                    .collect()
            })
            .unwrap_or_default(),
        false => BTreeSet::new(),
    };
    if let Some(track) = unfilled.iter().find(|t| !targets.contains(t)) {
        bail!(
            "track {track:016x} has no audio on the phone, and no unfinished insertion of it was cut short; nothing was sent"
        );
    }
    Ok(())
}

fn no_tracks_left(snapshot: &snapshot::Snapshot, id: CoordinationId) -> Result<()> {
    let left = snapshot.catalog.tracks().len();
    ensure!(
        left == 0 && snapshot.index.tracks().is_empty() && snapshot.index.other_audio().is_empty(),
        "the phone still has {left} tracks; coordination {} stays Active and running clear again continues it",
        id.as_i64()
    );
    Ok(())
}

fn selection_ids(values: &[TrackId]) -> Result<BTreeSet<u64>> {
    ensure!(!values.is_empty(), "deletion selection is empty");
    let mut ids = BTreeSet::new();
    for value in values {
        ensure!(ids.insert(value.get()), "zero or duplicate track ID");
    }
    Ok(ids)
}
fn ensure_selected_device(
    selected: Option<&str>,
    library: &music_protocol::catalogue::identity::LibraryIdentity,
    expected: &Identity,
) -> Result<()> {
    if let Some(selected) = selected {
        let identity = Identity::new(selected, library)?;
        ensure!(
            identity.device_digest() == expected.device_digest()
                && identity.library_digest() == expected.library_digest(),
            "selected device differs from removal snapshot"
        );
    }
    Ok(())
}
fn write_selection_output(path: &Path, bytes: &[u8]) -> Result<()> {
    private_write(path, bytes)?;
    std::fs::File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A Prepared batch an earlier run never sent is cancelled before the
    /// next selection, without connecting; an idle or missing journal has
    /// nothing to settle. A run tries a batch again only when nothing is
    /// left outstanding and it was not cancelled.
    #[tokio::test]
    async fn an_earlier_prepared_batch_is_cancelled_before_selecting() {
        use crate::engine::state::journal::test_support::{Temp, identity};
        use crate::engine::state::journal::{PlaylistTarget, RemovalIntent};
        let engine = crate::engine::Engine::quiet(crate::engine::Config {
            device: crate::engine::DeviceConfig::default(),
            deadline: None,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let t = Temp::new();
        assert_eq!(
            settle_unfinished(&t.path(), &identity(), "test", &engine, 1)
                .await
                .unwrap(),
            None
        );
        let mut journal = Journal::create(&t.path(), &identity()).unwrap();
        let id = journal
            .prepare_removal_batch(&RemovalIntent {
                coordination: None,
                compensates: None,
                compensates_insert_batch: None,
                operation: vec![5; 40],
                base_anchor: 14,
                before_current: [1; 32],
                before_index: [2; 32],
                input_receipt: [3; 32],
                evidence: "/run/unsent".into(),
                targets: Vec::new(),
                playlists: vec![PlaylistTarget {
                    container: 3,
                    name: "CLI Removal Test 3".into(),
                }],
            })
            .unwrap();
        drop(journal);
        // A dry run settles nothing: it names both ways to settle the batch.
        let error = refuse_unsettled(&t.path(), &identity(), Path::new("/run/snap"), " --wifi")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(&format!(
                "removal batch {} from an earlier run is unfinished; the same command without --dry-run settles it first, or rustunes resume --snapshot /run/snap --journal ",
                id.as_i64()
            )) && error.contains(" --trace-dir NEW_DIRECTORY --wifi settles it alone"),
            "{error}"
        );
        // The deadline must outlast the test, or the control reads as stopped.
        let control = engine.control(600);
        assert!(!removal_batch::may_try_again(&t.path(), &identity(), &control).unwrap());
        assert_eq!(
            settle_unfinished(&t.path(), &identity(), "test", &engine, 1)
                .await
                .unwrap(),
            Some(BTreeSet::new())
        );
        assert!(removal_batch::may_try_again(&t.path(), &identity(), &control).unwrap());
        let stopped = crate::engine::Engine::quiet(crate::engine::Config {
            device: crate::engine::DeviceConfig::default(),
            deadline: None,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        });
        assert!(
            !removal_batch::may_try_again(&t.path(), &identity(), &stopped.control(600)).unwrap()
        );
        let journal = Journal::open_read_only(&t.path(), &identity()).unwrap();
        assert_eq!(
            journal.removal_batch(id).unwrap().state,
            BatchState::Cancelled
        );
        assert!(journal.outstanding_removal_batch().unwrap().is_none());
        drop(journal);
        refuse_unsettled(&t.path(), &identity(), Path::new("/run/snap"), "").unwrap();
        assert_eq!(
            settle_unfinished(&t.path(), &identity(), "test", &engine, 1)
                .await
                .unwrap(),
            None
        );
    }
    #[test]
    fn a_sync_is_left_the_removals_that_do_not_fill_a_batch() {
        let none = |_: &[u64]| Ok(false);
        assert_eq!(left_for_sync(&[1, 2, 3, 4, 5], 2, none).unwrap(), [5]);
        assert_eq!(left_for_sync(&[1, 2, 3], 5, none).unwrap(), [1, 2, 3]);
        assert!(left_for_sync(&[1, 2, 3, 4], 2, none).unwrap().is_empty());
        assert!(
            left_for_sync(&[1, 2, 3], 5, |tail| Ok(tail.contains(&2)))
                .unwrap()
                .is_empty()
        );
        // Nothing is asked of an empty tail.
        assert!(
            left_for_sync(&[], 5, |_| -> Result<bool> { unreachable!() })
                .unwrap()
                .is_empty()
        );
    }
    /// A Prepared session an earlier run never sent is cancelled, both
    /// halves, through its insert half, without connecting.
    #[tokio::test]
    async fn an_earlier_prepared_session_is_cancelled_with_both_halves() {
        use crate::engine::state::journal::test_support::{Temp, alac_content, identity};
        use crate::engine::state::journal::{
            InsertBatchState, InsertIntent, InsertTarget, RemovalIntent, RemovalTarget,
        };
        let engine = crate::engine::Engine::quiet(crate::engine::Config {
            device: crate::engine::DeviceConfig::default(),
            deadline: None,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let t = Temp::new();
        let evidence = t.0.join("session");
        std::fs::create_dir(&evidence).unwrap();
        let input = serde_json::to_vec(&json!({"members":[{
            "source_path":"/library/gone.m4a","album":"0000000000000011","artist":"0000000000000012",
        }]}))
        .unwrap();
        std::fs::write(evidence.join("input.json"), &input).unwrap();
        let mut journal = Journal::create(&t.path(), &identity()).unwrap();
        let ids = journal
            .prepare_session(
                &InsertIntent {
                    coordination: None,
                    operation: vec![5; 40],
                    base_anchor: 14,
                    before_current: [1; 32],
                    before_index: [2; 32],
                    input_receipt: removal_batch::sha(&input),
                    evidence: evidence.clone(),
                    targets: vec![InsertTarget {
                        content: alac_content(1),
                        new_album_pid: None,
                        new_artist_pid: None,
                        conversion: None,
                        source: None,
                    }],
                },
                &RemovalIntent {
                    coordination: None,
                    compensates: None,
                    compensates_insert_batch: None,
                    operation: vec![5; 40],
                    base_anchor: 14,
                    before_current: [1; 32],
                    before_index: [2; 32],
                    input_receipt: removal_batch::sha(&input),
                    evidence,
                    targets: vec![RemovalTarget {
                        track: 7,
                        item_pid: 7,
                        path: None,
                        file_bytes: None,
                        file_sha256: None,
                        binding: None,
                    }],
                    playlists: Vec::new(),
                },
            )
            .unwrap();
        drop(journal);
        assert_eq!(
            settle_unfinished(&t.path(), &identity(), "test", &engine, 1)
                .await
                .unwrap(),
            Some(BTreeSet::new())
        );
        let journal = Journal::open_read_only(&t.path(), &identity()).unwrap();
        assert_eq!(
            journal.removal_batch(ids.removal).unwrap().state,
            BatchState::Cancelled
        );
        assert_eq!(
            journal.insert_batch(ids.insert).unwrap().state,
            InsertBatchState::Cancelled
        );
        assert!(journal.outstanding_insert_batch().unwrap().is_none());
    }
    /// A row may lack audio only as a target of the unfinished insert batch.
    #[test]
    fn rows_without_audio_belong_to_the_unfinished_insert_batch() {
        use crate::engine::state::journal::test_support::{Temp, alac_content, identity};
        use crate::engine::state::journal::{InsertIntent, InsertTarget};
        let t = Temp::new();
        let target = alac_content(1).track_pid;
        let rows = |tracks: &[u64]| tracks.iter().copied().collect::<BTreeSet<_>>();
        only_unfinished(&rows(&[]), &t.path(), &identity()).unwrap();
        assert!(only_unfinished(&rows(&[target]), &t.path(), &identity()).is_err());
        let mut journal = Journal::create(&t.path(), &identity()).unwrap();
        let batch = journal
            .prepare_insert_batch(&InsertIntent {
                coordination: None,
                operation: vec![5; 40],
                base_anchor: 14,
                before_current: [1; 32],
                before_index: [2; 32],
                input_receipt: [3; 32],
                evidence: "/run/batch".into(),
                targets: vec![InsertTarget {
                    content: alac_content(1),
                    new_album_pid: None,
                    new_artist_pid: None,
                    conversion: None,
                    source: None,
                }],
            })
            .unwrap();
        journal.start_insert_batch(batch, [8; 32], [1; 21]).unwrap();
        drop(journal);
        only_unfinished(&rows(&[target]), &t.path(), &identity()).unwrap();
        let error = only_unfinished(&rows(&[target, 99]), &t.path(), &identity())
            .err()
            .expect("a row no batch explains was accepted");
        assert!(error.to_string().contains("0000000000000063"), "{error:#}");
    }
    /// A file may be missing only for a track an unapplied or outstanding
    /// removal batch named: its session may have deleted the file before
    /// the phone rolled it back.
    #[test]
    fn taken_files_belong_to_unapplied_or_outstanding_removals() {
        use crate::engine::state::journal::test_support::{Temp, identity};
        use crate::engine::state::journal::{RemovalIntent, RemovalTarget};
        let t = Temp::new();
        assert!(
            removal_batch::taken(&t.path(), &identity())
                .unwrap()
                .is_empty()
        );
        let intent = |track: u64| RemovalIntent {
            coordination: None,
            compensates: None,
            compensates_insert_batch: None,
            operation: vec![5; 40],
            base_anchor: 14,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: "/run/taken".into(),
            targets: vec![RemovalTarget {
                track,
                item_pid: track as i64,
                path: None,
                file_bytes: None,
                file_sha256: None,
                binding: None,
            }],
            playlists: Vec::new(),
        };
        let mut journal = Journal::create(&t.path(), &identity()).unwrap();
        let rolled_back = journal.prepare_removal_batch(&intent(7)).unwrap();
        journal
            .start_removal_batch(rolled_back, [8; 32], [1; 21])
            .unwrap();
        journal
            .mark_removal_batch_not_applied(rolled_back, [4; 32])
            .unwrap();
        let outstanding = journal.prepare_removal_batch(&intent(9)).unwrap();
        drop(journal);
        assert_eq!(
            removal_batch::taken(&t.path(), &identity()).unwrap(),
            BTreeSet::from([7, 9])
        );
        let mut journal = Journal::open(&t.path(), &identity()).unwrap();
        journal.cancel_removal_batch(outstanding).unwrap();
        drop(journal);
        assert_eq!(
            removal_batch::taken(&t.path(), &identity()).unwrap(),
            BTreeSet::from([7])
        );
    }
    #[tokio::test]
    async fn cancelled_request_never_allocates_evidence_or_asks_for_confirmation() {
        let engine = crate::engine::Engine::quiet(crate::engine::Config {
            device: crate::engine::DeviceConfig::default(),
            deadline: None,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        });
        let root =
            std::env::temp_dir().join(format!("cancelled-removal-seam-{}", std::process::id()));
        assert!(!root.exists());
        let result = run(
            Options {
                dry_run: false,
                snapshot: None,
                journal: None,
                state_dir: root.clone(),
                output: None,
                host_name: "offline".into(),
                seconds: 1,
                batch_size: 100,
                leave_for_sync: false,
                pinned: None,
            },
            Mode::Delete(vec!["0000000000000001".parse().unwrap()]),
            None,
            &engine,
            &mut |_| panic!("cancelled request must not ask for write confirmation"),
        )
        .await;
        assert!(result.is_err());
        assert!(!root.exists(), "cancelled request allocated evidence");
    }

    #[test]
    fn explicit_selection_rejects_aliases_duplicates_and_zero_before_discovery() {
        for values in [
            vec![],
            vec!["1"],
            vec!["0000000000000000"],
            vec!["000000000000000a", "000000000000000A"],
            vec!["../Music/track.m4a"],
        ] {
            let selection = values
                .into_iter()
                .map(str::parse)
                .collect::<Result<Vec<TrackId>, _>>()
                .map_err(anyhow::Error::from)
                .and_then(|ids| selection_ids(&ids));
            assert!(selection.is_err());
        }
        assert_eq!(
            selection_ids(&[
                "000000000000000a".parse().unwrap(),
                "000000000000000B".parse().unwrap(),
            ])
            .unwrap(),
            BTreeSet::from([10, 11])
        );
    }
    struct SourceFixture(std::path::PathBuf);
    impl SourceFixture {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::env::temp_dir().join(format!(
                "delete-prune-recheck-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            std::fs::write(
                path.join("valid.m4a"),
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/silence-alac.m4a"
                )),
            )
            .unwrap();
            Self(path)
        }
    }
    impl Drop for SourceFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn explicit_device_mismatch_refuses_saved_removal_scope_without_discovery() {
        let mut prefs = vec![0; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4] = 1;
        prefs[6] = 25;
        prefs[8] = 1;
        prefs[12] = 1;
        prefs[20] = 2;
        let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
        let identity = Identity::new("device-A", &library).unwrap();
        ensure_selected_device(None, &library, &identity).unwrap();
        ensure_selected_device(Some("device-A"), &library, &identity).unwrap();
        assert!(ensure_selected_device(Some("device-B"), &library, &identity).is_err());
    }
    #[test]
    fn selection_output_supports_bare_filename_and_ordinary_parent_without_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "RUSTUNES_SELECTION_OUTPUT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            write_selection_output(Path::new("proposal.json"), b"private proposal").unwrap();
            assert_eq!(std::fs::read("proposal.json").unwrap(), b"private proposal");
            assert_eq!(
                std::fs::metadata("proposal.json")
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert!(write_selection_output(Path::new("proposal.json"), b"overwrite").is_err());
            assert_eq!(std::fs::read("proposal.json").unwrap(), b"private proposal");
            return;
        }
        let fixture = SourceFixture::new();
        // Isolate cwd in a child process so other tests cannot observe changes.
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact",concat!(module_path!(), "::selection_output_supports_bare_filename_and_ordinary_parent_without_overwrite").split_once("::").unwrap().1,"--nocapture"])
            .env(CHILD,"1").current_dir(&fixture.0).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"));
        write_selection_output(&fixture.0.join("absolute.json"), b"also private").unwrap();
        assert_eq!(
            std::fs::metadata(fixture.0.join("absolute.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
