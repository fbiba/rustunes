use super::*;
use crate::engine::state::journal::{
    AttemptKind, CompletionEvidence, Content, PreparedAttempt, StartEvidence,
};
use music_protocol::protocol::trace::private_write;
use plist::Value as Plist;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
fn hash(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}
fn sha(b: &[u8]) -> [u8; 32] {
    hash(b)
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
// Normally parsed audio and real Journal transitions, with synthetic storage
// verification receipts only. No receiver observations or native qualification.
struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    original: PathBuf,
    journal: Journal,
    journal_path: PathBuf,
    run: PathBuf,
    identity: Identity,
    input: Vec<u8>,
    plan: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        Self::made(|_| {})
    }
    fn modified_plan(change_plan: impl FnOnce(&mut Value)) -> Self {
        Self::made_with_source(
            |_| {},
            [1; 32],
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
            change_plan,
        )
    }
    fn made(change: impl FnOnce(&mut Plist)) -> Self {
        Self::made_with_identity(change, [1; 32])
    }
    fn made_with_identity(change: impl FnOnce(&mut Plist), device: [u8; 32]) -> Self {
        Self::made_with_source(
            change,
            device,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
            |_| {},
        )
    }
    fn made_with_source(
        change: impl FnOnce(&mut Plist),
        device: [u8; 32],
        bytes: &[u8],
        change_plan: impl FnOnce(&mut Value),
    ) -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let base = std::env::var_os("MANAGED_AUDIO_CASE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&base).unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = base.join(format!(
            "managed-audio-{}-{}-{stamp}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let root = dir.join("source");
        fs::create_dir(&root).unwrap();
        let original = dir.join("retained.m4a");
        fs::write(&original, bytes).unwrap();
        fs::write(root.join("native.m4a"), bytes).unwrap();
        let metadata = music_protocol::media::metadata::read(&original).unwrap();
        let fingerprint = music_protocol::media::fingerprint::read(&original).unwrap();
        let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
        let reserved = BTreeSet::new();
        let mut planner = crate::engine::planning::collections::Planner::new(&catalog, &reserved);
        let planned = planner
            .plan(
                &metadata,
                crate::engine::planning::collections::CandidateIds {
                    track: 1001,
                    album: Some(2001),
                    artist: Some(3001),
                },
            )
            .unwrap();
        let operation = crate::engine::execution::insert::prepare_live(
            &metadata,
            &planned,
            &catalog,
            std::time::UNIX_EPOCH,
        )
        .unwrap();
        let mut operation_value: Plist = plist::from_bytes(&operation).unwrap();
        change(&mut operation_value);
        let mut operation = Vec::new();
        operation_value.to_writer_binary(&mut operation).unwrap();
        let mut prefs = vec![0; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
        prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
        prefs[8] = 1;
        prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
        prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
        fs::write(dir.join("saved-prefs.bin"), &prefs).unwrap();
        let identity = Identity::for_saved_device(
            device,
            &music_protocol::catalogue::identity::parse(&prefs).unwrap(),
        )
        .unwrap();
        let scope = json!({"root":root,"recursive":true,"include":[],"exclude":[]});
        let path = "iTunes_Control/Music/F00/TEST.m4a";
        let mut plan_value = json!({"schema":"rustunes.additive-plan","mode":"additive_dry_run",
            "source_root":root,"selection":{"base_directory":root,"recursive":true,"include":[],"exclude":[]},
            "media_anchor":13,"device_sha256":hex(&identity.device_digest()),"library_sha256":hex(&identity.library_digest()),
            "phone_deletions":0,"execution_authorized_by_plan":false,"sources":[{"relative_path":"other.m4a","decision":"keep"},{"relative_path":"native.m4a",
            "source_file_bytes":fingerprint.file_bytes,"source_file_sha256":hex(&fingerprint.full_file_sha256),
            "encoded_audio_sha256":hex(&fingerprint.audio.sha256),"decision":"add","track_id":"00000000000003e9","path":path,
            "projected":ProjectedFields::from_source(&metadata).unwrap(),"artwork":{"status":"no_source_cover_preserve_existing"},"album":{"id":"00000000000007d1"},"artist":{"id":"0000000000000bb9"}}]});
        change_plan(&mut plan_value);
        let plan = serde_json::to_vec(&plan_value).unwrap();
        let input_value = json!({"schema":"rustunes.insertion-input","pre_session_attempt":1,
            "source":plan_value["sources"][1]["relative_path"],"plan_sha256":hex(&sha(&plan)),"full_source_sha256":hex(&fingerprint.full_file_sha256),
            "encoded_audio_sha256":hex(&fingerprint.audio.sha256),"track_id":"00000000000003e9","album_id":"00000000000007d1","artist_id":"0000000000000bb9",
            "path":path,"operation_sha256":hex(&sha(&operation)),"complete_index_sha256":hex(&[3;32]),"current_state_receipt_sha256":hex(&[4;32]),"phone_deletions":0});
        let input = serde_json::to_vec(&input_value).unwrap();
        let content = Content {
            source: fingerprint.clone().into(),
            track_pid: 1001,
            path: path.into(),
            artwork: None,
            collection: Some(sha(&serde_json::to_vec(&scope).unwrap())),
        };
        fs::write(dir.join("producer-input.json"), &input).unwrap();
        fs::write(dir.join("plan.json"), &plan).unwrap();
        fs::write(dir.join("initial-operation.plist"), &operation).unwrap();
        let state = dir.join("state");
        crate::engine::state::layout::private_directory(&state).unwrap();
        let journal_path = crate::engine::state::layout::journal_path(
            &state,
            &identity.device_digest(),
            &identity.library_digest(),
        )
        .unwrap();
        let runs = state.join("runs");
        crate::engine::state::layout::private_directory(&runs).unwrap();
        let run = runs.join("0001");
        crate::engine::state::layout::private_directory(&run).unwrap();
        let step = run.join("step-0001");
        crate::engine::state::layout::private_directory(&step).unwrap();
        fs::write(step.join("input.json"), &input).unwrap();
        let phase = step.join("preflight-0001");
        crate::engine::state::layout::private_directory(&phase).unwrap();
        crate::engine::state::layout::private_directory(&phase.join("before")).unwrap();
        fs::write(phase.join("input.json"), &input).unwrap();
        fs::write(run.join("plan.json"), &plan).unwrap();
        let mut writable = Journal::create(&journal_path, &identity).unwrap();
        let id = writable
            .prepare(PreparedAttempt {
                kind: AttemptKind::Insert,
                content,
                new_album_pid: Some(2001),
                new_artist_pid: Some(3001),
                current_media_anchor: 13,
                operation,
                input_receipt_sha256: sha(&input),
            })
            .unwrap();
        let permit = writable
            .mark_started(
                id,
                StartEvidence {
                    ready_media_anchor: 13,
                    ready_receipt_sha256: [5; 32],
                    signature: [1; 21],
                },
            )
            .unwrap();
        writable
            .complete(
                permit,
                CompletionEvidence {
                    track_pid: 1001,
                    path: path.into(),
                    audio_bytes: fingerprint.file_bytes,
                    audio_file_sha256: fingerprint.full_file_sha256,
                    artwork_sha256: None,
                    media_anchor: 14,
                    receipt_sha256: [7; 32],
                },
            )
            .unwrap();
        drop(writable);
        let journal = Journal::open_read_only(&journal_path, &identity).unwrap();
        Self {
            dir,
            root,
            original,
            journal,
            journal_path,
            run,
            identity,
            input,
            plan,
        }
    }
    fn catalog(&self, title: &str) -> music_protocol::catalogue::sqlite::Catalog {
        music_protocol::test_support::catalogue::modified_collection_catalog(
            14,
            &self.catalog_sql(title),
        )
    }
    fn catalog_sql(&self, title: &str) -> String {
        let p = ProjectedFields::from_source(
            &music_protocol::media::metadata::read(&self.original).unwrap(),
        )
        .unwrap();
        let f = music_protocol::media::fingerprint::read(&self.original).unwrap();
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        format!(
            "UPDATE item_store SET sync_id=1001;
             UPDATE album SET sync_id=2001,album={} WHERE album_pid=31;
             UPDATE album_artist SET sync_id=3001,album_artist={} WHERE album_artist_pid=21;
             UPDATE item_artist SET item_artist={}; UPDATE base_location SET path='iTunes_Control/Music/F00';
             UPDATE genre SET genre={}; UPDATE composer SET composer={},sort_composer={};
             UPDATE item SET genre_id={},composer_pid={},track_number={},disc_number={},is_compilation={};
             UPDATE item_extra SET location='TEST.m4a',title={},sort_title={},total_time_ms={},file_size={},track_count={},disc_count={},year={},copyright={},content_rating={},content_rating_level=0;",
            quote(&p.album),quote(&p.grouping_artist),quote(&p.artist),quote(p.genre.as_deref().unwrap_or("Unused")),
            quote(p.composer.as_deref().unwrap_or("Unused")),quote(p.composer.as_deref().unwrap_or("Unused")),
            if p.genre.is_some(){61}else{0},if p.composer.is_some(){71}else{0},p.track.unwrap_or(0),p.disc.unwrap_or(1),u8::from(p.compilation.unwrap_or(false)),
            quote(title),quote(title),p.total_time_ms,f.file_bytes,p.track_total.unwrap_or(0),p.disc_total.unwrap_or(0),p.year.unwrap_or(0),
            p.copyright.as_deref().map(&quote).unwrap_or_else(||"NULL".into()),p.content_rating.unwrap_or(0))
    }
}
fn edited(bytes: &[u8], atom: [u8; 4], value: lofty::mp4::AtomData) -> Vec<u8> {
    use lofty::{
        config::{ParseOptions, WriteOptions},
        file::AudioFile,
        mp4::{Atom, AtomIdent, Mp4File},
    };
    let mut data = std::io::Cursor::new(bytes.to_vec());
    let mut file = Mp4File::read_from(&mut data, ParseOptions::default()).unwrap();
    file.ilst_mut()
        .unwrap()
        .replace_atom(Atom::new(AtomIdent::Fourcc(atom), value));
    file.save_to(&mut data, WriteOptions::default()).unwrap();
    data.into_inner()
}

fn index(f: &Fixture, catalog: &Catalog) -> DeviceAudioIndex {
    let fp = music_protocol::media::fingerprint::read(&f.original).unwrap();
    let a = &fp.audio;
    let h = catalog.source_hashes();
    let i = f.identity.index_identity();
    let value = json!({"schema":"rustunes.device-audio-index","complete":true,
    "identity":{"device_identifier_sha256":hex(&i.device_identifier_sha256),"library_identity_sha256":hex(&i.library_identity_sha256)},
    "catalog":{"database_sha256":hex(&h.database),"database_bytes":h.database_bytes,"wal_sha256":h.wal.as_ref().map(|s|hex(s)),"wal_bytes":h.wal_bytes,"persisted_media_anchor":catalog.persisted_media_anchor()},
    "track_count":1,"total_bytes":fp.file_bytes,"alac_track_count":1,"other_track_count":0,"encoded_audio_kind":"encoded-alac-not-pcm",
    "records":[{"sync_id":"00000000000003E9","afc_path":"iTunes_Control/Music/F00/TEST.m4a","attributes":{"st_ifmt":"S_IFREG","st_mtime":"1","st_size":fp.file_bytes.to_string()},
    "fingerprint":{"file_bytes":fp.file_bytes,"full_file_sha256":hex(&fp.full_file_sha256),"audio":{"sha256":hex(&a.sha256),"alac_config_sha256":hex(&a.alac_config_sha256),"sample_sizes_sha256":hex(&a.sample_sizes_sha256),"media_sha256":hex(&a.media_sha256),"timescale":a.timescale,"duration_ticks":a.duration_ticks,"media_bytes":a.media_bytes}}}],
    "other_records":[],"progress_sha256":hex(&[4;32]),"completed_unix_time_ns":"1","fresh_prefs_before_after_components_equal":true,"atomic_snapshot_claim":false,"catalog_metadata":{}});
    let bytes = serde_json::to_vec(&value).unwrap();
    let p = f.dir.join("index.json");
    fs::write(&p, &bytes).unwrap();
    DeviceAudioIndex::load_complete(&p, hash(&bytes), catalog, &i).unwrap()
}

fn changed_collection(f: &Fixture) -> SourceCollection {
    let mut bytes = edited(
        &fs::read(&f.original).unwrap(),
        *b"\xa9nam",
        lofty::mp4::AtomData::UTF8("Replacement title".into()),
    );
    for (atom, value) in [
        (*b"\xa9ART", "Replacement artist"),
        (*b"\xa9alb", "Replacement album"),
        (*b"aART", "Replacement album artist"),
    ] {
        bytes = edited(&bytes, atom, lofty::mp4::AtomData::UTF8(value.into()));
    }
    let at = bytes.windows(4).position(|x| x == b"mdat").unwrap() + 4;
    bytes[at] ^= 1;
    fs::write(f.root.join("native.m4a"), bytes).unwrap();
    crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&f.root, &Default::default()).unwrap(),
    )
    .unwrap()
}
fn replacement_plan(f: &Fixture, add_only: bool) -> Plan {
    let sources = changed_collection(f);
    plan_for(f, &sources, add_only)
}
fn plan_for(f: &Fixture, sources: &SourceCollection, add_only: bool) -> Plan {
    let catalog = f.catalog("Standard title");
    let index = index(f, &catalog);
    let mut candidates = BTreeMap::new();
    candidates.insert(
        "native.m4a".into(),
        Candidate {
            ids: CandidateIds {
                track: 5001,
                album: Some(5002),
                artist: Some(5003),
            },
            path: "iTunes_Control/Music/F00/NEWX.m4a".into(),
        },
    );
    for source in &sources.tracks {
        if source.relative_path != "native.m4a" {
            candidates.insert(
                source.relative_path.clone(),
                Candidate {
                    ids: CandidateIds {
                        track: 6001,
                        album: Some(6002),
                        artist: Some(6003),
                    },
                    path: "iTunes_Control/Music/F00/OTHR.m4a".into(),
                },
            );
        }
    }
    build_with_policy(
        sources,
        &catalog,
        &index,
        index.identity(),
        Some(JournalEvidence {
            journal: &f.journal,
            identity: &f.identity,
        }),
        &candidates,
        add_only,
    )
    .unwrap()
}
#[test]
fn changed_audio_and_tags_same_proven_path_does_not_hide_as_unrelated_add() {
    let f = Fixture::new();
    let plan = replacement_plan(&f, false);
    let row = plan
        .sources
        .iter()
        .find(|r| r.relative_path == "native.m4a")
        .unwrap();

    assert!(
        matches!(row.decision, Decision::Skip { .. }),
        "proven same-path changed audio must be left out; old behavior: {}",
        serde_json::to_value(row).unwrap()
    );
    let detail = row.managed_audio_replacement.as_ref().unwrap();
    assert_eq!(
        detail.classification,
        "audio_replacement_proposed_execution_blocked"
    );
    assert!(detail.replacement_identity_established);
    assert!(!detail.original_bytes_available);
    assert!(
        !detail.execution_authorized
            && !detail.native_receiver_qualified
            && !detail.start_authority
            && !detail.completion_authority
    );
    let evidence = detail.evidence.as_ref().unwrap();
    assert_eq!(evidence["binding_id"], 1);
    assert_eq!(evidence["binding_version"], 1);
    assert_eq!(evidence["insertion_attempt"], 1);
    assert_eq!(evidence["relative_path"], "native.m4a");
    assert_eq!(
        evidence["immutable_original_content"]["track_id"],
        "00000000000003e9"
    );
    assert_eq!(
        evidence["immutable_original_content"]["path"],
        "iTunes_Control/Music/F00/TEST.m4a"
    );
    let fp = music_protocol::media::fingerprint::read(&f.original).unwrap();
    assert_eq!(
        evidence["immutable_original_content"]["source"]["full_file_sha256"],
        hex(&fp.full_file_sha256)
    );
    assert_ne!(
        evidence["immutable_original_content"]["source"]["audio"],
        evidence["desired_source"]["audio"]
    );
    assert_ne!(
        evidence["immutable_original_content"]["source"]["full_file_sha256"],
        evidence["desired_source"]["full_file_sha256"]
    );
    let historical =
        ProjectedFields::from_source(&music_protocol::media::metadata::read(&f.original).unwrap())
            .unwrap();
    assert_eq!(evidence["historical_projected"]["title"], historical.title);
    assert_eq!(
        evidence["historical_absolute_source_path"],
        json!(f.root.join("native.m4a"))
    );
    assert_eq!(evidence["desired_projected"]["title"], "Replacement title");
    assert_eq!(evidence["target_lifetime_retirement_authorized"], false);
    assert_eq!(evidence["old_ids_restored"], false);
    assert_eq!(
        evidence["statistics_or_memberships_migration_promised"],
        false
    );
    // Left out: the earlier version stays, and the rest of the plan may run.
    assert!(!plan.has_conflicts() && plan.decision_counts().skipped == 1);
    assert_eq!(plan.phone_deletions, 0);
    assert!(!plan.execution_authorized_by_plan);
}

fn row(plan: &Plan) -> &SourceDecision {
    plan.sources
        .iter()
        .find(|r| r.relative_path == "native.m4a")
        .unwrap()
}
/// The row proposes replacing the one historical track, and its evidence.
fn proposed(row: &SourceDecision) -> &Value {
    assert!(
        matches!(
            row.decision,
            Decision::Skip {
                code: "managed_audio_replacement_proposed",
                ..
            }
        ),
        "{}",
        serde_json::to_value(row).unwrap()
    );
    let detail = row.managed_audio_replacement.as_ref().unwrap();
    assert!(detail.replacement_identity_established && !detail.execution_authorized);
    let evidence = detail.evidence.as_ref().unwrap();
    assert_eq!(
        evidence["immutable_original_content"]["track_id"],
        "00000000000003e9"
    );
    evidence
}
fn refused(plan: &Plan, needle: &str) {
    refused_row(row(plan), needle);
}
fn refused_row(row: &SourceDecision, needle: &str) {
    assert!(
        matches!(
            row.decision,
            Decision::Skip {
                code: "managed_audio_replacement_evidence_refused",
                ..
            }
        ),
        "{}",
        serde_json::to_value(row).unwrap()
    );
    let detail = row.managed_audio_replacement.as_ref().unwrap();
    assert_eq!(detail.classification, "audio_replacement_evidence_refused");
    assert!(
        !detail.replacement_identity_established
            && !detail.execution_authorized
            && !detail.start_authority
            && !detail.completion_authority
    );
    assert!(
        detail.refusal.as_deref().unwrap().contains(needle),
        "{:?}",
        detail.refusal
    );
}
fn limitation(plan: &Plan, needle: &str) {
    let row = row(plan);
    assert!(
        matches!(row.decision, Decision::Add { .. }),
        "{}",
        serde_json::to_value(row).unwrap()
    );
    let detail = row.managed_audio_replacement.as_ref().unwrap();
    assert_eq!(detail.classification, "unassociated_history_limitation");
    assert!(!detail.replacement_identity_established && !detail.execution_authorized);
    assert!(detail.evidence.is_none());
    assert!(
        detail
            .unassociated_history
            .iter()
            .any(|v| v["reason"].as_str().unwrap().contains(needle)),
        "{:?}",
        detail.unassociated_history
    );
    for v in &detail.unassociated_history {
        assert_eq!(v["associated_to_desired_row"], false);
        assert_eq!(v["does_not_prove_source_absence"], true);
        assert_eq!(v["baseline_decision_preserved"], true);
    }
}
#[test]
fn managed_audio_add_only_plans_a_plain_add_with_no_replacement_detail() {
    let f = Fixture::new();
    let plan = replacement_plan(&f, true);
    assert!(matches!(row(&plan).decision, Decision::Add { .. }));
    let saved = serde_json::to_value(row(&plan)).unwrap();
    assert!(saved.get("managed_audio_replacement").is_none());
    assert_eq!(plan.phone_deletions, 0);
}
#[test]
fn managed_audio_unrelated_source_add_and_scope_encoding_remain_unchanged() {
    let f = Fixture::new();
    changed_collection(&f);
    fs::rename(f.root.join("native.m4a"), f.root.join("unrelated.m4a")).unwrap();
    fs::copy(&f.original, f.root.join("native.m4a")).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&f.root, &Default::default()).unwrap(),
    )
    .unwrap();
    let scope = crate::engine::planning::removal::collection_scope_sha256(&sources).unwrap();
    let plan = plan_for(&f, &sources, false);
    assert!(matches!(row(&plan).decision, Decision::Keep));
    let other = plan
        .sources
        .iter()
        .find(|r| r.relative_path == "unrelated.m4a")
        .unwrap();
    assert!(matches!(other.decision, Decision::Add { .. }));
    assert!(other.managed_audio_replacement.is_none());
    assert_eq!(
        scope,
        crate::engine::planning::removal::collection_scope_sha256(&sources).unwrap()
    );
    assert_eq!(plan.phone_deletions, 0);
}
#[test]
fn managed_audio_same_encoded_retag_is_a_metadata_update() {
    let f = Fixture::new();
    fs::write(
        f.root.join("native.m4a"),
        edited(
            &fs::read(&f.original).unwrap(),
            *b"\xa9nam",
            lofty::mp4::AtomData::UTF8("Desired title".into()),
        ),
    )
    .unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&f.root, &Default::default()).unwrap(),
    )
    .unwrap();
    let plan = plan_for(&f, &sources, false);
    let decision = row(&plan);
    assert!(decision.managed_audio_replacement.is_none());
    assert!(
        matches!(&decision.decision, Decision::Update { changes, .. } if changes.iter().any(|c| c.field == "title"))
    );
    let plan = plan_for(&f, &sources, true);
    assert!(matches!(row(&plan).decision, Decision::Keep));
}
#[test]
fn managed_audio_original_availability_is_exact_and_not_execution_authority() {
    let f = Fixture::new();
    let plan = replacement_plan(&f, false);
    assert!(
        !row(&plan)
            .managed_audio_replacement
            .as_ref()
            .unwrap()
            .original_bytes_available
    );
    fs::copy(&f.original, f.run.join("step-0001/retained-source.m4a")).unwrap();
    let plan = replacement_plan(&f, false);
    let detail = row(&plan).managed_audio_replacement.as_ref().unwrap();
    assert!(detail.original_bytes_available && detail.replacement_identity_established);
    assert!(!detail.execution_authorized);
    fs::copy(
        f.root.join("native.m4a"),
        f.run.join("step-0001/retained-source.m4a"),
    )
    .unwrap();
    refused(
        &replacement_plan(&f, false),
        "retained original bytes/encoded descriptor differ",
    );
}
#[test]
fn managed_audio_newer_unrelated_run_cannot_steal_exact_digest() {
    let f = Fixture::new();
    let run = f.run.parent().unwrap().join("9999");
    crate::engine::state::layout::private_directory(&run).unwrap();
    let step = run.join("step-0001");
    crate::engine::state::layout::private_directory(&step).unwrap();
    fs::write(step.join("input.json"), b"{\"schema\":\"newer-unrelated\"}").unwrap();
    fs::write(run.join("plan.json"), b"{\"schema\":\"unrelated-plan\"}").unwrap();
    let plan = replacement_plan(&f, false);
    let detail = row(&plan).managed_audio_replacement.as_ref().unwrap();
    assert!(detail.replacement_identity_established);
    assert_eq!(
        detail.evidence.as_ref().unwrap()["producer_input_sha256"],
        hex(&hash(&f.input))
    );
    assert_eq!(
        detail.evidence.as_ref().unwrap()["plan_sha256"],
        hex(&hash(&f.plan))
    );
}
#[test]
fn managed_audio_missing_duplicate_tampered_input_is_unassociated_not_identity() {
    let f = Fixture::new();
    fs::remove_file(f.run.join("step-0001/input.json")).unwrap();
    limitation(&replacement_plan(&f, false), "missing managed producer");
    let f = Fixture::new();
    let step = f.run.join("step-0002");
    crate::engine::state::layout::private_directory(&step).unwrap();
    fs::write(step.join("input.json"), &f.input).unwrap();
    limitation(&replacement_plan(&f, false), "ambiguous managed producer");
    let f = Fixture::new();
    fs::write(
        f.run.join("step-0001/input.json"),
        b"{\"schema\":\"tampered\"}",
    )
    .unwrap();
    limitation(&replacement_plan(&f, false), "missing managed producer");
}
#[test]
fn managed_audio_known_path_missing_tampered_and_aliased_plan_blocks() {
    let f = Fixture::new();
    fs::remove_file(f.run.join("plan.json")).unwrap();
    let p = replacement_plan(&f, false);
    assert_eq!(
        row(&p)
            .managed_audio_replacement
            .as_ref()
            .unwrap()
            .classification,
        "audio_replacement_evidence_refused"
    );
    let f = Fixture::new();
    let mut v: Value = serde_json::from_slice(&f.plan).unwrap();
    v["sources"][1]["projected"]["title"] = json!("tampered");
    fs::write(f.run.join("plan.json"), serde_json::to_vec(&v).unwrap()).unwrap();
    refused(&replacement_plan(&f, false), "plan bytes differ");
    let f = Fixture::new();
    fs::rename(f.run.join("plan.json"), f.run.join("actual-plan.json")).unwrap();
    std::os::unix::fs::symlink("actual-plan.json", f.run.join("plan.json")).unwrap();
    refused(&replacement_plan(&f, false), "size/type/link");
}
#[test]
fn managed_audio_hash_bound_wrong_root_base_options_anchor_prefix_blocks() {
    let f = Fixture::modified_plan(|v| v["source_root"] = json!("/a/different/historical/root"));
    refused(
        &replacement_plan(&f, false),
        "root/base attribution differs",
    );
    let f = Fixture::modified_plan(|v| v["selection"]["base_directory"] = json!("/different/base"));
    refused(
        &replacement_plan(&f, false),
        "root/base attribution differs",
    );
    let f = Fixture::modified_plan(|v| v["selection"]["include"] = json!(["other.m4a"]));
    refused(
        &replacement_plan(&f, false),
        "outside the historical selection",
    );
    // The plan's projected tags are descriptive: the producer input pins the
    // plan, and replacement adds the new source's own tags.
    let f = Fixture::modified_plan(|v| v["sources"][1]["projected"]["title"] = json!("forged"));
    let plan = replacement_plan(&f, false);
    assert_eq!(
        proposed(row(&plan))["historical_projected"]["title"],
        "forged"
    );
    let f = Fixture::modified_plan(|v| v["media_anchor"] = json!(12));
    refused(&replacement_plan(&f, false), "anchor does not join");
    let f = Fixture::modified_plan(
        |v| v["sources"][0] = json!({"decision":"add","relative_path":"other.m4a","track_id":"0000000000004321","path":"iTunes_Control/Music/F00/PREV.m4a"}),
    );
    refused(
        &replacement_plan(&f, false),
        "prior Add insertion is missing",
    );
}
#[test]
fn managed_audio_unsafe_hash_bound_path_claim_remains_unassociated() {
    let f = Fixture::modified_plan(|v| v["sources"][1]["relative_path"] = json!("../native.m4a"));
    limitation(&replacement_plan(&f, false), "unsafe relative");
}
#[test]
fn managed_audio_pending_history_never_promotes_ownership() {
    let mut f = Fixture::new();
    let mut writable = Journal::open(&f.journal_path, &f.identity).unwrap();
    crate::engine::state::journal::test_support::prepare_insert(
        &mut writable,
        crate::engine::state::journal::test_support::alac_content(9),
    );
    drop(writable);
    f.journal = Journal::open_read_only(&f.journal_path, &f.identity).unwrap();
    let plan = replacement_plan(&f, false);
    assert!(matches!(
        row(&plan).decision,
        Decision::Conflict {
            code: "journal_reconciliation_required",
            ..
        }
    ));
    let detail = row(&plan).managed_audio_replacement.as_ref().unwrap();
    assert_eq!(detail.classification, "unassociated_history_limitation");
    assert!(!detail.replacement_identity_established);
    assert!(
        detail
            .unassociated_history
            .iter()
            .any(|v| v["reason"].as_str().unwrap().contains("all-kind idle"))
    );
    assert!(!plan.journal_blockers.is_empty());
}

#[test]
fn managed_audio_stale_retired_binding_origin_refuses() {
    let f = Fixture::new();
    let fp = music_protocol::media::fingerprint::read(&f.original).unwrap();
    let binding = f
        .journal
        .lookup_active_verified(&fp.audio)
        .unwrap()
        .unwrap();
    let mut altered = f
        .journal
        .lookup_active_verified(&fp.audio)
        .unwrap()
        .unwrap();
    altered.receipt_sha256 = [99; 32];
    assert!(
        crate::engine::state::journal::source_attribution::replacement::origin(
            &f.journal,
            &altered,
            &f.identity
        )
        .is_err()
    );
    let mut writable = Journal::open(&f.journal_path, &f.identity).unwrap();
    crate::engine::state::journal::test_support::retire(&mut writable, &binding.content);
    drop(writable);
    let fresh = Journal::open_read_only(&f.journal_path, &f.identity).unwrap();
    assert!(
        crate::engine::state::journal::source_attribution::replacement::origin(
            &fresh,
            &binding,
            &f.identity
        )
        .is_err()
    );
}
#[test]
fn managed_audio_late_desired_and_input_drift_refuse_before_publication() {
    let f = Fixture::new();
    let sources = changed_collection(&f);
    let source = &sources.tracks[0];
    let fields = ProjectedFields::from_source(&source.metadata).unwrap();
    let catalog = f.catalog("Standard title");
    let index = index(&f, &catalog);
    let bindings = f
        .journal
        .active_bindings()
        .unwrap()
        .into_iter()
        .map(|b| b.binding)
        .collect::<Vec<_>>();
    let mut cache = Some(Ok(super::managed_source::ManagedContext::open(
        &f.journal,
        &f.identity,
    )
    .unwrap()));
    fs::write(f.root.join("native.m4a"), fs::read(&f.original).unwrap()).unwrap();
    let detail = super::managed_audio::describe(
        &mut cache,
        JournalEvidence {
            journal: &f.journal,
            identity: &f.identity,
        },
        &bindings,
        &sources,
        source,
        &fields,
        &catalog,
        &index,
        None,
    )
    .unwrap();
    assert_eq!(detail.classification, "audio_replacement_evidence_refused");
    assert!(detail.refusal.unwrap().contains("desired source changed"));
    let sources = changed_collection(&f);
    let source = &sources.tracks[0];
    let fields = ProjectedFields::from_source(&source.metadata).unwrap();
    let mut cache = Some(Ok(super::managed_source::ManagedContext::open(
        &f.journal,
        &f.identity,
    )
    .unwrap()));
    fs::write(f.run.join("step-0001/input.json"), b"tampered after lookup").unwrap();
    let detail = super::managed_audio::describe(
        &mut cache,
        JournalEvidence {
            journal: &f.journal,
            identity: &f.identity,
        },
        &bindings,
        &sources,
        source,
        &fields,
        &catalog,
        &index,
        None,
    )
    .unwrap();
    assert!(!detail.replacement_identity_established);
    assert_eq!(detail.classification, "unassociated_history_limitation");
    assert!(!detail.execution_authorized);
}
#[test]
fn managed_audio_saved_proposal_regeneration_and_tamper_refuse_before_apply() {
    let f = Fixture::new();
    let plan = replacement_plan(&f, false);
    let saved = serde_json::to_value(&plan).unwrap();
    let regenerated = replacement_plan(&f, false);
    assert_eq!(saved, serde_json::to_value(&regenerated).unwrap());
    // Left out, the track stays as it is, so the rest of the plan may run.
    crate::engine::planning::saved_plan::ensure_recomputed(&regenerated, &saved).unwrap();
    for field in [
        "execution_authorized",
        "original_bytes_available",
        "evidence",
        "unassociated_history",
    ] {
        let mut altered = saved.clone();
        altered["sources"][0]["managed_audio_replacement"][field] = json!("tampered");
        assert!(
            crate::engine::planning::saved_plan::ensure_recomputed(&regenerated, &altered)
                .unwrap_err()
                .to_string()
                .contains("saved plan differs")
        );
    }
}

#[test]
fn managed_audio_narrower_selection_still_proposes_but_proven_other_root_adds() {
    let f = Fixture::new();
    changed_collection(&f);
    for options in [
        crate::engine::planning::source_collection::SelectionOptions {
            include: ["native.m4a".into()].into(),
            ..Default::default()
        },
        crate::engine::planning::source_collection::SelectionOptions {
            exclude: ["unrelated".into()].into(),
            ..Default::default()
        },
        crate::engine::planning::source_collection::SelectionOptions {
            recursive: false,
            ..Default::default()
        },
    ] {
        let sources = crate::engine::planning::source_collection::inspect(
            &crate::engine::planning::source_collection::select(&f.root, &options).unwrap(),
        )
        .unwrap();
        let plan = plan_for(&f, &sources, false);
        let evidence = proposed(row(&plan));
        assert_eq!(evidence["historical_selection"]["include"], json!([]));
        assert_eq!(
            evidence["historical_absolute_source_path"],
            json!(f.root.join("native.m4a"))
        );
    }
    let other_root = f.dir.join("unrelated-source-root");
    fs::create_dir(&other_root).unwrap();
    fs::copy(f.root.join("native.m4a"), other_root.join("native.m4a")).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&other_root, &Default::default())
            .unwrap(),
    )
    .unwrap();
    let plan = plan_for(&f, &sources, false);
    assert!(matches!(row(&plan).decision, Decision::Add { .. }));
    assert!(row(&plan).managed_audio_replacement.is_none());
}
#[test]
fn managed_audio_duplicate_binding_candidates_are_descriptively_ambiguous() {
    let f = Fixture::new();
    let sources = changed_collection(&f);
    let source = &sources.tracks[0];
    let fields = ProjectedFields::from_source(&source.metadata).unwrap();
    let fp = music_protocol::media::fingerprint::read(&f.original).unwrap();
    let bindings = vec![
        f.journal
            .lookup_active_verified(&fp.audio)
            .unwrap()
            .unwrap(),
        f.journal
            .lookup_active_verified(&fp.audio)
            .unwrap()
            .unwrap(),
    ];
    let catalog = f.catalog("Standard title");
    let index = index(&f, &catalog);
    let mut cache = None;
    let detail = super::managed_audio::describe(
        &mut cache,
        JournalEvidence {
            journal: &f.journal,
            identity: &f.identity,
        },
        &bindings,
        &sources,
        source,
        &fields,
        &catalog,
        &index,
        None,
    )
    .unwrap();
    assert_eq!(detail.classification, "audio_replacement_evidence_refused");
    assert_eq!(
        detail.refusal.as_deref(),
        Some("ambiguous native historical bindings claim the same source path")
    );
    assert!(!detail.replacement_identity_established && !detail.execution_authorized);
}

#[test]
fn managed_audio_exact_historical_path_file_root_selection_proposes() {
    let f = Fixture::new();
    changed_collection(&f);
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(
            &f.root.join("native.m4a"),
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let plan = plan_for(&f, &sources, false);
    let evidence = proposed(row(&plan));
    assert_eq!(
        evidence["desired_source_root"],
        json!(f.root.join("native.m4a"))
    );
    assert_eq!(evidence["source_root"], json!(f.root));
}

#[test]
fn managed_audio_blocked_candidate_groups_cannot_leak_into_unrelated_add() {
    let f = Fixture::new();
    changed_collection(&f);
    let mut bytes = fs::read(f.root.join("native.m4a")).unwrap();
    let at = bytes.windows(4).position(|b| b == b"mdat").unwrap() + 4;
    bytes[at] ^= 2;
    fs::write(f.root.join("unrelated.m4a"), bytes).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&f.root, &Default::default()).unwrap(),
    )
    .unwrap();
    let plan = plan_for(&f, &sources, false);
    assert!(matches!(
        row(&plan).decision,
        Decision::Skip {
            code: "managed_audio_replacement_proposed",
            ..
        }
    ));
    let other = plan
        .sources
        .iter()
        .find(|r| r.relative_path == "unrelated.m4a")
        .unwrap();
    assert!(matches!(other.decision, Decision::Add { .. }));
    assert_eq!(
        serde_json::to_value(other).unwrap()["album"]["id"],
        format!("{:016x}", 6002),
        "unrelated row reused blocked candidate album"
    );
    assert_eq!(
        serde_json::to_value(other).unwrap()["artist"]["id"],
        format!("{:016x}", 6003),
        "unrelated row reused blocked candidate artist"
    );
    assert!(
        serde_json::to_value(row(&plan))
            .unwrap()
            .get("album")
            .is_none()
            && serde_json::to_value(row(&plan))
                .unwrap()
                .get("artist")
                .is_none()
    );
}
fn nested_fixture() -> Fixture {
    Fixture::modified_plan(|v| {
        v["sources"][1]["relative_path"] = json!("sub/song.m4a");
        let root = PathBuf::from(v["source_root"].as_str().unwrap());
        fs::create_dir(root.join("sub")).unwrap();
        fs::rename(root.join("native.m4a"), root.join("sub/song.m4a")).unwrap();
    })
}
fn nested_desired(f: &Fixture) {
    changed_collection(f);
    fs::rename(f.root.join("native.m4a"), f.root.join("sub/song.m4a")).unwrap();
}
#[test]
fn managed_audio_nested_historical_file_selector_proposes_under_changed_relative_spelling() {
    let f = nested_fixture();
    nested_desired(&f);
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(
            &f.root.join("sub/song.m4a"),
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sources.tracks[0].relative_path, "song.m4a");
    assert_eq!(sources.base_directory, f.root.join("sub"));
    let plan = plan_for(&f, &sources, false);
    let evidence = proposed(&plan.sources[0]);
    assert_eq!(evidence["relative_path"], "sub/song.m4a");
    assert_eq!(evidence["desired_relative_path"], "song.m4a");
}
#[test]
fn managed_audio_foreign_nested_file_selector_is_not_joined_by_basename() {
    let f = nested_fixture();
    nested_desired(&f);
    let foreign = f.dir.join("foreign/sub");
    fs::create_dir_all(&foreign).unwrap();
    fs::copy(f.root.join("sub/song.m4a"), foreign.join("song.m4a")).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(
            &foreign.join("song.m4a"),
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let plan = plan_for(&f, &sources, false);
    assert!(matches!(plan.sources[0].decision, Decision::Add { .. }));
    assert!(plan.sources[0].managed_audio_replacement.is_none());
}

#[test]
fn managed_audio_nested_historical_directory_selector_proposes_under_changed_relative_spelling() {
    let f = nested_fixture();
    nested_desired(&f);
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(
            &f.root.join("sub"),
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sources.tracks[0].relative_path, "song.m4a");
    assert_eq!(sources.root, f.root.join("sub"));
    assert_eq!(sources.base_directory, f.root.join("sub"));
    let plan = plan_for(&f, &sources, false);
    let evidence = proposed(&plan.sources[0]);
    assert_eq!(evidence["relative_path"], "sub/song.m4a");
    assert_eq!(
        evidence["historical_absolute_source_path"],
        json!(f.root.join("sub/song.m4a"))
    );
    assert_eq!(evidence["desired_source_root"], json!(f.root.join("sub")));
}
#[test]
fn managed_audio_foreign_nested_directory_selector_is_not_joined_by_basename() {
    let f = nested_fixture();
    nested_desired(&f);
    let foreign = f.dir.join("foreign/sub");
    fs::create_dir_all(&foreign).unwrap();
    fs::copy(f.root.join("sub/song.m4a"), foreign.join("song.m4a")).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&foreign, &Default::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(sources.tracks[0].relative_path, "song.m4a");
    let plan = plan_for(&f, &sources, false);
    assert!(matches!(plan.sources[0].decision, Decision::Add { .. }));
    assert!(plan.sources[0].managed_audio_replacement.is_none());
}

// Real command caller lives in crate::engine::execution::sync::tests; shared
// roots need no command imports.
pub(super) async fn ordinary_audio_entrypoint_fixture<F, Fut>(run: F)
where
    F: FnOnce(PathBuf, PathBuf, PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let f = Fixture::new();
    changed_collection(&f);
    let snapshot = f.dir.join("snapshot");
    crate::engine::state::layout::private_directory(&snapshot).unwrap();
    crate::engine::state::layout::private_directory(&snapshot.join("catalog")).unwrap();
    let database = snapshot.join("catalog/MediaLibrary.sqlitedb");
    let catalog = music_protocol::test_support::catalogue::write_modified_collection_catalog(
        14,
        &f.catalog_sql("Standard title"),
        &database,
    );
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    index(&f, &catalog);
    let mut receipt: Value =
        serde_json::from_slice(&fs::read(f.dir.join("index.json")).unwrap()).unwrap();
    receipt["progress_sha256"] = json!(hex(&sha(b"")));
    private_write(
        &snapshot.join("complete-index.json"),
        &serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    private_write(&snapshot.join("progress.jsonl"), b"").unwrap();
    let prefs = fs::read(f.dir.join("saved-prefs.bin")).unwrap();
    let mut dictionary = plist::Dictionary::new();
    dictionary.insert("iPodPrefs".into(), Plist::Data(prefs.clone()));
    let mut plist = Vec::new();
    Plist::Dictionary(dictionary)
        .to_writer_binary(&mut plist)
        .unwrap();
    for name in ["before-iTunesPrefs", "after-iTunesPrefs"] {
        private_write(&snapshot.join(name), &prefs).unwrap();
        private_write(&snapshot.join(format!("{name}.plist")), &plist).unwrap();
    }
    // The proposal is left out without holding the plan, so the dry run goes
    // on to its live preflight, which the runner's phone cannot answer.
    let state = f.run.parent().unwrap().parent().unwrap();
    let error = run(f.root.clone(), state.to_path_buf(), snapshot.clone())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("connecting to usbmuxd"),
        "{error:#}"
    );
    let runs: Vec<_> = fs::read_dir(state.join("runs"))
        .unwrap()
        .map(|p| p.unwrap().path())
        .filter(|p| *p != f.run)
        .collect();
    assert_eq!(runs.len(), 1);
    let plan_path = runs[0].join("plan.json");
    let bytes = fs::read(&plan_path).unwrap();
    let plan: Value = serde_json::from_slice(&bytes).unwrap();
    let row = plan["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["relative_path"] == "native.m4a")
        .unwrap();
    assert_eq!(row["decision"], "skip");
    assert_eq!(row["code"], "managed_audio_replacement_proposed");
    assert_eq!(
        row["managed_audio_replacement"]["classification"],
        "audio_replacement_proposed_execution_blocked"
    );
    assert_eq!(
        row["managed_audio_replacement"]["original_bytes_available"],
        false
    );
    for flag in [
        "execution_authorized",
        "native_receiver_qualified",
        "start_authority",
        "completion_authority",
    ] {
        assert_eq!(row["managed_audio_replacement"][flag], false);
    }
    // Planned again from the same state, the saved plan is the same, and
    // ready: its one row is left out.
    let loaded =
        crate::engine::planning::saved_plan::load_read_only(&plan_path, &snapshot, &f.journal_path)
            .unwrap();
    assert!(!loaded.plan.has_conflicts());
    let mut tampered = plan.clone();
    tampered["sources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["relative_path"] == "native.m4a")
        .unwrap()["managed_audio_replacement"]["start_authority"] = json!(true);
    private_write(
        &runs[0].join("tampered-plan.json"),
        &serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap();
    let error = crate::engine::planning::saved_plan::load_read_only(
        &runs[0].join("tampered-plan.json"),
        &snapshot,
        &f.journal_path,
    )
    .err()
    .unwrap();
    assert!(
        format!("{error:#}").contains("saved plan differs"),
        "{error:#}"
    );
    assert!(!runs[0].join("audio").exists());
    let add_only = replacement_plan(&f, true);
    assert!(matches!(
        add_only
            .sources
            .iter()
            .find(|r| r.relative_path == "native.m4a")
            .unwrap()
            .decision,
        Decision::Add { .. }
    ));
    assert!(
        add_only
            .sources
            .iter()
            .find(|r| r.relative_path == "native.m4a")
            .unwrap()
            .managed_audio_replacement
            .is_none()
    );
}

/// The same source inserted by a Verified insert batch instead of a single
/// Insert attempt. The batch's retained input names the absolute source path
/// and collection scope; `change` edits it before the journal pins it.
fn batch_fixture(change: impl FnOnce(&mut Value)) -> Fixture {
    converted_batch_fixture(change, None)
}

/// `batch_fixture`, its target recorded as converted from `conversion`'s FLAC.
fn converted_batch_fixture(
    change: impl FnOnce(&mut Value),
    conversion: Option<crate::engine::state::journal::Conversion>,
) -> Fixture {
    use crate::engine::state::journal::{InsertBatchState, InsertIntent, InsertTarget};
    let mut f = Fixture::new();
    let fingerprint = music_protocol::media::fingerprint::read(&f.original).unwrap();
    let scope = json!({"root":f.root,"recursive":true,"include":[],"exclude":[]});
    let scope = sha(&serde_json::to_vec(&scope).unwrap());
    let path = "iTunes_Control/Music/F00/TEST.m4a";
    let state = f.dir.join("batch-state");
    crate::engine::state::layout::private_directory(&state).unwrap();
    let journal_path = crate::engine::state::layout::journal_path(
        &state,
        &f.identity.device_digest(),
        &f.identity.library_digest(),
    )
    .unwrap();
    let evidence = state.join("runs/0001/audio/batch-001");
    for directory in evidence.ancestors().take(4) {
        crate::engine::state::layout::private_directory(directory).unwrap();
    }
    let mut input = json!({"schema":"rustunes.insert-batch-input","base_anchor":13,
        "collection_scope_sha256":hex(&scope),"members":[{"source":"native.m4a","source_path":f.root.join("native.m4a"),
        "full_source_sha256":hex(&fingerprint.full_file_sha256),"file_bytes":fingerprint.file_bytes,
        "track":"00000000000003e9","album":"00000000000007d1","artist":"0000000000000bb9","path":path,
        "new_album":"00000000000007d1","new_artist":"0000000000000bb9","cover":null}]});
    change(&mut input);
    let input = serde_json::to_vec_pretty(&input).unwrap();
    fs::write(evidence.join("input.json"), &input).unwrap();
    let content = Content {
        source: fingerprint.clone().into(),
        track_pid: 1001,
        path: path.into(),
        artwork: None,
        collection: Some(scope),
    };
    let mut writable = Journal::create(&journal_path, &f.identity).unwrap();
    let id = writable
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: vec![5; 40],
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: sha(&input),
            evidence: evidence.clone(),
            targets: vec![InsertTarget {
                content,
                new_album_pid: Some(2001),
                new_artist_pid: Some(3001),
                conversion,
                source: None,
            }],
        })
        .unwrap();
    writable.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let claim = CompletionEvidence {
        track_pid: 1001,
        path: path.into(),
        audio_bytes: fingerprint.file_bytes,
        audio_file_sha256: fingerprint.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 14,
        receipt_sha256: [9; 32],
    };
    assert_eq!(
        writable
            .settle_insert_batch(id, [9; 32], 14, &[(0, claim)], &[])
            .unwrap(),
        InsertBatchState::Verified
    );
    drop(writable);
    f.journal = Journal::open_read_only(&journal_path, &f.identity).unwrap();
    f.journal_path = journal_path;
    f
}
#[test]
fn batch_inserted_changed_audio_at_its_source_path_is_proposed_for_replacement() {
    let f = batch_fixture(|_| {});
    let plan = replacement_plan(&f, false);
    let proposed = row(&plan);
    assert!(matches!(
        proposed.decision,
        Decision::Skip {
            code: "managed_audio_replacement_proposed",
            ..
        }
    ));
    let detail = proposed.managed_audio_replacement.as_ref().unwrap();
    assert_eq!(
        detail.classification,
        "audio_replacement_proposed_execution_blocked"
    );
    assert!(detail.replacement_identity_established && !detail.execution_authorized);
    let evidence = detail.evidence.as_ref().unwrap();
    assert_eq!(evidence["insert_batch"], 1);
    assert_eq!(evidence["target_ordinal"], 0);
    assert_eq!(
        evidence["immutable_original_content"]["track_id"],
        "00000000000003e9"
    );
    assert_eq!(
        evidence["historical_absolute_source_path"],
        json!(f.root.join("native.m4a"))
    );
    assert_ne!(
        evidence["immutable_original_content"]["source"]["audio"],
        evidence["desired_source"]["audio"]
    );
    // The add-only planner never looks for replacements.
    assert!(
        row(&replacement_plan(&f, true))
            .managed_audio_replacement
            .is_none()
    );
}
#[test]
fn a_session_inserted_track_is_associated_by_its_input_too() {
    let f = batch_fixture(|input| {
        input["schema"] = json!("rustunes.session-input");
        input["removals"] = json!([{"track":"00000000000003ea","item_pid":7,
            "path":"iTunes_Control/Music/F01/GONE.m4a","file_bytes":9,"file_sha256":null,"binding":null}]);
    });
    let plan = replacement_plan(&f, false);
    let detail = row(&plan).managed_audio_replacement.as_ref().unwrap();
    assert_eq!(
        detail.classification,
        "audio_replacement_proposed_execution_blocked"
    );
    assert_eq!(
        detail.evidence.as_ref().unwrap()["historical_absolute_source_path"],
        json!(f.root.join("native.m4a"))
    );
}
#[test]
fn batch_inserted_track_of_another_source_path_is_unrelated() {
    let f = batch_fixture(|input| {
        input["members"][0]["source_path"] = json!("/elsewhere/native.m4a");
    });
    let plan = replacement_plan(&f, false);
    assert!(matches!(row(&plan).decision, Decision::Add { .. }));
    assert!(row(&plan).managed_audio_replacement.is_none());
}
#[test]
fn batch_input_scope_that_differs_from_its_target_is_refused() {
    let f = batch_fixture(|input| {
        input["collection_scope_sha256"] = json!(hex(&[7; 32]));
    });
    refused(
        &replacement_plan(&f, false),
        "batch target's collection differs from its batch input",
    );
}
#[test]
fn batch_inserted_changed_audio_is_replaced_under_a_narrower_selection() {
    let f = batch_fixture(|_| {});
    changed_collection(&f);
    let mut options = crate::engine::planning::source_collection::SelectionOptions::default();
    options.include.insert("native.m4a".into());
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&f.root, &options).unwrap(),
    )
    .unwrap();
    let plan = plan_for(&f, &sources, false);
    let detail = row(&plan).managed_audio_replacement.as_ref().unwrap();
    assert!(
        detail.replacement_identity_established,
        "{}",
        serde_json::to_string(detail).unwrap()
    );
    assert_eq!(
        detail.evidence.as_ref().unwrap()["historical_absolute_source_path"],
        json!(f.root.join("native.m4a"))
    );
}
#[test]
fn batch_input_that_differs_from_the_journal_is_unassociated() {
    let f = batch_fixture(|_| {});
    let input = f
        .dir
        .join("batch-state/runs/0001/audio/batch-001/input.json");
    let mut bytes = fs::read(&input).unwrap();
    bytes.push(b'\n');
    fs::write(&input, bytes).unwrap();
    limitation(
        &replacement_plan(&f, false),
        "retained batch input differs from the journal",
    );
}

/// An MP3 an insert batch added from `song.mp3`, whose audio then changed
/// there (its tags stay). Returns the plan of that changed source and the
/// fixture holding the journal.
fn mpeg_batch_plan(source_path: Option<&str>, add_only: bool) -> (Fixture, Plan) {
    use crate::engine::state::journal::{InsertBatchState, InsertIntent, InsertTarget};
    let mut f = Fixture::new();
    let root = f.dir.join("mp3src");
    fs::create_dir(&root).unwrap();
    let tags = [
        ("TIT2", "Synthetic Title"),
        ("TPE1", "Synthetic Artist"),
        ("TALB", "Synthetic Album"),
    ];
    let audio = super::tests::mp3_fixture_bytes();
    let inserted = super::tests::tagged_mp3(&tags, &audio);
    fs::write(root.join("song.mp3"), &inserted).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&root, &Default::default()).unwrap(),
    )
    .unwrap();
    let source = sources.mp3_tracks[0].source.transferred_identity().unwrap();
    let scope = crate::engine::planning::removal::collection_scope_sha256(&sources).unwrap();
    let path = "iTunes_Control/Music/F00/TEST.mp3";
    let state = f.dir.join("batch-state");
    crate::engine::state::layout::private_directory(&state).unwrap();
    let journal_path = crate::engine::state::layout::journal_path(
        &state,
        &f.identity.device_digest(),
        &f.identity.library_digest(),
    )
    .unwrap();
    let evidence = state.join("runs/0001/audio/batch-001");
    for directory in evidence.ancestors().take(4) {
        crate::engine::state::layout::private_directory(directory).unwrap();
    }
    let source_path = source_path
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("song.mp3"));
    let input = json!({"schema":"rustunes.insert-batch-input","base_anchor":13,
        "collection_scope_sha256":hex(&scope),"members":[{"source":"song.mp3","source_path":source_path,
        "full_source_sha256":hex(&source.full_file_sha256),"file_bytes":source.file_bytes,
        "track":"00000000000003e9","album":"00000000000007d1","artist":"0000000000000bb9","path":path,
        "new_album":"00000000000007d1","new_artist":"0000000000000bb9","cover":null}]});
    let input = serde_json::to_vec_pretty(&input).unwrap();
    fs::write(evidence.join("input.json"), &input).unwrap();
    let mut writable = Journal::create(&journal_path, &f.identity).unwrap();
    let id = writable
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: vec![5; 40],
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: sha(&input),
            evidence: evidence.clone(),
            targets: vec![InsertTarget {
                content: Content {
                    source: source.clone(),
                    track_pid: 1001,
                    path: path.into(),
                    artwork: None,
                    collection: Some(scope),
                },
                new_album_pid: Some(2001),
                new_artist_pid: Some(3001),
                conversion: None,
                source: None,
            }],
        })
        .unwrap();
    writable.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let claim = CompletionEvidence {
        track_pid: 1001,
        path: path.into(),
        audio_bytes: source.file_bytes,
        audio_file_sha256: source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 14,
        receipt_sha256: [9; 32],
    };
    assert_eq!(
        writable
            .settle_insert_batch(id, [9; 32], 14, &[(0, claim)], &[])
            .unwrap(),
        InsertBatchState::Verified
    );
    drop(writable);
    f.journal = Journal::open_read_only(&journal_path, &f.identity).unwrap();
    f.journal_path = journal_path;
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        14,
        &format!(
            "UPDATE item_store SET sync_id=1001; UPDATE base_location SET path='iTunes_Control/Music/F00';
             UPDATE item SET genre_id=0,composer_pid=0,track_number=0;
             UPDATE item_extra SET location='TEST.mp3',file_size={},total_time_ms=52,track_count=0,year=0;",
            source.file_bytes
        ),
    );
    let m = source.audio.mpeg().unwrap();
    let h = catalog.source_hashes();
    let i = f.identity.index_identity();
    let value = json!({"schema":"rustunes.device-audio-index","complete":true,
        "identity":{"device_identifier_sha256":hex(&i.device_identifier_sha256),"library_identity_sha256":hex(&i.library_identity_sha256)},
        "catalog":{"database_sha256":hex(&h.database),"database_bytes":h.database_bytes,"wal_sha256":h.wal.as_ref().map(|s|hex(s)),"wal_bytes":h.wal_bytes,"persisted_media_anchor":catalog.persisted_media_anchor()},
        "track_count":1,"total_bytes":source.file_bytes,"alac_track_count":0,"other_track_count":1,"encoded_audio_kind":"encoded-alac-not-pcm",
        "records":[],"other_records":[{"sync_id":"00000000000003E9","afc_path":path,
            "attributes":{"st_ifmt":"S_IFREG","st_mtime":"1","st_size":source.file_bytes.to_string()},"file_bytes":source.file_bytes,"full_file_sha256":hex(&source.full_file_sha256),
            "codec":{"kind":"mpeg1-layer3","sample_rate":m.sample_rate,"channels":m.channels,"frame_count":m.frame_count,"audio_offset":m.audio_offset,"audio_bytes":m.audio_bytes,"trailing_tag_bytes":m.trailing_bytes}}],
        "progress_sha256":hex(&[4;32]),"completed_unix_time_ns":"1","fresh_prefs_before_after_components_equal":true,"atomic_snapshot_claim":false,"catalog_metadata":{}});
    let bytes = serde_json::to_vec(&value).unwrap();
    let p = f.dir.join("mpeg-index.json");
    fs::write(&p, &bytes).unwrap();
    let index = DeviceAudioIndex::load_complete(&p, hash(&bytes), &catalog, &i).unwrap();
    // The same tags over other frames.
    let mut changed = audio.clone();
    changed[100] ^= 1;
    fs::write(
        root.join("song.mp3"),
        super::tests::tagged_mp3(&tags, &changed),
    )
    .unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&root, &Default::default()).unwrap(),
    )
    .unwrap();
    let candidates = BTreeMap::from([(
        "song.mp3".to_owned(),
        Candidate {
            ids: CandidateIds {
                track: 5001,
                album: Some(5002),
                artist: Some(5003),
            },
            path: "iTunes_Control/Music/F00/NEWX.mp3".into(),
        },
    )]);
    let plan = build_with_policy(
        &sources,
        &catalog,
        &index,
        index.identity(),
        Some(JournalEvidence {
            journal: &f.journal,
            identity: &f.identity,
        }),
        &candidates,
        add_only,
    )
    .unwrap();
    (f, plan)
}

#[test]
fn batch_inserted_mp3_with_changed_audio_at_its_source_path_is_proposed_for_replacement() {
    let (_f, plan) = mpeg_batch_plan(None, false);
    let row = &plan.sources[0];
    let evidence = proposed(row);
    assert_eq!(evidence["insert_batch"], 1);
    assert_eq!(evidence["relative_path"], "song.mp3");
    assert_eq!(
        evidence["immutable_original_content"]["path"],
        "iTunes_Control/Music/F00/TEST.mp3"
    );
    assert_ne!(
        evidence["immutable_original_content"]["source"]["audio"]["frame_bytes_sha256"],
        evidence["desired_source"]["audio"]["frame_bytes_sha256"]
    );
    assert!(row.managed_audio_replacement.as_ref().unwrap().blocks());
    // Add-only looks for no replacement, and the track that may be an earlier
    // version of it is not duplicated.
    for (path, add_only) in [(None, true), (Some("/elsewhere/song.mp3"), false)] {
        let (_f, plan) = mpeg_batch_plan(path, add_only);
        let row = &plan.sources[0];
        assert!(row.managed_audio_replacement.is_none());
        assert!(
            matches!(&row.decision, Decision::Skip { code, .. } if *code == "plausible_existing_audio_requires_comparison"),
            "{}",
            serde_json::to_value(row).unwrap()
        );
    }
}

fn pinned_source_paths(f: &Fixture) -> std::collections::BTreeMap<u64, Result<PathBuf, String>> {
    let control = crate::engine::execution::native_insert::Control {
        deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    source_paths(&f.journal, &f.identity, &control).unwrap()
}
#[test]
fn source_paths_name_where_single_and_batch_insertions_came_from() {
    for f in [Fixture::new(), batch_fixture(|_| {})] {
        assert_eq!(
            pinned_source_paths(&f),
            [(1001, Ok(f.root.join("native.m4a")))].into()
        );
    }
    let f = batch_fixture(|input| {
        input["members"][0]["source_path"] = json!("/elsewhere/native.m4a");
    });
    assert_eq!(
        pinned_source_paths(&f),
        [(1001, Ok(PathBuf::from("/elsewhere/native.m4a")))].into()
    );
    // An ALAC converted from a FLAC was sent from a temporary file; the FLAC
    // the journal records is where it came from.
    let flac = f.root.join("native.flac");
    let f = converted_batch_fixture(
        |input| input["members"][0]["source_path"] = json!("/tmp/conversion/0011.m4a"),
        Some(crate::engine::state::journal::Conversion {
            source_path: flac.clone(),
            source_bytes: 900,
            source_sha256: [4; 32],
            source_audio: [5; 32],
            ffmpeg: "ffmpeg version 8.1.3".into(),
        }),
    );
    assert_eq!(pinned_source_paths(&f), [(1001, Ok(flac))].into());
}
#[test]
fn source_paths_report_why_a_track_has_no_pinned_source() {
    let f = batch_fixture(|input| {
        input["collection_scope_sha256"] = json!(hex(&[7; 32]));
    });
    let paths = pinned_source_paths(&f);
    assert!(
        paths[&1001]
            .as_ref()
            .unwrap_err()
            .contains("batch target's collection differs from its batch input")
    );
    let f = batch_fixture(|_| {});
    let input = f
        .dir
        .join("batch-state/runs/0001/audio/batch-001/input.json");
    let mut bytes = fs::read(&input).unwrap();
    bytes.push(b'\n');
    fs::write(&input, bytes).unwrap();
    assert!(
        pinned_source_paths(&f)[&1001]
            .as_ref()
            .unwrap_err()
            .contains("retained batch input differs from the journal")
    );
    let f = Fixture::new();
    fs::remove_file(f.run.join("plan.json")).unwrap();
    assert!(pinned_source_paths(&f)[&1001].is_err());
}

/// A FLAC whose audio changed since an insert batch sent the ALAC converted
/// from it: a replacement of that track is proposed, with or without
/// --flac-to-alac, and nothing is added beside it.
#[test]
fn a_flac_whose_audio_changed_since_its_conversion_is_proposed_for_replacement() {
    use crate::engine::state::journal::{Conversion, InsertBatchState, InsertIntent, InsertTarget};
    let mut f = Fixture::new();
    let root = f.dir.join("flacsrc");
    fs::create_dir(&root).unwrap();
    let flac = root.join("song.flac");
    fs::write(&flac, super::tests::tagged_flac(&super::tests::FLAC_TAGS)).unwrap();
    let sources = crate::engine::planning::source_collection::inspect(
        &crate::engine::planning::source_collection::select(&root, &Default::default()).unwrap(),
    )
    .unwrap();
    let scope = crate::engine::planning::removal::collection_scope_sha256(&sources).unwrap();
    // The ALAC that was sent, from a temporary file since removed.
    let alac = music_protocol::media::fingerprint::read(&f.original).unwrap();
    let path = "iTunes_Control/Music/F00/TEST.m4a";
    let state = f.dir.join("batch-state");
    crate::engine::state::layout::private_directory(&state).unwrap();
    let journal_path = crate::engine::state::layout::journal_path(
        &state,
        &f.identity.device_digest(),
        &f.identity.library_digest(),
    )
    .unwrap();
    let evidence = state.join("runs/0001/audio/batch-001");
    for directory in evidence.ancestors().take(4) {
        crate::engine::state::layout::private_directory(directory).unwrap();
    }
    let input = json!({"schema":"rustunes.insert-batch-input","base_anchor":13,
        "collection_scope_sha256":hex(&scope),"members":[{"source":"song.flac",
        "source_path":"/tmp/rustunes-flac-1-2/0011223344556677.m4a",
        "full_source_sha256":hex(&alac.full_file_sha256),"file_bytes":alac.file_bytes,
        "track":"00000000000003e9","album":"00000000000007d1","artist":"0000000000000bb9","path":path,
        "new_album":"00000000000007d1","new_artist":"0000000000000bb9","cover":null}]});
    let input = serde_json::to_vec_pretty(&input).unwrap();
    fs::write(evidence.join("input.json"), &input).unwrap();
    let content = Content {
        source: alac.clone().into(),
        track_pid: 1001,
        path: path.into(),
        artwork: None,
        collection: Some(scope),
    };
    let mut writable = Journal::create(&journal_path, &f.identity).unwrap();
    let id = writable
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: vec![5; 40],
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: sha(&input),
            evidence: evidence.clone(),
            targets: vec![InsertTarget {
                content,
                new_album_pid: Some(2001),
                new_artist_pid: Some(3001),
                // The FLAC then held other audio than it does now.
                conversion: Some(Conversion {
                    source_path: flac.clone(),
                    source_bytes: 900,
                    source_sha256: [4; 32],
                    source_audio: [5; 32],
                    ffmpeg: "ffmpeg version 8.1.3".into(),
                }),
                source: None,
            }],
        })
        .unwrap();
    writable.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let claim = CompletionEvidence {
        track_pid: 1001,
        path: path.into(),
        audio_bytes: alac.file_bytes,
        audio_file_sha256: alac.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 14,
        receipt_sha256: [9; 32],
    };
    assert_eq!(
        writable
            .settle_insert_batch(id, [9; 32], 14, &[(0, claim)], &[])
            .unwrap(),
        InsertBatchState::Verified
    );
    drop(writable);
    f.journal = Journal::open_read_only(&journal_path, &f.identity).unwrap();
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        14,
        "UPDATE item_store SET sync_id=1001; UPDATE base_location SET path='iTunes_Control/Music/F00';",
    );
    let i = f.identity.index_identity();
    let mut value = super::tests::receipt(&catalog, &alac, false);
    value["identity"] = json!({"device_identifier_sha256":hex(&i.device_identifier_sha256),"library_identity_sha256":hex(&i.library_identity_sha256)});
    let bytes = serde_json::to_vec(&value).unwrap();
    let p = f.dir.join("alac-index.json");
    fs::write(&p, &bytes).unwrap();
    let index = DeviceAudioIndex::load_complete(&p, hash(&bytes), &catalog, &i).unwrap();
    let candidates = BTreeMap::from([(
        "song.flac".to_owned(),
        Candidate {
            ids: CandidateIds {
                track: 5001,
                album: Some(5002),
                artist: Some(5003),
            },
            path: "iTunes_Control/Music/F00/NEWX.m4a".into(),
        },
    )]);
    for flag in [true, false] {
        let mut sources = crate::engine::planning::source_collection::inspect(
            &crate::engine::planning::source_collection::select(&root, &Default::default())
                .unwrap(),
        )
        .unwrap();
        sources.flac_to_alac = flag;
        let plan = build(
            &sources,
            &catalog,
            &index,
            index.identity(),
            Some(JournalEvidence {
                journal: &f.journal,
                identity: &f.identity,
            }),
            &candidates,
        )
        .unwrap();
        let row = &plan.sources[0];
        assert!(
            matches!(&row.decision, Decision::Skip { code, message, .. }
                if *code == "managed_audio_replacement_proposed" && message.contains("from a FLAC file")),
            "{}",
            serde_json::to_value(row).unwrap()
        );
        let evidence = row
            .managed_audio_replacement
            .as_ref()
            .unwrap()
            .evidence
            .as_ref()
            .unwrap();
        assert_eq!(evidence["historical_absolute_source_path"], json!(flac));
        assert_eq!(evidence["converted_from"]["source_path"], json!(flac));
        assert_eq!(
            evidence["immutable_original_content"]["track_id"],
            "00000000000003e9"
        );
    }
    // The same file with its title repeated cannot be sent, so it is left
    // out whole: no replacement, no update, and its track stays as it is.
    // (This fixture's phone index drifts from its journal binding, which
    // holds both plans whatever the source; the rows are what is checked.)
    let tags = super::tests::FLAC_TAGS;
    fs::write(
        &flac,
        super::tests::tagged_flac(&[tags[0], ("TITLE", "Other Title"), tags[1], tags[2]]),
    )
    .unwrap();
    for flag in [true, false] {
        let mut sources = crate::engine::planning::source_collection::inspect(
            &crate::engine::planning::source_collection::select(&root, &Default::default())
                .unwrap(),
        )
        .unwrap();
        sources.flac_to_alac = flag;
        let plan = build(
            &sources,
            &catalog,
            &index,
            index.identity(),
            Some(JournalEvidence {
                journal: &f.journal,
                identity: &f.identity,
            }),
            &candidates,
        )
        .unwrap();
        let row = &plan.sources[0];
        assert!(
            matches!(&row.decision, Decision::Skip { code, .. } if *code == "mp3_source_profile_unsupported"),
            "{}",
            serde_json::to_value(row).unwrap()
        );
        assert!(row.managed_audio_replacement.is_none());
        // `sync --replace-changed` finds nothing to replace, and `sync` sends
        // no edit.
        assert!(
            crate::engine::execution::replace::replacements(&plan)
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::engine::execution::metadata_batch::source_updates(&plan)
                .unwrap()
                .is_empty()
        );
        // Nor does `sync` remove the track: its file is still at the path the
        // journal pins, and its audio is still in the selection.
        let origins = crate::engine::execution::delete::missing_origins(
            &f.journal,
            &f.identity,
            &crate::engine::planning::source_collection::select(&root, &Default::default())
                .unwrap(),
            &crate::engine::execution::native_insert::Control {
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        )
        .unwrap();
        assert_eq!(
            origins.get(&1001),
            Some(&crate::engine::planning::removal::Origin::Present)
        );
        let removal = crate::engine::planning::removal::build(
            &catalog,
            &index,
            index.identity(),
            Some(JournalEvidence {
                journal: &f.journal,
                identity: &f.identity,
            }),
            crate::engine::planning::removal::Request::Missing {
                sources: Some(&sources),
                origins: &origins,
            },
        )
        .unwrap();
        assert!(
            removal.proposed_removal_ids.is_empty(),
            "{}",
            serde_json::to_value(&removal).unwrap()
        );
        assert_eq!(removal.tracks[0].reasons, ["source_present"]);
    }
}
