//! RIFF WAVE with integer PCM (`WAVE_FORMAT_PCM`, or `WAVE_FORMAT_EXTENSIBLE`
//! with the PCM subformat). The identity covers the `fmt ` chunk and the
//! `data` payload; tags live in a `LIST`/`INFO` chunk or an `id3 ` chunk.
use std::io::{Read, Seek};

use anyhow::{Result, ensure};

use super::{Layout, TagKind, TagRegion, chunks, pcm_layout, read_span, sole};
use crate::media::format::Format;

/// KSDATAFORMAT_SUBTYPE_PCM after its first two bytes.
const PCM_GUID_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

pub fn inspect(input: &mut (impl Read + Seek), size: u64) -> Result<Layout> {
    let chunks = chunks(input, size, true)?;
    let fmt = sole(&chunks, b"fmt ")?;
    let data = sole(&chunks, b"data")?;
    ensure!(
        chunks.iter().position(|c| &c.id == b"fmt ") < chunks.iter().position(|c| &c.id == b"data"),
        "the WAV format chunk follows its data"
    );
    ensure!(
        (16..=40).contains(&fmt.bytes),
        "unsupported WAV format chunk length"
    );
    let config = read_span(input, fmt.offset, fmt.bytes)?;
    let u16_at = |at: usize| u16::from_le_bytes([config[at], config[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(config[at..at + 4].try_into().unwrap());
    let tag = u16_at(0);
    let channels = u16_at(2);
    let sample_rate = u32_at(4);
    let byte_rate = u32_at(8);
    let block_align = u16_at(12);
    let bits = u16_at(14);
    match tag {
        1 => {}
        0xfffe => {
            ensure!(
                config.len() >= 40 && u16_at(16) == 22,
                "truncated WAVE_FORMAT_EXTENSIBLE"
            );
            ensure!(
                u16_at(18) == bits && u16_at(24) == 1 && config[26..40] == PCM_GUID_TAIL,
                "only integer PCM WAV is supported (extensible subformat is not PCM, or its valid bits differ)"
            );
        }
        3 => anyhow::bail!("floating-point WAV is not supported; the phone plays integer PCM"),
        other => anyhow::bail!("WAV format tag {other:#06x} is not integer PCM"),
    }
    ensure!(
        (1..=8).contains(&channels) && matches!(bits, 8 | 16 | 24 | 32) && sample_rate > 0,
        "unsupported WAV channels, bit depth or sample rate"
    );
    let frame = u32::from(channels) * u32::from(bits / 8);
    ensure!(
        u32::from(block_align) == frame && byte_rate == sample_rate * frame,
        "WAV block alignment or byte rate disagrees with its format"
    );
    ensure!(
        data.bytes > 0 && data.bytes % u64::from(frame) == 0,
        "WAV data is not whole sample frames"
    );
    let mut tags = Vec::new();
    for chunk in &chunks {
        let kind = match &chunk.id {
            b"id3 " | b"ID3 " => TagKind::Id3v2,
            b"LIST" if chunk.bytes >= 4 && read_span(input, chunk.offset, 4)? == b"INFO" => {
                TagKind::RiffInfo
            }
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
        Format::Wav,
        &config,
        sample_rate,
        u8::try_from(channels)?,
        u8::try_from(bits)?,
        data.bytes / u64::from(frame),
        data.offset,
        data.bytes,
        tags,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    pub(crate) fn wav(channels: u16, rate: u32, bits: u16, frames: u32, extra: &[u8]) -> Vec<u8> {
        let frame = u32::from(channels) * u32::from(bits / 8);
        let mut fmt = Vec::new();
        fmt.extend(1u16.to_le_bytes());
        fmt.extend(channels.to_le_bytes());
        fmt.extend(rate.to_le_bytes());
        fmt.extend((rate * frame).to_le_bytes());
        fmt.extend((frame as u16).to_le_bytes());
        fmt.extend(bits.to_le_bytes());
        let data: Vec<u8> = (0..frames * frame).map(|n| n as u8).collect();
        let mut body = b"WAVE".to_vec();
        body.extend(b"fmt ");
        body.extend((fmt.len() as u32).to_le_bytes());
        body.extend(&fmt);
        body.extend(extra);
        body.extend(b"data");
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(&data);
        let mut out = b"RIFF".to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn reads_pcm_and_keeps_identity_across_tags() {
        let plain = wav(2, 44_100, 16, 1000, &[]);
        let mut list = b"LIST".to_vec();
        list.extend(12u32.to_le_bytes());
        list.extend(b"INFOINAM");
        list.extend(0u32.to_le_bytes());
        let tagged = wav(2, 44_100, 16, 1000, &list);
        let a = inspect(&mut Cursor::new(&plain), plain.len() as u64).unwrap();
        let b = inspect(&mut Cursor::new(&tagged), tagged.len() as u64).unwrap();
        assert_eq!(a.encoded.sha256(), b.encoded.sha256());
        assert_eq!(a.encoded.packets, 1000);
        assert_eq!(a.stream.bit_rate_kbps, 1411);
        assert_eq!(b.tags.len(), 1);
        assert_eq!(b.tags[0].kind, TagKind::RiffInfo);
        assert_ne!(a.encoded.audio_offset, b.encoded.audio_offset);
        let other = wav(2, 48_000, 24, 1000, &[]);
        let c = inspect(&mut Cursor::new(&other), other.len() as u64).unwrap();
        assert_ne!(a.encoded.sha256(), c.encoded.sha256());
        assert_eq!(c.encoded.bits_per_sample, 24);
    }

    #[test]
    fn refuses_float_truncation_and_partial_frames() {
        let mut float = wav(2, 44_100, 32, 10, &[]);
        float[20] = 3;
        assert!(inspect(&mut Cursor::new(&float), float.len() as u64).is_err());
        let good = wav(2, 44_100, 16, 10, &[]);
        let short = &good[..good.len() - 1];
        assert!(inspect(&mut Cursor::new(short), short.len() as u64).is_err());
        let mut odd = wav(2, 44_100, 16, 10, &[]);
        let at = odd.len() - 40 - 4;
        odd[at..at + 4].copy_from_slice(&39u32.to_le_bytes());
        odd.pop();
        let riff = (odd.len() - 8) as u32;
        odd[4..8].copy_from_slice(&riff.to_le_bytes());
        assert!(inspect(&mut Cursor::new(&odd), odd.len() as u64).is_err());
    }
}
