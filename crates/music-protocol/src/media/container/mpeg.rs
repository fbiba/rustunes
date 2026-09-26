//! MPEG-2 and MPEG-2.5 Layer III streams (half and quarter sample rates, 576
//! samples a frame), with optional ID3v2 before and ID3v1 after. An MPEG-1
//! stream is left to [`crate::media::other_audio`], which keeps its own
//! identity. The identity covers the stream's shape and every frame byte.
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

use super::{Layout, Stream, TagKind, TagRegion, average_kbps, hash_span};
use crate::media::{fingerprint::EncodedAudio, format::Format};

const BITRATES: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// `None` for anything that does not start (after ID3v2) with an MPEG-2 or
/// 2.5 Layer III frame.
pub fn inspect(input: &mut (impl Read + Seek), size: u64) -> Result<Option<Layout>> {
    let mut tags = Vec::new();
    let start = match id3v2_length(input, size)? {
        Some(length) => {
            tags.push(TagRegion {
                kind: TagKind::Id3v2,
                offset: 0,
                bytes: length,
            });
            length
        }
        None => 0,
    };
    let mut end = size;
    if size >= 128 + start {
        input.seek(SeekFrom::Start(size - 128))?;
        let mut marker = [0; 3];
        input.read_exact(&mut marker)?;
        if &marker == b"TAG" {
            end -= 128;
            tags.push(TagRegion {
                kind: TagKind::Id3v1,
                offset: end,
                bytes: 128,
            });
        }
    }
    let Some(first) = header_at(input, start, end)? else {
        return Ok(None);
    };
    let (version, rate_index, channels) = first.shape;
    let mut offset = start;
    let mut frames = 0u64;
    let mut kbps = Vec::new();
    while offset < end {
        ensure!(frames < 4_000_000, "MPEG frame count bound");
        let header = header_at(input, offset, end)?
            .filter(|h| h.length <= end - offset)
            .ok_or_else(|| anyhow::anyhow!("MPEG frame {frames} is damaged or truncated"))?;
        ensure!(
            header.shape == first.shape,
            "MPEG stream changes version, sample rate or channel count"
        );
        kbps.push(header.kbps);
        offset += header.length;
        frames += 1;
    }
    ensure!(frames >= 2, "MPEG stream has fewer than two frames");
    let sample_rate = first.sample_rate;
    let audio_bytes = end - start;
    let coded_samples = frames * 576;
    let constant = kbps.iter().all(|k| *k == kbps[0]);
    let encoded = EncodedAudio {
        format: if version == 2 {
            Format::Mpeg2Layer3
        } else {
            Format::Mpeg25Layer3
        },
        sample_rate,
        channels,
        bits_per_sample: 0,
        packets: frames,
        coded_samples,
        audio_offset: start,
        audio_bytes,
        config_sha256: Sha256::digest([b'L', b'3', version, rate_index, channels]).into(),
        audio_sha256: hash_span(input, start, audio_bytes)?,
    };
    Ok(Some(Layout {
        stream: Stream {
            bit_rate_kbps: if constant {
                kbps[0]
            } else {
                average_kbps(audio_bytes, coded_samples, sample_rate)?
            },
            variable_bit_rate: !constant,
            encoder_delay: 0,
            encoder_padding: 0,
        },
        encoded,
        tags,
    }))
}

struct Header {
    /// Version (2 or 25), sample-rate index, channels.
    shape: (u8, u8, u8),
    sample_rate: u32,
    kbps: u32,
    length: u64,
}

fn header_at(input: &mut (impl Read + Seek), offset: u64, end: u64) -> Result<Option<Header>> {
    if end - offset < 4 {
        return Ok(None);
    }
    input.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    let word = u32::from_be_bytes(bytes);
    let version = match (word >> 19) & 3 {
        0 => 25,
        2 => 2,
        _ => return Ok(None),
    };
    let bitrate_index = (word >> 12) & 15;
    let rate_index = (word >> 10) & 3;
    if word >> 21 != 0x7ff
        || (word >> 17) & 3 != 1
        || !(1..=14).contains(&bitrate_index)
        || rate_index == 3
        || word & 3 == 2
    {
        return Ok(None);
    }
    let rates: [u32; 3] = if version == 2 {
        [22_050, 24_000, 16_000]
    } else {
        [11_025, 12_000, 8_000]
    };
    let sample_rate = rates[rate_index as usize];
    let kbps = BITRATES[bitrate_index as usize];
    let length = u64::from(72_000 * kbps / sample_rate + ((word >> 9) & 1));
    let channels = if (word >> 6) & 3 == 3 { 1 } else { 2 };
    Ok(Some(Header {
        shape: (version, rate_index as u8, channels),
        sample_rate,
        kbps,
        length,
    }))
}

/// The length of a leading ID3v2 tag (with its footer), if there is one.
pub(crate) fn id3v2_length(input: &mut (impl Read + Seek), size: u64) -> Result<Option<u64>> {
    input.seek(SeekFrom::Start(0))?;
    let mut first = [0; 10];
    input.read_exact(&mut first)?;
    if &first[..3] != b"ID3" {
        return Ok(None);
    }
    ensure!(
        matches!(first[3], 3 | 4) && first[4] == 0 && first[6..].iter().all(|b| b & 0x80 == 0),
        "unsupported ID3 header"
    );
    let tag = first[6..]
        .iter()
        .fold(0u64, |n, b| (n << 7) | u64::from(*b));
    let footer = if first[3] == 4 && first[5] & 0x10 != 0 {
        10
    } else {
        0
    };
    let length = 10 + tag + footer;
    ensure!(length < size, "ID3 tag exceeds the file");
    Ok(Some(length))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// MPEG-2 Layer III, 22.05 kHz, 64 kb/s, joint stereo: 208-byte frames.
    pub(crate) fn mpeg2(frames: usize) -> Vec<u8> {
        let mut frame = vec![0; 208];
        frame[..4].copy_from_slice(&[0xff, 0xf3, 0x80, 0x40]);
        frame.repeat(frames)
    }

    #[test]
    fn reads_mpeg2_frames_and_leaves_mpeg1_alone() {
        let bytes = mpeg2(10);
        let layout = inspect(&mut Cursor::new(&bytes), bytes.len() as u64)
            .unwrap()
            .unwrap();
        assert_eq!(layout.encoded.format, Format::Mpeg2Layer3);
        assert_eq!(
            (layout.encoded.sample_rate, layout.encoded.packets),
            (22_050, 10)
        );
        assert_eq!(layout.encoded.coded_samples, 5760);
        assert_eq!(layout.stream.bit_rate_kbps, 64);
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x02\x00\x00".to_vec();
        tagged.extend(&bytes);
        let again = inspect(&mut Cursor::new(&tagged), tagged.len() as u64)
            .unwrap()
            .unwrap();
        assert_eq!(again.encoded.sha256(), layout.encoded.sha256());
        let mut mpeg1 = vec![0; 417];
        mpeg1[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let mpeg1 = mpeg1.repeat(3);
        assert!(inspect(&mut Cursor::new(&mpeg1), 1251).unwrap().is_none());
    }

    #[test]
    fn refuses_truncated_streams() {
        let bytes = mpeg2(3);
        let cut = &bytes[..bytes.len() - 5];
        assert!(inspect(&mut Cursor::new(cut), cut.len() as u64).is_err());
    }
}
