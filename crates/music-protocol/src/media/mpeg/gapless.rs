//! The timing and gapless fields Apple's sync sends for an MPEG Layer III file,
//! computed as AMPDevicesAgent 1.6.4.90 computes them. The frame count, display
//! time and bit rate come from the frame headers and a Xing or VBRI header. The
//! encoder delay, drain and played duration come from an `iTunSMPB` comment, an
//! accepted LAME tag or an `SMPB` frame, else from the encoder the tag names
//! and a search of the decoded last frames for where the sound stops.
//! Fraunhofer's CRC-protected `0xB4` marker is not read; a file with one goes
//! by the search.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};
use symphonia_core::{
    audio::SampleBuffer,
    codecs::{CODEC_TYPE_MP3, CodecParameters, Decoder, DecoderOptions},
    formats::Packet,
};

const MAX_FRAMES: usize = 4_000_000;
/// The search reads the last 4608 decoded sample frames (`0x14043d559`).
const TAIL_SAMPLES: usize = 4608;
/// Heuristic word of the decoder-analysis path (`0x14043dabe`).
const ANALYSED: u32 = 0x0200_0003;

/// What Apple's rule reads from an ID3v2 tag besides the catalogue fields
/// (`0x140436bc8`, `0x14043686c`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EncoderHints {
    /// The last TENC or TSSE text; before one, the first comment's
    /// description, or its text when it has none. At most 255 bytes.
    pub text: String,
    /// A comment described `iTunes_CDDB_1`, `iTunes_CDDB_IDs` or
    /// `iTunes_CDDB_TrackNumber`, or a comment text that became `text`
    /// starts `MusicMatch` or `Amazon.com`: the file is a CD rip.
    pub cd_rip: bool,
    /// The text of a comment described `iTunSMPB`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub itunsmpb: Option<String>,
}

impl EncoderHints {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// TENC and TSSE replace the text.
    pub(crate) fn encoder(&mut self, text: &str) {
        self.text = truncated(text).to_owned();
    }

    /// A comment frame, in tag order.
    pub(crate) fn comment(&mut self, description: &str, text: &str) {
        if matches!(
            description,
            "iTunes_CDDB_1" | "iTunes_CDDB_IDs" | "iTunes_CDDB_TrackNumber"
        ) {
            self.cd_rip = true;
        }
        if description == "iTunSMPB" {
            self.itunsmpb = Some(text.to_owned());
        }
        if self.text.is_empty() {
            let stored = if description.is_empty() {
                text
            } else {
                description
            };
            if stored.starts_with("MusicMatch") || stored.starts_with("Amazon.com") {
                self.cd_rip = true;
            }
            self.text = truncated(stored).to_owned();
        }
    }
}

/// The agent keeps the text in a 255-byte Pascal string.
fn truncated(text: &str) -> &str {
    let mut end = text.len().min(255);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Samples the encoder put before (`delay`) and after (`drain`) the audio,
/// how they were found (`heuristic`) and where the eighth frame from the end
/// starts (`resynch`, an MP3's only). An insert sends them as its gapless
/// fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Gapless {
    pub delay: u64,
    pub drain: u64,
    pub heuristic: u32,
    pub resynch: u64,
}

impl Gapless {
    /// A stream with no encoder trim, as Apple Devices sends ALAC.
    pub const NONE: Self = Self {
        delay: 0,
        drain: 0,
        heuristic: 1,
        resynch: 0,
    };
}

/// What Apple's sync sends for one MPEG Layer III file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppleTiming {
    /// The frames Apple counts: a Xing or VBRI header's count when it
    /// differs from the byte estimate, else the estimate when it is within
    /// 0.05 of a whole number, else every frame.
    pub frames: u32,
    pub samples_per_frame: u32,
    /// `item.total_time_ms`: every counted frame's samples.
    pub total_time_ms: u64,
    /// `avformat_info.bit_rate`: the average with a Xing or VBRI header, else
    /// the first frame's rate.
    pub bit_rate_kbps: u32,
    /// `avformat_info.duration`: the samples a player plays.
    pub duration: u64,
    pub delay: u32,
    pub drain: u32,
    /// `gapless_last_frame_resynch`: where the eighth frame from the end
    /// starts, in bytes from the first frame.
    pub resynch: u64,
    /// `gapless_heuristic_info`.
    pub heuristic: u32,
    /// Where delay and drain came from: `itunsmpb`, `lame_tag`, `smpb_frame`
    /// or `analysis` (the encoder rule and the tail search).
    pub source: &'static str,
}

#[derive(Clone, Copy)]
struct Header {
    mpeg1: bool,
    kbps: u32,
    rate: u32,
    channels: u8,
    /// Side information bytes, with the CRC (`0x14041653c`).
    side: usize,
    bytes: u64,
}

impl Header {
    fn samples(&self) -> u32 {
        if self.mpeg1 { 1152 } else { 576 }
    }
}

fn header(b: [u8; 4]) -> Option<Header> {
    let n = u32::from_be_bytes(b);
    let version = (n >> 19) & 3;
    let bitrate = ((n >> 12) & 15) as usize;
    let rate = ((n >> 10) & 3) as usize;
    if n >> 21 != 0x7ff
        || version == 1
        || (n >> 17) & 3 != 1
        || !(1..=14).contains(&bitrate)
        || rate == 3
    {
        return None;
    }
    let mpeg1 = version == 3;
    let kbps = if mpeg1 {
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ][bitrate]
    } else {
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160][bitrate]
    };
    let rate = match version {
        3 => [44100, 48000, 32000][rate],
        2 => [22050, 24000, 16000][rate],
        _ => [11025, 12000, 8000][rate],
    };
    let channels = if (n >> 6) & 3 == 3 { 1 } else { 2 };
    let side = match (mpeg1, channels) {
        (true, 1) => 17,
        (true, _) => 32,
        (false, 1) => 9,
        (false, _) => 17,
    } + if (n >> 16) & 1 == 0 { 2 } else { 0 };
    let scale = if mpeg1 { 144_000 } else { 72_000 };
    Some(Header {
        mpeg1,
        kbps,
        rate,
        channels,
        side,
        bytes: u64::from(scale * kbps / rate + ((n >> 9) & 1)),
    })
}

/// The first frame's Xing, Info or VBRI header as Apple reads it
/// (`0x14041cf50`).
#[derive(Default)]
struct SeekHeader {
    /// A Xing or VBRI header: the stream is variable-rate.
    variable: bool,
    /// The frame count a Xing or VBRI header declares; 0 when absent.
    frames: u32,
    /// A LAME tag's delay and padding.
    lame: Option<(u32, u32)>,
    /// A VBRI header's delay, less one frame and the decoder's 529 samples.
    vbri_delay: u32,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn seek_header(frame: &[u8], h: &Header) -> SeekHeader {
    let body = &frame[4..];
    for at in [h.side, 0x20] {
        match body.get(at..at + 4) {
            Some(sig @ (b"Xing" | b"Info")) => {
                let Some(flags) = be32(body, at + 4) else {
                    break;
                };
                let mut p = at + 8;
                let mut frames = 0;
                if flags & 1 != 0 {
                    frames = be32(body, p).unwrap_or(0);
                    p += 4;
                }
                p += 4 * usize::from(flags & 2 != 0)
                    + 100 * usize::from(flags & 4 != 0)
                    + 4 * usize::from(flags & 8 != 0);
                // The tag may sit four bytes on (`0x14041d18c`).
                if body.get(p..p + 4) != Some(b"LAME") && body.get(p + 4..p + 8) == Some(b"LAME") {
                    p += 4;
                }
                let lame = (body.get(p..p + 4) == Some(b"LAME"))
                    .then(|| body.get(p + 21..p + 24))
                    .flatten()
                    .map(|b| {
                        (
                            u32::from(b[0]) << 4 | u32::from(b[1]) >> 4,
                            u32::from(b[1] & 15) << 8 | u32::from(b[2]),
                        )
                    });
                // Info declares a constant rate: its frame count is not read.
                let xing = sig == b"Xing";
                return SeekHeader {
                    variable: xing,
                    frames: if xing { frames } else { 0 },
                    lame,
                    vbri_delay: 0,
                };
            }
            Some(b"VBRI") if at == 0x20 => {
                let field = |n: usize| {
                    body.get(at + n..at + n + 2)
                        .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])))
                };
                let delay = field(6).unwrap_or(0);
                let floor = h.samples() + 529;
                return SeekHeader {
                    variable: true,
                    frames: be32(body, at + 14).unwrap_or(0),
                    lame: None,
                    vbri_delay: if delay > floor { delay - floor } else { 0 },
                };
            }
            _ => {}
        }
    }
    SeekHeader::default()
}

/// An Apple `SMPB` frame (`0x140437024`): delay and played samples.
fn smpb_frame(frame: &[u8], h: &Header) -> Option<(u32, u64)> {
    let body = &frame[4..];
    (body.get(h.side..h.side + 4)? == b"SMPB").then_some(())?;
    let delay = be32(body, h.side + 8)?;
    let samples = u64::from_be_bytes(body.get(h.side + 12..h.side + 20)?.try_into().ok()?);
    Some((delay, samples))
}

/// An `iTunSMPB` text of 115 or 116 characters (`0x1403bdaf4`): delay,
/// padding, played samples, flags and resynch, in hex.
fn itunsmpb(text: &str) -> Option<(u32, u32, u64, u32, u64)> {
    if !(115..=116).contains(&text.len()) {
        return None;
    }
    let f: Vec<_> = text.split_whitespace().collect();
    let hex32 = |s: &str| u32::from_str_radix(s, 16).ok();
    let hex64 = |s: &str| u64::from_str_radix(s, 16).ok();
    Some((
        hex32(f.get(1)?)?,
        hex32(f.get(2)?)?,
        hex64(f.get(3)?)?,
        hex32(f.get(4)?)?,
        hex64(f.get(5)?)?,
    ))
}

/// The delay the encoder named in the tag implies, and whether it marks a CD
/// rip (`0x14043d34f`-`0x14043d42f`). The agent also accepts its own
/// localised name, which contains "Apple".
fn encoder_delay(text: &str, vbri_delay: u32) -> (u32, bool) {
    if text.contains("Apple") {
        (528, false)
    } else if text.starts_with("MusicMatch") {
        (726, true)
    } else if text.starts_with("Fraunhofer") {
        (if vbri_delay != 0 { vbri_delay } else { 726 }, false)
    } else if text.starts_with("Amazon.com") {
        (576, true)
    } else if text.starts_with("LAME") {
        (576, false)
    } else {
        (528, false)
    }
}

/// Apple's MPEG Layer III timing for the `bytes` of frames at `start`, with
/// the hints its ID3v2 tag gives.
pub fn analyse(
    input: &mut (impl Read + Seek),
    start: u64,
    bytes: u64,
    hints: &EncoderHints,
) -> Result<AppleTiming> {
    let end = start.checked_add(bytes).context("MPEG extent overflow")?;
    let mut offsets = Vec::new();
    let mut first: Option<(Header, Vec<u8>)> = None;
    let mut second = None;
    let mut at = start;
    while at < end {
        ensure!(offsets.len() < MAX_FRAMES, "MPEG frame count bound");
        input.seek(SeekFrom::Start(at))?;
        let mut word = [0; 4];
        input.read_exact(&mut word)?;
        let h = header(word)
            .filter(|h| h.bytes <= end - at)
            .with_context(|| format!("MPEG frame {} is damaged or truncated", offsets.len()))?;
        if offsets.len() < 2 {
            let mut frame = vec![0; h.bytes as usize];
            input.seek(SeekFrom::Start(at))?;
            input.read_exact(&mut frame)?;
            if first.is_none() {
                first = Some((h, frame));
            } else {
                second = Some(frame);
            }
        }
        offsets.push(at);
        at += h.bytes;
    }
    let (h, frame) = first.context("MPEG stream has no frames")?;
    let spf = h.samples();
    // `0x14041d99c`: the frame count the bytes imply, in single precision;
    // `end - start + 1` covers the audio and one byte more.
    let span = bytes + 1;
    let x =
        span as f32 / (h.kbps as f32 * if h.mpeg1 { 144_000.0 } else { 72_000.0 } / h.rate as f32);
    let estimate = (x + 0.5) as u32;
    let remainder = estimate as f32 - x;
    let seek = seek_header(&frame, &h);
    let frames = if seek.variable && seek.frames != 0 && seek.frames != estimate {
        seek.frames
    } else if remainder.abs() <= 0.05 && estimate != 0 {
        estimate
    } else {
        u32::try_from(offsets.len())?
    };
    let total = u64::from(frames) * u64::from(spf);
    let total_time_ms = total * 1000 / u64::from(h.rate);
    let bit_rate_kbps = if seek.variable && total_time_ms > 0 {
        u32::try_from(span * 8 / total_time_ms)?
    } else {
        h.kbps
    };
    // The offset of the eighth frame from the end (`0x14043d780`).
    let resynch = match offsets.len().checked_sub(8) {
        Some(n) => offsets[n] - start,
        None => 0,
    };
    let timing =
        |duration: u64, delay: u32, drain: u32, resynch: u64, heuristic, source| AppleTiming {
            frames,
            samples_per_frame: spf,
            total_time_ms,
            bit_rate_kbps,
            duration,
            delay,
            drain,
            resynch,
            heuristic,
            source,
        };
    // A tag's frame count far from the estimate is not believed
    // (`0x140438073`-`0x1404380c0`).
    let believable = |tag_frames: u64| {
        let (a, b) = (tag_frames as f64, f64::from(frames));
        frames != estimate || a.min(b) > 0.0 && (a - b).abs() / a.min(b) <= 1.2
    };
    let mut heuristic = ANALYSED;
    // The sources in the analyzer's order; the first found is kept
    // (`0x140437344`).
    if let Some((delay, padding, samples, flags, tag_resynch)) =
        hints.itunsmpb.as_deref().and_then(itunsmpb)
    {
        if believable((u64::from(delay) + u64::from(padding) + samples) / u64::from(spf)) {
            if flags & 2 == 0 {
                return Ok(timing(
                    samples,
                    delay,
                    padding,
                    if tag_resynch != 0 {
                        tag_resynch
                    } else {
                        resynch
                    },
                    1,
                    "itunsmpb",
                ));
            }
            // The analysis below replaces the tag's values (`0x14043da38`).
            heuristic = (flags & 0x00ff_ffff) | 3 | 0x0200_0000;
        }
    } else {
        if let Some((delay, padding)) = seek.lame.filter(|(d, p)| *d < 2000 && *p < 4000) {
            // `0x14041d2a6`: the header's frames, or the estimate; a length
            // that is not a CD multiple drops one more frame's samples.
            let tag_frames = u64::from(if seek.frames != 0 {
                seek.frames
            } else {
                estimate
            });
            let valid = |padding: u32| {
                (tag_frames * u64::from(spf)).checked_sub(u64::from(padding) + u64::from(delay))
            };
            let (padding, valid) = match valid(padding) {
                Some(v) if v % 588 == 0 => (padding, Some(v)),
                _ => (padding + spf, valid(padding + spf)),
            };
            if let Some(valid) = valid.filter(|v| *v != 0)
                && delay != 0
                && padding != 0
            {
                return Ok(timing(valid, delay, padding, resynch, 1, "lame_tag"));
            }
        }
        // After a Xing, VBRI or LAME frame the marker is in the next frame.
        let marked = if seek.variable || seek.lame.is_some() {
            second.as_deref()
        } else {
            Some(frame.as_slice())
        };
        if let Some((delay, samples)) =
            marked.and_then(|f| smpb_frame(f, &header(f[..4].try_into().ok()?)?))
            && let Some(drain) = total.checked_sub(samples + u64::from(delay))
        {
            return Ok(timing(
                samples,
                delay,
                u32::try_from(drain)?,
                resynch,
                1,
                "smpb_frame",
            ));
        }
    }
    let (delay, rip) = encoder_delay(&hints.text, seek.vbri_delay);
    let cd = hints.cd_rip || rip || h.rate == 44100;
    let pcm = tail(input, &offsets, end, &h, h.channels)?;
    let n = pcm.len() / usize::from(h.channels);
    let valid = search(
        &pcm,
        n,
        usize::from(h.channels),
        u64::from(delay) + 529,
        total,
        spf,
        cd,
    );
    let drain = total
        .checked_sub(valid + u64::from(delay))
        .context("MPEG gapless search exceeds the stream")?;
    Ok(timing(
        valid,
        delay,
        u32::try_from(drain)?,
        resynch,
        heuristic,
        "analysis",
    ))
}

/// The last decoded sample frames, interleaved, as the agent's tail pass has
/// them: it decodes from the first frame at least twenty frames of 320 kb/s
/// before the end, or a hundred when that holds fewer than eight
/// (`0x14043d5d6`, `0x14043d7ce`).
fn tail(
    input: &mut (impl Read + Seek),
    offsets: &[u64],
    end: u64,
    h: &Header,
    channels: u8,
) -> Result<Vec<f32>> {
    let frame_ms = u64::from(h.samples() * 1000).div_ceil(u64::from(h.rate));
    let mut from = 0;
    for frames in [20, 100] {
        let window = end.saturating_sub(frames * frame_ms * 40);
        from = offsets.partition_point(|o| *o < window);
        if offsets.len() - from >= 8 {
            break;
        }
    }
    let mut params = CodecParameters::new();
    params.for_codec(CODEC_TYPE_MP3).with_sample_rate(h.rate);
    let mut decoder =
        symphonia_bundle_mp3::MpaDecoder::try_new(&params, &DecoderOptions::default())
            .map_err(|e| anyhow::anyhow!("MP3 decoder: {e}"))?;
    let mut pcm = Vec::new();
    for (i, at) in offsets[from..].iter().enumerate() {
        let next = offsets.get(from + i + 1).copied().unwrap_or(end);
        let mut frame = vec![0; usize::try_from(next - at)?];
        input.seek(SeekFrom::Start(*at))?;
        input.read_exact(&mut frame)?;
        // A frame whose reservoir lies before the window decodes in part or
        // not at all, as in the agent; only the last frames are searched.
        let Ok(decoded) = decoder.decode(&Packet::new_from_slice(0, 0, 0, &frame)) else {
            continue;
        };
        let spec = *decoded.spec();
        if spec.channels.count() != usize::from(channels) {
            continue;
        }
        let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        samples.copy_interleaved_ref(decoded);
        pcm.extend_from_slice(samples.samples());
        let keep = TAIL_SAMPLES * usize::from(channels);
        if pcm.len() > 2 * keep {
            pcm.drain(..pcm.len() - keep);
        }
    }
    let keep = TAIL_SAMPLES * usize::from(channels);
    if pcm.len() > keep {
        pcm.drain(..pcm.len() - keep);
    }
    Ok(pcm)
}

/// The played length the agent finds by searching the decoded tail backwards
/// for where the sound stops (`0x14043cbc0`, transcribed). Candidates are
/// lengths `total - start - k + 1`; with `cd` only multiples of 588 (a CD
/// sector). A candidate hits where a sample above the floor `t2` follows one
/// that differs from it by more than `t1` of itself. Passes relax both; with
/// no hit the first candidate is the length.
fn search(
    pcm: &[f32],
    n: usize,
    channels: usize,
    start: u64,
    total: u64,
    spf: u32,
    cd: bool,
) -> u64 {
    let base = total.saturating_sub(start);
    if n == 0 {
        return base;
    }
    let tiny = 1e-38_f32;
    let (mut t1, mut t2) = (0.5_f32, 0.0003_f32);
    let mut fallback = 0;
    loop {
        let (mut hit, mut found, mut magnitude) = (false, 0, 0.0_f32);
        let mut prev = [0.0_f32; 2];
        let mut k = 1_u64;
        let mut i = n;
        loop {
            let at = (i - 1) * channels;
            if i < n {
                let candidate = (base + 1).saturating_sub(k);
                if !(cd && candidate % 588 != 0) {
                    if fallback == 0 {
                        fallback = candidate;
                    }
                    let mut rel = [0.0_f32; 2];
                    let mut hits = [false; 2];
                    for c in 0..channels {
                        let p = prev[c];
                        rel[c] = if p != 0.0 {
                            (p - pcm[at + c]) / p
                        } else {
                            f32::INFINITY
                        };
                        hits[c] = p.abs() > t2 && rel[c].abs() > t1;
                    }
                    if hits[..channels].iter().any(|h| *h) {
                        let m = if hits[0] || channels == 1 {
                            rel[0].abs()
                        } else {
                            rel[1].abs()
                        };
                        if hit {
                            return if magnitude > m { found } else { candidate };
                        }
                        (hit, found, magnitude) = (true, candidate, m);
                    }
                    if channels == 2 && k >= 2 * u64::from(spf) {
                        break;
                    }
                }
            }
            for c in 0..channels {
                prev[c] = pcm[at + c] + tiny;
            }
            k += 1;
            i -= 1;
            if i == 0 {
                break;
            }
        }
        if hit {
            return found;
        }
        t1 -= 0.1;
        t2 -= 2e-5;
        if !(t1 > 0.0 && t2 > 0.0) {
            return fallback;
        }
    }
}
