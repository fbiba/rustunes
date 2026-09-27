use super::*;
use std::io::Cursor;

fn frame(kbps_index: u8, mode: u8) -> Vec<u8> {
    let b = [0xff, 0xfb, kbps_index << 4, mode << 6];
    let mut out = vec![0; header(&b).unwrap().bytes];
    out[..4].copy_from_slice(&b);
    out
}
fn span(b: &[u8], frames: u32, channels: u8) -> FrameSpan {
    FrameSpan {
        offset: 0,
        bytes: b.len() as u64,
        frames,
        sample_rate: 44100,
        channels,
        sha256: Sha256::digest(b).into(),
    }
}
fn inspect_bytes(b: &[u8], frames: u32, channels: u8) -> Result<TimingEvidence> {
    inspect(&mut Cursor::new(b), span(b, frames, channels))
}
fn xing(info: bool, mode: u8, encoder: Option<&[u8; 4]>) -> Vec<u8> {
    let mut frames = [frame(9, mode), frame(9, mode), frame(9, mode)].concat();
    let bytes = frames.len() as u32;
    let at = 4 + if mode == 3 { 17 } else { 32 };
    frames[at..at + 4].copy_from_slice(if info { b"Info" } else { b"Xing" });
    frames[at + 4..at + 8].copy_from_slice(&3u32.to_be_bytes());
    frames[at + 8..at + 12].copy_from_slice(&2u32.to_be_bytes());
    frames[at + 12..at + 16].copy_from_slice(&bytes.to_be_bytes());
    if let Some(vendor) = encoder {
        let e = at + 16;
        frames[e..e + 4].copy_from_slice(vendor);
        frames[e + 21..e + 24].copy_from_slice(&[0x24, 0x00, 0x64]); // 576 + 100
    }
    frames
}
#[test]
fn absent_header_retains_exact_coded_fraction_and_explicit_unknown_trim() {
    let bytes = [frame(9, 1), frame(10, 0)].concat();
    let t = inspect_bytes(&bytes, 2, 2).unwrap();
    assert_eq!(
        t.physical.coded_duration,
        SampleDuration {
            samples: 2304,
            sample_rate: 44100
        }
    );
    assert_eq!(t.physical.bitrate_mode, BitrateMode::Variable);
    assert_eq!(
        t.physical.bitrates,
        vec![
            BitrateCount {
                kbps: 128,
                frames: 1
            },
            BitrateCount {
                kbps: 160,
                frames: 1
            }
        ]
    );
    assert_eq!(t.physical.channel_mode_counts, [1, 1, 0, 0]);
    assert!(t.header.is_none() && t.excluding_xing_info_frame.is_none());
    assert!(!t.native_timing_qualified);
}
#[test]
fn xing_info_all_three_encoder_families_keep_tag_arithmetic_separate() {
    for vendor in [b"LAME", b"Lavf", b"Lavc"] {
        for mode in [0, 1, 3] {
            let t = inspect_bytes(
                &xing(true, mode, Some(vendor)),
                3,
                if mode == 3 { 1 } else { 2 },
            )
            .unwrap();
            assert_eq!(t.physical.coded_duration.samples, 3456);
            assert_eq!(
                t.excluding_xing_info_frame.unwrap().coded_duration.samples,
                2304
            );
            let Some(SeekHeader::Info(tag)) = t.header else {
                panic!()
            };
            assert_eq!(tag.declared_audio_frames, Some(2));
            let e = tag.encoder.unwrap();
            assert_eq!((e.delay_samples, e.padding_samples), (576, 100));
            assert_eq!(e.declared_trimmed_duration.samples, 1628);
            assert!(!e.crc_verified);
        }
    }
}
#[test]
fn missing_declarations_and_unknown_encoder_never_become_zero_trim() {
    let mut b = xing(false, 0, None);
    b[40..44].fill(0); // flags absent, following bytes are opaque
    let Some(SeekHeader::Xing(x)) = inspect_bytes(&b, 3, 2).unwrap().header else {
        panic!()
    };
    assert!(
        x.declared_audio_frames.is_none()
            && x.declared_stream_bytes.is_none()
            && x.encoder.is_none()
    );
    let b = xing(false, 0, Some(b"Other"[..4].try_into().unwrap()));
    let Some(SeekHeader::Xing(x)) = inspect_bytes(&b, 3, 2).unwrap().header else {
        panic!()
    };
    assert!(x.encoder.is_none());
    let mut b = xing(false, 0, Some(b"LAME"));
    b[73..76].fill(0);
    let Some(SeekHeader::Xing(x)) = inspect_bytes(&b, 3, 2).unwrap().header else {
        panic!()
    };
    assert_eq!(x.encoder.unwrap().delay_samples, 0); // explicit tag zero, not unknown
}
#[test]
fn info_measures_audio_cbr_independently_of_metadata_frame_bitrate() {
    let b = xing(true, 0, None);
    let mut first = frame(10, 0);
    first[36..52].copy_from_slice(&b[36..52]);
    let mut b = [first, frame(9, 0), frame(9, 0)].concat();
    let len = b.len() as u32;
    b[48..52].copy_from_slice(&len.to_be_bytes());
    let t = inspect_bytes(&b, 3, 2).unwrap();
    assert_eq!(t.physical.bitrate_mode, BitrateMode::Variable);
    assert_eq!(
        t.excluding_xing_info_frame.unwrap().bitrate_mode,
        BitrateMode::Constant
    );
    let mut b = [xing(true, 0, None)[..834].to_vec(), frame(10, 0)].concat();
    let len = b.len() as u32;
    b[48..52].copy_from_slice(&len.to_be_bytes());
    assert!(
        inspect_bytes(&b, 3, 2)
            .unwrap_err()
            .to_string()
            .contains("constant-bitrate")
    );
}
#[test]
fn contradictory_xing_fields_trim_and_reserved_flags_refuse() {
    let original = xing(false, 0, Some(b"LAME"));
    for (at, data, message) in [
        (44, 3u32.to_be_bytes().to_vec(), "frame count"),
        (48, 0u32.to_be_bytes().to_vec(), "stream size"),
        (40, 16u32.to_be_bytes().to_vec(), "flags"),
        (73, vec![255; 3], "trim"),
        (61, vec![0x10], "revision"),
    ] {
        let mut b = original.clone();
        b[at..at + data.len()].copy_from_slice(&data);
        assert!(
            inspect_bytes(&b, 3, 2)
                .unwrap_err()
                .to_string()
                .contains(message)
        );
    }
}
#[test]
fn truncated_optional_toc_and_encoder_tag_do_not_read_the_next_frame() {
    // A 32 kbps frame has only 104 bytes. The declared TOC and recognized
    // encoder extension each exceed it even though more audio follows.
    for encoder in [false, true] {
        let mut b = [frame(1, 0), frame(1, 0)].concat();
        b[36..40].copy_from_slice(b"Xing");
        b[40..44].copy_from_slice(&(if encoder { 8u32 } else { 4u32 }).to_be_bytes());
        if encoder {
            // Flag 8 puts the quality field at 44..48 and the encoder tag at 48.
            // A bounded first-frame slice gives a deterministic truncation.
            b[48..52].copy_from_slice(b"LAME");
            let s = span(&b, 2, 2);
            assert!(
                parse_xing(&b[..75], 36, &s)
                    .unwrap_err()
                    .to_string()
                    .contains("truncated")
            );
        } else {
            assert!(
                inspect_bytes(&b, 2, 2)
                    .unwrap_err()
                    .to_string()
                    .contains("truncated")
            );
        }
    }
}
#[test]
fn unordered_toc_refuses_even_with_matching_counts_and_file_hash() {
    let mut b = xing(false, 0, None);
    b[40..44].copy_from_slice(&7u32.to_be_bytes());
    b[52] = 9;
    b[53] = 8;
    assert!(
        inspect_bytes(&b, 3, 2)
            .unwrap_err()
            .to_string()
            .contains("TOC")
    );
}
#[test]
fn crc_protected_header_records_observed_tag_position() {
    for shifted in [false, true] {
        let mut b = xing(false, 0, None);
        b[1] = 0xfa;
        if shifted {
            b.copy_within(36..52, 38);
            b[36..38].fill(0);
        }
        let t = inspect_bytes(&b, 3, 2).unwrap();
        assert_eq!(t.physical.crc_protected_frames, 1);
        let Some(SeekHeader::Xing(x)) = t.header else {
            panic!()
        };
        assert_eq!(x.frame_offset, if shifted { 38 } else { 36 });
    }
}
fn vbri() -> Vec<u8> {
    let mut b = [frame(9, 0), frame(9, 0), frame(9, 0)].concat();
    b[36..40].copy_from_slice(b"VBRI");
    b[40..42].copy_from_slice(&1u16.to_be_bytes());
    let bytes = b.len() as u32;
    b[46..50].copy_from_slice(&bytes.to_be_bytes());
    b[50..54].copy_from_slice(&3u32.to_be_bytes());
    b[54..56].copy_from_slice(&1u16.to_be_bytes()); // one table entry
    b[56..58].copy_from_slice(&1u16.to_be_bytes()); // scale
    b[58..60].copy_from_slice(&2u16.to_be_bytes()); // bytes per entry
    b[60..62].copy_from_slice(&3u16.to_be_bytes()); // frames per entry
    b[62..64].copy_from_slice(&(bytes as u16).to_be_bytes());
    b
}
#[test]
fn vbri_retains_actual_count_basis_without_inventing_gapless_trim() {
    let b = vbri();
    let t = inspect_bytes(&b, 3, 2).unwrap();
    assert!(t.excluding_xing_info_frame.is_none());
    let Some(SeekHeader::Vbri(v)) = t.header else {
        panic!()
    };
    assert_eq!(v.count_basis, VbriCountBasis::PhysicalFrames);
    let mut b = vbri();
    b[50..54].copy_from_slice(&2u32.to_be_bytes());
    let Some(SeekHeader::Vbri(v)) = inspect_bytes(&b, 3, 2).unwrap().header else {
        panic!()
    };
    assert_eq!(v.count_basis, VbriCountBasis::ExcludingFirstFrame);
}
#[test]
fn vbri_version_count_size_and_table_geometry_are_bounded() {
    for (at, data) in [
        (40, 2u16.to_be_bytes().to_vec()),
        (50, 9u32.to_be_bytes().to_vec()),
        (46, 0u32.to_be_bytes().to_vec()),
        (56, vec![0, 0]),
        (58, vec![0, 5]),
        (60, vec![0, 1]),
        (62, vec![0, 1]),
    ] {
        let mut b = vbri();
        b[at..at + data.len()].copy_from_slice(&data);
        assert!(inspect_bytes(&b, 3, 2).is_err());
    }
    let b = vbri();
    assert!(parse_vbri(&b[..63], &span(&b, 3, 2)).is_err());
}
#[test]
fn vbri_unit_scale_cannot_hide_an_empty_group_in_the_aggregate() {
    let mut b = vbri();
    b[54..56].copy_from_slice(&2u16.to_be_bytes()); // two entries
    b[60..62].copy_from_slice(&2u16.to_be_bytes()); // two frames per entry
    b[62..64].copy_from_slice(&0u16.to_be_bytes());
    b[64..66].copy_from_slice(&1251u16.to_be_bytes());
    assert!(inspect_bytes(&b, 3, 2).is_err());
}
#[test]
fn coarse_vbri_table_retains_quantized_declarations_without_offset_claim() {
    let mut b = vbri();
    b[54..56].copy_from_slice(&2u16.to_be_bytes());
    b[56..58].copy_from_slice(&1000u16.to_be_bytes());
    b[60..62].copy_from_slice(&2u16.to_be_bytes());
    b[62..64].copy_from_slice(&0u16.to_be_bytes());
    b[64..66].copy_from_slice(&1u16.to_be_bytes());
    let Some(SeekHeader::Vbri(v)) = inspect_bytes(&b, 3, 2).unwrap().header else {
        panic!()
    };
    assert_eq!(v.table_byte_extents, [0, 1000]);
    assert!(!v.table_offsets_verified);
}
#[test]
fn all_supported_rates_modes_and_bitrates_retain_exact_sample_fractions() {
    for rate_index in 0..3u8 {
        for bitrate_index in 1..=14u8 {
            for mode in 0..4u8 {
                let raw = [
                    0xff,
                    0xfb,
                    (bitrate_index << 4) | (rate_index << 2) | 2,
                    mode << 6,
                ];
                let h = header(&raw).unwrap();
                let mut frame = vec![0; h.bytes];
                frame[..4].copy_from_slice(&raw);
                let b = [frame.clone(), frame].concat();
                let mut s = span(&b, 2, h.channels);
                s.sample_rate = h.rate;
                let t = inspect(&mut Cursor::new(&b), s).unwrap();
                assert_eq!(
                    t.physical.coded_duration,
                    SampleDuration {
                        samples: 2304,
                        sample_rate: h.rate
                    }
                );
                assert_eq!(
                    t.physical.bitrates,
                    [BitrateCount {
                        kbps: h.kbps,
                        frames: 2
                    }]
                );
                assert_eq!(t.physical.channel_mode_counts[mode as usize], 2);
            }
        }
    }
}
#[test]
fn two_first_frame_headers_and_cross_scan_byte_drift_refuse() {
    let mut b = xing(false, 3, None); // Xing at 21; VBRI at 36
    b[36..40].copy_from_slice(b"VBRI");
    assert!(
        inspect_bytes(&b, 3, 1)
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    let mut b = xing(false, 0, None);
    let s = span(&b, 3, 2);
    b[400] ^= 1;
    assert!(
        inspect(&mut Cursor::new(b), s)
            .unwrap_err()
            .to_string()
            .contains("identity")
    );
}
#[test]
fn bounds_count_format_and_truncated_frame_are_not_duration_estimates() {
    let b = [frame(9, 0), frame(9, 0)].concat();
    let mut s = span(&b, 2, 2);
    s.frames = 2_000_001;
    assert!(inspect(&mut Cursor::new(&b), s).is_err());
    assert!(inspect_bytes(&b, 3, 2).is_err());
    assert!(inspect_bytes(&b[..b.len() - 1], 2, 2).is_err());
    assert!(inspect_bytes(&b, 2, 1).is_err());
    let mut b = b;
    b[1] = 0xf3;
    assert!(inspect_bytes(&b, 2, 2).is_err());
}
