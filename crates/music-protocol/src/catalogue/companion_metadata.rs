//! Pure acceptance rules for the two fixed artwork namespaces.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const ROOTS: [(&str, &str); 2] = [
    ("host_mapping", "iTunes_Control/Artwork"),
    ("permanent_payload", "iTunes_Control/iTunes/Artwork"),
];
pub const PREFS: &str = "iTunes_Control/iTunes/iTunesPrefs";
pub const ARTWORKDB: &str = "iTunes_Control/Artwork/ArtworkDB";
pub const MAX_ENTRIES: usize = 10_000;
pub const MAX_DIRECTORIES: usize = 4096;
/// The artwork discovery profile permits at most 512 combined directory reads.
pub const MAX_DISCOVERY_DIRECTORIES: usize = 512;
pub const MAX_DEPTH: usize = 8;
pub const MAX_OPERATIONS: usize = 27_000;
pub const MAX_NAMES_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_ATTR_BYTES: usize = 64 * 1024;
pub const MAX_GUARD_READS: usize = 4096;
pub type Attributes = BTreeMap<String, String>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Directory,
    RegularFile,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub role: String,
    pub kind: Kind,
    pub size: u64,
    pub attributes: Attributes,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub completed: bool,
    /// An absent root has no entry. Unknown failures never produce this status.
    pub roots: BTreeMap<String, String>,
    pub ancestors: BTreeMap<String, Attributes>,
    pub entries: BTreeMap<String, Entry>,
    pub directory_names: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Copy, Debug)]
enum DirectoryProfile {
    CompanionMetadata,
    ArtworkDiscovery,
}
impl DirectoryProfile {
    fn limit(self) -> usize {
        match self {
            Self::CompanionMetadata => MAX_DIRECTORIES,
            Self::ArtworkDiscovery => MAX_DISCOVERY_DIRECTORIES,
        }
    }
}

/// Counters for one of the two fixed observation profiles. The profile is not evidence data.
#[derive(Debug, Serialize)]
pub struct Budget {
    pub returned_non_dot_entries: usize,
    pub directories_listed: usize,
    pub operations_attempted: usize,
    #[serde(skip)]
    directory_profile: DirectoryProfile,
}

impl Default for Budget {
    fn default() -> Self {
        Self::companion_metadata()
    }
}

impl Budget {
    /// The companion metadata profile permits at most 4096 combined directory reads.
    pub fn companion_metadata() -> Self {
        Self::new(DirectoryProfile::CompanionMetadata)
    }

    /// The artwork discovery profile permits at most 512 combined directory reads.
    pub fn artwork_discovery() -> Self {
        Self::new(DirectoryProfile::ArtworkDiscovery)
    }

    fn new(directory_profile: DirectoryProfile) -> Self {
        Self {
            returned_non_dot_entries: 0,
            directories_listed: 0,
            operations_attempted: 0,
            directory_profile,
        }
    }

    pub fn operation(&mut self) -> Result<()> {
        ensure!(
            self.operations_attempted < MAX_OPERATIONS,
            "operation budget exhausted"
        );
        self.operations_attempted += 1;
        Ok(())
    }

    pub fn names(&mut self, count: usize) -> Result<()> {
        let total = self
            .returned_non_dot_entries
            .checked_add(count)
            .context("entry count overflow")?;
        ensure!(total <= MAX_ENTRIES, "returned entry budget exhausted");
        self.returned_non_dot_entries = total;
        Ok(())
    }

    pub fn directory(&mut self) -> Result<()> {
        ensure!(
            self.directories_listed < self.directory_profile.limit(),
            "directory budget exhausted"
        );
        self.directories_listed += 1;
        Ok(())
    }
}

/// A `Budget` whose `Default` is the artwork discovery profile (512 combined
/// directory reads). Counter access and serialization delegate to `Budget`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ArtworkDiscoveryBudget(Budget);
impl Default for ArtworkDiscoveryBudget {
    fn default() -> Self {
        Self(Budget::artwork_discovery())
    }
}
impl std::ops::Deref for ArtworkDiscoveryBudget {
    type Target = Budget;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ArtworkDiscoveryBudget {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn strings(bytes: &[u8], cap: usize, count_cap: usize) -> Result<Vec<String>> {
    ensure!(
        bytes.len() <= cap,
        "metadata reply exceeds accepted byte bound"
    );
    // An empty directory may return an empty string list. A nonempty list must
    // be fully NUL terminated. Attribute values can be empty; callers reject
    // empty directory names or attribute keys without dropping components.
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        bytes.last() == Some(&0),
        "unterminated metadata string list"
    );
    let mut result = Vec::new();
    for raw in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        ensure!(
            result.len() < count_cap,
            "metadata string count exceeds bound"
        );
        result.push(
            std::str::from_utf8(raw)
                .context("non-UTF8 metadata string")?
                .to_owned(),
        );
    }
    Ok(result)
}

pub fn names(bytes: &[u8]) -> Result<(Vec<String>, Vec<String>)> {
    let raw = strings(bytes, MAX_NAMES_BYTES, MAX_ENTRIES + 2)?;
    let mut seen = BTreeSet::new();
    let mut canonical = Vec::new();
    for name in &raw {
        ensure!(
            seen.insert(name.as_str()),
            "duplicate returned directory name"
        );
        if name == "." || name == ".." {
            continue;
        }
        component(name)?;
        canonical.push(name.clone());
    }
    canonical.sort();
    Ok((raw, canonical))
}

fn component(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "unsafe AFC path component"
    );
    Ok(())
}

pub fn child(root: &str, parent: &str, name: &str) -> Result<(String, usize)> {
    ensure!(
        ROOTS.iter().any(|(_, fixed)| *fixed == root),
        "unknown discovery root"
    );
    ensure!(
        parent == root || parent.starts_with(&format!("{root}/")),
        "parent escaped fixed root"
    );
    component(name)?;
    for part in parent.split('/') {
        component(part)?;
    }
    let path = format!("{parent}/{name}");
    let depth = path[root.len() + 1..].split('/').count();
    ensure!(
        depth <= MAX_DEPTH && path.len() <= 4096,
        "tree depth/path bound exceeded"
    );
    Ok((path, depth))
}

pub fn attributes(bytes: &[u8]) -> Result<Attributes> {
    let parts = strings(bytes, MAX_ATTR_BYTES, 256)?;
    ensure!(
        parts.len().is_multiple_of(2),
        "odd attribute key/value count"
    );
    let mut map = Attributes::new();
    for pair in parts.chunks_exact(2) {
        ensure!(
            !pair[0].is_empty() && pair[0].len() <= 128 && pair[1].len() <= 4096,
            "attribute component exceeds bound or key is empty"
        );
        ensure!(
            !pair[0].chars().any(char::is_control),
            "unsafe attribute key"
        );
        ensure!(
            map.insert(pair[0].clone(), pair[1].clone()).is_none(),
            "duplicate AFC attribute key"
        );
    }
    Ok(map)
}

pub fn entry(role: &str, attributes: Attributes) -> Result<Entry> {
    let kind = match attributes.get("st_ifmt").map(String::as_str) {
        Some("S_IFDIR") => Kind::Directory,
        Some("S_IFREG") => Kind::RegularFile,
        _ => anyhow::bail!("unsupported file kind or symlink"),
    };
    let size = attributes.get("st_size").context("missing st_size")?;
    ensure!(
        !size.is_empty() && size.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid st_size"
    );
    let size = size.parse().context("st_size overflow")?;
    Ok(Entry {
        role: role.to_owned(),
        kind,
        size,
        attributes,
    })
}

pub fn stable(attributes: &Attributes) -> Attributes {
    attributes
        .iter()
        .filter(|(key, _)| key.as_str() != "st_atime")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn insert(snapshot: &mut Snapshot, path: String, entry: Entry) -> Result<()> {
    ensure!(
        ROOTS.iter().any(|(role, root)| entry.role == *role
            && (path == *root || path.starts_with(&format!("{root}/")))),
        "entry escaped role/root"
    );
    ensure!(
        !snapshot.entries.contains_key(&path),
        "duplicate discovered path"
    );
    snapshot.entries.insert(path, entry);
    Ok(())
}

fn consistent_coverage(snapshot: &Snapshot) -> Result<()> {
    let ancestor_paths: BTreeSet<_> = ["iTunes_Control", "iTunes_Control/iTunes"]
        .into_iter()
        .map(String::from)
        .collect();
    ensure!(
        snapshot.ancestors.keys().cloned().collect::<BTreeSet<_>>() == ancestor_paths,
        "fixed ancestor observations incomplete"
    );
    for attrs in snapshot.ancestors.values() {
        ensure!(
            entry("ancestor", attrs.clone())?.kind == Kind::Directory,
            "ancestor is not a supported directory"
        );
    }
    for (path, entry) in &snapshot.entries {
        let (_, root) = ROOTS
            .iter()
            .find(|(role, root)| {
                entry.role == *role && (path == *root || path.starts_with(&format!("{root}/")))
            })
            .context("entry escaped fixed role/root")?;
        let parsed = self::entry(&entry.role, entry.attributes.clone())?;
        ensure!(
            entry.kind == parsed.kind && entry.size == parsed.size,
            "entry kind/size disagrees with raw attributes"
        );
        if entry.kind == Kind::Directory {
            ensure!(
                snapshot.directory_names.contains_key(path),
                "directory has no names observation"
            );
        }
        if path != root {
            let (parent, name) = path
                .rsplit_once('/')
                .context("non-root entry has no parent")?;
            ensure!(
                child(root, parent, name)?.0 == *path,
                "noncanonical child path"
            );
            let parent_entry = snapshot
                .entries
                .get(parent)
                .context("child parent absent")?;
            let names = snapshot
                .directory_names
                .get(parent)
                .context("child parent names absent")?;
            ensure!(
                parent_entry.kind == Kind::Directory
                    && parent_entry.role == entry.role
                    && names
                        .binary_search_by(|candidate| candidate.as_str().cmp(name))
                        .is_ok(),
                "entry is not named by its directory parent"
            );
        }
    }
    for (path, names) in &snapshot.directory_names {
        let directory = snapshot
            .entries
            .get(path)
            .context("names record belongs to an absent node")?;
        ensure!(
            directory.kind == Kind::Directory,
            "names record belongs to a non-directory node"
        );
        ensure!(
            names.windows(2).all(|pair| pair[0] < pair[1]),
            "names are not unique/canonical"
        );
        let (_, root) = ROOTS
            .iter()
            .find(|(role, _)| directory.role == *role)
            .context("directory role unknown")?;
        for name in names {
            let (child, _) = child(root, path, name)?;
            let observed = snapshot
                .entries
                .get(&child)
                .context("returned child has no entry observation")?;
            ensure!(
                observed.role == directory.role,
                "child role differs from its parent"
            );
        }
    }
    Ok(())
}

/// Verify the companion metadata profile, including its combined 4096-directory bound.
pub fn matching_passes(before: &Snapshot, after: &Snapshot) -> Result<()> {
    matching_passes_for_profile(before, after, DirectoryProfile::CompanionMetadata)
}

/// Verify the artwork discovery profile with its stricter 512-directory bound.
pub fn matching_artwork_discovery_passes(before: &Snapshot, after: &Snapshot) -> Result<()> {
    matching_passes_for_profile(before, after, DirectoryProfile::ArtworkDiscovery)
}

fn matching_passes_for_profile(
    before: &Snapshot,
    after: &Snapshot,
    profile: DirectoryProfile,
) -> Result<()> {
    ensure!(before.completed && after.completed, "tree pass incomplete");
    let roles: BTreeSet<_> = ROOTS.iter().map(|(role, _)| role.to_string()).collect();
    ensure!(
        before.roots.keys().cloned().collect::<BTreeSet<_>>() == roles
            && after.roots.keys().cloned().collect::<BTreeSet<_>>() == roles,
        "missing fixed root observation"
    );
    ensure!(
        before.roots == after.roots
            && before
                .roots
                .values()
                .all(|value| value == "present" || value == "canonically_absent"),
        "root changed or unknown"
    );
    let listed_directories = before
        .directory_names
        .len()
        .checked_add(after.directory_names.len())
        .context("directory count overflow")?;
    let names_total = before
        .directory_names
        .values()
        .chain(after.directory_names.values())
        .try_fold(0usize, |total, names| total.checked_add(names.len()))
        .context("returned names count overflow")?;
    ensure!(
        listed_directories <= profile.limit() && names_total <= MAX_ENTRIES,
        "retained coverage exceeds combined traversal bounds"
    );
    for snapshot in [before, after] {
        for (role, root) in ROOTS {
            let entry = snapshot.entries.get(root);
            ensure!(
                if snapshot.roots[role] == "present" {
                    entry.is_some_and(|entry| entry.role == role && entry.kind == Kind::Directory)
                        && snapshot.directory_names.contains_key(root)
                } else {
                    entry.is_none()
                        && !snapshot
                            .entries
                            .keys()
                            .any(|path| path.starts_with(&format!("{root}/")))
                        && !snapshot.directory_names.contains_key(root)
                },
                "root observation and retained entries disagree"
            );
        }
        consistent_coverage(snapshot)?;
    }
    ensure!(
        before.directory_names == after.directory_names,
        "directory names changed"
    );
    ensure!(
        before.ancestors.keys().eq(after.ancestors.keys()),
        "ancestor set changed"
    );
    for (path, attrs) in &before.ancestors {
        ensure!(
            stable(attrs) == stable(&after.ancestors[path]),
            "ancestor attributes changed"
        );
    }
    ensure!(
        before.entries.keys().eq(after.entries.keys()),
        "entry paths changed"
    );
    for (path, entry) in &before.entries {
        let other = &after.entries[path];
        ensure!(
            entry.role == other.role
                && entry.kind == other.kind
                && entry.size == other.size
                && stable(&entry.attributes) == stable(&other.attributes),
            "entry attributes changed"
        );
    }
    Ok(())
}
