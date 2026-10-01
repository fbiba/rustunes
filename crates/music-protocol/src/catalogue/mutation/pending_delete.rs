//! Pure metadata-only removal bytes for one represented, unfilled Music row.
//!
//! This profile is deliberately separate from ordinary installed-track deletion.
//! Its constructor checks supplied catalogue data, never device freshness, AFC
//! absence, Original/claim ownership, SQL relationships or execution permission.
//! It admits only the zero advertised-size profile observed in saved phone
//! data. Zero metadata size still does not prove AFC absence.

use super::{CatalogIdentity, Mutation, catalog_identity, encode, target};
use crate::catalogue::sqlite::{Catalog, Track};
use anyhow::{Context, Result, ensure};
use std::time::SystemTime;

#[derive(PartialEq, Eq)]
struct Binding {
    database: u64,
    primary_container: u64,
    anchor: u32,
    database_sha256: [u8; 32],
    database_bytes: u64,
    wal_sha256: Option<[u8; 32]>,
    wal_bytes: Option<u64>,
}
impl Binding {
    fn from_catalog(catalog: &Catalog) -> Self {
        let hashes = catalog.source_hashes();
        Self {
            database: catalog.database_pid(),
            primary_container: catalog.primary_container_pid(),
            anchor: catalog.persisted_media_anchor(),
            database_sha256: hashes.database,
            database_bytes: hashes.database_bytes,
            wal_sha256: hashes.wal,
            wal_bytes: hashes.wal_bytes,
        }
    }
}

/// Checked operation data only. There is no conversion into ordinary delete,
/// source delivery, signature, Started permit or completion evidence.
/// A caller must derive `expected` from authenticated Original lineage and
/// fresh Current, retain those owners, and separately observe the reserved
/// destination absent before any Start.
pub struct PendingDelete {
    binding: Binding,
    track: Track,
    reserved_destination: String,
    operation_bytes: Vec<u8>,
    revision: u32,
}
impl PendingDelete {
    pub fn prepare(
        catalog: &Catalog,
        expected: &Track,
        reserved_destination: &str,
        timestamp: SystemTime,
    ) -> Result<Self> {
        check_target(catalog, expected, reserved_destination)?;
        let identity = catalog_identity(catalog);
        let revision = next_revision(&identity)?;
        let operation_bytes = encode(
            identity,
            expected.sync_id,
            Mutation::Delete,
            timestamp,
            revision,
        )?;
        Ok(Self {
            binding: Binding::from_catalog(catalog),
            track: expected.clone(),
            reserved_destination: reserved_destination.to_owned(),
            operation_bytes,
            revision,
        })
    }

    /// Exact same catalogue bytes and complete projected target row. This is
    /// consistency only: a saved catalogue cannot become fresh through recheck.
    pub fn ensure_current(&self, catalog: &Catalog) -> Result<()> {
        ensure!(
            self.binding == Binding::from_catalog(catalog),
            "pending delete belongs to different catalogue bytes/identity/anchor"
        );
        check_target(catalog, &self.track, &self.reserved_destination)?;
        ensure!(
            next_revision(&catalog_identity(catalog))? == self.revision,
            "pending delete revision differs"
        );
        Ok(())
    }

    pub fn target_sync_id(&self) -> u64 {
        self.track.sync_id
    }
    pub fn target_asset_id(&self) -> String {
        (self.track.sync_id as i64).to_string()
    }
    pub fn base_anchor(&self) -> u32 {
        self.binding.anchor
    }
    pub fn revision(&self) -> u32 {
        self.revision
    }
    pub fn reserved_destination(&self) -> &str {
        &self.reserved_destination
    }
    pub fn operation_bytes(&self) -> &[u8] {
        &self.operation_bytes
    }
}

fn next_revision(identity: &CatalogIdentity) -> Result<u32> {
    ensure!(
        identity.database != 0 && identity.primary_container != 0,
        "pending delete lacks current catalogue identity"
    );
    identity
        .anchor
        .checked_add(1)
        .context("Media anchor exhausted")
}

fn check_target(catalog: &Catalog, expected: &Track, destination: &str) -> Result<()> {
    let track = target(catalog.tracks(), expected.sync_id)?;
    ensure!(
        track == expected,
        "pending delete target metadata/identity differs"
    );
    let state = &track.availability;
    ensure!(
        track.internal_id != 0
            && track.album_sync_id != 0
            && track.artist_sync_id != 0
            && track.afc_path.is_none()
            && track.metadata.file_size == 0
            && !state.has_non_purgeable_asset
            && !state.is_placeholder
            && state.is_library_added
            && state.is_supported_type,
        "pending delete requires a represented zero-size unfilled supported Music row"
    );
    ensure!(
        track.sync_id != catalog.database_pid()
            && track.sync_id != catalog.primary_container_pid()
            && !catalog
                .albums()
                .values()
                .any(|album| album.sync_id == track.sync_id)
            && !catalog
                .artists()
                .values()
                .any(|artist| artist.sync_id == track.sync_id),
        "pending delete track collides with catalogue/group identity"
    );
    // Zero projected file_size is an observed row-profile restriction, never
    // physical absence proof. Other pending-audio profiles remain unchanged.
    check_destination(destination)?;
    ensure!(
        catalog.tracks().iter().all(|other| other
            .afc_path
            .as_deref()
            .is_none_or(|path| !path.eq_ignore_ascii_case(destination))),
        "pending delete reserved destination is referenced by catalogue"
    );
    Ok(())
}

fn check_destination(path: &str) -> Result<()> {
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() == 4 && parts[..2] == ["iTunes_Control", "Music"],
        "pending delete destination is not a Music path"
    );
    let bucket = parts[2].as_bytes();
    let leaf = parts[3];
    ensure!(
        bucket.len() == 3
            && bucket[0] == b'F'
            && bucket[1..].iter().all(u8::is_ascii_digit)
            && leaf.len() > 4
            && leaf.len() <= 255
            && leaf.ends_with(".m4a")
            && leaf
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "pending delete destination is not a canonical ALAC path"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
