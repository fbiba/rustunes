//! Optional decoded equality evidence for an already validated ALAC profile.
//!
//! No matching, ownership, allocation, or phone operation is implied.
//! Linux `/proc/self/fd/0` gives FFmpeg the same opened, seekable, read-only file
//! that was inspected and hashed. The caller supplies validated container timing;
//! this module rechecks it and the profile on that handle before decoding.

use std::{
    fs::{self, File, Metadata},
    io::{Read, Seek},
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use lofty::{
    config::{ParseOptions, ParsingMode},
    file::AudioFile,
    mp4::{Mp4Codec, Mp4File},
};
use sha2::{Digest, Sha256};

use crate::media::timing::{self, Timing};

const MAX_FILE: u64 = 512 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);
const TERMINATION_RESERVE: Duration = Duration::from_secs(2);
const MAX_LOG: usize = 64 * 1024;
const DOMAIN: &[u8] = b"music-protocol/decoded-pcm\0";

#[derive(Clone, PartialEq, Eq)]
pub struct PcmS16Stereo {
    /// SHA256(domain || 44100 BE32 || 16 U8 || 2 U8 || samples BE64 || pcm_sha256).
    pub sha256: [u8; 32],
    /// SHA256 of raw interleaved signed 16-bit little-endian stereo samples.
    pub pcm_sha256: [u8; 32],
    pub pcm_bytes: u64,
    pub samples_per_channel: u64,
}

pub struct DecodedFingerprint {
    pub file_bytes: u64,
    pub full_file_sha256: [u8; 32],
    /// Compare this value, not the entire file hash, for optional PCM equality.
    pub pcm: PcmS16Stereo,
}

/// Decode at most 512 MiB of stable source, with a 120-second original budget.
/// Two seconds of that budget are reserved for child termination and pipe joins.
/// No resampling, remix, seek, trim, volume, or audio filter is requested. The
/// only output-format change is lossless planar-to-interleaved s16 storage.
/// The source must be 44100 Hz, 16-bit, stereo ALAC.
pub fn read(path: &Path, expected: &Timing) -> Result<DecodedFingerprint> {
    let deadline = Instant::now() + DEADLINE;
    let before = fs::symlink_metadata(path).context("reading PCM source attributes")?;
    regular(&before)?;
    let mut file = File::open(path).context("opening PCM source read-only")?;
    stable(path, &file, &before)?;
    let actual = timing::read(&mut file).context("checking PCM source timing")?;
    ensure!(
        &actual == expected,
        "PCM source timing differs from validation"
    );
    let expected_bytes = expected_bytes(expected)?;
    file.rewind()?;
    let mp4 = Mp4File::read_from(
        &mut file,
        ParseOptions::new()
            .parsing_mode(ParsingMode::Strict)
            .read_tags(false)
            .read_cover_art(false),
    )
    .context("checking actual PCM source profile")?;
    let properties = mp4.properties();
    ensure!(
        properties.codec() == &Mp4Codec::ALAC
            && !properties.is_drm_protected()
            && properties.sample_rate() == 44_100
            && properties.bit_depth() == Some(16)
            && properties.channels() == 2,
        "PCM fallback requires unprotected 44100 Hz 16-bit stereo ALAC"
    );
    file.rewind()?;
    let full_file_sha256 = hash_exact(&mut file, before.len(), deadline)?;
    stable(path, &file, &before)?;
    file.rewind()?;

    let mut command = Command::new("ffmpeg");
    command
        // FFREPORT would otherwise enable a caller-selected output log file.
        .env_remove("FFREPORT")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-xerror",
            "-max_error_rate",
            "0",
            "-err_detect",
            "explode",
            "-protocol_whitelist",
            "file,pipe",
            "-threads",
            "1",
            "-c:a",
            "alac",
            "-f",
            "mov",
            "-i",
            "/proc/self/fd/0",
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-threads",
            "1",
            "-c:a",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ])
        .stdin(Stdio::from(file.try_clone()?));
    let pcm_sha256 = decode(&mut command, expected_bytes, deadline, TERMINATION_RESERVE)?;
    stable(path, &file, &before)?;
    ensure!(
        Instant::now() < deadline,
        "PCM fingerprint deadline expired"
    );
    let mut digest = Sha256::new();
    digest.update(DOMAIN);
    digest.update(44_100_u32.to_be_bytes());
    digest.update([16, 2]);
    digest.update(expected.pcm_samples_per_channel().to_be_bytes());
    digest.update(pcm_sha256);
    Ok(DecodedFingerprint {
        file_bytes: before.len(),
        full_file_sha256,
        pcm: PcmS16Stereo {
            sha256: digest.finalize().into(),
            pcm_sha256,
            pcm_bytes: expected_bytes,
            samples_per_channel: expected.pcm_samples_per_channel(),
        },
    })
}

fn expected_bytes(timing: &Timing) -> Result<u64> {
    ensure!(
        timing.timescale == 44_100 && timing.duration_ticks > 0 && timing.packets > 0,
        "PCM fallback requires positive validated 44100 Hz timing"
    );
    timing
        .pcm_samples_per_channel()
        .checked_mul(4)
        .context("PCM sample byte count overflow")
}

fn regular(metadata: &Metadata) -> Result<()> {
    ensure!(
        metadata.is_file() && (1..=MAX_FILE).contains(&metadata.len()),
        "PCM source must be a bounded regular nonsymlink file"
    );
    Ok(())
}

fn stable(path: &Path, file: &File, before: &Metadata) -> Result<()> {
    for now in [file.metadata()?, fs::symlink_metadata(path)?] {
        regular(&now)?;
        ensure!(
            before.dev() == now.dev()
                && before.ino() == now.ino()
                && before.len() == now.len()
                && before.mtime() == now.mtime()
                && before.mtime_nsec() == now.mtime_nsec()
                && before.ctime() == now.ctime()
                && before.ctime_nsec() == now.ctime_nsec(),
            "PCM source handle or path changed"
        );
    }
    Ok(())
}

fn hash_exact(input: &mut impl Read, expected: u64, deadline: Instant) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut remaining = expected;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        ensure!(Instant::now() < deadline, "PCM hash deadline expired");
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let count = input.read(&mut buffer[..wanted])?;
        ensure!(count != 0, "PCM stream ended before expected byte count");
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    ensure!(
        input.read(&mut buffer[..1])? == 0,
        "PCM stream exceeds expected byte count"
    );
    Ok(digest.finalize().into())
}

struct Log {
    bytes: Vec<u8>,
    truncated: bool,
}

fn drain_log(mut input: impl Read) -> Result<Log> {
    let mut log = Log {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0; 4096];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return Ok(log);
        }
        let keep = count.min(MAX_LOG - log.bytes.len());
        log.bytes.extend_from_slice(&buffer[..keep]);
        log.truncated |= keep < count;
    }
}

fn decode(
    command: &mut Command,
    bytes: u64,
    deadline: Instant,
    reserve: Duration,
) -> Result<[u8; 32]> {
    let stop = deadline
        .checked_sub(reserve)
        .context("invalid PCM decoder budget")?;
    ensure!(
        Instant::now() < stop,
        "PCM decoder budget exhausted before spawn"
    );
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting FFmpeg for PCM fingerprint")?;
    // Piped handles are present after a successful spawn. No fallible work is
    // allowed before the cleanup path owns both reader threads and the child.
    let stdout = child.stdout.take().expect("piped decoder stdout");
    let stderr = child.stderr.take().expect("piped decoder stderr");
    let mut output = None;
    let mut diagnostics = None;
    let workers = (|| -> Result<()> {
        output = Some(
            thread::Builder::new()
                .name("pcm-output".into())
                .spawn(move || hash_exact(&mut { stdout }, bytes, stop))?,
        );
        diagnostics = Some(
            thread::Builder::new()
                .name("pcm-diagnostics".into())
                .spawn(move || drain_log(stderr))?,
        );
        Ok(())
    })();
    if let Err(error) = workers {
        let _ = child.kill();
        cleanup(&mut child, &mut output, &mut diagnostics, deadline)?;
        return Err(error.context("starting bounded PCM readers"));
    }
    let result = (|| {
        let mut pcm = None;
        let mut log = None;
        loop {
            if output.as_ref().is_some_and(JoinHandle::is_finished) {
                pcm = Some(join(output.take().expect("available output reader"))?);
            }
            if diagnostics.as_ref().is_some_and(JoinHandle::is_finished) {
                log = Some(join(
                    diagnostics.take().expect("available diagnostic reader"),
                )?);
            }
            if let Some(status) = child.try_wait()?
                && let (Some(pcm), Some(log)) = (pcm, log.as_ref())
            {
                ensure!(
                    status.success() && log.bytes.is_empty() && !log.truncated,
                    "FFmpeg PCM decode failed (status {status}): {}{}",
                    String::from_utf8_lossy(&log.bytes),
                    if log.truncated {
                        " [diagnostics truncated]"
                    } else {
                        ""
                    },
                );
                return Ok(pcm);
            }
            ensure!(Instant::now() < stop, "FFmpeg PCM decode deadline expired");
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        // Includes oversized stdout and decoder/pipe errors. Never wait for a
        // misbehaving decoder to voluntarily close its streams after failure.
        let _ = child.kill();
    }
    cleanup(&mut child, &mut output, &mut diagnostics, deadline)?;
    result
}

fn join<T>(thread: JoinHandle<Result<T>>) -> Result<T> {
    thread
        .join()
        .map_err(|_| anyhow::anyhow!("PCM reader thread panicked"))?
}

fn cleanup(
    child: &mut Child,
    output: &mut Option<JoinHandle<Result<[u8; 32]>>>,
    diagnostics: &mut Option<JoinHandle<Result<Log>>>,
    deadline: Instant,
) -> Result<()> {
    loop {
        let exited = child.try_wait()?.is_some();
        let output_done = output.as_ref().is_none_or(JoinHandle::is_finished);
        let diagnostics_done = diagnostics.as_ref().is_none_or(JoinHandle::is_finished);
        if exited && output_done && diagnostics_done {
            if let Some(thread) = output.take() {
                let _ = join(thread);
            }
            if let Some(thread) = diagnostics.take() {
                let _ = join(thread);
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            anyhow::bail!("PCM child/pipe cleanup exceeded original deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests;
