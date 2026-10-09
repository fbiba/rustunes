use super::*;
use std::path::PathBuf;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "validation-mpeg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self, extra: Option<(&str, &[u8])>, info: bool) -> PathBuf {
        let mut tags = Vec::new();
        for (id, data) in [
            ("TIT2", b"\0Validation title".as_slice()),
            ("TPE1", b"\0Validation artist".as_slice()),
            ("TALB", b"\0Validation album".as_slice()),
        ]
        .into_iter()
        .chain(extra)
        {
            tags.extend(id.as_bytes());
            tags.extend((data.len() as u32).to_be_bytes());
            tags.extend([0, 0]);
            tags.extend(data);
        }
        let n = tags.len();
        let mut bytes = b"ID3\x03\0\0".to_vec();
        bytes.extend([
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ]);
        bytes.extend(tags);
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let mut first = frame.clone();
        if info {
            first[36..40].copy_from_slice(b"Info");
            first[40..44].copy_from_slice(&1u32.to_be_bytes());
            first[44..48].copy_from_slice(&1u32.to_be_bytes());
        }
        bytes.extend(first);
        bytes.extend(frame);
        let path = self.0.join("source.mp3");
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// Shared synthetic input for engine facts and CLI rendering tests only.
pub(crate) fn plain_report() -> Report {
    let t = Temp::new();
    inspect(&t.source(None, false), false)
}

#[test]
fn a_plain_mp3_fits_an_insert_batch_without_library_authority() {
    let report = plain_report();
    assert!(report.supported_by_current_profile);
    // The MPEG facts are under `mp3`; the tags are listed as for ALAC.
    assert!(report.audio.is_none());
    assert_eq!(
        report.tags.as_ref().unwrap().title,
        report.mp3.as_ref().unwrap().metadata.title
    );
    assert!(report.mp3.is_some());
    // Its gapless fields go only in an insert batch.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["mp3_library_eligibility"], "not_checked");
    assert_eq!(json["decode"], "not_requested");
    assert_insert_batch_only(&report);
}

/// Rich tags and an Info header fit only an insert batch.
fn assert_insert_batch_only(report: &Report) {
    assert!(report.supported_by_current_profile);
    assert_eq!(report.profile, "mpeg1-layer3-insert-batch-source");
    assert!(report.issues.iter().all(|i| i.severity != Severity::Error));
    assert!(report.issues.iter().any(|i| {
        i.code == "mp3_library_eligibility_not_checked" && i.message.contains("insert batch")
    }));
}

#[test]
fn rich_and_header_sources_fit_only_an_insert_batch() {
    let t = Temp::new();
    for (extra, info) in [(Some(("TCON", b"\0Rock".as_slice())), false), (None, true)] {
        let path = t.source(extra, info);
        let report = inspect(&path, false);
        assert!(report.mp3.is_some());
        assert_insert_batch_only(&report);
    }
    // A comment that says something stays in the file, sent by a batch.
    let path = t.source(Some(("COMM", b"\0eng\0a comment".as_slice())), false);
    let report = inspect(&path, false);
    assert_insert_batch_only(&report);
}

#[test]
fn comments_and_sort_names_are_reported_as_an_insert_sends_them() {
    let t = Temp::new();
    let path = t.source(Some(("COMM", b"\0eng\0a comment")), false);
    let report = inspect(&path, false);
    assert_eq!(report.tags.as_ref().unwrap().comment, ["a comment"]);
    let projected = report.projected_tags.as_ref().unwrap();
    assert_eq!(projected.comment.as_deref(), Some("a comment"));
    // Without a sort tag, the name itself.
    assert_eq!(projected.sort_title, "Validation title");
    let path = t.source(Some(("TSOP", b"\0Artist, Validation")), false);
    let report = inspect(&path, false);
    assert_eq!(
        report.tags.as_ref().unwrap().sort_artist,
        ["Artist, Validation"]
    );
    let projected = report.projected_tags.as_ref().unwrap();
    assert_eq!(projected.sort_artist, "Artist, Validation");
    assert_eq!(projected.sort_album, "Validation album");
    assert!(projected.sort_album_artist.is_none() && projected.comment.is_none());
}

#[test]
fn malformed_mp3_remains_a_parser_refusal() {
    let t = Temp::new();
    let path = t.0.join("broken.mp3");
    fs::write(&path, b"not MPEG").unwrap();
    let report = inspect(&path, true);
    assert!(!report.supported_by_current_profile);
    assert!(report.mp3.is_none());
    assert!(report.decode == Decode::SkippedInvalidInput);
    assert_eq!(report.issues[0].code, "source_inspection_failed");
}

#[test]
fn an_embedded_front_cover_fits_only_an_insert_batch() {
    let t = Temp::new();
    let jpeg = [
        0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 0, 2, 0, 3, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 0xff,
        0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0, 0, 0xff, 0xd9,
    ];
    let apic = [b"\0image/jpeg\0\x03\0".as_slice(), &jpeg].concat();
    let path = t.source(Some(("APIC", &apic)), false);
    let report = inspect(&path, false);
    assert_eq!(report.mp3.as_ref().unwrap().artwork.len(), 1);
    // The checksum an insert batch sends: the image plus the ID3 picture's one byte.
    assert_eq!(
        report.artwork.as_ref().unwrap().native_checksum,
        jpeg.len() as u32 + 1
    );
    assert_insert_batch_only(&report);
}

#[test]
fn structurally_valid_other_rate_fits_only_an_insert_batch() {
    let t = Temp::new();
    let path = t.source(None, false);
    let source = music_protocol::media::mpeg::read(&path).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes.truncate(source.audio_offset as usize);
    let mut frame = vec![0; 384]; // MPEG-1, 128 kb/s, 48 kHz.
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x94, 0]);
    bytes.extend(&frame);
    bytes.extend(&frame);
    fs::write(&path, bytes).unwrap();
    let report = inspect(&path, false);
    assert_eq!(
        report.mp3.as_ref().unwrap().fingerprint.audio.sample_rate,
        48000
    );
    assert_insert_batch_only(&report);
}

#[test]
fn alac_report_omits_mpeg_eligibility_extension() {
    let report = inspect(
        &crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a"),
        false,
    );
    assert!(report.supported_by_current_profile);
    let json = serde_json::to_value(&report).unwrap();
    assert!(json.get("mp3").is_none());
    assert!(json.get("mp3_library_eligibility").is_none());
}
