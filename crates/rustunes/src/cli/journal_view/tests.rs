use super::*;
use crate::engine::state::journal::{
    Artwork, CompletionEvidence, Identity, PreparedAttempt, StartEvidence,
};
use music_protocol::media::fingerprint::{EncodedAlac, SourceFingerprint};
use plist::{Dictionary, Value as Plist};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "journal-view-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
    fn journal(&self) -> Journal {
        Journal::create(&self.0.join("journal"), &identity()).unwrap()
    }
    fn database_bytes(&self) -> Vec<u8> {
        fs::read(self.0.join("journal/journal.sqlite")).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn identity() -> Identity {
    let mut bytes = vec![0; 1232];
    bytes[..4].copy_from_slice(b"frpd");
    bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
    bytes[6..8].copy_from_slice(&25u16.to_le_bytes());
    bytes[8] = 1;
    bytes[12..20].copy_from_slice(&1u64.to_le_bytes());
    bytes[20..28].copy_from_slice(&2u64.to_le_bytes());
    Identity::new(
        "synthetic",
        &music_protocol::catalogue::identity::parse(&bytes).unwrap(),
    )
    .unwrap()
}
fn content(seed: u8) -> Content {
    let mut audio = EncodedAlac {
        sha256: [0; 32],
        alac_config_sha256: [seed; 32],
        sample_sizes_sha256: [2; 32],
        media_sha256: [3; 32],
        timescale: 44100,
        duration_ticks: 132300,
        media_bytes: 800,
    };
    audio.sha256 = audio.composite_sha256();
    Content {
        source: (SourceFingerprint {
            file_bytes: 1200,
            full_file_sha256: [seed; 32],
            audio,
        })
        .into(),
        track_pid: u64::MAX - u64::from(seed),
        path: format!(
            "iTunes_Control/Music/F39/AAA{}.m4a",
            char::from(b'A' + seed)
        ),
        artwork: None,
        collection: Some([9; 32]),
    }
}
fn prepared(seed: u8) -> PreparedAttempt {
    let content = content(seed);
    let dict = |items: Vec<(&str, Plist)>| {
        Plist::Dictionary(
            items
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect::<Dictionary>(),
        )
    };
    let root = dict(vec![
        ("revision", 14u64.into()),
        ("timestamp", Plist::Date(UNIX_EPOCH.into())),
        (
            "operations",
            Plist::Array(vec![
                dict(vec![
                    ("operation", "update_db_info".into()),
                    ("pid", 50i64.into()),
                    (
                        "db_info",
                        dict(vec![("primary_container_pid", 51i64.into())]),
                    ),
                ]),
                dict(vec![
                    ("operation", "insert_track".into()),
                    ("pid", (content.track_pid as i64).into()),
                    (
                        "item",
                        dict(vec![
                            ("album_pid", (1000 + i64::from(seed)).into()),
                            ("artist_pid", (2000 + i64::from(seed)).into()),
                        ]),
                    ),
                ]),
            ]),
        ),
    ]);
    let mut operation = Vec::new();
    root.to_writer_binary(&mut operation).unwrap();
    PreparedAttempt {
        kind: AttemptKind::Insert,
        content,
        new_album_pid: Some(1000 + u64::from(seed)),
        new_artist_pid: Some(2000 + u64::from(seed)),
        current_media_anchor: 13,
        operation,
        input_receipt_sha256: [7; 32],
    }
}
fn completion(c: &Content) -> CompletionEvidence {
    CompletionEvidence {
        track_pid: c.track_pid,
        path: c.path.clone(),
        audio_bytes: c.source.file_bytes,
        audio_file_sha256: c.source.full_file_sha256,
        artwork_sha256: c.artwork.as_ref().map(|a| a.sha256),
        media_anchor: 14,
        receipt_sha256: [8; 32],
    }
}
fn start() -> StartEvidence {
    StartEvidence {
        ready_media_anchor: 13,
        ready_receipt_sha256: [9; 32],
        signature: [1; 21],
    }
}
fn report(journal: &Journal) -> Value {
    let mut out = Vec::new();
    write(journal, true, &mut out, "").unwrap();
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn empty_status_is_local_read_only_and_does_not_authorize_execution() {
    let temp = Temp::new();
    let journal = temp.journal();
    let before = temp.database_bytes();
    let json = report(&journal);
    assert_eq!(json["verified_binding_count"], 0);
    assert!(json["outstanding"].is_null());
    assert_eq!(json["execution_authorized"], false);
    assert_eq!(json["phone_deletion_authorized"], false);
    assert_eq!(json["device_state_checked"], false);
    let mut human = Vec::new();
    write(&journal, false, &mut human, "").unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("Outstanding: none"));
    assert!(human.contains("Fresh device evidence"));
    assert_eq!(before, temp.database_bytes());
    assert!(journal.outstanding().unwrap().is_none());
}

#[test]
fn hashes_signed_ids_and_historical_reservations_are_exact() {
    let temp = Temp::new();
    let mut journal = temp.journal();
    let mut first_content = content(1);
    first_content.artwork = Some(Artwork {
        cache_id: 677,
        bytes: 4,
        sha256: [10; 32],
    });
    crate::engine::state::journal::test_support::insert_verified_with(
        &mut journal,
        first_content.clone(),
        completion(&first_content),
    );
    let own = prepared(2);
    let own_content = own.content.clone();
    let own_id = journal.prepare(own).unwrap();
    let permit = journal.mark_started(own_id, start()).unwrap();
    journal.complete(permit, completion(&own_content)).unwrap();
    let cancelled = journal.prepare(prepared(4)).unwrap();
    journal.cancel_prepared(cancelled).unwrap();
    let pending = prepared(3);
    let operation_hash = hex(&Sha256::digest(&pending.operation));
    let id = journal.prepare(pending).unwrap();
    let before = temp.database_bytes();
    let json = report(&journal);
    assert_eq!(json["verified_binding_count"], 2);
    let rows = json["bindings"].as_array().unwrap();
    let a = rows
        .iter()
        .find(|r| r["content"]["track_id"] == "fffffffffffffffe")
        .unwrap();
    assert_eq!(a["content"]["track_id"], "fffffffffffffffe");
    assert_eq!(a["content"]["wire_track_id"], "-2");
    assert_eq!(
        a["content"]["transferred_audio"]["full_file_sha256"],
        hex(&[1; 32])
    );
    assert_eq!(
        a["content"]["transferred_audio"]["encoded_alac"]["sha256"],
        hex(&first_content.source.audio.sha256)
    );
    assert_eq!(a["content"]["artwork"]["cache_id"], 677);
    assert_eq!(a["content"]["artwork"]["sha256"], hex(&[10; 32]));
    assert_eq!(json["outstanding"]["attempt_id"], id.as_i64().to_string());
    assert_eq!(json["outstanding"]["state"], "prepared");
    assert_eq!(json["outstanding"]["operation_sha256"], operation_hash);
    assert_eq!(json["outstanding"]["started_recorded"], false);
    assert_eq!(json["reservations"]["path_count"], 4);
    assert_eq!(json["reservations"]["persistent_id_count"], 10);
    assert!(
        json["reservations"]["paths_casefolded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "itunes_control/music/f39/aaae.m4a")
    );
    assert_eq!(json["reservations"]["reusable"], false);
    assert_eq!(before, temp.database_bytes());
    let mut human = Vec::new();
    write(&journal, false, &mut human, "").unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("Journal: 2 active verified bindings"));
    assert!(human.contains("Use --json for complete binding and reservation details."));
    assert!(!human.contains(&hex(&first_content.source.full_file_sha256)));
    assert!(human.contains("no recorded Started transition"));
    assert!(!human.contains("outcome verified"));
}

#[test]
fn reopened_started_report_requires_reconciliation_and_never_replays() {
    let temp = Temp::new();
    let mut journal = temp.journal();
    let id = journal.prepare(prepared(1)).unwrap();
    drop(journal.mark_started(id, start()).unwrap());
    drop(journal);
    let journal = Journal::open(&temp.0.join("journal"), &identity()).unwrap();
    let before = temp.database_bytes();
    let json = report(&journal);
    assert_eq!(json["outstanding"]["attempt_id"], id.as_i64().to_string());
    assert_eq!(
        json["outstanding"]["state"],
        "started_requires_reconciliation"
    );
    assert_eq!(json["outstanding"]["fresh_reconciliation_required"], true);
    assert_eq!(json["verified_binding_count"], 0);
    assert!(
        json["recovery_guidance"]
            .as_str()
            .unwrap()
            .contains("Do not retry, reinsert")
    );
    let mut human = Vec::new();
    write(&journal, false, &mut human, "").unwrap();
    assert!(
        String::from_utf8(human)
            .unwrap()
            .contains("Outcome unknown")
    );
    assert_eq!(
        journal.attempt(id).unwrap().state,
        AttemptState::ReconciliationRequired
    );
    assert_eq!(before, temp.database_bytes());
}

#[test]
fn terminal_controls_and_bidi_are_escaped_and_writer_errors_propagate() {
    assert_eq!(
        terminal("title\x1b[31m\n\r\t\u{202e}end"),
        "title\\u{1b}[31m\\n\\r\\t\\u{202e}end"
    );
    assert_eq!(terminal("Καλημέρα"), "Καλημέρα");
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let temp = Temp::new();
    let journal = temp.journal();
    assert!(write(&journal, false, &mut Broken, "").is_err());
    assert!(write(&journal, true, &mut Broken, "").is_err());
}

#[test]
fn status_separates_retired_lifetime_from_same_audio_active_readd() {
    use crate::engine::state::journal::test_support;
    let temp = Temp::new();
    let mut j = temp.journal();
    let old = content(1);
    test_support::insert_verified(&mut j, old.clone());
    test_support::retire(&mut j, &old);
    let retired = report(&j);
    assert_eq!(retired["active_verified_binding_count"], 0);
    assert_eq!(retired["history"]["retired_binding_count"], 1);
    let mut current = content(2);
    current.source = old.source.clone();
    test_support::insert_verified(&mut j, current.clone());
    let before = temp.database_bytes();
    let v = report(&j);
    assert_eq!(v["binding_scope"], "active_verified_only");
    assert_eq!(v["active_verified_binding_count"], 1);
    assert_eq!(
        v["bindings"][0]["content"]["track_id"],
        format!("{:016x}", current.track_pid)
    );
    assert_eq!(v["history"]["lifetime_count"], 2);
    assert_eq!(v["history"]["version_count"], 2);
    assert_eq!(v["history"]["retired_binding_count"], 1);
    assert_eq!(v["phone_deletion_authorized"], false);
    assert_eq!(before, temp.database_bytes());
}

#[test]
fn an_outstanding_insert_batch_is_shown_with_how_it_settles() {
    use crate::engine::state::journal::test_support;
    let temp = Temp::new();
    let mut j = temp.journal();
    let track = content(3);
    let id = test_support::prepare_insert_batch_at(&mut j, track.clone(), &temp.0.join("run"));
    let before = temp.database_bytes();
    let v = report(&j);
    assert_eq!(v["insert_batch"]["batch_id"], id.as_i64().to_string());
    assert_eq!(v["insert_batch"]["state"], "prepared");
    assert_eq!(v["insert_batch"]["track_count"], 1);
    assert!(v["metadata_batch"].is_null());
    assert!(v["outstanding"].is_null());
    let mut text = Vec::new();
    write(&j, false, &mut text, "").unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(
        text.contains(&format!(
            "Outstanding: insert batch {} · prepared · 1 tracks · base anchor 13",
            id.as_i64()
        )),
        "{text}"
    );
    assert!(text.contains(&format!("  Track: {:016x} · ", track.track_pid)));
    assert!(!text.contains("Outstanding: none"), "{text}");
    assert!(text.contains("Run resume"), "{text}");
    assert_eq!(before, temp.database_bytes());
}
