use super::*;
use std::io::Cursor;

/// `count` MPEG-1 Layer III frames at 128 kb/s and 48 kHz (384 bytes, no
/// padding, so the byte estimate is exact), silent; `first` replaces the
/// first frame's body from offset 32 (after the stereo side information).
fn stream(count: usize, first: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 384];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x94, 0x00]);
    let mut bytes = frame.repeat(count);
    bytes[36..36 + first.len()].copy_from_slice(first);
    bytes
}

fn lame_header(kind: &[u8; 4], frames: u32, delay: u32, padding: u32) -> Vec<u8> {
    let mut h = kind.to_vec();
    h.extend(1_u32.to_be_bytes()); // frames only
    h.extend(frames.to_be_bytes());
    let mut tag = b"LAME3.100".to_vec();
    tag.resize(21, 0);
    tag.extend([
        (delay >> 4) as u8,
        ((delay & 15) << 4 | padding >> 8) as u8,
        padding as u8,
    ]);
    h.extend(tag);
    h
}

fn analysed(bytes: &[u8], hints: &EncoderHints) -> AppleTiming {
    analyse(&mut Cursor::new(bytes), 0, bytes.len() as u64, hints).unwrap()
}

#[test]
fn the_encoder_named_in_the_tag_gives_the_delay() {
    for (text, delay, cd) in [
        ("", 528, false),
        ("Lavf58.76.100", 528, false),
        ("LAME 3.100", 576, false),
        ("Encoded by Apple Music", 528, false),
        ("MusicMatch Jukebox", 726, true),
        ("Amazon.com Song ID: 1", 576, true),
        ("Fraunhofer IIS", 726, false),
        ("iTunes v12", 528, false),
    ] {
        assert_eq!(encoder_delay(text, 0), (delay, cd), "{text:?}");
    }
    assert_eq!(encoder_delay("Fraunhofer IIS", 1000), (1000, false));
}

#[test]
fn hints_follow_the_tags_order() {
    let mut hints = EncoderHints::default();
    hints.comment("", "Ripped with MusicMatch");
    hints.encoder("LAME3.100");
    assert_eq!((hints.text.as_str(), hints.cd_rip), ("LAME3.100", false));
    // A comment after an encoder frame names nothing.
    hints.comment("", "MusicMatch Jukebox");
    assert_eq!(hints.text, "LAME3.100");
    let mut hints = EncoderHints::default();
    hints.comment("iTunNORM", " 0000044E");
    hints.comment("iTunes_CDDB_1", "x");
    assert_eq!((hints.text.as_str(), hints.cd_rip), ("iTunNORM", true));
    let mut hints = EncoderHints::default();
    hints.comment("", "MusicMatch Jukebox 7");
    assert!(hints.cd_rip);
    hints.encoder(&"é".repeat(200));
    assert_eq!(hints.text.len(), 254);
}

#[test]
fn an_itunsmpb_comment_gives_the_values_it_holds() {
    let text = " 00000000 00000840 000001A4 0000000000A5D5CC 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000";
    assert_eq!(text.len(), 116);
    assert_eq!(itunsmpb(text), Some((0x840, 0x1a4, 0xa5d5cc, 0, 0)));
    assert_eq!(itunsmpb(&text[1..]), Some((0x840, 0x1a4, 0xa5d5cc, 0, 0)));
    assert_eq!(itunsmpb(&text[2..]), None);
    // 50 frames of 1152: 57600 samples.
    let hints = EncoderHints {
        itunsmpb: Some(format!(
            " 00000000 00000240 000001C0 {:016X} 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000",
            57600 - 0x240 - 0x1c0
        )),
        ..Default::default()
    };
    let t = analysed(&stream(50, &[]), &hints);
    assert_eq!(
        (t.source, t.delay, t.drain, t.duration, t.heuristic),
        ("itunsmpb", 0x240, 0x1c0, 57600 - 0x240 - 0x1c0, 1)
    );
    // The resynch the tag leaves at 0 is the eighth frame from the end.
    assert_eq!(t.resynch, 42 * 384);
}

#[test]
fn a_lame_tag_is_taken_with_one_frame_more_when_not_a_cd_multiple() {
    // Info: the byte estimate counts its frame; 50 * 1152 - 576 - 1000 is
    // not a multiple of 588, so another 1152 samples are dropped.
    let t = analysed(
        &stream(50, &lame_header(b"Info", 49, 576, 1000)),
        &Default::default(),
    );
    assert_eq!(
        (
            t.source,
            t.frames,
            t.delay,
            t.drain,
            t.duration,
            t.heuristic
        ),
        ("lame_tag", 50, 576, 2152, 57600 - 2152 - 576, 1)
    );
    assert_eq!((t.total_time_ms, t.bit_rate_kbps), (1200, 128));
    // Xing: its own frame is not counted, and the rate is the average.
    let t = analysed(
        &stream(50, &lame_header(b"Xing", 49, 576, 1000)),
        &Default::default(),
    );
    assert_eq!(
        (t.frames, t.total_time_ms, t.bit_rate_kbps),
        (49, 1176, 130)
    );
    assert_eq!(t.duration, 49 * 1152 - u64::from(t.drain) - 576);
    // The agent tests the padding after that correction: none becomes one
    // frame's, unless the length is already a CD multiple (25 frames).
    let t = analysed(
        &stream(50, &lame_header(b"Info", 49, 576, 0)),
        &Default::default(),
    );
    assert_eq!((t.source, t.drain), ("lame_tag", 1152));
    let t = analysed(
        &stream(25, &lame_header(b"Info", 24, 576, 0)),
        &Default::default(),
    );
    assert_eq!(t.source, "analysis");
    // No delay, or one of 2000 or more, is no tag.
    for delay in [0, 2000] {
        let t = analysed(
            &stream(50, &lame_header(b"Info", 49, delay, 100)),
            &Default::default(),
        );
        assert_eq!(t.source, "analysis", "{delay}");
    }
}

#[test]
fn without_a_tag_the_silent_tail_gives_the_first_candidate() {
    let t = analysed(&stream(50, &[]), &Default::default());
    // 48 kHz is no CD rate: the length is total - delay - 530.
    assert_eq!(
        (
            t.source,
            t.frames,
            t.delay,
            t.duration,
            t.drain,
            t.heuristic
        ),
        ("analysis", 50, 528, 57600 - 528 - 530, 530, 0x0200_0003)
    );
    assert_eq!(t.resynch, 42 * 384);
    // A CD rip rounds it down to a multiple of 588.
    let cd = EncoderHints {
        cd_rip: true,
        ..Default::default()
    };
    let t = analysed(&stream(50, &[]), &cd);
    assert_eq!(t.duration, (57600 - 528 - 530) / 588 * 588);
    assert_eq!(t.drain as u64, 57600 - 528 - t.duration);
    // Fewer than eight frames have no resynch.
    assert_eq!(analysed(&stream(7, &[]), &Default::default()).resynch, 0);
}

#[test]
fn the_search_takes_where_a_loud_tail_stops() {
    let total = 57600;
    let start = 528 + 529;
    let n = 4608;
    let silent = vec![0.0_f32; n * 2];
    assert_eq!(
        search(&silent, n, 2, start, total, 1152, false),
        total - start - 1
    );
    // A loud tail that alternates sign hits at every candidate; of the first
    // two (588 apart with the CD rule) the later wins on equal magnitude.
    let loud: Vec<f32> = (0..n * 2)
        .map(|i| if (i / 2) % 2 == 0 { 0.5 } else { -0.5 })
        .collect();
    let first = (total - start - 1) / 588 * 588;
    assert_eq!(search(&loud, n, 2, start, total, 1152, true), first - 588);
    assert_eq!(
        search(&loud, n, 2, start, total, 1152, false),
        total - start - 2
    );
    assert_eq!(search(&[], 0, 2, start, total, 1152, true), total - start);
}

fn fixture(name: &str) -> crate::media::mpeg::Source {
    crate::media::mpeg::read(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[test]
fn the_vbr_fixture_ends_where_its_tone_is_cut() {
    // 14 frames, a Xing header counting 13; the tone ends abruptly, so the
    // search stops before the drain the silent rule gives. Agrees with an
    // untrimmed FFmpeg decode.
    let t = fixture("tone-mp3-vbr.mp3").apple_timing.unwrap();
    assert_eq!(
        (
            t.frames,
            t.total_time_ms,
            t.bit_rate_kbps,
            t.delay,
            t.resynch
        ),
        (13, 339, 54, 528, 1355)
    );
    assert_eq!(
        (t.duration, t.drain, t.heuristic),
        (11172, 3276, 0x0200_0003)
    );
}

#[test]
fn the_mpeg2_fixture_counts_576_samples_a_frame() {
    let s = fixture("tone-mpeg2.mp3");
    let t = s.apple_timing.unwrap();
    assert_eq!(t.samples_per_frame, 576);
    assert_eq!(u64::from(t.frames) * 576 * 1000 / 22050, t.total_time_ms);
    assert_eq!(
        u64::from(t.frames) * 576,
        t.duration + u64::from(t.delay + t.drain)
    );
}

#[test]
#[ignore = "retained user source, read-only; explicit MP3_GAPLESS_SOURCE required"]
fn gapless_source_reproduces_apples_insert() {
    let path = std::env::var_os("MP3_GAPLESS_SOURCE").expect("MP3_GAPLESS_SOURCE");
    let t = crate::media::mpeg::read(std::path::Path::new(&path))
        .unwrap()
        .apple_timing
        .unwrap();
    assert_eq!(
        t,
        AppleTiming {
            frames: 9691,
            samples_per_frame: 1152,
            total_time_ms: 253_152,
            bit_rate_kbps: 128,
            duration: 11_162_592,
            delay: 528,
            drain: 912,
            resynch: 4_047_097,
            heuristic: 33_554_435,
            source: "analysis",
        }
    );
}
