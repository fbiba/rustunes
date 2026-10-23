//! Reconciling a capture with an earlier journal and the library, finding
//! the earlier journal, and installing the pulled one.
use super::*;
use crate::engine::state::journal;
use crate::engine::state::journal::{Artwork, PriorBinding, test_support};

fn on_phone(content: &Content) -> OnPhone {
    OnPhone {
        pid: content.track_pid,
        path: content.path.clone(),
        artist: "Artist".into(),
        title: format!("Title {:x}", content.track_pid),
        identity: Some(content.source.clone()),
        file: (content.source.full_file_sha256, content.source.file_bytes),
        cover: None,
    }
}

fn matching(phone: &[OnPhone]) -> impl Fn(u64, &str, &TransferredSource) -> bool + '_ {
    |pid, path, source| {
        phone
            .iter()
            .any(|t| t.pid == pid && t.path == path && t.identity.as_ref() == Some(source))
    }
}

fn prior(bindings: Vec<PriorBinding>) -> Prior {
    Prior {
        bindings,
        reserved: BTreeSet::new(),
        unfinished: Vec::new(),
        unreadable: Vec::new(),
    }
}

fn binding(content: Content, source: Option<PathBuf>) -> PriorBinding {
    PriorBinding {
        content,
        source,
        conversion: None,
    }
}

fn library(files: &[(&str, &Content)]) -> Library {
    Library {
        base: "/library".into(),
        files: files
            .iter()
            .map(|(path, c)| (Path::new("/library").join(path), c.source.clone()))
            .collect(),
        scope: [7; 32],
    }
}

/// `content` re-encoded with other tags: the same audio in another file.
fn retagged(content: &Content, seed: u8) -> Content {
    let mut other = content.clone();
    other.source.full_file_sha256 = [seed; 32];
    other
}

#[test]
fn an_earlier_binding_the_phone_still_holds_is_carried_with_its_cover_and_source() {
    let t = test_support::Temp::new();
    let recorded = t.0.join("kept.m4a");
    fs::write(&recorded, b"").unwrap();
    let mut covered = test_support::alac_content(1);
    covered.artwork = Some(Artwork {
        cache_id: 140,
        bytes: 9,
        sha256: [9; 32],
    });
    covered.collection = None;
    let uncovered = Content {
        artwork: covered.artwork.clone(),
        ..test_support::alac_content(2)
    };
    let mut phone = vec![on_phone(&covered), on_phone(&uncovered)];
    phone[0].cover = Some(140);
    // The phone dropped the second track's cover registration.
    phone[1].cover = None;
    let earlier = prior(vec![
        binding(covered.clone(), Some(recorded.clone())),
        binding(uncovered.clone(), None),
    ]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), None);
    assert_eq!((out.carried, out.matched, out.gone), (2, 0, 0));
    assert!(out.unbound.is_empty());
    let first = &out.pulled[0];
    assert!(first.content == covered);
    assert_eq!(first.source.as_deref(), Some(recorded.as_path()));
    assert!(out.pulled[1].content.artwork.is_none());
    assert!(out.pulled[1].content.source == uncovered.source);
    // Neither needs a readback: one is proven, the other has no cover.
    assert!(out.covers.is_empty());
}

#[test]
fn a_phone_cover_no_record_proves_is_read_back_for_carried_and_matched_tracks() {
    let art = |cache_id| Artwork {
        cache_id,
        bytes: 9,
        sha256: [9; 32],
    };
    // Sent again under another cache ID after the phone was cleared.
    let recached = Content {
        artwork: Some(art(140)),
        ..test_support::alac_content(20)
    };
    // An earlier pull lost its record.
    let unrecorded = test_support::alac_content(21);
    let matched = test_support::alac_content(22);
    let bare = test_support::alac_content(23);
    let mut phone: Vec<_> = [&recached, &unrecorded, &matched, &bare]
        .into_iter()
        .map(on_phone)
        .collect();
    phone[0].cover = Some(700);
    phone[1].cover = Some(701);
    phone[2].cover = Some(702);
    let earlier = prior(vec![
        binding(recached.clone(), None),
        binding(unrecorded.clone(), None),
    ]);
    let lib = library(&[("m.m4a", &matched), ("b.m4a", &bare)]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), Some(&lib));
    assert_eq!((out.carried, out.matched), (2, 2));
    assert!(out.pulled.iter().all(|p| p.content.artwork.is_none()));
    assert_eq!(
        out.covers,
        [
            CoverCandidate {
                pulled: 0,
                cache_id: 700,
                earlier: Some((9, [9; 32])),
            },
            CoverCandidate {
                pulled: 1,
                cache_id: 701,
                earlier: None,
            },
            CoverCandidate {
                pulled: 2,
                cache_id: 702,
                earlier: None,
            },
        ]
    );
    assert_eq!(out.pulled[2].content.track_pid, matched.track_pid);
}

#[test]
fn a_cover_is_proven_against_what_sync_sends_for_its_source_format() {
    let t = test_support::Temp::new();
    // An MP3 with one front cover: its picture, as an insert batch sends it.
    let image = [
        0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 2, 44, 2, 44, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0,
        0xff, 0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0, 0x11, 0xff, 0, 0xff, 0xd9,
    ];
    let data = [b"\x00image/jpeg\x00\x03\x00".as_slice(), &image].concat();
    let len = (data.len() as u32).to_be_bytes();
    let frame = [b"APIC".as_slice(), &len, &[0, 0], &data].concat();
    let n = frame.len();
    let size = [n >> 21, n >> 14, n >> 7, n].map(|part| (part & 127) as u8);
    let audio = music_protocol::test_support::mpeg::audio();
    let mp3 = t.0.join("covered.mp3");
    let tagged = [b"ID3\x03\x00\x00".as_slice(), &size, &frame, &audio].concat();
    fs::write(&mp3, tagged).unwrap();
    let sha256: [u8; 32] = Sha256::digest(&image).into();
    assert_eq!(source_cover(&mp3), Some((image.len() as u64, sha256)));
    // Without a picture, or in a file no reader takes, there is none.
    let bare = t.0.join("bare.mp3");
    fs::write(&bare, &audio).unwrap();
    assert_eq!(source_cover(&bare), None);
    let noise = t.0.join("noise.m4a");
    fs::write(&noise, b"not audio").unwrap();
    assert_eq!(source_cover(&noise), None);
}

#[test]
fn a_converted_track_whose_recorded_source_is_gone_is_carried_with_its_flac() {
    let t = test_support::Temp::new();
    let flac = t.0.join("kept.flac");
    fs::write(&flac, b"").unwrap();
    let converted = |content: Content, flac: &Path| PriorBinding {
        // An insert batch recorded the ALAC it sent, removed since.
        source: Some(t.0.join("sent.m4a")),
        conversion: Some(journal::Conversion {
            source_path: flac.to_owned(),
            source_bytes: 900,
            source_sha256: [1; 32],
            source_audio: [2; 32],
            ffmpeg: "ffmpeg version 8.1.3".into(),
        }),
        ..binding(content, None)
    };
    let kept = test_support::alac_content(10);
    let lost = test_support::alac_content(11);
    let phone = vec![on_phone(&kept), on_phone(&lost)];
    let earlier = prior(vec![
        converted(kept, &flac),
        converted(lost, &t.0.join("gone.flac")),
    ]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), None);
    assert_eq!(out.carried, 2);
    assert_eq!(out.pulled[0].source.as_deref(), Some(flac.as_path()));
    assert!(out.pulled[0].conversion.is_some());
    // With its FLAC gone too, and no library file holding the audio.
    assert_eq!(out.pulled[1].source, None);
}

#[test]
fn a_track_the_phone_numbered_anew_is_carried_by_its_audio() {
    let earlier_content = test_support::alac_content(3);
    let mut now = earlier_content.clone();
    now.track_pid = 77;
    now.path = "iTunes_Control/Music/F01/NEW.m4a".into();
    let phone = vec![on_phone(&now)];
    let earlier = prior(vec![binding(earlier_content, None)]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), None);
    assert_eq!((out.carried, out.gone), (1, 0));
    assert_eq!(out.pulled[0].content.track_pid, 77);
    assert_eq!(out.pulled[0].content.path, now.path);
}

#[test]
fn library_files_are_matched_and_managed_and_the_rest_left_unbound_or_gone() {
    let in_library = test_support::alac_content(4);
    let phone_only = test_support::alac_content(5);
    let removed = test_support::alac_content(6);
    let phone = vec![on_phone(&in_library), on_phone(&phone_only)];
    let earlier = prior(vec![binding(removed, None)]);
    let lib = library(&[("a.m4a", &in_library)]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), Some(&lib));
    assert_eq!((out.carried, out.matched, out.gone), (0, 1, 1));
    let matched = &out.pulled[0];
    assert!(matched.content.source == in_library.source);
    assert_eq!(matched.content.track_pid, in_library.track_pid);
    assert_eq!(matched.content.collection, Some([7; 32]));
    assert_eq!(matched.source.as_deref(), Some(Path::new("/library/a.m4a")));
    assert_eq!(out.unbound.len(), 1);
    assert_eq!(
        out.unbound[0].track_id,
        format!("{:016x}", phone_only.track_pid)
    );
    assert_eq!(out.unbound[0].reason.as_deref(), Some(NOT_IN_LIBRARY));
    // Without a library, nothing is matched.
    let out = reconcile(&phone, matching(&phone), None, None);
    assert_eq!((out.matched, out.unbound.len()), (0, 2));
}

#[test]
fn several_library_files_with_the_audio_match_only_by_their_bytes() {
    let track = test_support::alac_content(8);
    let phone = vec![on_phone(&track)];
    let other = retagged(&track, 99);
    // The same audio twice, once byte for byte the phone's file.
    let lib = library(&[("retagged.m4a", &other), ("exact.m4a", &track)]);
    let out = reconcile(&phone, matching(&phone), None, Some(&lib));
    assert_eq!(out.matched, 1);
    assert_eq!(
        out.pulled[0].source.as_deref(),
        Some(Path::new("/library/exact.m4a"))
    );
    // Two copies of the same file leave it unbound: neither is the one.
    let lib = library(&[("one.m4a", &track), ("two.m4a", &track)]);
    let out = reconcile(&phone, matching(&phone), None, Some(&lib));
    assert_eq!((out.matched, out.unbound.len()), (0, 1));
}

#[test]
fn a_second_phone_track_with_carried_audio_never_displaces_its_binding() {
    let carried = test_support::alac_content(10);
    let mut copy = carried.clone();
    copy.track_pid = 5;
    copy.path = "iTunes_Control/Music/F00/COPY.m4a".into();
    // The copy comes first; the earlier binding still names the original.
    let phone = vec![on_phone(&copy), on_phone(&carried)];
    let earlier = prior(vec![binding(carried.clone(), None)]);
    let lib = library(&[("a.m4a", &carried)]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), Some(&lib));
    assert_eq!((out.carried, out.matched), (1, 0));
    assert_eq!(out.pulled[0].content.track_pid, carried.track_pid);
    assert_eq!(out.unbound.len(), 1);
    assert_eq!(out.unbound[0].track_id, format!("{:016x}", 5));
    assert_eq!(
        out.unbound[0].reason.as_deref(),
        Some("another phone track holds the same audio")
    );
}

#[test]
fn a_carried_source_is_kept_while_it_exists_and_joins_the_library_it_lies_in() {
    let t = test_support::Temp::new();
    let gone = test_support::alac_content(11);
    let present = test_support::alac_content(12);
    let elsewhere = test_support::alac_content(13);
    let outside = t.0.join("outside.m4a");
    fs::write(&outside, b"").unwrap();
    let phone = vec![on_phone(&gone), on_phone(&present), on_phone(&elsewhere)];
    let lib = library(&[("moved.m4a", &gone), ("changed.m4a", &elsewhere)]);
    // /library itself does not exist here, so a source in it is gone.
    let earlier = prior(vec![
        binding(gone.clone(), Some("/library/old-name.m4a".into())),
        binding(present.clone(), Some(outside.clone())),
        binding(elsewhere.clone(), None),
    ]);
    let out = reconcile(&phone, matching(&phone), Some(&earlier), Some(&lib));
    assert_eq!(out.carried, 3);
    let source = |n: usize| out.pulled[n].source.as_deref();
    assert_eq!(source(0), Some(Path::new("/library/moved.m4a")));
    assert_eq!(source(1), Some(outside.as_path()));
    assert_eq!(source(2), Some(Path::new("/library/changed.m4a")));
    // In the library: its scope. Outside: the scope it was sent under.
    assert_eq!(out.pulled[0].content.collection, Some([7; 32]));
    assert_eq!(out.pulled[1].content.collection, present.collection);
    assert_eq!(out.pulled[2].content.collection, Some([7; 32]));
}

#[test]
fn copied_tracks_are_named_after_their_tags_without_overwriting() {
    let t = test_support::Temp::new();
    let track = |artist: &str, title: &str| PhoneTrack {
        track_id: "00000000000000aa".into(),
        artist: artist.into(),
        title: title.into(),
        copied_to: None,
        reason: None,
    };
    assert_eq!(file_name(&track("AC/DC", "T.N.T.")), "AC DC - T.N.T.");
    assert_eq!(file_name(&track("", "..hidden")), "hidden");
    assert_eq!(file_name(&track("Someone", "")), "00000000000000aa");
    assert!(file_name(&track("a", &"é".repeat(300))).len() <= 200);
    fs::write(t.0.join("A - B.m4a"), b"").unwrap();
    let mut used = BTreeSet::new();
    let first = free_name(&t.0, "A - B", "m4a", &mut used).unwrap();
    let second = free_name(&t.0, "A - B", "m4a", &mut used).unwrap();
    assert_eq!(first, t.0.join("A - B (2).m4a"));
    assert_eq!(second, t.0.join("A - B (3).m4a"));
}

#[test]
fn an_earlier_journal_elsewhere_is_found_by_the_tracks_the_capture_confirms() {
    let t = test_support::Temp::new();
    let state = t.0.join("state");
    let identity = test_support::identity();
    let journal = |device: &str, library: &str| {
        let parent = state.join("devices").join(device).join(library);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)
            .unwrap();
        parent.join("journal")
    };
    let confirmed = test_support::alac_content(20);
    let older = journal("aa", "11");
    let mut j = Journal::create(&older, &identity).unwrap();
    test_support::insert_verified(&mut j, confirmed.clone());
    drop(j);
    // Another phone's journal: nothing the capture shows.
    let other = journal("bb", "22");
    let mut j = Journal::create(&other, &identity).unwrap();
    test_support::insert_verified(&mut j, test_support::alac_content(21));
    test_support::insert_verified(&mut j, test_support::alac_content(22));
    drop(j);
    let phone = vec![on_phone(&confirmed)];
    let target = journal("aa", "33");
    let run = t.0.join("run");
    fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
    let found = find_prior(&state, &target, &identity, matching(&phone), &run)
        .unwrap()
        .unwrap();
    assert_eq!(found.path, older);
    assert!(!found.superseded);
    assert_eq!(found.prior.bindings.len(), 1);
    // Nothing confirmed: nothing found.
    let run = t.0.join("run-2");
    fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
    assert!(
        find_prior(&state, &target, &identity, |_, _, _| false, &run)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_pulled_journal_replaces_the_one_in_use_which_stays_as_previous() {
    let t = test_support::Temp::new();
    let identity = test_support::identity();
    let target = t.path();
    let mut j = Journal::create(&target, &identity).unwrap();
    let kept = test_support::alac_content(15);
    test_support::insert_verified(&mut j, kept.clone());
    drop(j);
    let earlier = journal::read_prior(&target, &t.0.join("copy")).unwrap();
    let pulled: Vec<Pulled> = earlier
        .bindings
        .iter()
        .map(|b| Pulled {
            content: b.content.clone(),
            source: Some("/library/kept.m4a".into()),
            conversion: None,
        })
        .collect();
    // A dry run writes elsewhere and leaves the journal in use alone.
    let dry = t.0.join("run-journal");
    write(
        &dry,
        &identity,
        (&[5; 32], 20),
        &pulled,
        Some(&earlier),
        true,
    )
    .unwrap();
    assert!(!target.with_file_name("journal.previous").exists());
    Journal::open_read_only(&dry, &identity).unwrap();
    // An interrupted pull's leftovers are cleared, not used.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(target.with_file_name("journal.pulling"))
        .unwrap();
    write(
        &target,
        &identity,
        (&[5; 32], 20),
        &pulled,
        Some(&earlier),
        false,
    )
    .unwrap();
    assert!(!target.with_file_name("journal.pulling").exists());
    let j = Journal::open_read_only(&target, &identity).unwrap();
    assert_eq!(j.recorded_sources().unwrap().len(), 1);
    assert!(
        j.lookup_active_verified(&kept.source.audio)
            .unwrap()
            .is_some()
    );
    let previous =
        Journal::open_read_only(&target.with_file_name("journal.previous"), &identity).unwrap();
    assert!(previous.recorded_sources().unwrap().is_empty());
}
