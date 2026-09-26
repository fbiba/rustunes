//! AIFF and AIFF-C with integer PCM (`NONE`, `twos` big-endian or `sowt`
//! little-endian). The identity covers the `COMM` chunk and the sound data of
//! `SSND`; tags live in an `ID3 ` chunk or the text chunks.
use std::io::{Read, Seek};

use anyhow::{Context, Result, bail, ensure};

use super::{Layout, TagKind, TagRegion, chunks, pcm_layout, read_span, sole};
use crate::media::format::Format;

pub fn inspect(input: &mut (impl Read + Seek), size: u64) -> Result<Layout> {
    let form = read_span(input, 8, 4)?;
    let compressed = form == b"AIFC";
    let chunks = chunks(input, size, false)?;
    let comm = sole(&chunks, b"COMM")?;
    let ssnd = sole(&chunks, b"SSND")?;
    ensure!(
        (18..=512).contains(&comm.bytes),
        "unsupported AIFF COMM chunk length"
    );
    let config = read_span(input, comm.offset, comm.bytes)?;
    let channels = u16::from_be_bytes([config[0], config[1]]);
    let frames = u32::from_be_bytes(config[2..6].try_into()?);
    let bits = u16::from_be_bytes([config[6], config[7]]);
    let sample_rate = extended_rate(config[8..18].try_into()?)?;
    if compressed {
        ensure!(config.len() >= 22, "truncated AIFF-C COMM chunk");
        match &config[18..22] {
            b"NONE" | b"twos" | b"sowt" => {}
            other => bail!(
                "AIFF-C compression {:?} is not integer PCM",
                String::from_utf8_lossy(other)
            ),
        }
    } else {
        ensure!(config.len() == 18, "AIFF COMM chunk has extra bytes");
    }
    ensure!(
        (1..=8).contains(&channels) && matches!(bits, 8 | 16 | 24 | 32) && frames > 0,
        "unsupported AIFF channels, bit depth or length"
    );
    ensure!(ssnd.bytes >= 8, "truncated AIFF SSND chunk");
    let header = read_span(input, ssnd.offset, 8)?;
    let skip = u64::from(u32::from_be_bytes(header[..4].try_into()?));
    let audio_bytes = u64::from(frames) * u64::from(channels) * u64::from(bits / 8);
    ensure!(
        skip.checked_add(8).and_then(|n| n.checked_add(audio_bytes)) == Some(ssnd.bytes),
        "AIFF sound data length ({}) differs from its COMM frames ({audio_bytes} bytes)",
        ssnd.bytes.saturating_sub(8 + skip)
    );
    let mut tags = Vec::new();
    for chunk in &chunks {
        let kind = match &chunk.id {
            b"ID3 " | b"id3 " => TagKind::Id3v2,
            b"NAME" | b"AUTH" | b"(c) " | b"ANNO" => TagKind::AiffText,
            _ => continue,
        };
        tags.push(TagRegion {
            kind,
            offset: chunk.offset,
            bytes: chunk.bytes,
        });
    }
    pcm_layout(
        input,
        Format::Aiff,
        &config,
        sample_rate,
        u8::try_from(channels)?,
        u8::try_from(bits)?,
        u64::from(frames),
        ssnd.offset + 8 + skip,
        audio_bytes,
        tags,
    )
}

/// An IEEE 754 80-bit extended sample rate that is a whole number of Hz.
fn extended_rate(bytes: [u8; 10]) -> Result<u32> {
    let exponent = u16::from_be_bytes([bytes[0], bytes[1]]);
    let mantissa = u64::from_be_bytes(bytes[2..].try_into()?);
    ensure!(
        exponent & 0x8000 == 0 && mantissa >> 63 == 1,
        "invalid AIFF sample rate"
    );
    let shift = (16383 + 63_i32)
        .checked_sub(i32::from(exponent))
        .filter(|s| (0..64).contains(s))
        .context("AIFF sample rate out of range")?;
    ensure!(
        mantissa & ((1u64 << shift) - 1) == 0,
        "AIFF sample rate is not a whole number of Hz"
    );
    u32::try_from(mantissa >> shift)
        .ok()
        .filter(|rate| *rate > 0)
        .context("AIFF sample rate out of range")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub(crate) fn extended(rate: u32) -> [u8; 10] {
        let shift = rate.leading_zeros();
        let mantissa = u64::from(rate) << (32 + shift);
        let exponent = (16383 + 31 - shift) as u16;
        let mut out = [0; 10];
        out[..2].copy_from_slice(&exponent.to_be_bytes());
        out[2..].copy_from_slice(&mantissa.to_be_bytes());
        out
    }

    pub(crate) fn aiff(channels: u16, rate: u32, bits: u16, frames: u32, extra: &[u8]) -> Vec<u8> {
        let mut comm = Vec::new();
        comm.extend(channels.to_be_bytes());
        comm.extend(frames.to_be_bytes());
        comm.extend(bits.to_be_bytes());
        comm.extend(extended(rate));
        let bytes = frames * u32::from(channels) * u32::from(bits / 8);
        let mut body = b"AIFF".to_vec();
        body.extend(b"COMM");
        body.extend((comm.len() as u32).to_be_bytes());
        body.extend(&comm);
        body.extend(extra);
        body.extend(b"SSND");
        body.extend((bytes + 8).to_be_bytes());
        body.extend([0; 8]);
        body.extend((0..bytes).map(|n| n as u8));
        if bytes % 2 == 1 {
            body.push(0);
        }
        let mut out = b"FORM".to_vec();
        out.extend((body.len() as u32).to_be_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn sample_rates_round_trip() {
        for rate in [8_000, 22_050, 44_100, 48_000, 96_000, 192_000] {
            assert_eq!(extended_rate(extended(rate)).unwrap(), rate);
        }
        let mut fraction = extended(44_100);
        fraction[9] = 1;
        assert!(extended_rate(fraction).is_err());
    }

    #[test]
    fn reads_pcm_and_keeps_identity_across_tags() {
        let plain = aiff(2, 44_100, 16, 500, &[]);
        let mut name = b"NAME".to_vec();
        name.extend(4u32.to_be_bytes());
        name.extend(b"Song");
        let tagged = aiff(2, 44_100, 16, 500, &name);
        let a = inspect(&mut Cursor::new(&plain), plain.len() as u64).unwrap();
        let b = inspect(&mut Cursor::new(&tagged), tagged.len() as u64).unwrap();
        assert_eq!(a.encoded.sha256(), b.encoded.sha256());
        assert_eq!((a.encoded.sample_rate, a.encoded.packets), (44_100, 500));
        assert_eq!(b.tags[0].kind, TagKind::AiffText);
        let mono = aiff(1, 44_100, 24, 3, &[]);
        let c = inspect(&mut Cursor::new(&mono), mono.len() as u64).unwrap();
        assert_eq!((c.encoded.channels, c.encoded.bits_per_sample), (1, 24));
    }

    #[test]
    fn refuses_frame_count_mismatch() {
        let mut bad = aiff(2, 44_100, 16, 500, &[]);
        bad[20 + 2..20 + 6].copy_from_slice(&501u32.to_be_bytes());
        assert!(inspect(&mut Cursor::new(&bad), bad.len() as u64).is_err());
    }
}
