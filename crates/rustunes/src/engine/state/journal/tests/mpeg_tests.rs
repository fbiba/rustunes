use super::*;
use music_protocol::media::fingerprint::{EncodedMpeg1Layer3, TransferredSource};

fn mpeg_content() -> Content {
    Content {
        source: TransferredSource::mpeg(
            844,
            [23; 32],
            EncodedMpeg1Layer3 {
                sample_rate: 44100,
                channels: 2,
                frame_count: 2,
                audio_offset: 10,
                audio_bytes: 834,
                trailing_bytes: 0,
                frame_bytes_sha256: [24; 32],
            },
        )
        .unwrap(),
        track_pid: 0x1234,
        path: "iTunes_Control/Music/F40/ABCD.mp3".into(),
        artwork: None,
        collection: Some([42; 32]),
    }
}
fn idle() -> (Temp, Journal) {
    let t = Temp::new();
    drop(Journal::create(&t.journal(), &scope()).unwrap());
    let j = Journal::open(&t.journal(), &scope()).unwrap();
    (t, j)
}
fn mpeg_prepared() -> PreparedAttempt {
    let mut p = insert(1);
    p.content = mpeg_content();
    p.operation = operation(
        p.kind,
        &p.content,
        p.current_media_anchor,
        p.new_album_pid,
        p.new_artist_pid,
    );
    p
}

#[test]
fn typed_content_roundtrip_and_cross_codec_path_refusals() {
    let c = mpeg_content();
    let b = encode_content(&c).unwrap();
    assert_eq!(b[0], 2);
    assert!(decode_content(&b).unwrap() == c);
    assert!(c.source.require_alac().is_err());
    let a = content(1);
    assert_eq!(encode_content(&a).unwrap()[0], 1);
    assert!(decode_content(&encode_content(&a).unwrap()).unwrap() == a);
    for mut wrong in [a.clone(), c.clone()] {
        wrong.path = if wrong.source.is_alac() {
            c.path.clone()
        } else {
            a.path.clone()
        };
        assert!(encode_content(&wrong).is_err());
    }
    for at in [0, 9, 41, 73, 77, 78, 82, 90, 98, 106] {
        let mut corrupt = b.clone();
        corrupt[at] ^= 0xff;
        // Full-file hash is intentionally opaque, but all coded facts/digests
        // and record version must agree, not merely have the right lengths.
        if at != 9 {
            assert!(
                decode_content(&corrupt).is_err(),
                "accepted corruption at {at}"
            );
        }
    }
    let mut trailing = b.clone();
    trailing.push(0);
    assert!(decode_content(&trailing).is_err());
    assert!(decode_content(&b[..b.len() - 1]).is_err());
    let mut forged = c;
    forged.source.audio.sha256 = a.source.audio.sha256;
    assert!(encode_content(&forged).is_err());
}

#[test]
fn mpeg_pending_storage_does_not_grant_start_or_completion() {
    let (t, mut j) = idle();
    let p = mpeg_prepared();
    assert!(j.prepare(mpeg_prepared()).is_err());
    // Synthetic typed storage only; the helper also checks that the public
    // start gate refuses before retaining the fixture's Ready evidence.
    let e = start(p.current_media_anchor);
    let id = test_support::mpeg_started_storage_fixture(&mut j, &p, &e);
    drop(j);
    let mut j = Journal::open(&t.journal(), &scope()).unwrap();
    let a = j.attempt(id).unwrap();
    assert!(a.prepared.content == p.content);
    assert_eq!(a.state, AttemptState::ReconciliationRequired);
    assert_eq!(
        j.start_evidence(id).unwrap().unwrap().signature,
        e.signature
    );
    let mut status = Vec::new();
    crate::cli::journal_view::write(&j, true, &mut status, "").unwrap();
    let status = String::from_utf8(status).unwrap();
    assert!(status.contains("encoded_mpeg1_layer3") && !status.contains("encoded_alac"));
    let stored = fs::read(t.journal().join(DB_NAME)).unwrap();
    assert!(j.reconcile_completed(id, verified(&p.content, 13)).is_err());
    assert_eq!(stored, fs::read(t.journal().join(DB_NAME)).unwrap());
    // Future verified storage fixture only. The public completion route above
    // still refused; this verifies the one existing history representation.
    bindings::store_binding(
        &j.connection,
        &p.content,
        &verified(&p.content, 14),
        false,
        id,
    )
    .unwrap();
    j.connection
        .execute(
            "UPDATE reservations SET consumed=1 WHERE attempt=?1",
            [id.0],
        )
        .unwrap();
    j.connection
        .execute(
            "UPDATE attempts SET state=2,verified_receipt=?1,verified_anchor=14 WHERE id=?2",
            params![[9_u8; 32], id.0],
        )
        .unwrap();
    drop(j);
    let j = Journal::open_read_only(&t.journal(), &scope()).unwrap();
    let binding = j.lookup_active(&p.content.source.audio).unwrap().unwrap();
    let history = j.binding_history(binding.reference.id, 0, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].binding.content == p.content);
    assert_eq!(
        history[0].binding.receipt_kind,
        BindingReceiptKind::InsertVerification
    );
}
