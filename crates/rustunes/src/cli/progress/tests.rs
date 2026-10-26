use super::*;
use crate::engine::{Hold, SkipStage, Skipped};
use std::path::Path;

/// A sync that left out one file while planning and dropped one while
/// sending, as `outcome`.
pub(crate) fn skipping(outcome: SyncOutcome) -> SyncResult {
    let mut result = SyncResult::new(
        outcome,
        Path::new("/state/runs/1"),
        Path::new("/state/runs/1/plan.json"),
        Path::new("/state/journal"),
        2,
        0,
    );
    result.skipped = vec![
        Skipped {
            path: "Artist A - \"One\".flac".into(),
            code: "mp3_source_profile_unsupported",
            reason: "it lists 2 titles, and the phone keeps one. Edit the tag so it has a single title.".into(),
            detail: "FLAC source is outside the insert-batch profile: title has 2 values in the file's tag; edit the tag to keep one".into(),
            stage: SkipStage::Plan,
        },
        Skipped {
            path: "Artist B - Two.flac".into(),
            code: "flac_conversion_failed",
            reason: "its conversion to ALAC failed its check, so it was not sent.".into(),
            detail: "ffmpeg exited with status 1".into(),
            stage: SkipStage::Send,
        },
    ];
    result
}

#[test]
fn a_finished_sync_names_each_file_it_left_out_in_plain_words() {
    let result = skipping(SyncOutcome::Completed);
    assert!(!result.sent_all());
    let lines = finished(&result);
    let at = lines.iter().position(|l| l == "Skipped 2 files:").unwrap();
    assert_eq!(
        lines[at + 1..],
        [
            "  Artist A - \"One\".flac: it lists 2 titles, and the phone keeps one. Edit the tag so it has a single title.",
            "  Artist B - Two.flac: its conversion to ALAC failed its check, so it was not sent.",
            "The run directory (/state/runs/1) and --message-format json have the full details.",
        ]
    );
    // No sentence is a JSON string, and the full refusal stays out.
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("\\\"") && !l.contains("profile"))
    );
    // A dry run would leave them out.
    let mut dry = skipping(SyncOutcome::Preflight);
    dry.skipped.truncate(1);
    assert!(dry.sent_all());
    let lines = finished(&dry);
    assert!(
        lines.contains(&"Would skip 1 file:".to_owned()),
        "{lines:?}"
    );
    // Nothing left out, nothing said.
    dry.skipped.clear();
    assert_eq!(finished(&dry).len(), 1);
}

#[test]
fn a_blocked_sync_says_what_holds_it_and_how_to_send_the_rest() {
    let holds = SyncHolds {
        holds: vec![Hold {
            path: Some("a.m4a".into()),
            code: "multiple_existing_audio_matches".into(),
            reason: "the phone has more than one track with exactly its audio, and rustunes won't choose between them.".into(),
        }],
        hint: Some("To send the rest now without them, run `rustunes push --exclude a.m4a`: push sends new tracks only, and removes and updates nothing.".into()),
    };
    assert_eq!(
        blocked(&holds),
        "Nothing was sent. This must be fixed first:\n  a.m4a: the phone has more than one track with exactly its audio, and rustunes won't choose between them.\nTo send the rest now without them, run `rustunes push --exclude a.m4a`: push sends new tracks only, and removes and updates nothing."
    );
    let journal = SyncHolds {
        holds: vec![Hold {
            path: None,
            code: "outstanding_prepared_requires_resolution".into(),
            reason: "an earlier run prepared a change it never started.".into(),
        }],
        hint: None,
    };
    assert_eq!(
        blocked(&journal),
        "Nothing was sent. This must be fixed first:\n  journal: an earlier run prepared a change it never started."
    );
    assert_eq!(
        blocked(&SyncHolds {
            holds: Vec::new(),
            hint: None
        }),
        "Nothing was sent."
    );
}

#[test]
fn an_untested_catalogue_version_names_it_and_the_tested_ones() {
    assert_eq!(
        observation_line(&Observation::UntestedCatalogueVersion(2_420_000)),
        "The phone's Music database is version 2420000, which rustunes has not been tested with (tested: 2400060, 2410000); every table and column it reads is present"
    );
}
