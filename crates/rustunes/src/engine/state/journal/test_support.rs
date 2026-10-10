//! Synthetic, cfg(test)-only consumer fixtures. No device observations or native
//! qualification are performed, and this module is absent from production.
use super::*;
use plist::Dictionary;
use std::time::UNIX_EPOCH;

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<Dictionary>(),
    )
}
fn operation(pid: u64, anchor: u32) -> Vec<u8> {
    envelope(
        anchor,
        vec![
            ("operation", "insert_track".into()),
            ("pid", (pid as i64).into()),
            ("item", dict(vec![])),
        ],
    )
}
/// The artwork-only update that gives track `pid` the cover `art`.
fn artwork_operation(pid: u64, anchor: u32, art: &Artwork) -> Vec<u8> {
    envelope(
        anchor,
        vec![
            ("operation", "update_track".into()),
            ("pid", (pid as i64).into()),
            (
                "item",
                dict(vec![
                    ("artwork_cache_id", u64::from(art.cache_id).into()),
                    ("artwork_checksum", (art.bytes + 17).into()),
                ]),
            ),
        ],
    )
}
fn envelope(anchor: u32, own: Vec<(&str, Value)>) -> Vec<u8> {
    let value = dict(vec![
        ("revision", (anchor + 1).into()),
        ("timestamp", Value::Date(UNIX_EPOCH.into())),
        (
            "operations",
            Value::Array(vec![
                dict(vec![
                    ("operation", "update_db_info".into()),
                    ("pid", 51_i64.into()),
                    (
                        "db_info",
                        dict(vec![("primary_container_pid", 50_i64.into())]),
                    ),
                ]),
                dict(own),
            ]),
        ),
    ]);
    let mut bytes = Vec::new();
    value.to_writer_binary(&mut bytes).unwrap();
    bytes
}
fn start(anchor: u32) -> StartEvidence {
    StartEvidence {
        ready_media_anchor: anchor,
        ready_receipt_sha256: [8; 32],
        signature: [1; 21],
    }
}
pub fn try_prepare_insert(journal: &mut Journal, content: Content) -> Result<AttemptId> {
    try_prepare_insert_with_input(journal, content, [7; 32])
}
fn try_prepare_insert_with_input(
    journal: &mut Journal,
    content: Content,
    input_receipt_sha256: [u8; 32],
) -> Result<AttemptId> {
    journal.prepare(PreparedAttempt {
        kind: AttemptKind::Insert,
        operation: operation(content.track_pid, 13),
        content: content.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: 13,
        input_receipt_sha256,
    })
}
pub fn prepare_insert(journal: &mut Journal, content: Content) -> AttemptId {
    try_prepare_insert(journal, content).unwrap()
}
/// A Prepared insertion whose run holds an `input.json` with this SHA-256.
pub fn prepare_insert_with_input(
    journal: &mut Journal,
    content: Content,
    input_receipt_sha256: [u8; 32],
) -> AttemptId {
    try_prepare_insert_with_input(journal, content, input_receipt_sha256).unwrap()
}
/// One ALAC in a Prepared insert batch whose evidence is `evidence`.
pub fn prepare_insert_batch_at(
    journal: &mut Journal,
    content: Content,
    evidence: &Path,
) -> InsertBatchId {
    journal
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: operation(content.track_pid, 13),
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: evidence.to_owned(),
            targets: vec![InsertTarget {
                content,
                new_album_pid: None,
                new_artist_pid: None,
                conversion: None,
                source: None,
            }],
        })
        .unwrap()
}
pub fn insert_verified(journal: &mut Journal, content: Content) -> AttemptId {
    let evidence = CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path.clone(),
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 14,
        receipt_sha256: [9; 32],
    };
    insert_verified_with(journal, content, evidence)
}
/// A verified binding of `content` whose last completion is `evidence`. An
/// insert carries no cover, so content with artwork is inserted bare one
/// revision earlier and then gets its cover from an artwork attempt.
pub fn insert_verified_with(
    journal: &mut Journal,
    content: Content,
    evidence: CompletionEvidence,
) -> AttemptId {
    let base = evidence.media_anchor - 1;
    if content.artwork.is_none() {
        return verified(journal, AttemptKind::Insert, content, base, evidence);
    }
    let bare = Content {
        artwork: None,
        ..content.clone()
    };
    let inserted = CompletionEvidence {
        track_pid: evidence.track_pid,
        path: evidence.path.clone(),
        audio_bytes: evidence.audio_bytes,
        audio_file_sha256: evidence.audio_file_sha256,
        artwork_sha256: None,
        media_anchor: base,
        receipt_sha256: evidence.receipt_sha256,
    };
    verified(journal, AttemptKind::Insert, bare, base - 1, inserted);
    verified(journal, AttemptKind::Artwork, content, base, evidence)
}
fn verified(
    journal: &mut Journal,
    kind: AttemptKind,
    content: Content,
    base: u32,
    evidence: CompletionEvidence,
) -> AttemptId {
    let operation = match &content.artwork {
        Some(art) if kind == AttemptKind::Artwork => {
            artwork_operation(content.track_pid, base, art)
        }
        _ => operation(content.track_pid, base),
    };
    let id = journal
        .prepare(PreparedAttempt {
            kind,
            operation,
            content,
            new_album_pid: None,
            new_artist_pid: None,
            current_media_anchor: base,
            input_receipt_sha256: [7; 32],
        })
        .unwrap();
    let permit = journal.mark_started(id, start(base)).unwrap();
    journal.complete(permit, evidence).unwrap();
    id
}
/// Retire `content`'s active binding the way removal does: a batch that
/// targets it, started and settled.
pub fn retire(journal: &mut Journal, content: &Content) -> BatchId {
    let active = journal
        .lookup_active(&content.source.audio)
        .unwrap()
        .unwrap();
    let target = RemovalTarget {
        track: content.track_pid,
        item_pid: content.track_pid as i64,
        path: Some(content.path.clone()),
        file_bytes: Some(content.source.file_bytes),
        file_sha256: Some(content.source.full_file_sha256),
        binding: Some((active.reference.id, active.reference.version)),
    };
    let id = journal
        .prepare_removal_batch(&RemovalIntent {
            coordination: None,
            compensates: None,
            compensates_insert_batch: None,
            operation: vec![5; 40],
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: "/run/removal-fixture".into(),
            targets: vec![target],
            playlists: Vec::new(),
        })
        .unwrap();
    journal.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
    journal.settle_removal_batch(id, [9; 32], 14).unwrap();
    id
}

/// One ALAC converted from the FLAC `conversion` names, in a Prepared insert
/// batch whose evidence is `evidence` and retained input hashes to `input`.
pub fn prepare_converted_insert_batch_at(
    journal: &mut Journal,
    content: Content,
    conversion: Conversion,
    evidence: &Path,
    input: [u8; 32],
) -> InsertBatchId {
    journal
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: operation(content.track_pid, 13),
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: input,
            evidence: evidence.to_owned(),
            targets: vec![InsertTarget {
                content,
                new_album_pid: None,
                new_artist_pid: None,
                conversion: Some(conversion),
                source: None,
            }],
        })
        .unwrap()
}

/// One ALAC bound by a Verified insert batch prepared at `base`, converted
/// from the FLAC `conversion` names.
pub fn converted_insert_verified(
    journal: &mut Journal,
    content: Content,
    conversion: Conversion,
    base: u32,
) -> InsertBatchId {
    let id = journal
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: operation(content.track_pid, base),
            base_anchor: base,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: "/run/converted-insert-fixture".into(),
            targets: vec![InsertTarget {
                content: content.clone(),
                new_album_pid: None,
                new_artist_pid: None,
                conversion: Some(conversion),
                source: None,
            }],
        })
        .unwrap();
    journal.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let claim = CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path,
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: content.artwork.as_ref().map(|a| a.sha256),
        media_anchor: base + 1,
        receipt_sha256: [9; 32],
    };
    journal
        .settle_insert_batch(id, [9; 32], base + 1, &[(0, claim)], &[])
        .unwrap();
    id
}

/// Synthetic Started MPEG Insert storage, written straight into the journal:
/// no production path prepares or starts an MPEG attempt.
pub fn mpeg_started_storage_fixture(
    journal: &mut Journal,
    prepared: &PreparedAttempt,
    evidence: &StartEvidence,
) -> AttemptId {
    assert!(!prepared.content.source.is_alac());
    let bytes = encode_content(&prepared.content).unwrap();
    bindings::prepare(
        &journal.connection,
        prepared,
        &bytes,
        Sha256::digest(&prepared.operation).into(),
    )
    .unwrap();
    let id = AttemptId(journal.connection.last_insert_rowid());
    for pid in [
        Some(prepared.content.track_pid),
        prepared.new_album_pid,
        prepared.new_artist_pid,
    ]
    .into_iter()
    .flatten()
    {
        reserve(&journal.connection, 1, &pid.to_be_bytes(), Some(id), false).unwrap();
    }
    reserve(
        &journal.connection,
        3,
        prepared.content.path.to_ascii_lowercase().as_bytes(),
        Some(id),
        false,
    )
    .unwrap();
    assert!(
        journal
            .mark_started(
                id,
                StartEvidence {
                    ready_media_anchor: evidence.ready_media_anchor,
                    ready_receipt_sha256: evidence.ready_receipt_sha256,
                    signature: evidence.signature,
                },
            )
            .is_err()
    );
    journal
        .connection
        .execute(
            "UPDATE attempts SET state=1,ready_receipt=?1,signature=?2 WHERE id=?3",
            params![
                evidence.ready_receipt_sha256,
                evidence.signature.as_slice(),
                id.0
            ],
        )
        .unwrap();
    id
}

/// Synthetic stored-MPEG readback fixture only; this bypasses no production
/// gate. Completion must still refuse the same Content.
pub fn mpeg_verified_readback_fixture(
    journal: &mut Journal,
    content: Content,
    base: u32,
) -> AttemptId {
    let prepared = PreparedAttempt {
        kind: AttemptKind::Insert,
        operation: operation(content.track_pid, base),
        content: content.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: base,
        input_receipt_sha256: [7; 32],
    };
    let id = mpeg_started_storage_fixture(journal, &prepared, &start(base));
    let evidence = || CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path.clone(),
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: base + 1,
        receipt_sha256: [9; 32],
    };
    assert!(journal.reconcile_completed(id, evidence()).is_err());
    bindings::store_binding(&journal.connection, &content, &evidence(), false, id).unwrap();
    journal
        .connection
        .execute(
            "UPDATE reservations SET consumed=1 WHERE attempt=?1",
            [id.0],
        )
        .unwrap();
    journal
        .connection
        .execute(
            "UPDATE attempts SET state=2,verified_receipt=?1,verified_anchor=?2 WHERE id=?3",
            params![[9_u8; 32], base + 1, id.0],
        )
        .unwrap();
    id
}

/// A Started Insert left outstanding, the shape compensation settles.
pub fn insert_started(journal: &mut Journal, content: Content) -> AttemptId {
    let id = prepare_insert(journal, content);
    journal.mark_started(id, start(13)).unwrap();
    id
}

/// Synthetic ALAC Content; `seed` varies its identity, path and bytes.
pub fn alac_content(seed: u8) -> Content {
    let mut audio = EncodedAlac {
        sha256: [0; 32],
        alac_config_sha256: [seed; 32],
        sample_sizes_sha256: [2; 32],
        media_sha256: [3; 32],
        timescale: 44100,
        duration_ticks: 132300,
        media_bytes: 800,
    };
    audio.sha256 = audio.composite_sha256();
    Content {
        source: (SourceFingerprint {
            file_bytes: 1200,
            full_file_sha256: [seed; 32],
            audio,
        })
        .into(),
        track_pid: u64::MAX - u64::from(seed),
        path: format!(
            "iTunes_Control/Music/F39/AAA{}.m4a",
            char::from(b'A' + seed)
        ),
        artwork: None,
        collection: Some([42; 32]),
    }
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
/// A private scratch directory holding one journal at `path()`.
pub struct Temp(pub PathBuf);
impl Temp {
    pub fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "composed-journal-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
    pub fn path(&self) -> PathBuf {
        self.0.join("journal")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
pub fn identity() -> Identity {
    let mut b = vec![0; 1232];
    b[..4].copy_from_slice(b"frpd");
    b[4..6].copy_from_slice(&1u16.to_le_bytes());
    b[6..8].copy_from_slice(&25u16.to_le_bytes());
    b[8] = 1;
    b[12..20].copy_from_slice(&1u64.to_le_bytes());
    b[20..28].copy_from_slice(&2u64.to_le_bytes());
    Identity::new(
        "offline-composed-fixture",
        &music_protocol::catalogue::identity::parse(&b).unwrap(),
    )
    .unwrap()
}
