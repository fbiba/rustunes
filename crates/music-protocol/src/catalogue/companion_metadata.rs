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

#[derive(Debug, Serialize)]
pub struct Guard {
    pub path: String,
    pub stage: String,
    pub expected_bytes: u64,
    pub expected_sha256: String,
    pub raw_local_file: String,
    pub status: String,
    pub before: Option<Attributes>,
    pub after: Option<Attributes>,
    pub retained_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub opened: bool,
    pub close_attempted: bool,
    pub close_acknowledged: bool,
    pub error: Option<String>,
}

pub fn matching_guards(guards: &[Guard]) -> Result<()> {
    ensure!(guards.len() == 4, "guard set incomplete");
    for (index, guard) in guards.iter().enumerate() {
        ensure!(
            guard.path == if index % 2 == 0 { PREFS } else { ARTWORKDB }
                && guard.stage == if index < 2 { "before" } else { "after" },
            "guard order differs"
        );
        ensure!(
            guard.status == "complete"
                && guard.opened
                && guard.close_acknowledged
                && guard.close_attempted
                && guard.retained_bytes == Some(guard.expected_bytes)
                && guard.sha256.as_deref() == Some(guard.expected_sha256.as_str()),
            "guard incomplete or bytes differ"
        );
        let before = guard
            .before
            .as_ref()
            .context("guard before attributes absent")?;
        let after = guard
            .after
            .as_ref()
            .context("guard after attributes absent")?;
        let observed = entry("guard", before.clone())?;
        ensure!(
            observed.kind == Kind::RegularFile
                && observed.size == guard.expected_bytes
                && stable(before) == stable(after),
            "guard attributes changed"
        );
    }
    for index in 0..2 {
        let before = &guards[index];
        let after = &guards[index + 2];
        ensure!(
            before.expected_bytes == after.expected_bytes
                && before.expected_sha256 == after.expected_sha256
                && stable(before.before.as_ref().unwrap()) == stable(after.after.as_ref().unwrap()),
            "guard attributes changed across tree passes"
        );
    }
    Ok(())
}

pub fn outcome(complete: bool) -> serde_json::Value {
    serde_json::json!({
        "status": if complete { "complete" } else { "incomplete" },
        "inventory_complete": complete,
        "atomic_snapshot_claimed": false,
        "restoration_or_mutation_authorized": false,
        "phone_writes": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn attrs(kind: &str) -> Attributes {
        BTreeMap::from([
            ("st_ifmt".into(), kind.into()),
            ("st_size".into(), "7".into()),
            ("st_mtime".into(), "23".into()),
            ("st_atime".into(), "10".into()),
        ])
    }
    fn snapshot() -> Snapshot {
        let mut result = Snapshot {
            completed: true,
            ..Snapshot::default()
        };
        for parent in ["iTunes_Control", "iTunes_Control/iTunes"] {
            result.ancestors.insert(parent.into(), attrs("S_IFDIR"));
        }
        for (role, root) in ROOTS {
            result.roots.insert(role.into(), "present".into());
            result
                .directory_names
                .insert(root.into(), vec!["returned-chunk".into()]);
            insert(
                &mut result,
                root.into(),
                entry(role, attrs("S_IFDIR")).unwrap(),
            )
            .unwrap();
            insert(
                &mut result,
                format!("{root}/returned-chunk"),
                entry(role, attrs("S_IFREG")).unwrap(),
            )
            .unwrap();
        }
        result
    }
    fn guards() -> Vec<Guard> {
        (0..4)
            .map(|index| Guard {
                path: if index % 2 == 0 {
                    PREFS.into()
                } else {
                    ARTWORKDB.into()
                },
                stage: if index < 2 {
                    "before".into()
                } else {
                    "after".into()
                },
                expected_bytes: 7,
                expected_sha256: "a".repeat(64),
                raw_local_file: "private-guard".into(),
                status: "complete".into(),
                before: Some(attrs("S_IFREG")),
                after: Some(attrs("S_IFREG")),
                retained_bytes: Some(7),
                sha256: Some("a".repeat(64)),
                opened: true,
                close_attempted: true,
                close_acknowledged: true,
                error: None,
            })
            .collect()
    }

    #[test]
    fn returned_names_define_paths_and_reject_unsafe_or_duplicate_components() {
        let (_, names) = super::names(b".\0..\0Purgable\0real-chunk\0").unwrap();
        assert_eq!(names, ["Purgable", "real-chunk"]);
        assert_eq!(
            child(ROOTS[0].1, ROOTS[0].1, &names[0]).unwrap().0,
            "iTunes_Control/Artwork/Purgable"
        );
        for raw in [
            b"x\0x\0".as_slice(),
            b"../x\0",
            b"x\\y\0",
            b"\xff\0",
            b"x\0\0",
            b"unterminated",
        ] {
            assert!(super::names(raw).is_err());
        }
        assert!(child(ROOTS[0].1, ROOTS[1].1, "x").is_err());
        assert!(child("arbitrary", "arbitrary", "x").is_err());
    }

    #[test]
    fn combined_budgets_and_depth_refuse_the_first_excess() {
        for (mut budget, limit) in [
            (Budget::companion_metadata(), MAX_DIRECTORIES),
            (Budget::artwork_discovery(), MAX_DISCOVERY_DIRECTORIES),
        ] {
            budget.names(MAX_ENTRIES - 1).unwrap();
            budget.names(1).unwrap();
            assert!(budget.names(1).is_err());
            for _ in 0..limit {
                budget.directory().unwrap();
            }
            assert!(budget.directory().is_err());
            for _ in 0..MAX_OPERATIONS {
                budget.operation().unwrap();
            }
            assert!(budget.operation().is_err());
            assert_eq!(budget.directories_listed, limit);
            assert_eq!(budget.returned_non_dot_entries, MAX_ENTRIES);
            assert_eq!(budget.operations_attempted, MAX_OPERATIONS);
        }
        let root = ROOTS[0].1;
        let parent = format!("{root}/{}", ["x"; MAX_DEPTH - 1].join("/"));
        let (deepest, depth) = child(root, &parent, "last").unwrap();
        assert_eq!(depth, MAX_DEPTH);
        assert!(child(root, &deepest, "excess").is_err());
    }

    #[test]
    fn full_attributes_reject_duplicates_unknown_kinds_and_symlinks() {
        assert!(attributes(b"st_size\0").is_err());
        assert!(attributes(b"st_size\07\0st_size\08\0").is_err());
        for kind in ["S_IFLNK", "S_IFIFO", "unknown"] {
            assert!(entry("role", attrs(kind)).is_err());
        }
        assert!(entry("role", BTreeMap::new()).is_err());
        let mut map = attrs("S_IFREG");
        map.insert("st_size".into(), "-1".into());
        assert!(entry("role", map).is_err());
        let mut snapshot = snapshot();
        assert!(
            insert(
                &mut snapshot,
                ROOTS[0].1.into(),
                entry("role", attrs("S_IFDIR")).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn two_passes_ignore_only_atime_and_reject_names_or_attribute_changes() {
        let before = snapshot();
        let mut after = snapshot();
        after
            .entries
            .get_mut(ROOTS[0].1)
            .unwrap()
            .attributes
            .insert("st_atime".into(), "999".into());
        matching_passes(&before, &after).unwrap();
        after
            .entries
            .get_mut(ROOTS[0].1)
            .unwrap()
            .attributes
            .insert("st_mtime".into(), "999".into());
        assert!(matching_passes(&before, &after).is_err());
        after = snapshot();
        after
            .directory_names
            .get_mut(ROOTS[1].1)
            .unwrap()
            .push("new".into());
        assert!(matching_passes(&before, &after).is_err());
        after = snapshot();
        after
            .entries
            .get_mut(ROOTS[1].1)
            .unwrap()
            .attributes
            .insert("new_attribute".into(), "yes".into());
        assert!(matching_passes(&before, &after).is_err());
    }

    #[test]
    fn changed_guards_and_lost_close_never_establish_inventory_completion() {
        let mut records = guards();
        matching_guards(&records).unwrap();
        records[3].close_acknowledged = false;
        assert!(matching_guards(&records).is_err());
        records = guards();
        records[2].sha256 = Some("b".repeat(64));
        assert!(matching_guards(&records).is_err());
        records = guards();
        records[2]
            .before
            .as_mut()
            .unwrap()
            .insert("st_mtime".into(), "24".into());
        records[2]
            .after
            .as_mut()
            .unwrap()
            .insert("st_mtime".into(), "24".into());
        assert!(matching_guards(&records).is_err());
        assert_eq!(outcome(false)["status"], "incomplete");
        assert_eq!(outcome(false)["inventory_complete"], false);
    }

    #[test]
    fn root_absence_is_separate_from_unknown_and_incomplete_passes() {
        let mut before = snapshot();
        let role = ROOTS[1].0;
        let root = ROOTS[1].1;
        before
            .roots
            .insert(role.into(), "canonically_absent".into());
        before
            .entries
            .retain(|path, _| path != root && !path.starts_with(&format!("{root}/")));
        before.directory_names.remove(root);
        let mut after = before.clone();
        matching_passes(&before, &after).unwrap();
        after.roots.insert(role.into(), "unknown".into());
        assert!(matching_passes(&before, &after).is_err());
        after = before.clone();
        after.completed = false;
        assert!(matching_passes(&before, &after).is_err());
    }

    #[test]
    fn receipt_coverage_rejects_missing_child_unlisted_entry_and_non_directory_list() {
        let root = ROOTS[0].1;
        let mut missing = snapshot();
        missing.entries.remove(&format!("{root}/returned-chunk"));
        assert!(matching_passes(&missing, &missing).is_err());
        let mut orphan = snapshot();
        insert(
            &mut orphan,
            format!("{root}/unlisted"),
            entry(ROOTS[0].0, attrs("S_IFREG")).unwrap(),
        )
        .unwrap();
        assert!(matching_passes(&orphan, &orphan).is_err());
        let mut file_list = snapshot();
        file_list
            .directory_names
            .insert(format!("{root}/returned-chunk"), vec![]);
        assert!(matching_passes(&file_list, &file_list).is_err());
        let mut unlisted_directory = snapshot();
        *unlisted_directory
            .entries
            .get_mut(&format!("{root}/returned-chunk"))
            .unwrap() = entry(ROOTS[0].0, attrs("S_IFDIR")).unwrap();
        assert!(matching_passes(&unlisted_directory, &unlisted_directory).is_err());
        let mut dangling = snapshot();
        dangling
            .directory_names
            .insert(format!("{root}/unknown"), vec![]);
        assert!(matching_passes(&dangling, &dangling).is_err());
    }

    #[test]
    fn fixed_directory_profiles_preserve_serialized_counters() {
        for mut budget in [Budget::companion_metadata(), Budget::artwork_discovery()] {
            budget.directory().unwrap();
            budget.names(7).unwrap();
            budget.operation().unwrap();
            assert_eq!(
                serde_json::to_string(&budget).unwrap(),
                "{\"returned_non_dot_entries\":7,\"directories_listed\":1,\"operations_attempted\":1}"
            );
        }
        let mut discovery = ArtworkDiscoveryBudget::default();
        for _ in 0..MAX_DISCOVERY_DIRECTORIES {
            discovery.directory().unwrap();
        }
        assert!(discovery.directory().is_err());
        assert_eq!(
            serde_json::to_string(&discovery).unwrap(),
            "{\"returned_non_dot_entries\":0,\"directories_listed\":512,\"operations_attempted\":0}"
        );
    }

    fn snapshot_with_directories(count: usize) -> Snapshot {
        let mut value = snapshot();
        let (role, root) = ROOTS[0];
        for index in 2..count {
            let name = format!("directory-{index:04}");
            let path = format!("{root}/{name}");
            insert(
                &mut value,
                path.clone(),
                entry(role, attrs("S_IFDIR")).unwrap(),
            )
            .unwrap();
            value.directory_names.insert(path, Vec::new());
            value.directory_names.get_mut(root).unwrap().push(name);
        }
        value.directory_names.get_mut(root).unwrap().sort();
        assert_eq!(value.directory_names.len(), count);
        value
    }

    #[test]
    fn retained_directory_profiles_refuse_first_excess() {
        let discovery_edge = snapshot_with_directories(MAX_DISCOVERY_DIRECTORIES / 2);
        matching_artwork_discovery_passes(&discovery_edge, &discovery_edge).unwrap();
        let discovery_excess = snapshot_with_directories(MAX_DISCOVERY_DIRECTORIES / 2 + 1);
        assert_eq!(
            matching_artwork_discovery_passes(&discovery_edge, &discovery_excess)
                .unwrap_err()
                .to_string(),
            "retained coverage exceeds combined traversal bounds"
        );
        matching_passes(&discovery_excess, &discovery_excess).unwrap();
        let companion_edge = snapshot_with_directories(MAX_DIRECTORIES / 2);
        matching_passes(&companion_edge, &companion_edge).unwrap();
        let companion_excess = snapshot_with_directories(MAX_DIRECTORIES / 2 + 1);
        assert_eq!(
            matching_passes(&companion_edge, &companion_excess)
                .unwrap_err()
                .to_string(),
            "retained coverage exceeds combined traversal bounds"
        );
    }
}
