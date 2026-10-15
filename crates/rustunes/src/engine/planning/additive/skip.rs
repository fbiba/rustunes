//! Which refusals leave their source out of the run, and the plain words a
//! refusal is reported in.
//!
//! A refusal is a Skip when it concerns one source file, leaving that file
//! out leaves its phone track (if it has one) exactly as it is, and the
//! planner decided it before allocating anything for the file, so no other
//! source's groups, identities, destination or cover depend on it. The rest
//! of the plan goes on without it. Refusals about the journal, the phone's
//! state or groups other sources share stay Conflicts, which hold the whole
//! run.

use super::{Decision, SourceDecision};
use crate::engine::execution::flac_to_alac;

/// The refusals that leave their source out.
const LEFT_OUT: [&str; 13] = [
    // The file itself: unreadable, changed while it was read, outside what
    // the phone takes, or a second copy of another file's audio.
    "source_inspection_failed",
    "source_changed_or_invalid",
    "unsupported_source",
    "mp3_source_profile_unsupported",
    "flac_conversion_refused",
    "duplicate_source_audio",
    // The phone may hold it already, so adding it could make a duplicate.
    "plausible_existing_audio_requires_comparison",
    // A phone track whose source changed in a way this run does not send:
    // the track stays as it is. `sync --replace-changed` replaces the first
    // two, whose sources the planner checked before describing them.
    "managed_audio_replacement_proposed",
    "metadata_update_needs_replacement",
    "managed_audio_replacement_evidence_refused",
    "metadata_update_unsupported",
    "mp3_cover_update_unsupported",
    "existing_metadata_update_unqualified",
];

/// Turn each refusal `LEFT_OUT` names into a Skip with its plain reason.
pub(super) fn leave_out(rows: &mut [SourceDecision]) {
    let reasons: Vec<_> = rows
        .iter()
        .map(|row| match &row.decision {
            Decision::Conflict { code, message } if LEFT_OUT.contains(code) => {
                Some(reason(row, code, message, rows))
            }
            _ => None,
        })
        .collect();
    for (row, reason) in rows.iter_mut().zip(reasons) {
        if let (Some(reason), Decision::Conflict { code, message }) = (reason, &row.decision) {
            row.decision = Decision::Skip {
                code,
                reason,
                message: message.clone(),
            };
        }
    }
}

const REPLACE: &str = "`rustunes sync --replace-changed` replaces the track (its play counts, ratings and playlist places are lost).";
const DETAILS: &str = "The plan has the details.";

/// What `code` (with `message`, its full text) says about `row`, in plain
/// words: what is wrong, then what to do. `rows` is the whole plan.
pub(crate) fn reason(
    row: &SourceDecision,
    code: &str,
    message: &str,
    rows: &[SourceDecision],
) -> String {
    let tracks = |filter: &dyn Fn(&super::ExistingMatch) -> bool| {
        row.matches
            .iter()
            .filter(|m| filter(m))
            .map(|m| m.track_id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match code {
        "source_inspection_failed" => {
            "rustunes couldn't read it as a music file. Check that it plays, or encode it again."
                .into()
        }
        "source_changed_or_invalid" => {
            "it changed while rustunes was reading it. Sync again once nothing is writing to it."
                .into()
        }
        "unsupported_source" => sentence(
            row.validation_issues
                .iter()
                .filter(|issue| issue.severity == crate::engine::planning::source_validation::Severity::Error)
                .map(|issue| validation_problem(issue.code, &issue.message))
                .collect(),
            message,
        ),
        "mp3_source_profile_unsupported" => sentence(
            row.mp3_insertion
                .iter()
                .flat_map(|a| a.batch_refusals())
                .map(|issue| batch_problem(issue.code, &issue.detail))
                .collect(),
            message,
        ),
        "flac_conversion_refused" => {
            match row.mp3.as_ref().and_then(|s| s.stream.as_ref()) {
                Some(stream) if stream.bits_per_sample > flac_to_alac::MAX_BITS => format!(
                    "it is {}-bit, and ALAC holds at most {} bits, so converting it would lose audio. Convert it to {}-bit yourself to send it.",
                    stream.bits_per_sample,
                    flac_to_alac::MAX_BITS,
                    flac_to_alac::MAX_BITS
                ),
                Some(stream) if !(1..=2).contains(&stream.encoded.channels) => format!(
                    "it has {} channels, and rustunes converts only mono and stereo FLACs. Mix it down to stereo yourself to send it.",
                    stream.encoded.channels
                ),
                _ => format!("rustunes can't convert it to ALAC without losing audio ({message})."),
            }
        }
        "duplicate_source_audio" => {
            let others: Vec<_> = rows
                .iter()
                .filter(|other| {
                    other.relative_path != row.relative_path
                        && other.encoded_audio_sha256.is_some()
                        && other.encoded_audio_sha256 == row.encoded_audio_sha256
                })
                .map(|other| format!("{:?}", other.relative_path))
                .collect();
            format!(
                "it has the same audio as {}, so rustunes leaves them all out. Keep only one of them in the folder.",
                if others.is_empty() {
                    "another file".to_owned()
                } else {
                    others.join(", ")
                }
            )
        }
        "plausible_existing_audio_requires_comparison" => {
            // An MPEG refusal names its track in its message.
            let mut ids = tracks(&|m| !m.encoded_audio_equal);
            if ids.is_empty() {
                ids = message
                    .split_once("track ")
                    .map(|(_, rest)| rest.chars().take_while(char::is_ascii_hexdigit).collect())
                    .unwrap_or_default();
            }
            let (which, delete) = if ids.is_empty() {
                ("a track".to_owned(), "ID".to_owned())
            } else {
                (format!("track {ids}"), ids.replace(", ", " "))
            };
            format!(
                "the phone already has a track with its title, its artist or album, and its length ({which}), maybe an earlier version of it; rustunes doesn't add it, so as not to make a duplicate. If it is a different recording, or a version you want to replace, remove that track with `rustunes delete {delete}` and sync again."
            )
        }
        "managed_audio_replacement_proposed" => format!(
            "its audio changed since rustunes sent it, and the phone keeps the earlier version. {REPLACE}"
        ),
        "managed_audio_replacement_evidence_refused" => {
            "rustunes sent an earlier version of it but can't tell which phone track that is, so the phone keeps what it has."
                .into()
        }
        "metadata_update_needs_replacement" => {
            let field = message
                .strip_prefix("the source has no ")
                .and_then(|rest| rest.split_once(','))
                .map_or("value", |(field, _)| field);
            format!(
                "it has no {field} tag, but its phone track has one, which an edit can't clear, so the phone keeps the old one. {REPLACE}"
            )
        }
        "metadata_update_unsupported" => {
            let problems: Vec<_> = row
                .mp3_insertion
                .iter()
                .flat_map(|a| a.batch_refusals())
                .map(|issue| batch_problem(issue.code, &issue.detail))
                .collect();
            if problems.is_empty() {
                format!(
                    "its tags changed in a way rustunes can't edit on the phone ({message}), so the phone keeps the old ones."
                )
            } else {
                let (problems, fixes): (Vec<_>, Vec<_>) = problems.into_iter().unzip();
                with_fixes(
                    format!(
                        "{}, so its new tags can't be sent and the phone keeps the old ones",
                        problems.join("; ")
                    ),
                    fixes,
                )
            }
        }
        "mp3_cover_update_unsupported" => {
            let id = tracks(&|_| true);
            format!(
                "its cover changed since rustunes sent it, and rustunes sends this kind of file's cover only with its audio, so the phone keeps the old cover and tags. To send the new cover, remove the track with `rustunes delete {}` and sync again.",
                if id.is_empty() { "ID" } else { &id }
            )
        }
        "existing_metadata_update_unqualified" => {
            "the phone already has its audio, from outside rustunes, with other tags; rustunes edits only the tracks it sent, so that track keeps its tags."
                .into()
        }
        // Refusals that hold the run.
        "journal_reconciliation_required" => {
            "rustunes' journal has unfinished work, or a track it sent changed on the phone; that must be settled before anything new is added. `rustunes status` shows it."
                .into()
        }
        "mp3_journal_required" => {
            "rustunes sends this kind of file in insert batches, which need this phone's journal, and there is none yet. Run `rustunes pull` to build it, then sync again."
                .into()
        }
        "multiple_existing_audio_matches" => {
            let ids = tracks(&|m| m.encoded_audio_equal);
            format!(
                "the phone has more than one track with exactly its audio{}, and rustunes won't choose between them. Remove the extra copies with `rustunes delete`, then sync again.",
                if ids.is_empty() {
                    String::new()
                } else {
                    format!(" (tracks {ids})")
                }
            )
        }
        "device_mpeg_classification_conflict" => {
            format!("the phone's copy of it doesn't match what rustunes measured of that copy. Sync again; if it stays, {}", lower(DETAILS))
        }
        "composer_lookup_unqualified" => {
            format!("the phone already has its composer, in a form rustunes can't reuse. {DETAILS}")
        }
        "candidate_or_grouping_conflict" | "mp3_candidate_or_grouping_conflict" => {
            format!("rustunes couldn't place it among the phone's albums and artists. {DETAILS}")
        }
        _ => format!("rustunes couldn't decide what to do with it. {DETAILS}"),
    }
}

/// `problems` (each what is wrong and how to fix it) as one reason, or
/// `message` when none is known.
fn sentence(problems: Vec<(String, String)>, message: &str) -> String {
    let mut seen = Vec::new();
    for problem in problems {
        if !seen.contains(&problem) {
            seen.push(problem);
        }
    }
    if seen.is_empty() {
        return format!("rustunes can't send it as it is ({message}).");
    }
    let (problems, fixes): (Vec<_>, Vec<_>) = seen.into_iter().unzip();
    with_fixes(problems.join("; "), fixes)
}

fn with_fixes(problem: String, fixes: Vec<String>) -> String {
    let mut text = format!("{problem}.");
    let mut seen = Vec::new();
    for fix in fixes.into_iter().filter(|fix| !fix.is_empty()) {
        if !seen.contains(&fix) {
            text.push(' ');
            text.push_str(&fix);
            seen.push(fix);
        }
    }
    text
}

fn lower(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}

const FIX_TAG: &str = "Fix the tag.";

/// An issue that keeps an MP3, FLAC or other non-ALAC source out of an
/// insert batch (`preparation::assess`).
fn batch_problem(code: &str, detail: &str) -> (String, String) {
    match code {
        "mp3_metadata_policy_required" => tag_problem(detail),
        "mp3_native_transfer_size_limit" => (
            "it is larger than the 64 MiB rustunes sends as one track".into(),
            "Encode it smaller.".into(),
        ),
        "mp3_timing_policy_required" => (
            "rustunes can't work out its exact length from its audio frames".into(),
            "Encode it again.".into(),
        ),
        _ => (
            format!("rustunes can't send it as it is ({detail})"),
            String::new(),
        ),
    }
}

/// A tag the phone cannot take, from the refusal of the MPEG and FLAC tag
/// projection (`music_protocol::media::mpeg::projection`).
fn tag_problem(detail: &str) -> (String, String) {
    if let Some((field, rest)) = detail.split_once(" has ")
        && let Some(count) =
            rest.strip_suffix(" values in the file's tag; edit the tag to keep one")
    {
        return (
            format!("it lists {count} {field}s, and the phone keeps one"),
            format!("Edit the tag so it has a single {field}."),
        );
    }
    if let Some(field) = detail.strip_prefix("missing ") {
        return (
            format!("it has no {field} tag, which the phone needs"),
            "Add one.".into(),
        );
    }
    if let Some(field) = detail
        .strip_prefix("invalid ")
        .and_then(|rest| rest.strip_suffix(" text"))
    {
        return (invalid_text(field), FIX_TAG.into());
    }
    if let Some(field) = detail
        .strip_prefix("zero ")
        .and_then(|rest| rest.strip_suffix(" numbering"))
    {
        return (format!("its {field} number or total is 0"), FIX_TAG.into());
    }
    if let Some(field) = detail.strip_suffix(" total without a number") {
        return (
            format!("it has a {field} total but no {field} number"),
            FIX_TAG.into(),
        );
    }
    if let Some(field) = detail.strip_suffix(" number exceeds total") {
        return (
            format!("its {field} number is larger than its total"),
            FIX_TAG.into(),
        );
    }
    match detail {
        "a release date must start with a four-digit year" => (
            "its release date doesn't start with a four-digit year".into(),
            FIX_TAG.into(),
        ),
        "zero release year" => ("its release year is 0".into(), FIX_TAG.into()),
        "numeric ID3 genres require an explicit label mapping" => (
            "its genre is an ID3 genre number, not a name".into(),
            "Write the genre's name in the tag.".into(),
        ),
        "MPEG compilation needs explicit album artist" => (
            "it is marked as part of a compilation but has no album artist".into(),
            "Add an album artist tag.".into(),
        ),
        _ => (
            format!("its tags can't go to the phone as they are ({detail})"),
            FIX_TAG.into(),
        ),
    }
}

/// A validation error of an ALAC source (`source_validation::inspect`).
fn validation_problem(code: &str, message: &str) -> (String, String) {
    match code {
        "required_tag_absent" => match message
            .strip_prefix("The current insertion profile requires a standard ")
            .and_then(|rest| rest.strip_suffix(" tag."))
        {
            Some(field) => (
                format!(
                    "it has no {} tag, which the phone needs",
                    field.replace('_', " ")
                ),
                "Add one.".into(),
            ),
            None => tag_problem(message),
        },
        "invalid_tag_text" => match message.strip_suffix(" contains blank text or NUL characters.")
        {
            Some(field) => (
                format!(
                    "its {} tag is blank or holds a NUL character",
                    field.replace('_', " ")
                ),
                FIX_TAG.into(),
            ),
            None => tag_problem(message),
        },
        "invalid_numbering" => {
            match message.strip_suffix(" total requires a number between one and the total.") {
                Some(field) => (
                    format!(
                        "it has a {field} total without a {field} number between one and that total"
                    ),
                    FIX_TAG.into(),
                ),
                None => tag_problem(message),
            }
        }
        "metadata_projection_unsupported" => projection_problem(message),
        "compilation_grouping_unverified" => (
            "it is marked as part of a compilation but has no single album artist".into(),
            "Give it one album artist tag.".into(),
        ),
        "audio_profile_unsupported" => (
            message.into(),
            "Encode it again in a form the phone plays.".into(),
        ),
        "artwork_profile_unsupported" => (
            "its cover image can't be read".into(),
            "Replace or remove the cover.".into(),
        ),
        "source_inspection_failed" => (
            "rustunes couldn't read it as a music file".into(),
            "Check that it plays, or encode it again.".into(),
        ),
        _ => (
            format!("rustunes can't send it as it is ({message})"),
            String::new(),
        ),
    }
}

/// An ALAC's tags the catalogue projection refuses
/// (`music_protocol::media::metadata::Metadata::projected`).
fn projection_problem(message: &str) -> (String, String) {
    if let Some(atoms) = message.strip_prefix("duplicate outer standard atoms are unprojectable: ")
    {
        let fields: Vec<_> = atoms
            .trim_matches(['[', ']'])
            .split(", ")
            .map(|atom| atom_field(atom.trim_matches('"')))
            .collect();
        return (
            format!("it lists its {} more than once", fields.join(" and ")),
            "Edit the tag so each has a single value.".into(),
        );
    }
    if let Some(field) = message.strip_prefix("missing standard ") {
        return (
            format!("it has no {field} tag, which the phone needs"),
            "Add one.".into(),
        );
    }
    if let Some(field) = message
        .strip_prefix("invalid standard ")
        .and_then(|rest| rest.strip_suffix(" text"))
    {
        return (invalid_text(field), FIX_TAG.into());
    }
    (
        format!("its tags can't go to the phone as they are ({message})"),
        FIX_TAG.into(),
    )
}

/// A tag value the projections refuse (`invalid {field} text`). An empty one
/// is never refused: it is no value, and a required field left with none is
/// reported missing.
fn invalid_text(field: &str) -> String {
    format!("its {field} tag holds a control character or is longer than 4,096 bytes")
}

/// The tag an MP4 metadata atom holds.
fn atom_field(atom: &str) -> &str {
    match atom {
        "©nam" => "title",
        "©ART" => "artist",
        "©alb" => "album",
        "aART" => "album artist",
        "©gen" | "gnre" => "genre",
        "©wrt" => "composer",
        "cprt" => "copyright",
        "©day" => "release date",
        "©cmt" => "comment",
        "sonm" => "sort title",
        "soar" => "sort artist",
        "soal" => "sort album",
        "soaa" => "sort album artist",
        "soco" => "sort composer",
        "rtng" => "content rating",
        "cpil" => "compilation flag",
        "trkn" => "track number",
        "disk" => "disc number",
        other => other,
    }
}

#[cfg(test)]
mod tests;
