//! Typed general metadata requests and offline operation descriptions.
//! Uses music-protocol's composed metadata serializer
//! (`metadata_update::prepare_source_metadata`). Neither this document nor its
//! epoch template is a Prepared intent or native permission.
use super::{Engine, Event, additive, saved_plan};
use crate::{
    engine::state::journal::{Identity, Journal},
    engine::state::snapshot,
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::identity;
use music_protocol::catalogue::metadata_update::{self, SourceMetadataPatch};
use music_protocol::catalogue::sqlite::{Catalog, Track};
use music_protocol::protocol::trace::private_write;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    path::Path,
    time::SystemTime,
};

const REQUEST_SCHEMA: &str = "rustunes.metadata-request";
const MAX_REQUEST: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataRequestDocument {
    pub schema: String,
    pub patch: SourceMetadataPatch,
}
#[cfg(test)]
impl MetadataRequestDocument {
    pub fn new(patch: SourceMetadataPatch) -> Self {
        Self {
            schema: REQUEST_SCHEMA.into(),
            patch,
        }
    }
}

pub struct MetadataPlanRequest<'a> {
    pub track: u64,
    pub request: &'a Path,
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub output: Option<&'a Path>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MetadataWireCandidate {
    ComposedTemplate {
        /// Exactly one update_db_info and update_track from the pure serializer.
        binary_plist: Vec<u8>,
        sha256: String,
        revision: u32,
        timestamp_unix_seconds: u64,
    },
    Unsupported {
        reason: String,
    },
}

#[derive(Serialize)]
pub struct MetadataIntent {
    pub schema: &'static str,
    pub track_id: String,
    pub requested: SourceMetadataPatch,
    pub before_catalogue: Value,
    pub database_id: String,
    pub primary_container_id: String,
    pub media_anchor: u32,
    pub catalogue_database_sha256: String,
    pub catalogue_wal_sha256: Option<String>,
    pub wire: MetadataWireCandidate,
    // Fixed values: the intent is an offline template, never a permission to
    // write.
    pub template_only: bool,
    pub fresh_preparation_required: bool,
    pub physical_sql_eligibility_checked: bool,
    pub execution_authorized: bool,
    pub native_receiver_qualified: bool,
    pub start_authority: bool,
    pub completion_authority: bool,
}

#[derive(Serialize)]
pub struct MetadataPlan {
    pub schema: &'static str,
    pub intent: MetadataIntent,
    pub request_sha256: String,
    pub device_sha256: String,
    pub library_sha256: String,
    pub complete_index_sha256: String,
    pub binding_id: i64,
    pub binding_version: u32,
    pub binding_content_sha256: String,
    pub original_transferred_source_sha256: String,
    pub source_ancestry_checked: bool,
    pub journal_changed: bool,
    pub device_contacted: bool,
    pub phone_writes: u32,
}
pub struct MetadataPlanResult {
    pub serialized: Vec<u8>,
}

pub(crate) fn parse_request(bytes: &[u8]) -> Result<MetadataRequestDocument> {
    ensure!(
        bytes.len() <= MAX_REQUEST,
        "metadata request exceeds 128 KiB"
    );
    let document: MetadataRequestDocument =
        serde_json::from_value(saved_plan::unique_value(bytes)?)
            .context("reading a complete typed metadata request")?;
    ensure!(
        document.schema == REQUEST_SCHEMA,
        "unsupported metadata request schema"
    );
    Ok(document)
}

pub(crate) fn describe(
    catalog: &Catalog,
    track: &Track,
    patch: &SourceMetadataPatch,
) -> Result<MetadataIntent> {
    let binding = metadata_update::bind_source_metadata(catalog, track.sync_id)?;
    ensure!(
        binding.before() == track,
        "metadata intent target differs from current catalogue"
    );
    let wire = match metadata_update::prepare_source_metadata(
        catalog,
        &binding,
        patch,
        SystemTime::UNIX_EPOCH,
    ) {
        Ok(bytes) => MetadataWireCandidate::ComposedTemplate {
            sha256: hex(&Sha256::digest(&bytes)),
            binary_plist: bytes,
            revision: catalog
                .persisted_media_anchor()
                .checked_add(1)
                .context("metadata anchor exhausted")?,
            timestamp_unix_seconds: 0,
        },
        Err(error) => MetadataWireCandidate::Unsupported {
            reason: format!("{error:#}"),
        },
    };
    Ok(MetadataIntent {
        schema: "rustunes.metadata-intent-description",
        track_id: format!("{:016x}", track.sync_id),
        requested: patch.clone(),
        before_catalogue: additive::metadata_catalogue_fields(catalog, track),
        database_id: format!("{:016x}", catalog.database_pid()),
        primary_container_id: format!("{:016x}", catalog.primary_container_pid()),
        media_anchor: catalog.persisted_media_anchor(),
        catalogue_database_sha256: hex(&catalog.source_hashes().database),
        catalogue_wal_sha256: catalog.source_hashes().wal.as_ref().map(|sha| hex(sha)),
        wire,
        template_only: true,
        fresh_preparation_required: true,
        physical_sql_eligibility_checked: false,
        execution_authorized: false,
        native_receiver_qualified: false,
        start_authority: false,
        completion_authority: false,
    })
}

pub(in crate::engine) fn run(
    request: MetadataPlanRequest<'_>,
    engine: &Engine,
) -> Result<MetadataPlanResult> {
    engine.check()?;
    if let Some(output) = request.output {
        ensure!(
            !output.try_exists()?,
            "metadata intent output already exists; choose a new file"
        );
    }
    ensure!(
        fs::symlink_metadata(request.request)?.len() <= MAX_REQUEST as u64,
        "metadata request exceeds 128 KiB"
    );
    let bytes = saved_plan::read_stable(request.request)?;
    let document = parse_request(&bytes)?;
    engine.check()?;
    engine.emit(Event::ReadingSavedSnapshot)?;
    let saved = snapshot::open(request.snapshot)?;
    if let Some(device) = engine.selected_device() {
        ensure!(
            <[u8; 32]>::from(Sha256::digest(device.as_bytes()))
                == saved.identity.device_identifier_sha256,
            "selected device differs from metadata snapshot"
        );
    }
    let identity = Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &identity::parse(&saved.raw_preferences)?,
    )?;
    let journal = Journal::open_read_only(request.journal, &identity)?;
    ensure!(
        journal.outstanding()?.is_none(),
        "metadata intent planning requires an idle journal"
    );
    let active = journal
        .active_bindings()?
        .into_iter()
        .filter(|active| active.binding.content.track_pid == request.track)
        .collect::<Vec<_>>();
    ensure!(
        active.len() == 1,
        "metadata intent requires exactly one active target binding"
    );
    let active = &active[0];
    let content = &active.binding.content;
    content.source.require_alac()?;
    ensure!(
        saved
            .index
            .matches_transferred_source(request.track, &content.path, &content.source),
        "metadata intent index differs from original transferred Content"
    );
    let track = metadata_update::target_track(&saved.catalog, request.track)?;
    let intent = describe(&saved.catalog, track, &document.patch)?;
    let plan = MetadataPlan {
        schema: "rustunes.metadata-plan",
        intent,
        request_sha256: hex(&Sha256::digest(&bytes)),
        device_sha256: hex(&identity.device_digest()),
        library_sha256: hex(&identity.library_digest()),
        complete_index_sha256: hex(&saved.index_receipt_sha256),
        binding_id: active.reference.id.as_i64(),
        binding_version: active.reference.version,
        binding_content_sha256: hex(&active.binding.content_sha256()?),
        original_transferred_source_sha256: hex(&content.source.full_file_sha256),
        source_ancestry_checked: false,
        journal_changed: false,
        device_contacted: false,
        phone_writes: 0,
    };
    journal.recheck_read_only_source()?;
    engine.check()?;
    let mut serialized = serde_json::to_vec_pretty(&plan)?;
    serialized.push(b'\n');
    if let Some(output) = request.output {
        engine.check()?;
        private_write(output, &serialized)?;
        File::open(
            output
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        engine.emit(Event::PlanSaved(output))?;
    }
    engine.check()?;
    Ok(MetadataPlanResult { serialized })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use music_protocol::catalogue::metadata_update::SourceFieldRequest;

    #[test]
    fn metadata_request_parser_retains_complete_set_clear_and_preserve() {
        let request = MetadataRequestDocument::new(SourceMetadataPatch {
            title: SourceFieldRequest::Set("New title".into()),
            composer: SourceFieldRequest::Clear,
            year: SourceFieldRequest::Set(2025),
            ..Default::default()
        });
        let parsed = parse_request(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(parsed, request);
        assert_eq!(parsed.patch.composer, SourceFieldRequest::Clear);
        assert_eq!(parsed.patch.genre, SourceFieldRequest::Preserve);
    }
}
