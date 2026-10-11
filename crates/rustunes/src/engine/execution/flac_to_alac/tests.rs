//! Conversions of FLACs FFmpeg generates here: tags, sort names and covers
//! carried, hi-res audio kept, repeatable output, and the refusals.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "flac-to-alac-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-v", "error", "-n"])
        .args(args)
        .env_remove("FFREPORT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const TAGS: [(&str, &str); 19] = [
    ("TITLE", "The Title"),
    ("ARTIST", "An Artist"),
    ("ALBUM", "The Album"),
    ("ALBUMARTIST", "Various Artists"),
    ("GENRE", "Test"),
    ("COMPOSER", "A Composer"),
    ("COPYRIGHT", "2024 Nobody"),
    ("DATE", "2024-05-01"),
    ("TRACKNUMBER", "2"),
    ("TRACKTOTAL", "12"),
    ("DISCNUMBER", "1"),
    ("DISCTOTAL", "2"),
    ("COMPILATION", "1"),
    ("COMMENT", "Line one\nLine two"),
    ("TITLESORT", "Title, The"),
    ("ARTISTSORT", "Artist, An"),
    ("ALBUMSORT", "Album, The"),
    ("ALBUMARTISTSORT", "Various"),
    ("COMPOSERSORT", "Composer, A"),
];

/// A 0.3 s tone FLAC with `tags`, generated in `temp`, and its cover from
/// `cover` (a file FFmpeg makes) as its front cover.
fn flac(
    temp: &Temp,
    name: &str,
    audio: &[&str],
    tags: &[(&str, &str)],
    cover: Option<&str>,
) -> PathBuf {
    let path = temp.path(name);
    let mut args = vec![
        "-f".to_owned(),
        "lavfi".into(),
        "-t".into(),
        "0.3".into(),
        "-i".into(),
        "sine=frequency=440:sample_rate=44100".into(),
    ];
    if let Some(image) = cover {
        let image = temp.path(image);
        if !image.exists() {
            ffmpeg(&[
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=64x48",
                "-frames:v",
                "1",
                image.to_str().unwrap(),
            ]);
        }
        args.extend(["-i".into(), image.to_str().unwrap().into()]);
        args.extend(["-map", "0:a", "-map", "1:v", "-c:v", "copy"].map(String::from));
        args.extend(
            [
                "-disposition:v:0",
                "attached_pic",
                "-metadata:s:v",
                "comment=Cover (front)",
            ]
            .map(String::from),
        );
    }
    args.extend(audio.iter().map(|a| a.to_string()));
    args.extend(
        [
            "-c:a",
            "flac",
            "-fflags",
            "+bitexact",
            "-map_metadata",
            "-1",
        ]
        .map(String::from),
    );
    for (key, value) in tags {
        args.extend(["-metadata".into(), format!("{key}={value}")]);
    }
    args.push(path.to_str().unwrap().into());
    ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>());
    path
}

/// A 0.3 s tone FLAC (title, artist and album set) holding `pictures` in order: each an image
/// FFmpeg makes in `temp` (its color, its file name) and the FLAC picture
/// type FFmpeg is given by name, or none, which it writes as "other".
fn flac_with_pictures(temp: &Temp, name: &str, pictures: &[(&str, &str, Option<&str>)]) -> PathBuf {
    let path = temp.path(name);
    let mut args: Vec<String> = [
        "-f",
        "lavfi",
        "-t",
        "0.3",
        "-i",
        "sine=frequency=440:sample_rate=44100",
    ]
    .map(String::from)
    .into();
    for (color, image, _) in pictures {
        let image = temp.path(image);
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={color}:s=64x48"),
            "-frames:v",
            "1",
            image.to_str().unwrap(),
        ]);
        args.extend(["-i".into(), image.to_str().unwrap().into()]);
    }
    args.extend(["-map", "0:a"].map(String::from));
    for (index, (_, _, kind)) in pictures.iter().enumerate() {
        args.extend([
            "-map".into(),
            format!("{}:v", index + 1),
            format!("-disposition:v:{index}"),
            "attached_pic".into(),
        ]);
        if let Some(kind) = kind {
            args.extend([format!("-metadata:s:v:{index}"), format!("comment={kind}")]);
        }
    }
    args.extend(
        [
            "-c:v",
            "copy",
            "-c:a",
            "flac",
            "-fflags",
            "+bitexact",
            "-map_metadata",
            "-1",
            "-metadata",
            "TITLE=T",
            "-metadata",
            "ARTIST=A",
            "-metadata",
            "ALBUM=B",
        ]
        .map(String::from),
    );
    args.push(path.to_str().unwrap().into());
    ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>());
    path
}

fn prepared(path: &Path) -> PreparedSource {
    let prepared =
        PreparedSource::read_expected(path, &music_protocol::media::mpeg::read(path).unwrap())
            .unwrap();
    prepared.ensure_batch_insertion_allowed().unwrap();
    prepared
}

fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("the call must fail"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn a_tagged_covered_flac_converts_losslessly_and_repeats_byte_for_byte() {
    let ffmpeg = require_ffmpeg().unwrap();
    assert!(ffmpeg.version().starts_with("ffmpeg version "));
    let temp = Temp::new();
    let path = flac(&temp, "song.flac", &["-ac", "2"], &TAGS, Some("cover.jpg"));
    let source = prepared(&path);
    let first = Directory::create_in(&temp.0).unwrap();
    let converted = convert(&ffmpeg, &path, &source, &first).unwrap();
    assert!(converted.path.starts_with(first.path()));
    let m = &converted.metadata;
    assert_eq!(m.title, ["The Title"]);
    assert_eq!(m.album_artist, ["Various Artists"]);
    assert_eq!((m.track, m.track_total), (Some(2), Some(12)));
    assert_eq!((m.disc, m.disc_total), (Some(1), Some(2)));
    assert_eq!(m.year, Some(2024));
    assert_eq!(m.compilation, Some(true));
    assert_eq!(m.comment, ["Line one\nLine two"]);
    assert_eq!(m.sort_title, ["Title, The"]);
    assert_eq!(m.sort_composer, ["Composer, A"]);
    assert_eq!((m.sample_rate, m.bit_depth, m.channels), (44100, 16, 2));
    let cover = converted.cover.as_ref().unwrap();
    assert_eq!(hex(&cover.sha256), source.source().artwork[0].sha256);
    assert_eq!(cover.checksum, cover.bytes.len() as u32 + 17);
    let identity = source.identity();
    assert_eq!(
        converted.conversion,
        Conversion {
            source_path: fs::canonicalize(&path).unwrap(),
            source_bytes: identity.file_bytes,
            source_sha256: identity.full_file_sha256,
            source_audio: identity.audio.sha256,
            ffmpeg: ffmpeg.version().into(),
        }
    );
    assert!(
        converted.fingerprint == music_protocol::media::fingerprint::read(&converted.path).unwrap()
    );
    // The same FFmpeg converts the same FLAC to the same bytes.
    let second = Directory::create_in(&temp.0).unwrap();
    let again = convert(&ffmpeg, &path, &source, &second).unwrap();
    assert_eq!(
        fs::read(&converted.path).unwrap(),
        fs::read(&again.path).unwrap()
    );
    // A directory still holding a conversion stays; an emptied one goes.
    let kept = first.path().to_owned();
    first.finish().unwrap();
    assert!(kept.exists());
    fs::remove_file(&again.path).unwrap();
    let emptied = second.path().to_owned();
    second.finish().unwrap();
    assert!(!emptied.exists());
}

#[test]
fn hi_res_and_plain_flacs_convert_and_an_untagged_cover_is_absent() {
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let tags = &TAGS[..3];
    let path = flac(
        &temp,
        "hires.flac",
        &["-ac", "2", "-ar", "96000", "-sample_fmt", "s32"],
        tags,
        None,
    );
    assert_eq!(
        prepared(&path)
            .source()
            .stream
            .as_ref()
            .unwrap()
            .bits_per_sample,
        24
    );
    let directory = Directory::create_in(&temp.0).unwrap();
    let converted = convert(&ffmpeg, &path, &prepared(&path), &directory).unwrap();
    let m = &converted.metadata;
    assert_eq!((m.sample_rate, m.bit_depth, m.channels), (96000, 24, 2));
    assert!(converted.cover.is_none());
    assert_eq!(m.track, None);
    assert!(m.sort_title.is_empty());
    let mono = flac(&temp, "mono.flac", &["-ac", "1"], tags, None);
    let converted = convert(&ffmpeg, &mono, &prepared(&mono), &directory).unwrap();
    assert_eq!(converted.metadata.channels, 1);
}

#[test]
fn a_flac_with_emptied_tags_converts_as_one_without_them() {
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let untagged = fs::read(flac(&temp, "untagged.flac", &["-ac", "2"], &[], None)).unwrap();
    let write = |name: &str, tags: &[(&str, &str)]| {
        let path = temp.path(name);
        let tags = [&TAGS[..3], tags].concat();
        fs::write(&path, crate::test_fixtures::retagged_flac(&untagged, &tags)).unwrap();
        path
    };
    let removed = write("removed.flac", &[("DATE", "2024-05-01")]);
    // An empty date ahead of the one that says something, and empty text,
    // numbers and sort names.
    let emptied = write(
        "emptied.flac",
        &[
            ("COMPOSER", ""),
            ("GENRE", ""),
            ("DATE", ""),
            ("DATE", "2024-05-01"),
            ("TRACKNUMBER", ""),
            ("DISCTOTAL", ""),
            ("ARTISTSORT", ""),
        ],
    );
    let converted = |path: &Path| {
        let directory = Directory::create_in(&temp.0).unwrap();
        let converted = convert(&ffmpeg, path, &prepared(path), &directory).unwrap();
        let alac = fs::read(&converted.path).unwrap();
        (converted.metadata.release_date, alac)
    };
    let (date, alac) = converted(&emptied);
    assert_eq!(date, ["2024-05-01"]);
    // The tags written are the projection's, so the ALAC is the same file.
    assert!(alac == converted(&removed).1);
}

#[test]
fn a_png_cover_stays_in_the_file_and_is_sent_as_its_jpeg() {
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let path = flac(
        &temp,
        "png.flac",
        &["-ac", "2"],
        &TAGS[..3],
        Some("cover.png"),
    );
    let source = prepared(&path);
    assert_eq!(source.source().artwork[0].mime, "image/png");
    let directory = Directory::create_in(&temp.0).unwrap();
    let converted = convert(&ffmpeg, &path, &source, &directory).unwrap();
    let held = music_protocol::media::artwork::read(&converted.path).unwrap();
    assert_eq!(
        hex(&held.embedded.unwrap().sha256),
        source.source().artwork[0].sha256
    );
    assert_eq!(converted.cover.unwrap().mime(), "image/jpeg");
}

/// `path`'s FLAC with the declared type of each PICTURE block emptied, as some
/// taggers write it; FFmpeg cannot read such a picture.
fn without_picture_types(path: &Path) {
    let flac = fs::read(path).unwrap();
    let mut out = flac[..4].to_vec();
    let mut at = 4;
    loop {
        let size = u32::from_be_bytes([0, flac[at + 1], flac[at + 2], flac[at + 3]]) as usize;
        let body = &flac[at + 4..at + 4 + size];
        let mut block = body.to_vec();
        if flac[at] & 0x7f == 6 {
            let mime = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
            block = [&body[..4], &0u32.to_be_bytes(), &body[8 + mime..]].concat();
        }
        out.push(flac[at]);
        out.extend(&(block.len() as u32).to_be_bytes()[1..]);
        out.extend(block);
        at += 4 + size;
        if flac[at - 4 - size] & 0x80 != 0 {
            break;
        }
    }
    out.extend(&flac[at..]);
    fs::write(path, out).unwrap();
}

#[test]
fn a_cover_declaring_no_type_is_kept_though_ffmpeg_cannot_read_it() {
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let directory = Directory::create_in(&temp.0).unwrap();
    for (name, image) in [("jpeg.flac", "cover.jpg"), ("png.flac", "cover.png")] {
        let path = flac_with_pictures(&temp, name, &[("blue", image, Some("Cover (front)"))]);
        let with_type = prepared(&path);
        without_picture_types(&path);
        let source = prepared(&path);
        let picture = &source.source().artwork[0];
        assert!(!picture.declared_type && with_type.source().artwork[0].declared_type);
        assert_eq!(picture.mime, with_type.source().artwork[0].mime);
        assert_eq!(picture.sha256, with_type.source().artwork[0].sha256);
        // FFmpeg is not asked to copy the picture; the cover is written.
        let converted = convert(&ffmpeg, &path, &source, &directory).unwrap();
        let held = music_protocol::media::artwork::read(&converted.path).unwrap();
        let sent = converted.cover.unwrap();
        assert_eq!(sent.mime(), "image/jpeg");
        assert_eq!(held.cover.unwrap().sha256, sent.sha256);
        if picture.mime == "image/jpeg" {
            assert_eq!(hex(&sent.sha256), picture.sha256);
        }
    }
}

#[test]
fn the_cover_rule_picks_what_a_flac_sends_whatever_ffmpeg_attached() {
    use music_protocol::media::mpeg::cover_index;
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let directory = Directory::create_in(&temp.0).unwrap();
    let convert = |path: &Path| {
        let source = prepared(path);
        let converted = convert(&ffmpeg, path, &source, &directory).unwrap();
        (source, converted)
    };
    // A lone picture FFmpeg marks "other" is the cover, copied as it is.
    let lone = flac_with_pictures(&temp, "lone.flac", &[("blue", "lone.jpg", None)]);
    let (source, converted) = convert(&lone);
    assert_eq!(source.source().artwork[0].picture_type, 0);
    let sent = converted.cover.unwrap();
    assert_eq!(hex(&sent.sha256), source.source().artwork[0].sha256);
    // Beside a back cover, the front cover goes in as the JPEG sync sends.
    let both = flac_with_pictures(
        &temp,
        "both.flac",
        &[
            ("red", "back.png", Some("Cover (back)")),
            ("green", "front.jpg", Some("Cover (front)")),
        ],
    );
    let (source, converted) = convert(&both);
    assert_eq!(cover_index(&source.source().artwork), Some(1));
    let sent = converted.cover.unwrap();
    assert_eq!(hex(&sent.sha256), source.source().artwork[1].sha256);
    // Two pictures and no front cover: the ALAC goes without one.
    let neither = flac_with_pictures(
        &temp,
        "neither.flac",
        &[
            ("red", "one.jpg", None),
            ("green", "two.png", Some("Cover (back)")),
        ],
    );
    let (source, converted) = convert(&neither);
    assert_eq!(cover_index(&source.source().artwork), None);
    assert!(converted.cover.is_none());
    assert!(
        music_protocol::media::artwork::read(&converted.path)
            .unwrap()
            .cover
            .is_none()
    );
    // A GIF cover is held, and sent, as its JPEG. FFmpeg's FLAC writer
    // cannot attach a GIF, so lofty adds it.
    let gif = flac_with_pictures(&temp, "gif.flac", &[]);
    let image = temp.path("cover.gif");
    self::ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=yellow:s=64x48",
        "-frames:v",
        "1",
        image.to_str().unwrap(),
    ]);
    {
        use lofty::{
            config::{ParseOptions, WriteOptions},
            file::AudioFile,
            flac::FlacFile,
            ogg::OggPictureStorage,
            picture::{MimeType, Picture, PictureInformation, PictureType},
        };
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gif)
            .unwrap();
        let mut flac = FlacFile::read_from(&mut file, ParseOptions::new()).unwrap();
        flac.insert_picture(
            Picture::new_unchecked(
                PictureType::CoverFront,
                Some(MimeType::Gif),
                None,
                fs::read(&image).unwrap(),
            ),
            Some(PictureInformation {
                width: 64,
                height: 48,
                color_depth: 8,
                num_colors: 256,
            }),
        )
        .unwrap();
        std::io::Seek::rewind(&mut file).unwrap();
        flac.save_to(&mut file, WriteOptions::new()).unwrap();
    }
    let (source, converted) = convert(&gif);
    assert_eq!(source.source().artwork[0].mime, "image/gif");
    let sent = converted.cover.unwrap();
    assert_eq!(sent.mime(), "image/jpeg");
    assert_eq!(
        sent.bytes,
        music_protocol::media::bitmap::to_jpeg(&fs::read(temp.path("cover.gif")).unwrap()).unwrap()
    );
}

#[test]
fn deep_or_multichannel_flacs_and_mismatched_conversions_are_refused() {
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let tags = &TAGS[..3];
    let deep = flac(
        &temp,
        "deep.flac",
        &["-ac", "2", "-sample_fmt", "s32", "-strict", "experimental"],
        tags,
        None,
    );
    let deep = music_protocol::media::mpeg::read(&deep).unwrap();
    assert_eq!(deep.stream.as_ref().unwrap().bits_per_sample, 32);
    assert!(failure(convertible(&deep)).contains("32-bit"));
    let surround = flac(
        &temp,
        "surround.flac",
        &["-af", "aformat=channel_layouts=3.0"],
        tags,
        None,
    );
    assert!(
        failure(convertible(
            &music_protocol::media::mpeg::read(&surround).unwrap()
        ))
        .contains("3 channels")
    );
    let other = music_protocol::media::mpeg::read(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tone-wav.wav"),
    )
    .unwrap();
    assert!(failure(convertible(&other)).contains("only a FLAC"));
    // An ALAC of other audio or with other tags is never taken for a FLAC's.
    let song = flac(&temp, "song.flac", &["-ac", "2"], tags, None);
    let retitled = flac(
        &temp,
        "retitled.flac",
        &["-ac", "2"],
        &[("TITLE", "Another"), TAGS[1], TAGS[2]],
        None,
    );
    let louder = flac(
        &temp,
        "louder.flac",
        &["-ac", "2", "-af", "volume=0.5"],
        tags,
        None,
    );
    let directory = Directory::create_in(&temp.0).unwrap();
    let converted = |path: &Path| convert(&ffmpeg, path, &prepared(path), &directory).unwrap();
    let (song_alac, retitled_alac, louder_alac) =
        (converted(&song), converted(&retitled), converted(&louder));
    check(&prepared(&song), &song, &song_alac.path, None, &|| false).unwrap();
    assert!(
        failure(check(
            &prepared(&song),
            &song,
            &retitled_alac.path,
            None,
            &|| false
        ))
        .contains("title")
    );
    assert!(
        failure(check(
            &prepared(&song),
            &song,
            &louder_alac.path,
            None,
            &|| false
        ))
        .contains("other PCM")
    );
}

#[test]
fn a_sweep_removes_left_conversions_but_one_an_unsettled_batch_needs() {
    let temp = Temp::new();
    let left = Directory::create_in(&temp.0).unwrap();
    fs::write(left.path().join("a.m4a"), b"x").unwrap();
    let needed = Directory::create_in(&temp.0).unwrap();
    let source = needed.path().join("b.m4a");
    fs::write(&source, b"y").unwrap();
    let unrelated = temp.path("rustunes-other");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&unrelated)
        .unwrap();
    sweep_in(&temp.0, std::slice::from_ref(&source)).unwrap();
    assert!(!left.path().exists());
    assert!(source.exists());
    assert!(unrelated.exists());
}

#[test]
fn a_conversion_that_is_gone_is_made_again_only_as_the_file_sent() {
    let ffmpeg = require_ffmpeg().unwrap();
    let temp = Temp::new();
    let path = flac(&temp, "song.flac", &["-ac", "2"], &TAGS, Some("cover.jpg"));
    let directory = Directory::create_in(&temp.0).unwrap();
    let converted = convert(&ffmpeg, &path, &prepared(&path), &directory).unwrap();
    let sent: music_protocol::media::fingerprint::TransferredSource =
        converted.fingerprint.clone().into();
    let bytes = fs::read(&converted.path).unwrap();
    remove_left(&converted.path).unwrap();
    // The directory went with its last file.
    assert!(!directory.path().exists());
    let directory = Directory::create_in(&temp.0).unwrap();
    let again = reconvert(&converted.conversion, &sent, &directory).unwrap();
    assert_eq!(fs::read(&again).unwrap(), bytes);
    // Settling removes it, and its directory with it; the run's own cleanup
    // then finds that directory gone, and a file already gone is no error.
    remove_left(&again).unwrap();
    remove_left(&converted.path).unwrap();
    assert!(!directory.path().exists());
    directory.finish().unwrap();
    let directory = Directory::create_in(&temp.0).unwrap();
    // Another FFmpeg, another file than the one sent, or a changed FLAC is
    // no ground to call it the same.
    let other = Conversion {
        ffmpeg: "ffmpeg version 0".into(),
        ..converted.conversion.clone()
    };
    assert!(failure(reconvert(&other, &sent, &directory)).contains("FFmpeg is now"));
    let mut elsewhere = sent.clone();
    elsewhere.full_file_sha256 = [7; 32];
    assert!(
        failure(reconvert(&converted.conversion, &elsewhere, &directory)).contains("another file")
    );
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    let retagged = flac(
        &temp,
        "retagged.flac",
        &["-ac", "2"],
        &TAGS[..3],
        Some("cover.jpg"),
    );
    fs::rename(&retagged, &path).unwrap();
    assert!(failure(reconvert(&converted.conversion, &sent, &directory)).contains("changed since"));
}

#[test]
fn settling_a_killed_runs_batch_removes_what_it_converted_ahead() {
    let temp = Temp::new();
    // A run that ended (no such process) left its batch's conversion and
    // one it made ahead for the next batch.
    let ended = temp.path(&format!("{PREFIX}4000000000-1"));
    fs::DirBuilder::new().mode(0o700).create(&ended).unwrap();
    fs::write(ended.join("sent.m4a"), b"x").unwrap();
    fs::write(ended.join("ahead.m4a"), b"y").unwrap();
    remove_left(&ended.join("sent.m4a")).unwrap();
    assert!(!ended.exists());
    // A run still going (this one) keeps what it made ahead.
    let going = Directory::create_in(&temp.0).unwrap();
    fs::write(going.path().join("sent.m4a"), b"x").unwrap();
    fs::write(going.path().join("ahead.m4a"), b"y").unwrap();
    remove_left(&going.path().join("sent.m4a")).unwrap();
    assert!(going.path().join("ahead.m4a").exists());
    assert!(!going.path().join("sent.m4a").exists());
}

/// Run an hour of tone, read in real time, abandon it once FFmpeg has been
/// started, and check that FFmpeg is gone. No time limit is involved, so a slow
/// machine cannot fail it: an FFmpeg left to finish would keep `run` waiting for
/// the hour instead.
fn assert_abandoning_kills_ffmpeg(format: &str, output: Option<&mut (u64, Sha256)>) {
    let temp = Temp::new();
    let started = temp.path("pid");
    let mut command = Command::new("sh");
    // The shell records its process id, which FFmpeg then keeps by taking over
    // the process.
    command.args([
        "-c",
        r#"echo $$ > "$1.new" && mv "$1.new" "$1" && shift && exec ffmpeg "$@""#,
        "sh",
    ]);
    command.arg(&started);
    command.args([
        "-nostdin",
        "-hide_banner",
        "-v",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=3600",
        "-f",
        format,
        "-",
    ]);
    let abandoned = || started.exists();
    let failure = format!("{:#}", run(command, output, &abandoned).unwrap_err());
    assert!(failure.contains("stopped"), "{failure}");
    // `run` waited for the process, so it is gone, not left running or a zombie.
    let pid = fs::read_to_string(&started).unwrap();
    assert!(!Path::new("/proc").join(pid.trim()).exists());
}

#[test]
fn an_abandoned_ffmpeg_is_killed() {
    assert_abandoning_kills_ffmpeg("null", None);
    // The same while its output is read.
    assert_abandoning_kills_ffmpeg("s32le", Some(&mut (0, Sha256::new())));
}
