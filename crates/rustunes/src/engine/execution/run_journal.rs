//! Which journal a run uses, and what it did to get it (docs/07 §3.5).
//!
//! A command that reads the phone opens its run with a capture (`Captured`:
//! one this run took, or a saved snapshot) and then asks for its journal
//! (`RunJournal::resolve`): the one `--journal` names, or the phone's in the
//! state directory. When this build cannot use the phone's journal, or there
//! is none but an earlier one elsewhere in the state directory knows the
//! phone's tracks, the journal is carried forward first as a pull would
//! carry it; with none to carry, a phone `rustunes init` prepared that holds
//! no track gets the journal a pull would write for it, which binds nothing.
//! When the phone's journal binds a track the phone no longer holds (the
//! song was deleted on the phone), it is written again without that binding
//! first, as a pull would drop it, and is otherwise unchanged.
//! Either happens only from a capture this run took: a saved snapshot may
//! predate tracks the phone holds now. A dry run writes what it carries or
//! starts into its run directory and plans against it there, leaving the
//! phone's journal as it is, and that journal is never written
//! (`RunJournal::writable`).
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::engine::{
    execution::{device, native_insert::Control, pull},
    state::{
        audio_index::Absent,
        journal::{self, Identity, Journal},
        layout,
        snapshot::{self, Snapshot},
    },
    verification::audio_capture,
};

/// How a run came by its capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    /// This run captured the phone.
    ByThisRun,
    /// A saved snapshot (`--snapshot`), which may predate tracks the phone
    /// holds now.
    Saved,
}

/// The capture a run plans against, with the identity of its phone's
/// journal.
pub(crate) struct Captured {
    /// The capture's directory: the run's own, or the saved snapshot's,
    /// canonical.
    pub path: PathBuf,
    pub snapshot: Snapshot,
    pub identity: Identity,
    pub taken: Taken,
}

impl Captured {
    /// Capture the phone into `path`, a new directory, allowing the rows
    /// without audio `absent` names, under `control`.
    pub(crate) async fn take(path: PathBuf, absent: Absent<'_>, control: &Control) -> Result<Self> {
        Box::pin(audio_capture::capture_allowing(
            &path,
            absent,
            control,
            device::provider,
        ))
        .await?;
        Self::open(path, absent, Taken::ByThisRun)
    }

    /// Open the saved snapshot at `path`, allowing the rows without audio
    /// `absent` names.
    pub(crate) fn saved(path: &Path, absent: Absent<'_>) -> Result<Self> {
        let path = std::fs::canonicalize(path).context("opening supplied snapshot")?;
        Self::open(path, absent, Taken::Saved)
    }

    fn open(path: PathBuf, absent: Absent<'_>, taken: Taken) -> Result<Self> {
        let snapshot = snapshot::open_allowing(&path, absent)?;
        let identity = snapshot.journal_identity()?;
        Ok(Self {
            path,
            snapshot,
            identity,
            taken,
        })
    }
}

/// What a run asks for its journal.
pub(crate) struct Ask<'a> {
    /// The state directory, which holds the phone's journal.
    pub state: &'a Path,
    /// This run's directory: what a dry run carries or starts goes in it, as
    /// do the copies and cover traces of a carry.
    pub run: &'a Path,
    /// `--journal`: this journal and no other, as it is.
    pub explicit: Option<&'a Path>,
    pub dry_run: bool,
    /// The library directory phone tracks are matched to when the journal is
    /// carried forward, with its subfolders when `recursive`; `None` carries
    /// only the earlier journal's bindings.
    pub library: Option<&'a Path>,
    pub recursive: bool,
}

/// How a run came by its journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// `--journal` named it; nothing was checked or written.
    Explicit,
    /// The phone's journal, which this build opens.
    Current,
    /// The phone's journal, which is not there and was not started. The run
    /// goes on without one, and a real run creates it where it writes.
    Absent,
    /// Carried forward from an earlier journal, or written again without
    /// the tracks deleted on the phone. When not `installed` (a dry run), it
    /// is in the run directory and the phone's journal is as it was.
    Carried { installed: bool },
    /// Started, binding nothing, for a phone `rustunes init` prepared that
    /// holds no track. When not `installed` (a dry run), it is in the run
    /// directory and the phone's journal is still absent.
    Started { installed: bool },
}

/// The journal a run uses.
#[derive(Debug)]
pub(crate) struct RunJournal {
    path: PathBuf,
    resolution: Resolution,
}

impl RunJournal {
    /// The journal for `ask`, against `snapshot` (taken as `taken`). An
    /// explicit journal is used as it is. Otherwise the phone's journal is
    /// used when this build opens it; when it cannot (`Superseded`), or there
    /// is none but an earlier journal elsewhere in the state directory knows
    /// the phone's tracks, that journal is carried forward first (reading the
    /// library and the phone's covers), and with none to carry, a phone
    /// `rustunes init` prepared from the state directory that holds no track
    /// gets a journal binding nothing. A saved snapshot starts no journal,
    /// and refuses, naming `rustunes pull`, when one must be carried. A dry
    /// run writes what it carries or starts into `ask.run`. Progress and what
    /// it did go to the engine scope's observer.
    pub(crate) async fn resolve(
        ask: Ask<'_>,
        snapshot: &Snapshot,
        taken: Taken,
        control: &Control,
    ) -> Result<Self> {
        if let Some(path) = ask.explicit {
            return Ok(Self::at(std::path::absolute(path)?, Resolution::Explicit));
        }
        let identity = snapshot.journal_identity()?;
        let journal = layout::journal_path(
            ask.state,
            &snapshot.identity.device_identifier_sha256,
            &snapshot.identity.library_identity_sha256,
        )?;
        let exists = journal.try_exists()?;
        // As `pull --dry-run` does, a dry run plans against what it writes
        // in its run directory and leaves the phone's journal as it is.
        let target = if ask.dry_run {
            ask.run.join("journal")
        } else {
            journal.clone()
        };
        let installed = !ask.dry_run;
        if exists {
            match Journal::open_read_only(&journal, &identity) {
                Ok(current) => {
                    // Songs deleted on the phone are forgotten first, from a
                    // capture this run took; a saved one may predate them.
                    let deleted = match taken {
                        Taken::ByThisRun => {
                            pull::deleted_on_phone(&journal, &current, snapshot, ask.run)?
                        }
                        Taken::Saved => None,
                    };
                    let Some(found) = deleted else {
                        return Ok(Self::at(journal, Resolution::Current));
                    };
                    let tracks = pull::forget_deleted(snapshot, &current, found, &target, control)?;
                    drop(current);
                    crate::engine::observe(crate::engine::Event::DeletedOnPhone {
                        tracks,
                        journal: &target,
                        installed,
                    })?;
                    return Ok(Self::at(target, Resolution::Carried { installed }));
                }
                Err(error) if journal::is_superseded(&error) => {}
                Err(error) => return Err(error),
            }
        }
        let Some(found) = pull::find_earlier(ask.state, &journal, snapshot, ask.run)? else {
            if !exists && taken == Taken::ByThisRun && initialised_and_empty(ask.state, snapshot)? {
                pull::write_unbound(&target, snapshot)?;
                crate::engine::observe(crate::engine::Event::JournalStarted {
                    journal: &target,
                    installed,
                })?;
                return Ok(Self::at(target, Resolution::Started { installed }));
            }
            return Ok(Self::at(journal, Resolution::Absent));
        };
        ensure!(
            taken == Taken::ByThisRun,
            "this phone's journal must be carried forward before this command can use it, and only \
             from a fresh capture: a saved snapshot may predate tracks the phone holds now. Run \
             `rustunes pull`, or this command without --snapshot; nothing was changed"
        );
        let carry = pull::carry(
            snapshot,
            found,
            ask.library,
            ask.recursive,
            ask.run,
            &target,
            control,
        )
        .await?;
        crate::engine::observe(crate::engine::Event::JournalCarried {
            earlier: &carry.earlier,
            journal: &target,
            installed,
            carried: carry.carried,
            matched: carry.matched,
            unbound: carry.unbound,
            gone: carry.gone,
            covers_proven: carry.covers_proven,
            covers_unproven: carry.covers_unproven,
            unfinished: &carry.unfinished,
        })?;
        Ok(Self::at(target, Resolution::Carried { installed }))
    }

    fn at(path: PathBuf, resolution: Resolution) -> Self {
        Self { path, resolution }
    }

    /// Where the journal is, or would be created: absolute.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn resolution(&self) -> Resolution {
        self.resolution
    }

    /// Whether the journal's directory is there.
    pub(crate) fn exists(&self) -> Result<bool> {
        Ok(self.path.try_exists()?)
    }

    /// The journal, open for writing: opened when its `journal.sqlite` is
    /// there, otherwise created. A journal a dry run carried or started in
    /// its run directory is never written, so it refuses.
    pub(crate) fn writable(&self, identity: &Identity) -> Result<Journal> {
        ensure!(
            !matches!(
                self.resolution,
                Resolution::Carried { installed: false } | Resolution::Started { installed: false }
            ),
            "the journal a dry run planned against in its run directory is never written"
        );
        if self.path.join("journal.sqlite").try_exists()? {
            Journal::open(&self.path, identity)
        } else {
            Journal::create(&self.path, identity)
        }
    }
}

/// Whether `rustunes init` prepared this phone from `state` (its record there
/// owns the library identity and database the capture shows) and the phone
/// holds no track a sync made. A pull would bind nothing on it and write a
/// journal that only records this capture.
fn initialised_and_empty(state: &Path, saved: &Snapshot) -> Result<bool> {
    initialised_without_tracks(
        state,
        &saved.identity.device_identifier_sha256,
        &saved.raw_preferences,
        &saved.catalog,
        saved.index.tracks().is_empty() && saved.index.other_audio().is_empty(),
    )
}

/// `initialised_and_empty` for the device digest, the raw `iTunesPrefs`,
/// the catalogue and whether the audio index is empty of a capture.
fn initialised_without_tracks(
    state: &Path,
    device: &[u8; 32],
    raw_preferences: &[u8],
    catalog: &music_protocol::catalogue::sqlite::Catalog,
    index_empty: bool,
) -> Result<bool> {
    if !catalog.tracks().is_empty() || !index_empty {
        return Ok(false);
    }
    let Some(record) = crate::engine::state::init_record::load(state, device)? else {
        return Ok(false);
    };
    let library = music_protocol::catalogue::identity::parse(raw_preferences)?;
    Ok(record.owns(&library)? && catalog.database_pid() == record.database_pid)
}

#[cfg(test)]
mod tests;
