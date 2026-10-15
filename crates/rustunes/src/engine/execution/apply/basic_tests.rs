//! Offline tests of `retain_alac_source`: the private copy of an ALAC original
//! that apply keeps before transfer.
use super::*;
use crate::engine::execution::native_insert;
use crate::engine::planning::source_collection::{self, SelectionOptions};
use music_protocol::protocol::trace::private_write;
use std::sync::atomic::AtomicBool;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::time::Instant;

fn directory(p: &Path) {
    fs::DirBuilder::new().mode(0o700).create(p).unwrap();
}

struct RetainedAlacFixture {
    directory: PathBuf,
    keep: bool,
}
impl RetainedAlacFixture {
    fn new(case: &str) -> Self {
        let retained_root = std::env::var_os("ALAC_RETENTION_CASE_ROOT").map(PathBuf::from);
        let root = retained_root.clone().unwrap_or_else(std::env::temp_dir);
        let directory = root.join(format!(
            "alac-retention-{case}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        Self {
            directory,
            keep: retained_root.is_some(),
        }
    }
    fn original(&self) -> PathBuf {
        let path = self.directory.join("source.m4a");
        fs::write(
            &path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
        )
        .unwrap();
        path
    }
    fn step(&self) -> PathBuf {
        let step = self.directory.join("step-0001");
        directory(&step);
        step
    }
}
impl Drop for RetainedAlacFixture {
    fn drop(&mut self) {
        if !self.keep {
            fs::remove_dir_all(&self.directory).unwrap();
        }
    }
}
fn alac_title_edit(bytes: &[u8], title: &str) -> Vec<u8> {
    use lofty::{
        config::{ParseOptions, WriteOptions},
        file::AudioFile,
        mp4::{Atom, AtomData, AtomIdent, Mp4File},
    };
    let mut input = std::io::Cursor::new(bytes.to_vec());
    let mut file = Mp4File::read_from(&mut input, ParseOptions::default()).unwrap();
    file.ilst_mut().unwrap().replace_atom(Atom::new(
        AtomIdent::Fourcc(*b"\xa9nam"),
        AtomData::UTF8(title.into()),
    ));
    file.save_to(&mut input, WriteOptions::default()).unwrap();
    input.into_inner()
}

fn retention_control() -> Control {
    Control {
        deadline: Instant::now() + Duration::from_secs(180),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn alac_retention_second_copy_collision_preserves_original() {
    let f = RetainedAlacFixture::new("second-copy");
    let original = f.original();
    let expected = music_protocol::media::fingerprint::read(&original).unwrap();
    let step = f.step();
    let retained = step.join("retained-source.m4a");
    retain_alac_source(&original, &expected, &step, &retention_control()).unwrap();
    let bytes = fs::read(&retained).unwrap();
    assert_eq!(bytes, fs::read(&original).unwrap());
    assert!(retain_alac_source(&original, &expected, &step, &retention_control()).is_err());
    assert_eq!(fs::read(&retained).unwrap(), bytes);
    assert_eq!(fs::metadata(&retained).unwrap().nlink(), 1);
    assert!(!step.join("input.json").exists());
}

#[test]
fn alac_retention_leaf_collisions_do_not_overwrite_or_follow_aliases() {
    for kind in ["regular", "symlink", "hardlink", "directory"] {
        let f = RetainedAlacFixture::new(kind);
        let original = f.original();
        let expected = music_protocol::media::fingerprint::read(&original).unwrap();
        let step = f.step();
        let retained = step.join("retained-source.m4a");
        let target = f.directory.join("existing-target");
        fs::write(&target, b"existing evidence").unwrap();
        match kind {
            "regular" => fs::write(&retained, b"prior retained evidence").unwrap(),
            "symlink" => std::os::unix::fs::symlink(&target, &retained).unwrap(),
            "hardlink" => fs::hard_link(&target, &retained).unwrap(),
            "directory" => directory(&retained),
            _ => unreachable!(),
        }
        assert!(
            retain_alac_source(&original, &expected, &step, &retention_control()).is_err(),
            "{kind}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"existing evidence");
        if kind == "regular" {
            assert_eq!(fs::read(&retained).unwrap(), b"prior retained evidence");
        }
        if kind == "symlink" {
            assert!(
                fs::symlink_metadata(&retained)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        if kind == "hardlink" {
            assert_eq!(fs::metadata(&target).unwrap().nlink(), 2);
        }
        assert!(!step.join("input.json").exists());
    }
}

#[test]
fn alac_retention_source_mismatch_and_source_symlink_fail_before_writing() {
    for kind in ["edited", "symlink", "missing"] {
        let f = RetainedAlacFixture::new(kind);
        let original = f.original();
        let bytes = fs::read(&original).unwrap();
        let expected = music_protocol::media::fingerprint::read(&original).unwrap();
        let step = f.step();
        match kind {
            "edited" => fs::write(
                &original,
                alac_title_edit(&bytes, "Changed before retention"),
            )
            .unwrap(),
            "symlink" => {
                let target = f.directory.join("replacement.m4a");
                fs::rename(&original, &target).unwrap();
                std::os::unix::fs::symlink(target, &original).unwrap();
            }
            "missing" => fs::remove_file(&original).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            retain_alac_source(&original, &expected, &step, &retention_control()).is_err(),
            "{kind}"
        );
        assert!(!step.join("retained-source.m4a").exists());
        assert!(!step.join("input.json").exists());
    }
}

#[test]
fn alac_retention_full_descriptor_mismatch_keeps_failed_local_evidence() {
    let f = RetainedAlacFixture::new("descriptor-mismatch");
    let original = f.original();
    let original_bytes = fs::read(&original).unwrap();
    let mut expected = music_protocol::media::fingerprint::read(&original).unwrap();
    // The full file is exact; the retained parser must also match every typed
    // ALAC descriptor field, rather than trusting only the full-file digest.
    expected.audio.sha256[0] ^= 1;
    let step = f.step();
    let error = retain_alac_source(&original, &expected, &step, &retention_control()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("source changed while retaining ALAC")
    );
    assert_eq!(
        fs::read(step.join("retained-source.m4a")).unwrap(),
        original_bytes
    );
    assert!(!step.join("input.json").exists());
    assert!(retain_alac_source(&original, &expected, &step, &retention_control()).is_err());
    assert_eq!(
        fs::read(step.join("retained-source.m4a")).unwrap(),
        original_bytes
    );
}

#[test]
fn alac_retention_step_alias_type_and_private_mode_refuse_before_writing() {
    for kind in ["symlink", "regular", "missing", "public-mode"] {
        let f = RetainedAlacFixture::new(kind);
        let original = f.original();
        let expected = music_protocol::media::fingerprint::read(&original).unwrap();
        let step = f.directory.join("step-0001");
        let target = f.directory.join("step-target");
        match kind {
            "symlink" => {
                directory(&target);
                std::os::unix::fs::symlink(&target, &step).unwrap();
            }
            "regular" => fs::write(&step, b"not a directory").unwrap(),
            "missing" => (),
            "public-mode" => {
                directory(&step);
                fs::set_permissions(&step, fs::Permissions::from_mode(0o755)).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            retain_alac_source(&original, &expected, &step, &retention_control()).is_err(),
            "{kind}"
        );
        assert!(!step.join("retained-source.m4a").exists());
        assert!(!target.join("retained-source.m4a").exists());
    }
}

#[test]
fn alac_retention_expired_or_cancelled_control_refuses_without_copy() {
    for cancelled in [false, true] {
        let f = RetainedAlacFixture::new(if cancelled { "cancelled" } else { "expired" });
        let original = f.original();
        let expected = music_protocol::media::fingerprint::read(&original).unwrap();
        let step = f.step();
        let control = Control {
            deadline: if cancelled {
                Instant::now() + Duration::from_secs(180)
            } else {
                Instant::now()
            },
            cancelled: Arc::new(AtomicBool::new(cancelled)),
        };
        let error = retain_alac_source(&original, &expected, &step, &control).unwrap_err();
        assert!(error.to_string().contains(if cancelled {
            "cancelled"
        } else {
            "deadline expired"
        }));
        assert!(!step.join("retained-source.m4a").exists());
        assert!(!step.join("input.json").exists());
    }
}

#[test]
fn alac_retention_existing_native_size_bound_refuses_before_reading() {
    let f = RetainedAlacFixture::new("size-bound");
    let original = f.original();
    let mut expected = music_protocol::media::fingerprint::read(&original).unwrap();
    expected.file_bytes = music_protocol::protocol::transfer::MAX_AUDIO + 1;
    fs::OpenOptions::new()
        .write(true)
        .open(&original)
        .unwrap()
        .set_len(expected.file_bytes)
        .unwrap();
    let step = f.step();
    let error = retain_alac_source(&original, &expected, &step, &retention_control()).unwrap_err();
    assert!(error.to_string().contains("source type/size differs"));
    assert!(!step.join("retained-source.m4a").exists());
}

#[test]
fn alac_retention_replaced_after_fingerprint_cannot_finish_successfully() {
    let f = RetainedAlacFixture::new("replaced-before-finalization");
    let original = f.original();
    let original_bytes = fs::read(&original).unwrap();
    let expected = music_protocol::media::fingerprint::read(&original).unwrap();
    let step = f.step();
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&step)
        .unwrap();
    let held_directory = directory.metadata().unwrap();
    let original_binding = native_insert::BoundSource::open(&original, &expected).unwrap();
    let retained = step.join("retained-source.m4a");
    private_write(&retained, &original_bytes).unwrap();
    assert!(music_protocol::media::fingerprint::read(&retained).unwrap() == expected);
    let retained_binding = native_insert::BoundSource::open(&retained, &expected).unwrap();
    // This replacement is a private regular unaliased ALAC file. The old
    // type/nlink/mode checks cannot establish continuity after fingerprinting.
    let replacement = f.directory.join("replacement.m4a");
    let replacement_bytes = alac_title_edit(&original_bytes, "Replacement after fingerprint");
    private_write(&replacement, &replacement_bytes).unwrap();
    fs::rename(&replacement, &retained).unwrap();
    assert!(
        finish_alac_retention(
            &step,
            &directory,
            &held_directory,
            &original_binding,
            &retained_binding,
            &retention_control()
        )
        .is_err(),
        "retained path replacement after fingerprint must fail finalization"
    );
    assert_eq!(fs::read(&retained).unwrap(), replacement_bytes);
    assert!(!step.join("input.json").exists());
}

#[test]
fn ordinary_alac_retains_exact_original_after_later_title_edit() {
    let f = RetainedAlacFixture::new("ordinary-title");
    let original = f.original();
    let original_bytes = fs::read(&original).unwrap();
    let selected = source_collection::select(&original, &SelectionOptions::default()).unwrap();
    let sources = source_collection::inspect(&selected).unwrap();
    sources.ensure_unambiguous().unwrap();
    assert_eq!(sources.tracks.len(), 1);
    let expected = &sources.tracks[0].fingerprint;
    let step = f.step();
    retain_alac_source(&original, expected, &step, &retention_control()).unwrap();
    let retained = step.join("retained-source.m4a");
    assert!(
        retained.is_file(),
        "ordinary ALAC Add must retain original transferred tag bytes"
    );
    assert_eq!(fs::read(&retained).unwrap(), original_bytes);
    assert!(music_protocol::media::fingerprint::read(&retained).unwrap() == *expected);
    assert_eq!(
        fs::metadata(&retained).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::write(
        &original,
        alac_title_edit(&original_bytes, "Later desired title"),
    )
    .unwrap();
    let desired = music_protocol::media::fingerprint::read(&original).unwrap();
    assert_ne!(desired.full_file_sha256, expected.full_file_sha256);
    assert!(desired.audio == expected.audio);
    assert_eq!(fs::read(&retained).unwrap(), original_bytes);
    assert!(music_protocol::media::fingerprint::read(&retained).unwrap() == *expected);
    assert_eq!(
        music_protocol::media::metadata::read(&original)
            .unwrap()
            .title,
        vec!["Later desired title"]
    );
    assert!(!step.join("input.json").exists());
}
