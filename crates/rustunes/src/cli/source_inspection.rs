//! Human-readable presentation of what the non-ALAC reader found in a source.
use music_protocol::media::mpeg::Source;
use std::io;

pub(super) fn write_text(source: &Source, output: &mut impl io::Write) -> io::Result<()> {
    let audio = &source.fingerprint.audio;
    writeln!(output, "Codec: {}", source.format().label())?;
    writeln!(
        output,
        "File: {} bytes; SHA-256 {}",
        source.fingerprint.file_bytes, source.fingerprint.full_file_sha256
    )?;
    writeln!(
        output,
        "Encoded audio identity ({}): {}",
        audio.format, audio.sha256
    )?;
    writeln!(output, "Sample rate (Hz): {}", audio.sample_rate)?;
    writeln!(output, "Channels: {}", audio.channels)?;
    match &source.stream {
        Some(stream) => {
            if stream.bits_per_sample > 0 {
                writeln!(output, "Bit depth: {}", stream.bits_per_sample)?;
            }
            writeln!(
                output,
                "Bit rate (kb/s): {}{}",
                stream.bit_rate_kbps,
                if stream.variable_bit_rate {
                    " (variable)"
                } else {
                    ""
                }
            )?;
            writeln!(
                output,
                "Encoder delay: {}; padding: {}; played samples: {}",
                stream.encoder_delay, stream.encoder_padding, stream.valid_samples
            )?;
        }
        None => writeln!(
            output,
            "MPEG frames: {}; coded samples: {}",
            audio.frame_count, audio.coded_samples
        )?,
    }
    if let Some(timing) = &source.apple_timing {
        writeln!(
            output,
            "Apple timing: {} frames, {} ms, {} kb/s, {} played samples, delay {}, drain {}, resynch {}, heuristic {:#010x} (from {})",
            timing.frames,
            timing.total_time_ms,
            timing.bit_rate_kbps,
            timing.duration,
            timing.delay,
            timing.drain,
            timing.resynch,
            timing.heuristic,
            timing.source
        )?;
    }
    match source.id3_version {
        Some(version) => writeln!(output, "Tag store: ID3v2.{version}")?,
        None => writeln!(output, "Tag store: the container's own")?,
    }
    let tags = &source.metadata;
    for (name, values) in [
        ("Title", &tags.title),
        ("Artist", &tags.artist),
        ("Album", &tags.album),
        ("Album artist", &tags.album_artist),
        ("Genre", &tags.genre),
        ("Composer", &tags.composer),
        ("Copyright", &tags.copyright),
        ("Release date", &tags.release_date),
        ("Sort title", &tags.sort_title),
        ("Sort artist", &tags.sort_artist),
        ("Sort album", &tags.sort_album),
        ("Sort album artist", &tags.sort_album_artist),
        ("Sort composer", &tags.sort_composer),
    ] {
        if values.is_empty() {
            writeln!(output, "{name}: (absent)")?;
        }
        for value in values {
            // Escape embedded controls in untrusted local tags.
            writeln!(output, "{name}: {value:?}")?;
        }
    }
    if tags.comments.is_empty() {
        writeln!(output, "Comment: (absent)")?;
    }
    for comment in &tags.comments {
        writeln!(
            output,
            "Comment: {:?} (language {:?}, description {:?}{})",
            comment.text,
            comment.language,
            comment.description,
            if comment.description.is_empty() {
                ""
            } else {
                "; not the catalogue comment"
            }
        )?;
    }
    match tags.compilation {
        Some(value) => writeln!(output, "Compilation: {value}")?,
        None => writeln!(output, "Compilation: (absent)")?,
    }
    for (name, value) in [
        ("Track", tags.track),
        ("Track total", tags.track_total),
        ("Disc", tags.disc),
        ("Disc total", tags.disc_total),
    ] {
        match value {
            Some(value) => writeln!(output, "{name}: {value}")?,
            None => writeln!(output, "{name}: (absent)")?,
        }
    }
    let hints = &tags.encoder_hints;
    if !hints.is_empty() {
        writeln!(
            output,
            "Encoder hints: {:?}; CD rip: {}{}",
            hints.text,
            hints.cd_rip,
            hints
                .itunsmpb
                .as_ref()
                .map(|text| format!("; iTunSMPB {text:?}"))
                .unwrap_or_default()
        )?;
    }
    if !source.frames.is_empty() {
        let frames: Vec<_> = source
            .frames
            .iter()
            .map(|frame| format!("{} ({} bytes)", frame.id, frame.bytes))
            .collect();
        writeln!(output, "Tag frames: {}", frames.join(", "))?;
    }
    if source.artwork.is_empty() {
        writeln!(output, "Embedded pictures: none")?;
    }
    let cover = music_protocol::media::mpeg::cover_index(&source.artwork);
    for (index, picture) in source.artwork.iter().enumerate() {
        writeln!(
            output,
            "Picture: {}, type {}{}{}, {}x{}, {} bytes; SHA-256 {}; description {:?}",
            picture.mime,
            picture.picture_type,
            if picture.picture_type == 3 {
                " (front cover)"
            } else {
                ""
            },
            if cover == Some(index) {
                ", sent as the cover"
            } else {
                ""
            },
            picture.width,
            picture.height,
            picture.bytes,
            picture.sha256,
            picture.description
        )?;
    }
    if source.trailing_tag_bytes > 0 {
        writeln!(
            output,
            "Trailing tag bytes: {} (kept in the file, not read)",
            source.trailing_tag_bytes
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(name: &str) -> String {
        let path = crate::test_fixtures::workspace_root()
            .join("tests/fixtures")
            .join(name);
        let source = music_protocol::media::mpeg::read(&path).unwrap();
        let mut text = Vec::new();
        write_text(&source, &mut text).unwrap();
        String::from_utf8(text).unwrap()
    }

    #[test]
    fn every_other_format_reads_as_text() {
        let flac = text("tone-flac.flac");
        assert!(flac.starts_with("Codec: FLAC\n"), "{flac}");
        assert!(flac.contains("Title: \"Format title\"\n"), "{flac}");
        assert!(flac.contains("Track: 3\nTrack total: 9\n"), "{flac}");
        assert!(flac.contains("Bit depth: 16\n"), "{flac}");
        let mp3 = text("tone-mp3-vbr.mp3");
        assert!(mp3.contains("Apple timing: "), "{mp3}");
        assert!(mp3.contains("Tag store: ID3v2."), "{mp3}");
    }
}
