//! Which files are leftover song files: only old enough regular song files
//! in Apple's music folders, or ones the library registers, that no track
//! names, compared without regard to case.
use super::*;

const NOW: i128 = 1_800_000_000_000_000_000;
const SECOND: i128 = 1_000_000_000;

fn stat(kind: &str, bytes: u64, modified: i128) -> Attributes {
    [
        ("st_ifmt".to_owned(), kind.to_owned()),
        ("st_size".to_owned(), bytes.to_string()),
        ("st_mtime".to_owned(), modified.to_string()),
        ("st_birthtime".to_owned(), modified.to_string()),
    ]
    .into_iter()
    .collect()
}

fn found(listed: &[(&str, Attributes)], named: &[&str]) -> Vec<String> {
    let named = named.iter().map(|p| p.to_ascii_lowercase()).collect();
    let listed = listed
        .iter()
        .map(|(path, attributes)| ((*path).to_owned(), attributes.clone()))
        .collect();
    leftovers(listed, &named, NOW, MINIMUM_AGE)
        .into_iter()
        .map(|f| f.path)
        .collect()
}

#[test]
fn an_old_song_file_no_track_names_is_a_leftover() {
    let old = NOW - 301 * SECOND;
    let files = leftovers(
        vec![(
            "iTunes_Control/Music/F12/QZXW.m4a".to_owned(),
            stat("S_IFREG", 41_872_160, old),
        )],
        &BTreeSet::new(),
        NOW,
        MINIMUM_AGE,
    );
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].bytes, 41_872_160);
    assert_eq!(files[0].modified, old);
    assert_eq!(total_bytes(&files), 41_872_160);
}

/// Each rule on its own keeps a file: a track names it (in any case), it
/// is too new by the clock given, it is not a regular file, it is not a
/// song file, or it is not directly in a music folder.
#[test]
fn every_other_file_is_kept() {
    let old = NOW - 301 * SECOND;
    let listed = [
        ("iTunes_Control/Music/F02/LIVE.m4a", stat("S_IFREG", 9, old)),
        (
            "iTunes_Control/Music/F03/NEWS.m4a",
            stat("S_IFREG", 9, NOW - 299 * SECOND),
        ),
        ("iTunes_Control/Music/F04/LINK.m4a", stat("S_IFLNK", 9, old)),
        (
            "iTunes_Control/Music/F05/DIRS.m4a",
            stat("S_IFDIR", 64, old),
        ),
        (
            "iTunes_Control/Music/F06/SIDE.plist",
            stat("S_IFREG", 9, old),
        ),
        ("iTunes_Control/Music/F07/FILM.m4v", stat("S_IFREG", 9, old)),
        ("Music/Cache/CACH.m4a", stat("S_IFREG", 9, old)),
        ("Music/Cache_LoFi/LOFI.m4a", stat("S_IFREG", 9, old)),
        ("Music/Downloads/DOWN.m4a", stat("S_IFREG", 9, old)),
        ("Airlock/Media/123.m4a", stat("S_IFREG", 9, old)),
        ("iTunes_Control/Music/F08/GONE.mp3", stat("S_IFREG", 9, old)),
    ];
    assert_eq!(
        found(&listed, &["iTunes_Control/Music/F02/live.M4A"]),
        ["iTunes_Control/Music/F08/GONE.mp3"]
    );
    // A stat without a size or a time is no file to judge.
    let mut partial = stat("S_IFREG", 9, old);
    partial.remove("st_mtime");
    assert!(found(&[("iTunes_Control/Music/F09/PART.m4a", partial)], &[]).is_empty());
    // By the host's clock, an hour.
    let listed = vec![(
        "iTunes_Control/Music/F10/HOUR.m4a".to_owned(),
        stat("S_IFREG", 9, NOW - 3599 * SECOND),
    )];
    assert!(leftovers(listed, &BTreeSet::new(), NOW, HOST_MINIMUM_AGE).is_empty());
}

#[test]
fn only_apples_fifty_folders_and_registered_ones_are_searched() {
    let names = ["F00", "F49", "F50", "F51", "Cache", "F7", ".", "F12"]
        .map(str::to_owned)
        .to_vec();
    let registered: BTreeSet<String> = ["iTunes_Control/Music/F51".to_owned()].into();
    assert_eq!(
        folders(names, &registered),
        [
            "iTunes_Control/Music/F00",
            "iTunes_Control/Music/F12",
            "iTunes_Control/Music/F49",
            "iTunes_Control/Music/F51",
        ]
    );
}

/// The offer is made once a sync, however often it plans again.
#[test]
fn the_offer_is_made_once() {
    let confirm = |_: &[LeftoverFile]| Ok(true);
    let offer = LeftoverOffer::new(&confirm);
    assert!(offer.first());
    assert!(!offer.first());
}

/// The search lists Apple's music folders and the registered ones, and stats
/// only the song files no track names; then a locked connection removes the
/// leftover one it found and nothing else.
#[tokio::test]
async fn the_folders_are_searched_and_only_the_leftover_is_removed() {
    use crate::engine::state::journal::test_support::Temp;
    use music_protocol::protocol::transfer::CaptureControl;
    use music_protocol::test_support::afc::FakePhone;
    use std::sync::{Arc, atomic::AtomicBool};
    let control = || CaptureControl {
        deadline: Instant::now() + Duration::from_secs(30),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let mut phone = FakePhone::new(&[
        "iTunes_Control",
        "iTunes_Control/Music",
        "iTunes_Control/Music/F02",
        "iTunes_Control/Music/F12",
        "iTunes_Control/Music/F51",
        "iTunes_Control/Music/F77",
        "iTunes_Control/Music/Cache",
    ]);
    let old = (NOW - 3600 * SECOND).to_string();
    for path in [
        "iTunes_Control/Music/F02/LIVE.m4a",
        "iTunes_Control/Music/F12/QZXW.m4a",
        "iTunes_Control/Music/F12/NOTE.plist",
        "iTunes_Control/Music/F51/REGD.mp3",
        "iTunes_Control/Music/F77/ELSE.m4a",
        "iTunes_Control/Music/Cache/CACH.m4a",
    ] {
        phone.file(path, b"audio");
        phone.set_attribute(path, "st_mtime", Some(&old));
    }
    let named: BTreeSet<String> = ["itunes_control/music/f02/live.m4a".to_owned()].into();
    let registered: BTreeSet<String> = ["iTunes_Control/Music/F51".to_owned()].into();
    let trace = Temp::new();
    let (mut afc, server) = phone.connect(&trace.0, &control(), 64).unwrap();
    let listed = unnamed_files(&mut afc, &registered, &named).await.unwrap();
    drop(afc);
    let phone = server.await.unwrap();
    let paths: Vec<_> = listed.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "iTunes_Control/Music/F12/QZXW.m4a",
            "iTunes_Control/Music/F51/REGD.mp3"
        ]
    );
    let files = leftovers(listed, &named, NOW, MINIMUM_AGE);
    assert_eq!(files.len(), 2);

    let trace = Temp::new();
    let (mut afc, server) = phone.connect(&trace.0, &control(), 64).unwrap();
    assert_eq!(
        afc.remove_leftover_media(&files[0].path, &files[0].found)
            .await
            .unwrap(),
        LeftoverRemoval::Removed
    );
    drop(afc);
    let phone = server.await.unwrap();
    assert!(!phone.tree.contains_key("iTunes_Control/Music/F12/QZXW.m4a"));
    for kept in [
        "iTunes_Control/Music/F02/LIVE.m4a",
        "iTunes_Control/Music/F12/NOTE.plist",
        "iTunes_Control/Music/F51/REGD.mp3",
        "iTunes_Control/Music/F77/ELSE.m4a",
        "iTunes_Control/Music/Cache/CACH.m4a",
    ] {
        assert!(phone.tree.contains_key(kept), "{kept}");
    }
}
