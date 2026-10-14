//! Synthetic stored bindings exercise read-only behavior, never MPEG completion.
use super::*;

fn frames() -> Vec<u8> {
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    [frame.clone(), frame].concat()
}
fn mpeg_content(temp: &Temp, bytes: &[u8]) -> Content {
    let path = temp.0.join("source.mp3");
    fs::write(&path, bytes).unwrap();
    Content {
        source: music_protocol::media::mpeg::read(&path)
            .unwrap()
            .transferred_identity()
            .unwrap(),
        track_pid: u64::MAX,
        path: "iTunes_Control/Music/F39/TEST.mp3".into(),
        artwork: None,
        collection: None,
    }
}
fn mpeg_current(bytes: u64) -> Current {
    let mut c = current();
    c.catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        &format!("UPDATE item_extra SET location='TEST.mp3',file_size={bytes}"),
    );
    c
}
fn stored(temp: &Temp, c: &Current, content: Content) -> Journal {
    let dir = temp.0.join("journal");
    drop(Journal::create(&dir, &c.identity).unwrap());
    let mut journal = Journal::open(&dir, &c.identity).unwrap();
    test_support::mpeg_verified_readback_fixture(&mut journal, content, 12);
    drop(journal);
    Journal::open_read_only(&dir, &c.identity).unwrap()
}
#[test]
fn stored_mpeg_can_be_planned_read_only_without_completion() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content.clone());
    let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let p = prepare(&journal, &c, Selection::All).unwrap();
    assert_eq!(p.bindings.len(), 1);
    assert!(p.bindings[0].content == content);
    assert_eq!(p.report.planned_audio_bytes, bytes.len() as u64);
    assert_eq!(p.batches.len(), 1);
    assert_eq!(p.report.phone_writes, 0);
    assert_eq!(p.report.journal_mutations, 0);
    assert_eq!(
        before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}
#[tokio::test]
async fn alac_report_verifies_with_one_read_and_two_guards() {
    let temp = Temp::new();
    let c = current();
    let content = temp.content();
    let journal = temp.journal(&c, Some(content.clone()));
    let mut p = prepare(&journal, &c, Selection::All).unwrap();
    let mut mock = Mock::new(content.source.full_file_sha256);
    read_batch(
        &mut mock,
        &p.state,
        &p.device,
        &p.bindings,
        &mut p.report.tracks,
        &p.batches[0],
        0,
        &control(),
    )
    .await
    .unwrap();
    p.report.completed_batches = 1;
    finish_report(&mut p.report);
    assert_eq!(mock.reads, 1);
    assert_eq!(mock.guards, 2);
    assert!(p.report.all_selected_audio_verified);
}

struct MpegMock {
    bytes: Vec<u8>,
    guards: usize,
    reads: usize,
    stats: usize,
    fail_final_guard: bool,
    changed: bool,
    cancel_after_read: Option<Arc<AtomicBool>>,
}
impl MpegMock {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            guards: 0,
            reads: 0,
            stats: 0,
            fail_final_guard: false,
            changed: false,
            cancel_after_read: None,
        }
    }
}
impl ReadBatch for MpegMock {
    async fn state_guard(&mut self, _: &FreshState) -> Result<()> {
        self.guards += 1;
        ensure!(
            !(self.fail_final_guard && self.guards == 2),
            "synthetic final state drift"
        );
        Ok(())
    }
    async fn attributes(&mut self, _: &str) -> Result<Option<Attributes>> {
        self.stats += 1;
        let mut a = attrs();
        a.insert("st_size".into(), self.bytes.len().to_string());
        a.insert("st_atime".into(), self.stats.to_string());
        if self.changed && self.stats > 1 {
            a.insert("st_mtime".into(), "changed".into());
        }
        Ok(Some(a))
    }
    async fn audio_hash(&mut self, _: &str, _: u64) -> Result<[u8; 32]> {
        panic!("MPEG verifier used the ALAC hash-only route")
    }
    async fn mpeg_bytes(&mut self, _: &str, _: u64) -> Result<Vec<u8>> {
        self.reads += 1;
        if let Some(cancelled) = &self.cancel_after_read {
            cancelled.store(true, Ordering::Relaxed);
        }
        Ok(self.bytes.clone())
    }
}
async fn check(p: &mut Prepared, mock: &mut MpegMock, control: &ReadControl) -> Result<()> {
    read_batch(
        mock,
        &p.state,
        &p.device,
        &p.bindings,
        &mut p.report.tracks,
        &p.batches[0],
        0,
        control,
    )
    .await
}
#[tokio::test]
async fn actual_mpeg_frames_and_full_file_are_verified_only_after_both_guards() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content);
    let original = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let mut p = prepare(&journal, &c, Selection::All).unwrap();
    let mut mock = MpegMock::new(bytes);
    check(&mut p, &mut mock, &control()).await.unwrap();
    assert_eq!((mock.guards, mock.stats, mock.reads), (2, 2, 1));
    assert!(p.report.tracks[0].audio_status == AudioStatus::VerifiedTransferredContainer);
    let facts = serde_json::to_value(&p.report.tracks[0]).unwrap();
    assert_eq!(facts["mpeg_frame_readback"]["frame_count"], 2);
    assert_eq!(facts["mpeg_frame_readback"]["audio_bytes"], 834);
    assert_eq!(
        facts["mpeg_frame_readback"]["encoded_identity_verified"],
        true
    );
    assert_eq!(
        facts["mpeg_frame_readback"]["encoded_audio_sha256"],
        facts["encoded_audio_sha256"]
    );
    finish_report(&mut p.report);
    assert!(p.report.all_selected_audio_verified);
    assert!(!p.report.metadata_projection_verified);
    assert_eq!(
        original,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}
#[tokio::test]
async fn identical_full_hash_cannot_hide_wrong_stored_frame_identity_or_layout() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content.clone());
    for case in 0..6 {
        let mut p = prepare(&journal, &c, Selection::All).unwrap();
        let mut expected = content.source.audio.mpeg().unwrap().clone();
        match case {
            0 => expected.frame_bytes_sha256[0] ^= 1,
            1 => expected.frame_count += 1,
            2 => {
                expected.audio_offset = 1;
                expected.audio_bytes -= 1;
            }
            3 => expected.sample_rate = 48000,
            4 => expected.channels = 1,
            _ => {
                expected.trailing_bytes = 128;
                expected.audio_bytes -= 128;
            }
        }
        p.bindings[0].content.source = music_protocol::media::fingerprint::TransferredSource::mpeg(
            content.source.file_bytes,
            content.source.full_file_sha256,
            expected,
        )
        .unwrap();
        p.report.tracks[0].encoded_audio_sha256 = hex(&p.bindings[0].content.source.audio.sha256);
        check(&mut p, &mut MpegMock::new(bytes.clone()), &control())
            .await
            .unwrap();
        assert!(
            p.report.tracks[0].audio_status == AudioStatus::EncodedIdentityMismatch,
            "case {case}"
        );
        assert!(
            !p.report.tracks[0]
                .mpeg_frame_readback
                .as_ref()
                .unwrap()
                .encoded_identity_verified
        );
        finish_report(&mut p.report);
        assert!(!p.report.all_selected_audio_verified);
    }
}
#[tokio::test]
async fn malformed_mpeg_hash_drift_attributes_and_cancel_never_yield_success() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content);
    for case in 0..6 {
        let mut p = prepare(&journal, &c, Selection::All).unwrap();
        let ctrl = control();
        let mut mock = MpegMock::new(bytes.clone());
        match case {
            0 => mock.bytes[100] ^= 1,
            1 => mock.bytes[417] = 0,
            2 => mock.changed = true,
            3 => mock.fail_final_guard = true,
            4 => mock.cancel_after_read = Some(Arc::clone(&ctrl.cancelled)),
            _ => ctrl.cancelled.store(true, Ordering::Relaxed),
        }
        let result = check(&mut p, &mut mock, &ctrl).await;
        if case == 0 {
            result.unwrap();
            assert!(p.report.tracks[0].audio_status == AudioStatus::HashMismatch);
            finish_report(&mut p.report);
            assert!(!p.report.all_selected_audio_verified);
        } else {
            assert!(result.is_err(), "case {case}");
            assert!(!p.report.verification_completed);
            assert!(!p.report.all_selected_audio_verified);
            if let Some(facts) = &p.report.tracks[0].mpeg_frame_readback {
                assert!(!facts.encoded_identity_verified);
            }
        }
    }
    let mut p = prepare(&journal, &c, Selection::All).unwrap();
    let mut ctrl = control();
    ctrl.deadline = Instant::now();
    let mut mock = MpegMock::new(bytes);
    assert!(check(&mut p, &mut mock, &ctrl).await.is_err());
    assert_eq!((mock.guards, mock.stats, mock.reads), (0, 0, 0));
}
#[test]
fn mpeg_tag_spans_and_native_transfer_size_ceiling_are_explicit() {
    let temp = Temp::new();
    let mut bytes = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
    bytes.extend(frames());
    bytes.extend(b"TAG");
    bytes.resize(bytes.len() + 125, 0);
    let content = mpeg_content(&temp, &bytes);
    let (hash, facts) = mpeg::inspect(&bytes, &control()).unwrap();
    assert_eq!(hash, content.source.full_file_sha256);
    assert!(facts.matches(&content.source));
    let json = serde_json::to_value(&facts).unwrap();
    assert_eq!(json["audio_offset"], 10);
    assert_eq!(json["trailing_tag_bytes"], 128);
    assert_eq!(json["encoded_identity_verified"], false);
    // Scheduling uses the same fixed cap as upload/readback, never a new lower cap.
    assert!(schedule(&[(0, MAX_AUDIO)], 1).is_ok());
    assert!(schedule(&[(0, MAX_AUDIO + 1)], 1).is_err());
    let c = mpeg_current(MAX_AUDIO + 1);
    let mut oversized = content;
    oversized.source.file_bytes = MAX_AUDIO + 1;
    assert_eq!(
        catalog_finding(&oversized, Some(&c.catalog.tracks()[0]), 1),
        Some("audio_exceeds_64mib_read_bound")
    );
    let mut ctrl = control();
    ctrl.deadline = Instant::now();
    assert!(mpeg::inspect(&bytes, &ctrl).is_err());
}
#[tokio::test]
async fn mixed_alac_mpeg_schedule_keeps_both_typed_paths_and_alac_track_bytes() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let mut c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content);
    let mut alac = temp.content();
    alac.track_pid = 42;
    alac.path = "iTunes_Control/Music/F39/ALAC.m4a".into();
    let mut writer = Journal::open(&temp.0.join("journal"), &c.identity).unwrap();
    test_support::insert_verified(&mut writer, alac.clone());
    drop(writer);
    c.catalog = music_protocol::test_support::catalogue::modified_collection_catalog(13, &format!(
        "UPDATE item_extra SET location='TEST.mp3',file_size={};
        INSERT INTO item VALUES(2,8,31,21,41,61,71,51,0,6,1,0,100);
        INSERT INTO item_extra VALUES(2,'Second','ALAC.m4a',147004.0,12345,10,0,2026,NULL,0,0,'Second',NULL);
        INSERT INTO item_store VALUES(2,42);
        INSERT INTO item_state VALUES(2,0,1,1,1);
        INSERT INTO item_stats VALUES(2,0,0,0,0,0,0,0,0);", bytes.len()));
    drop(journal);
    let journal = Journal::open_read_only(&temp.0.join("journal"), &c.identity).unwrap();
    let original = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let mut p = prepare(&journal, &c, Selection::All).unwrap();
    assert_eq!(p.bindings.len(), 2);
    assert_eq!(p.batches.len(), 1);
    struct Mixed {
        mpeg: MpegMock,
        alac: Mock,
    }
    impl ReadBatch for Mixed {
        async fn state_guard(&mut self, s: &FreshState) -> Result<()> {
            self.mpeg.state_guard(s).await
        }
        async fn attributes(&mut self, path: &str) -> Result<Option<Attributes>> {
            if path.ends_with(".mp3") {
                self.mpeg.attributes(path).await
            } else {
                self.alac.attributes(path).await
            }
        }
        async fn audio_hash(&mut self, path: &str, size: u64) -> Result<[u8; 32]> {
            self.alac.audio_hash(path, size).await
        }
        async fn mpeg_bytes(&mut self, path: &str, size: u64) -> Result<Vec<u8>> {
            self.mpeg.mpeg_bytes(path, size).await
        }
    }
    let mut reader = Mixed {
        mpeg: MpegMock::new(bytes),
        alac: Mock::new(alac.source.full_file_sha256),
    };
    read_batch(
        &mut reader,
        &p.state,
        &p.device,
        &p.bindings,
        &mut p.report.tracks,
        &p.batches[0],
        0,
        &control(),
    )
    .await
    .unwrap();
    assert_eq!(reader.mpeg.reads, 1);
    assert_eq!(reader.alac.reads, 1);
    assert_eq!(reader.mpeg.guards, 2);
    assert!(
        p.report
            .tracks
            .iter()
            .all(|t| t.audio_status == AudioStatus::VerifiedTransferredContainer)
    );
    let alac_track = p
        .report
        .tracks
        .iter()
        .find(|t| t.path.ends_with(".m4a"))
        .unwrap();
    assert!(
        serde_json::to_value(alac_track)
            .unwrap()
            .get("mpeg_frame_readback")
            .is_none()
    );
    assert_eq!(
        original,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}
#[test]
fn actual_frame_parser_accepts_the_exact_native_cap_and_refuses_one_more_byte() {
    let size = usize::try_from(MAX_AUDIO).unwrap();
    let span = size / 417 * 417;
    let offset = size - span;
    assert!(offset >= 10 && offset < 417);
    let mut bytes = vec![0; size];
    bytes[..6].copy_from_slice(b"ID3\x04\x00\x00");
    let tag = offset - 10;
    bytes[6..10].copy_from_slice(&[
        (tag >> 21) as u8 & 127,
        (tag >> 14) as u8 & 127,
        (tag >> 7) as u8 & 127,
        tag as u8 & 127,
    ]);
    for frame in bytes[offset..].chunks_exact_mut(417) {
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    }
    let (_, facts) = mpeg::inspect(&bytes, &control()).unwrap();
    let facts = serde_json::to_value(facts).unwrap();
    assert_eq!(facts["audio_offset"], offset);
    assert_eq!(facts["audio_bytes"], span);
    assert_eq!(facts["frame_count"], span / 417);
    bytes.push(0);
    let error = mpeg::inspect(&bytes, &control()).err().unwrap();
    assert!(error.to_string().contains("native transfer bound"));
}

#[tokio::test]
async fn mpeg_incoherent_identity_refuses_before_discovery() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let mut c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content);
    c.identity = Identity::new("foreign", &c.library).unwrap();
    let trace = temp.0.join("no-output");
    assert!(
        verify(&journal, &c, Selection::All, &trace, &control(), || async {
            panic!("MPEG identity refusal must precede discovery")
        })
        .await
        .is_err()
    );
    assert!(!trace.exists());
}

const TEST_PATH: &str = "iTunes_Control/Music/F39/TEST.mp3";
/// A cache scope seeded (or not) with a verified copy of `bytes`, as the
/// phone stat it with this modification time.
fn cache_scope(
    temp: &Temp,
    device: &[u8; 32],
    bytes: &[u8],
    seed: Option<&str>,
) -> crate::engine::state::audio_index::cache::Config {
    use crate::engine::state::audio_index::cache::{
        Config, FileKey, Mode, seed_for_tests, select_for_tests,
    };
    let config = Config::new(temp.0.join("fingerprints"), Mode::Reuse);
    select_for_tests(&config, device);
    if let Some(mtime) = seed {
        let copy = temp.0.join("copy.mp3");
        fs::write(&copy, bytes).unwrap();
        let mut a = attrs();
        a.insert("st_size".into(), bytes.len().to_string());
        a.insert("st_mtime".into(), mtime.into());
        let key = FileKey::new(device, TEST_PATH, &a).unwrap();
        seed_for_tests(&config, &key, &copy).unwrap();
    }
    config
}

#[tokio::test]
async fn stat_of_a_verified_copy_proves_mpeg_without_copying_it_again() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content);
    let mut p = prepare(&journal, &c, Selection::All).unwrap();
    let config = cache_scope(&temp, &p.device, &bytes, Some("1"));
    let mut mock = MpegMock::new(bytes.clone());
    crate::engine::state::audio_index::cache::scope(Some(config), async {
        read_batch(
            &mut mock,
            &p.state,
            &p.device,
            &p.bindings,
            &mut p.report.tracks,
            &p.batches[0],
            0,
            &control(),
        )
        .await
    })
    .await
    .unwrap();
    assert_eq!((mock.reads, mock.guards), (0, 2));
    assert!(p.report.tracks[0].audio_status == AudioStatus::VerifiedTransferredContainer);
    let facts = serde_json::to_value(&p.report.tracks[0]).unwrap();
    assert_eq!(facts["audio_proof"], "stat_of_verified_copy");
    assert_eq!(facts["mpeg_frame_readback"]["frame_count"], 2);
    assert_eq!(
        facts["mpeg_frame_readback"]["encoded_identity_verified"],
        true
    );
    assert_eq!(
        facts["mpeg_frame_readback"]["encoded_audio_sha256"],
        facts["encoded_audio_sha256"]
    );
    finish_report(&mut p.report);
    assert!(p.report.all_selected_audio_verified);
}

#[tokio::test]
async fn cached_frame_facts_still_expose_a_wrong_stored_frame_identity() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content.clone());
    let mut p = prepare(&journal, &c, Selection::All).unwrap();
    let config = cache_scope(&temp, &p.device, &bytes, Some("1"));
    let mut expected = content.source.audio.mpeg().unwrap().clone();
    expected.frame_bytes_sha256[0] ^= 1;
    p.bindings[0].content.source = music_protocol::media::fingerprint::TransferredSource::mpeg(
        content.source.file_bytes,
        content.source.full_file_sha256,
        expected,
    )
    .unwrap();
    p.report.tracks[0].encoded_audio_sha256 = hex(&p.bindings[0].content.source.audio.sha256);
    let mut mock = MpegMock::new(bytes.clone());
    crate::engine::state::audio_index::cache::scope(Some(config), async {
        read_batch(
            &mut mock,
            &p.state,
            &p.device,
            &p.bindings,
            &mut p.report.tracks,
            &p.batches[0],
            0,
            &control(),
        )
        .await
    })
    .await
    .unwrap();
    assert_eq!(mock.reads, 0);
    assert!(p.report.tracks[0].audio_status == AudioStatus::EncodedIdentityMismatch);
}

#[tokio::test]
async fn cache_miss_or_rewritten_file_falls_back_to_readback() {
    let temp = Temp::new();
    let bytes = frames();
    let content = mpeg_content(&temp, &bytes);
    let c = mpeg_current(bytes.len() as u64);
    let journal = stored(&temp, &c, content);
    // Rewritten: the cached copy had another modification time than the file now.
    for (seed, phone_bytes_changed) in [(None, false), (Some("0"), true)] {
        let mut p = prepare(&journal, &c, Selection::All).unwrap();
        let config = cache_scope(&temp, &p.device, &bytes, seed);
        let mut phone = bytes.clone();
        if phone_bytes_changed {
            phone[100] ^= 1;
        }
        let mut mock = MpegMock::new(phone);
        crate::engine::state::audio_index::cache::scope(Some(config), async {
            read_batch(
                &mut mock,
                &p.state,
                &p.device,
                &p.bindings,
                &mut p.report.tracks,
                &p.batches[0],
                0,
                &control(),
            )
            .await
        })
        .await
        .unwrap();
        assert_eq!(mock.reads, 1);
        let expected = if phone_bytes_changed {
            AudioStatus::HashMismatch
        } else {
            AudioStatus::VerifiedTransferredContainer
        };
        assert!(p.report.tracks[0].audio_status == expected);
        assert!(p.report.tracks[0].audio_proof.is_none());
        let _ = fs::remove_dir_all(temp.0.join("fingerprints"));
    }
}
