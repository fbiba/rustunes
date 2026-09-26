//! MP4 audio other than ALAC: AAC-LC, HE-AAC (v1, v2) and xHE-AAC in an `mp4a`
//! sample description. The file must hold one self-contained audio track
//! whose chunks cover one `mdat` payload in order, as for ALAC. The identity
//! covers the sample description, sample times and sizes, and the payload;
//! tags live in `moov/udta/meta/ilst`. An edit list gives the encoder delay
//! and padding.
use std::io::{Read, Seek};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::{Layout, Stream, TagKind, TagRegion, average_kbps};
use crate::media::{
    fingerprint::{
        EncodedAudio, MAX_SAMPLES, atoms, bounded_payload, children, hash_file_and_media, one,
        read_at, sample_extent, self_contained, u32_at,
    },
    format::Format,
};

const MAX_TIME_ENTRIES: u64 = 65_536;

/// `None` for an ALAC track, which keeps its own reader and identity.
pub fn inspect(input: &mut (impl Read + Seek), size: u64) -> Result<Option<Layout>> {
    let top = atoms(input, 0, size)?;
    ensure!(
        !top.iter().any(|a| &a.kind == b"moof"),
        "fragmented MP4 is not supported"
    );
    let moov = one(&top, b"moov")?;
    let media = one(&top, b"mdat")?;
    ensure!(media.len() > 0, "empty MP4 media payload");
    let movie = children(input, moov, 0)?;
    ensure!(
        !movie.iter().any(|a| &a.kind == b"mvex"),
        "fragmented MP4 is not supported"
    );
    let (movie_scale, _) = media_header(&bounded_payload(input, one(&movie, b"mvhd")?, 1024)?)?;
    let trak = children(input, one(&movie, b"trak")?, 0)?;
    let mdia = children(input, one(&trak, b"mdia")?, 0)?;
    let handler = one(&mdia, b"hdlr")?;
    ensure!(
        read_at::<4>(input, handler, 8)? == *b"soun",
        "the MP4 track is not audio"
    );
    let (timescale, duration) = media_header(&bounded_payload(input, one(&mdia, b"mdhd")?, 1024)?)?;
    let minf = children(input, one(&mdia, b"minf")?, 0)?;
    self_contained(input, &minf)?;
    let table = children(input, one(&minf, b"stbl")?, 0)?;
    ensure!(
        !table.iter().any(|a| &a.kind == b"ctts"),
        "composition offsets are not supported"
    );
    let stsd = one(&table, b"stsd")?;
    ensure!(
        read_at::<8>(input, stsd, 0)? == [0, 0, 0, 0, 0, 0, 0, 1],
        "the MP4 track needs exactly one sample description"
    );
    let entries = children(input, stsd, 8)?;
    ensure!(entries.len() == 1, "ambiguous MP4 sample descriptions");
    let entry = entries[0];
    match &entry.kind {
        b"alac" => return Ok(None),
        b"mp4a" => {}
        b"enca" | b"drms" => bail!("protected MP4 audio is not supported"),
        other => bail!(
            "MP4 audio {:?} is not one the phone syncs (AAC, HE-AAC, xHE-AAC or ALAC)",
            String::from_utf8_lossy(other)
        ),
    }
    let header = read_at::<28>(input, entry, 0)?;
    ensure!(
        header[..6] == [0; 6] && header[6..8] == [0, 1],
        "MP4 audio references another data source"
    );
    ensure!(
        header[8..10] == [0, 0],
        "unsupported MP4 sound description version"
    );
    let entry_channels = u16::from_be_bytes([header[16], header[17]]);
    let descriptors = children(input, entry, 28)?;
    let esds = bounded_payload(input, one(&descriptors, b"esds")?, 4096)?;
    let config = decoder_config(&esds)?;
    let audio = audio_specific_config(&config.specific)?;
    let channels = audio.channels.unwrap_or(entry_channels);
    ensure!(
        (1..=8).contains(&channels),
        "unsupported AAC channel count {channels}"
    );

    let times = bounded_payload(input, one(&table, b"stts")?, 8 + MAX_TIME_ENTRIES * 8)?;
    let (ticks, packets) = time_table(&times)?;
    ensure!(
        ticks == duration,
        "MP4 sample times disagree with the track duration"
    );
    let sizes = bounded_payload(
        input,
        one(&table, b"stsz")?,
        12 + u64::from(MAX_SAMPLES) * 4,
    )?;
    ensure!(
        sizes.len() >= 12 && sizes[..4] == [0; 4],
        "unsupported MP4 sample-size table"
    );
    let fixed_size = u32_at(&sizes, 4)?;
    let sample_count = u32_at(&sizes, 8)?;
    ensure!(
        u64::from(sample_count) == packets && packets > 0,
        "MP4 sample count disagrees with its times"
    );
    ensure!(
        sizes.len() as u64
            == 12
                + if fixed_size == 0 {
                    u64::from(sample_count) * 4
                } else {
                    0
                },
        "MP4 sample-size table length mismatch"
    );
    // An encoder may leave bytes in `mdat` that no sample uses (exhale writes
    // two before its first); they stay outside the identity, like tags.
    let media = sample_extent(input, &table, media, &sizes, fixed_size, sample_count)?;

    let sample_rate = audio.output_rate;
    let to_samples = |ticks: u64| -> Result<u64> {
        let scaled = u128::from(ticks) * u128::from(sample_rate);
        ensure!(
            scaled % u128::from(timescale) == 0,
            "MP4 time scale {timescale} does not divide into {sample_rate} Hz samples"
        );
        Ok(u64::try_from(scaled / u128::from(timescale))?)
    };
    let coded_samples = to_samples(ticks)?;
    let (encoder_delay, encoder_padding) = if trak.iter().any(|a| &a.kind == b"edts") {
        let edts = children(input, one(&trak, b"edts")?, 0)?;
        let elst = bounded_payload(input, one(&edts, b"elst")?, 4096)?;
        let (span, start) = edit(&elst)?;
        let delay = to_samples(start)?;
        let shown = u128::from(span) * u128::from(sample_rate);
        let valid = u64::try_from((shown + u128::from(movie_scale) / 2) / u128::from(movie_scale))?;
        let held = coded_samples
            .checked_sub(delay)
            .context("the MP4 edit list starts after the track ends")?;
        // A coarse movie time scale (ffmpeg's is milliseconds) can round the
        // span past the last sample by less than one of its ticks.
        let tick = u64::from(sample_rate.div_ceil(movie_scale));
        ensure!(
            valid <= held + tick,
            "the MP4 edit list shows more than the track holds"
        );
        (delay, held.saturating_sub(valid))
    } else {
        (0, 0)
    };

    let (_, audio_sha256) = hash_file_and_media(input, size, media)?;
    // The configuration is what a decoder reads: the AudioSpecificConfig, as
    // ALAC's identity takes its magic cookie. A remux rewrites the esds bit
    // rate fields, which decode nothing.
    let mut digest = Sha256::new();
    for part in [&entry.kind[..], &config.specific, &times, &sizes] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let encoded = EncodedAudio {
        format: audio.format,
        sample_rate,
        channels: u8::try_from(channels)?,
        bits_per_sample: 0,
        packets,
        coded_samples,
        audio_offset: media.start,
        audio_bytes: media.len(),
        config_sha256: digest.finalize().into(),
        audio_sha256,
    };
    let bit_rate_kbps = if config.average_bit_rate > 0 {
        (config.average_bit_rate + 500) / 1000
    } else {
        average_kbps(media.len(), coded_samples, sample_rate)?
    };
    Ok(Some(Layout {
        encoded,
        stream: Stream {
            bit_rate_kbps,
            variable_bit_rate: config.average_bit_rate == 0
                || config.max_bit_rate != config.average_bit_rate,
            encoder_delay,
            encoder_padding,
        },
        tags: vec![TagRegion {
            kind: TagKind::Mp4Movie,
            offset: moov.start,
            bytes: moov.len(),
        }],
    }))
}

/// Time scale and duration of an `mvhd` or `mdhd` payload.
fn media_header(bytes: &[u8]) -> Result<(u32, u64)> {
    ensure!(
        bytes.len() >= 4 && bytes[1..4] == [0; 3],
        "unsupported MP4 time header"
    );
    let (scale, ticks) = match bytes[0] {
        0 => {
            ensure!(bytes.len() >= 20, "truncated MP4 time header");
            (u32_at(bytes, 12)?, u64::from(u32_at(bytes, 16)?))
        }
        1 => {
            ensure!(bytes.len() >= 32, "truncated MP4 time header");
            (
                u32_at(bytes, 20)?,
                u64::from_be_bytes(bytes[24..32].try_into()?),
            )
        }
        other => bail!("unsupported MP4 time header version {other}"),
    };
    ensure!(scale > 0 && ticks > 0, "MP4 track has no duration");
    Ok((scale, ticks))
}

/// Total ticks and samples of an `stts` payload.
fn time_table(bytes: &[u8]) -> Result<(u64, u64)> {
    ensure!(
        bytes.len() >= 8 && bytes[..4] == [0; 4],
        "unsupported MP4 sample times"
    );
    let entries = u64::from(u32_at(bytes, 4)?);
    ensure!(
        (1..=MAX_TIME_ENTRIES).contains(&entries) && bytes.len() as u64 == 8 + entries * 8,
        "MP4 sample-time table length mismatch"
    );
    let (mut ticks, mut samples) = (0u64, 0u64);
    for index in 0..entries as usize {
        let count = u64::from(u32_at(bytes, 8 + index * 8)?);
        let delta = u64::from(u32_at(bytes, 12 + index * 8)?);
        ensure!(count > 0 && delta > 0, "zero MP4 sample time");
        ticks = ticks
            .checked_add(count * delta)
            .context("MP4 duration overflow")?;
        samples += count;
    }
    Ok((ticks, samples))
}

/// The one edit of an `elst` payload: its span (movie time scale) and where it
/// starts in the media (media time scale). Empty edits and rates are refused.
fn edit(bytes: &[u8]) -> Result<(u64, u64)> {
    ensure!(
        bytes.len() >= 8 && bytes[1..4] == [0; 3],
        "unsupported MP4 edit list"
    );
    ensure!(
        u32_at(bytes, 4)? == 1,
        "MP4 edit lists other than one edit are not supported"
    );
    let (span, start, rate) = match bytes[0] {
        0 if bytes.len() == 20 => (
            u64::from(u32_at(bytes, 8)?),
            i64::from(i32::from_be_bytes(bytes[12..16].try_into()?)),
            u32_at(bytes, 16)?,
        ),
        1 if bytes.len() == 28 => (
            u64::from_be_bytes(bytes[8..16].try_into()?),
            i64::from_be_bytes(bytes[16..24].try_into()?),
            u32_at(bytes, 24)?,
        ),
        _ => bail!("unsupported MP4 edit list"),
    };
    ensure!(
        start >= 0 && span > 0 && rate == 0x0001_0000,
        "empty or rate-changing MP4 edits are not supported"
    );
    Ok((span, start as u64))
}

struct DecoderConfig {
    max_bit_rate: u32,
    average_bit_rate: u32,
    specific: Vec<u8>,
}

/// The `DecoderConfigDescriptor` of an `esds` payload.
fn decoder_config(esds: &[u8]) -> Result<DecoderConfig> {
    ensure!(
        esds.len() >= 4 && esds[..4] == [0; 4],
        "unsupported esds version"
    );
    let (tag, es, _) = descriptor(&esds[4..])?;
    ensure!(tag == 3, "esds holds no ES descriptor");
    ensure!(es.len() >= 3, "truncated ES descriptor");
    let flags = es[2];
    let mut at = 3;
    if flags & 0x80 != 0 {
        at += 2;
    }
    if flags & 0x40 != 0 {
        at += 1 + usize::from(*es.get(at).context("truncated ES descriptor")?);
    }
    if flags & 0x20 != 0 {
        at += 2;
    }
    let (tag, config, _) = descriptor(es.get(at..).context("truncated ES descriptor")?)?;
    ensure!(
        tag == 4 && config.len() >= 13,
        "esds holds no decoder configuration"
    );
    ensure!(
        config[0] == 0x40 && config[1] >> 2 == 5,
        "the MP4 stream is not MPEG-4 audio"
    );
    let (tag, specific, _) = descriptor(&config[13..])?;
    ensure!(tag == 5, "esds holds no audio specific configuration");
    Ok(DecoderConfig {
        max_bit_rate: u32::from_be_bytes(config[5..9].try_into()?),
        average_bit_rate: u32::from_be_bytes(config[9..13].try_into()?),
        specific: specific.to_vec(),
    })
}

/// One MPEG-4 descriptor: its tag, payload and the bytes after it.
fn descriptor(bytes: &[u8]) -> Result<(u8, &[u8], &[u8])> {
    let tag = *bytes.first().context("truncated descriptor")?;
    let mut length = 0usize;
    let mut at = 1;
    loop {
        let byte = *bytes.get(at).context("truncated descriptor length")?;
        length = (length << 7) | usize::from(byte & 0x7f);
        at += 1;
        if byte & 0x80 == 0 {
            break;
        }
        ensure!(at < 5, "descriptor length too long");
    }
    let payload = bytes
        .get(at..at + length)
        .context("descriptor exceeds its parent")?;
    Ok((tag, payload, &bytes[at + length..]))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AudioConfig {
    pub format: Format,
    /// What a decoder puts out (twice the core rate with SBR).
    pub output_rate: u32,
    /// From the channel configuration; `None` where it does not say.
    pub channels: Option<u16>,
}

const RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self
                .bytes
                .get(self.at / 8)
                .context("truncated audio specific configuration")?;
            value = (value << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }
    fn left(&self) -> usize {
        (self.bytes.len() * 8).saturating_sub(self.at)
    }
    fn object_type(&mut self) -> Result<u32> {
        let aot = self.read(5)?;
        Ok(if aot == 31 { 32 + self.read(6)? } else { aot })
    }
    fn rate(&mut self, index_bits: usize) -> Result<u32> {
        let index = self.read(index_bits)?;
        if index == (1 << index_bits) - 1 {
            return self.read(24);
        }
        RATES
            .get(usize::try_from(index)?)
            .copied()
            .context("reserved sampling frequency index")
    }
}

const CHANNELS: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 8];

/// The parts of an AudioSpecificConfig (ISO/IEC 14496-3 1.6.2.1) that name
/// the format, output rate and channels.
pub(crate) fn audio_specific_config(bytes: &[u8]) -> Result<AudioConfig> {
    let mut bits = Bits { bytes, at: 0 };
    let mut aot = bits.object_type()?;
    let core_rate = bits.rate(4)?;
    let channel_config = bits.read(4)?;
    let channels = CHANNELS
        .get(usize::try_from(channel_config)?)
        .copied()
        .filter(|c| *c > 0);
    if aot == 42 {
        // UsacConfig repeats the rate (5-bit index) and the channel layout.
        let rate = match bits.read(5)? {
            0x1f => bits.read(24)?,
            index => RATES
                .get(usize::try_from(index)?)
                .copied()
                .context("unsupported USAC sampling frequency index")?,
        };
        let _core_sbr_frame_length = bits.read(3)?;
        let layout = bits.read(5)?;
        return Ok(AudioConfig {
            format: Format::Usac,
            output_rate: rate,
            channels: CHANNELS
                .get(usize::try_from(layout)?)
                .copied()
                .filter(|c| *c > 0)
                .or(channels),
        });
    }
    let mut sbr = None;
    let mut ps = false;
    if aot == 5 || aot == 29 {
        ps = aot == 29;
        sbr = Some(bits.rate(4)?);
        aot = bits.object_type()?;
    }
    ensure!(
        aot == 2,
        "AAC object type {aot} is not one the phone plays (AAC-LC, HE-AAC, HE-AAC v2 or xHE-AAC)"
    );
    ensure!(
        channel_config != 0,
        "AAC with a program configuration element is not supported"
    );
    // GASpecificConfig for AAC-LC.
    let _frame_length = bits.read(1)?;
    if bits.read(1)? == 1 {
        bits.read(14)?;
    }
    let _extension = bits.read(1)?;
    // Backward-compatible SBR / PS signalling.
    if sbr.is_none() && bits.left() >= 16 && bits.read(11)? == 0x2b7 {
        if bits.object_type()? == 5 && bits.read(1)? == 1 {
            sbr = Some(bits.rate(4)?);
            if bits.left() >= 12 && bits.read(11)? == 0x548 {
                ps = bits.read(1)? == 1;
            }
        }
    }
    Ok(match sbr {
        None => AudioConfig {
            format: Format::AacLc,
            output_rate: core_rate,
            channels,
        },
        Some(rate) => AudioConfig {
            format: if ps { Format::HeAacV2 } else { Format::HeAac },
            output_rate: if rate == core_rate {
                core_rate * 2
            } else {
                rate
            },
            channels: if ps { Some(2) } else { channels },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(fields: &[(u32, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0;
        for &(value, count) in fields {
            for bit in (0..count).rev() {
                if at % 8 == 0 {
                    out.push(0);
                }
                *out.last_mut().unwrap() |= (((value >> bit) & 1) as u8) << (7 - at % 8);
                at += 1;
            }
        }
        out
    }

    #[test]
    fn audio_specific_configs() {
        // AAC-LC 44.1 kHz stereo: 0x12 0x10.
        assert_eq!(
            audio_specific_config(&[0x12, 0x10]).unwrap(),
            AudioConfig {
                format: Format::AacLc,
                output_rate: 44_100,
                channels: Some(2)
            }
        );
        // Explicit HE-AAC: SBR at 44.1 kHz over a 22.05 kHz core.
        let he = bits(&[(5, 5), (7, 4), (2, 4), (4, 4), (2, 5), (0, 3)]);
        assert_eq!(
            audio_specific_config(&he).unwrap(),
            AudioConfig {
                format: Format::HeAac,
                output_rate: 44_100,
                channels: Some(2)
            }
        );
        // Explicit HE-AAC v2: mono core, two output channels.
        let v2 = bits(&[(29, 5), (7, 4), (1, 4), (4, 4), (2, 5), (0, 3)]);
        assert_eq!(audio_specific_config(&v2).unwrap().format, Format::HeAacV2);
        assert_eq!(audio_specific_config(&v2).unwrap().channels, Some(2));
        // Backward-compatible signalling after an AAC-LC core.
        let compatible = bits(&[
            (2, 5),
            (7, 4),
            (2, 4),
            (0, 3),
            (0x2b7, 11),
            (5, 5),
            (1, 1),
            (4, 4),
        ]);
        let config = audio_specific_config(&compatible).unwrap();
        assert_eq!((config.format, config.output_rate), (Format::HeAac, 44_100));
        // xHE-AAC: USAC at 48 kHz stereo.
        let usac = bits(&[(31, 5), (10, 6), (3, 4), (2, 4), (3, 5), (1, 3), (2, 5)]);
        let config = audio_specific_config(&usac).unwrap();
        assert_eq!(
            (config.format, config.output_rate, config.channels),
            (Format::Usac, 48_000, Some(2))
        );
        // AAC Main and AAC-LD are refused.
        assert!(audio_specific_config(&bits(&[(1, 5), (4, 4), (2, 4), (0, 3)])).is_err());
        assert!(audio_specific_config(&bits(&[(23, 5), (4, 4), (2, 4), (0, 3)])).is_err());
    }

    #[test]
    fn esds_bit_rates_stay_outside_the_identity() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tone-aac.m4a");
        let original = std::fs::read(path).unwrap();
        let read = |bytes: &[u8]| {
            let mut input = std::io::Cursor::new(bytes);
            inspect(&mut input, bytes.len() as u64)
                .unwrap()
                .unwrap()
                .encoded
        };
        // Object type 0x40, stream type 0x15, buffer size, then the bit rates.
        let esds = original.windows(4).position(|w| w == b"esds").unwrap();
        let config = esds
            + original[esds..]
                .windows(2)
                .position(|w| w == [0x40, 0x15])
                .unwrap();
        let mut remuxed = original.clone();
        remuxed[config + 5..config + 13].copy_from_slice(&[0, 3, 0x4f, 0x7a, 0, 3, 0x4f, 0x7a]);
        assert_ne!(remuxed, original);
        assert_eq!(read(&remuxed), read(&original));
    }

    #[test]
    fn unused_media_bytes_stay_outside_the_identity() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tone-aac.m4a");
        let original = std::fs::read(path).unwrap();
        let read = |bytes: &[u8]| {
            let mut input = std::io::Cursor::new(bytes);
            inspect(&mut input, bytes.len() as u64)
                .unwrap()
                .unwrap()
                .encoded
        };
        let u32_at =
            |bytes: &[u8], at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        // Two bytes before the first chunk, as exhale writes, and three after
        // the last: `mdat` grows by five and every chunk moves by two.
        let mdat = original.windows(4).position(|w| w == b"mdat").unwrap() - 4;
        let size = u32_at(&original, mdat) as usize;
        let mut padded = original[..mdat + 8].to_vec();
        padded.extend([0x7a, 0x32]);
        padded.extend(&original[mdat + 8..mdat + size]);
        padded.extend([0, 0, 0]);
        padded.extend(&original[mdat + size..]);
        padded[mdat..mdat + 4].copy_from_slice(&(size as u32 + 5).to_be_bytes());
        let stco = padded.windows(4).position(|w| w == b"stco").unwrap() + 4;
        for entry in 0..u32_at(&padded, stco + 4) as usize {
            let at = stco + 8 + entry * 4;
            let moved = u32_at(&padded, at) + 2;
            padded[at..at + 4].copy_from_slice(&moved.to_be_bytes());
        }
        let (before, after) = (read(&original), read(&padded));
        assert_eq!(after.audio_offset, before.audio_offset + 2);
        assert_eq!(after.sha256(), before.sha256());
    }

    #[test]
    fn edits_and_descriptors() {
        let mut v0 = vec![0, 0, 0, 0, 0, 0, 0, 1];
        v0.extend(441_000u32.to_be_bytes());
        v0.extend(1024u32.to_be_bytes());
        v0.extend(0x0001_0000u32.to_be_bytes());
        assert_eq!(edit(&v0).unwrap(), (441_000, 1024));
        let mut empty = v0.clone();
        empty[12..16].copy_from_slice(&(-1i32).to_be_bytes());
        assert!(edit(&empty).is_err());
        let (tag, payload, rest) = descriptor(&[5, 0x80, 0x80, 0x02, 0x12, 0x10, 6]).unwrap();
        assert_eq!((tag, payload, rest), (5, &[0x12, 0x10][..], &[6][..]));
    }
}
