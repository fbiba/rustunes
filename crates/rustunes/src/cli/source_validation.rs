//! Human-readable presentation of source compatibility facts.
use crate::engine::model::{SourceDecode, SourceIssueSeverity, SourceValidationReport};
use std::io;

pub(super) fn write_text(
    report: &SourceValidationReport,
    output: &mut impl io::Write,
) -> io::Result<()> {
    writeln!(output, "File: {:?}", report.path)?;
    writeln!(output, "Sync profile: {}", report.profile)?;
    writeln!(
        output,
        "Result: {}",
        if report.supported_by_current_profile && report.mp3.is_some() {
            "source compatible with the insert-batch profile; library eligibility not checked"
        } else if report.supported_by_current_profile {
            "supported by the current sync profile"
        } else {
            "needs attention before this CLI can sync it"
        }
    )?;
    if let Some(mp3) = &report.mp3 {
        let audio = &mp3.fingerprint.audio;
        match &mp3.stream {
            None => writeln!(
                output,
                "Audio: MPEG-1 Layer III, {} Hz, {} channels, {} checked frames",
                audio.sample_rate, audio.channels, audio.frame_count
            )?,
            Some(stream) => writeln!(
                output,
                "Audio: {}, {} Hz, {}{} channels, {} kb/s{}",
                stream.format.label(),
                audio.sample_rate,
                if stream.bits_per_sample > 0 {
                    format!("{} bit, ", stream.bits_per_sample)
                } else {
                    String::new()
                },
                audio.channels,
                stream.bit_rate_kbps,
                if stream.variable_bit_rate {
                    " (variable)"
                } else {
                    ""
                }
            )?,
        }
        // What a player plays: Apple's MPEG duration, or the samples the
        // stream keeps after its encoder delay and padding.
        let played = match (&mp3.apple_timing, &mp3.stream) {
            (Some(timing), _) => Some(timing.duration),
            (None, Some(stream)) => Some(stream.valid_samples),
            (None, None) => None,
        };
        if let Some(samples) = played {
            writeln!(
                output,
                "Duration: {:.3} s ({samples} played samples at {} Hz)",
                samples as f64 / f64::from(audio.sample_rate),
                audio.sample_rate
            )?;
        }
        writeln!(
            output,
            "File: {} bytes; SHA-256 {}",
            mp3.fingerprint.file_bytes, mp3.fingerprint.full_file_sha256
        )?;
        writeln!(
            output,
            "Encoded {} identity: {}",
            if mp3.stream.is_none() {
                "MPEG frame"
            } else {
                "audio"
            },
            mp3.fingerprint.audio.sha256
        )?;
        if let Some(version) = mp3.id3_version {
            writeln!(output, "Tag store: ID3v2.{version}")?;
        }
        writeln!(output, "Embedded pictures: {}", mp3.artwork.len())?;
    }
    if let Some(audio) = &report.audio {
        writeln!(
            output,
            "Audio: ALAC, {} Hz, {} bit, {} channels",
            audio.sample_rate_hz, audio.bit_depth, audio.channels
        )?;
        writeln!(
            output,
            "Duration: {:.3} s ({} ticks / {} Hz)",
            audio.duration_ticks as f64 / f64::from(audio.timescale),
            audio.duration_ticks,
            audio.timescale
        )?;
        writeln!(
            output,
            "File: {} bytes; SHA-256 {}",
            audio.file_bytes, audio.file_sha256
        )?;
        writeln!(
            output,
            "Encoded audio identity: {}",
            audio.encoded_audio_sha256
        )?;
    }
    if let Some(tags) = &report.tags {
        for (name, values) in [
            ("Title", &tags.title),
            ("Artist", &tags.artist),
            ("Album", &tags.album),
            ("Album artist", &tags.album_artist),
            ("Genre", &tags.genre),
            ("Composer", &tags.composer),
            ("Copyright", &tags.copyright),
            ("Comment", &tags.comment),
        ] {
            writeln!(output, "{name}: {values:?}")?;
        }
        for (name, values) in [
            ("Sort title tag", &tags.sort_title),
            ("Sort artist tag", &tags.sort_artist),
            ("Sort album tag", &tags.sort_album),
            ("Sort album artist tag", &tags.sort_album_artist),
            ("Sort composer tag", &tags.sort_composer),
        ] {
            if !values.is_empty() {
                writeln!(output, "{name}: {values:?}")?;
            }
        }
        writeln!(
            output,
            "Track: {}; disc: {}; year: {}; rating: {}",
            numbering(tags.track, tags.track_total),
            numbering(tags.disc, tags.disc_total),
            optional(tags.year),
            optional(tags.content_rating)
        )?;
        writeln!(
            output,
            "Release date: {:?}; compilation: {}",
            tags.release_date,
            optional(tags.compilation)
        )?;
    }
    if let Some(tags) = &report.projected_tags {
        writeln!(
            output,
            "Catalog artist: {:?}; grouping artist: {:?}",
            tags.artist, tags.effective_artist
        )?;
        write!(
            output,
            "Sort names sent: title {:?}; artist {:?}; album {:?}",
            tags.sort_title, tags.sort_artist, tags.sort_album
        )?;
        if let Some(name) = &tags.sort_album_artist {
            write!(output, "; album artist {name:?}")?;
        }
        if let Some(name) = &tags.sort_composer {
            write!(output, "; composer {name:?}")?;
        }
        writeln!(output, " (a group already on the phone keeps its own)")?;
    }
    match &report.artwork {
        Some(cover) => writeln!(
            output,
            "Artwork: JPEG{}, {} bytes; SHA-256 {} ({})",
            match cover.converted_from {
                Some(format) => format!(
                    " converted from the embedded {}",
                    format.to_ascii_uppercase()
                ),
                None => String::new(),
            },
            cover.bytes,
            cover.sha256,
            if cover.converted_from.is_some() {
                "sent in its place; the phone keeps only JPEG covers"
            } else {
                "container checked, image not decoded"
            }
        )?,
        None => writeln!(output, "Artwork: no supported embedded cover reported")?,
    }
    writeln!(
        output,
        "Audio decode: {}",
        match report.decode {
            SourceDecode::NotRequested => "not requested (use --decode)",
            SourceDecode::SkippedInvalidInput => "skipped because container validation failed",
            SourceDecode::Passed => "passed (FFmpeg, complete audio stream)",
            SourceDecode::Failed => "failed or could not complete",
        }
    )?;
    for issue in &report.issues {
        let level = match issue.severity {
            SourceIssueSeverity::Error => "ERROR",
            SourceIssueSeverity::Warning => "WARN",
        };
        writeln!(output, "{level} {}: {:?}", issue.code, issue.message)?;
    }
    writeln!(
        output,
        "Profile limits describe this CLI; they do not establish incompatibility with iOS."
    )
}

/// A track or disc number with its total: "3/9", "3", or "absent".
fn numbering(number: Option<u32>, total: Option<u32>) -> String {
    match (number, total) {
        (Some(number), Some(total)) => format!("{number}/{total}"),
        (Some(number), None) => number.to_string(),
        (None, Some(total)) => format!("absent/{total}"),
        (None, None) => "absent".into(),
    }
}

fn optional(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "absent".into(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::SourceIssue;

    #[test]
    fn supported_alac_retains_cli_profile_scope() {
        let report = crate::engine::planning::source_validation::inspect(
            &crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a"),
            false,
        );
        let mut text = Vec::new();
        write_text(&report, &mut text).unwrap();
        assert!(
            String::from_utf8(text)
                .unwrap()
                .contains("Profile limits describe this CLI")
        );
    }

    #[test]
    fn plain_mp3_retains_source_compatibility_wording() {
        let report = crate::engine::planning::source_validation::mpeg_tests::plain_report();
        let mut text = Vec::new();
        write_text(&report, &mut text).unwrap();
        assert!(
            String::from_utf8(text)
                .unwrap()
                .contains("source compatible")
        );
    }

    #[test]
    fn other_formats_report_their_own_format_tags_and_profile() {
        let report = crate::engine::planning::source_validation::inspect(
            &crate::test_fixtures::workspace_root().join("tests/fixtures/tone-flac.flac"),
            false,
        );
        assert_eq!(report.profile, "other-format-insert-batch-source");
        let tags = report.tags.as_ref().unwrap();
        assert_eq!(tags.title, ["Format title"]);
        assert_eq!(
            (tags.track, tags.track_total, tags.year),
            (Some(3), Some(9), Some(2024))
        );
        assert_eq!(
            report.projected_tags.as_ref().unwrap().album,
            "Format album"
        );
        let mut text = Vec::new();
        write_text(&report, &mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(
            text.contains("Audio: FLAC, 44100 Hz, 16 bit, 2 channels"),
            "{text}"
        );
        assert!(text.contains("insert-batch profile"), "{text}");
        assert!(text.contains("Title: [\"Format title\"]"), "{text}");
        assert!(text.contains("Duration: "), "{text}");
        assert!(text.contains("Track: 3/9; disc: "), "{text}");
        assert!(!text.contains("MPEG"), "{text}");
    }

    #[test]
    fn refusal_output_preserves_escaping_severity_and_decode_scope() {
        let report = SourceValidationReport {
            schema: "rustunes.validation",
            path: "odd\nname.m4a".into(),
            profile: "alac-source",
            supported_by_current_profile: false,
            audio: None,
            mp3: None,
            mp3_library_eligibility: None,
            tags: None,
            projected_tags: None,
            artwork: None,
            decode: SourceDecode::SkippedInvalidInput,
            issues: vec![
                SourceIssue {
                    severity: SourceIssueSeverity::Error,
                    code: "source_inspection_failed",
                    message: "unsupported \"codec\"\nretained".into(),
                },
                SourceIssue {
                    severity: SourceIssueSeverity::Warning,
                    code: "source_warning",
                    message: "inspect first".into(),
                },
            ],
        };
        let mut text = Vec::new();
        write_text(&report, &mut text).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            concat!(
                "File: \"odd\\nname.m4a\"\n",
                "Sync profile: alac-source\n",
                "Result: needs attention before this CLI can sync it\n",
                "Artwork: no supported embedded cover reported\n",
                "Audio decode: skipped because container validation failed\n",
                "ERROR source_inspection_failed: \"unsupported \\\"codec\\\"\\nretained\"\n",
                "WARN source_warning: \"inspect first\"\n",
                "Profile limits describe this CLI; they do not establish incompatibility with iOS.\n",
            )
        );
    }
}
