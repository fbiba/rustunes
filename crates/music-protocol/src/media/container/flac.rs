//! Native FLAC: `fLaC`, metadata blocks (STREAMINFO first), then frames to the
//! end of the file (less a trailing ID3v1 tag). The identity covers STREAMINFO
//! and every frame byte; tags live in VORBIS_COMMENT and PICTURE blocks, so
//! retagging or repadding keeps it.
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::{Layout, Stream, TagKind, TagRegion, average_kbps, hash_span, read_span};
use crate::media::{fingerprint::EncodedAudio, format::Format};

pub fn inspect(input: &mut (impl Read + Seek), size: u64) -> Result<Layout> {
    let mut position = 4;
    let mut streaminfo = None;
    let mut tags = Vec::new();
    loop {
        ensure!(size - position >= 4, "truncated FLAC metadata block");
        input.seek(SeekFrom::Start(position))?;
        let mut header = [0; 4];
        input.read_exact(&mut header)?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let bytes = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        let offset = position + 4;
        ensure!(
            bytes <= size - offset,
            "FLAC metadata block exceeds the file"
        );
        match kind {
            0 => {
                ensure!(
                    streaminfo.is_none() && position == 4 && bytes == 34,
                    "FLAC STREAMINFO must come first, once, with 34 bytes"
                );
                streaminfo = Some(read_span(input, offset, 34)?);
            }
            1 | 2 | 3 | 5 => {}
            4 => tags.push(TagRegion {
                kind: TagKind::VorbisComment,
                offset,
                bytes,
            }),
            6 => tags.push(TagRegion {
                kind: TagKind::FlacPicture,
                offset,
                bytes,
            }),
            127 => bail!("invalid FLAC metadata block type"),
            _ => {}
        }
        ensure!(
            streaminfo.is_some(),
            "FLAC STREAMINFO must be the first metadata block"
        );
        position = offset + bytes;
        if last {
            break;
        }
    }
    let info = streaminfo.context("FLAC STREAMINFO missing")?;
    let sample_rate =
        (u32::from(info[10]) << 12) | (u32::from(info[11]) << 4) | u32::from(info[12] >> 4);
    let channels = ((info[12] >> 1) & 7) + 1;
    let bits = (((info[12] & 1) << 4) | (info[13] >> 4)) + 1;
    let samples = (u64::from(info[13] & 0x0f) << 32)
        | u64::from(u32::from_be_bytes(info[14..18].try_into()?));
    ensure!(sample_rate > 0, "FLAC sample rate missing");
    ensure!(
        samples > 0,
        "FLAC STREAMINFO does not give the total sample count"
    );
    let mut end = size;
    if size - position >= 128 {
        let marker = read_span(input, size - 128, 3)?;
        if marker == b"TAG" {
            end -= 128;
            tags.push(TagRegion {
                kind: TagKind::Id3v1,
                offset: end,
                bytes: 128,
            });
        }
    }
    ensure!(end - position >= 2, "FLAC has no audio frames");
    let sync = read_span(input, position, 2)?;
    ensure!(
        sync[0] == 0xff && sync[1] & 0xfe == 0xf8,
        "FLAC audio does not start with a frame"
    );
    let audio_bytes = end - position;
    let encoded = EncodedAudio {
        format: Format::Flac,
        sample_rate,
        channels,
        bits_per_sample: bits,
        packets: 0,
        coded_samples: samples,
        audio_offset: position,
        audio_bytes,
        config_sha256: Sha256::digest(&info).into(),
        audio_sha256: hash_span(input, position, audio_bytes)?,
    };
    Ok(Layout {
        stream: Stream {
            bit_rate_kbps: average_kbps(audio_bytes, samples, sample_rate)?,
            variable_bit_rate: true,
            encoder_delay: 0,
            encoder_padding: 0,
        },
        encoded,
        tags,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub(crate) fn streaminfo(rate: u32, channels: u8, bits: u8, samples: u64) -> Vec<u8> {
        let mut info = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
        info.push((rate >> 12) as u8);
        info.push((rate >> 4) as u8);
        info.push(((rate & 0xf) as u8) << 4 | ((channels - 1) << 1) | ((bits - 1) >> 4));
        info.push(((bits - 1) & 0xf) << 4 | ((samples >> 32) as u8 & 0xf));
        info.extend((samples as u32).to_be_bytes());
        info.extend([7; 16]);
        info
    }

    pub(crate) fn flac(comment: &[u8], padding: usize) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        out.extend([0, 0, 0, 34]);
        out.extend(streaminfo(44_100, 2, 16, 4096));
        out.push(4);
        out.extend(&(comment.len() as u32).to_be_bytes()[1..]);
        out.extend(comment);
        out.push(0x81);
        out.extend(&(padding as u32).to_be_bytes()[1..]);
        out.extend(vec![0; padding]);
        out.extend([0xff, 0xf8, 0x69, 0x08]);
        out.extend((0..400u32).map(|n| n as u8));
        out
    }

    #[test]
    fn reads_streaminfo_and_keeps_identity_across_tags_and_padding() {
        let a = flac(b"", 0);
        let b = flac(b"\x00\x00\x00\x00\x01\x00\x00\x00", 100);
        let la = inspect(&mut Cursor::new(&a), a.len() as u64).unwrap();
        let lb = inspect(&mut Cursor::new(&b), b.len() as u64).unwrap();
        assert_eq!(la.encoded.sha256(), lb.encoded.sha256());
        assert_eq!(
            (
                la.encoded.sample_rate,
                la.encoded.channels,
                la.encoded.bits_per_sample
            ),
            (44_100, 2, 16)
        );
        assert_eq!(la.encoded.coded_samples, 4096);
        assert_eq!(lb.tags[0].kind, TagKind::VorbisComment);
    }

    #[test]
    fn refuses_missing_frames_and_late_streaminfo() {
        let mut no_sync = flac(b"", 0);
        let at = no_sync.len() - 404;
        no_sync[at] = 0;
        assert!(inspect(&mut Cursor::new(&no_sync), no_sync.len() as u64).is_err());
        let mut late = b"fLaC".to_vec();
        late.extend([1, 0, 0, 0]);
        late.extend([0x80, 0, 0, 34]);
        late.extend(streaminfo(44_100, 2, 16, 4096));
        late.extend([0xff, 0xf8, 0, 0]);
        assert!(inspect(&mut Cursor::new(&late), late.len() as u64).is_err());
    }
}
