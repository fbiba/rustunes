//! Replacement proposals for tracks this tool sent whose source audio has
//! since changed. The add-only plan skips such a source; `sync
//! --replace-changed` replaces the tracks whose replacement identity is
//! established (`execution/replace.rs`).
use super::{
    JournalEvidence,
    managed_source::{Held, MAX_PLAN, ManagedContext},
};
use crate::{
    engine::planning::source_collection::{SourceCollection, SourceTrack},
    engine::state::audio_index::{DeviceAudioIndex, fingerprint_json},
    engine::state::journal::{
        Binding, Identity, InsertBatchId,
        source_attribution::{self, replacement as ancestry},
    },
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::metadata::ProjectedFields;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

#[derive(Serialize)]
pub struct ManagedAudioReplacement {
    pub classification: &'static str,
    pub refusal: Option<String>,
    pub replacement_identity_established: bool,
    pub evidence: Option<Value>,
    pub unassociated_history: Vec<Value>,
    pub original_bytes_available: bool,
    // Always false: a proposal authorizes nothing; the replacement works
    // from fresh device evidence.
    pub execution_authorized: bool,
    pub native_receiver_qualified: bool,
    pub start_authority: bool,
    pub completion_authority: bool,
}
impl ManagedAudioReplacement {
    fn empty(classification: &'static str) -> Self {
        Self {
            classification,
            refusal: None,
            replacement_identity_established: false,
            evidence: None,
            unassociated_history: Vec::new(),
            original_bytes_available: false,
            execution_authorized: false,
            native_receiver_qualified: false,
            start_authority: false,
            completion_authority: false,
        }
    }
    pub(super) fn blocks(&self) -> bool {
        self.classification != "unassociated_history_limitation"
    }
}
fn hash(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

/// A track whose binding records its source path (a pull or a single
/// insertion) is associated through that path: as for a batch, the same
/// absolute path is the same source whatever selection reaches it now.
/// `changed` says whether the source's encoded audio differs from what the
/// phone holds; `unchanged_since_planning` reads it again.
fn recorded_association(
    context: &ManagedContext,
    recorded: &Path,
    binding: &Binding,
    desired_path: &Path,
    changed: bool,
    catalog: &Catalog,
    unchanged_since_planning: impl FnOnce() -> Result<bool>,
) -> Result<Option<ManagedAudioReplacement>> {
    if desired_path != recorded {
        return Ok(None);
    }
    let content = &binding.content;
    let result = (|| -> Result<ManagedAudioReplacement> {
        ensure!(changed, "encoded audio is unchanged; not audio replacement");
        ensure!(
            catalog
                .tracks()
                .iter()
                .any(|t| t.sync_id == content.track_pid),
            "current target missing"
        );
        let mut detail =
            ManagedAudioReplacement::empty("audio_replacement_proposed_execution_blocked");
        detail.replacement_identity_established = true;
        detail.evidence = Some(json!({"recorded_absolute_source_path":recorded,
            "immutable_original_content":{"track_id":format!("{:016x}",content.track_pid),"path":content.path,
                "collection_sha256":content.collection.map(|s|hex(&s)),
                "artwork":content.artwork.as_ref().map(|a|json!({"cache_id":a.cache_id,"bytes":a.bytes,"sha256":hex(&a.sha256)}))},
            "audio_equality":"encoded_audio_differs_not_decoded_pcm_comparison"}));
        ensure!(
            unchanged_since_planning()?,
            "desired source changed before replacement proposal publication"
        );
        context.control.check()?;
        Ok(detail)
    })();
    Ok(Some(result.unwrap_or_else(|error| {
        let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
        detail.refusal = Some(format!("{error:#}"));
        detail.evidence = Some(json!({"recorded_absolute_source_path":recorded,
            "desired_absolute_source_path":desired_path}));
        detail
    })))
}

fn mpeg_fingerprint(source: &music_protocol::media::fingerprint::TransferredSource) -> Value {
    let audio = source.audio.mpeg().map(|a| {
        json!({"codec":"mpeg1-layer3","sha256":hex(&source.audio.sha256),"frame_bytes_sha256":hex(&a.frame_bytes_sha256),
            "sample_rate":a.sample_rate,"channels":a.channels,"frame_count":a.frame_count,"audio_offset":a.audio_offset,
            "audio_bytes":a.audio_bytes,"trailing_bytes":a.trailing_bytes})
    }).or_else(|| source.audio.encoded().map(|e| {
        let mut audio = crate::engine::state::audio_index::codec_json(
            &crate::engine::state::audio_index::OtherCodec::Encoded(e.clone()),
        );
        audio["sha256"] = json!(hex(&source.audio.sha256));
        audio
    }));
    json!({"file_bytes":source.file_bytes,"full_file_sha256":hex(&source.full_file_sha256),"audio":audio})
}
