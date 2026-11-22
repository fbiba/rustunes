//! Removing a song file no track names: only a song file directly in a music
//! folder, only while it is the file the caller found, and a locked
//! connection can do it and nothing more.
use super::*;
use crate::test_support::afc::{FakePhone, Node};

const PATH: &str = "iTunes_Control/Music/F12/QZXW.m4a";
const BYTES: &[u8] = b"the audio of a song deleted while it was copied";

/// The phone holding `PATH` with `BYTES`, and what a stat of it shows.
fn holding() -> (FakePhone, Attributes) {
    let mut phone = FakePhone::new(&[
        "iTunes_Control",
        "iTunes_Control/Music",
        "iTunes_Control/Music/F12",
    ]);
    phone.file(PATH, BYTES);
    phone.set_attribute(PATH, "st_mtime", Some("1800000000123456789"));
    let found = phone.stat(PATH).unwrap();
    (phone, found)
}

async fn run(
    phone: FakePhone,
    path: &str,
    found: &Attributes,
) -> (Result<LeftoverRemoval>, FakePhone) {
    let trace = Temporary::new();
    // Locked: no Started permit unlocked it.
    let (mut transfer, peer) = pair(&trace.0, Duration::from_secs(5));
    let server = tokio::spawn(phone.serve(peer));
    let result = transfer.remove_leftover_media(path, found).await;
    drop(transfer);
    (result, server.await.unwrap())
}

#[tokio::test]
async fn the_file_found_is_removed_by_a_locked_connection() {
    let (phone, found) = holding();
    let (result, phone) = run(phone, PATH, &found).await;
    assert_eq!(result.unwrap(), LeftoverRemoval::Removed);
    assert!(!phone.tree.contains_key(PATH));
}

#[tokio::test]
async fn a_file_gone_or_changed_since_it_was_found_is_left_alone() {
    let (mut phone, found) = holding();
    phone.tree.remove(PATH);
    let (result, phone) = run(phone, PATH, &found).await;
    assert_eq!(result.unwrap(), LeftoverRemoval::Gone);
    assert!(!phone.log.contains(&AfcOpcode::RemovePath));

    let changes: [(&str, Option<&str>); 2] = [
        ("st_mtime", Some("1800000000123456790")),
        ("st_ifmt", Some("S_IFLNK")),
    ];
    for (key, value) in changes {
        let (mut phone, found) = holding();
        phone.set_attribute(PATH, key, value);
        let (result, phone) = run(phone, PATH, &found).await;
        assert_eq!(result.unwrap(), LeftoverRemoval::Changed, "{key}");
        assert!(phone.tree.contains_key(PATH));
        assert!(!phone.log.contains(&AfcOpcode::RemovePath));
    }
    // Rewritten with other bytes.
    let (mut phone, found) = holding();
    phone.file(PATH, b"another song");
    phone.set_attribute(PATH, "st_mtime", Some("1800000000123456789"));
    let (result, phone) = run(phone, PATH, &found).await;
    assert_eq!(result.unwrap(), LeftoverRemoval::Changed);
    assert!(phone.tree.contains_key(PATH));
    // A folder where the file was.
    let (mut phone, found) = holding();
    phone.tree.insert(PATH.to_owned(), Node::Directory);
    let (result, phone) = run(phone, PATH, &found).await;
    assert_eq!(result.unwrap(), LeftoverRemoval::Changed);
    assert_eq!(phone.tree.get(PATH), Some(&Node::Directory));
}

/// Nothing but a song file directly in a music folder is ever removed, and
/// nothing is even asked of the phone for another path.
#[tokio::test]
async fn only_a_song_file_in_a_music_folder_is_ever_removed() {
    for path in [
        "Music/Cache/QZXW.m4a",
        "Music/Cache_LoFi/QZXW.m4a",
        "iTunes_Control/Music/F12/QZXW.plist",
        "iTunes_Control/Music/F12/QZXW.m4v",
        "iTunes_Control/Music/F12",
        "iTunes_Control/Music/F5/QZXW.m4a",
        "iTunes_Control/Music/FAB/QZXW.m4a",
        "iTunes_Control/Music/F12/sub/QZXW.m4a",
        "iTunes_Control/Music/F12/.m4a",
        "iTunes_Control/Music/Cache/QZXW.m4a",
        "iTunes_Control/iTunes/MediaLibrary.sqlitedb",
        "Airlock/Media/123.m4a",
        "iTunes_Control/Sync/Media/Sync_00000000.plist",
    ] {
        let (mut phone, found) = holding();
        phone.file(path, BYTES);
        let (result, phone) = run(phone, path, &found).await;
        assert!(result.is_err(), "{path}");
        assert!(phone.log.is_empty(), "{path}: {:?}", phone.log);
    }
    for path in [
        "iTunes_Control/Music/F00/ABCD.MP3",
        "iTunes_Control/Music/F49/ABCD 1.m4a",
        "iTunes_Control/Music/F40/RTMN.flac",
        "iTunes_Control/Music/F07/WXYZ.Aif",
    ] {
        assert!(leftover_media_path(path), "{path}");
    }
}
