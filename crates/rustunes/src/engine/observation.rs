//! Factual progress from capture and independent verification phases.
use std::path::Path;

pub enum Observation<'a> {
    /// A capture is copying the phone's Music catalogue.
    CapturingCatalogue,
    RevalidatingSources {
        done: usize,
        total: usize,
    },
    CatalogueCaptureRetained(&'a Path),
    /// The phone's catalogue has a user_version rustunes was not tested
    /// with; every table and column it reads is present.
    UntestedCatalogueVersion(i64),
    RecapturingBeforeAudioIndex,
    RecapturingAfterAudioIndex,
    FreshCatalogue {
        tracks: usize,
        bytes: u64,
    },
    AudioFingerprinted {
        /// The track just fingerprinted.
        track: u64,
        completed: usize,
        total: usize,
        bytes: u64,
        other_codec: bool,
        /// The file's stat matched a cached fingerprint; nothing was copied.
        reused: bool,
    },
    AudioIndexRetained {
        tracks: usize,
        bytes: u64,
    },
    StateCaptureRetained(&'a Path),
    RecapturingState,
    RecapturingAfterDroppedConnection,
    VerifyingAudioBatch {
        number: usize,
        total: usize,
        tracks: usize,
        bytes: u64,
    },
    AudioBatchChecked {
        number: usize,
        total: usize,
        matched: usize,
        findings: usize,
    },
    VerifyingArtwork {
        number: usize,
        total: usize,
    },
    /// Covers whose files kept the stats of a verified readback, so its proof
    /// was reused; the rest are read.
    ReusedArtworkProofs {
        reused: usize,
        total: usize,
    },
    /// `read` of the `file_bytes` bytes of a track's audio are copied from
    /// the phone for its fingerprint.
    CopyingTrackAudio {
        track: u64,
        title: &'a str,
        artist: &'a str,
        read: u64,
        file_bytes: u64,
    },
    /// Source `number` of `total` is being read; the sources before it hold
    /// `bytes_done` of the selection's `bytes_total` bytes.
    InspectingSource {
        number: usize,
        total: usize,
        path: &'a str,
        file_bytes: u64,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// Every selected source was read: `total` files of `bytes_total` bytes.
    SourcesInspected {
        total: usize,
        bytes_total: u64,
    },
    /// A local cache could not be used; the run goes on without it.
    CacheUnavailable {
        cache: &'static str,
        error: String,
    },
    /// The track's original cover is exact, but the phone left its rendition
    /// at this edge empty.
    EmptyRendition {
        track: u64,
        edge: u16,
    },
}
