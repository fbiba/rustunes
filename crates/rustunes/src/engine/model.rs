//! Descriptive request/report data for alternative frontends. These exports
//! contain no journal, native execution, reservation, or completion authority.
//! Protocol facts remain the same types supplied by `music-protocol`.
pub use crate::engine::planning::additive::{
    ArtworkDecision, Decision as PlanDecision, ExcludedSource, ExistingMatch, FieldDifference,
    Group as PlannedGroup, ManagedAudioReplacement, MetadataComparisonScope, NativeComposer, Plan,
    ProjectedFields, SelectionScope, SourceDecision,
};
pub use crate::engine::planning::removal::{
    Decision as RemovalDecision, Plan as RemovalPlan, SourceEvidence, SourceScope,
    TrackDecision as RemovalTrackDecision,
};
pub use crate::engine::planning::source_collection::mp3::preparation::{
    ArtworkAssessment as MpegArtworkAssessment, Assessment as MpegInsertionAssessment,
    CodedTimingProposal as MpegCodedTimingProposal, CompatibilityIssue as MpegCompatibilityIssue,
    Metadata as MpegMetadata,
};
pub use crate::engine::planning::source_validation::{
    Artwork as SourceValidationArtwork, Audio as SourceValidationAudio, Decode as SourceDecode,
    Issue as SourceIssue, ProjectedTags as SourceValidationProjectedTags,
    Report as SourceValidationReport, Severity as SourceIssueSeverity,
    Tags as SourceValidationTags,
};
pub use crate::engine::verification::artwork::{
    PermanentFile as PermanentArtworkFile, VerifiedTrackArtworkBytes,
};
pub use crate::engine::verification::journal::{
    ArtworkStatus as VerificationArtworkStatus, MpegFrameReport,
    TrackReport as VerificationTrackReport,
};
pub use music_protocol::catalogue::artwork_verification::{
    RecentPlayCountReset, RecentSkipCountReset,
};
pub use music_protocol::media::fingerprint::SourceFingerprint;
pub use music_protocol::protocol::transfer::Attributes as FileAttributes;
