//! Identity of encoded ALAC bytes, independent of tags and artwork.
//!
//! This is not decoded-PCM equivalence. It deliberately distinguishes
//! different ALAC configurations and different `stsz` representations. It only
//! accepts one self-contained audio track whose chunks cover one entire `mdat`
//! payload, in order, without gaps or overlaps. No metadata is normalized.

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata},
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BOXES: usize = 128;
pub(crate) const MAX_SAMPLES: u32 = 1_048_576;
const MAX_CHUNKS: u32 = MAX_SAMPLES;
const MAX_CHUNK_RUNS: u32 = 65_536;
const HASH_BUFFER: usize = 64 * 1024;
const DOMAIN: &[u8] = b"music-protocol/encoded-alac\0";

#[derive(Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub file_bytes: u64,
    pub full_file_sha256: [u8; 32],
    pub audio: EncodedAlac,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EncodedAlac {
    /// The composite identity of the other fields, as
    /// [`EncodedAlac::composite_sha256`] computes it.
    pub sha256: [u8; 32],
    /// The 28-byte ALAC configuration payload, including version/flags.
    pub alac_config_sha256: [u8; 32],
    /// The complete `stsz` payload, including version/flags and counts.
    pub sample_sizes_sha256: [u8; 32],
    /// The complete sole `mdat` payload (excluding its box header).
    pub media_sha256: [u8; 32],
    pub timescale: u32,
    pub duration_ticks: u64,
    pub media_bytes: u64,
}

impl EncodedAlac {
    /// The composite identity of this fingerprint's other fields:
    /// SHA256(domain || timescale BE32 || ticks BE64 || media bytes BE64 ||
    /// config SHA256 || sample-size SHA256 || media SHA256). The `sha256`
    /// field itself is not an input.
    pub fn composite_sha256(&self) -> [u8; 32] {
        let mut composite = Sha256::new();
        composite.update(DOMAIN);
        composite.update(self.timescale.to_be_bytes());
        composite.update(self.duration_ticks.to_be_bytes());
        composite.update(self.media_bytes.to_be_bytes());
        composite.update(self.alac_config_sha256);
        composite.update(self.sample_sizes_sha256);
        composite.update(self.media_sha256);
        composite.finalize().into()
    }
}

fn regular(metadata: &Metadata) -> Result<()> {
    ensure!(
        metadata.file_type().is_file(),
        "source must be a regular nonsymlink file"
    );
    ensure!(
        (1..=MAX_FILE_BYTES).contains(&metadata.len()),
        "source size exceeds fingerprint bounds"
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct Atom {
    pub(crate) kind: [u8; 4],
    pub(crate) start: u64,
    pub(crate) end: u64,
}

impl Atom {
    pub(crate) fn len(self) -> u64 {
        self.end - self.start
    }
}

/// Inspect an already held, seekable source without opening a pathname.
/// The caller supplies the observed length and remains responsible for descriptor stability.
/// The same byte, atom and sample bounds apply as in [`read`].
pub fn inspect(input: &mut (impl Read + Seek), file_bytes: u64) -> Result<SourceFingerprint> {
    ensure!(
        (1..=MAX_FILE_BYTES).contains(&file_bytes),
        "source size exceeds fingerprint bounds"
    );
    let timing = crate::media::timing::read(input)?;
    ensure!(
        (1..=u64::from(MAX_SAMPLES)).contains(&timing.packets),
        "sample count exceeds fingerprint bounds"
    );
    let top = atoms(input, 0, file_bytes)?;
    let media = one(&top, b"mdat")?;
    ensure!(media.len() > 0, "empty media payload");
    let movie = children(input, one(&top, b"moov")?, 0)?;
    let track = children(input, one(&movie, b"trak")?, 0)?;
    let mdia = children(input, one(&track, b"mdia")?, 0)?;
    let minf = children(input, one(&mdia, b"minf")?, 0)?;
    self_contained(input, &minf)?;
    let table = children(input, one(&minf, b"stbl")?, 0)?;
    let descriptions = children(input, one(&table, b"stsd")?, 8)?;
    let description = one(&descriptions, b"alac")?;
    let header = read_at::<28>(input, description, 0)?;
    ensure!(
        header[..6] == [0; 6] && header[6..8] == [0, 1],
        "audio references unsupported data source"
    );
    let configs = children(input, description, 28)?;
    let config = one(&configs, b"alac")?;
    ensure!(config.len() == 28, "unsupported ALAC configuration length");
    let config = read_at::<28>(input, config, 0)?;

    let sizes = bounded_payload(
        input,
        one(&table, b"stsz")?,
        12 + u64::from(MAX_SAMPLES) * 4,
    )?;
    ensure!(
        sizes.len() >= 12 && sizes[..4] == [0; 4],
        "unsupported sample-size header"
    );
    let fixed_size = u32_at(&sizes, 4)?;
    let sample_count = u32_at(&sizes, 8)?;
    ensure!(
        u64::from(sample_count) == timing.packets,
        "sample count disagrees with timing"
    );
    let expected = 12
        + if fixed_size == 0 {
            u64::from(sample_count) * 4
        } else {
            0
        };
    ensure!(
        sizes.len() as u64 == expected,
        "sample-size table length mismatch"
    );
    validate_coverage(input, &table, media, &sizes, fixed_size, sample_count)?;

    let (full_file_sha256, media_sha256) = hash_file_and_media(input, file_bytes, media)?;
    let alac_config_sha256: [u8; 32] = Sha256::digest(config).into();
    let sample_sizes_sha256: [u8; 32] = Sha256::digest(&sizes).into();
    let mut audio = EncodedAlac {
        sha256: [0; 32],
        alac_config_sha256,
        sample_sizes_sha256,
        media_sha256,
        timescale: timing.timescale,
        duration_ticks: timing.duration_ticks,
        media_bytes: media.len(),
    };
    audio.sha256 = audio.composite_sha256();
    Ok(SourceFingerprint {
        file_bytes,
        full_file_sha256,
        audio,
    })
}

pub(crate) fn self_contained(input: &mut (impl Read + Seek), minf: &[Atom]) -> Result<()> {
    let dinf = children(input, one(minf, b"dinf")?, 0)?;
    let dref = one(&dinf, b"dref")?;
    ensure!(
        read_at::<8>(input, dref, 0)? == [0, 0, 0, 0, 0, 0, 0, 1],
        "unsupported data reference table"
    );
    let references = children(input, dref, 8)?;
    ensure!(references.len() == 1, "ambiguous data references");
    let reference = one(&references, b"url ")?;
    ensure!(
        reference.len() == 4 && read_at::<4>(input, reference, 0)? == [0, 0, 0, 1],
        "external data reference is unsupported"
    );
    Ok(())
}

pub(crate) fn validate_coverage(
    input: &mut (impl Read + Seek),
    table: &[Atom],
    media: Atom,
    sizes: &[u8],
    fixed_size: u32,
    sample_count: u32,
) -> Result<()> {
    let extent = sample_extent(input, table, media, sizes, fixed_size, sample_count)?;
    ensure!(
        extent.start == media.start && extent.end == media.end,
        "audio sample tables leave unreferenced media or samples"
    );
    Ok(())
}

/// The part of `media` the sample tables reference: chunks in order, each
/// starting where the one before ended, holding every sample. Bytes before
/// the first chunk or after the last are left out.
pub(crate) fn sample_extent(
    input: &mut (impl Read + Seek),
    table: &[Atom],
    media: Atom,
    sizes: &[u8],
    fixed_size: u32,
    sample_count: u32,
) -> Result<Atom> {
    let offsets: Vec<_> = table
        .iter()
        .filter(|atom| atom.kind == *b"stco" || atom.kind == *b"co64")
        .copied()
        .collect();
    ensure!(offsets.len() == 1, "ambiguous or missing chunk offsets");
    let offset_atom = offsets[0];
    let width = if offset_atom.kind == *b"stco" { 4 } else { 8 };
    let offsets = bounded_payload(input, offset_atom, 8 + u64::from(MAX_CHUNKS) * width)?;
    let chunks = table_count(&offsets, MAX_CHUNKS, width)?;
    let runs = bounded_payload(
        input,
        one(table, b"stsc")?,
        8 + u64::from(MAX_CHUNK_RUNS) * 12,
    )?;
    let run_count = table_count(&runs, MAX_CHUNK_RUNS, 12)?;
    let mut prior_first = 0;
    for run in 0..run_count {
        let at = 8 + run as usize * 12;
        let first = u32_at(&runs, at)?;
        ensure!(
            first > prior_first && first <= chunks,
            "invalid sample-to-chunk run ordering"
        );
        ensure!(
            run != 0 || first == 1,
            "first sample-to-chunk run must start at one"
        );
        ensure!(
            u32_at(&runs, at + 4)? > 0 && u32_at(&runs, at + 8)? == 1,
            "unsupported sample-to-chunk description or count"
        );
        prior_first = first;
    }
    let first = if width == 4 {
        u64::from(u32_at(&offsets, 8)?)
    } else {
        u64::from_be_bytes(offsets[8..16].try_into()?)
    };
    ensure!(
        (media.start..media.end).contains(&first),
        "the first audio chunk lies outside the media"
    );
    let (mut run, mut sample, mut position) = (0_u32, 0_u32, first);
    for chunk in 1..=chunks {
        if run + 1 < run_count && u32_at(&runs, 8 + (run + 1) as usize * 12)? == chunk {
            run += 1;
        }
        let offset_at = 8 + (chunk - 1) as usize * width as usize;
        let offset = if width == 4 {
            u64::from(u32_at(&offsets, offset_at)?)
        } else {
            u64::from_be_bytes(offsets[offset_at..offset_at + 8].try_into()?)
        };
        ensure!(
            offset == position,
            "audio chunks do not cover media contiguously in order"
        );
        let count = u32_at(&runs, 12 + run as usize * 12)?;
        let end_sample = sample
            .checked_add(count)
            .context("chunk sample count overflow")?;
        ensure!(
            end_sample <= sample_count,
            "chunk references excess samples"
        );
        while sample < end_sample {
            let size = if fixed_size == 0 {
                u32_at(sizes, 12 + sample as usize * 4)?
            } else {
                fixed_size
            };
            ensure!(size > 0, "zero audio sample size");
            position = position
                .checked_add(u64::from(size))
                .context("audio extent overflow")?;
            ensure!(position <= media.end, "audio sample extends beyond media");
            sample += 1;
        }
    }
    ensure!(
        sample == sample_count,
        "audio sample tables leave samples unreferenced"
    );
    Ok(Atom {
        kind: media.kind,
        start: first,
        end: position,
    })
}

pub(crate) fn table_count(bytes: &[u8], limit: u32, width: u64) -> Result<u32> {
    ensure!(
        bytes.len() >= 8 && bytes[..4] == [0; 4],
        "unsupported table version or flags"
    );
    let count = u32_at(bytes, 4)?;
    ensure!(
        (1..=limit).contains(&count),
        "table count exceeds fingerprint bounds"
    );
    ensure!(
        bytes.len() as u64 == 8 + u64::from(count) * width,
        "table length mismatch"
    );
    Ok(count)
}

pub(crate) fn hash_file_and_media(
    input: &mut (impl Read + Seek),
    file_bytes: u64,
    media: Atom,
) -> Result<([u8; 32], [u8; 32])> {
    input.rewind()?;
    let (mut full, mut audio) = (Sha256::new(), Sha256::new());
    let mut buffer = [0_u8; HASH_BUFFER];
    let mut position = 0;
    while position < file_bytes {
        let length = (file_bytes - position).min(HASH_BUFFER as u64) as usize;
        input.read_exact(&mut buffer[..length])?;
        full.update(&buffer[..length]);
        let left = position.max(media.start);
        let right = (position + length as u64).min(media.end);
        if left < right {
            audio.update(&buffer[(left - position) as usize..(right - position) as usize]);
        }
        position += length as u64;
    }
    ensure!(
        input.read(&mut buffer[..1])? == 0,
        "source grew while hashing"
    );
    Ok((full.finalize().into(), audio.finalize().into()))
}

pub(crate) fn one(atoms: &[Atom], kind: &[u8; 4]) -> Result<Atom> {
    let mut selected = atoms.iter().filter(|atom| &atom.kind == kind);
    let atom = *selected
        .next()
        .with_context(|| format!("missing {:?} atom", kind.escape_ascii().to_string()))?;
    ensure!(
        selected.next().is_none(),
        "ambiguous {:?} atom",
        kind.escape_ascii().to_string()
    );
    Ok(atom)
}

pub(crate) fn children(
    input: &mut (impl Read + Seek),
    parent: Atom,
    skip: u64,
) -> Result<Vec<Atom>> {
    ensure!(skip <= parent.len(), "truncated atom payload");
    atoms(input, parent.start + skip, parent.end)
}

pub(crate) fn atoms(input: &mut (impl Read + Seek), mut start: u64, end: u64) -> Result<Vec<Atom>> {
    let mut atoms = Vec::new();
    while start < end {
        ensure!(
            atoms.len() < MAX_BOXES && end - start >= 8,
            "atom count/header exceeds bounds"
        );
        input.seek(SeekFrom::Start(start))?;
        let mut header = [0; 8];
        input.read_exact(&mut header)?;
        let short = u32::from_be_bytes(header[..4].try_into()?);
        let (size, head) = if short == 1 {
            ensure!(end - start >= 16, "truncated extended atom header");
            let mut extended = [0; 8];
            input.read_exact(&mut extended)?;
            (u64::from_be_bytes(extended), 16)
        } else {
            (u64::from(short), 8)
        };
        ensure!(
            size >= head && size <= end - start,
            "atom length exceeds parent"
        );
        atoms.push(Atom {
            kind: header[4..].try_into()?,
            start: start + head,
            end: start + size,
        });
        start += size;
    }
    Ok(atoms)
}

pub(crate) fn read_at<const N: usize>(
    input: &mut (impl Read + Seek),
    atom: Atom,
    offset: u64,
) -> Result<[u8; N]> {
    ensure!(
        offset <= atom.len() && N as u64 <= atom.len() - offset,
        "truncated atom field"
    );
    input.seek(SeekFrom::Start(atom.start + offset))?;
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn bounded_payload(
    input: &mut (impl Read + Seek),
    atom: Atom,
    limit: u64,
) -> Result<Vec<u8>> {
    ensure!(atom.len() <= limit, "sample table exceeds allocation bound");
    let mut bytes = vec![0; usize::try_from(atom.len())?];
    input.seek(SeekFrom::Start(atom.start))?;
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let value = bytes
        .get(at..at + 4)
        .context("truncated sample table field")?;
    Ok(u32::from_be_bytes(value.try_into()?))
}

mod transferred;
