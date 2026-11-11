//! `rustunes init`: the first session of a phone no computer has synced.
//!
//! Such a phone has no library identity, no `db_info` row and no Media anchor,
//! and every other command refuses it. `init` gives it what Apple Devices
//! gives a phone it adopts with Music sync off (docs/10 §8), and
//! nothing more:
//!
//! - the prefs pair, with an identity rustunes creates and keeps per phone in
//!   the state directory, so a later Apple Devices connection sees another
//!   library instead of adopting the phone silently;
//! - one Delta session whose operation file holds `update_db_info` and the
//!   library playlist, hidden, named after the phone and empty.
//!
//! No track is sent. The first track goes in an ordinary `sync`, which can be
//! repeated; this session cannot, so it carries as little as possible.
//!
//! The order keeps the phone unwritten until it has accepted the session:
//! inspect and judge it (read-only), show the plan, connect ATC, and only at
//! Ready write anything. There, a retry first removes what its interrupted
//! attempt left: its staged operation pair and any prefs file it was writing
//! through `.tmp`, each only if it holds that attempt's bytes or the start of
//! them. The phone gives this Ready only with its Media anchor still 0, so it
//! applied no earlier attempt's pair. Then, under the Started permit of the
//! signed operation, `init` writes whichever of the prefs pair is missing and
//! the `Sync/Media` folders, and stages. A session that stops before
//! FinishedSyncingMetadata leaves the database untouched (docs/05 §5),
//! and a later `init` recognises what it wrote, whole or cut short, and
//! finishes.
use std::{
    future::Future,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use music_protocol::{
    catalogue::{identity::LibraryIdentity, mutation, prefs::Prefs, sqlite::LibraryState},
    protocol::{
        session::{self, Session},
        templates,
        trace::private_write,
        transfer::{METADATA, SIGNATURE, Transfer},
    },
};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout};

use crate::engine::{
    Engine,
    execution::device::{self, DeviceProvider},
    state::{
        device_files,
        init_record::{self, InitRecord},
        journal::device_binding,
        layout,
    },
    verification::{
        catalogue_capture,
        readiness::{self, Inspection, Readiness, SYNC_MEDIA},
    },
};

/// Captures of the database after the session at most, and the pause
/// before the first retry, doubled after each up to `READ_BACK_MAX_PAUSE`.
/// The phone may still be writing its database when it sends SyncFinished:
/// on iOS 27.2 it committed to it for a moment longer, so two captures
/// back to back both changed underneath.
const READ_BACK_ATTEMPTS: usize = 12;
const READ_BACK_FIRST_PAUSE: Duration = Duration::from_millis(250);
const READ_BACK_MAX_PAUSE: Duration = Duration::from_secs(4);

const FOLDERS: [&str; 4] = [
    "iTunes_Control",
    "iTunes_Control/iTunes",
    "iTunes_Control/Sync",
    SYNC_MEDIA,
];

pub struct Request<'a> {
    pub state: &'a Path,
    pub host_name: &'a str,
    pub dry_run: bool,
    /// Bound for each phase: the inspection, the session after the plan is
    /// confirmed, and reading its result back.
    pub seconds: u64,
}

/// What `init` will do, or did.
pub struct Plan {
    pub run: PathBuf,
    pub device_name: String,
    pub product: String,
    pub database_pid: u64,
    pub library_playlist_pid: u64,
    /// An earlier `init` began on this phone and did not finish.
    pub retry: bool,
    /// The files of the prefs pair written: both on a phone with none, only
    /// the plist after an attempt that stopped between the two, none when a
    /// retry finds both.
    pub write_prefs: Vec<&'static str>,
    pub create_folders: Vec<&'static str>,
    /// What an earlier attempt left part-written or staged, removed at Ready
    /// before anything is written.
    pub remove: Vec<&'static str>,
    /// The inspection report the plan rests on.
    pub inspection: Value,
}

pub enum Outcome {
    /// `--dry-run`: the plan, nothing written to the phone.
    Planned(Plan),
    /// `init` already finished for this phone.
    AlreadyInitialised { run: PathBuf },
    /// The first session finished and the phone shows its result.
    /// `playlist` is false when the phone did not keep the library playlist
    /// (it ignores host playlists while iCloud Music Library is on).
    Initialised { plan: Plan, playlist: bool },
}

pub(crate) async fn run(
    request: Request<'_>,
    engine: &Engine,
    confirm: &mut impl FnMut(&Plan) -> Result<bool>,
) -> Result<Outcome> {
    engine.check()?;
    ensure!(
        (1..=1800).contains(&request.seconds),
        "init seconds must be between 1 and 1800"
    );
    let control = engine.control(request.seconds);
    let provider = timeout(Duration::from_secs(15), device::provider())
        .await
        .context("timed out discovering the device")??;
    let run = layout::new_run(request.state)?;
    let inspection =
        readiness::inspect(&provider, &run.join("inspection"), control.deadline).await?;
    let device = device_binding(&provider.udid)?;
    let record = init_record::load(request.state, &device)?;
    let readiness = inspection.readiness(record.as_ref())?;
    private_write(
        &run.join("inspection.json"),
        &serde_json::to_vec_pretty(&inspection.report(&readiness))?,
    )?;
    match &readiness {
        Readiness::NeverSynced | Readiness::InitUnfinished => {}
        Readiness::Synced { initialised: true } => {
            return Ok(Outcome::AlreadyInitialised { run });
        }
        Readiness::Synced { initialised: false } => bail!(
            "this iPhone is already synced with a music library; rustunes syncs it as it is. \
             init is only for an iPhone no computer has synced"
        ),
        Readiness::Unclear(reasons) => bail!(
            "init refused: this iPhone is neither unsynced nor synced:\n- {}",
            reasons.join("\n- ")
        ),
    }
    let record = init_record::load_or_create(request.state, &device)?;
    let identity = record.prefs(request.host_name)?.identity()?;
    let staged = init_record::staged(request.state, &device)?;
    let work = work_on(&inspection, &record, request.host_name, staged.as_ref())?;
    let plan = Plan {
        device_name: inspection.device_name.clone(),
        product: format!(
            "{}, iOS {} ({})",
            inspection.product_type, inspection.product_version, inspection.build_version
        ),
        database_pid: record.database_pid,
        library_playlist_pid: record.library_playlist_pid,
        retry: readiness == Readiness::InitUnfinished,
        write_prefs: work.write_prefs.clone(),
        create_folders: work.create_folders.clone(),
        remove: work.leftovers.iter().map(|(path, _)| *path).collect(),
        inspection: inspection.report(&readiness),
        run,
    };
    private_write(
        &plan.run.join("plan.json"),
        &serde_json::to_vec_pretty(&plan_json(&plan))?,
    )?;
    if request.dry_run {
        return Ok(Outcome::Planned(plan));
    }
    if !confirm(&plan)? {
        bail!("init not confirmed; nothing was written to the iPhone");
    }
    let control = engine.control(request.seconds);
    // The plan may have waited on the user: judge the phone again.
    let again = readiness::inspect(
        &provider,
        &plan.run.join("inspection-confirmed"),
        control.deadline,
    )
    .await?;
    let again_work = work_on(&again, &record, request.host_name, staged.as_ref())?;
    ensure!(
        again.readiness(Some(&record))? == readiness && again_work.same(&work),
        "the iPhone changed while the plan was shown; nothing was written. Run init again"
    );
    // Its evidence is what the messages below point to: pruning keeps it
    // until a later attempt on this phone.
    init_record::save_run(request.state, &device, &plan.run)?;
    first_session(
        &provider,
        &plan,
        &record,
        &identity,
        &again_work,
        (request.state, request.host_name),
        &device,
        control.deadline,
        engine,
    )
    .await?;
    // The phone sent SyncFinished: from here on, no message may suggest that
    // init failed or should be repeated. Confirming the result is a phase
    // of its own, bounded by `--seconds` like the others.
    let deadline = engine.control(request.seconds).deadline;
    let shows = |after: &LibraryState| {
        after.media_anchor.as_deref() == Some("1")
            && after.db_info
                == Some((
                    record.database_pid as i64,
                    record.library_playlist_pid as i64,
                ))
    };
    let after = read_back(deadline, shows, |attempt| {
        let trace = plan.run.join(match attempt {
            1 => "after".to_owned(),
            n => format!("after-{n}"),
        });
        let provider = &provider;
        async move {
            engine.check()?;
            catalogue_capture::library_state(provider, &trace, deadline).await
        }
    })
    .await;
    let unconfirmed = |error: String| {
        outcome(
            &plan.run,
            json!({
                "schema": "rustunes.init-outcome",
                "complete": true,
                "confirmed": false,
                "error": error,
            }),
        )
    };
    let after = match after {
        Ok(ReadBack::Shown(after)) => after,
        Ok(ReadBack::NotShown { last, attempts }) => {
            let message = format!(
                "the first session finished, but after {attempts} readings of its database \
                 the iPhone does not show its result yet ({}); run rustunes device check in \
                 a moment. Evidence in {}",
                match &last {
                    Some(last) => format!(
                        "Media anchor {:?}, db_info {:?}",
                        last.media_anchor, last.db_info
                    ),
                    None => "no database".to_owned(),
                },
                plan.run.display()
            );
            unconfirmed(message.clone())?;
            bail!(message);
        }
        Err(error) => {
            unconfirmed(format!("{error:#}"))?;
            return Err(error.context(format!(
                "the first session finished (the iPhone sent SyncFinished), but reading its \
                 database back failed, so its result is not confirmed yet. Run rustunes device \
                 check: it should say the iPhone is initialised by rustunes. Evidence in {}",
                plan.run.display()
            )));
        }
    };
    let kept = after.containers.iter().any(|container| {
        container.container_pid == record.library_playlist_pid as i64
            && container.is_hidden
            && container.name == plan.device_name
    });
    outcome(
        &plan.run,
        json!({
            "schema": "rustunes.init-outcome",
            "complete": true,
            "confirmed": true,
            "media_anchor": after.media_anchor,
            "library_playlist_kept": kept,
        }),
    )?;
    Ok(Outcome::Initialised {
        plan,
        playlist: kept,
    })
}

fn outcome(run: &Path, value: Value) -> Result<()> {
    private_write(
        &run.join("outcome.json"),
        &serde_json::to_vec_pretty(&value)?,
    )
}

/// What reading the database back after the first session found.
enum ReadBack {
    /// A stable capture shows the session's result.
    Shown(LibraryState),
    /// No stable capture showed it: the last one (None when the phone had no
    /// database), after `attempts` captures.
    NotShown {
        last: Option<LibraryState>,
        attempts: usize,
    },
}

/// Read the phone's database after its first session until a stable capture
/// shows the session's result. The phone may still be writing it right
/// after SyncFinished, so a capture that saw the database change while
/// copying it, lost the connection, or showed no result yet is followed by
/// another after a pause, while `deadline` allows and at most
/// `READ_BACK_ATTEMPTS` times. Any other failure ends it at once. `attempt`
/// gets the attempt's number. Nothing but a stable capture that `shows`
/// the result returns `Shown`.
async fn read_back<F, Fut>(
    deadline: Instant,
    shows: impl Fn(&LibraryState) -> bool,
    mut attempt: F,
) -> Result<ReadBack>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<Option<LibraryState>>>,
{
    let started = Instant::now();
    let mut pause = READ_BACK_FIRST_PAUSE;
    let mut number = 1;
    loop {
        let result = attempt(number).await;
        let again = number < READ_BACK_ATTEMPTS && Instant::now() + pause < deadline;
        match result {
            Ok(Some(after)) if shows(&after) => return Ok(ReadBack::Shown(after)),
            Ok(last) if !again => {
                return Ok(ReadBack::NotShown {
                    last,
                    attempts: number,
                });
            }
            Ok(_) => {}
            Err(error)
                if again
                    && (error
                        .downcast_ref::<catalogue_capture::CatalogCaptureDrift>()
                        .is_some()
                        || music_protocol::protocol::afc::dropped_by_phone(&error)) => {}
            Err(error) => {
                return Err(error.context(format!(
                    "after {number} readings of the database in {:.1} s",
                    started.elapsed().as_secs_f64()
                )));
            }
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(READ_BACK_MAX_PAUSE);
        number += 1;
    }
}

/// What the first session does to a phone as an inspection found it.
/// No Debug: the prefs hold the library identity.
struct Work {
    /// The prefs pair to complete; None when both files are there.
    prefs: Option<Prefs>,
    /// The files of that pair that are absent, and so written.
    write_prefs: Vec<&'static str>,
    /// Each file an earlier attempt left, with the bytes it was writing there.
    leftovers: Vec<(&'static str, Vec<u8>)>,
    create_folders: Vec<&'static str>,
}

impl Work {
    /// Whether `other` writes, creates and removes the same, byte for byte.
    fn same(&self, other: &Work) -> bool {
        let prefs = |work: &Work| {
            work.prefs
                .as_ref()
                .map(|p| (p.raw.clone(), p.plist.clone()))
        };
        prefs(self) == prefs(other)
            && self.write_prefs == other.write_prefs
            && self.leftovers == other.leftovers
            && self.create_folders == other.create_folders
    }
}

/// The work on a phone `readiness` judged NeverSynced or InitUnfinished,
/// for `record`'s library. `staged` is the pair the last attempt saved.
fn work_on(
    inspection: &Inspection,
    record: &InitRecord,
    host_name: &str,
    staged: Option<&(Vec<u8>, [u8; 21])>,
) -> Result<Work> {
    let prefs = match &inspection.prefs {
        readiness::Prefs::Absent => Some(record.prefs(host_name)?),
        // Written under the name of the attempt that wrote iTunesPrefs, so
        // the plist matches it whatever this run's --host-name.
        readiness::Prefs::Partial { present, .. } => Some(
            inspection
                .own_prefs(record)
                .with_context(|| format!("{present} on the iPhone is not rustunes init's"))?,
        ),
        readiness::Prefs::Valid { .. } => None,
        readiness::Prefs::Invalid(error) => {
            bail!("the iPhone's library identity is unusable: {error}")
        }
    };
    let write_prefs = match prefs {
        Some(_) => [device_files::PREFS, device_files::PREFS_PLIST]
            .into_iter()
            .filter(|path| !inspection.present(path))
            .collect(),
        None => Vec::new(),
    };
    let mut leftovers = Vec::new();
    if inspection
        .staged
        .as_ref()
        .is_some_and(|names| !names.is_empty())
    {
        let (operation, signature) = staged
            .context("Sync/Media holds a staged pair, but no earlier init attempt saved one")?;
        leftovers.push((SIGNATURE, signature.to_vec()));
        leftovers.push((METADATA, operation.clone()));
    }
    for (path, writing) in inspection.leftover_temporaries(Some(record)) {
        let writing =
            writing.with_context(|| format!("{path} on the iPhone is not rustunes init's"))?;
        leftovers.push((path, writing));
    }
    Ok(Work {
        prefs,
        write_prefs,
        leftovers,
        create_folders: FOLDERS
            .into_iter()
            .filter(|folder| !inspection.present(folder))
            .collect(),
    })
}

/// Connect, and at Ready remove what an earlier attempt left, write the
/// missing prefs and folders and stage the operation pair, then finish and
/// wait for the phone's SyncFinished.
#[allow(clippy::too_many_arguments)]
async fn first_session(
    provider: &DeviceProvider,
    plan: &Plan,
    record: &InitRecord,
    identity: &LibraryIdentity,
    work: &Work,
    (state, host_name): (&Path, &str),
    device: &[u8; 32],
    deadline: Instant,
    engine: &Engine,
) -> Result<()> {
    let afc_trace = plan.run.join("afc");
    let atc_trace = plan.run.join("atc");
    for trace in [&afc_trace, &atc_trace] {
        std::fs::DirBuilder::new().mode(0o700).create(trace)?;
    }
    let mut afc = Transfer::connect(provider, &afc_trace, deadline).await?;
    let templates = templates::build(
        identity,
        host_name,
        0,
        templates::QUALIFIED_VOICE_MEMO_ANCHOR,
    )?;
    let prepared = session::prepare(
        templates.request,
        &templates.finished,
        identity.library_id(),
        0,
    )?;
    engine.check()?;
    let mut session = Session::connect(provider, prepared, &atc_trace, deadline).await?;
    let operation = mutation::first_session(
        record.database_pid,
        record.library_playlist_pid,
        &plan.device_name,
        SystemTime::now(),
    )?;
    let leftovers: Vec<(&str, &[u8])> = work
        .leftovers
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_slice()))
        .collect();
    session
        .finish_first_session(
            &mut afc,
            operation,
            &leftovers,
            work.prefs.as_ref(),
            |operation, signature| {
                init_record::save_staged(state, device, operation, signature)?;
                private_write(&plan.run.join("Sync_00000000.plist"), operation)
            },
            || engine.check(),
        )
        .await
}

pub fn plan_json(plan: &Plan) -> Value {
    json!({
        "schema": "rustunes.init-plan",
        "device": {"name": plan.device_name, "product": plan.product},
        "retry": plan.retry,
        "remove": plan.remove,
        "write_prefs": plan.write_prefs,
        "create_folders": plan.create_folders,
        "session": {
            "sync_type": "Delta",
            "media_anchor": {"requested": "0", "finished": "1"},
            "voice_memo_anchor": "1",
            "operations": [
                {"operation": "update_db_info", "pid": format!("{:016X}", plan.database_pid),
                 "primary_container_pid": format!("{:016X}", plan.library_playlist_pid)},
                {"operation": "insert_playlist", "pid": format!("{:016X}", plan.library_playlist_pid),
                 "name": plan.device_name, "is_hidden": true, "tracks": 0},
            ],
        },
        "inspection": plan.inspection,
    })
}

#[cfg(test)]
mod tests;
