use super::*;
use std::{
    io::{Cursor, Write},
    os::unix::fs::{DirBuilderExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "music-protocol-pcm-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/silence-alac.m4a")
}
fn source_timing(path: &Path) -> Timing {
    timing::read(&mut File::open(path).unwrap()).unwrap()
}
fn fingerprint(path: &Path) -> DecodedFingerprint {
    read(path, &source_timing(path)).unwrap()
}
fn ffmpeg(command: &mut Command) {
    let output = command.env_remove("FFREPORT").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn encode(raw: &Path, path: &Path, order: &str) {
    ffmpeg(
        Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-v",
                "error",
                "-xerror",
                "-f",
                "s16le",
                "-ar",
                "44100",
                "-ac",
                "2",
                "-i",
            ])
            .arg(raw)
            .args([
                "-c:a",
                "alac",
                "-sample_fmt",
                "s16p",
                "-min_prediction_order",
                order,
                "-max_prediction_order",
                order,
            ])
            .arg(path),
    );
}

#[test]
fn retagged_and_differently_encoded_alac_have_identical_pcm_but_changed_samples_do_not() {
    let temp = Temp::new();
    let mut samples = Vec::new();
    for index in 0..44_100_i32 {
        // Deterministic non-silent, distinct left/right samples, one second.
        samples.extend_from_slice(&(((index * 313) % 32_000 - 16_000) as i16).to_le_bytes());
        samples.extend_from_slice(&(((index * 117) % 28_000 - 14_000) as i16).to_le_bytes());
    }
    let raw = temp.path("source.raw");
    fs::write(&raw, &samples).unwrap();
    let original = temp.path("original.m4a");
    let retagged = temp.path("retagged.m4a");
    let reencoded = temp.path("reencoded.m4a");
    encode(&raw, &original, "1");
    encode(&raw, &reencoded, "10");
    ffmpeg(
        Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-v", "error", "-i"])
            .arg(&original)
            .args(["-c", "copy", "-metadata", "title=Retagged same samples"])
            .arg(&retagged),
    );
    let before: [u8; 32] = Sha256::digest(fs::read(&original).unwrap()).into();
    let a = fingerprint(&original);
    let tags = fingerprint(&retagged);
    let b = fingerprint(&reencoded);
    assert!(a.pcm == tags.pcm && a.pcm == b.pcm);
    assert_ne!(a.full_file_sha256, tags.full_file_sha256);
    assert_ne!(
        crate::media::fingerprint::read(&original)
            .unwrap()
            .audio
            .sha256,
        crate::media::fingerprint::read(&reencoded)
            .unwrap()
            .audio
            .sha256,
    );
    assert_eq!(a.pcm.pcm_sha256, <[u8; 32]>::from(Sha256::digest(&samples)));
    assert_eq!(a.pcm.pcm_bytes, 176_400);
    assert_eq!(a.pcm.samples_per_channel, 44_100);
    let mut domain = Sha256::new();
    domain.update(b"music-protocol/decoded-pcm\0");
    domain.update(44_100_u32.to_be_bytes());
    domain.update([16, 2]);
    domain.update(44_100_u64.to_be_bytes());
    domain.update(Sha256::digest(&samples));
    assert_eq!(a.pcm.sha256, <[u8; 32]>::from(domain.finalize()));
    samples[333] ^= 1;
    fs::write(&raw, samples).unwrap();
    let changed = temp.path("changed.m4a");
    encode(&raw, &changed, "1");
    assert!(a.pcm != fingerprint(&changed).pcm);
    assert_eq!(
        before,
        <[u8; 32]>::from(Sha256::digest(fs::read(&original).unwrap()))
    );
}

#[test]
fn validated_timing_actual_profile_and_source_shape_must_agree() {
    let original = fixture();
    let timing = source_timing(&original);
    let mut wrong = source_timing(&original);
    wrong.duration_ticks += 1;
    assert!(read(&original, &wrong).is_err());
    assert_eq!(
        fingerprint(&original).pcm.pcm_bytes,
        timing.duration_ticks * 4
    );
    let temp = Temp::new();
    let link = temp.path("symlink.m4a");
    symlink(&original, &link).unwrap();
    assert!(read(&link, &timing).is_err());
    let oversized = temp.path("oversized.m4a");
    File::create(&oversized)
        .unwrap()
        .set_len(MAX_FILE + 1)
        .unwrap();
    assert!(read(&oversized, &timing).is_err());
    let mono = temp.path("mono.m4a");
    ffmpeg(
        Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-v", "error", "-i"])
            .arg(&original)
            .args(["-ac", "1", "-c:a", "alac"])
            .arg(&mono),
    );
    assert!(read(&mono, &source_timing(&mono)).is_err());
    let overflow = Timing {
        timescale: 44_100,
        duration_ticks: u64::MAX,
        packets: 1,
    };
    assert!(expected_bytes(&overflow).is_err());
}

#[test]
fn changed_path_or_same_size_mutation_invalidates_the_held_source() {
    let temp = Temp::new();
    let path = temp.path("source.m4a");
    fs::copy(fixture(), &path).unwrap();
    let file = File::open(&path).unwrap();
    let before = file.metadata().unwrap();
    let replacement = temp.path("replacement.m4a");
    fs::copy(&path, &replacement).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(stable(&path, &file, &before).is_err());
    let file = File::open(&path).unwrap();
    let before = file.metadata().unwrap();
    let mut changed = fs::OpenOptions::new().write(true).open(&path).unwrap();
    changed.write_all(b"changed!").unwrap();
    assert!(stable(&path, &file, &before).is_err());
}

#[test]
fn corrupted_media_is_a_decode_error_not_a_successful_short_hash() {
    let temp = Temp::new();
    let path = temp.path("corrupt.m4a");
    let mut bytes = fs::read(fixture()).unwrap();
    let media = bytes.windows(4).position(|part| part == b"mdat").unwrap();
    let size = u32::from_be_bytes(bytes[media - 4..media].try_into().unwrap()) as usize;
    bytes[media + 4..media - 4 + size].fill(0xff);
    fs::write(&path, bytes).unwrap();
    let timing = source_timing(&path);
    let error = read(&path, &timing).err().unwrap();
    assert!(format!("{error:#}").contains("PCM"));
}

#[test]
fn stream_bounds_and_diagnostics_are_exact_and_drained() {
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(hash_exact(&mut Cursor::new([1; 7]), 8, deadline).is_err());
    assert!(hash_exact(&mut Cursor::new([1; 9]), 8, deadline).is_err());
    let mut input = Cursor::new(vec![b'x'; MAX_LOG + 90_000]);
    let log = drain_log(&mut input).unwrap();
    assert_eq!(log.bytes.len(), MAX_LOG);
    assert!(log.truncated);
    assert_eq!(input.position(), (MAX_LOG + 90_000) as u64);
}

#[test]
fn deadline_and_excess_stdout_kill_reap_and_join_without_waiting_for_voluntary_exit() {
    let mut failed = Command::new("false");
    failed.stdin(Stdio::null());
    let error = decode(
        &mut failed,
        0,
        Instant::now() + Duration::from_secs(2),
        Duration::from_millis(50),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("decode failed"));
    let start = Instant::now();
    let mut sleeper = Command::new("sleep");
    sleeper.arg("5").stdin(Stdio::null());
    assert!(
        decode(
            &mut sleeper,
            1,
            start + Duration::from_millis(150),
            Duration::from_millis(50)
        )
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    // `yes` runs until its pipe closes or it is killed: one expected byte must
    // reject extra output immediately, then close every worker and reap it.
    let start = Instant::now();
    let mut excess = Command::new("yes");
    excess.stdin(Stdio::null());
    let error = decode(
        &mut excess,
        1,
        start + Duration::from_secs(2),
        Duration::from_millis(50),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("exceeds expected byte count"));
    assert!(start.elapsed() < Duration::from_secs(1));
}
