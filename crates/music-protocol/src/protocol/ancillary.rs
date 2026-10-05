//! Bounded, snapshot-bound ancillary refusals for a single new Music audio asset.
//! IDs come from the fresh manifest and must resolve to existing track entities;
//! artwork cache IDs are evidence, never wire AssetIDs. This module sends nothing.
//!
//! Every policy assumes the host has no foreign artwork to send and that Genius
//! is disabled: a foreign track's request for artwork alone (part 2) or Genius
//! alone (part 4) is declined, and any other foreign request refuses.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};

use crate::catalogue::{
    identity::LibraryIdentity,
    sqlite::{Catalog, Track},
};

pub const MAX_MANIFEST_ASSETS: usize = 1024;

/// Fixed audio manifest contracts; selecting AudioWithDisabledGenius requires
/// its explicit factory.
/// This describes host protocol behavior, never insertion or replay authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioManifestProfile {
    AudioOnly,
    AudioWithDisabledGenius,
    /// Each own track requests audio (1) or audio and its cover (3), either
    /// possibly with the disabled Genius bit (5 or 7).
    AudioWithArtwork,
}

#[derive(Clone, Copy)]
enum OwnRequestProfile {
    SinglePart,
    AudioWithDisabledGenius,
    /// Each own asset requests the part mask declared for it, possibly with
    /// the disabled Genius bit.
    Declared,
}

#[derive(Clone, PartialEq, Eq)]
struct Binding {
    identity: [u8; 32],
    database: [u8; 32],
    wal: Option<[u8; 32]>,
    database_bytes: u64,
    wal_bytes: Option<u64>,
    database_pid: u64,
    container_pid: u64,
    anchor: u32,
}

impl Binding {
    fn new(catalog: &Catalog, identity: &LibraryIdentity) -> Self {
        let hashes = catalog.source_hashes();
        Self {
            identity: identity.binding_sha256(),
            database: hashes.database,
            wal: hashes.wal,
            database_bytes: hashes.database_bytes,
            wal_bytes: hashes.wal_bytes,
            database_pid: catalog.database_pid(),
            container_pid: catalog.primary_container_pid(),
            anchor: catalog.persisted_media_anchor(),
        }
    }
}

/// Private fields prevent callers from substituting arbitrary eligible IDs.
/// Retains the complete represented namespace so an own PID cannot collide
/// with an ineligible existing entity or a collection.
pub struct EligibleSnapshot {
    binding: Binding,
    library_id: String,
    occupied: BTreeSet<u64>,
    tracks: BTreeMap<u64, Option<u32>>,
}

impl EligibleSnapshot {
    pub fn new(catalog: &Catalog, identity: &LibraryIdentity) -> Self {
        let tracks = catalog
            .tracks()
            .iter()
            .filter(|track| eligible_track(track))
            .map(|track| {
                (
                    track.sync_id,
                    catalog
                        .artwork()
                        .track_cache_ids()
                        .get(&track.sync_id)
                        .copied(),
                )
            })
            .collect();
        Self {
            binding: Binding::new(catalog, identity),
            library_id: identity.library_id().to_owned(),
            occupied: catalog.represented_sync_ids().clone(),
            tracks,
        }
    }

    pub fn ensure_current(&self, catalog: &Catalog, identity: &LibraryIdentity) -> Result<()> {
        ensure!(
            self.binding == Binding::new(catalog, identity),
            "ancillary policy belongs to different library/catalog/anchor evidence"
        );
        Ok(())
    }

    pub fn audio(&self, own_asset_id: &str) -> Result<AudioPolicy<'_>> {
        let own_pid = asset_pid(own_asset_id)?;
        ensure!(
            !self.occupied.contains(&own_pid),
            "new audio identity collides with existing catalog entity"
        );
        Ok(AssetPolicy {
            snapshot: self,
            own: vec![own_asset_id.to_owned()],
            own_request_profile: OwnRequestProfile::SinglePart,
            declared: Vec::new(),
        })
    }

    /// Several new tracks inserted in one session. Each identity passes the
    /// same occupied-namespace guard as a single insert, and the identities
    /// are distinct. The manifest must request every one of them.
    pub fn audio_batch(&self, own_asset_ids: &[String]) -> Result<AudioPolicy<'_>> {
        ensure!(
            !own_asset_ids.is_empty() && own_asset_ids.len() <= MAX_MANIFEST_ASSETS,
            "audio batch size outside qualified bound"
        );
        let mut seen = BTreeSet::new();
        for id in own_asset_ids {
            let pid = asset_pid(id)?;
            ensure!(seen.insert(pid), "duplicate new audio identity in batch");
            ensure!(
                !self.occupied.contains(&pid),
                "new audio identity collides with existing catalog entity"
            );
        }
        Ok(AssetPolicy {
            snapshot: self,
            own: own_asset_ids.to_vec(),
            own_request_profile: OwnRequestProfile::SinglePart,
            declared: Vec::new(),
        })
    }

    /// An audio batch whose tracks may carry their covers in the same
    /// session. A track sent with artwork must be requested with Track and
    /// Artwork (mask 3), the others with Track alone (mask 1), as the reference
    /// host's insertions with artwork were.
    /// The phone may add Genius (4) to either, as for a single insertion:
    /// Genius is disabled, so the track and cover go as declared, with one
    /// ordinary FileComplete.
    pub fn audio_batch_with_artwork(&self, own: &[(String, bool)]) -> Result<AudioPolicy<'_>> {
        let ids: Vec<String> = own.iter().map(|(id, _)| id.clone()).collect();
        let mut policy = self.audio_batch(&ids)?;
        policy.own_request_profile = OwnRequestProfile::Declared;
        policy.declared = own
            .iter()
            .map(|(_, artwork)| if *artwork { 3 } else { 1 })
            .collect();
        Ok(policy)
    }

    /// Explicit audio/Genius contract. Uses the same canonical own identity,
    /// occupied-namespace and snapshot guards as `audio`, which it calls.
    /// It always selects the disabled-Genius request profile (own mask 1 or
    /// 5), with no configurable masks.
    pub fn audio_with_disabled_genius(&self, own_asset_id: &str) -> Result<AudioPolicy<'_>> {
        let mut policy = self.audio(own_asset_id)?;
        policy.own_request_profile = OwnRequestProfile::AudioWithDisabledGenius;
        Ok(policy)
    }

    /// Separate existing-track artwork contract. The target must already be a
    /// supported playable track; it need not yet have an artwork registration,
    /// since this session's metadata update introduces that registration.
    /// The phone may add the disabled Genius bit (6) to the Artwork part, as
    /// it does for an insertion; the cover goes with one ordinary FileComplete.
    pub fn artwork(&self, own_asset_id: &str) -> Result<ArtworkPolicy<'_>> {
        let own_pid = asset_pid(own_asset_id)?;
        ensure!(
            self.tracks.contains_key(&own_pid),
            "artwork target is not an eligible existing track"
        );
        Ok(AssetPolicy {
            snapshot: self,
            own: vec![own_asset_id.to_owned()],
            own_request_profile: OwnRequestProfile::Declared,
            declared: vec![2],
        })
    }

    /// Existing-track scalar metadata only: no own asset may be requested.
    /// This is a refusal policy, not permission to mutate the target's metadata.
    pub fn metadata(&self, target: &str) -> Result<MetadataPolicy<'_>> {
        let pid = asset_pid(target)?;
        ensure!(
            self.tracks.contains_key(&pid),
            "metadata target is not an eligible existing track"
        );
        Ok(AssetPolicy {
            snapshot: self,
            own: vec![target.to_owned()],
            own_request_profile: OwnRequestProfile::SinglePart,
            declared: Vec::new(),
        })
    }

    /// Metadata-only removal of rows an interrupted insertion left without
    /// audio: each target is represented but not installed, and a request for
    /// any of them refuses like a request for an own asset.
    pub fn metadata_unfilled(&self, targets: &[String]) -> Result<MetadataPolicy<'_>> {
        ensure!(
            !targets.is_empty() && targets.len() <= MAX_MANIFEST_ASSETS,
            "unfilled removal target count outside qualified bound"
        );
        let mut seen = BTreeSet::new();
        for target in targets {
            let pid = asset_pid(target)?;
            ensure!(
                seen.insert(pid) && self.occupied.contains(&pid) && !self.tracks.contains_key(&pid),
                "unfilled removal target is absent, repeated or installed"
            );
        }
        Ok(AssetPolicy {
            snapshot: self,
            own: targets.to_vec(),
            own_request_profile: OwnRequestProfile::SinglePart,
            declared: Vec::new(),
        })
    }

    /// Metadata-only session that names no track (a playlist deletion): no
    /// asset is its own, so only eligible foreign artwork or Genius requests
    /// are declined and any other request refuses.
    pub fn metadata_without_tracks(&self) -> MetadataPolicy<'_> {
        AssetPolicy {
            snapshot: self,
            own: Vec::new(),
            own_request_profile: OwnRequestProfile::SinglePart,
            declared: Vec::new(),
        }
    }

    /// Local audit binding, not an authenticator or proof of file availability.
    pub fn binding_sha256(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"music-protocol/ancillary-policy\0");
        h.update(self.binding.identity);
        h.update(self.binding.database);
        h.update(self.binding.database_bytes.to_le_bytes());
        h.update([u8::from(self.binding.wal.is_some())]);
        if let Some(wal) = self.binding.wal {
            h.update(wal);
        }
        h.update(self.binding.wal_bytes.unwrap_or(0).to_le_bytes());
        h.update(self.binding.database_pid.to_le_bytes());
        h.update(self.binding.container_pid.to_le_bytes());
        h.update(self.binding.anchor.to_le_bytes());
        h.update((self.tracks.len() as u64).to_le_bytes());
        for (pid, art) in &self.tracks {
            h.update(pid.to_le_bytes());
            h.update([u8::from(art.is_some())]);
            h.update(art.unwrap_or(0).to_le_bytes());
        }
        h.finalize().into()
    }
}

/// Existing pending audio demand only. It grants no original-parent, source,
/// producer-execution, writable AFC, completion or journal capability.
/// Kept distinct from AudioPolicy so ordinary new insertion cannot select it.
pub struct PendingAudioPolicy<'a> {
    policy: AssetPolicy<'a, 1>,
}

/// Atomic accepted manifest shape; not an asset-delivery permit.
pub struct PendingAudioManifest {
    own_parts: u64,
    foreign: Vec<String>,
}
impl PendingAudioManifest {
    pub fn own_parts(&self) -> u64 {
        self.own_parts
    }
    pub fn foreign_asset_ids(&self) -> &[String] {
        &self.foreign
    }
}
impl PendingAudioPolicy<'_> {
    pub fn own_asset_id(&self) -> &str {
        self.policy.own_asset_id()
    }
    pub fn ensure_session(&self, library_id: &str, anchor: u32) -> Result<()> {
        self.policy.ensure_session(library_id, anchor)
    }
    pub fn validate_manifest(&self, message: &Value, session: u32) -> Result<PendingAudioManifest> {
        // Share the strict whole-message validator, without constructing or
        // returning the ordinary NEW-audio capability.
        let foreign = self.policy.validate_manifest(message, session)?;
        let params = child(dictionary(message)?, "Params")?;
        let manifest = child(params, "AssetManifest")?;
        let assets = manifest["Media"]
            .as_array()
            .context("validated Media array missing")?;
        let own = assets
            .iter()
            .find_map(|value| {
                let asset = value.as_dictionary()?;
                (asset.get("AssetID")?.as_string()? == self.own_asset_id()).then_some(asset)
            })
            .context("validated pending own record missing")?;
        let own_parts = number(child(own, "Variant")?, "AssetParts")?;
        Ok(PendingAudioManifest { own_parts, foreign })
    }
}

impl EligibleSnapshot {
    /// Snapshot consistency only. A caller must separately bind the original
    /// source and fresh current-state proof.
    pub fn pending_audio_with_disabled_genius(
        &self,
        catalog: &Catalog,
        identity: &LibraryIdentity,
        own_asset_id: &str,
    ) -> Result<PendingAudioPolicy<'_>> {
        self.ensure_current(catalog, identity)?;
        let own_pid = asset_pid(own_asset_id)?;
        let mut matches = catalog
            .tracks()
            .iter()
            .filter(|track| track.sync_id == own_pid);
        let track = matches.next().context("pending audio target is absent")?;
        ensure!(
            matches.next().is_none()
                && self.occupied.contains(&own_pid)
                && track.afc_path.is_none()
                && !track.availability.has_non_purgeable_asset
                && !track.availability.is_placeholder
                && track.availability.is_library_added
                && track.availability.is_supported_type,
            "pending audio requires one represented supported pathless unfilled track"
        );
        Ok(PendingAudioPolicy {
            policy: AssetPolicy {
                snapshot: self,
                own: vec![own_asset_id.to_owned()],
                own_request_profile: OwnRequestProfile::AudioWithDisabledGenius,
                declared: Vec::new(),
            },
        })
    }
}

fn eligible_track(track: &Track) -> bool {
    // Catalog parsing already requires ordinary local media_type8/remote0.
    track.sync_id != 0
        && track.afc_path.is_some()
        && !track.availability.is_placeholder
        && track.availability.is_library_added
        && track.availability.has_non_purgeable_asset
        && track.availability.is_supported_type
}

pub type AudioPolicy<'a> = AssetPolicy<'a, 1>;
pub type ArtworkPolicy<'a> = AssetPolicy<'a, 2>;
pub type MetadataPolicy<'a> = AssetPolicy<'a, 0>;

/// Only the audio/new and artwork/existing constructors can create a policy.
/// The const part keeps their public session entrypoints distinct.
pub struct AssetPolicy<'a, const OWN_PART: u64> {
    snapshot: &'a EligibleSnapshot,
    /// One identity, except for an audio batch.
    own: Vec<String>,
    own_request_profile: OwnRequestProfile,
    /// The part mask of each `own` identity, for the Declared profile only.
    declared: Vec<u64>,
}

impl AssetPolicy<'_, 1> {
    pub fn manifest_profile(&self) -> AudioManifestProfile {
        match self.own_request_profile {
            OwnRequestProfile::SinglePart => AudioManifestProfile::AudioOnly,
            OwnRequestProfile::AudioWithDisabledGenius => {
                AudioManifestProfile::AudioWithDisabledGenius
            }
            OwnRequestProfile::Declared => AudioManifestProfile::AudioWithArtwork,
        }
    }
}

impl<const OWN_PART: u64> AssetPolicy<'_, OWN_PART> {
    /// The first (for every policy but an audio batch, the only) own identity.
    pub fn own_asset_id(&self) -> &str {
        &self.own[0]
    }

    pub fn own_asset_ids(&self) -> &[String] {
        &self.own
    }

    /// Session has independently matched Ready to the current Media anchor.
    /// The engine constructs the snapshot itself from its full identity and
    /// catalog; this wire check does not substitute XOR for full identity.
    pub fn ensure_session(&self, library_id: &str, media_anchor: u32) -> Result<()> {
        ensure!(
            library_id == self.snapshot.library_id && media_anchor == self.snapshot.binding.anchor,
            "ancillary policy differs from negotiated library/anchor"
        );
        Ok(())
    }

    /// Validate the complete incoming message atomically. No accepted prefix is
    /// returned on any error. The caller owns single-consumption/session state.
    pub fn validate_manifest(&self, message: &Value, session: u32) -> Result<Vec<String>> {
        let root = dictionary(message)?;
        exact_keys(root, &["Command", "Session", "Type", "Id", "Params"])?;
        ensure!(
            text(root, "Command")? == "AssetManifest"
                && number(root, "Session")? == u64::from(session)
                && session != 0
                && number(root, "Type")? == 0,
            "wrong manifest command/session/type"
        );
        number(root, "Id")?;
        let params = child(root, "Params")?;
        exact_keys(params, &["AssetManifest"])?;
        let manifest = child(params, "AssetManifest")?;
        exact_keys(manifest, &["Media"])?;
        let assets = manifest["Media"]
            .as_array()
            .context("Media manifest must be an array")?;
        ensure!(
            (OWN_PART == 0 || !assets.is_empty()) && assets.len() <= MAX_MANIFEST_ASSETS,
            "manifest count outside qualified bound"
        );
        let mut seen = BTreeSet::new();
        let mut own = 0;
        let mut declined = Vec::new();
        for asset in assets {
            let asset = dictionary(asset)?;
            exact_keys(asset, &["AssetID", "AssetType", "IsDownload", "Variant"])?;
            ensure!(
                text(asset, "AssetType")? == "Music"
                    && asset["IsDownload"].as_boolean() == Some(true),
                "unsupported asset class/direction"
            );
            let id = text(asset, "AssetID")?;
            let pid = asset_pid(id)?;
            ensure!(seen.insert(pid), "duplicate manifest track identity");
            let variant = child(asset, "Variant")?;
            exact_keys(variant, &["AssetParts"])?;
            let part = number(variant, "AssetParts")?;
            if self.own.iter().any(|own| own == id) {
                ensure!(
                    OWN_PART != 0,
                    "metadata-only target unexpectedly requested an asset"
                );
                match self.own_request_profile {
                    OwnRequestProfile::SinglePart => ensure!(
                        part == OWN_PART,
                        "own asset must request its exact qualified single part"
                    ),
                    OwnRequestProfile::AudioWithDisabledGenius => {
                        // Native AMP gives audio precedence over disabled Genius.
                        // Admit only masks 1 and 5; all other bits stay refused.
                        ensure!(
                            OWN_PART == 1 && matches!(part, 1 | 5),
                            "own audio asset has an unqualified disabled-Genius part mask"
                        );
                    }
                    OwnRequestProfile::Declared => {
                        let index = self
                            .own
                            .iter()
                            .position(|own| own == id)
                            .context("own asset vanished")?;
                        let declared = *self.declared.get(index).context("own mask absent")?;
                        ensure!(
                            part == declared || part == declared | 4,
                            "own asset must request exactly its declared parts"
                        );
                    }
                }
                own += 1;
            } else {
                let art = self
                    .snapshot
                    .tracks
                    .get(&pid)
                    .context("foreign asset is not an eligible existing track")?;
                match part {
                    2 => {
                        ensure!(
                            art.is_some_and(|id| id != 0),
                            "foreign artwork lacks the supported track registration"
                        );
                    }
                    4 => {}
                    _ => anyhow::bail!("foreign request is not artwork-only or Genius-only"),
                }
                declined.push(id.to_owned());
            }
        }
        ensure!(
            OWN_PART == 0 || own == self.own.len(),
            "manifest omitted own asset"
        );
        Ok(declined)
    }
}

fn asset_pid(id: &str) -> Result<u64> {
    let signed = id
        .parse::<i64>()
        .context("asset ID is not signed decimal")?;
    ensure!(
        signed != 0 && signed.to_string() == id,
        "asset ID is zero/noncanonical signed decimal"
    );
    Ok(signed as u64)
}
fn dictionary(value: &Value) -> Result<&Dictionary> {
    value.as_dictionary().context("expected dictionary")
}
fn child<'a>(parent: &'a Dictionary, key: &str) -> Result<&'a Dictionary> {
    dictionary(parent.get(key).with_context(|| format!("missing {key}"))?)
}
fn text<'a>(parent: &'a Dictionary, key: &str) -> Result<&'a str> {
    parent
        .get(key)
        .and_then(Value::as_string)
        .with_context(|| format!("missing/string type mismatch: {key}"))
}
fn number(parent: &Dictionary, key: &str) -> Result<u64> {
    parent
        .get(key)
        .and_then(Value::as_unsigned_integer)
        .with_context(|| format!("missing/unsigned type mismatch: {key}"))
}
fn exact_keys(dict: &Dictionary, keys: &[&str]) -> Result<()> {
    ensure!(
        dict.len() == keys.len() && keys.iter().all(|key| dict.contains_key(key)),
        "unexpected dictionary keys"
    );
    Ok(())
}

pub(crate) mod pending_delete;
pub use pending_delete::PendingDeletePolicy;

#[cfg(test)]
pub(crate) mod tests;
