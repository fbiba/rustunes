//! Source expectations exercise read-only planning; no checksum or mutation grant.
use super::*;

fn cover() -> Cover {
    let bytes = b"same payload, different source containers".to_vec();
    Cover {
        sha256: Sha256::digest(&bytes).into(),
        checksum: bytes.len() as u32 + 17,
        bytes,
    }
}
fn expected(c: &Cover) -> TrackArtworkExpectation {
    TrackArtworkExpectation {
        cache_id: 677,
        bytes: c.bytes.len() as u64,
        sha256: c.sha256,
    }
}
#[test]
fn album_original_expectation_refuses_a_wrong_size_digest_or_album() {
    let catalog = music_protocol::test_support::catalogue::artwork_catalog(677, 678);
    let c = cover();
    let mut e = expected(&c);
    let album_id = catalog.tracks()[0].album_sync_id;
    assert_eq!(
        album_plan(&catalog, album_id, &e.original())
            .unwrap()
            .cache_id,
        678
    );
    let mut file = PermanentFile {
        afc_path: "held original".into(),
        bytes: e.bytes,
        sha256: hex(&e.sha256),
        before: Attributes::new(),
        after: Attributes::new(),
    };
    validate_original(&file, &e.original()).unwrap();
    file.bytes += 1;
    assert!(validate_original(&file, &e.original()).is_err());
    file.bytes -= 1;
    file.sha256 = "09".repeat(32);
    assert!(validate_original(&file, &e.original()).is_err());
    for bytes in [0, MAX_IMAGE_BYTES + 1] {
        e.bytes = bytes;
        assert!(album_plan(&catalog, album_id, &e.original()).is_err());
    }
    e = expected(&c);
    e.sha256 = [0; 32];
    assert!(album_plan(&catalog, album_id, &e.original()).is_err());
    assert!(album_plan(&catalog, 999, &expected(&c).original()).is_err());
}
#[tokio::test]
async fn new_observers_refuse_invalid_cancelled_and_expired_inputs_before_connect() {
    let catalog = music_protocol::test_support::catalogue::artwork_catalog(677, 678);
    let mut prefs = vec![0u8; 1232];
    prefs[..4].copy_from_slice(b"frpd");
    prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
    prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
    prefs[8] = 1;
    prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
    prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
    let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
    let current = Current {
        device: "synthetic-device".into(),
        identity: crate::engine::state::journal::Identity::new("synthetic-device", &library)
            .unwrap(),
        library,
        catalog,
        raw_preferences: prefs.clone(),
        plist_preferences: prefs,
        cdb: vec![],
        artwork_db: None,
        receipt_sha256: [1; 32],
    };
    let called = AtomicBool::new(false);
    let connect = || async {
        called.store(true, Ordering::Relaxed);
        bail!("unexpected endpoint")
    };
    for case in 0..4 {
        let control = Control {
            deadline: if case == 2 {
                Instant::now() - Duration::from_secs(1)
            } else {
                Instant::now() + Duration::from_secs(10)
            },
            cancelled: Arc::new(AtomicBool::new(case == 1)),
        };
        let mut e = expected(&cover());
        if case == 0 {
            e.bytes = 0;
        }
        if case == 3 {
            e.sha256 = [0; 32];
        }
        let trace = Path::new("/not-created-by-negative-source-observation");
        assert!(
            verify_track_expected(&current, u64::MAX, &e, trace, &control, connect)
                .await
                .is_err()
        );
        assert!(
            verify_album_original_expected(
                &current,
                current.catalog.tracks()[0].album_sync_id,
                &e.original(),
                trace,
                &control,
                connect
            )
            .await
            .is_err()
        );
        assert!(!called.load(Ordering::Relaxed));
        assert!(!trace.exists());
    }
}
