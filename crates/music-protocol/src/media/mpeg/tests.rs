use super::*;
use std::path::PathBuf;
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mp3-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn read(&self, bytes: &[u8]) -> Result<Source> {
        let path = self.0.join("test.mp3");
        fs::write(&path, bytes)?;
        read(&path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub(crate) use super::test_fixture::audio;
fn safe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 127) as u8,
        ((n >> 14) & 127) as u8,
        ((n >> 7) & 127) as u8,
        (n & 127) as u8,
    ]
}
fn frame(id: &str, data: &[u8], v: u8) -> Vec<u8> {
    [
        id.as_bytes(),
        if v == 4 {
            safe(data.len())
        } else {
            (data.len() as u32).to_be_bytes()
        }
        .as_slice(),
        &[0, 0],
        data,
    ]
    .concat()
}
fn tagged(frames: Vec<Vec<u8>>, v: u8) -> Vec<u8> {
    let frames = frames.concat();
    [
        b"ID3".as_slice(),
        &[v, 0, 0],
        &safe(frames.len()),
        frames.as_slice(),
        audio().as_slice(),
    ]
    .concat()
}
#[test]
fn real_frame_bytes_identify_tag_variants_and_distinguish_audio_changes() {
    let t = Temp::new();
    let a = tagged(vec![frame("TIT2", b"\x03Track", 4)], 4);
    let b = tagged(vec![frame("TIT2", b"\x03TRACK\0alternate", 4)], 4);
    let x = t.read(&a).unwrap();
    let y = t.read(&b).unwrap();
    assert_ne!(
        x.fingerprint.full_file_sha256,
        y.fingerprint.full_file_sha256
    );
    assert_eq!(x.fingerprint.audio, y.fingerprint.audio);
    assert_eq!(y.metadata.title, ["TRACK", "alternate"]);
    let mut c = b;
    c.last_mut().map(|b| *b = 1);
    assert_ne!(t.read(&c).unwrap().fingerprint.audio, x.fingerprint.audio);
    assert_eq!(x.fingerprint.audio.coded_samples, 2304);
    assert_eq!(
        x.fingerprint.audio.sha256,
        "0e9ea8a196febd7d8097c3eff0ef4f4bce3f681ebadb0851b390d8117494adba"
    );
    assert_eq!(
        x.fingerprint.audio.frame_bytes_sha256,
        "ade453b6d5fc720acc24dd0602d5b5e086f56cb7c50190f4641a76d14014ce3a"
    );
    assert_eq!(x.timing, y.timing);
    assert_eq!(
        x.frame_timing().unwrap().physical.coded_duration.samples,
        2304
    );
    // Descriptive timing is not part of either encoded identity or the
    // transferred-source value that journal Content serializes.
    let original = x.transferred_identity().unwrap();
    let mut altered_description = x;
    altered_description
        .timing
        .as_mut()
        .unwrap()
        .physical
        .coded_duration
        .samples = 1;
    assert!(altered_description.transferred_identity().unwrap() == original);
}
#[test]
fn id3_versions_unicode_and_numeric_arrays_are_real_not_mp4_fields() {
    let t = Temp::new();
    let bytes = tagged(
        vec![
            frame("TIT2", &[1, 0xff, 0xfe, b'E', 0, 0xac, 0x20], 3),
            frame("TRCK", b"\x001/2", 3),
            frame("TPOS", b"\x001/1", 3),
        ],
        3,
    );
    let s = t.read(&bytes).unwrap();
    assert_eq!(s.metadata.title, ["E€"]);
    assert_eq!(
        (s.metadata.track, s.metadata.track_total),
        (Some(1), Some(2))
    );
    assert!(serde_json::to_value(s).unwrap().get("bit_depth").is_none());
}
#[test]
fn an_empty_comment_says_nothing_and_a_written_one_is_kept() {
    let t = Temp::new();
    let empty = |data: &[u8], v| t.read(&tagged(vec![frame("COMM", data, v)], v)).unwrap();
    // Latin-1, UTF-8 with a text terminator, and UTF-16 with a BOM-less empty
    // description: none of them says anything.
    for (data, v) in [
        (b"\0eng\0".as_slice(), 4),
        (b"\x03eng\0\0".as_slice(), 4),
        (b"\x01eng\0\0".as_slice(), 3),
    ] {
        let s = empty(data, v);
        assert!(s.metadata.comments.is_empty(), "{data:?}");
        assert_eq!(s.frames[0].id, "COMM");
        assert!(
            serde_json::to_value(&s.metadata)
                .unwrap()
                .get("comments")
                .is_none()
        );
    }
    let s = empty(b"\0engiTunNORM\0 0000", 4);
    assert_eq!(
        s.metadata.comments,
        [Comment {
            language: "eng".into(),
            description: "iTunNORM".into(),
            text: " 0000".into(),
        }]
    );
    let s = empty(
        &[
            1, b'e', b'n', b'g', 0xff, 0xfe, 0, 0, 0xff, 0xfe, b'h', 0, b'i', 0,
        ],
        3,
    );
    assert_eq!(s.metadata.comments[0].text, "hi");
    for bad in [b"\0en".as_slice(), b"\0e1g\0", b"\0engno terminator"] {
        assert!(t.read(&tagged(vec![frame("COMM", bad, 4)], 4)).is_err());
    }
}
#[test]
fn an_empty_numeric_frame_is_no_number() {
    let t = Temp::new();
    let title = || frame("TIT2", b"\x03Title", 4);
    let plain = t.read(&tagged(vec![title()], 4)).unwrap();
    // UTF-8 with no text, and Latin-1 with a bare terminator.
    for data in [b"\x03".as_slice(), b"\x00\x00"] {
        let s = t
            .read(&tagged(
                vec![
                    title(),
                    frame("TRCK", data, 4),
                    frame("TPOS", data, 4),
                    frame("TCMP", data, 4),
                ],
                4,
            ))
            .unwrap();
        assert_eq!(s.metadata, plain.metadata, "{data:?}");
    }
    // An empty value beside a number drops out; two numbers stay ambiguous.
    let s = t.read(&tagged(vec![frame("TRCK", b"\x03\x005", 4)], 4));
    assert_eq!(s.unwrap().metadata.track, Some(5));
    let two = t.read(&tagged(vec![frame("TRCK", b"\x035\x006", 4)], 4));
    assert_eq!(two.unwrap_err().to_string(), "ambiguous ID3 numeric frame");
}
#[test]
fn malformed_flags_extents_utf16_duplicates_and_frames_refuse() {
    let t = Temp::new();
    let mut flags = tagged(vec![], 4);
    flags[5] = 0x80;
    let mut broken = audio();
    broken.pop();
    let duplicate = tagged(
        vec![frame("TRCK", b"\x001", 4), frame("TRCK", b"\x002", 4)],
        4,
    );
    let utf = tagged(vec![frame("TIT2", &[1, 0xff, 0xfe, 0], 4)], 4);
    let mut extent = tagged(vec![frame("TIT2", b"\x03A", 4)], 4);
    extent[17] = 127;
    for bytes in [flags, broken, duplicate, utf, extent, vec![0; 834]] {
        assert!(t.read(&bytes).is_err());
    }
}
#[test]
fn apic_validates_exact_jpeg_and_has_no_native_checksum() {
    let t = Temp::new();
    let jpeg = [
        0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 0, 2, 0, 3, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 0xff,
        0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0, 0, 0xff, 0xd9,
    ];
    let apic = [b"\x00image/jpeg\x00\x03\x00".as_slice(), &jpeg].concat();
    let s = t.read(&tagged(vec![frame("APIC", &apic, 4)], 4)).unwrap();
    let p = &s.artwork[0];
    assert_eq!((p.width, p.height), (3, 2));
    assert_eq!(p.sha256, hex(&Sha256::digest(jpeg)));
    assert!(!p.image_decode_verified);
    assert!(
        serde_json::to_value(p)
            .unwrap()
            .get("native_checksum")
            .is_none()
    );
    let captured = read_front_cover(&t.0.join("test.mp3"), &s)
        .unwrap()
        .unwrap();
    assert_eq!(captured.bytes(), jpeg);
    assert_eq!(captured.facts(), p);
    let duplicated = t
        .read(&tagged(
            vec![frame("APIC", &apic, 4), frame("APIC", &apic, 4)],
            4,
        ))
        .unwrap();
    // Two front covers: none is chosen, and the file goes without one.
    assert!(
        read_front_cover(&t.0.join("test.mp3"), &duplicated)
            .unwrap()
            .is_none()
    );
    let why = cover_left_out(&duplicated.artwork).unwrap();
    assert!(
        why.contains("2 pictures marked as the front cover"),
        "{why}"
    );
    // A lone picture is the cover whatever its type; beside other pictures,
    // the one front cover is.
    let other = [b"\x00image/jpeg\x00\x00\x00".as_slice(), &jpeg].concat();
    let lone = t.read(&tagged(vec![frame("APIC", &other, 4)], 4)).unwrap();
    assert_eq!(lone.artwork[0].picture_type, 0);
    assert_eq!(cover_index(&lone.artwork), Some(0));
    assert_eq!(
        read_front_cover(&t.0.join("test.mp3"), &lone)
            .unwrap()
            .unwrap()
            .bytes(),
        jpeg
    );
    let back = [b"\x00image/jpeg\x00\x04\x00".as_slice(), &jpeg].concat();
    let mixed = t
        .read(&tagged(
            vec![frame("APIC", &back, 4), frame("APIC", &apic, 4)],
            4,
        ))
        .unwrap();
    assert_eq!(cover_index(&mixed.artwork), Some(1));
    assert!(cover_left_out(&mixed.artwork).is_none());
    let unmarked = t
        .read(&tagged(
            vec![frame("APIC", &back, 4), frame("APIC", &other, 4)],
            4,
        ))
        .unwrap();
    assert_eq!(cover_index(&unmarked.artwork), None);
    assert!(
        cover_left_out(&unmarked.artwork)
            .unwrap()
            .contains("none is marked as the front cover")
    );
    let mut changed = tagged(vec![frame("APIC", &apic, 4)], 4);
    let last = changed.len() - 1;
    changed[last] ^= 1;
    fs::write(t.0.join("test.mp3"), changed).unwrap();
    assert!(read_front_cover(&t.0.join("test.mp3"), &s).is_err());
    let mut bad = apic;
    bad.pop();
    assert!(t.read(&tagged(vec![frame("APIC", &bad, 4)], 4)).is_err());
}
#[test]
fn apic_reads_png_and_any_jpeg_kind_by_their_own_bytes() {
    let t = Temp::new();
    let read = |mime: &[u8], image: &[u8]| {
        let apic = [b"\x00", mime, b"\x00\x03\x00", image].concat();
        t.read(&tagged(vec![frame("APIC", &apic, 4)], 4))
    };
    let png = crate::media::png::tests::png(5, 4);
    let p = &read(b"image/png", &png).unwrap().artwork[0];
    assert_eq!((p.mime, p.width, p.height), ("image/png", 5, 4));
    // A progressive grayscale JPEG: one component and two scans.
    let progressive = [
        0xff, 0xd8, 0xff, 0xc2, 0, 11, 8, 0, 2, 0, 3, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0,
        0, 0, 0x12, 0xff, 0xda, 0, 8, 1, 1, 0, 1, 63, 0, 0x34, 0xff, 0xd9,
    ];
    let p = &read(b"image/jpeg", &progressive).unwrap().artwork[0];
    assert_eq!((p.mime, p.width, p.height), ("image/jpeg", 3, 2));
    // A phone rendition is still held to the one baseline shape.
    assert!(crate::media::jpeg::dimensions(&progressive).is_err());
    // GIF, BMP and TIFF pictures are read, to be sent as JPEGs.
    use crate::media::bitmap::{Format, tests::picture};
    for (mime, format) in [
        (&b"image/gif"[..], Format::Gif),
        (b"image/x-ms-bmp", Format::Bmp),
        (b"image/tiff", Format::Tiff),
    ] {
        let p = &read(mime, &picture(format)).unwrap().artwork[0];
        assert_eq!((p.mime, p.width, p.height), (format.mime(), 2, 1));
    }
    // The declared type must agree with the bytes; other image types refuse.
    assert!(read(b"image/jpeg", &png).is_err());
    assert!(read(b"image/png", &progressive).is_err());
    assert!(read(b"image/gif", &png).is_err());
    let gif = format!("{:#}", read(b"image/gif", b"GIF89a").unwrap_err());
    assert!(gif.contains("reading a GIF header"), "{gif}");
    let webp = format!("{:#}", read(b"image/webp", &png).unwrap_err());
    assert!(webp.contains("only JPEG, PNG, GIF, BMP and TIFF"), "{webp}");
}
#[test]
fn a_picture_declaring_no_type_is_read_by_its_bytes() {
    let png = crate::media::png::tests::png(5, 4);
    let jpeg = [
        0xff, 0xd8, 0xff, 0xc2, 0, 11, 8, 0, 2, 0, 3, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0,
        0, 0, 0x12, 0xff, 0xda, 0, 8, 1, 1, 0, 1, 63, 0, 0x34, 0xff, 0xd9,
    ];
    // An empty type, or a bare `image/`, declares nothing: the bytes decide.
    for mime in [&b""[..], b"image/", b"IMAGE/"] {
        assert_eq!(image_facts(mime, &png).unwrap(), ("image/png", (5, 4)));
        assert_eq!(image_facts(mime, &jpeg).unwrap(), ("image/jpeg", (3, 2)));
        assert!(image_facts(mime, b"RIFF\x04\0\0\0WEBP").is_err());
        assert!(image_facts(mime, b"not a picture").is_err());
        assert!(image_facts(mime, b"").is_err());
    }
    // A declared type that disagrees with the bytes still refuses.
    assert!(image_facts(b"image/jpeg", &png).is_err());
    assert!(image_facts(b"image/png", &jpeg).is_err());
    assert!(image_facts(b"image/webp", &png).is_err());
    // An ID3v2.3 APIC frame may leave its type empty.
    let t = Temp::new();
    let apic = [b"\x00".as_slice(), b"\x00\x03\x00", &png].concat();
    let p = &t
        .read(&tagged(vec![frame("APIC", &apic, 3)], 3))
        .unwrap()
        .artwork[0];
    assert_eq!((p.mime, p.width, p.height), ("image/png", 5, 4));
}
#[test]
fn symlink_source_is_not_read() {
    let t = Temp::new();
    fs::write(t.0.join("target"), audio()).unwrap();
    std::os::unix::fs::symlink(t.0.join("target"), t.0.join("alias.mp3")).unwrap();
    assert!(read(&t.0.join("alias.mp3")).is_err());
}
