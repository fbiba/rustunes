//! Terminal rendering for typed engine observations.
use crate::engine::{
    ApplyResult, CoverResult, Event, Observation, SyncHolds, SyncOutcome, SyncResult,
    execution::device,
};
use anyhow::Result;

/// A cached pass's per-track line that is not printed: one line per track
/// would flood the terminal and the log.
pub(super) fn thinned(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Observation(Observation::AudioFingerprinted {
            completed,
            total,
            reused: true,
            ..
        }) if completed % 50 != 0 && completed != total
    )
}

/// What a sync that sent nothing says: what holds it, and what to do.
pub(super) fn blocked(holds: &SyncHolds) -> String {
    let mut text = String::from("Nothing was sent.");
    if !holds.holds.is_empty() {
        text.push_str(&format!(
            " {} must be fixed first:",
            match holds.holds.len() {
                1 => "This".to_owned(),
                n => format!("These {n}"),
            }
        ));
        for hold in &holds.holds {
            text.push_str(&format!(
                "\n  {}: {}",
                hold.path.as_deref().unwrap_or("journal"),
                hold.reason
            ));
        }
    }
    if let Some(hint) = &holds.hint {
        text.push('\n');
        text.push_str(hint);
    }
    text
}

/// Each source the sync left out, with why and what to do; `verb` says
/// whether it was or would be.
fn skipped(lines: &mut Vec<String>, result: &SyncResult, verb: &str) {
    if result.skipped.is_empty() {
        return;
    }
    lines.push(format!(
        "{verb} {}:",
        match result.skipped.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        }
    ));
    for skipped in &result.skipped {
        lines.push(format!("  {}: {}", skipped.path, skipped.reason));
    }
    lines.push(format!(
        "The run directory ({}) and --message-format json have the full details.",
        result.run.display()
    ));
}

/// What a finished sync prints on stdout.
pub(super) fn finished(result: &SyncResult) -> Vec<String> {
    let mut lines = Vec::new();
    match result.outcome {
        SyncOutcome::Blocked => {}
        SyncOutcome::Preflight => {
            lines.push(format!(
                "Dry run complete. Saved plan: {}",
                result.plan.display()
            ));
            skipped(&mut lines, result, "Would skip");
        }
        SyncOutcome::Completed => {
            lines.push(format!(
                "Sync complete. Verified {} managed covers. Preserved {} existing covers without claiming image verification.",
                result.managed_covers_verified, result.existing_unmanaged_covers_unverified
            ));
            lines.push(format!("Journal: {}", result.journal.display()));
            if result.metadata_updates_verified > 0 {
                lines.push(format!(
                    "Verified {} metadata updates.",
                    result.metadata_updates_verified
                ));
            }
            if result.covers_left_out > 0 {
                lines.push(format!(
                    "Left out {} new covers whose album shows another image; their tracks were added without them.",
                    result.covers_left_out
                ));
            }
            skipped(&mut lines, result, "Skipped");
        }
    }
    lines
}

/// What a finished apply prints on stdout.
pub(super) fn applied(result: &ApplyResult) -> String {
    match *result {
        ApplyResult::Preflight { additions } => format!(
            "Live preflight passed: {additions} additions; no phone writes or journal reservations."
        ),
        ApplyResult::PreflightSourceChanges {
            additions,
            metadata_updates,
        } => format!(
            "Live preflight passed: {additions} additions and {metadata_updates} metadata updates; no phone writes or journal reservations."
        ),
        ApplyResult::Unchanged => {
            "Current library matches the plan: no audio additions. Artwork statuses remain separate."
                .into()
        }
        ApplyResult::Completed { additions } => format!(
            "Verified {additions} audio additions. No tracks deleted. Artwork updates remain separate."
        ),
    }
}

/// What a finished cover update prints on stdout.
pub(super) fn covered(result: &CoverResult) -> String {
    match *result {
        CoverResult::Unchanged => {
            "Artwork already matches the permanent device image; no update needed.".into()
        }
        CoverResult::Planned {
            track_id,
            cache_id,
            bytes,
        } => format!(
            "Artwork plan: track {track_id:016x}, cache {cache_id}, {bytes} cover bytes; no phone writes or reservations."
        ),
        CoverResult::Verified => {
            "Artwork verified in the permanent device cache. Audio and catalogue tracks preserved."
                .into()
        }
    }
}

pub(super) fn observation_line(event: &Observation<'_>) -> String {
    match event {
        Observation::CapturingCatalogue => "Copying the phone's Music catalogue".into(),
        Observation::SourcesInspected { total, .. } => format!("Inspected {total} sources"),
        Observation::RevalidatingSources { done, total } => {
            format!("Revalidated {done}/{total} planned sources")
        }
        Observation::CatalogueCaptureRetained(path) => {
            format!("Catalog capture retained at {}", path.display())
        }
        Observation::UntestedCatalogueVersion(version) => {
            let tested = music_protocol::catalogue::sqlite::TESTED_USER_VERSIONS.iter();
            let tested: Vec<_> = tested.map(i64::to_string).collect();
            format!(
                "The phone's Music database is version {version}, which rustunes has not been tested with (tested: {}); every table and column it reads is present",
                tested.join(", ")
            )
        }
        Observation::RecapturingBeforeAudioIndex => "Catalogue changed before audio indexing; retaining failed attempt and recapturing once within the original deadline".into(),
        Observation::RecapturingAfterAudioIndex => "Catalogue changed during audio indexing; retaining failed attempt and recapturing once within the original deadline".into(),
        Observation::FreshCatalogue { tracks, bytes } => {
            format!("Fresh catalog accepted: {tracks} tracks, {bytes} bytes of audio to index")
        }
        Observation::AudioFingerprinted {
            completed,
            total,
            bytes,
            other_codec,
            reused,
            ..
        } => {
            if *reused {
                format!("Matched {completed}/{total} unchanged tracks to cached fingerprints")
            } else if *other_codec {
                format!("Classified other audio {completed}/{total} tracks; {bytes} bytes verified")
            } else {
                format!("Fingerprinted {completed}/{total} tracks; {bytes} bytes verified")
            }
        }
        Observation::AudioIndexRetained { tracks, bytes } => {
            format!("Complete read-only index retained: {tracks} tracks, {bytes} bytes")
        }
        Observation::StateCaptureRetained(path) => {
            format!("Complete state capture retained at {}", path.display())
        }
        Observation::RecapturingAfterDroppedConnection => "The phone dropped a connection during the read-only state capture; retaining attempt 1 and recapturing complete state once within the original deadline".into(),
        Observation::RecapturingState => "Catalog changed during its read-only capture; retaining attempt 1 and recapturing complete state once within the original deadline".into(),
        Observation::VerifyingAudioBatch {
            number,
            total,
            tracks,
            bytes,
        } => format!("Verifying audio batch {number}/{total}: {tracks} tracks, {bytes} bytes"),
        Observation::AudioBatchChecked {
            number,
            total,
            matched,
            findings,
        } => format!(
            "Audio batch {number}/{total} checked: {matched} matching files, {findings} findings"
        ),
        Observation::VerifyingArtwork { number, total } => {
            format!("Verifying track artwork {number}/{total}")
        }
        Observation::ReusedArtworkProofs { reused, total } => {
            format!("Matched {reused}/{total} unchanged covers to verified proofs")
        }
        Observation::EmptyRendition { track, edge } => format!(
            "The cover of track {track:016x} is exact on the phone, but the phone left its {edge}x{edge} rendition empty"
        ),
        Observation::CopyingTrackAudio { title, .. } => format!("Copying the audio of {title:?}"),
        Observation::InspectingSource { number, total, path, .. } => {
            format!("Inspecting source {number}/{total}: {path:?}")
        }
        Observation::CacheUnavailable { cache, error } => {
            format!("{cache} unavailable, continuing without it: {error}")
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
