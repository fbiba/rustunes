//! Source timing declarations, not decoder output or Apple transport values.
//! The complete bounded frame scan is hash-bound to EncodedMpeg1Layer3. Fractions
//! retain sample counts/rate exactly; no rounded duration enters identity.
use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
};

const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FRAMES: u32 = 2_000_000;
const SAMPLES: u64 = 1152;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SampleDuration {
    pub samples: u64,
    pub sample_rate: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BitrateCount {
    pub kbps: u16,
    pub frames: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BitrateMode {
    Constant,
    Variable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FrameSummary {
    pub frame_count: u32,
    pub coded_duration: SampleDuration,
    pub bitrate_mode: BitrateMode,
    pub bitrates: Vec<BitrateCount>,
    /// Stereo, joint stereo, dual channel, mono respectively.
    pub channel_mode_counts: [u32; 4],
    pub crc_protected_frames: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TimingEvidence {
    pub schema: &'static str,
    pub frame_bytes_sha256: String,
    pub physical: FrameSummary,
    /// Only Xing/Info explicitly establishes a separate leading metadata frame.
    /// VBRI count conventions are reported without choosing a playback basis.
    pub excluding_xing_info_frame: Option<FrameSummary>,
    pub first_frame_bytes: u16,
    pub first_frame_sha256: String,
    pub header: Option<SeekHeader>,
    pub native_timing_qualified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeekHeader {
    Xing(XingInfo),
    Info(XingInfo),
    Vbri(Vbri),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct XingInfo {
    pub frame_offset: u16,
    pub flags: u32,
    /// This declaration excludes the leading Xing/Info frame; checked against
    /// the actual scan, when present. Missing is unknown, never zero.
    pub declared_audio_frames: Option<u32>,
    pub declared_stream_bytes: Option<u32>,
    pub toc: Option<Vec<u8>>,
    pub quality: Option<u32>,
    pub encoder: Option<EncoderTag>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EncoderTag {
    pub family: &'static str,
    pub identifier_hex: String,
    pub revision: u8,
    pub method: u8,
    pub delay_samples: u16,
    pub padding_samples: u16,
    /// Source tag arithmetic only, not decoded PCM or native gapless duration.
    pub declared_trimmed_duration: SampleDuration,
    pub raw_tag_sha256: String,
    pub crc_verified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VbriCountBasis {
    PhysicalFrames,
    ExcludingFirstFrame,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Vbri {
    pub frame_offset: u16,
    pub version: u16,
    pub delay_field: u16,
    pub quality: u16,
    pub declared_stream_bytes: u32,
    pub declared_frames: u32,
    pub count_basis: VbriCountBasis,
    pub table_entries: u16,
    pub table_scale: u16,
    pub bytes_per_entry: u16,
    pub frames_per_entry: u16,
    pub table_sha256: String,
    /// Scaled source declarations; aggregate coverage is checked, not the
    /// correspondence of each seek group to actual scanned frame offsets.
    pub table_byte_extents: Vec<u64>,
    pub table_offsets_verified: bool,
}

pub(super) struct FrameSpan {
    pub offset: u64,
    pub bytes: u64,
    pub frames: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub sha256: [u8; 32],
}
#[derive(Clone, Copy)]
struct Header {
    rate: u32,
    channels: u8,
    mode: u8,
    kbps: u16,
    bytes: usize,
    crc: bool,
}
fn header(b: &[u8]) -> Result<Header> {
    ensure!(b.len() >= 4, "truncated MPEG timing frame header");
    let n = u32::from_be_bytes(b[..4].try_into()?);
    ensure!(
        n >> 21 == 0x7ff && (n >> 19) & 3 == 3 && (n >> 17) & 3 == 1,
        "timing requires MPEG-1 Layer III frames"
    );
    let bitrate = ((n >> 12) & 15) as usize;
    let rate = ((n >> 10) & 3) as usize;
    ensure!(
        (1..=14).contains(&bitrate) && rate < 3 && n & 3 != 2,
        "unsupported MPEG timing header"
    );
    let kbps = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ][bitrate];
    let rate = [44100, 48000, 32000][rate];
    let mode = ((n >> 6) & 3) as u8;
    Ok(Header {
        rate,
        channels: if mode == 3 { 1 } else { 2 },
        mode,
        kbps,
        bytes: (144000 * u32::from(kbps) / rate + ((n >> 9) & 1)) as usize,
        crc: (n >> 16) & 1 == 0,
    })
}
#[derive(Default, Clone)]
struct Counts {
    frames: u32,
    rates: BTreeMap<u16, u32>,
    modes: [u32; 4],
    crc: u32,
}
impl Counts {
    fn add(&mut self, h: Header) {
        self.frames += 1;
        *self.rates.entry(h.kbps).or_default() += 1;
        self.modes[h.mode as usize] += 1;
        self.crc += u32::from(h.crc);
    }
    fn summary(self, sample_rate: u32) -> FrameSummary {
        FrameSummary {
            frame_count: self.frames,
            coded_duration: SampleDuration {
                samples: u64::from(self.frames) * SAMPLES,
                sample_rate,
            },
            bitrate_mode: if self.rates.len() == 1 {
                BitrateMode::Constant
            } else {
                BitrateMode::Variable
            },
            bitrates: self
                .rates
                .into_iter()
                .map(|(kbps, frames)| BitrateCount { kbps, frames })
                .collect(),
            channel_mode_counts: self.modes,
            crc_protected_frames: self.crc,
        }
    }
}
pub(super) fn inspect<R: Read + Seek>(reader: &mut R, span: FrameSpan) -> Result<TimingEvidence> {
    ensure!(
        (1..=MAX_BYTES).contains(&span.bytes) && (2..=MAX_FRAMES).contains(&span.frames),
        "MPEG timing scan exceeds bound"
    );
    let end = span
        .offset
        .checked_add(span.bytes)
        .ok_or_else(|| anyhow::anyhow!("MPEG extent overflow"))?;
    let mut offset = span.offset;
    let mut all = Counts::default();
    let mut rest = Counts::default();
    let mut first = Vec::new();
    let mut hash = Sha256::new();
    // MPEG-1 Layer III max: 320 kbps / 32 kHz + one padding byte.
    let mut frame = [0u8; 1441];
    while offset < end {
        ensure!(
            all.frames < span.frames && end - offset >= 4,
            "MPEG timing frame count/extent differs"
        );
        reader.seek(SeekFrom::Start(offset))?;
        reader.read_exact(&mut frame[..4])?;
        let h = header(&frame[..4])?;
        ensure!(
            h.rate == span.sample_rate
                && h.channels == span.channels
                && h.bytes <= frame.len()
                && h.bytes as u64 <= end - offset,
            "MPEG timing frame format/extent differs"
        );
        reader.read_exact(&mut frame[4..h.bytes])?;
        hash.update(&frame[..h.bytes]);
        if all.frames == 0 {
            first.extend_from_slice(&frame[..h.bytes]);
        } else {
            rest.add(h);
        }
        all.add(h);
        offset += h.bytes as u64;
    }
    let actual: [u8; 32] = hash.finalize().into();
    ensure!(
        all.frames == span.frames && actual == span.sha256,
        "MPEG timing bytes/count differ from inspected identity"
    );
    let parsed = parse_first(&first, &span)?;
    let has_xing = matches!(&parsed, Some(SeekHeader::Xing(_) | SeekHeader::Info(_)));
    if matches!(&parsed, Some(SeekHeader::Info(_))) {
        ensure!(
            rest.rates.len() == 1,
            "Info constant-bitrate declaration contradicts audio frames"
        );
    }
    Ok(TimingEvidence {
        schema: "music-protocol.mpeg-source-timing",
        frame_bytes_sha256: super::hex(&actual),
        physical: all.summary(span.sample_rate),
        excluding_xing_info_frame: has_xing.then(|| rest.summary(span.sample_rate)),
        first_frame_bytes: first.len() as u16,
        first_frame_sha256: super::hex(&Sha256::digest(&first)),
        header: parsed,
        native_timing_qualified: false,
    })
}
fn marker(b: &[u8], at: usize) -> Option<&[u8]> {
    b.get(at..at + 4)
}
fn parse_first(b: &[u8], span: &FrameSpan) -> Result<Option<SeekHeader>> {
    let h = header(b)?;
    let at = 4 + if h.channels == 1 { 17 } else { 32 };
    let mut positions = vec![at];
    // Some CRC-protected encoders include the two CRC bytes before side info.
    // Retain the observed position, refuse ambiguous dual headers.
    if h.crc {
        positions.push(at + 2);
    }
    let xing: Vec<_> = positions
        .into_iter()
        .filter(|p| matches!(marker(b, *p), Some(b"Xing" | b"Info")))
        .collect();
    let vbri = marker(b, 36) == Some(b"VBRI");
    ensure!(
        xing.len() + usize::from(vbri) <= 1,
        "ambiguous MPEG seek headers"
    );
    if let Some(at) = xing.first().copied() {
        let info = marker(b, at) == Some(b"Info");
        let parsed = parse_xing(b, at, span)?;
        Ok(Some(if info {
            SeekHeader::Info(parsed)
        } else {
            SeekHeader::Xing(parsed)
        }))
    } else if vbri {
        Ok(Some(SeekHeader::Vbri(parse_vbri(b, span)?)))
    } else {
        Ok(None)
    }
}
struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}
impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let out = self
            .b
            .get(
                self.at
                    ..self
                        .at
                        .checked_add(n)
                        .ok_or_else(|| anyhow::anyhow!("MPEG tag overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("truncated MPEG timing tag"))?;
        self.at += n;
        Ok(out)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
}
fn parse_xing(b: &[u8], at: usize, span: &FrameSpan) -> Result<XingInfo> {
    let mut r = Bytes { b, at: at + 4 };
    let flags = r.u32()?;
    ensure!(flags & !15 == 0, "unsupported Xing/Info flags");
    let frames = if flags & 1 != 0 { Some(r.u32()?) } else { None };
    let bytes = if flags & 2 != 0 { Some(r.u32()?) } else { None };
    ensure!(
        frames.is_none_or(|n| n == span.frames - 1),
        "Xing/Info frame count contradicts complete scan"
    );
    ensure!(
        bytes.is_none_or(|n| u64::from(n) == span.bytes),
        "Xing/Info stream size contradicts complete scan"
    );
    let toc = if flags & 4 != 0 {
        let toc = r.take(100)?.to_vec();
        ensure!(
            toc.windows(2).all(|p| p[0] <= p[1]),
            "Xing/Info TOC is not ordered"
        );
        Some(toc)
    } else {
        None
    };
    let quality = if flags & 8 != 0 { Some(r.u32()?) } else { None };
    let family = match b.get(r.at..r.at + 4) {
        Some(b"LAME") => Some("LAME"),
        Some(b"Lavf") => Some("Lavf"),
        Some(b"Lavc") => Some("Lavc"),
        _ => None,
    };
    let encoder = if let Some(family) = family {
        let tag = r.take(36)?;
        let revision = tag[9] >> 4;
        ensure!(revision == 0, "unsupported MPEG encoder tag revision");
        let delay = (u16::from(tag[21]) << 4) | u16::from(tag[22] >> 4);
        let padding = (u16::from(tag[22] & 15) << 8) | u16::from(tag[23]);
        let samples = u64::from(span.frames - 1) * SAMPLES;
        let trim = u64::from(delay) + u64::from(padding);
        ensure!(
            trim < samples,
            "MPEG encoder trim exceeds audio sample count"
        );
        Some(EncoderTag {
            family,
            identifier_hex: super::hex(&tag[..9]),
            revision,
            method: tag[9] & 15,
            delay_samples: delay,
            padding_samples: padding,
            declared_trimmed_duration: SampleDuration {
                samples: samples - trim,
                sample_rate: span.sample_rate,
            },
            raw_tag_sha256: super::hex(&Sha256::digest(tag)),
            crc_verified: false,
        })
    } else {
        None
    };
    Ok(XingInfo {
        frame_offset: at as u16,
        flags,
        declared_audio_frames: frames,
        declared_stream_bytes: bytes,
        toc,
        quality,
        encoder,
    })
}
fn parse_vbri(b: &[u8], span: &FrameSpan) -> Result<Vbri> {
    let mut r = Bytes { b, at: 40 };
    let version = r.u16()?;
    ensure!(version == 1, "unsupported VBRI version");
    let delay_field = r.u16()?;
    let quality = r.u16()?;
    let bytes = r.u32()?;
    let frames = r.u32()?;
    ensure!(
        u64::from(bytes) == span.bytes,
        "VBRI stream size contradicts complete scan"
    );
    let count_basis = if frames == span.frames {
        VbriCountBasis::PhysicalFrames
    } else {
        ensure!(
            frames == span.frames - 1,
            "VBRI frame count contradicts complete scan"
        );
        VbriCountBasis::ExcludingFirstFrame
    };
    let entries = r.u16()?;
    let scale = r.u16()?;
    let entry_bytes = r.u16()?;
    let entry_frames = r.u16()?;
    ensure!(
        entries > 0 && scale > 0 && (1..=4).contains(&entry_bytes) && entry_frames > 0,
        "invalid VBRI seek table geometry"
    );
    ensure!(
        frames.div_ceil(u32::from(entry_frames)) == u32::from(entries),
        "VBRI seek table frame coverage differs"
    );
    let table = r.take(usize::from(entries) * usize::from(entry_bytes))?;
    // Each entry is a scaled byte extent, not an absolute offset. Do not claim
    // sample trimming from VBRI's opaque delay field.
    let extents: Vec<u64> = table
        .chunks_exact(usize::from(entry_bytes))
        .map(|entry| entry.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b)) * u64::from(scale))
        .collect();
    ensure!(
        scale != 1 || extents.iter().all(|bytes| *bytes > 0),
        "VBRI unit-scale seek table declares an empty frame group"
    );
    let sum: u64 = extents.iter().sum();
    ensure!(
        sum > 0 && sum <= span.bytes && span.bytes - sum < u64::from(entries) * u64::from(scale),
        "VBRI seek table byte coverage differs"
    );
    Ok(Vbri {
        frame_offset: 36,
        version,
        delay_field,
        quality,
        declared_stream_bytes: bytes,
        declared_frames: frames,
        count_basis,
        table_entries: entries,
        table_scale: scale,
        bytes_per_entry: entry_bytes,
        frames_per_entry: entry_frames,
        table_sha256: super::hex(&Sha256::digest(table)),
        table_byte_extents: extents,
        table_offsets_verified: false,
    })
}

#[cfg(test)]
mod tests;
