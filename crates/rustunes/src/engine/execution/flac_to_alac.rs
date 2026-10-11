//! FLAC sent as ALAC (`sync --flac-to-alac`). FFmpeg converts each FLAC a
//! plan adds into a private temporary ALAC-in-M4A file, which then goes
//! through the ordinary ALAC insert batch. FFmpeg runs directly, never
//! through a shell, and converts only the audio and the front cover. The tags
//! are written afterwards from the FLAC's own projection: FFmpeg's tag mapping
//! drops TRACKTOTAL and DISCTOTAL, and its M4A writer has no sort names. A
//! conversion is sent only when both files decode to the same PCM, the ALAC's
//! tags project as the FLAC's do, it holds the FLAC's cover and it fits the
//! transfer cap.
use std::{
    fs,
    io::{self, Read},
    os::unix::{
        fs::{DirBuilderExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::{
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::state::journal::Conversion,
};
use music_protocol::media::artwork::Cover;
use music_protocol::media::fingerprint::SourceFingerprint;
use music_protocol::media::metadata::{Metadata, ProjectedFields, tag_values};
use music_protocol::media::{format::Format, mpeg::Source};

/// FFmpeg's ALAC encoder writes at most 24 bits per sample.
pub(crate) const MAX_BITS: u8 = 24;
/// Each FFmpeg run, conversion or decode.
const DEADLINE: Duration = Duration::from_secs(600);
const MAX_LOG: usize = 64 * 1024;
/// Temporary directories are named with this prefix and the run's process.
const PREFIX: &str = "rustunes-flac-";

/// The FFmpeg on `PATH`, checked to convert FLAC to ALAC.
#[derive(Clone, Debug)]
pub(crate) struct Ffmpeg {
    /// The first line of `ffmpeg -version`, recorded with each conversion.
    version: String,
}

impl Ffmpeg {
    #[cfg(test)]
    pub(crate) fn version(&self) -> &str {
        &self.version
    }
}

/// Find FFmpeg before anything else is done: on `PATH`, with the ALAC encoder
/// and both decoders the conversion and its check need.
pub(crate) fn require_ffmpeg() -> Result<Ffmpeg> {
    const HINT: &str = "--flac-to-alac needs FFmpeg with its ALAC encoder and its FLAC and ALAC decoders; install ffmpeg (on Fedora: dnf install ffmpeg-free) and run again";
    let found = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            fs::metadata(directory.join("ffmpeg"))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    });
    ensure!(found, "FFmpeg is not on PATH; {HINT}");
    let listing = |argument: &str| -> Result<String> {
        let output = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", argument])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running ffmpeg {argument}; {HINT}"))?;
        ensure!(
            output.status.success(),
            "ffmpeg {argument} failed ({}); {HINT}",
            output.status
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let version = listing("-version")?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    ensure!(
        version.starts_with("ffmpeg version ")
            && version.len() <= 256
            && !version.chars().any(char::is_control),
        "ffmpeg -version printed no version line; {HINT}"
    );
    // A listing line is its capability flags, the codec's name and its
    // description, as in " A....D alac   ALAC (Apple Lossless Audio Codec)".
    let lists = |text: &str, name: &str| {
        text.lines().any(|line| {
            let mut words = line.split_whitespace();
            words.next().is_some_and(|flags| flags.starts_with('A')) && words.next() == Some(name)
        })
    };
    let encoders = listing("-encoders")?;
    let decoders = listing("-decoders")?;
    ensure!(
        lists(&encoders, "alac"),
        "this FFmpeg has no ALAC encoder; {HINT}"
    );
    for name in ["flac", "alac"] {
        ensure!(
            lists(&decoders, name),
            "this FFmpeg has no {name} decoder; {HINT}"
        );
    }
    Ok(Ffmpeg { version })
}

/// Whether FFmpeg can convert this FLAC without losing anything: at most 24
/// bits and one or two channels.
pub(crate) fn convertible(source: &Source) -> Result<()> {
    let stream = source
        .stream
        .as_ref()
        .filter(|s| s.format == Format::Flac)
        .context("only a FLAC is converted to ALAC")?;
    ensure!(
        stream.bits_per_sample <= MAX_BITS,
        "this FLAC is {}-bit and FFmpeg's ALAC encoder writes at most {MAX_BITS} bits, so converting it would lose audio",
        stream.bits_per_sample
    );
    ensure!(
        (1..=2).contains(&stream.encoded.channels),
        "this FLAC has {} channels; only mono and stereo FLACs are converted",
        stream.encoded.channels
    );
    Ok(())
}

/// A private (0700) directory under the system temporary directory holding
/// one run's conversions. Each file is removed once its insert batch settles;
/// a run killed mid-batch leaves its files for `resume`.
pub(crate) struct Directory {
    path: PathBuf,
}

impl Directory {
    pub(crate) fn create() -> Result<Self> {
        Self::create_in(&std::env::temp_dir())
    }

    fn create_in(base: &Path) -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = base.join(format!("{PREFIX}{}-{nanos}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the directory if no conversion is left in it (and it is not
    /// gone already, with its last file).
    pub(crate) fn finish(self) -> Result<()> {
        match fs::remove_dir(&self.path) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                ) =>
            {
                Ok(())
            }
            result => result.with_context(|| format!("removing {}", self.path.display())),
        }
    }
}

/// Free bytes in the directory `path` (where the conversions go), for a
/// batch's estimate.
pub(crate) fn available(path: &Path) -> Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string and `stat` a valid buffer.
    let status = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    ensure!(status == 0, "{}", io::Error::last_os_error());
    Ok(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

/// What `add_all` needs to send FLACs as ALAC: FFmpeg and this run's
/// private directory.
pub(crate) struct Converter {
    pub(crate) ffmpeg: Ffmpeg,
    pub(crate) directory: Directory,
}

/// Whether `path` is a file in a conversion directory; nothing else is ever
/// removed here.
fn converted_file(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(PREFIX))
}

/// Remove a conversion its settled batch no longer needs.
pub(crate) fn remove(path: &Path) -> Result<()> {
    ensure!(
        converted_file(path),
        "{} is not a conversion; it is not removed",
        path.display()
    );
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result.with_context(|| format!("removing {}", path.display())),
    }
}

/// Remove a conversion an earlier run left for its batch, and then its
/// directory if nothing else is in it. Once that run has ended, what else is
/// in it goes too: the conversions it made ahead for later batches, which
/// nothing sends now.
pub(crate) fn remove_left(path: &Path) -> Result<()> {
    remove(path)?;
    let directory = path.parent().context("conversion directory")?;
    let removed = match directory.file_name().and_then(|name| name.to_str()) {
        Some(name) if ended(name) => fs::remove_dir_all(directory),
        _ => fs::remove_dir(directory),
    };
    match removed {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        result => result.with_context(|| format!("removing {}", directory.display())),
    }
}

/// The process a conversion directory `PREFIX<process>-<nanos>` is named
/// after.
fn process(name: &str) -> Option<u32> {
    name.strip_prefix(PREFIX)?
        .split_once('-')
        .and_then(|(process, _)| process.parse().ok())
}

fn running(process: u32) -> bool {
    Path::new(&format!("/proc/{process}")).exists()
}

/// Whether the conversion directory `name` belongs to another process that
/// is no longer running.
fn ended(name: &str) -> bool {
    process(name).is_some_and(|p| p != std::process::id() && !running(p))
}

/// One FLAC converted and checked: what its ALAC insert sends.
pub(crate) struct Converted {
    pub(crate) path: PathBuf,
    pub(crate) fingerprint: SourceFingerprint,
    pub(crate) metadata: Metadata,
    pub(crate) cover: Option<Cover>,
    pub(crate) conversion: Conversion,
}

/// Whether FFmpeg copies the FLAC's picture as the M4A's cover: its only
/// picture, a JPEG or PNG, which the M4A then holds as the FLAC does. FFmpeg
/// skips a picture that declares no type ("Could not read mimetype from an
/// attached picture"), so that cover is written as the JPEG sync sends.
fn copies_picture(source: &Source) -> bool {
    matches!(
        source.artwork.as_slice(),
        [p] if p.declared_type && (p.mime == "image/jpeg" || p.mime == "image/png")
    )
}

/// The FFmpeg arguments: the FLAC's audio as ALAC and the picture
/// [`copies_picture`] takes copied as the M4A's cover, with no tags.
/// `bitexact` keeps FFmpeg's own name and times out of the file, so a
/// conversion repeats byte for byte.
fn conversion_command(flac: &Path, source: &Source, output: &Path) -> Result<Command> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-xerror",
            "-n",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(flac)
        .args(["-map", "0:a:0"]);
    if copies_picture(source) {
        command.args([
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-disposition:v:0",
            "attached_pic",
        ]);
    }
    command
        .args([
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-fflags",
            "+bitexact",
            "-flags:a",
            "+bitexact",
            "-c:a",
            "alac",
            "-f",
            "ipod",
        ])
        .arg(output);
    Ok(command)
}

/// Write the FLAC's projected tags into the converted file's `ilst`, beside
/// the cover FFmpeg put there, as iTunes writes them: text atoms, `trkn`
/// and `disk` as implicit numbers, `cpil` as one byte. `cover`, when FFmpeg
/// copied none, is written as the file's one JPEG cover.
fn write_tags(output: &Path, prepared: &PreparedSource, cover: Option<&Cover>) -> Result<()> {
    use lofty::{
        config::{ParseOptions, ParsingMode, WriteOptions},
        file::AudioFile,
        mp4::{Atom, AtomData, AtomIdent, DataType, Ilst, Mp4File},
        picture::{MimeType, Picture, PictureType},
    };
    let (m, _) = prepared.coded_insert_fields()?;
    let mut file = fs::OpenOptions::new().read(true).write(true).open(output)?;
    let mut mp4 = Mp4File::read_from(
        &mut file,
        ParseOptions::new()
            .parsing_mode(ParsingMode::Strict)
            .read_cover_art(true)
            .implicit_conversions(false),
    )
    .context("reading FFmpeg's ALAC")?;
    let mut ilst = mp4.ilst().cloned().unwrap_or_else(Ilst::new);
    let mut put = |id: [u8; 4], data: AtomData| {
        ilst.replace_atom(Atom::new(AtomIdent::Fourcc(id), data));
    };
    // A release date gives the year; the file keeps the date as the FLAC has
    // it, the one that says something (an empty one is none).
    let date = tag_values(&prepared.source().metadata.release_date)
        .next()
        .filter(|_| m.year.is_some())
        .map(str::to_owned);
    for (id, value) in [
        (*b"\xa9nam", Some(&m.title)),
        (*b"\xa9ART", Some(&m.artist)),
        (*b"\xa9alb", Some(&m.album)),
        (*b"aART", m.album_artist.as_ref()),
        (*b"\xa9gen", m.genre.as_ref()),
        (*b"\xa9wrt", m.composer.as_ref()),
        (*b"cprt", m.copyright.as_ref()),
        (*b"\xa9day", date.as_ref()),
        (*b"\xa9cmt", m.comment.as_ref()),
        (*b"sonm", m.sort_title.as_ref()),
        (*b"soar", m.sort_artist.as_ref()),
        (*b"soal", m.sort_album.as_ref()),
        (*b"soaa", m.sort_album_artist.as_ref()),
        (*b"soco", m.sort_composer.as_ref()),
    ] {
        if let Some(text) = value {
            put(id, AtomData::UTF8(text.clone()));
        }
    }
    let number = |n: Option<u32>| -> Result<[u8; 2]> {
        Ok(u16::try_from(n.unwrap_or(0))
            .context("number out of range")?
            .to_be_bytes())
    };
    for (id, n, total, tail) in [
        (*b"trkn", m.track, m.track_total, &[0u8, 0][..]),
        (*b"disk", m.disc, m.disc_total, &[][..]),
    ] {
        if n.is_some() {
            let (n, total) = (number(n)?, number(total)?);
            let mut data = vec![0, 0, n[0], n[1], total[0], total[1]];
            data.extend_from_slice(tail);
            put(
                id,
                AtomData::Unknown {
                    code: DataType::Reserved,
                    data,
                },
            );
        }
    }
    if let Some(compilation) = m.compilation {
        put(*b"cpil", AtomData::Bool(compilation));
    }
    if let Some(cover) = cover {
        put(
            *b"covr",
            AtomData::Picture(Picture::new_unchecked(
                PictureType::CoverFront,
                Some(MimeType::Jpeg),
                None,
                cover.bytes.clone(),
            )),
        );
    }
    mp4.set_ilst(ilst);
    std::io::Seek::rewind(&mut file)?;
    mp4.save_to(&mut file, WriteOptions::new().preferred_padding(0))
        .context("writing the FLAC's tags into its ALAC")?;
    file.sync_all()?;
    Ok(())
}
