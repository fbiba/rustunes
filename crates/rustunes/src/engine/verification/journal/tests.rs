use super::*;
use crate::engine::state::journal::{Artwork, CompletionEvidence, Content, test_support};
use crate::engine::verification::artwork;
use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf, sync::atomic::AtomicU64};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "journal-verify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
    fn content(&self) -> Content {
        let mut bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/silence-alac.m4a"
        ))
        .to_vec();
        let n = 12345 - bytes.len();
        bytes.extend((n as u32).to_be_bytes());
        bytes.extend(b"free");
        bytes.resize(12345, 0);
        let path = self.0.join("source.m4a");
        fs::write(&path, bytes).unwrap();
        Content {
            source: (music_protocol::media::fingerprint::read(&path).unwrap()).into(),
            track_pid: u64::MAX,
            path: "iTunes_Control/Music/F39/TEST.m4a".into(),
            artwork: None,
            collection: None,
        }
    }
    fn journal(&self, current: &Current, content: Option<Content>) -> Journal {
        let mut journal = Journal::create(&self.0.join("journal"), &current.identity).unwrap();
        if let Some(content) = content {
            let evidence = CompletionEvidence {
                track_pid: content.track_pid,
                path: content.path.clone(),
                audio_bytes: content.source.file_bytes,
                audio_file_sha256: content.source.full_file_sha256,
                artwork_sha256: content.artwork.as_ref().map(|a| a.sha256),
                media_anchor: 13,
                receipt_sha256: [8; 32],
            };
            crate::engine::state::journal::test_support::insert_verified_with(
                &mut journal,
                content,
                evidence,
            );
        }
        journal
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn prefs() -> Vec<u8> {
    let mut p = vec![0; 1232];
    p[..4].copy_from_slice(b"frpd");
    p[4..6].copy_from_slice(&1u16.to_le_bytes());
    p[6..8].copy_from_slice(&25u16.to_le_bytes());
    p[8] = 1;
    p[12..20].copy_from_slice(&1u64.to_le_bytes());
    p[20..28].copy_from_slice(&2u64.to_le_bytes());
    p
}
fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn current() -> Current {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let library = music_protocol::catalogue::identity::parse(&prefs()).unwrap();
    let mut cdb = vec![0; 388];
    cdb[..4].copy_from_slice(b"mhbd");
    put(&mut cdb, 4, 244);
    put(&mut cdb, 8, 388);
    put(&mut cdb, 12, 1);
    put(&mut cdb, 16, 117);
    put(&mut cdb, 20, 1);
    cdb[0x48..0x50].copy_from_slice(&catalog.database_pid().to_le_bytes());
    cdb[244..248].copy_from_slice(b"mhsd");
    put(&mut cdb, 248, 96);
    put(&mut cdb, 252, 144);
    put(&mut cdb, 256, 11);
    put(&mut cdb, 340, 1);
    put(&mut cdb, 348, 1);
    Current {
        device: "synthetic".into(),
        identity: Identity::new("synthetic", &library).unwrap(),
        library,
        catalog,
        raw_preferences: prefs(),
        plist_preferences: prefs(),
        cdb,
        artwork_db: None,
        receipt_sha256: [7; 32],
    }
}
fn control() -> ReadControl {
    ReadControl {
        deadline: Instant::now() + Duration::from_secs(1800),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
fn attrs() -> Attributes {
    BTreeMap::from([
        ("st_ifmt".into(), "S_IFREG".into()),
        ("st_size".into(), "12345".into()),
        ("st_mtime".into(), "1".into()),
        ("st_birthtime".into(), "1".into()),
        ("st_atime".into(), "1".into()),
    ])
}
struct Mock {
    hash: [u8; 32],
    guards: usize,
    reads: usize,
    stats: usize,
    missing: bool,
    bad_size: bool,
    changed: bool,
    fail_read: bool,
    fail_final_guard: bool,
}
impl Mock {
    fn new(hash: [u8; 32]) -> Self {
        Self {
            hash,
            guards: 0,
            reads: 0,
            stats: 0,
            missing: false,
            bad_size: false,
            changed: false,
            fail_read: false,
            fail_final_guard: false,
        }
    }
}
impl ReadBatch for Mock {
    async fn state_guard(&mut self, _: &FreshState) -> Result<()> {
        self.guards += 1;
        ensure!(
            !(self.fail_final_guard && self.guards == 2),
            "synthetic state drift"
        );
        Ok(())
    }
    async fn attributes(&mut self, _: &str) -> Result<Option<Attributes>> {
        self.stats += 1;
        if self.missing {
            return Ok(None);
        }
        let mut a = attrs();
        if self.bad_size {
            a.insert("st_size".into(), "7".into());
        }
        if self.stats > 1 {
            a.insert("st_atime".into(), "2".into());
            if self.changed {
                a.insert("st_mtime".into(), "2".into());
            }
        }
        Ok(Some(a))
    }
    async fn audio_hash(&mut self, _: &str, _: u64) -> Result<[u8; 32]> {
        self.reads += 1;
        ensure!(!self.fail_read, "synthetic transport failure");
        Ok(self.hash)
    }
    async fn mpeg_bytes(&mut self, _: &str, _: u64) -> Result<Vec<u8>> {
        panic!("ALAC verifier attempted MPEG payload inspection")
    }
}

#[test]
fn schedule_bounds_every_connection_and_counts_state_bytes_toward_total() {
    let files: Vec<_> = (0..512).map(|i| (i, MAX_AUDIO)).collect();
    let (batches, audio, total) = schedule(&files, 10).unwrap();
    assert!(batches.len() > 1);
    assert_eq!(audio, 512 * MAX_AUDIO);
    assert!(total > audio);
    assert!(total <= MAX_TOTAL);
    assert!(
        batches
            .iter()
            .all(|b| !b.indices.is_empty() && b.operations <= MAX_BOUNDED_OPS)
    );
    assert_eq!(
        batches
            .iter()
            .flat_map(|b| b.indices.iter().copied())
            .collect::<Vec<_>>(),
        (0..512).collect::<Vec<_>>()
    );
    assert!(schedule(&(0..1024).map(|i| (i, MAX_AUDIO)).collect::<Vec<_>>(), 10).is_err());
    assert!(schedule(&[(0, MAX_AUDIO + 1)], 10).is_err());
    assert!(schedule(&[(0, 1)], u64::MAX).is_err());
    assert!(schedule(&[(0, MAX_AUDIO)], 4000).is_err());
    assert!(schedule(&vec![(0, 1); 4097], 0).is_err());
    assert!(schedule(&[], 0).unwrap().0.is_empty());
    // A library of 400 typical tracks needs two connections, not seventeen.
    let typical: Vec<_> = (0..400).map(|i| (i, 20 << 20)).collect();
    assert_eq!(schedule(&typical, 10).unwrap().0.len(), 2);
    assert!(bounded_attributes(&attrs()).is_ok());
    assert!(bounded_attributes(&BTreeMap::from([("oversize".into(), "a".repeat(1025))])).is_err());
}

#[test]
fn selection_local_catalog_failures_and_art_registration_never_imply_permanent_proof() {
    let temp = Temp::new();
    let current = current();
    let mut content = temp.content();
    content.artwork = Some(Artwork {
        cache_id: 677,
        bytes: 4,
        sha256: [9; 32],
    });
    let journal = temp.journal(&current, Some(content.clone()));
    let plan = prepare(
        &journal,
        &current,
        Selection::TrackIds(BTreeSet::from([u64::MAX])),
    )
    .unwrap();
    assert_eq!(plan.bindings.len(), 1);
    assert_eq!(plan.report.tracks[0].artwork.registration, "missing");
    assert_eq!(
        plan.report.tracks[0].artwork.permanent_cache_verification,
        "not_performed"
    );
    assert!(
        prepare(
            &journal,
            &current,
            Selection::TrackIds(BTreeSet::from([12]))
        )
        .is_err()
    );
    assert!(prepare(&journal, &current, Selection::TrackIds(BTreeSet::new())).is_err());
    let track = &current.catalog.tracks()[0];
    assert!(catalog_finding(&content, Some(track), 1).is_none());
    assert_eq!(
        catalog_finding(&content, None, 0),
        Some("track_absent_from_catalog")
    );
    content.path = "iTunes_Control/Music/F39/DIFF.m4a".into();
    assert_eq!(
        catalog_finding(&content, Some(track), 1),
        Some("catalog_path_differs")
    );
    content.path = track.afc_path.clone().unwrap();
    content.source.file_bytes += 1;
    assert_eq!(
        catalog_finding(&content, Some(track), 1),
        Some("catalog_audio_size_differs")
    );
    let mut unavailable = track.clone();
    unavailable.availability.is_placeholder = true;
    assert_eq!(
        catalog_finding(&content, Some(&unavailable), 1),
        Some("catalog_audio_unavailable")
    );
}

#[tokio::test]
async fn successful_batch_checks_both_guards_and_only_then_confirms_audio() {
    let temp = Temp::new();
    let current = current();
    let content = temp.content();
    let journal = temp.journal(&current, Some(content.clone()));
    let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let mut p = prepare(&journal, &current, Selection::All).unwrap();
    let mut reader = Mock::new(content.source.full_file_sha256);
    read_batch(
        &mut reader,
        &p.state,
        &p.device,
        &p.bindings,
        &mut p.report.tracks,
        &p.batches[0],
        0,
        &control(),
    )
    .await
    .unwrap();
    assert_eq!(reader.guards, 2);
    assert_eq!(reader.reads, 1);
    assert!(p.report.tracks[0].audio_status == AudioStatus::VerifiedTransferredContainer);
    p.report.completed_batches = 1;
    finish_report(&mut p.report);
    assert!(p.report.all_selected_audio_verified);
    assert!(!p.report.metadata_projection_verified);
    assert!(!p.report.permanent_artwork_verified);
    assert_eq!(
        before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}

#[test]
fn sibling_album_cache_change_is_informational_and_never_claims_permanent_proof() {
    let first = Artwork {
        cache_id: 100,
        bytes: 4,
        sha256: [9; 32],
    };
    let sibling = Artwork {
        cache_id: 101,
        bytes: 4,
        sha256: [8; 32],
    };
    let previous = observed_artwork_status(Some(&first), Some(100), Some(101), true);
    let newest = observed_artwork_status(Some(&sibling), Some(101), Some(101), true);
    assert_eq!(previous.track_registration_matches, Some(true));
    assert_eq!(
        previous.album_registration,
        "independent_album_representative"
    );
    assert_eq!(previous.registration, "track_registration_matches");
    assert_ne!(
        previous.expected_source_cover_sha256,
        newest.expected_source_cover_sha256
    );
    assert_eq!(newest.registration, "track_registration_matches");
    assert_eq!(previous.permanent_cache_verification, "not_performed");
    // Different per-track source covers are valid within one shared album.
    let encoded = serde_json::to_value(&previous).unwrap();
    assert_eq!(encoded["track_registration_matches"], true);
    assert_eq!(
        encoded["album_registration"],
        "independent_album_representative"
    );
    assert_ne!(encoded["registration"], "mismatch");
    let absent = observed_artwork_status(Some(&first), Some(100), None, true);
    assert_eq!(absent.registration, "track_registration_matches");
    assert_eq!(absent.album_registration, "missing");
    assert!(absent.permanent_cache_proof.is_none());
}

#[test]
fn journal_success_requires_permanent_track_proof_but_not_album_cover_equality() {
    let temp = Temp::new();
    let current = current();
    let content = temp.content();
    let journal = temp.journal(&current, Some(content));
    let mut report = prepare(&journal, &current, Selection::All).unwrap().report;
    let art = Artwork {
        cache_id: 100,
        bytes: 4,
        sha256: [9; 32],
    };
    report.tracks[0].audio_status = AudioStatus::VerifiedTransferredContainer;
    report.tracks[0].artwork = observed_artwork_status(Some(&art), Some(100), Some(101), true);
    finish_report(&mut report);
    assert_eq!(report.status, "complete_with_findings");
    assert!(!report.permanent_artwork_verified);
    let original = artwork::PermanentFile {
        afc_path: "original".into(),
        bytes: 4,
        sha256: hex(&art.sha256),
        before: Attributes::new(),
        after: Attributes::new(),
    };
    report.tracks[0].artwork.permanent_cache_proof = Some(VerifiedTrackArtworkBytes {
        schema: "rustunes.permanent-track-artwork-bytes",
        target_sync_id: "ffffffffffffffff".into(),
        cache_id: 100,
        media_anchor: 13,
        state_receipt_sha256: "receipt".into(),
        database_sha256: "database".into(),
        wal_sha256: None,
        source_cover_sha256: hex(&art.sha256),
        original: original.clone(),
        original_exact_source_bytes: true,
        observed_album_cache_id: Some(101),
        album_representative_source_equality_claimed: false,
        derivative: Some(original),
        derivative_width: 556,
        derivative_height: 556,
        derivative_entropy_decode_verified: true,
        derivative_rgb24_sha256: Some("pixels".into()),
        derivative_rgb24_bytes: 556 * 556 * 3,
        derivative_source_pixel_equality_claimed: false,
        decoder: "synthetic proof fixture",
        phone_writes: 0,
        readback: "read",
    });
    finish_report(&mut report);
    assert_eq!(report.status, "audio_checks_complete");
    assert!(report.permanent_artwork_verified);
    assert_eq!(
        report.tracks[0].artwork.album_registration,
        "independent_album_representative"
    );
}

#[test]
fn artwork_budget_covers_original_derivative_and_both_state_guards() {
    assert_eq!(
        artwork_read_budget(&[4, 9], 3).unwrap(),
        13 + 2 * artwork::MAX_IMAGE_BYTES + 12 * CHUNK
    );
    assert!(artwork_read_budget(&[0], 0).is_err());
    assert!(artwork_read_budget(&[artwork::MAX_IMAGE_BYTES + 1], 0).is_err());
    assert!(artwork_read_budget(&[1], u64::MAX).is_err());
    assert!(artwork_read_budget(&vec![artwork::MAX_IMAGE_BYTES; MAX_BINDINGS], 1).is_err());
}

#[tokio::test]
async fn hash_missing_size_and_state_drift_are_distinct_and_never_partial_success() {
    let temp = Temp::new();
    let current = current();
    let content = temp.content();
    let journal = temp.journal(&current, Some(content.clone()));
    for variant in 0..6 {
        let mut p = prepare(&journal, &current, Selection::All).unwrap();
        let mut reader = Mock::new(content.source.full_file_sha256);
        match variant {
            0 => reader.hash[0] ^= 1,
            1 => reader.missing = true,
            2 => reader.bad_size = true,
            3 => reader.fail_final_guard = true,
            4 => reader.changed = true,
            _ => reader.fail_read = true,
        }
        let result = read_batch(
            &mut reader,
            &p.state,
            &p.device,
            &p.bindings,
            &mut p.report.tracks,
            &p.batches[0],
            0,
            &control(),
        )
        .await;
        if variant < 3 {
            result.unwrap();
            finish_report(&mut p.report);
            assert_eq!(p.report.status, "complete_with_findings");
            assert!(!p.report.all_selected_audio_verified);
        } else {
            assert!(result.is_err());
            assert!(!p.report.verification_completed);
            assert!(!p.report.all_selected_audio_verified);
            assert_eq!(p.report.status, "incomplete");
        }
        if variant == 3 || variant == 4 {
            assert!(p.report.tracks[0].audio_status == AudioStatus::ReadPendingStateGuard);
        }
        if variant == 1 || variant == 2 {
            assert_eq!(reader.reads, 0);
        }
    }
}

#[tokio::test]
async fn identity_and_cancel_guards_abort_before_connector_and_trace() {
    let temp = Temp::new();
    let mut current = current();
    let content = temp.content();
    let journal = temp.journal(&current, Some(content));
    let trace = temp.0.join("not-created");
    current.identity = Identity::new("wrong-device", &current.library).unwrap();
    let result = verify(
        &journal,
        &current,
        Selection::All,
        &trace,
        &control(),
        || async { panic!("identity mismatch reached connector") },
    )
    .await;
    assert!(result.is_err());
    assert!(!trace.exists());
    current.identity = Identity::new(&current.device, &current.library).unwrap();
    let result = verify(
        &journal,
        &current,
        Selection::All,
        &trace,
        &control(),
        || async {
            Ok(DeviceProvider::usb(idevice::provider::UsbmuxdProvider {
                addr: idevice::usbmuxd::UsbmuxdAddr::UnixSocket(
                    "/tmp/never-opened-journal-verifier-test.sock".into(),
                ),
                tag: 0,
                udid: "different-device".into(),
                device_id: 0,
                label: "synthetic".into(),
            }))
        },
    )
    .await;
    assert!(result.is_err());
    assert!(!trace.exists());
    for expired in [false, true] {
        let mut control = control();
        if expired {
            control.deadline = Instant::now();
        } else {
            control.cancelled.store(true, Ordering::Relaxed);
        }
        assert!(
            verify(
                &journal,
                &current,
                Selection::All,
                &trace,
                &control,
                || async { panic!("cancelled verification reached connector") }
            )
            .await
            .is_err()
        );
        assert!(!trace.exists());
    }
    let mut mismatch = current.raw_preferences.clone();
    mismatch[12] ^= 1;
    current.raw_preferences = mismatch;
    assert!(
        verify(
            &journal,
            &current,
            Selection::All,
            &trace,
            &control(),
            || async { panic!("preference mismatch reached connector") }
        )
        .await
        .is_err()
    );
    assert!(!trace.exists());
}

#[tokio::test]
async fn zero_bindings_reports_no_checks_without_discovery_or_journal_changes() {
    let temp = Temp::new();
    let current = current();
    let journal = temp.journal(&current, None);
    let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let trace = temp.0.join("trace");
    let report = verify(
        &journal,
        &current,
        Selection::All,
        &trace,
        &control(),
        || async { panic!("empty journal reached connector") },
    )
    .await
    .unwrap();
    assert_eq!(report.status, "no_bindings");
    assert_eq!(report.completed_batches, 0);
    assert!(!report.all_selected_audio_verified);
    assert_eq!(report.phone_writes, 0);
    assert_eq!(report.journal_mutations, 0);
    assert_eq!(
        before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
    assert!(trace.join("report.json").exists());
}

#[tokio::test]
async fn retired_lifetime_is_never_scheduled_and_same_audio_readd_verifies_only_new_binding() {
    use crate::engine::state::journal::test_support;
    let temp = Temp::new();
    let current = current();
    let new = temp.content();
    let mut old = new.clone();
    old.track_pid -= 1;
    old.path = "iTunes_Control/Music/F39/OLDX.m4a".into();
    let mut journal = temp.journal(&current, None);
    test_support::insert_verified(&mut journal, old.clone());
    test_support::retire(&mut journal, &old);
    let empty = prepare(&journal, &current, Selection::All).unwrap();
    assert_eq!(empty.report.selected_bindings, 0);
    assert!(empty.report.tracks.is_empty() && empty.batches.is_empty());
    assert!(
        prepare(
            &journal,
            &current,
            Selection::TrackIds(BTreeSet::from([old.track_pid]))
        )
        .is_err()
    );
    test_support::insert_verified(&mut journal, new.clone());
    let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let mut p = prepare(&journal, &current, Selection::All).unwrap();
    assert_eq!(p.report.total_verified_journal_bindings, 1);
    assert_eq!(p.bindings[0].content.track_pid, new.track_pid);
    assert_eq!(p.report.tracks[0].path, new.path);
    let mut reader = Mock::new(new.source.full_file_sha256);
    read_batch(
        &mut reader,
        &p.state,
        &p.device,
        &p.bindings,
        &mut p.report.tracks,
        &p.batches[0],
        0,
        &control(),
    )
    .await
    .unwrap();
    assert_eq!(reader.reads, 1);
    assert_eq!(reader.guards, 2);
    assert!(p.report.tracks[0].audio_status == AudioStatus::VerifiedTransferredContainer);
    assert_eq!(
        before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}

mod mpeg_tests;
