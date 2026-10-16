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

#[allow(clippy::too_many_arguments)] // Exact independent planner evidence inputs.
pub(super) fn describe(
    cache: &mut Option<Result<ManagedContext>>,
    journal: JournalEvidence<'_>,
    bindings: &[Binding],
    collection: &SourceCollection,
    source: &SourceTrack,
    desired: &ProjectedFields,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    control: Option<&crate::engine::execution::native_insert::Control>,
) -> Option<ManagedAudioReplacement> {
    let scope = crate::engine::planning::removal::collection_scope_sha256(collection).ok()?;
    let relevant = bindings
        .iter()
        .filter(|b| b.content.source.audio != source.fingerprint.audio)
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        return None;
    }
    let mut unassociated = Vec::new();
    let mut proposals = Vec::new();
    let context = match cache.get_or_insert_with(|| match control {
        Some(control) => {
            ManagedContext::open_with_control(journal.journal, journal.identity, control)
        }
        None => ManagedContext::open(journal.journal, journal.identity),
    }) {
        Ok(context) => context,
        Err(error) => {
            let mut detail = ManagedAudioReplacement::empty("unassociated_history_limitation");
            detail.unassociated_history.push(
                json!({"scope":"managed_history", "reason":format!("{error:#}"),
                "source_path_unknown":true,"associated_to_desired_row":false,
                "does_not_prove_source_absence":true,"baseline_decision_preserved":true}),
            );
            return Some(detail);
        }
    };
    for binding in relevant {
        let result = (|| -> Result<Option<ManagedAudioReplacement>> {
            context.control.check()?;
            ensure!(
                index.matches_transferred_source(
                    binding.content.track_pid,
                    &binding.content.path,
                    &binding.content.source
                ),
                "active binding/index lineage differs"
            );
            if let Some((batch, ordinal)) = context.batch_target(binding.content.track_pid)? {
                return batch_association(
                    context, batch, ordinal, binding, collection, source, desired, catalog,
                );
            }
            if let Some(recorded) = context.recorded_source(binding.content.track_pid)? {
                let desired_path = collection.base_directory.join(&source.relative_path);
                return recorded_association(
                    context,
                    &recorded,
                    binding,
                    &desired_path,
                    binding.content.source.audio.sha256 != source.fingerprint.audio.sha256,
                    catalog,
                    || {
                        Ok(music_protocol::media::fingerprint::read(&desired_path)?
                            == source.fingerprint
                            && ProjectedFields::from_source(
                                &music_protocol::media::metadata::read(&desired_path)?,
                            )? == *desired)
                    },
                );
            }
            let origin = ancestry::origin(&context.journal, binding, journal.identity)?;
            let digest = origin.input_digest();
            let (step, input) = context.find(digest)?;
            let claim = ancestry::path_claim(origin, journal.identity, &input)?;
            // Relative spelling can change when selecting a historical child
            // directory or file. Only the full checked historical absolute path
            // can establish whether this is the same source association.
            let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
            detail.evidence = Some(json!({"known_relative_path_claim":claim.relative_path(),
                "producer_input_sha256":hex(&hash(&input)),"selection_association_checked":false}));
            match checked(
                context,
                binding,
                journal.identity,
                collection,
                source,
                desired,
                catalog,
                index,
                &step,
                &input,
                digest,
                &claim,
            ) {
                Ok(checked) => Ok(checked),
                Err(error)
                    if binding.content.collection == Some(scope)
                        && claim.relative_path() == source.relative_path =>
                {
                    detail.refusal = Some(format!("{error:#}"));
                    Ok(Some(detail))
                }
                Err(error) => Err(error),
            }
        })();
        match result {
            Ok(Some(detail)) => proposals.push(detail),
            Ok(None) => {},
            Err(error) => unassociated.push(json!({"scope":binding.content.collection.map(|s|hex(&s)),
                "observed_track_id":format!("{:016x}",binding.content.track_pid),"reason":format!("{error:#}"),
                "source_path_unknown":true,"associated_to_desired_row":false,
                "does_not_prove_source_absence":true,"baseline_decision_preserved":true})),
        }
    }
    // Recheck the row's source once across the complete row, including all
    // refusal and other-path branches. The retained run history is rechecked
    // in full once, when the plan is published; a change there fails the plan.
    // The cached context holds the plan's one absolute Control.
    if let Err(error) = (|| -> Result<()> {
        ensure!(
            crate::engine::planning::source_reads::fingerprint(
                &collection.base_directory.join(&source.relative_path)
            )? == source.fingerprint
                && ProjectedFields::from_source(&music_protocol::media::metadata::read(
                    &collection.base_directory.join(&source.relative_path)
                )?)? == *desired,
            "desired source changed before replacement proposal publication"
        );
        context.control.check()
    })() {
        if let Some(detail) = proposals.first_mut() {
            *detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
            detail.refusal = Some(format!("{error:#}"));
        } else {
            unassociated.push(
                json!({"scope":"managed_history","reason":format!("{error:#}"),
                "source_path_unknown":true,"associated_to_desired_row":false,
                "does_not_prove_source_absence":true,"baseline_decision_preserved":true}),
            );
        }
    }
    if proposals.len() > 1 {
        let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
        detail.refusal =
            Some("ambiguous native historical bindings claim the same source path".into());
        detail.unassociated_history = unassociated;
        return Some(detail);
    }
    if let Some(mut detail) = proposals.pop() {
        detail.unassociated_history = unassociated;
        return Some(detail);
    }
    if unassociated.is_empty() {
        None
    } else {
        let mut detail = ManagedAudioReplacement::empty("unassociated_history_limitation");
        detail.unassociated_history = unassociated;
        Some(detail)
    }
}

/// The absolute source path of every active track this tool manages: the one
/// its binding records, else the one the journal pins through retained run
/// history (`historical_source`), or why it cannot be named.
pub(crate) fn source_paths(
    journal: &crate::engine::state::journal::Journal,
    identity: &Identity,
    control: &crate::engine::execution::native_insert::Control,
) -> Result<std::collections::BTreeMap<u64, std::result::Result<std::path::PathBuf, String>>> {
    let recorded = journal.recorded_sources()?;
    // Run history is read only for a binding that records no source.
    let mut context = None;
    let mut paths = std::collections::BTreeMap::new();
    for binding in journal.active_verified_bindings()? {
        let path = match recorded.get(&binding.content.track_pid) {
            Some(path) => Ok(path.clone()),
            None => {
                let context = match &mut context {
                    Some(context) => context,
                    None => context.insert(ManagedContext::open_with_control(
                        journal, identity, control,
                    )?),
                };
                historical_source(context, &binding, identity).map_err(|error| format!("{error:#}"))
            }
        };
        ensure!(
            paths.insert(binding.content.track_pid, path).is_none(),
            "duplicate journal track binding"
        );
    }
    if let Some(context) = context {
        context.recheck()?;
    }
    Ok(paths)
}

/// Where a track this tool inserted came from: the absolute path its insert
/// batch's input names (`batch_association`), or the one its single
/// insertion's producer input and plan give (`checked`).
fn historical_source(
    context: &mut ManagedContext,
    binding: &Binding,
    identity: &Identity,
) -> Result<std::path::PathBuf> {
    if let Some((batch, ordinal)) = context.batch_target(binding.content.track_pid)? {
        let input = context.batch_input(batch)?;
        let content = &input
            .record
            .targets
            .get(ordinal)
            .context("batch input lacks the bound target")?
            .target
            .content;
        ensure!(
            content.collection.is_some_and(|c| hex(&c) == input.scope),
            "batch target's collection differs from its batch input"
        );
        ensure!(
            binding.content.track_pid == content.track_pid
                && binding.content.path == content.path
                && binding.content.source == content.source,
            "active binding differs from its batch target"
        );
        return Ok(input.sources[ordinal].clone());
    }
    let origin = ancestry::origin(&context.journal, binding, identity)?;
    let digest = origin.input_digest();
    let (_, input) = context.find(digest)?;
    let claim = ancestry::path_claim(origin, identity, &input)?;
    let run = context.inputs[&digest][0].0.clone();
    let plan = Held::read(&run.join("plan.json"), MAX_PLAN)?;
    let prior = claim
        .prior_digests(&context.journal, identity, &plan.bytes)?
        .into_iter()
        .map(|digest| context.find(digest).map(|(_, bytes)| bytes))
        .collect::<Result<Vec<_>>>()?;
    let association = ancestry::check(
        &context.journal,
        identity,
        &claim,
        &input,
        &plan.bytes,
        &prior,
        &context.control,
    )?;
    context.retain(plan)?;
    Ok(association.historical_path())
}

/// A track an insert batch bound is associated through its batch's input,
/// which the journal pins and which names each target's absolute source path.
/// The same absolute path is the same source whatever selection reaches it
/// now, so a narrower `--include` replaces it too. Replacement removes the old
/// track and inserts the new source with its own cover, so the old track's
/// artwork does not limit it.
#[allow(clippy::too_many_arguments)] // Independent checked source/state/batch joins.
fn batch_association(
    context: &mut ManagedContext,
    batch: InsertBatchId,
    ordinal: usize,
    binding: &Binding,
    collection: &SourceCollection,
    source: &SourceTrack,
    desired: &ProjectedFields,
    catalog: &Catalog,
) -> Result<Option<ManagedAudioReplacement>> {
    let input = context.batch_input(batch)?;
    let historical_path = input
        .sources
        .get(ordinal)
        .context("batch input lacks the bound target")?;
    let desired_path = collection.base_directory.join(&source.relative_path);
    if desired_path != *historical_path {
        return Ok(None);
    }
    let target = &input.record.targets[ordinal];
    let content = &target.target.content;
    let result = (|| -> Result<ManagedAudioReplacement> {
        ensure!(
            content.collection.is_some_and(|c| hex(&c) == input.scope),
            "batch target's collection differs from its batch input"
        );
        ensure!(
            binding.content.track_pid == content.track_pid
                && binding.content.path == content.path
                && binding.content.source == content.source,
            "active binding differs from its batch target"
        );
        ensure!(
            content.source.audio != source.fingerprint.audio,
            "encoded descriptor is unchanged; not audio replacement"
        );
        ensure!(
            catalog
                .tracks()
                .iter()
                .any(|t| t.sync_id == content.track_pid),
            "current target missing"
        );
        let (binding_id, binding_version) = target.binding.context("batch target is not bound")?;
        let cover = music_protocol::media::artwork::read(&desired_path)?;
        let mut detail =
            ManagedAudioReplacement::empty("audio_replacement_proposed_execution_blocked");
        detail.replacement_identity_established = true;
        detail.evidence = Some(
            json!({"binding_id":binding_id.as_i64(),"binding_version":binding_version,
            "insert_batch":batch.as_i64(),"target_ordinal":ordinal,"producer_input_sha256":hex(&input.record.input_receipt),
            "source_root":collection.root,"base_directory":collection.base_directory,"relative_path":source.relative_path,
            "historical_absolute_source_path":historical_path,
            "immutable_original_content":{"track_id":format!("{:016x}",content.track_pid),"path":content.path,
                "source":fingerprint_json(&content.source.require_alac()?),"collection_sha256":content.collection.map(|s|hex(&s)),
                "artwork":content.artwork.as_ref().map(|a|json!({"cache_id":a.cache_id,"bytes":a.bytes,"sha256":hex(&a.sha256)}))},
            "desired_projected":desired,"desired_source":fingerprint_json(&source.fingerprint),
            "desired_cover":cover.cover.as_ref().map(|c|json!({"bytes":c.bytes.len(),"sha256":hex(&c.sha256)})),
            "audio_equality":"encoded_descriptor_differs_not_decoded_pcm_comparison"}),
        );
        ensure!(
            music_protocol::media::fingerprint::read(&desired_path)? == source.fingerprint
                && ProjectedFields::from_source(&music_protocol::media::metadata::read(
                    &desired_path
                )?)? == *desired,
            "desired source changed before replacement proposal publication"
        );
        context.control.check()?;
        Ok(detail)
    })();
    Ok(Some(result.unwrap_or_else(|error| {
        let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
        detail.refusal = Some(format!("{error:#}"));
        detail.evidence = Some(json!({"insert_batch":batch.as_i64(),"target_ordinal":ordinal,
            "historical_absolute_source_path":historical_path,"desired_absolute_source_path":desired_path}));
        detail
    })))
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

/// A track a single insertion bound is associated through that insertion's
/// producer input and plan, both pinned by the journal: they name the
/// source's root and relative path, so its absolute path. As for a batch, the
/// same absolute path is the same source whatever selection reaches it now
/// (a child directory, the file itself, `--include`/`--exclude`). Replacement
/// removes the old track and inserts the new source with its own cover, so the
/// old track's later tag or cover edits do not limit it.
#[allow(clippy::too_many_arguments)] // Independent checked source/state/ancestry joins.
fn checked(
    context: &mut ManagedContext,
    binding: &Binding,
    identity: &Identity,
    collection: &SourceCollection,
    source: &SourceTrack,
    desired: &ProjectedFields,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    step: &Path,
    input: &[u8],
    digest: [u8; 32],
    claim: &ancestry::PathClaim,
) -> Result<Option<ManagedAudioReplacement>> {
    let run = context.inputs[&digest][0].0.clone();
    let plan = Held::read(&run.join("plan.json"), MAX_PLAN)?;
    let prior = claim
        .prior_digests(&context.journal, identity, &plan.bytes)?
        .into_iter()
        .map(|digest| context.find(digest).map(|(_, bytes)| bytes))
        .collect::<Result<Vec<_>>>()?;
    let association = ancestry::check(
        &context.journal,
        identity,
        claim,
        input,
        &plan.bytes,
        &prior,
        &context.control,
    )?;
    let historical_path = association.historical_path();
    let desired_path = collection.base_directory.join(&source.relative_path);
    if desired_path != historical_path {
        return Ok(None);
    }
    let result = (|| -> Result<ManagedAudioReplacement> {
        let content = association.content();
        ensure!(
            content.source.audio != source.fingerprint.audio,
            "encoded descriptor is unchanged; not audio replacement"
        );
        ensure!(
            index.matches_transferred_source(content.track_pid, &content.path, &content.source),
            "original Content/current index differs"
        );
        ensure!(
            catalog
                .tracks()
                .iter()
                .any(|t| t.sync_id == content.track_pid),
            "current target missing"
        );
        ensure!(
            catalog.persisted_media_anchor() >= binding.media_anchor,
            "current anchor precedes the binding"
        );
        let retained = step.join("retained-source.m4a");
        let available = retained.try_exists()?;
        if available {
            ensure!(
                retained != desired_path,
                "desired source cannot be original bytes"
            );
            for p in retained.ancestors() {
                ensure!(
                    !fs::symlink_metadata(p)?.file_type().is_symlink(),
                    "original source symlink ancestor"
                );
            }
            ensure!(
                fs::symlink_metadata(&retained)?.nlink() == 1,
                "original source hardlink refused"
            );
            source_attribution::check(
                &context.journal,
                association.reference(),
                identity,
                input,
                &plan.bytes,
                &retained,
                &prior,
            )?;
        }
        let cover = music_protocol::media::artwork::read(&desired_path)?;
        let before = association.projected();
        let after = serde_json::to_value(desired)?;
        let changed_fields = after
            .as_object()
            .context("desired projection object")?
            .iter()
            .filter(|(name, value)| before[*name] != **value)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let options = association.options();
        let mut detail =
            ManagedAudioReplacement::empty("audio_replacement_proposed_execution_blocked");
        detail.replacement_identity_established = true;
        detail.original_bytes_available = available;
        detail.evidence = Some(
            json!({"binding_id":association.reference().id.as_i64(),"binding_version":association.reference().version,
        "insertion_attempt":association.insertion(),"producer_input_sha256":hex(&hash(input)),"plan_sha256":hex(&hash(&plan.bytes)),
        "checked_ancestry":association.evidence(),"source_root":association.root(),"base_directory":association.base(),"relative_path":association.relative_path(),
        "historical_selection":{"recursive":options.recursive,"include":options.include,"exclude":options.exclude},
        "historical_absolute_source_path":historical_path,"desired_source_root":collection.root,"desired_relative_path":source.relative_path,
        "immutable_original_content":{"track_id":format!("{:016x}",content.track_pid),"path":content.path,
            "source":fingerprint_json(&content.source.require_alac()?),"collection_sha256":content.collection.map(|s|hex(&s)),
            "artwork":binding.content.artwork.as_ref().map(|a|json!({"cache_id":a.cache_id,"bytes":a.bytes,"sha256":hex(&a.sha256)}))},
        "historical_projected":before,"desired_projected":after,"changed_projected_fields":changed_fields,
        "desired_source":fingerprint_json(&source.fingerprint),"desired_cover":cover.cover.as_ref().map(|c|json!({"bytes":c.bytes.len(),"sha256":hex(&c.sha256)})),
        "original_path":available.then_some(&retained),"audio_equality":"encoded_descriptor_differs_not_decoded_pcm_comparison",
        "target_lifetime_retirement_authorized":false,"old_ids_restored":false,"statistics_or_memberships_migration_promised":false}),
        );
        context.retain(plan)?;
        context.recheck()?;
        ensure!(
            music_protocol::media::fingerprint::read(&desired_path)? == source.fingerprint
                && ProjectedFields::from_source(&music_protocol::media::metadata::read(
                    &desired_path
                )?)? == *desired,
            "desired source changed before replacement proposal publication"
        );
        if available {
            ensure!(
                music_protocol::media::fingerprint::read(&retained)?
                    == content.source.require_alac()?,
                "original changed before proposal publication"
            );
        }
        context.control.check()?;
        Ok(detail)
    })();
    Ok(Some(match result {
        Ok(detail) => detail,
        Err(error) => {
            let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
            detail.refusal = Some(format!("{error:#}"));
            detail.evidence = Some(
                json!({"known_relative_path_claim":association.relative_path(),
                "source_root":association.root(),"base_directory":association.base(),
                "historical_absolute_source_path":historical_path,"desired_absolute_source_path":desired_path,
                "checked_historical_ancestry":association.evidence()}),
            );
            detail
        }
    }))
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

/// The replacement proposal for an MP3 whose audio changed at the absolute
/// path an insert batch added an owned MP3 from, as `describe` gives for
/// ALAC; likewise for a FLAC whose audio changed since an insert batch sent
/// the ALAC converted from it (`conversions`, by track). A track a single
/// session inserted is not associated here (that insertion keeps no checked
/// absolute source path); the planner's plausible track check refuses to add
/// a second copy of it.
#[allow(clippy::too_many_arguments)]
pub(super) fn describe_mpeg(
    cache: &mut Option<Result<ManagedContext>>,
    journal: JournalEvidence<'_>,
    bindings: &[Binding],
    conversions: &std::collections::BTreeMap<u64, crate::engine::state::journal::Conversion>,
    collection: &SourceCollection,
    source: &crate::engine::planning::source_collection::Mp3Track,
    catalog: &Catalog,
    control: Option<&crate::engine::execution::native_insert::Control>,
) -> Option<ManagedAudioReplacement> {
    let identity = source.source.transferred_identity().ok()?;
    let relevant = bindings
        .iter()
        .filter(|b| {
            let sent = if b.content.source.is_alac() {
                conversions
                    .get(&b.content.track_pid)
                    .map(|c| c.source_audio)
            } else {
                Some(b.content.source.audio.sha256)
            };
            sent.is_some_and(|audio| audio != identity.audio.sha256)
        })
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        return None;
    }
    let context = match cache.get_or_insert_with(|| match control {
        Some(control) => {
            ManagedContext::open_with_control(journal.journal, journal.identity, control)
        }
        None => ManagedContext::open(journal.journal, journal.identity),
    }) {
        Ok(context) => context,
        Err(error) => {
            let mut detail = ManagedAudioReplacement::empty("unassociated_history_limitation");
            detail.unassociated_history.push(
                json!({"scope":"managed_history", "reason":format!("{error:#}"),
                "source_path_unknown":true,"associated_to_desired_row":false,
                "does_not_prove_source_absence":true,"baseline_decision_preserved":true}),
            );
            return Some(detail);
        }
    };
    let mut proposals = Vec::new();
    let mut unassociated = Vec::new();
    for binding in relevant {
        let result =
            context
                .batch_target(binding.content.track_pid)
                .and_then(|target| match target {
                    Some((batch, ordinal)) => mpeg_batch_association(
                        context, batch, ordinal, binding, collection, source, &identity, catalog,
                    ),
                    None => match context.recorded_source(binding.content.track_pid)? {
                        Some(recorded) => {
                            let desired_path =
                                collection.base_directory.join(&source.relative_path);
                            // An ALAC converted from a FLAC holds that FLAC's audio.
                            let sent = conversions
                                .get(&binding.content.track_pid)
                                .map_or(binding.content.source.audio.sha256, |c| c.source_audio);
                            recorded_association(
                                context,
                                &recorded,
                                binding,
                                &desired_path,
                                sent != identity.audio.sha256,
                                catalog,
                                || {
                                    Ok(music_protocol::media::mpeg::read(&desired_path)?
                                        == source.source)
                                },
                            )
                        }
                        None => Ok(None),
                    },
                });
        match result {
            Ok(Some(detail)) => proposals.push(detail),
            Ok(None) => {}
            Err(error) => unassociated.push(json!({"scope":binding.content.collection.map(|s|hex(&s)),
                "observed_track_id":format!("{:016x}",binding.content.track_pid),"reason":format!("{error:#}"),
                "source_path_unknown":true,"associated_to_desired_row":false,
                "does_not_prove_source_absence":true,"baseline_decision_preserved":true})),
        }
    }
    if proposals.len() > 1 {
        let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
        detail.refusal =
            Some("ambiguous native historical bindings claim the same source path".into());
        detail.unassociated_history = unassociated;
        return Some(detail);
    }
    if let Some(mut detail) = proposals.pop() {
        detail.unassociated_history = unassociated;
        return Some(detail);
    }
    if unassociated.is_empty() {
        None
    } else {
        let mut detail = ManagedAudioReplacement::empty("unassociated_history_limitation");
        detail.unassociated_history = unassociated;
        Some(detail)
    }
}

/// `batch_association` for an MP3: the batch's input names the absolute path
/// the owned track came from; changed frames there are a replacement.
#[allow(clippy::too_many_arguments)] // Independent checked source/state/batch joins.
fn mpeg_batch_association(
    context: &mut ManagedContext,
    batch: InsertBatchId,
    ordinal: usize,
    binding: &Binding,
    collection: &SourceCollection,
    source: &crate::engine::planning::source_collection::Mp3Track,
    identity: &music_protocol::media::fingerprint::TransferredSource,
    catalog: &Catalog,
) -> Result<Option<ManagedAudioReplacement>> {
    let input = context.batch_input(batch)?;
    let historical_path = input
        .sources
        .get(ordinal)
        .context("batch input lacks the bound target")?;
    let desired_path = collection.base_directory.join(&source.relative_path);
    if desired_path != *historical_path {
        return Ok(None);
    }
    let target = &input.record.targets[ordinal];
    let content = &target.target.content;
    let result = (|| -> Result<ManagedAudioReplacement> {
        ensure!(
            content.collection.is_some_and(|c| hex(&c) == input.scope),
            "batch target's collection differs from its batch input"
        );
        ensure!(
            binding.content.track_pid == content.track_pid
                && binding.content.path == content.path
                && binding.content.source == content.source,
            "active binding differs from its batch target"
        );
        // An ALAC converted from a FLAC holds that FLAC's audio.
        let conversion = target.target.conversion.as_ref();
        ensure!(
            conversion.map_or(content.source.audio.sha256, |c| c.source_audio)
                != identity.audio.sha256,
            "encoded frames are unchanged; not audio replacement"
        );
        ensure!(
            catalog
                .tracks()
                .iter()
                .any(|t| t.sync_id == content.track_pid),
            "current target missing"
        );
        let (binding_id, binding_version) = target.binding.context("batch target is not bound")?;
        let cover = music_protocol::media::mpeg::cover_picture(&source.source.artwork);
        let mut detail =
            ManagedAudioReplacement::empty("audio_replacement_proposed_execution_blocked");
        detail.replacement_identity_established = true;
        detail.evidence = Some(
            json!({"binding_id":binding_id.as_i64(),"binding_version":binding_version,
            "insert_batch":batch.as_i64(),"target_ordinal":ordinal,"producer_input_sha256":hex(&input.record.input_receipt),
            "source_root":collection.root,"base_directory":collection.base_directory,"relative_path":source.relative_path,
            "historical_absolute_source_path":historical_path,
            "immutable_original_content":{"track_id":format!("{:016x}",content.track_pid),"path":content.path,
                "source":mpeg_fingerprint(&content.source),"collection_sha256":content.collection.map(|s|hex(&s)),
                "artwork":content.artwork.as_ref().map(|a|json!({"cache_id":a.cache_id,"bytes":a.bytes,"sha256":hex(&a.sha256)}))},
            "desired_source":mpeg_fingerprint(identity),
            "desired_cover":cover.map(|c|json!({"bytes":c.bytes,"sha256":c.sha256})),
            "audio_equality":"encoded_frames_differ_not_decoded_pcm_comparison"}),
        );
        if let (Some(conversion), Some(evidence)) = (conversion, detail.evidence.as_mut()) {
            evidence["converted_from"] = json!({"source_path":conversion.source_path,
                "file_bytes":conversion.source_bytes,"full_file_sha256":hex(&conversion.source_sha256),
                "audio_sha256":hex(&conversion.source_audio),"ffmpeg":conversion.ffmpeg});
        }
        ensure!(
            music_protocol::media::mpeg::read(&desired_path)? == source.source,
            "desired source changed before replacement proposal publication"
        );
        context.control.check()?;
        Ok(detail)
    })();
    Ok(Some(result.unwrap_or_else(|error| {
        let mut detail = ManagedAudioReplacement::empty("audio_replacement_evidence_refused");
        detail.refusal = Some(format!("{error:#}"));
        detail.evidence = Some(json!({"insert_batch":batch.as_i64(),"target_ordinal":ordinal,
            "historical_absolute_source_path":historical_path,"desired_absolute_source_path":desired_path}));
        detail
    })))
}
