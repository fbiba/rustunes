use super::*;

const AUDIO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/silence-alac.m4a"
));

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "source-collection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn names(selection: &Selection) -> Vec<&str> {
    selection
        .files()
        .iter()
        .map(SelectedSource::relative_path)
        .collect()
}

#[test]
fn default_recursive_selection_reports_skipped_roots_without_counting_descendants() {
    let tree = Tree::new();
    for name in [
        "a.m4a",
        "z.M4A",
        "Album/b.m4a",
        "backup/x.m4a",
        "BACKUPS/x.m4a",
        "old-backup-2026/x.m4a",
        "save-backup/x.m4a",
        ".hidden/x.m4a",
    ] {
        tree.write(name, AUDIO);
    }
    tree.write("song.ogg", b"outside source scope");
    tree.write("song.flac", AUDIO);
    let selected = select(&tree.0, &SelectionOptions::default()).unwrap();
    // A directory named like a backup is an ordinary directory.
    assert_eq!(
        names(&selected),
        [
            "Album/b.m4a",
            "BACKUPS/x.m4a",
            "a.m4a",
            "backup/x.m4a",
            "old-backup-2026/x.m4a",
            "save-backup/x.m4a",
            "song.flac",
            "z.M4A"
        ]
    );
    assert_eq!(
        selected.stats().excluded[&ExclusionReason::HiddenDirectory],
        1
    );
    assert_eq!(
        selected.stats().excluded[&ExclusionReason::UnsupportedExtension],
        1
    );
    assert!(
        selected
            .exclusions()
            .iter()
            .filter(|e| e.reason == ExclusionReason::HiddenDirectory)
            .all(|e| e.descendants_unscanned)
    );
    assert_eq!(selected.stats().selected_bytes, AUDIO.len() as u64 * 8);
    assert_eq!(selected.stats().visited_directories, 6);
}

#[test]
fn exact_subtree_selection_exclusion_and_recursion_are_explicit() {
    let tree = Tree::new();
    for name in [
        "a.m4a",
        "Album/b.m4a",
        "Album/c.m4a",
        "backups/x.m4a",
        ".hidden/y.m4a",
    ] {
        tree.write(name, AUDIO);
    }
    let options = SelectionOptions {
        include: BTreeSet::from(["Album".into(), "a.m4a".into()]),
        exclude: BTreeSet::from(["Album/b.m4a".into()]),
        ..Default::default()
    };
    assert_eq!(
        names(&select(&tree.0, &options).unwrap()),
        ["Album/c.m4a", "a.m4a"]
    );
    let selected = select(&tree.0, &SelectionOptions::default()).unwrap();
    assert!(names(&selected).contains(&"backups/x.m4a"));
    assert!(!names(&selected).contains(&".hidden/y.m4a"));
    assert_eq!(
        names(&select(&tree.0.join(".hidden"), &SelectionOptions::default()).unwrap()),
        ["y.m4a"]
    );
    assert_eq!(
        names(&select(&tree.0.join("backups/x.m4a"), &SelectionOptions::default()).unwrap()),
        ["x.m4a"]
    );
    let shallow = SelectionOptions {
        recursive: false,
        ..Default::default()
    };
    assert_eq!(names(&select(&tree.0, &shallow).unwrap()), ["a.m4a"]);
}

#[test]
fn invalid_and_unresolved_selectors_and_excess_depth_refuse_complete_selection() {
    let tree = Tree::new();
    tree.write("ok.m4a", AUDIO);
    for path in ["../escape", "/absolute", "a//b", "a\\b", ".", "missing.m4a"] {
        let options = SelectionOptions {
            include: BTreeSet::from([path.into()]),
            ..Default::default()
        };
        assert!(select(&tree.0, &options).is_err(), "{path}");
    }
    let mut nested = tree.0.clone();
    for _ in 0..=MAX_DEPTH {
        nested.push("d");
        fs::create_dir(&nested).unwrap();
    }
    assert!(select(&tree.0, &SelectionOptions::default()).is_err());
}

#[test]
fn changed_and_invalid_m4a_and_mp3_files_are_visible_failures() {
    let tree = Tree::new();
    tree.write("good.m4a", AUDIO);
    tree.write("bad.m4a", b"not an MP4");
    tree.write("skip.mp3", b"ignored");
    let selected = select(&tree.0, &SelectionOptions::default()).unwrap();
    let result = inspect(&selected).unwrap();
    assert_eq!(result.tracks.len(), 1);
    assert_eq!(result.failures.len(), 2);
    assert_eq!(result.failures[0].relative_path, "bad.m4a");
    assert!(result.ensure_complete().is_err());
    tree.write("good.m4a", b"changed");
    let changed = inspect(&selected).unwrap();
    assert_eq!(changed.failures.len(), 3);
    assert!(
        changed
            .failures
            .iter()
            .any(|failure| failure.message.contains("changed since discovery"))
    );
}

fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    [
        (payload.len() as u32 + 8).to_be_bytes().as_slice(),
        kind,
        payload,
    ]
    .concat()
}

fn replace(data: &[u8], path: &[[u8; 4]], replacement: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut at = 0;
    let mut found = false;
    while at < data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let current = &data[at..at + size];
        if current[4..8] == path[0] {
            found = true;
            if path.len() == 1 {
                result.extend_from_slice(replacement);
            } else {
                result.extend(atom(
                    &path[0],
                    &replace(&current[8..], &path[1..], replacement),
                ));
            }
        } else {
            result.extend_from_slice(current);
        }
        at += size;
    }
    assert!(found);
    result
}

fn tags(artists: &[&str], title: &str) -> Vec<u8> {
    let text = |value: &str| {
        atom(
            b"data",
            &[&1_u32.to_be_bytes()[..], &[0; 4], value.as_bytes()].concat(),
        )
    };
    let artist = atom(
        b"\xa9ART",
        &artists
            .iter()
            .flat_map(|value| text(value))
            .collect::<Vec<_>>(),
    );
    let title = atom(b"\xa9nam", &text(title));
    let meta = atom(
        b"meta",
        &[&[0; 4][..], &atom(b"ilst", &[artist, title].concat())].concat(),
    );
    replace(AUDIO, &[*b"moov", *b"udta"], &atom(b"udta", &meta))
}

#[test]
fn repeated_values_survive_and_audio_duplicate_variants_remain_visible() {
    let tree = Tree::new();
    let same = tags(&["First", "Second", "First"], "Title");
    tree.write("a.m4a", &same);
    tree.write("b.m4a", &same);
    tree.write(
        "variant.m4a",
        &tags(&["First", "Second", "First"], "Changed title"),
    );
    let selected = select(&tree.0, &SelectionOptions::default()).unwrap();
    let result = inspect(&selected).unwrap();
    result.ensure_complete().unwrap();
    assert_eq!(result.tracks.len(), 3);
    assert_eq!(
        result.tracks[0].metadata.artist,
        ["First", "Second", "First"]
    );
    assert!(result.tracks[0].metadata.album_artist.is_empty());
    assert_eq!(result.duplicates.len(), 1);
    let group = &result.duplicates[0];
    assert_eq!(group.relative_paths, ["a.m4a", "b.m4a", "variant.m4a"]);
    assert!(!group.identical_complete_files && !group.identical_standard_metadata);
    assert!(result.ensure_unambiguous().is_err());
    let options = SelectionOptions {
        exclude: BTreeSet::from(["variant.m4a".into()]),
        ..Default::default()
    };
    let exact = inspect(&select(&tree.0, &options).unwrap()).unwrap();
    assert!(exact.duplicates[0].identical_complete_files);
    assert!(exact.duplicates[0].identical_standard_metadata);
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_followed_even_if_requested_or_changed_after_discovery() {
    let tree = Tree::new();
    tree.write("real.m4a", AUDIO);
    std::os::unix::fs::symlink(&tree.0, tree.0.join("loop")).unwrap();
    std::os::unix::fs::symlink(tree.0.join("real.m4a"), tree.0.join("link.m4a")).unwrap();
    let selected = select(&tree.0, &SelectionOptions::default()).unwrap();
    assert_eq!(names(&selected), ["real.m4a"]);
    assert_eq!(selected.stats().excluded[&ExclusionReason::Symlink], 2);
    assert!(select(&tree.0.join("link.m4a"), &SelectionOptions::default()).is_err());
    fs::remove_file(tree.0.join("real.m4a")).unwrap();
    std::os::unix::fs::symlink("link.m4a", tree.0.join("real.m4a")).unwrap();
    let changed = inspect(&selected).unwrap();
    assert_eq!(changed.failures.len(), 1);
    assert!(changed.failures[0].message.contains("nonsymlink"));
}

#[test]
fn source_scan_includes_mp3_and_never_allocates_alac_records() {
    let t = Tree::new();
    fs::write(
        t.0.join("x.MP3"),
        music_protocol::test_support::mpeg::audio(),
    )
    .unwrap();
    let selected = select(&t.0, &SelectionOptions::default()).unwrap();
    let inspected = inspect(&selected).unwrap();
    assert!(inspected.tracks.is_empty());
    assert_eq!(inspected.mp3_tracks.len(), 1);
    assert_eq!(inspected.inspected_len(), 1);
    assert_eq!(inspected.inspected_bytes().unwrap(), 834);
    fs::write(t.0.join("x.MP3"), b"changed").unwrap();
    assert_eq!(inspect(&selected).unwrap().failures.len(), 1);
}

#[test]
fn a_path_is_reached_exactly_when_discovery_would_select_a_file_there() {
    let tree = Tree::new();
    let files = [
        "a.m4a",
        "Album/b.m4a",
        "Album/Disc 2/c.m4a",
        "Other/d.mp3",
        "backups/x.m4a",
        "old-backup/y.m4a",
        ".hidden/z.m4a",
    ];
    for name in files {
        tree.write(name, AUDIO);
    }
    let root = fs::canonicalize(&tree.0).unwrap();
    let selector = |include: &[&str], exclude: &[&str]| SelectionOptions {
        include: include.iter().map(|s| s.to_string()).collect(),
        exclude: exclude.iter().map(|s| s.to_string()).collect(),
        ..SelectionOptions::default()
    };
    for options in [
        SelectionOptions::default(),
        SelectionOptions {
            recursive: false,
            ..SelectionOptions::default()
        },
        selector(&["Album"], &[]),
        selector(&["Album/Disc 2/c.m4a", "a.m4a"], &[]),
        selector(&[], &["Album/Disc 2", "a.m4a"]),
        selector(&["Album"], &["Album/b.m4a"]),
    ] {
        let selected = select(&root, &options).unwrap();
        for name in files {
            assert_eq!(
                reaches(&root, &options, &root.join(name)),
                names(&selected).contains(&name),
                "{name} under {options:?}"
            );
        }
    }
    // The rule needs no file, nor its directory.
    let options = SelectionOptions::default();
    assert!(reaches(&root, &options, &root.join("Gone/gone.m4a")));
    assert!(!reaches(
        &root,
        &selector(&[], &["Gone"]),
        &root.join("Gone/gone.m4a")
    ));
    assert!(reaches(&root, &options, &root.join("Gone/backup/gone.m4a")));
    assert!(!reaches(
        &root,
        &options,
        &root.join("Gone/.hidden/gone.m4a")
    ));
    assert!(!reaches(&root, &options, &root));
    assert!(!reaches(&root, &options, Path::new("/elsewhere/a.m4a")));
    assert!(!reaches(&root, &options, &root.join("Album/../a.m4a")));
    assert!(!reaches(&root.join("Album"), &options, &root.join("a.m4a")));
}
