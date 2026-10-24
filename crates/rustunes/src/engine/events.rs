//! Observations for frontends. Events never grant execution authority.
use super::{ApplyResult, CoverResult, PlanResult, SyncHolds, SyncResult};
use std::path::Path;

/// Observations never authorize a write. An observer error is propagated at
/// the reporting boundary; durable attempts and permits remain intact. Use the
/// explicit cancellation flag to request cancellation during bounded I/O.
pub enum Event<'a> {
    VerificationStarting,
    Deletion(super::DeletionEvent<'a>),
    /// `resume` reads the phone's current state to settle the unfinished
    /// operation.
    ReadingCurrentState,
    Observation(super::Observation<'a>),
    Device(crate::engine::execution::device::Event),
    RunEvidence(&'a Path),
    ReadingDeviceSnapshot,
    ReadingSavedSnapshot,
    InspectingSources {
        done: usize,
        total: usize,
    },
    ComparingSources,
    PlanSaved(&'a Path),
    PlanReady(&'a PlanResult),
    /// Nothing was sent: these refusals hold the sync.
    SyncBlocked(&'a SyncHolds),
    SyncFinished(&'a SyncResult),
    CheckingCover {
        number: usize,
        total: usize,
    },
    ApplyFinished(ApplyResult),
    AdditionStarted {
        number: usize,
        total: usize,
        source: &'a str,
    },
    AudioInsertionVerified,
    /// The batched additions are done; a fresh sync runs the `remaining`
    /// actions one per session.
    SyncContinuing {
        remaining: usize,
    },
    /// An insert batch was cut short and undone, for the given reason; a fresh
    /// sync adds the rest.
    SyncResuming(&'a str),
    /// Add-only left this new track's cover out of its batch: its album on
    /// the phone shows another image, which it keeps.
    CoverLeftOut(&'a str),
    /// This FLAC was not sent: converting it to ALAC failed or lost audio.
    FlacRefused {
        source: &'a str,
        reason: String,
    },
    /// FLACs are converted to ALAC in this private directory, one batch at a
    /// time.
    ConvertingFlacs {
        directory: &'a std::path::Path,
    },
    /// Insert batch `number` of `batches` begins with `tracks` new tracks;
    /// `inserted` of all `total` are on the phone. `bytes` is the size of
    /// every new track's source (a FLAC's for one sent converted). Its
    /// session first removes `removals` tracks the sync's removal left it.
    InsertBatchStarted {
        number: usize,
        batches: usize,
        tracks: usize,
        inserted: usize,
        total: usize,
        bytes: u64,
        removals: usize,
    },
    /// `sent` of a new track's `file_bytes` bytes of audio are on the phone.
    /// `source` names its file, relative to the source root, where known.
    Uploading {
        source: Option<&'a str>,
        track: u64,
        sent: u64,
        file_bytes: u64,
    },
    /// One insert batch verified; `inserted` of `total` new tracks are on the
    /// phone, and its session removed `removed` tracks.
    InsertBatchVerified {
        inserted: usize,
        total: usize,
        removed: usize,
    },
    /// The plan's metadata updates of `tracks` tracks fit in `sessions`
    /// metadata batches, scheduled against the planning snapshot.
    UpdatesScheduled {
        tracks: usize,
        sessions: usize,
    },
    /// The plan's metadata updates cannot all be sent; nothing was sent.
    UpdatesRefused(&'a str),
    /// Metadata batch `session` of about `sessions` verified.
    MetadataBatchVerified {
        session: usize,
        sessions: usize,
    },
    /// A replacement coordination entered its removing or adding phase.
    ReplacementPhase {
        coordination: i64,
        phase: &'static str,
    },
    /// Removal batch `number` of `batches` begins with `tracks` tracks;
    /// `removed` of all `total` are off the phone.
    RemovalBatchStarted {
        number: usize,
        batches: usize,
        tracks: usize,
        removed: usize,
        total: usize,
    },
    /// One removal batch of a replacement verified; `removed` of `total`.
    ReplacementRemoved {
        removed: usize,
        total: usize,
    },
    RefreshingBeforeSession,
    ArtworkRecentCountResets {
        plays: &'a [music_protocol::catalogue::artwork_verification::RecentPlayCountReset],
        skips: &'a [music_protocol::catalogue::artwork_verification::RecentSkipCountReset],
    },
    ArtworkStarted {
        track_id: u64,
    },
    CoverFinished(CoverResult),
    /// The journal was superseded or missing, and the earlier journal was
    /// carried forward from a fresh capture (`rustunes pull`, run first).
    JournalCarried {
        earlier: &'a Path,
        journal: &'a Path,
        carried: usize,
        matched: usize,
        unbound: usize,
        gone: usize,
        /// Phone covers read back as their binding's image, and those that
        /// could not be (the next sync sends them again).
        covers_proven: usize,
        covers_unproven: usize,
        /// Work the earlier journal left unfinished, which this build
        /// cannot resume.
        unfinished: &'a [String],
    },
    /// `pull --tracks` is copying phone track `number` of `total`, `bytes`
    /// long, after `bytes_done` of all `bytes_total`.
    PullingTrack {
        number: usize,
        total: usize,
        track_id: u64,
        title: &'a str,
        artist: &'a str,
        bytes: u64,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// `pull --tracks` has handled all `total` tracks it set out to copy.
    TracksPulled {
        total: usize,
        bytes_total: u64,
    },
}
