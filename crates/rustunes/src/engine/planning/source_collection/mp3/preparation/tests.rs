use super::*;
use music_protocol::protocol::mpeg::{Envelope, Gapless, Ids};
use std::{fs, path::PathBuf, time::UNIX_EPOCH};

mod pins;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mpeg-preparation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self, tags: &[(&str, &str)], audio: Vec<u8>) -> (PathBuf, Source) {
        let mut frames = Vec::new();
        for (id, value) in tags {
            frames.extend_from_slice(id.as_bytes());
            frames.extend_from_slice(&((value.len() + 1) as u32).to_be_bytes());
            frames.extend_from_slice(&[0, 0, 0]);
            frames.extend_from_slice(value.as_bytes());
        }
        let n = frames.len();
        let size = [
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ];
        let bytes = [b"ID3\x03\0\0".as_slice(), &size, &frames, &audio].concat();
        let path = self.0.join("source.mp3");
        fs::write(&path, bytes).unwrap();
        let source = music_protocol::media::mpeg::read(&path).unwrap();
        (path, source)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tags() -> Vec<(&'static str, &'static str)> {
    vec![
        ("TIT2", "Independent Song"),
        ("TPE1", "Source Artist"),
        ("TALB", "Source Album"),
        ("TSSE", "Local encoder"),
    ]
}
fn audio() -> Vec<u8> {
    music_protocol::test_support::mpeg::audio()
}
fn envelope() -> Envelope {
    Envelope {
        database_pid: 4444,
        primary_container_pid: 5555,
        before_anchor: 17,
        timestamp: UNIX_EPOCH,
    }
}
fn ids() -> Ids {
    Ids {
        track: u64::MAX - 1,
        album: 2222,
        artist: 3333,
    }
}

/// Apple's fields for `audio()`: two silent frames, so the search finds no
/// sound and the length is the first CD multiple below 2,304 - 528 - 529.
const SILENT: Gapless = Gapless {
    delay: 528,
    drain: 600,
    heuristic: 0x0200_0003,
    resynch: 0,
};

#[test]
fn an_mp3_goes_by_batch_with_apples_timing_and_no_authority() {
    let temp = Temp::new();
    let (path, source) = temp.source(&tags(), audio());
    let prepared = PreparedSource::read_expected(&path, &source).unwrap();
    let (metadata, timing) = prepared.coded_insert_fields().unwrap();
    assert_eq!(metadata.title, "Independent Song");
    assert_eq!(
        (timing.coded_samples, timing.total_time_ms_floor),
        (1176, 52)
    );
    assert_eq!(timing.gapless, Some(SILENT));
    assert_eq!(
        (timing.sample_rate, timing.channels, timing.bit_rate_kbps),
        (44100, 2, 128)
    );
    assert!(!timing.native_mapping_qualified);
    assert!(prepared.identity().audio.mpeg().is_some());
    assert_eq!(codes(&prepared), ["mp3_gapless_batch_only"]);
    prepared.ensure_batch_insertion_allowed().unwrap();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let bytes = prepared
        .operation_with_metadata(envelope(), ids(), &catalog)
        .unwrap();
    let wire = plist::Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
    let root = wire.as_dictionary().unwrap();
    assert_eq!(root["revision"].as_unsigned_integer(), Some(18));
    let operations = root["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 2);
    let track = operations[1].as_dictionary().unwrap();
    assert_eq!(track["pid"].as_signed_integer(), Some(-2));
    let item = track["item"].as_dictionary().unwrap();
    assert_eq!(item["title"].as_string(), Some("Independent Song"));
    assert_eq!(item["total_time_ms"].as_unsigned_integer(), Some(52));
    let av = track["avformat_info"].as_dictionary().unwrap();
    assert_eq!(av.len(), 9);
    assert_eq!(av["sample_rate"].as_real(), Some(44100.0));
    for (key, value) in [
        ("duration", 1176),
        ("audio_format", 301),
        ("gapless_encoding_delay", 528),
        ("gapless_encoding_drain", 600),
        ("gapless_heuristic_info", 0x0200_0003),
        ("gapless_last_frame_resynch", 0),
    ] {
        assert_eq!(av[key].as_unsigned_integer(), Some(value), "{key}");
    }
    assert_eq!(track["location"].as_dictionary().unwrap().len(), 1);
    assert!(!prepared.assessment().execution_authorized);
    assert_eq!(prepared.assessment().required_qualification.len(), 3);
}

#[test]
fn metadata_is_not_silently_truncated_normalized_or_relabelled() {
    for (id, value) in [
        ("TIT2", "second title"),
        ("TDRC", "24-05-01"),
        ("TCON", "(17)"),
        ("TRCK", "0/2"),
        ("TPOS", "3/2"),
    ] {
        let temp = Temp::new();
        let mut values = tags();
        values.push((id, value));
        let (path, source) = temp.source(&values, audio());
        let p = PreparedSource::read_expected(&path, &source).unwrap();
        assert!(p.assessment().metadata.is_none(), "{id}");
    }
    // An empty value is no value: a second, empty artist is dropped.
    let temp = Temp::new();
    let mut values = tags();
    values.push(("TPE1", ""));
    let (path, source) = temp.source(&values, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    let (plain_path, plain) = temp.source(&tags(), audio());
    let plain = PreparedSource::read_expected(&plain_path, &plain).unwrap();
    assert_eq!(p.assessment().metadata, plain.assessment().metadata);
    assert!(p.assessment().metadata.is_some());
    // A full date gives its year.
    for date in ["2024-05-01", "2024-05-01T07:00:00Z"] {
        let temp = Temp::new();
        let mut values = tags();
        values.push(("TDRC", date));
        let (path, source) = temp.source(&values, audio());
        let p = PreparedSource::read_expected(&path, &source).unwrap();
        assert_eq!(p.assessment().metadata.as_ref().unwrap().year, Some(2024));
    }
}

#[test]
fn an_mp3_with_two_composers_is_refused_with_the_tag_to_edit() {
    // Apple's readers take one composer; only a FLAC's are joined.
    let temp = Temp::new();
    let mut values = tags();
    values.extend([("TCOM", "Artist B"), ("TCOM", "Artist C")]);
    let (path, source) = temp.source(&values, audio());
    assert_eq!(source.metadata.composer, ["Artist B", "Artist C"]);
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert!(p.assessment().metadata.is_none());
    assert_eq!(
        p.ensure_batch_insertion_allowed().unwrap_err().to_string(),
        "MPEG-1 Layer III source is outside the insert-batch profile: \
         composer has 2 values in the file's tag; edit the tag to keep one"
    );
}

#[test]
fn missing_title_and_unknown_frame_are_actionable_blockers() {
    let temp = Temp::new();
    let (_, source) = temp.source(&[("TPE1", "Artist"), ("TALB", "Album")], audio());
    assert!(assess(&source).metadata.is_none());
    let mut values = tags();
    values.push(("TIT3", "a subtitle"));
    let (path, source) = temp.source(&values, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert!(
        p.assessment()
            .compatibility_issues
            .iter()
            .any(|i| i.code == "mp3_unmapped_metadata_frames")
    );
}

#[test]
fn sort_frames_are_projected_and_go_by_batch() {
    let temp = Temp::new();
    let mut values = tags();
    values.extend([
        ("TSOT", "Song, Independent"),
        ("TSOP", "Artist, Source"),
        ("TSOA", "Album, Source"),
        ("TPE2", "The Album Artist"),
        ("TSO2", ""),
        ("TCOM", "A Composer"),
        ("TSOC", "Composer, A"),
    ]);
    let (path, source) = temp.source(&values, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert_eq!(
        codes(&p),
        [
            "mp3_extended_metadata_mapping_required",
            "mp3_gapless_batch_only"
        ]
    );
    p.ensure_batch_insertion_allowed().unwrap();
    let m = p.assessment().metadata.as_ref().unwrap();
    assert_eq!(
        (
            m.sort_title.as_deref(),
            m.sort_artist.as_deref(),
            m.sort_album.as_deref(),
            m.sort_album_artist.as_deref(),
            m.sort_composer.as_deref(),
        ),
        (
            Some("Song, Independent"),
            Some("Artist, Source"),
            Some("Album, Source"),
            None,
            Some("Composer, A")
        )
    );
    let expected = p.expected().unwrap();
    let tags = expected.projected().unwrap();
    // An empty sort frame leaves the default: the name without its article.
    assert_eq!(tags.album_artist_sort(), Some("Album Artist"));
    assert_eq!(p.projected().unwrap().sort_name, "Song, Independent");
}

#[test]
fn richer_tags_remain_projected_but_cannot_be_dropped_by_basic_serializer() {
    let temp = Temp::new();
    let mut values = tags();
    values.extend([
        ("TPE2", "Album Artist"),
        ("TCOM", "Composer"),
        ("TCON", "Rock"),
        ("TCOP", "Copyright"),
        ("TYER", "2026"),
        ("TRCK", "2/3"),
        ("TPOS", "1/2"),
        ("TCMP", "1"),
    ]);
    let (path, source) = temp.source(&values, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    let metadata = p.assessment().metadata.as_ref().unwrap();
    assert_eq!(
        (
            metadata.year,
            metadata.track,
            metadata.track_total,
            metadata.disc,
            metadata.disc_total
        ),
        (Some(2026), Some(2), Some(3), Some(1), Some(2))
    );
    assert_eq!(metadata.compilation, Some(true));
    assert_eq!(metadata.composer.as_deref(), Some("Composer"));
}

#[test]
fn an_info_or_xing_header_is_read_by_apples_rule() {
    let temp = Temp::new();
    let mut frames = audio();
    frames[36..40].copy_from_slice(b"Info");
    let (path, source) = temp.source(&tags(), frames);
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    // With no frame count the byte estimate counts both frames, the Info
    // frame too; without a LAME tag the search gives the trim.
    let timing = p.assessment().coded_timing_proposal.as_ref().unwrap();
    assert_eq!(
        (
            timing.coded_samples,
            timing.total_time_ms_floor,
            &timing.gapless
        ),
        (1176, 52, &Some(SILENT))
    );
    assert!(!timing.variable_bit_rate);
    assert_eq!(codes(&p), ["mp3_gapless_batch_only"]);
    p.ensure_batch_insertion_allowed().unwrap();
    // A Xing header marks the rate variable: it is the average.
    let mut frames = audio();
    frames[36..40].copy_from_slice(b"Xing");
    let (path, source) = temp.source(&tags(), frames);
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    let timing = p.assessment().coded_timing_proposal.as_ref().unwrap();
    assert_eq!((timing.coded_samples, timing.bit_rate_kbps), (1176, 128));
    assert!(timing.variable_bit_rate);
    assert_eq!(codes(&p), ["mp3_gapless_batch_only"]);
    p.ensure_batch_insertion_allowed().unwrap();
}

#[test]
fn a_vbri_header_goes_by_batch_with_its_frame_count() {
    let temp = Temp::new();
    let mut frames = audio();
    let bytes = frames.len() as u32;
    frames[36..40].copy_from_slice(b"VBRI");
    frames[40..42].copy_from_slice(&1u16.to_be_bytes()); // version
    frames[46..50].copy_from_slice(&bytes.to_be_bytes());
    frames[50..54].copy_from_slice(&2u32.to_be_bytes()); // frames
    frames[54..56].copy_from_slice(&1u16.to_be_bytes()); // one table entry
    frames[56..58].copy_from_slice(&1u16.to_be_bytes()); // scale
    frames[58..60].copy_from_slice(&2u16.to_be_bytes()); // bytes per entry
    frames[60..62].copy_from_slice(&2u16.to_be_bytes()); // frames per entry
    frames[62..64].copy_from_slice(&(bytes as u16).to_be_bytes());
    let (path, source) = temp.source(&tags(), frames);
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    // Its count equals the byte estimate, which counts the VBRI frame.
    let timing = p.assessment().coded_timing_proposal.as_ref().unwrap();
    assert_eq!(
        (
            timing.coded_samples,
            timing.total_time_ms_floor,
            timing.bit_rate_kbps
        ),
        (1176, 52, 128)
    );
    assert_eq!(codes(&p), ["mp3_gapless_batch_only"]);
    p.ensure_batch_insertion_allowed().unwrap();
}

fn codes(p: &PreparedSource) -> Vec<&'static str> {
    p.assessment()
        .compatibility_issues
        .iter()
        .map(|i| i.code)
        .collect()
}

#[test]
fn an_empty_comment_frame_needs_no_policy_and_a_written_one_is_sent_by_batch() {
    let temp = Temp::new();
    let mut with = tags();
    with.push(("COMM", "eng\0"));
    let (path, source) = temp.source(&with, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert_eq!(codes(&p), ["mp3_gapless_batch_only"]);
    p.ensure_batch_insertion_allowed().unwrap();
    let mut with = tags();
    with.push(("COMM", "eng\0a comment\non two lines"));
    let (path, source) = temp.source(&with, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    // A batch sends the comment.
    assert_eq!(
        codes(&p),
        [
            "mp3_extended_metadata_mapping_required",
            "mp3_gapless_batch_only"
        ]
    );
    p.ensure_batch_insertion_allowed().unwrap();
    assert_eq!(
        p.projected().unwrap().comment.as_deref(),
        Some("a comment\non two lines")
    );
    // A described comment (iTunNORM and the like) is not the catalogue's; it
    // stays in the file.
    let mut with = tags();
    with.push(("COMM", "engiTunNORM\0 00000A1B"));
    let (path, source) = temp.source(&with, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert_eq!(
        codes(&p),
        ["mp3_unmapped_metadata_frames", "mp3_gapless_batch_only"]
    );
    p.ensure_batch_insertion_allowed().unwrap();
    assert_eq!(p.projected().unwrap().comment, None);
}

#[test]
fn an_insert_batch_expects_the_projected_tags_and_coded_display_time() {
    let temp = Temp::new();
    let mut with = tags();
    with.extend([("TPE2", "Album Artist"), ("TRCK", "3/9")]);
    let (path, source) = temp.source(&with, audio());
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert_eq!(
        codes(&p),
        [
            "mp3_extended_metadata_mapping_required",
            "mp3_gapless_batch_only"
        ]
    );
    p.ensure_batch_insertion_allowed().unwrap();
    let expected = p.expected().unwrap();
    let tags = expected.projected().unwrap();
    assert_eq!(
        (tags.title, tags.artist, tags.album, tags.album_artist),
        (
            "Independent Song",
            "Source Artist",
            "Source Album",
            Some("Album Artist")
        )
    );
    assert_eq!(
        (tags.track, tags.track_total, tags.content_rating),
        (Some(3), Some(9), None)
    );
    assert_eq!(expected.total_time_ms().unwrap(), 52);
}

#[test]
fn variable_bitrate_takes_the_first_rate_and_a_trailing_tag_stays_unprojected() {
    let temp = Temp::new();
    let mut frames = audio();
    frames.truncate(417);
    let mut higher = vec![0; 522];
    higher[..4].copy_from_slice(&[0xff, 0xfb, 0xa0, 0]);
    frames.extend(higher);
    let (_, source) = temp.source(&tags(), frames);
    // Without a Xing or VBRI header Apple's rule takes the first frame's
    // rate; 939 bytes are no whole number of its frames, so both count.
    let timing = assess(&source).coded_timing_proposal.unwrap();
    assert!(timing.variable_bit_rate);
    assert_eq!(
        (
            timing.bit_rate_kbps,
            timing.coded_samples,
            timing.total_time_ms_floor
        ),
        (128, 1176, 52)
    );
    let mut frames = audio();
    let mut trailer = vec![0; 128];
    trailer[..3].copy_from_slice(b"TAG");
    frames.extend(trailer);
    let (path, source) = temp.source(&tags(), frames);
    let p = PreparedSource::read_expected(&path, &source).unwrap();
    assert!(
        p.assessment()
            .compatibility_issues
            .iter()
            .any(|i| i.code == "mp3_trailing_tag_not_projected")
    );
    p.ensure_batch_insertion_allowed().unwrap();
}

#[test]
fn source_drift_or_forged_inspection_cannot_prepare() {
    let temp = Temp::new();
    let (path, source) = temp.source(&tags(), audio());
    let mut forged = source.clone();
    forged
        .timing
        .as_mut()
        .unwrap()
        .physical
        .coded_duration
        .samples = 1152;
    assert!(PreparedSource::read_expected(&path, &forged).is_err());
    forged = source.clone();
    forged.metadata.title = vec!["Forged title".into()];
    assert!(PreparedSource::read_expected(&path, &forged).is_err());
    let mut bytes = fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() = 1;
    fs::write(&path, bytes).unwrap();
    assert!(PreparedSource::read_expected(&path, &source).is_err());
    let alias = temp.0.join("alias.mp3");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert!(PreparedSource::read_expected(&alias, &source).is_err());
}

#[test]
fn pure_serializer_rejects_bad_format_ids_text_anchor_and_duration() {
    use music_protocol::protocol::mpeg::{Timing, TrackFields, serialize};
    let make = || TrackFields {
        title: "Title",
        sort_title: "Title",
        artist: "Artist",
        sort_artist: "Artist",
        album: "Album",
        sort_album: "Album",
        bit_rate_kbps: 128,
        sample_rate: 44100,
        channels: 2,
        timing: Timing::Omitted,
    };
    let mut fields = make();
    fields.sample_rate = 0;
    assert!(serialize(envelope(), ids(), fields).is_err());
    let mut fields = make();
    fields.bit_rate_kbps = 1411;
    assert!(serialize(envelope(), ids(), fields).is_err());
    let mut fields = make();
    fields.channels = 3;
    assert!(serialize(envelope(), ids(), fields).is_err());
    let mut fields = make();
    fields.title = "";
    assert!(serialize(envelope(), ids(), fields).is_err());
    let mut fields = make();
    fields.timing = Timing::Coded {
        samples: 2304,
        total_time_ms: 53,
    };
    assert!(serialize(envelope(), ids(), fields).is_err());
    let mut e = envelope();
    e.before_anchor = u32::MAX;
    assert!(serialize(e, ids(), make()).is_err());
    let mut i = ids();
    i.track = i.album;
    assert!(serialize(envelope(), i, make()).is_err());
    let mut e = envelope();
    e.database_pid = 0;
    assert!(serialize(e, ids(), make()).is_err());
}

#[test]
fn complete_over_transfer_cap_source_is_inspectable_but_cannot_prepare() {
    let temp = Temp::new();
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    let frames = (music_protocol::protocol::transfer::MAX_AUDIO / 417 + 1) as usize;
    let (path, source) = temp.source(&tags(), frame.repeat(frames));
    assert!(source.fingerprint.file_bytes > music_protocol::protocol::transfer::MAX_AUDIO);
    let assessment = assess(&source);
    assert!(
        assessment
            .compatibility_issues
            .iter()
            .any(|i| i.code == "mp3_native_transfer_size_limit")
    );
    let refusal = PreparedSource::read_expected(&path, &source).err().unwrap();
    assert!(refusal.to_string().contains("64 MiB"));
    assert!(!assessment.execution_authorized);
}

#[test]
fn ordinary_metadata_builder_preserves_rich_scalars_without_opening_execution() {
    let temp = Temp::new();
    let mut values = tags();
    values.extend([
        ("TPE2", "Ensemble"),
        ("TCMP", "1"),
        ("TCON", "Jazz"),
        ("TCOM", "New Composer"),
        ("TCOP", "Copyright Owner"),
        ("TYER", "2026"),
        ("TRCK", "2/3"),
        ("TPOS", "1/2"),
    ]);
    let (path, source) = temp.source(&values, audio());
    let prepared = PreparedSource::read_expected(&path, &source).unwrap();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let operation = prepared
        .operation_with_metadata(envelope(), ids(), &catalog)
        .unwrap();
    let wire = plist::Value::from_reader(std::io::Cursor::new(operation)).unwrap();
    let item = wire.as_dictionary().unwrap()["operations"]
        .as_array()
        .unwrap()[1]
        .as_dictionary()
        .unwrap()["item"]
        .as_dictionary()
        .unwrap();
    for (k, v) in [
        ("album_artist", "Ensemble"),
        ("sort_album_artist", "Ensemble"),
        ("genre", "Jazz"),
        ("composer", "New Composer"),
        ("sort_composer", "New Composer"),
        ("copyright", "Copyright Owner"),
    ] {
        assert_eq!(item[k].as_string(), Some(v));
    }
    for (k, v) in [
        ("year", 2026),
        ("track_number", 2),
        ("track_count", 3),
        ("disc_number", 1),
        ("disc_count", 2),
    ] {
        assert_eq!(item[k].as_unsigned_integer(), Some(v));
    }
    assert_eq!(item["is_compilation"].as_boolean(), Some(true));
    assert_eq!(item["artwork_checksum"].as_unsigned_integer(), Some(0));
}

#[test]
fn every_other_format_goes_by_batch_with_its_own_timing_and_tags() {
    use music_protocol::media::format::Format;
    for (name, format, bits) in [
        ("tone-aac.m4a", Format::AacLc, 0),
        ("tone-flac.flac", Format::Flac, 16),
        ("tone-wav.wav", Format::Wav, 16),
        ("tone-aiff.aif", Format::Aiff, 24),
        ("tone-mpeg2.mp3", Format::Mpeg2Layer3, 0),
    ] {
        let path = crate::test_fixtures::workspace_root()
            .join("tests/fixtures")
            .join(name);
        let source = music_protocol::media::mpeg::read(&path).unwrap();
        let p = PreparedSource::read_expected(&path, &source).unwrap();
        assert_eq!(
            codes(&p),
            [
                "mp3_other_format_batch_only",
                "mp3_extended_metadata_mapping_required"
            ],
            "{name}: {:?}",
            p.assessment().compatibility_issues
        );
        p.ensure_batch_insertion_allowed().unwrap();
        let timing = p.assessment().coded_timing_proposal.as_ref().unwrap();
        assert_eq!(
            (timing.format, timing.bits_per_sample),
            (format, bits),
            "{name}"
        );
        match format {
            // FFmpeg's AAC primes 1,024 samples; the insert sends the rest.
            Format::AacLc => {
                assert_eq!(timing.coded_samples, 4410);
                assert_eq!(timing.gapless.as_ref().unwrap().delay, 1024);
            }
            // Apple's rule, as for MPEG-1: 576 samples a frame.
            Format::Mpeg2Layer3 => {
                let gapless = timing.gapless.as_ref().unwrap();
                assert_eq!(gapless.heuristic, 0x0200_0003);
                assert_eq!(
                    (timing.coded_samples + gapless.delay + gapless.drain) % 576,
                    0
                );
            }
            _ => {
                assert_eq!(timing.coded_samples, 4410);
                assert_eq!(timing.gapless, Some(Gapless::NONE));
            }
        }
        let fields = p.projected().unwrap();
        assert_eq!(
            (
                fields.title.as_str(),
                fields.artist.as_str(),
                fields.album.as_str()
            ),
            ("Format title", "Format artist", "Format album"),
            "{name}"
        );
        assert_eq!((fields.year, fields.track), (Some(2024), Some(3)), "{name}");
        assert_eq!(fields.genre.as_deref(), Some("Test"), "{name}");
        assert_eq!(fields.bit_depth, bits, "{name}");
    }
}
