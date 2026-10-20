use super::*;
use crate::{
    engine::state::journal::{Binding, BindingReceiptKind},
    engine::verification::journal as jv,
};

#[test]
fn ordinary_missing_raw_artwork_does_not_widen_artwork_policy() {
    for artwork in [None, Some(vec![1])] {
        let a = current();
        let mut b = current();
        b.artwork_db = artwork;
        assert!(playback::same_ordinary_scope(&a, &b).is_ok());
        let temp = Temp::new();
        let output = temp.0.join("artwork-bridge");
        assert!(
            playback::verify(
                &a,
                &temp.0,
                &b,
                &temp.0,
                &output,
                control().deadline.into_std()
            )
            .is_err()
        );
        assert!(!output.exists());
    }
}

#[test]
fn ordinary_scope_rejects_identity_preferences_cdb_anchor_and_artwork_drift() {
    for field in 0..7 {
        let mut a = current();
        let mut b = current();
        match field {
            0 => b.device.push('x'),
            1 => b.identity = Identity::new("other", &b.library).unwrap(),
            2 => b.raw_preferences[20] ^= 1,
            3 => b.plist_preferences[20] ^= 1,
            4 => b.cdb[48] ^= 1,
            5 => b.catalog = music_protocol::test_support::catalogue::collection_catalog(14),
            6 => {
                a.artwork_db = Some(vec![1]);
                b.artwork_db = Some(vec![2]);
            }
            _ => unreachable!(),
        }
        assert!(
            playback::same_ordinary_scope(&a, &b).is_err(),
            "field {field}"
        );
    }
}

#[test]
fn ordinary_raw_artwork_policy_discloses_uncaptured_baseline_and_preserves_known_bytes() {
    for (before, after, expected) in [
        (None, None, Some("absent_from_both_captures")),
        (None, Some(vec![1]), Some("not_captured_in_immediate_state")),
        (Some(vec![1]), Some(vec![1]), Some("captured_equal")),
        (Some(vec![1]), Some(vec![2]), None),
        (Some(vec![1]), None, None),
    ] {
        let mut immediate = current();
        let mut fresh = current();
        immediate.artwork_db = before;
        fresh.artwork_db = after;
        assert_eq!(
            playback::same_ordinary_scope(&immediate, &fresh).is_ok(),
            expected.is_some()
        );
        assert_eq!(
            playback::ordinary_raw_artwork_comparison(&immediate, &fresh).ok(),
            expected
        );
    }
}

#[test]
fn ordinary_saved_fresh_artwork_must_exactly_match_the_captured_current() {
    for artwork in [None, Some(vec![1])] {
        let mut captured = current();
        captured.artwork_db = artwork.clone();
        let mut loaded = current();
        loaded.artwork_db = artwork;
        assert!(ordinary_bridge::ensure_loaded_fresh(&captured, &loaded).is_ok());
        loaded.artwork_db = if captured.artwork_db.is_some() {
            None
        } else {
            Some(vec![1])
        };
        assert!(ordinary_bridge::ensure_loaded_fresh(&captured, &loaded).is_err());
        if captured.artwork_db.is_some() {
            loaded.artwork_db = Some(vec![2]);
            assert!(ordinary_bridge::ensure_loaded_fresh(&captured, &loaded).is_err());
        }
    }
}

#[test]
fn ordinary_playback_entry_keeps_original_cancel_and_deadline_before_output() {
    for cancelled in [false, true] {
        let temp = Temp::new();
        let a = current();
        let b = current();
        let mut original = control();
        if cancelled {
            original.cancelled.store(true, Ordering::Relaxed);
        } else {
            original.deadline = Instant::now() - Duration::from_secs(1);
        }
        let output = temp.0.join("ordinary-bridge");
        assert!(playback::verify_ordinary(&a, &temp.0, &b, &temp.0, &output, &original).is_err());
        assert!(!output.exists());
    }
}

fn binding(temp: &Temp) -> Binding {
    Binding {
        content: temp.prepared().content,
        receipt_kind: BindingReceiptKind::InsertVerification,
        receipt_sha256: [4; 32],
        media_anchor: 13,
    }
}

fn physical_report(current: &Current, binding: Option<&Binding>) -> jv::Report {
    let tracks = binding
        .map(|b| {
            let c = &b.content;
            jv::TrackReport {
                track_id: format!("{:016x}", c.track_pid),
                wire_track_id: (c.track_pid as i64).to_string(),
                path: c.path.clone(),
                audio_bytes: c.source.file_bytes,
                expected_audio_sha256: hex(&c.source.full_file_sha256),
                encoded_audio_sha256: hex(&c.source.audio.sha256),
                mpeg_frame_readback: None,
                audio_proof: None,
                audio_status: jv::AudioStatus::VerifiedTransferredContainer,
                audio_observed_sha256: Some(hex(&c.source.full_file_sha256)),
                audio_attributes: Some(BTreeMap::from([
                    ("st_ifmt".into(), "S_IFREG".into()),
                    ("st_size".into(), c.source.file_bytes.to_string()),
                    ("st_mtime".into(), "1".into()),
                ])),
                finding: None,
                batch: Some(0),
                artwork: jv::ArtworkStatus {
                    expected_cache_id: None,
                    expected_source_cover_sha256: None,
                    observed_track_cache_id: None,
                    observed_album_cache_id: None,
                    track_registration_matches: None,
                    album_registration: "not_journaled",
                    registration: "not_journaled",
                    permanent_cache_verification: "not_performed",
                    permanent_cache_proof: None,
                    permanent_cache_failure: None,
                },
            }
        })
        .into_iter()
        .collect::<Vec<_>>();
    let count = tracks.len();
    jv::Report {
        schema: "rustunes.journal-verification",
        status: if count == 0 {
            "no_bindings"
        } else {
            "audio_checks_complete"
        },
        verification_completed: true,
        all_selected_audio_verified: count > 0,
        device_identifier_sha256: hex(&current.identity.device_digest()),
        library_identity_sha256: hex(&current.identity.library_digest()),
        input_receipt_sha256: hex(&current.receipt_sha256),
        media_anchor: current.catalog.persisted_media_anchor(),
        selected_bindings: count,
        total_verified_journal_bindings: count,
        outstanding_attempt_present: true,
        planned_audio_bytes: binding.map_or(0, |b| b.content.source.file_bytes),
        planned_total_read_bytes_upper_bound: 0,
        planned_batches: count,
        completed_batches: count,
        planned_artwork_tracks: 0,
        completed_artwork_tracks: 0,
        planned_artwork_read_bytes_upper_bound: 0,
        tracks,
        failure: None,
        phone_writes: 0,
        journal_mutations: 0,
        metadata_projection_verified: false,
        permanent_artwork_verified: false,
        atomic_snapshot_claimed: false,
        limitations: ["test"; 4],
    }
}

#[test]
fn complete_managed_audio_report_is_required_with_explicit_empty_scope() {
    let current = current();
    let temp = Temp::new();
    let binding = binding(&temp);
    let report = physical_report(&current, Some(&binding));
    assert!(ordinary_bridge::require_complete_preservation(&current, &[binding], &report).is_ok());
    let empty = physical_report(&current, None);
    assert!(ordinary_bridge::require_complete_preservation(&current, &[], &empty).is_ok());
}

#[test]
fn partial_or_foreign_managed_report_never_passes_the_completion_gate() {
    for field in 0..12 {
        let current = current();
        let temp = Temp::new();
        let binding = binding(&temp);
        let mut report = physical_report(&current, Some(&binding));
        assert!(
            ordinary_bridge::require_complete_preservation(
                &current,
                std::slice::from_ref(&binding),
                &report
            )
            .is_ok()
        );
        match field {
            0 => report.verification_completed = false,
            1 => report.all_selected_audio_verified = false,
            2 => report.failure = Some("late state failure".into()),
            3 => report.completed_batches = 0,
            4 => report.total_verified_journal_bindings += 1,
            5 => report.device_identifier_sha256 = hex(&[9; 32]),
            6 => report.library_identity_sha256 = hex(&[9; 32]),
            7 => report.input_receipt_sha256 = hex(&[9; 32]),
            8 => report.media_anchor += 1,
            9 => report.phone_writes = 1,
            10 => report.journal_mutations = 1,
            11 => report.tracks.clear(),
            _ => unreachable!(),
        }
        assert!(
            ordinary_bridge::require_complete_preservation(&current, &[binding], &report).is_err(),
            "field {field}"
        );
    }
}

#[test]
fn provisional_changed_or_duplicate_managed_audio_never_passes() {
    for field in 0..8 {
        let current = current();
        let temp = Temp::new();
        let binding = binding(&temp);
        let mut report = physical_report(&current, Some(&binding));
        assert!(
            ordinary_bridge::require_complete_preservation(
                &current,
                std::slice::from_ref(&binding),
                &report
            )
            .is_ok()
        );
        let track = &mut report.tracks[0];
        match field {
            0 => track.audio_status = jv::AudioStatus::ReadPendingStateGuard,
            1 => track.audio_observed_sha256 = Some(hex(&[9; 32])),
            2 => track.path.push('x'),
            3 => track.audio_bytes += 1,
            4 => track.encoded_audio_sha256 = hex(&[9; 32]),
            5 => track.finding = Some("catalog_mismatch"),
            6 => track.track_id = "ffffffffffffffff".into(),
            7 => track.audio_attributes = None,
            _ => unreachable!(),
        }
        assert!(
            ordinary_bridge::require_complete_preservation(&current, &[binding], &report).is_err(),
            "field {field}"
        );
    }
}

#[test]
fn incomplete_managed_artwork_count_never_passes_even_with_verified_audio() {
    let current = current();
    let temp = Temp::new();
    let mut binding = binding(&temp);
    let baseline = physical_report(&current, Some(&binding));
    assert!(
        ordinary_bridge::require_complete_preservation(
            &current,
            std::slice::from_ref(&binding),
            &baseline
        )
        .is_ok()
    );
    binding.content.artwork = Some(crate::engine::state::journal::Artwork {
        cache_id: 7,
        bytes: 100,
        sha256: [7; 32],
    });
    let mut report = physical_report(&current, Some(&binding));
    report.planned_artwork_tracks = 1;
    assert!(ordinary_bridge::require_complete_preservation(&current, &[binding], &report).is_err());
}
