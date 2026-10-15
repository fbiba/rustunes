use super::*;

#[test]
fn a_refused_tag_reads_as_what_is_wrong_and_how_to_fix_it() {
    // The tag projection's refusals, as `music_protocol` words them.
    for (detail, reason) in [
        (
            "title has 2 values in the file's tag; edit the tag to keep one",
            "it lists 2 titles, and the phone keeps one. Edit the tag so it has a single title.",
        ),
        (
            "sort composer has 3 values in the file's tag; edit the tag to keep one",
            "it lists 3 sort composers, and the phone keeps one. Edit the tag so it has a single sort composer.",
        ),
        (
            "missing album",
            "it has no album tag, which the phone needs. Add one.",
        ),
        (
            "invalid artist text",
            "its artist tag holds a control character or is longer than 4,096 bytes. Fix the tag.",
        ),
        (
            "zero track numbering",
            "its track number or total is 0. Fix the tag.",
        ),
        (
            "disc total without a number",
            "it has a disc total but no disc number. Fix the tag.",
        ),
        (
            "track number exceeds total",
            "its track number is larger than its total. Fix the tag.",
        ),
        (
            "a release date must start with a four-digit year",
            "its release date doesn't start with a four-digit year. Fix the tag.",
        ),
        ("zero release year", "its release year is 0. Fix the tag."),
        (
            "numeric ID3 genres require an explicit label mapping",
            "its genre is an ID3 genre number, not a name. Write the genre's name in the tag.",
        ),
        (
            "MPEG compilation needs explicit album artist",
            "it is marked as part of a compilation but has no album artist. Add an album artist tag.",
        ),
        (
            "something new",
            "its tags can't go to the phone as they are (something new). Fix the tag.",
        ),
    ] {
        let problem = batch_problem("mp3_metadata_policy_required", detail);
        assert_eq!(sentence(vec![problem], "unused"), reason, "{detail}");
    }
    // Two problems share one sentence, and a fix is given once.
    assert_eq!(
        sentence(
            vec![
                tag_problem("missing title"),
                tag_problem("missing album"),
                tag_problem("missing title"),
            ],
            "unused"
        ),
        "it has no title tag, which the phone needs; it has no album tag, which the phone needs. Add one."
    );
    assert_eq!(
        sentence(Vec::new(), "the planner's own words"),
        "rustunes can't send it as it is (the planner's own words)."
    );
}

#[test]
fn an_alac_validation_error_reads_as_the_tag_to_fix() {
    for (code, message, reason) in [
        (
            "required_tag_absent",
            "The current insertion profile requires a standard title tag.",
            "it has no title tag, which the phone needs. Add one.",
        ),
        (
            "invalid_tag_text",
            "album_artist contains blank text or NUL characters.",
            "its album artist tag is blank or holds a NUL character. Fix the tag.",
        ),
        (
            "invalid_numbering",
            "track total requires a number between one and the total.",
            "it has a track total without a track number between one and that total. Fix the tag.",
        ),
        (
            "metadata_projection_unsupported",
            "invalid standard composer text",
            "its composer tag holds a control character or is longer than 4,096 bytes. Fix the tag.",
        ),
        // An emptied title is a missing one.
        (
            "metadata_projection_unsupported",
            "missing standard title",
            "it has no title tag, which the phone needs. Add one.",
        ),
        (
            "metadata_projection_unsupported",
            r#"duplicate outer standard atoms are unprojectable: ["©nam", "aART"]"#,
            "it lists its title and album artist more than once. Edit the tag so each has a single value.",
        ),
        (
            "audio_profile_unsupported",
            "the phone plays ALAC with one or two channels; this has 6",
            "the phone plays ALAC with one or two channels; this has 6. Encode it again in a form the phone plays.",
        ),
    ] {
        assert_eq!(
            sentence(vec![validation_problem(code, message)], "unused"),
            reason
        );
    }
}

fn row(path: &str, code: &'static str, message: &str) -> SourceDecision {
    SourceDecision {
        relative_path: path.into(),
        managed_audio_replacement: None,
        source_file_bytes: None,
        source_file_sha256: None,
        encoded_audio_sha256: Some("ab".repeat(32)),
        projected: None,
        mp3: None,
        mp3_insertion: None,
        flac_to_alac: false,
        validation_issues: Vec::new(),
        matches: Vec::new(),
        artwork: super::super::ArtworkDecision::UnavailableUntilSourceValidated,
        native_composer: None,
        decision: Decision::Conflict {
            code,
            message: message.into(),
        },
    }
}

/// The plain reason of each refusal says no more than a person needs: none
/// is the planner's message escaped, or its jargon.
fn plain(reason: &str) {
    for jargon in ["\\\"", "profile", "qualif", "Encoded mismatch", "candidate"] {
        assert!(!reason.contains(jargon), "{reason}");
    }
    assert!(reason.ends_with('.'), "{reason}");
}

#[test]
fn every_refusal_about_one_file_is_left_out_and_every_other_holds_the_run() {
    let mut rows: Vec<_> = LEFT_OUT
        .iter()
        .enumerate()
        .map(|(i, code)| row(&format!("{i}.flac"), code, "the planner's own words"))
        .collect();
    // These hold the run: the journal, the phone's state, or groups other
    // sources share.
    const HOLDING: [&str; 11] = [
        "journal_reconciliation_required",
        "mp3_journal_required",
        "multiple_existing_audio_matches",
        "device_mpeg_classification_conflict",
        "composer_lookup_unqualified",
        "candidate_or_grouping_conflict",
        "mp3_candidate_or_grouping_conflict",
        "candidate_required",
        "unvalidated",
        "mp3_insertion_unqualified",
        "a_code_nobody_classified",
    ];
    rows.extend(
        HOLDING
            .iter()
            .map(|code| row(&format!("{code}.flac"), code, "the planner's own words")),
    );
    for (i, row) in rows.iter_mut().enumerate() {
        row.encoded_audio_sha256 = Some(format!("{i:064x}"));
    }
    leave_out(&mut rows);
    for row in &rows {
        match &row.decision {
            Decision::Skip {
                code,
                reason,
                message,
            } => {
                assert!(LEFT_OUT.contains(code), "{code}");
                assert_eq!(message, "the planner's own words");
                plain(reason);
            }
            Decision::Conflict { code, message } => {
                assert!(HOLDING.contains(code), "{code}");
                plain(&reason(row, code, message, &rows));
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(
        rows.iter()
            .filter(|r| matches!(r.decision, Decision::Skip { .. }))
            .count(),
        LEFT_OUT.len()
    );
}

#[test]
fn a_reason_names_what_its_fix_needs() {
    // Two files with the same audio name each other.
    let rows = vec![
        row("a.flac", "duplicate_source_audio", ""),
        row("b.flac", "duplicate_source_audio", ""),
    ];
    assert_eq!(
        reason(&rows[0], "duplicate_source_audio", "", &rows),
        "it has the same audio as \"b.flac\", so rustunes leaves them all out. Keep only one of them in the folder."
    );
    // The track an MP3 may be an earlier version of is in its message.
    let plausible = reason(
        &rows[0],
        "plausible_existing_audio_requires_comparison",
        "Encoded mismatch is not absence: track 00000000000003e9 has this source's title, its artist or album and its duration",
        &rows,
    );
    assert!(
        plausible.contains("(track 00000000000003e9)")
            && plausible.contains("`rustunes delete 00000000000003e9`"),
        "{plausible}"
    );
    // A field only a replacement clears is named, with the replacement.
    let cleared = reason(
        &rows[0],
        "metadata_update_needs_replacement",
        "the source has no genre, and an edit cannot clear one on the phone; sync with --replace-changed",
        &rows,
    );
    assert!(
        cleared.starts_with("it has no genre tag, but its phone track has one")
            && cleared.contains("`rustunes sync --replace-changed`"),
        "{cleared}"
    );
    assert!(
        reason(&rows[0], "managed_audio_replacement_proposed", "", &rows)
            .contains("`rustunes sync --replace-changed`")
    );
}
