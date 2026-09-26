//! Exact container timing for a single, unedited ALAC track. Lofty exposes
//! only rounded milliseconds. This reader does not decode audio, allocate its
//! body, or interpret device/wire metadata.

use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, ensure};

const MAX_BOXES: usize = 128;
const MAX_TIME_ENTRIES: u32 = 65_536;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct Timing {
    pub timescale: u32,
    pub duration_ticks: u64,
    pub packets: u64,
}

impl Timing {
    /// The reader requires the media timescale to equal ALAC's sample rate.
    /// This is the container's expected PCM frame count, not a decode result.
    pub fn pcm_samples_per_channel(&self) -> u64 {
        self.duration_ticks
    }
}

#[derive(Clone, Copy, Debug)]
struct BoxInfo {
    kind: [u8; 4],
    data: u64,
    end: u64,
}

pub fn read(source: &mut (impl Read + Seek)) -> Result<Timing> {
    let end = source.seek(SeekFrom::End(0))?;
    let top = boxes(source, 0, end)?;
    ensure!(!has(&top, b"moof"), "fragmented MP4 timing is unsupported");
    let moov = one(&top, b"moov")?;
    let movie = boxes(source, moov.data, moov.end)?;
    ensure!(
        !has(&movie, b"mvex"),
        "fragmented MP4 timing is unsupported"
    );
    // Deliberately refuse even an audio + chapter/video track combination.
    // This binds Lofty's first-audio properties to the same unique track.
    let trak = one(&movie, b"trak")?;
    let track = boxes(source, trak.data, trak.end)?;
    let mdia = one(&track, b"mdia")?;
    let media = boxes(source, mdia.data, mdia.end)?;
    let handler = one(&media, b"hdlr")?;
    ensure!(version(source, handler)? == 0, "unsupported hdlr version");
    ensure!(
        bytes::<4>(source, handler, 8)? == *b"soun",
        "track is not audio"
    );
    let (timescale, duration_ticks) = header(source, one(&media, b"mdhd")?, true)?;
    // Lofty's later rounded-duration calculation multiplies ticks by 1000.
    duration_ticks
        .checked_mul(1000)
        .context("media duration exceeds metadata reader bounds")?;
    let minf = one(&media, b"minf")?;
    let media_info = boxes(source, minf.data, minf.end)?;
    let stbl = one(&media_info, b"stbl")?;
    let table = boxes(source, stbl.data, stbl.end)?;
    ensure!(!has(&table, b"ctts"), "composition offsets are unsupported");
    let stsd = one(&table, b"stsd")?;
    ensure!(version(source, stsd)? == 0, "unsupported stsd version");
    ensure!(
        u32_at(source, stsd, 4)? == 1,
        "exactly one sample description is required"
    );
    let descriptions = boxes(source, stsd.data + 8, stsd.end)?;
    ensure!(descriptions.len() == 1, "ambiguous sample descriptions");
    ensure!(
        descriptions[0].kind == *b"alac",
        "source must contain unprotected ALAC audio"
    );
    let alac = one(&descriptions, b"alac")?;
    ensure!(
        alac.end - alac.data >= 28,
        "truncated ALAC sample description"
    );
    ensure!(
        bytes::<2>(source, alac, 8)? == [0, 0],
        "unsupported ALAC sample description version"
    );
    let configs = boxes(source, alac.data + 28, alac.end)?;
    let config = one(&configs, b"alac")?;
    ensure!(
        version(source, config)? == 0 && config.end - config.data == 28,
        "unsupported ALAC config"
    );
    let sample_rate = u32_at(source, config, 24)?;
    ensure!(
        sample_rate > 0 && timescale == sample_rate,
        "media timescale must equal ALAC sample rate for exact PCM counting"
    );

    let (table_ticks, packets) = time_table(source, one(&table, b"stts")?)?;
    ensure!(
        table_ticks == duration_ticks,
        "stts duration disagrees with mdhd"
    );
    let sizes = one(&table, b"stsz")?;
    ensure!(version(source, sizes)? == 0, "unsupported stsz version");
    let fixed_size = u32_at(source, sizes, 4)?;
    let size_count = u32_at(source, sizes, 8)?;
    let expected_size = 12
        + if fixed_size == 0 {
            u64::from(size_count) * 4
        } else {
            0
        };
    ensure!(
        sizes.end - sizes.data == expected_size,
        "truncated or extra stsz entries"
    );
    ensure!(
        u64::from(size_count) == packets,
        "stsz packet count disagrees with stts"
    );

    let (movie_scale, movie_ticks) = header(source, one(&movie, b"mvhd")?, false)?;
    let full_span =
        (u128::from(duration_ticks) * u128::from(movie_scale)).div_ceil(u128::from(timescale));
    ensure!(
        u128::from(movie_ticks) == full_span,
        "unsupported movie duration mapping"
    );
    if has(&track, b"edts") {
        let edts = one(&track, b"edts")?;
        let edits = boxes(source, edts.data, edts.end)?;
        ensure!(edits.len() == 1, "unsupported edit container");
        identity_edit(source, one(&edits, b"elst")?, full_span)?;
    }
    Ok(Timing {
        timescale,
        duration_ticks,
        packets,
    })
}

fn has(boxes: &[BoxInfo], kind: &[u8; 4]) -> bool {
    boxes.iter().any(|atom| atom.kind == *kind)
}

fn one(boxes: &[BoxInfo], kind: &[u8; 4]) -> Result<BoxInfo> {
    let mut matches = boxes.iter().filter(|atom| atom.kind == *kind);
    let atom = *matches
        .next()
        .with_context(|| format!("missing {:?} box", kind.escape_ascii().to_string()))?;
    ensure!(
        matches.next().is_none(),
        "multiple {:?} boxes are unsupported",
        kind.escape_ascii().to_string()
    );
    Ok(atom)
}

fn boxes(source: &mut (impl Read + Seek), start: u64, end: u64) -> Result<Vec<BoxInfo>> {
    ensure!(start <= end, "box content starts beyond parent");
    let mut position = start;
    let mut result = Vec::new();
    while position < end {
        ensure!(result.len() < MAX_BOXES, "too many boxes in container");
        ensure!(end - position >= 8, "truncated box header");
        source.seek(SeekFrom::Start(position))?;
        let mut head = [0; 8];
        source.read_exact(&mut head)?;
        let short_size = u32::from_be_bytes(head[..4].try_into()?);
        let kind = head[4..].try_into()?;
        let (size, header) = if short_size == 1 {
            ensure!(end - position >= 16, "truncated extended box header");
            let mut extended = [0; 8];
            source.read_exact(&mut extended)?;
            (u64::from_be_bytes(extended), 16)
        } else {
            ensure!(short_size != 0, "open-ended boxes are unsupported");
            (u64::from(short_size), 8)
        };
        ensure!(
            size >= header && size <= end - position,
            "box size exceeds parent or header"
        );
        result.push(BoxInfo {
            kind,
            data: position + header,
            end: position + size,
        });
        position += size;
    }
    Ok(result)
}

fn bytes<const N: usize>(
    source: &mut (impl Read + Seek),
    atom: BoxInfo,
    offset: u64,
) -> Result<[u8; N]> {
    let length = atom.end - atom.data;
    ensure!(
        offset <= length && N as u64 <= length - offset,
        "truncated {:?} field",
        atom.kind.escape_ascii().to_string()
    );
    source.seek(SeekFrom::Start(atom.data + offset))?;
    let mut data = [0; N];
    source.read_exact(&mut data)?;
    Ok(data)
}

fn u32_at(source: &mut (impl Read + Seek), atom: BoxInfo, offset: u64) -> Result<u32> {
    Ok(u32::from_be_bytes(bytes(source, atom, offset)?))
}

fn version(source: &mut (impl Read + Seek), atom: BoxInfo) -> Result<u8> {
    let full = bytes::<4>(source, atom, 0)?;
    ensure!(full[1..] == [0, 0, 0], "unsupported full-box flags");
    Ok(full[0])
}

fn header(source: &mut (impl Read + Seek), atom: BoxInfo, media: bool) -> Result<(u32, u64)> {
    let version = version(source, atom)?;
    let (scale, ticks, min_length, unknown) = match version {
        0 => (
            u32_at(source, atom, 12)?,
            u64::from(u32_at(source, atom, 16)?),
            if media { 24 } else { 100 },
            u64::from(u32::MAX),
        ),
        1 => (
            u32_at(source, atom, 20)?,
            u64::from_be_bytes(bytes(source, atom, 24)?),
            if media { 36 } else { 112 },
            u64::MAX,
        ),
        _ => anyhow::bail!("unsupported timing header version {version}"),
    };
    ensure!(
        atom.end - atom.data >= min_length,
        "truncated timing header"
    );
    ensure!(
        scale > 0 && ticks > 0 && ticks != unknown,
        "unknown or zero media timing"
    );
    Ok((scale, ticks))
}

fn time_table(source: &mut (impl Read + Seek), atom: BoxInfo) -> Result<(u64, u64)> {
    ensure!(version(source, atom)? == 0, "unsupported stts version");
    let entries = u32_at(source, atom, 4)?;
    ensure!(
        (1..=MAX_TIME_ENTRIES).contains(&entries),
        "unsupported stts entry count"
    );
    ensure!(
        atom.end - atom.data == 8 + u64::from(entries) * 8,
        "truncated or extra stts entries"
    );
    let (mut ticks, mut packets) = (0_u64, 0_u64);
    for index in 0..entries {
        let offset = 8 + u64::from(index) * 8;
        let count = u64::from(u32_at(source, atom, offset)?);
        let delta = u64::from(u32_at(source, atom, offset + 4)?);
        ensure!(count > 0 && delta > 0, "zero stts count or duration");
        ticks = ticks
            .checked_add(count * delta)
            .context("stts duration overflow")?;
        packets = packets
            .checked_add(count)
            .context("stts packet count overflow")?;
    }
    Ok((ticks, packets))
}

fn identity_edit(source: &mut (impl Read + Seek), atom: BoxInfo, full_span: u128) -> Result<()> {
    let version = version(source, atom)?;
    ensure!(
        u32_at(source, atom, 4)? == 1,
        "multiple/empty edits are unsupported"
    );
    let (span, offset, rate, length) = match version {
        0 => (
            u64::from(u32_at(source, atom, 8)?),
            i64::from(i32::from_be_bytes(bytes(source, atom, 12)?)),
            u32_at(source, atom, 16)?,
            20,
        ),
        1 => (
            u64::from_be_bytes(bytes(source, atom, 8)?),
            i64::from_be_bytes(bytes(source, atom, 16)?),
            u32_at(source, atom, 24)?,
            28,
        ),
        _ => anyhow::bail!("unsupported edit version {version}"),
    };
    ensure!(
        atom.end - atom.data == length,
        "truncated or extra edit entries"
    );
    ensure!(
        u128::from(span) == full_span && offset == 0 && rate == 0x0001_0000,
        "trimmed, delayed or rate-adjusted edits are unsupported"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
