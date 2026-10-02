//! Exact immediate companion observation. No producer authority.
use super::companion_metadata as metadata;
use anyhow::{Context, Result, ensure};
pub use metadata::{Attributes, Budget, Guard, Kind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub const ROOT: &str = "iTunes_Control/iTunes";
pub const GUARD_PATHS: [&str; 6] = [
    "iTunes_Control/iTunes/iTunesPrefs",
    "iTunes_Control/iTunes/iTunesPrefs.plist",
    "iTunes_Control/iTunes/iTunesCDB",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal",
    "iTunes_Control/Artwork/ArtworkDB",
];
pub const MAX_GUARD_BYTES: u64 = 64 << 20;
pub const MAX_GUARD_AGGREGATE: u64 = 256 << 20;
pub const MAX_CANONICAL: usize = 512;
pub const MAX_MANIFEST_BYTES: usize = 256 << 10;

pub fn required_option<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(d)
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Namespace {
    pub device: String,
    pub current_receipt_sha256: String,
    pub complete_index_sha256: String,
    pub database_sha256: String,
    #[serde(deserialize_with = "required_option")]
    pub wal_sha256: Option<String>,
    pub media_anchor: u64,
}
pub fn digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid SHA256"
    );
    Ok(())
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuardSpec {
    pub path: String,
    #[serde(deserialize_with = "required_option")]
    pub expected: Option<Expected>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub namespace: Namespace,
    pub guards: Vec<GuardSpec>,
    pub canonical_on_the_go_ids: Vec<u64>,
}
pub fn canonical_path(id: u64) -> Result<String> {
    ensure!(id > 0, "zero OnTheGo ID");
    Ok(format!("{ROOT}/OnTheGoPlaylist_{id}.plist"))
}
impl Manifest {
    pub fn validate(&self, device: &str) -> Result<()> {
        ensure!(
            self.schema == "music-protocol.companion-guard-input"
                && self.namespace.device == device
                && !device.is_empty(),
            "manifest schema/device differs"
        );
        for h in [
            &self.namespace.current_receipt_sha256,
            &self.namespace.complete_index_sha256,
            &self.namespace.database_sha256,
        ] {
            digest(h)?;
        }
        if let Some(h) = &self.namespace.wal_sha256 {
            digest(h)?;
        }
        ensure!(
            self.guards.len() == GUARD_PATHS.len(),
            "incomplete Current guard set"
        );
        let mut total = 0u64;
        for (i, (g, path)) in self.guards.iter().zip(GUARD_PATHS).enumerate() {
            ensure!(g.path == path, "unknown/changed Current guard path");
            if let Some(e) = &g.expected {
                digest(&e.sha256)?;
                ensure!(
                    e.bytes <= MAX_GUARD_BYTES && (e.bytes > 0 || i == 4),
                    "guard size unsupported"
                );
                total = total
                    .checked_add(e.bytes.checked_mul(2).context("guard aggregate overflow")?)
                    .context("guard aggregate overflow")?;
            } else {
                ensure!(i >= 4, "required Current guard absent");
            }
        }
        ensure!(
            total <= MAX_GUARD_AGGREGATE,
            "both-phase guard aggregate exceeds bound"
        );
        ensure!(
            self.guards[3].expected.as_ref().unwrap().sha256 == self.namespace.database_sha256
                && self.guards[4].expected.as_ref().map(|e| &e.sha256)
                    == self.namespace.wal_sha256.as_ref(),
            "SQL namespace differs"
        );
        ensure!(
            self.canonical_on_the_go_ids.len() <= MAX_CANONICAL
                && self.canonical_on_the_go_ids.windows(2).all(|w| w[0] < w[1]),
            "canonical IDs unsorted/duplicate/over bound"
        );
        for id in &self.canonical_on_the_go_ids {
            canonical_path(*id)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub completed: bool,
    pub ancestors: BTreeMap<String, Attributes>,
    pub entries: BTreeMap<String, metadata::Entry>,
    pub names: Vec<String>,
    pub canonical_stats: BTreeMap<String, Option<Attributes>>,
}
pub fn child(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\\')
            && !name.bytes().any(|b| b < 32 || b == 127),
        "unsafe companion component"
    );
    Ok(format!("{ROOT}/{name}"))
}
pub fn insert(snapshot: &mut Snapshot, path: String, attrs: Attributes) -> Result<()> {
    let e = metadata::entry("immediate_companion", attrs)?;
    ensure!(
        e.kind != Kind::RegularFile
            || e.attributes.get("st_nlink").map(String::as_str) == Some("1"),
        "unsupported regular-file link shape"
    );
    ensure!(
        snapshot.entries.insert(path, e).is_none(),
        "duplicate companion path"
    );
    Ok(())
}
fn consistent(s: &Snapshot, manifest: &Manifest) -> Result<()> {
    ensure!(
        s.completed && s.ancestors.len() == 2,
        "incomplete companion pass/ancestors"
    );
    for p in ["iTunes_Control", ROOT] {
        ensure!(
            metadata::entry(
                "ancestor",
                s.ancestors.get(p).context("missing ancestor")?.clone()
            )?
            .kind
                == Kind::Directory,
            "ancestor not directory"
        );
    }
    ensure!(
        s.names.len() <= metadata::MAX_ENTRIES && s.names.windows(2).all(|w| w[0] < w[1]),
        "names unordered/duplicate/over bound"
    );
    let paths: BTreeSet<_> = s.names.iter().map(|n| child(n)).collect::<Result<_>>()?;
    ensure!(
        paths == s.entries.keys().cloned().collect(),
        "missing child/unlisted companion entry"
    );
    for e in s.entries.values() {
        ensure!(
            e == &metadata::entry("immediate_companion", e.attributes.clone())?,
            "entry attrs/kind/size disagree"
        );
        ensure!(
            e.kind != Kind::RegularFile
                || e.attributes.get("st_nlink").map(String::as_str) == Some("1"),
            "unsupported regular-file link shape"
        );
    }
    let canonical: BTreeSet<_> = manifest
        .canonical_on_the_go_ids
        .iter()
        .map(|id| canonical_path(*id))
        .collect::<Result<_>>()?;
    ensure!(
        canonical == s.canonical_stats.keys().cloned().collect(),
        "incomplete canonical physical namespace"
    );
    for (p, attrs) in &s.canonical_stats {
        match attrs {
            Some(attrs) => {
                let e = s
                    .entries
                    .get(p)
                    .context("present canonical companion not returned")?;
                ensure!(
                    e.kind == Kind::RegularFile
                        && metadata::stable(attrs) == metadata::stable(&e.attributes),
                    "canonical stat differs/nonregular"
                );
            }
            None => ensure!(
                !s.entries.contains_key(p),
                "absent canonical companion was returned"
            ),
        }
    }
    Ok(())
}
pub fn matching_passes(a: &Snapshot, b: &Snapshot, manifest: &Manifest) -> Result<()> {
    consistent(a, manifest)?;
    consistent(b, manifest)?;
    ensure!(
        a.names
            .len()
            .checked_add(b.names.len())
            .is_some_and(|v| v <= metadata::MAX_ENTRIES),
        "both-pass name bound"
    );
    ensure!(
        a.names == b.names
            && a.entries.len() == b.entries.len()
            && a.canonical_stats.len() == b.canonical_stats.len(),
        "changed companion namespace"
    );
    for (p, attrs) in &a.ancestors {
        ensure!(
            metadata::stable(attrs) == metadata::stable(&b.ancestors[p]),
            "changed ancestor attrs"
        );
    }
    for (p, e) in &a.entries {
        let f = b.entries.get(p).context("changed companion path")?;
        ensure!(
            e.kind == f.kind
                && e.size == f.size
                && metadata::stable(&e.attributes) == metadata::stable(&f.attributes),
            "changed companion attrs/size"
        );
    }
    for (p, attrs) in &a.canonical_stats {
        ensure!(
            attrs.as_ref().map(metadata::stable)
                == b.canonical_stats[p].as_ref().map(metadata::stable),
            "changed canonical presence/attrs"
        );
    }
    Ok(())
}
pub fn matching_guards(guards: &[Guard], manifest: &Manifest) -> Result<()> {
    ensure!(guards.len() == 12, "incomplete guard captures");
    for (i, g) in guards.iter().enumerate() {
        let spec = &manifest.guards[i % 6];
        ensure!(
            g.path == spec.path
                && g.stage == if i < 6 { "before" } else { "after" }
                && g.error.is_none(),
            "guard path/stage/error differs"
        );
        if let Some(e) = &spec.expected {
            ensure!(
                g.status == "complete"
                    && g.expected_bytes == e.bytes
                    && g.expected_sha256 == e.sha256
                    && g.retained_bytes == Some(e.bytes)
                    && g.sha256.as_ref() == Some(&e.sha256)
                    && g.opened
                    && g.close_attempted
                    && g.close_acknowledged,
                "guard length/hash/acknowledged close differs"
            );
            let before = g.before.as_ref().context("guard before absent")?;
            let after = g.after.as_ref().context("guard after absent")?;
            ensure!(
                metadata::entry("guard", before.clone())?.kind == Kind::RegularFile
                    && metadata::entry("guard", before.clone())?.size == e.bytes
                    && before.get("st_nlink").map(String::as_str) == Some("1")
                    && metadata::stable(before) == metadata::stable(after),
                "guard attributes changed/unsupported link shape"
            );
            if i >= 6 {
                ensure!(
                    metadata::stable(before)
                        == metadata::stable(
                            guards[i - 6]
                                .before
                                .as_ref()
                                .context("first guard attrs absent")?
                        ),
                    "guard attrs differ across discovery"
                );
            }
        } else {
            ensure!(
                g.status == "canonically_absent"
                    && g.expected_bytes == 0
                    && g.expected_sha256.is_empty()
                    && g.before.is_none()
                    && g.after.is_none()
                    && g.retained_bytes == Some(0)
                    && g.sha256.is_none()
                    && !g.opened
                    && !g.close_attempted
                    && !g.close_acknowledged,
                "absence unproved"
            );
        }
    }
    Ok(())
}
pub fn matching_guard_namespace(s: &Snapshot, guards: &[Guard], manifest: &Manifest) -> Result<()> {
    consistent(s, manifest)?;
    matching_guards(guards, manifest)?;
    for (i, spec) in manifest.guards.iter().enumerate().take(5) {
        if let Some(expected) = &spec.expected {
            let entry = s
                .entries
                .get(&spec.path)
                .context("required Current guard not returned")?;
            ensure!(
                entry.kind == Kind::RegularFile
                    && entry.size == expected.bytes
                    && metadata::stable(&entry.attributes)
                        == metadata::stable(
                            guards[i].before.as_ref().context("guard attrs absent")?
                        ),
                "Current guard and companion namespace differ"
            );
        } else {
            ensure!(
                !s.entries.contains_key(&spec.path),
                "canonically absent guard returned in namespace"
            );
        }
    }
    Ok(())
}
pub fn outcome(complete: bool) -> serde_json::Value {
    serde_json::json!({"status":if complete {"complete"}else{"incomplete"},"inventory_complete":complete,"atomic_snapshot_claimed":false,"restoration_or_mutation_authorized":false,"phone_writes":0})
}
#[cfg(test)]
mod tests;
