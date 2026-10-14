use super::*;
use std::{
    fs,
    time::{Duration, Instant},
};

const UDID: &str = "00008030-001A2B3C4D5E6F70";

/// A private scratch directory of this test's own.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("rustunes-lock-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    layout::private_directory(&directory).unwrap();
    directory
}

fn refused(process: &str) -> String {
    format!(
        "another rustunes run{process} is using this phone; wait for it to finish, then run this command again"
    )
}

/// Take the lock at `path` once the descriptors that held it have all closed.
/// A lock belongs to the open file, not to the descriptor a test dropped: a
/// child another test is spawning holds a copy of every open descriptor until
/// it execs, so the lock comes free a moment after the drop.
fn locked_once_free(path: &Path) -> File {
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        match locked(path) {
            Err(error)
                if Instant::now() < give_up
                    && error.to_string().starts_with("another rustunes run") =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            result => return result.unwrap(),
        }
    }
}

#[test]
fn a_second_run_is_refused_naming_the_first_until_it_lets_go() {
    let directory = scratch("refused");
    let path = path(&directory, UDID).unwrap();
    assert!(!path.to_str().unwrap().contains("001A2B3C4D5E6F70"));
    let first = locked(&path).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{}\n", std::process::id())
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Each open is a run of its own to the lock, even in one process.
    assert_eq!(
        locked(&path).unwrap_err().to_string(),
        refused(&format!(" (process {})", std::process::id()))
    );
    // Before the holder has written its process.
    first.set_len(0).unwrap();
    assert_eq!(locked(&path).unwrap_err().to_string(), refused(""));
    drop(first);
    let _again = locked_once_free(&path);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn a_lock_that_is_shared_or_a_symlink_is_refused() {
    let directory = scratch("unsafe");
    let shared = directory.join("shared");
    fs::write(&shared, "").unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o644)).unwrap();
    let error = locked(&shared).unwrap_err().to_string();
    assert!(
        error.starts_with("a phone's lock must be a private regular file"),
        "{error}"
    );
    let link = directory.join("link");
    std::os::unix::fs::symlink(directory.join("elsewhere"), &link).unwrap();
    let error = locked(&link).unwrap_err().to_string();
    assert!(error.starts_with("opening the phone's lock"), "{error}");
    assert!(!directory.join("elsewhere").exists());
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn a_process_takes_each_phone_once_and_keeps_it() {
    let directory = scratch("kept").join("locks");
    take(&directory, UDID).unwrap();
    take(&directory, UDID).unwrap();
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(locked(&path(&directory, UDID).unwrap()).is_err());
    take(&directory, "00008030-0000000000000001").unwrap();
}
