use super::*;
use music_protocol::media::fingerprint;

struct Temp(PathBuf);
impl Temp {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("source-reads-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn alac(dir: &Path) -> PathBuf {
    let path = dir.join("tone.m4a");
    std::fs::write(
        &path,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/tone-alac.m4a"
        )),
    )
    .unwrap();
    path
}

fn artwork_of(size: u64) -> artwork::SourceArtwork {
    let bytes = b"\xff\xd8\xff\xe0 not really a JPEG".to_vec();
    artwork::SourceArtwork {
        source_size: size,
        source_sha256: [7; 32],
        cover: Some(artwork::Cover {
            sha256: Sha256::digest(&bytes).into(),
            checksum: bytes.len() as u32 + artwork::MP4_CHECKSUM_HEADER,
            bytes,
        }),
        embedded: Some(artwork::EmbeddedImage {
            format: "gif",
            sha256: [9; 32],
            width: 600,
            height: 400,
        }),
        left_out: None,
    }
}

fn same_artwork(left: &artwork::SourceArtwork, right: &artwork::SourceArtwork) -> bool {
    let cover = |a: &artwork::SourceArtwork| {
        a.cover
            .as_ref()
            .map(|c| (c.bytes.clone(), c.sha256, c.checksum))
    };
    let image = |a: &artwork::SourceArtwork| {
        a.embedded
            .as_ref()
            .map(|p| (p.format, p.sha256, p.width, p.height))
    };
    left.source_size == right.source_size
        && left.source_sha256 == right.source_sha256
        && cover(left) == cover(right)
        && image(left) == image(right)
        && left.left_out == right.left_out
}

#[test]
fn a_changed_file_is_read_again_and_an_unchanged_one_is_kept() {
    let dir = Temp::new("memo");
    let path = dir.0.join("a.m4a");
    std::fs::write(&path, b"one").unwrap();
    let store = Mutex::new(None);
    let reads = AtomicUsize::new(0);
    let read = |p: &Path| -> Result<Vec<u8>> {
        reads.fetch_add(1, Ordering::Relaxed);
        Ok(std::fs::read(p)?)
    };
    assert_eq!(kept(&store, &path, read).unwrap(), b"one");
    assert_eq!(kept(&store, &path, read).unwrap(), b"one");
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    // Same size, new contents: mtime/ctime move, so it is read again.
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&path, b"two").unwrap();
    assert_eq!(kept(&store, &path, read).unwrap(), b"two");
    assert_eq!(reads.load(Ordering::Relaxed), 2);
    // Failures are not kept.
    let failing = |_: &Path| -> Result<Vec<u8>> { anyhow::bail!("unreadable") };
    let other = dir.0.join("b.m4a");
    std::fs::write(&other, b"x").unwrap();
    assert!(kept(&store, &other, failing).is_err());
    assert_eq!(kept(&store, &other, read).unwrap(), b"x");
}

#[test]
fn reads_persist_across_runs_only_for_the_same_file_identity() {
    let dir = Temp::new("disk");
    let path = alac(&dir.0);
    let key = key(&path).unwrap();
    let fingerprint = fingerprint::read(&path).unwrap();
    let art = artwork_of(key.size);
    let cache = dir.0.join("cache");

    let first = Disk::new(cache.clone(), true);
    assert!(first.load::<fingerprint::SourceFingerprint>(&key).is_none());
    assert!(!cache.exists(), "a lookup creates nothing");
    first.store(&key, &fingerprint);
    first.store(&key, &art);
    assert_eq!(first.stored.load(Ordering::Relaxed), 2);

    // A later run opens the same database.
    let later = Disk::new(cache.clone(), true);
    assert!(later.load::<fingerprint::SourceFingerprint>(&key) == Some(fingerprint.clone()));
    assert!(same_artwork(
        &later.load::<artwork::SourceArtwork>(&key).unwrap(),
        &art
    ));
    assert_eq!(later.reused.load(Ordering::Relaxed), 2);
    // A file that goes without a cover keeps the reason.
    let left_out = artwork::SourceArtwork {
        cover: None,
        embedded: None,
        left_out: Some("it holds 2 cover images, so it goes without a cover".into()),
        ..artwork_of(key.size)
    };
    later.store(&key, &left_out);
    assert!(same_artwork(
        &later.load::<artwork::SourceArtwork>(&key).unwrap(),
        &left_out
    ));
    later.store(&key, &art);

    // Any change to the file's identity is a miss.
    for changed in [
        Key {
            ctime: (key.ctime.0, key.ctime.1 + 1),
            ..key.clone()
        },
        Key {
            mtime: (key.mtime.0 + 1, key.mtime.1),
            ..key.clone()
        },
        Key {
            ino: key.ino + 1,
            ..key.clone()
        },
        Key {
            size: key.size + 1,
            ..key.clone()
        },
        Key {
            path: dir.0.join("other.m4a"),
            ..key.clone()
        },
    ] {
        assert!(
            later
                .load::<fingerprint::SourceFingerprint>(&changed)
                .is_none()
        );
        assert!(later.load::<artwork::SourceArtwork>(&changed).is_none());
    }

    // Full readback reads again but still stores.
    let refresh = Disk::new(cache.clone(), false);
    assert!(
        refresh
            .load::<fingerprint::SourceFingerprint>(&key)
            .is_none()
    );
    refresh.store(&key, &fingerprint);
    assert_eq!(refresh.stored.load(Ordering::Relaxed), 1);
    assert!(!refresh.warned.load(Ordering::Relaxed));
    assert!(!later.warned.load(Ordering::Relaxed));
}

#[test]
fn a_damaged_cover_or_an_old_row_is_a_miss() {
    let dir = Temp::new("damage");
    let path = alac(&dir.0);
    let key = key(&path).unwrap();
    let cache = dir.0.join("cache");
    let disk = Disk::new(cache.clone(), true);
    disk.store(&key, &artwork_of(key.size));
    disk.store(&key, &fingerprint::read(&path).unwrap());

    let c = Connection::open(cache.join("sources.sqlite")).unwrap();
    c.execute("UPDATE covers SET bytes=x'00'", []).unwrap();
    let damaged = Disk::new(cache.clone(), true);
    assert!(damaged.load::<artwork::SourceArtwork>(&key).is_none());
    assert!(damaged.warned.load(Ordering::Relaxed));

    c.execute("UPDATE fingerprints SET stored=0", []).unwrap();
    c.execute("UPDATE artwork SET stored=0", []).unwrap();
    let aged = Disk::new(cache.clone(), true);
    assert!(aged.load::<fingerprint::SourceFingerprint>(&key).is_none());
    let left: i64 = c
        .query_row(
            "SELECT (SELECT count(*) FROM fingerprints)+(SELECT count(*) FROM artwork)+(SELECT count(*) FROM covers)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        left, 0,
        "old rows and the covers only they named are dropped"
    );
}

#[test]
fn an_unfamiliar_database_is_set_aside_and_rebuilt() {
    let dir = Temp::new("unfamiliar");
    let path = alac(&dir.0);
    let key = key(&path).unwrap();
    let cache = dir.0.join("cache");
    std::fs::create_dir(&cache).unwrap();
    std::fs::write(
        cache.join("sources.sqlite"),
        b"not a database at all, just bytes",
    )
    .unwrap();
    let disk = Disk::new(cache.clone(), true);
    disk.store(&key, &fingerprint::read(&path).unwrap());
    assert!(!disk.warned.load(Ordering::Relaxed));
    assert!(cache.join("sources.sqlite.unreadable").exists());
    assert!(disk.load::<fingerprint::SourceFingerprint>(&key).is_some());
}

#[test]
fn a_row_stored_under_another_composite_definition_loads_with_this_builds() {
    let dir = Temp::new("composite");
    let path = alac(&dir.0);
    let key = key(&path).unwrap();
    let fingerprint = fingerprint::read(&path).unwrap();
    let cache = dir.0.join("cache");
    Disk::new(cache.clone(), true).store(&key, &fingerprint);
    // As a build with another hash domain would have stored it.
    Connection::open(cache.join("sources.sqlite"))
        .unwrap()
        .execute(
            "UPDATE fingerprints SET audio_sha256=?1",
            [[5u8; 32].as_slice()],
        )
        .unwrap();
    let loaded = Disk::new(cache, true)
        .load::<fingerprint::SourceFingerprint>(&key)
        .unwrap();
    assert!(loaded == fingerprint);
    assert_eq!(loaded.audio.sha256, loaded.audio.composite_sha256());
}
