use super::*;
use crate::engine::planning::source_collection::mp3::preparation::PreparedSource;
use crate::engine::planning::source_validation;
use crate::{
    engine::planning::source_collection::{SelectionOptions, inspect, select},
    engine::state::journal::Content,
};
use music_protocol::media::fingerprint::{self, SourceFingerprint};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn artwork_proposals_disclose_exact_potential_recent_resets_but_keeps_do_not() {
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        12,
        "UPDATE item_stats SET play_count_recent=4;",
    );
    let mut row = SourceDecision {
        relative_path: "fixture.m4a".into(),
        managed_audio_replacement: None,
        source_file_bytes: Some(1),
        source_file_sha256: Some("a".repeat(64)),
        encoded_audio_sha256: Some("b".repeat(64)),
        projected: None,
        mp3: None,
        mp3_insertion: None,
        flac_to_alac: false,
        validation_issues: Vec::new(),
        matches: Vec::new(),
        artwork: ArtworkDecision::NoSourceCoverPreserveExisting,
        native_composer: None,
        decision: Decision::Keep,
    };
    assert!(
        artwork_recent_reset_disclosure(std::slice::from_ref(&row), &catalog)
            == (Vec::new(), Vec::new())
    );
    for art in [
        ArtworkDecision::PendingOwnedCover {
            sha256: "c".repeat(64),
            bytes: 1,
        },
        ArtworkDecision::SeparateUpdateNeeded {
            sha256: "c".repeat(64),
            bytes: 1,
        },
    ] {
        row.artwork = art;
        let (reset, skips) = artwork_recent_reset_disclosure(std::slice::from_ref(&row), &catalog);
        assert!(skips.is_empty());
        assert_eq!(reset.len(), 1);
        assert_eq!(reset[0]["track_id"], "ffffffffffffffff");
        assert_eq!(reset[0]["before"], 4);
        assert_eq!(reset[0]["after"], 0);
    }
    row.decision = conflict("blocked", "not executable".into());
    assert!(
        artwork_recent_reset_disclosure(std::slice::from_ref(&row), &catalog)
            == (Vec::new(), Vec::new())
    );
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "sync-plan-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> PathBuf {
    crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a")
}
fn sources() -> SourceCollection {
    inspect(&select(&fixture(), &SelectionOptions::default()).unwrap()).unwrap()
}
fn identity() -> IndexIdentity {
    IndexIdentity {
        device_identifier_sha256: [1; 32],
        library_identity_sha256: [2; 32],
    }
}
fn different(mut fp: SourceFingerprint) -> SourceFingerprint {
    fp.audio.media_sha256 = [9; 32];
    fp.audio.sha256 = fp.audio.composite_sha256();
    fp
}
pub(super) fn receipt(catalog: &Catalog, fp: &SourceFingerprint, other: bool) -> Value {
    let t = &catalog.tracks()[0];
    let h = catalog.source_hashes();
    let a = &fp.audio;
    let record = json!({"sync_id":format!("{:016X}",t.sync_id),"afc_path":t.afc_path,"attributes":{"st_ifmt":"S_IFREG","st_mtime":"1","st_size":t.metadata.file_size.to_string()},
        "fingerprint":{"file_bytes":t.metadata.file_size,"full_file_sha256":hex(&fp.full_file_sha256),"audio":{"sha256":hex(&a.sha256),"alac_config_sha256":hex(&a.alac_config_sha256),
        "sample_sizes_sha256":hex(&a.sample_sizes_sha256),"media_sha256":hex(&a.media_sha256),"timescale":a.timescale,"duration_ticks":a.duration_ticks,"media_bytes":a.media_bytes}}});
    let mut value = json!({"schema":"rustunes.device-audio-index","complete":true,
        "identity":{"device_identifier_sha256":hex(&identity().device_identifier_sha256),"library_identity_sha256":hex(&identity().library_identity_sha256)},
        "catalog":{"database_sha256":hex(&h.database),"database_bytes":h.database_bytes,"wal_sha256":h.wal.as_ref().map(hex),"wal_bytes":h.wal_bytes,"persisted_media_anchor":catalog.persisted_media_anchor()},
        "track_count":1,"total_bytes":t.metadata.file_size,"alac_track_count":1,"other_track_count":0,
        "encoded_audio_kind":"encoded-alac-not-pcm","records":[record],"other_records":[],"progress_sha256":hex(&[4;32]),"completed_unix_time_ns":"1",
        "fresh_prefs_before_after_components_equal":true,"atomic_snapshot_claim":false,"catalog_metadata":{}});
    if other {
        value["records"] = json!([]);
        value["alac_track_count"] = json!(0);
        value["other_track_count"] = json!(1);
        value["other_records"] = json!([{"sync_id":format!("{:016X}",t.sync_id),"afc_path":t.afc_path,
            "attributes":{"st_ifmt":"S_IFREG","st_mtime":"1","st_size":t.metadata.file_size.to_string()},"file_bytes":t.metadata.file_size,"full_file_sha256":hex(&fp.full_file_sha256),
            "codec":{"kind":"mpeg1-layer3","sample_rate":44100,"channels":2,"frame_count":10,"audio_offset":0,"audio_bytes":t.metadata.file_size,"trailing_tag_bytes":0}}]);
    }
    value
}
fn load(catalog: &Catalog, receipt: &Value) -> Result<DeviceAudioIndex> {
    load_for(catalog, receipt, &identity())
}
fn load_for(
    catalog: &Catalog,
    receipt: &Value,
    identity: &IndexIdentity,
) -> Result<DeviceAudioIndex> {
    let temp = Temp::new();
    let path = temp.0.join("complete.json");
    let bytes = serde_json::to_vec(receipt)?;
    fs::write(&path, &bytes)?;
    DeviceAudioIndex::load_complete(&path, Sha256::digest(bytes).into(), catalog, identity)
}
fn candidates(source: &SourceCollection) -> BTreeMap<String, Candidate> {
    BTreeMap::from([(
        source.tracks[0].relative_path.clone(),
        Candidate {
            ids: CandidateIds {
                track: 1001,
                album: Some(1002),
                artist: Some(1003),
            },
            path: "iTunes_Control/Music/F39/ABCD.m4a".into(),
        },
    )])
}

#[test]
fn exact_add_proposal_retains_ids_fields_and_preserves_unmatched_phone_without_reservation() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let other = different(fingerprint::read(&fixture()).unwrap());
    let index = load(&catalog, &receipt(&catalog, &other, false)).unwrap();
    let proposed = candidates(&source);
    let plan = build(&source, &catalog, &index, &identity(), None, &proposed).unwrap();
    assert_eq!(plan.sources.len(), 1);
    assert!(plan.sources[0].matches.is_empty());
    assert_eq!(plan.phone_deletions, 0);
    assert!(!plan.execution_authorized_by_plan);
    assert!(
        serde_json::to_value(&plan)
            .unwrap()
            .get("artwork_import_recent_play_resets")
            .is_none()
    );
    assert_eq!(
        plan.metadata_comparison_scope.unverified_fields,
        vec!["content_rating", "compilation"]
    );
    assert_eq!(
        plan.preserved_unmatched_device_tracks,
        vec!["ffffffffffffffff"]
    );
    let Decision::Add {
        track_id,
        album,
        artist,
        path,
        ..
    } = &plan.sources[0].decision
    else {
        panic!("expected additive proposal")
    };
    assert_eq!(track_id, "00000000000003e9");
    assert_eq!(album.id, "00000000000003ea");
    assert_eq!(artist.id, "00000000000003eb");
    assert_eq!(path, "iTunes_Control/Music/F39/ABCD.m4a");
    assert_eq!(album.status, "new_this_source");
    assert_eq!(
        plan.sources[0].projected.as_ref().unwrap().title,
        "Standard title"
    );
    let again = build(&source, &catalog, &index, &identity(), None, &proposed).unwrap();
    assert_eq!(
        serde_json::to_value(&plan).unwrap(),
        serde_json::to_value(again).unwrap()
    );
}

#[test]
fn encoded_match_reports_metadata_drift_and_container_difference_without_inserting() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut phone = fingerprint::read(&fixture()).unwrap();
    phone.full_file_sha256 = [8; 32];
    let index = load(&catalog, &receipt(&catalog, &phone, false)).unwrap();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(matches!(
        plan.sources[0].decision,
        Decision::Skip {
            code: "existing_metadata_update_unqualified",
            ..
        }
    ));
    assert_eq!(plan.sources[0].matches.len(), 1);
    let matched = &plan.sources[0].matches[0];
    assert_eq!(matched.track_id, "ffffffffffffffff");
    assert_eq!(matched.path, "iTunes_Control/Music/F39/TEST.m4a");
    assert!(matched.encoded_audio_equal);
    assert!(!matched.full_container_equal);
    let serialized = serde_json::to_value(matched).unwrap();
    assert_eq!(serialized["track_id"], "ffffffffffffffff");
    assert_eq!(serialized["encoded_audio_equal"], true);
    assert_eq!(serialized["full_container_equal"], false);
    assert_eq!(matched.ownership, "unowned");
    assert!(
        matched
            .catalog_differences
            .iter()
            .any(|f| f.field == "title")
    );
    assert!(plan.preserved_unmatched_device_tracks.is_empty());
    let additive = build_with_policy(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
        true,
    )
    .unwrap();
    assert!(matches!(additive.sources[0].decision, Decision::Keep));
    assert!(additive.preserve_existing_tracks);
    assert!(
        !additive.sources[0].matches[0]
            .catalog_differences
            .is_empty()
    );
    assert_eq!(additive.phone_deletions, 0);
    assert_eq!(
        serde_json::to_value(additive).unwrap()["preserve_existing_tracks"],
        true
    );
    assert!(
        serde_json::to_value(plan)
            .unwrap()
            .get("preserve_existing_tracks")
            .is_none()
    );
}

#[test]
fn existing_duplicate_audio_retains_every_identity_in_machine_output() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::composer_transition_catalog(
        "Synthetic Composer",
        Some("Synthetic Composer"),
        100,
        true,
    );
    let phone = fingerprint::read(&fixture()).unwrap();
    let mut value = receipt(&catalog, &phone, false);
    let template = value["records"][0].clone();
    value["records"] = Value::Array(
        catalog
            .tracks()
            .iter()
            .map(|track| {
                let mut record = template.clone();
                record["sync_id"] = json!(format!("{:016X}", track.sync_id));
                record["afc_path"] = json!(track.afc_path);
                if track.sync_id == u64::MAX {
                    record["fingerprint"]["full_file_sha256"] = json!(hex(&[8; 32]));
                }
                record
            })
            .collect(),
    );
    value["track_count"] = json!(2);
    value["alac_track_count"] = json!(2);
    value["total_bytes"] = json!(
        catalog
            .tracks()
            .iter()
            .map(|track| track.metadata.file_size)
            .sum::<u64>()
    );
    let index = load(&catalog, &value).unwrap();
    for preserve_existing_tracks in [false, true] {
        let plan = build_with_policy(
            &source,
            &catalog,
            &index,
            &identity(),
            None,
            &BTreeMap::new(),
            preserve_existing_tracks,
        )
        .unwrap();
        assert!(matches!(
            plan.sources[0].decision,
            Decision::Conflict {
                code: "multiple_existing_audio_matches",
                ..
            }
        ));
        assert!(plan.preserved_unmatched_device_tracks.is_empty());
        assert_eq!(plan.phone_deletions, 0);
        assert!(!plan.execution_authorized_by_plan);
        let output = serde_json::to_value(&plan).unwrap();
        let row = &output["sources"][0];
        assert_eq!(row["decision"], "conflict");
        assert_eq!(row["code"], "multiple_existing_audio_matches");
        assert!(row.get("track_id").is_none());
        let matches = row["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0]["track_id"], "00000000000003e8");
        assert_eq!(matches[0]["path"], "iTunes_Control/Music/F39/NEW.m4a");
        assert_eq!(matches[0]["encoded_audio_equal"], true);
        assert_eq!(matches[0]["full_container_equal"], true);
        assert_eq!(matches[1]["track_id"], "ffffffffffffffff");
        assert_eq!(matches[1]["path"], "iTunes_Control/Music/F39/TEST.m4a");
        assert_eq!(matches[1]["encoded_audio_equal"], true);
        assert_eq!(matches[1]["full_container_equal"], false);
    }
}

#[test]
fn add_only_preserves_matched_compilation_but_still_refuses_its_insertion() {
    use lofty::{
        config::{ParseOptions, WriteOptions},
        file::AudioFile,
        mp4::{Atom, AtomData, AtomIdent, Mp4File},
    };
    let temp = Temp::new();
    let path = temp.0.join("compilation.m4a");
    let mut file = std::io::Cursor::new(fs::read(fixture()).unwrap());
    let mut mp4 = Mp4File::read_from(&mut file, ParseOptions::default()).unwrap();
    mp4.ilst_mut()
        .unwrap()
        .insert(Atom::new(AtomIdent::Fourcc(*b"cpil"), AtomData::Bool(true)));
    mp4.save_to(&mut file, WriteOptions::default()).unwrap();
    fs::write(&path, file.into_inner()).unwrap();
    let sources = inspect(&select(&path, &SelectionOptions::default()).unwrap()).unwrap();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let same = sources.tracks[0].fingerprint.clone();
    let matched = load(&catalog, &receipt(&catalog, &same, false)).unwrap();
    let preserved = build_with_policy(
        &sources,
        &catalog,
        &matched,
        &identity(),
        None,
        &BTreeMap::new(),
        true,
    )
    .unwrap();
    assert!(matches!(preserved.sources[0].decision, Decision::Keep));
    assert!(matches!(
        preserved.sources[0].artwork,
        ArtworkDecision::PreservedExistingTrack { .. }
    ));
    assert!(
        preserved.sources[0]
            .validation_issues
            .iter()
            .any(|i| i.code == "compilation_grouping_unverified")
    );
    let missing = load(&catalog, &receipt(&catalog, &different(same), false)).unwrap();
    let refused = build_with_policy(
        &sources,
        &catalog,
        &missing,
        &identity(),
        None,
        &candidates(&sources),
        true,
    )
    .unwrap();
    assert!(matches!(
        refused.sources[0].decision,
        Decision::Skip {
            code: "unsupported_source",
            ..
        }
    ));
}

#[test]
fn started_journal_blocks_new_proposals_without_changing_its_durable_state() {
    use crate::engine::state::journal::{
        AttemptKind, AttemptState, PreparedAttempt, StartEvidence,
    };
    let source = sources();
    let s = &source.tracks[0];
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4] = 1;
    raw[6] = 25;
    raw[8] = 1;
    raw[12] = 1;
    raw[20] = 2;
    let library = music_protocol::catalogue::identity::parse(&raw).unwrap();
    let scope = Identity::new("planner-test-device", &library).unwrap();
    let identity = scope.index_identity();
    let mut input = receipt(&catalog, &different(s.fingerprint.clone()), false);
    input["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &input, &identity).unwrap();
    let temp = Temp::new();
    let directory = temp.0.join("journal");
    let mut journal = Journal::create(&directory, &scope).unwrap();
    let proposed = candidates(&source);
    let candidate = &proposed[&s.relative_path];
    let p = Planner::new(&catalog, &BTreeSet::new())
        .plan(&s.metadata, candidate.ids)
        .unwrap();
    let operation = crate::engine::execution::insert::prepare_live(
        &s.metadata,
        &p,
        &catalog,
        std::time::UNIX_EPOCH,
    )
    .unwrap();
    let attempt = journal
        .prepare(PreparedAttempt {
            kind: AttemptKind::Insert,
            content: Content {
                source: (s.fingerprint.clone()).into(),
                track_pid: p.track_pid(),
                path: candidate.path.clone(),
                artwork: None,
                collection: None,
            },
            new_album_pid: Some(p.album_pid()),
            new_artist_pid: Some(p.artist_pid()),
            current_media_anchor: 13,
            operation,
            input_receipt_sha256: [1; 32],
        })
        .unwrap();
    let _permit = journal
        .mark_started(
            attempt,
            StartEvidence {
                ready_media_anchor: 13,
                ready_receipt_sha256: [2; 32],
                signature: [1; 21],
            },
        )
        .unwrap();
    let before: [u8; 32] =
        Sha256::digest(fs::read(directory.join("journal.sqlite")).unwrap()).into();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity,
        Some(JournalEvidence {
            journal: &journal,
            identity: &scope,
        }),
        &proposed,
    )
    .unwrap();
    assert!(matches!(
        plan.sources[0].decision,
        Decision::Conflict {
            code: "journal_reconciliation_required",
            ..
        }
    ));
    assert_eq!(
        plan.journal_blockers,
        vec!["outstanding_started_requires_reconciliation"]
    );
    assert_eq!(
        journal.attempt(attempt).unwrap().state,
        AttemptState::ReconciliationRequired
    );
    assert_eq!(
        before,
        <[u8; 32]>::from(Sha256::digest(
            fs::read(directory.join("journal.sqlite")).unwrap()
        ))
    );
}

#[test]
fn pending_attempts_block_additive_plans_without_journal_changes() {
    use crate::engine::state::journal::{StartEvidence, test_support};
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (scope, identity) = journal_scope_for_consumers();
    let mut input = receipt(
        &catalog,
        &different(source.tracks[0].fingerprint.clone()),
        false,
    );
    input["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &input, &identity).unwrap();
    let temp = Temp::new();
    let directory = temp.0.join("journal");
    let mut journal = Journal::create(&directory, &scope).unwrap();
    let audio = index.tracks().values().next().unwrap();
    let c = Content {
        source: (audio.fingerprint.clone()).into(),
        track_pid: audio.sync_id,
        path: audio.afc_path.clone(),
        artwork: None,
        collection: None,
    };
    test_support::insert_verified(&mut journal, c.clone());
    let attempt = test_support::prepare_insert(&mut journal, test_support::alac_content(9));
    for started in [false, true] {
        if started {
            journal
                .mark_started(
                    attempt,
                    StartEvidence {
                        ready_media_anchor: 13,
                        ready_receipt_sha256: [6; 32],
                        signature: [1; 21],
                    },
                )
                .unwrap();
        }
        let before = fs::read(directory.join("journal.sqlite")).unwrap();
        let plan = build(
            &source,
            &catalog,
            &index,
            &identity,
            Some(JournalEvidence {
                journal: &journal,
                identity: &scope,
            }),
            &candidates(&source),
        )
        .unwrap();
        assert_eq!(
            plan.journal_blockers,
            [if started {
                "outstanding_started_requires_reconciliation"
            } else {
                "outstanding_prepared_requires_resolution"
            }]
        );
        assert!(plan.sources.iter().all(|s| matches!(
            s.decision,
            Decision::Conflict {
                code: "journal_reconciliation_required",
                ..
            }
        )));
        assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
    }
}

#[test]
fn duplicate_local_audio_is_visible_for_every_path_and_uses_no_candidates() {
    let temp = Temp::new();
    fs::copy(fixture(), temp.0.join("one.m4a")).unwrap();
    fs::copy(fixture(), temp.0.join("two.m4a")).unwrap();
    let mut source = inspect(&select(&temp.0, &SelectionOptions::default()).unwrap()).unwrap();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let index = load(
        &catalog,
        &receipt(
            &catalog,
            &different(source.tracks[0].fingerprint.clone()),
            false,
        ),
    )
    .unwrap();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(plan.sources.len(), 2);
    assert!(plan.sources.iter().all(|s| matches!(
        s.decision,
        Decision::Skip {
            code: "duplicate_source_audio",
            ..
        }
    )));
    source.tracks.pop();
    assert!(
        build(
            &source,
            &catalog,
            &index,
            &identity(),
            None,
            &BTreeMap::new()
        )
        .is_err()
    );
}

#[test]
fn exact_plan_exposes_hidden_and_explicit_selection_exclusions() {
    let temp = Temp::new();
    fs::copy(fixture(), temp.0.join("selected.m4a")).unwrap();
    fs::copy(fixture(), temp.0.join("excluded.m4a")).unwrap();
    fs::create_dir(temp.0.join(".hidden")).unwrap();
    fs::copy(fixture(), temp.0.join(".hidden/old.m4a")).unwrap();
    let options = SelectionOptions {
        exclude: BTreeSet::from(["excluded.m4a".into()]),
        ..Default::default()
    };
    let source = inspect(&select(&temp.0, &options).unwrap()).unwrap();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let index = load(
        &catalog,
        &receipt(
            &catalog,
            &different(source.tracks[0].fingerprint.clone()),
            false,
        ),
    )
    .unwrap();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &candidates(&source),
    )
    .unwrap();
    assert!(!plan.has_conflicts());
    assert_eq!(plan.selection.selected_files, 1);
    assert_eq!(
        plan.selection.exclude,
        BTreeSet::from(["excluded.m4a".into()])
    );
    assert!(plan.exclusions.iter().any(|e| e.relative_path == ".hidden"
        && e.reason == "hidden-directory"
        && e.descendants_unscanned));
    assert!(
        plan.exclusions
            .iter()
            .any(|e| e.relative_path == "excluded.m4a"
                && e.reason == "explicit-exclusion"
                && !e.descendants_unscanned)
    );
}

#[test]
fn complete_mixed_receipt_preserves_other_codec_and_incomplete_or_wrong_scope_is_rejected() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let fp = fingerprint::read(&fixture()).unwrap();
    let mut value = receipt(&catalog, &fp, true);
    let index = load(&catalog, &value).unwrap();
    assert!(index.tracks().is_empty());
    assert_eq!(index.other_audio().len(), 1);
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &candidates(&source),
    )
    .unwrap();
    assert!(matches!(plan.sources[0].decision, Decision::Add { .. }));
    assert_eq!(plan.preserved_unmatched_device_tracks.len(), 1);
    let wrong = IndexIdentity {
        device_identifier_sha256: [7; 32],
        ..identity()
    };
    assert!(build(&source, &catalog, &index, &wrong, None, &BTreeMap::new()).is_err());
    value["complete"] = json!(false);
    assert!(load(&catalog, &value).is_err());
}

#[test]
fn source_revalidation_rejects_encoded_timing_metadata_and_fullhash_changes() {
    let report = source_validation::inspect(&fixture(), false);
    let mut source = sources();
    let s = &mut source.tracks[0];
    assert!(check_revalidation(s, &report).is_ok());
    s.fingerprint.audio.sha256 = [1; 32];
    assert!(check_revalidation(s, &report).is_err());
    let mut source = sources();
    source.tracks[0].metadata.timing.duration_ticks += 1;
    assert!(check_revalidation(&source.tracks[0], &report).is_err());
    let mut source = sources();
    source.tracks[0].fingerprint.full_file_sha256 = [1; 32];
    assert!(check_revalidation(&source.tracks[0], &report).is_err());
    let mut source = sources();
    source.tracks[0].metadata.title[0] = "changed".into();
    assert!(check_revalidation(&source.tracks[0], &report).is_err());
}

#[test]
fn metadata_difference_and_plausibility_rules_do_not_turn_titles_into_identity() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut track = catalog.tracks()[0].clone();
    let s = ProjectedFields::from_source(&sources().tracks[0].metadata).unwrap();
    track.metadata.title = s.title.to_ascii_uppercase();
    track.metadata.artist = s.artist.clone();
    track.metadata.total_time_ms = s.total_time_ms as u32 + 1000;
    assert!(plausible(&s, &track));
    assert!(!differences(&s, &track).is_empty());
    track.metadata.total_time_ms += 1;
    assert!(!plausible(&s, &track));
    track.metadata.total_time_ms = s.total_time_ms as u32;
    track.metadata.artist = "unrelated".into();
    assert!(!plausible(&s, &track));
    track.metadata.album = s.album.clone();
    assert!(plausible(&s, &track));
    track.metadata.title = s.title.clone();
    track.metadata.artist = s.artist.clone();
    track.metadata.album_artist = s.grouping_artist.clone();
    track.metadata.genre = s.genre.clone();
    track.metadata.composer = s.composer.clone();
    track.metadata.copyright = s.copyright.clone();
    track.metadata.track_number = s.track.unwrap_or(0);
    track.metadata.track_count = s.track_total.unwrap_or(0);
    track.metadata.disc_number = s.disc.unwrap_or(0);
    track.metadata.disc_count = s.disc_total.unwrap_or(0);
    track.metadata.year = s.year.unwrap_or(0);
    assert!(differences(&s, &track).is_empty());
}

#[test]
fn collision_checks_are_casefolded_and_pending_groups_keep_predetermined_ids() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut planner = Planner::new(&catalog, &BTreeSet::new());
    let folders = BTreeSet::from(["iTunes_Control/Music/F39".into()]);
    let mut paths = BTreeSet::from(["itunes_control/music/f39/test.m4a".into()]);
    for invalid in [
        "iTunes_Control/Music/F39/TEST.m4a",
        "iTunes_Control/Music/F40/ABCD.m4a",
        "iTunes_Control/Music/F39/AbCD.m4a",
        "iTunes_Control/Music/F39/ABCDE.m4a",
    ] {
        assert!(candidate_path(invalid, &folders, &paths).is_err());
    }
    let source = sources();
    let first = plan_add(
        &mut planner,
        &source.tracks[0],
        &candidates(&source)[&source.tracks[0].relative_path],
        &folders,
        &mut paths,
    )
    .unwrap();
    let second = Candidate {
        ids: CandidateIds {
            track: 2001,
            album: Some(2002),
            artist: Some(2003),
        },
        path: "iTunes_Control/Music/F39/EFGH.m4a".into(),
    };
    let second = plan_add(
        &mut planner,
        &source.tracks[0],
        &second,
        &folders,
        &mut paths,
    )
    .unwrap();
    let (
        Decision::Add {
            album: a,
            artist: b,
            ..
        },
        Decision::Add {
            album: c,
            artist: d,
            pending_collection_ids,
            ..
        },
    ) = (first, second)
    else {
        panic!()
    };
    assert_eq!(a.id, c.id);
    assert_eq!(b.id, d.id);
    assert_eq!(c.status, "pending_earlier_source");
    assert_eq!(pending_collection_ids.len(), 2);
}

#[test]
fn composer_plan_preserves_independent_sort_and_reports_bound_canonicalization() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::composer_transition_catalog(
        "ACME",
        Some("Independent sort"),
        123,
        false,
    );
    let native = native_composer(&catalog, Some("Acme")).unwrap().unwrap();
    assert_eq!(native.requested_label, "Acme");
    assert_eq!(native.wire_label, "ACME");
    assert_eq!(native.wire_sort, "Independent sort");
    assert_eq!(native.existing_lookup_id, 71);
    let missing_sort = music_protocol::test_support::catalogue::composer_transition_catalog(
        "ACME", None, 123, false,
    );
    assert!(native_composer(&missing_sort, Some("Acme")).is_err());
    assert!(native_composer(&catalog, Some("Other")).unwrap().is_none());

    let fp = fingerprint::read(&fixture()).unwrap();
    let index = load(&catalog, &receipt(&catalog, &fp, false)).unwrap();
    let mut projected = ProjectedFields::from_source(&source.tracks[0].metadata).unwrap();
    projected.composer = Some("Acme".into());
    let journal = JournalView::default();
    let matched = existing_match(
        &source.tracks[0],
        &catalog.tracks()[0],
        &index,
        &journal,
        true,
        Some(&projected),
        &catalog,
    );
    assert!(
        !matched
            .catalog_differences
            .iter()
            .any(|d| d.field == "composer")
    );
    assert_eq!(matched.canonicalized_fields.len(), 1);
    assert_eq!(matched.canonicalized_fields[0].current, json!("ACME"));
    assert_eq!(matched.canonicalized_fields[0].desired, json!("Acme"));

    projected.composer = Some("Other".into());
    let changed = existing_match(
        &source.tracks[0],
        &catalog.tracks()[0],
        &index,
        &journal,
        true,
        Some(&projected),
        &catalog,
    );
    assert!(changed.canonicalized_fields.is_empty());
    assert!(
        changed
            .catalog_differences
            .iter()
            .any(|d| d.field == "composer")
    );
}

#[test]
fn add_only_pending_cover_requires_unchanged_owned_insertion_and_no_cover_or_blocker() {
    let source = fingerprint::read(&fixture()).unwrap();
    let mut binding = Binding {
        receipt_kind: crate::engine::state::journal::BindingReceiptKind::InsertVerification,
        content: Content {
            source: (source.clone()).into(),
            track_pid: 123,
            path: "iTunes_Control/Music/F39/ABCD.m4a".into(),
            artwork: None,
            collection: None,
        },
        receipt_sha256: [1; 32],
        media_anchor: 13,
    };
    assert!(pending_owned_cover(&source, &binding, false, false, false));
    assert!(!pending_owned_cover(&source, &binding, false, false, true));
    assert!(!pending_owned_cover(&source, &binding, true, false, false));
    let mut changed = source.clone();
    changed.full_file_sha256 = [8; 32];
    assert!(changed.audio == source.audio);
    assert!(!pending_owned_cover(
        &changed, &binding, false, false, false
    ));
    binding.content.artwork = Some(crate::engine::state::journal::Artwork {
        cache_id: 678,
        bytes: 10,
        sha256: [2; 32],
    });
    // Lost registration is a repair, not an unfinished first cover.
    assert!(!pending_owned_cover(&source, &binding, false, false, false));
    // A cover abandoned after the phone took its metadata: the track names
    // an ID with no image, while the record keeps the last verified cover.
    assert!(pending_owned_cover(&source, &binding, true, true, false));
    assert!(!pending_owned_cover(&source, &binding, true, true, true));
    assert!(!pending_owned_cover(&changed, &binding, true, true, false));
    assert!(!pending_owned_cover(&source, &binding, true, false, false));
    // As the catalogue registers it: the fixture's track (entity 1) names
    // 723, first with no `artwork` row, then with its image.
    binding.content.track_pid = u64::MAX;
    let token = "INSERT INTO artwork_token VALUES(1,'723',300,1,0,0);";
    let imageless = music_protocol::test_support::catalogue::modified_collection_catalog(13, token);
    assert!(owes_cover(&imageless, u64::MAX, &source, &binding, false));
    assert!(!owes_cover(&imageless, u64::MAX, &changed, &binding, false));
    let image =
        "INSERT INTO artwork VALUES('723',300,6,0,'aa/00000000000000000000000000000000000000');";
    let cached = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        &format!("{image}{token}"),
    );
    assert!(!owes_cover(&cached, u64::MAX, &source, &binding, false));
}

#[test]
fn ownership_requires_exact_verified_binding() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let fp = fingerprint::read(&fixture()).unwrap();
    let index = load(&catalog, &receipt(&catalog, &fp, false)).unwrap();
    let remote = &index.tracks()[&u64::MAX];
    let content = Content {
        source: (remote.fingerprint.clone()).into(),
        track_pid: remote.sync_id,
        path: remote.afc_path.clone(),
        artwork: None,
        collection: None,
    };
    let mut journal = JournalView {
        active_bindings: vec![Binding {
            receipt_kind: crate::engine::state::journal::BindingReceiptKind::InsertVerification,
            content,
            receipt_sha256: [1; 32],
            media_anchor: 13,
        }],
        ..Default::default()
    };
    assert_eq!(
        existing_match(
            &source.tracks[0],
            &catalog.tracks()[0],
            &index,
            &journal,
            true,
            None,
            &catalog
        )
        .ownership,
        "inserted_by_us"
    );
    journal.active_bindings[0].content.source.full_file_sha256 = [7; 32];
    assert_eq!(
        existing_match(
            &source.tracks[0],
            &catalog.tracks()[0],
            &index,
            &journal,
            true,
            None,
            &catalog
        )
        .ownership,
        "unowned"
    );
}

#[test]
fn inspected_audio_above_transfer_cap_is_refused_before_allocating_any_planned_role() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut source = sources();
    let candidate = candidates(&source)
        .remove(&source.tracks[0].relative_path)
        .unwrap();
    let mut planner = Planner::new(&catalog, &BTreeSet::new());
    let folders = BTreeSet::from(["iTunes_Control/Music/F39".into()]);
    let mut paths = BTreeSet::new();
    let original = source.tracks[0].fingerprint.file_bytes;
    source.tracks[0].fingerprint.file_bytes = 64 * 1024 * 1024 + 1;
    assert!(
        plan_add(
            &mut planner,
            &source.tracks[0],
            &candidate,
            &folders,
            &mut paths,
        )
        .is_err()
    );
    assert!(paths.is_empty());
    source.tracks[0].fingerprint.file_bytes = original;
    assert!(matches!(
        plan_add(
            &mut planner,
            &source.tracks[0],
            &candidate,
            &folders,
            &mut paths,
        )
        .unwrap(),
        Decision::Add { .. }
    ));
}

fn journal_scope_for_consumers() -> (Identity, IndexIdentity) {
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4] = 1;
    raw[6] = 25;
    raw[8] = 1;
    raw[12] = 1;
    raw[20] = 2;
    let library = music_protocol::catalogue::identity::parse(&raw).unwrap();
    let scope = Identity::new("planner-consumer-test", &library).unwrap();
    let id = scope.index_identity();
    (scope, id)
}
#[test]
fn planner_ownership_uses_only_active_lifetime_after_retire_and_same_audio_readd() {
    use crate::engine::state::journal::test_support;
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let fp = fingerprint::read(&fixture()).unwrap();
    let (scope, identity) = journal_scope_for_consumers();
    let mut r = receipt(&catalog, &fp, false);
    r["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &r, &identity).unwrap();
    let remote = &index.tracks()[&u64::MAX];
    let current = Content {
        source: (remote.fingerprint.clone()).into(),
        track_pid: remote.sync_id,
        path: remote.afc_path.clone(),
        artwork: None,
        collection: None,
    };
    let mut old = current.clone();
    old.track_pid -= 1;
    old.path = "iTunes_Control/Music/F39/OLDX.m4a".into();
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.0.join("journal"), &scope).unwrap();
    test_support::insert_verified(&mut journal, old.clone());
    test_support::retire(&mut journal, &old);
    let view = JournalView::read(
        Some(JournalEvidence {
            journal: &journal,
            identity: &scope,
        }),
        &identity,
        &index,
    )
    .unwrap();
    assert!(view.active_bindings.is_empty() && view.blockers.is_empty());
    assert!(view.reserved_ids.contains(&old.track_pid));
    assert!(view.reserved_paths.contains(&old.path.to_ascii_lowercase()));
    assert_eq!(
        existing_match(
            &source.tracks[0],
            &catalog.tracks()[0],
            &index,
            &view,
            true,
            None,
            &catalog
        )
        .ownership,
        "unowned"
    );
    test_support::insert_verified(&mut journal, current);
    let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let view = JournalView::read(
        Some(JournalEvidence {
            journal: &journal,
            identity: &scope,
        }),
        &identity,
        &index,
    )
    .unwrap();
    assert_eq!(view.active_bindings.len(), 1);
    assert!(view.blockers.is_empty());
    assert_eq!(
        existing_match(
            &source.tracks[0],
            &catalog.tracks()[0],
            &index,
            &view,
            true,
            None,
            &catalog
        )
        .ownership,
        "inserted_by_us"
    );
    assert_eq!(
        before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}

struct DisclosureFixture {
    temp: Temp,
    snapshot: crate::engine::state::snapshot::Snapshot,
    source: SourceCollection,
}
impl DisclosureFixture {
    fn new(play: u32, skip: u32, cover: bool) -> Self {
        use lofty::{
            config::{ParseOptions, WriteOptions},
            file::AudioFile,
            mp4::Mp4File,
            picture::{MimeType, Picture, PictureType},
        };
        use std::{io::Cursor, os::unix::fs::PermissionsExt};
        let temp = Temp::new();
        let mut file = Cursor::new(fs::read(fixture()).unwrap());
        if cover {
            let mut mp4 = Mp4File::read_from(&mut file, ParseOptions::default()).unwrap();
            mp4.ilst_mut()
                .unwrap()
                .insert_picture(Picture::new_unchecked(
                    PictureType::CoverFront,
                    Some(MimeType::Jpeg),
                    None,
                    vec![0xff, 0xd8, 0xff, 0xd9],
                ));
            mp4.save_to(&mut file, WriteOptions::default()).unwrap();
        }
        let source_path = temp.0.join("covered.m4a");
        fs::write(&source_path, file.into_inner()).unwrap();
        let source = inspect(&select(&source_path, &SelectionOptions::default()).unwrap()).unwrap();
        assert!(source.failures.is_empty());
        let snapshot_path = temp.0.join("snapshot");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&snapshot_path)
            .unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(snapshot_path.join("catalog"))
            .unwrap();
        let db = snapshot_path.join("catalog/MediaLibrary.sqlitedb");
        music_protocol::test_support::catalogue::copy_closed_collection_fixture(&db);
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(&format!(
                "UPDATE item_stats SET play_count_recent={play},skip_count_recent={skip}"
            ))
            .unwrap();
        connection.close().unwrap();
        let catalog = music_protocol::catalogue::sqlite::read_snapshot(&db, None, &temp.0).unwrap();
        let mut prefs = vec![0u8; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4] = 1;
        prefs[6] = 25;
        prefs[8] = 1;
        prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
        prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
        let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
        let phone = different(source.tracks[0].fingerprint.clone());
        let mut index = receipt(&catalog, &phone, false);
        index["identity"]["library_identity_sha256"] = json!(hex(&library.binding_sha256()));
        index["progress_sha256"] = json!(hex(&Sha256::digest(b"progress").into()));
        for name in [
            "before-iTunesPrefs",
            "before-iTunesPrefs.plist",
            "after-iTunesPrefs",
            "after-iTunesPrefs.plist",
        ] {
            fs::write(snapshot_path.join(name), &prefs).unwrap();
        }
        fs::write(
            snapshot_path.join("complete-index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        fs::write(snapshot_path.join("progress.jsonl"), b"progress").unwrap();
        for path in fs::read_dir(&snapshot_path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .chain([db])
        {
            if path.is_file() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        let snapshot = crate::engine::state::snapshot::open(&snapshot_path).unwrap();
        Self {
            temp,
            snapshot,
            source,
        }
    }
    fn plan(&self) -> Plan {
        build_from_snapshot(
            &self.source,
            &self.snapshot,
            None,
            &candidates(&self.source),
            false,
        )
        .unwrap()
    }
}

pub(crate) fn artwork_disclosure_test_plan(play: u32, skip: u32) -> Plan {
    DisclosureFixture::new(play, skip, true).plan()
}

#[test]
fn public_plan_discloses_skip_only_and_mixed_artwork_resets() {
    for play in [0, 4] {
        let f = DisclosureFixture::new(play, 3, true);
        let plan = f.plan();
        assert!(matches!(
            plan.sources[0].artwork,
            ArtworkDecision::SeparateUpdateNeeded { .. }
        ));
        assert!(matches!(plan.sources[0].decision, Decision::Add { .. }));
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["artwork_import_recent_skip_resets"][0]["before"], 3);
        assert_eq!(v["artwork_import_recent_skip_resets"][0]["after"], 0);
        assert_eq!(
            v["artwork_import_recent_skip_resets"][0]["condition"],
            "artwork_import_may_reset_all_positive_recent_play_and_skip_counts"
        );
        if play > 0 {
            let row = &v["artwork_import_recent_play_resets"][0];
            assert_eq!(row["before"], play);
            assert!(
                row.get("lifetime_counts_and_all_other_statistics_must_be_preserved")
                    .is_none()
            );
            assert_eq!(
                row["all_statistics_except_recent_play_and_skip_must_be_preserved"],
                true
            );
        }
    }
}

#[test]
fn saved_artwork_plan_recomputes_and_refuses_stale_skip_disclosure() {
    for play in [0, 4] {
        let f = DisclosureFixture::new(play, 3, true);
        let plan = f.plan();
        let good = serde_json::to_value(&plan).unwrap();
        assert!(good.get("artwork_import_recent_skip_resets").is_some());
        let file = f.temp.0.join("plan.json");
        let snapshot = f.temp.0.join("snapshot");
        let journal = f.temp.0.join("absent-journal");
        fs::write(&file, serde_json::to_vec(&good).unwrap()).unwrap();
        let rebuilt =
            crate::engine::planning::saved_plan::load_read_only(&file, &snapshot, &journal)
                .unwrap();
        assert_eq!(serde_json::to_value(rebuilt.plan).unwrap(), good);
        for variant in 0..3 {
            let mut stale = good.clone();
            match variant {
                0 => {
                    stale
                        .as_object_mut()
                        .unwrap()
                        .remove("artwork_import_recent_skip_resets");
                }
                1 => stale["artwork_import_recent_skip_resets"][0]["before"] = json!(2),
                _ => {
                    stale["artwork_import_recent_skip_resets"][0]["condition"] = json!("skip_only")
                }
            }
            fs::write(&file, serde_json::to_vec(&stale).unwrap()).unwrap();
            let error =
                crate::engine::planning::saved_plan::load_read_only(&file, &snapshot, &journal)
                    .err()
                    .unwrap();
            assert!(
                error.to_string().contains("saved plan differs"),
                "{error:#}"
            );
            assert!(!journal.exists());
        }
    }
}

#[test]
fn public_plan_mixed_disclosure_does_not_claim_recent_skip_is_preserved() {
    let plan = artwork_disclosure_test_plan(4, 3);
    let v = serde_json::to_value(plan).unwrap();
    assert!(
        v["artwork_import_recent_play_resets"][0]
            .get("lifetime_counts_and_all_other_statistics_must_be_preserved")
            .is_none()
    );
}

#[test]
fn artwork_skip_disclosure_omits_no_artwork_and_conflicting_proposals() {
    let f = DisclosureFixture::new(4, 3, false);
    let plan = f.plan();
    let v = serde_json::to_value(plan).unwrap();
    assert!(v.get("artwork_import_recent_play_resets").is_none());
    assert!(v.get("artwork_import_recent_skip_resets").is_none());

    let f = DisclosureFixture::new(4, 3, true);
    let sources =
        inspect(&select(&f.temp.0.join("covered.m4a"), &SelectionOptions::default()).unwrap())
            .unwrap();
    // Source tamper after inspection leaves the source out, never a proposed import.
    fs::write(f.temp.0.join("covered.m4a"), b"invalid source").unwrap();
    let plan =
        build_from_snapshot(&sources, &f.snapshot, None, &candidates(&sources), false).unwrap();
    assert!(matches!(
        plan.sources[0].decision,
        Decision::Skip {
            code: "source_changed_or_invalid",
            ..
        }
    ));
    let v = serde_json::to_value(plan).unwrap();
    assert!(v.get("artwork_import_recent_play_resets").is_none());
    assert!(v.get("artwork_import_recent_skip_resets").is_none());
}

#[test]
fn no_skip_artwork_plan_has_play_only_disclosure_and_saved_roundtrip() {
    let f = DisclosureFixture::new(4, 0, true);
    let plan = f.plan();
    let good = serde_json::to_value(&plan).unwrap();
    assert!(good.get("artwork_import_recent_skip_resets").is_none());
    let expected=br#"[{"after":0,"before":4,"condition":"artwork_import_may_reset_all_positive_recent_play_counts","fresh_counts_rechecked_before_each_import":true,"lifetime_counts_and_all_other_statistics_must_be_preserved":true,"track_id":"ffffffffffffffff"}]"#;
    assert_eq!(
        serde_json::to_vec(&plan.artwork_import_recent_play_resets).unwrap(),
        expected
    );
    let path = f.temp.0.join("plan.json");
    let bytes = serde_json::to_vec_pretty(&plan).unwrap();
    fs::write(&path, &bytes).unwrap();
    let rebuilt = crate::engine::planning::saved_plan::load_read_only(
        &path,
        &f.temp.0.join("snapshot"),
        &f.temp.0.join("absent-journal"),
    )
    .unwrap();
    assert_eq!(serde_json::to_vec_pretty(&rebuilt.plan).unwrap(), bytes);
}

pub(super) fn mp3_fixture_bytes() -> Vec<u8> {
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    [frame.clone(), frame].concat()
}
#[test]
fn mp3_exact_match_is_unowned_keep_and_new_or_changed_bytes_never_allocate() {
    let temp = Temp::new();
    let path = temp.0.join("source.mp3");
    let bytes = mp3_fixture_bytes();
    fs::write(&path, &bytes).unwrap();
    let source = inspect(&select(&path, &SelectionOptions::default()).unwrap()).unwrap();
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item_extra SET file_size=834;",
    );
    let alac = fingerprint::read(&fixture()).unwrap();
    let mut value = receipt(&catalog, &alac, true);
    value["other_records"][0]["full_file_sha256"] =
        json!(source.mp3_tracks[0].source.fingerprint.full_file_sha256);
    value["other_records"][0]["codec"]["frame_count"] = json!(2);
    let index = load(&catalog, &value).unwrap();
    for preserve in [true, false] {
        let plan = build_inner(
            &source,
            &catalog,
            &index,
            &identity(),
            None,
            &BTreeMap::new(),
            preserve,
        )
        .unwrap();
        assert!(!plan.has_conflicts());
        assert_eq!(plan.sources.len(), 1);
        assert!(matches!(plan.sources[0].decision, Decision::Keep));
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["sources"][0]["matches"][0]["ownership"], "unowned");
        assert!(v["sources"][0].get("track_id").is_none());
        assert!(v["sources"][0]["projected"].is_null());
        assert!(
            !plan
                .preserved_unmatched_device_tracks
                .contains(&"ffffffffffffffff".into())
        );
    }
    assert!(
        crate::engine::planning::candidates::generate(&source, &catalog, None)
            .unwrap()
            .is_empty()
    );
    let mut forged = value.clone();
    forged["other_records"][0]["codec"]["frame_count"] = json!(3);
    let forged = load(&catalog, &forged).unwrap();
    let refused = build(
        &source,
        &catalog,
        &forged,
        &identity(),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(
        matches!(&refused.sources[0].decision,Decision::Conflict{code,..} if *code=="device_mpeg_classification_conflict")
    );
    // Tag-only drift has real equal source frame bytes, but this device index
    // cannot prove frame identity independently of its now-different full file.
    fs::write(
        &path,
        [b"ID3\x04\0\0\0\0\0\0".as_slice(), bytes.as_slice()].concat(),
    )
    .unwrap();
    let changed = inspect(&select(&path, &SelectionOptions::default()).unwrap()).unwrap();
    assert_eq!(
        source.mp3_tracks[0].source.fingerprint.audio,
        changed.mp3_tracks[0].source.fingerprint.audio
    );
    let plan = build(
        &changed,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(
        matches!(&plan.sources[0].decision,Decision::Skip{code,..}if *code=="mp3_source_profile_unsupported")
    );
    let compatibility = plan.sources[0].mp3_insertion.as_ref().unwrap();
    assert!(!compatibility.execution_authorized);
    assert!(!compatibility.required_qualification.is_empty());
    assert!(
        compatibility
            .compatibility_issues
            .iter()
            .any(|i| i.code == "mp3_metadata_policy_required")
    );
    let json = serde_json::to_value(&plan).unwrap();
    assert!(json["sources"][0].get("track_id").is_none());
    assert!(json["sources"][0].get("path").is_none());
    let stale = build(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    assert!(
        matches!(&stale.sources[0].decision,Decision::Skip{code,..}if *code=="source_changed_or_invalid")
    );
    // Complete source accounting includes MPEG.
    let scope = crate::engine::planning::removal::collection_scope_sha256(&changed).unwrap();
    assert_ne!(scope, [0; 32]);
}

#[test]
fn unowned_metadata_drift_reports_raw_sort_rating_compilation_and_refuses_ownership() {
    let source = sources();
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item_extra SET sort_title='Actual independent sort',content_rating=4,content_rating_level=1; UPDATE item SET is_compilation=1;",
    );
    let fp = fingerprint::read(&fixture()).unwrap();
    let index = load(&catalog, &receipt(&catalog, &fp, false)).unwrap();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    let Decision::Skip { code, message, .. } = &plan.sources[0].decision else {
        panic!("unowned matching audio cannot receive a managed Update action");
    };
    assert_eq!(*code, "existing_metadata_update_unqualified");
    assert!(message.contains("admitted inserted ownership"));
    let diffs = &plan.sources[0].matches[0].catalog_differences;
    for field in [
        "sort_name",
        "content_rating",
        "content_rating_level",
        "compilation",
    ] {
        assert!(diffs.iter().any(|d| d.field == field), "{field}");
    }
    assert_eq!(plan.decision_counts().updates, 0);
    assert_eq!(plan.decision_counts().skipped, 1);
    assert!(!plan.has_conflicts());
    assert!(!plan.execution_authorized_by_plan && plan.phone_deletions == 0);
    let preserved = build_with_policy(
        &source,
        &catalog,
        &index,
        &identity(),
        None,
        &BTreeMap::new(),
        true,
    )
    .unwrap();
    assert!(matches!(preserved.sources[0].decision, Decision::Keep));
}

/// A whole-library replacement checks the sources no plan could add before it
/// removes anything (seen on the phone: an MP3 no plan could add was left off
/// the phone after the removal).
#[test]
fn uninsertable_sources_are_the_source_side_refusals() {
    let temp = Temp::new();
    let alac = fs::read(fixture()).unwrap();
    fs::write(temp.0.join("good.m4a"), &alac).unwrap();
    fs::write(temp.0.join("broken.m4a"), b"not an m4a").unwrap();
    fs::write(
        temp.0.join("tagged.mp3"),
        [
            b"ID3\x04\0\0\0\0\0\0".as_slice(),
            mp3_fixture_bytes().as_slice(),
        ]
        .concat(),
    )
    .unwrap();
    let sources = inspect(&select(&temp.0, &SelectionOptions::default()).unwrap()).unwrap();
    let refused: Vec<_> = uninsertable_sources(&sources)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(refused, ["broken.m4a", "tagged.mp3"]);
    // Two sources with the same audio are both refused.
    fs::write(temp.0.join("copy.m4a"), &alac).unwrap();
    let sources = inspect(&select(&temp.0, &SelectionOptions::default()).unwrap()).unwrap();
    let refused: Vec<_> = uninsertable_sources(&sources)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        refused,
        ["broken.m4a", "copy.m4a", "good.m4a", "tagged.mp3"]
    );
}

#[test]
fn owned_track_with_changed_tags_plans_an_update() {
    use crate::engine::state::journal::test_support;
    let source = sources();
    // The source names no composer or year (a cleared composer is not an
    // edit); its duration is the fixture's.
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item SET composer_pid=0;UPDATE item_extra SET year=0,total_time_ms=50;",
    );
    let fp = fingerprint::read(&fixture()).unwrap();
    let (scope, identity) = journal_scope_for_consumers();
    let mut r = receipt(&catalog, &fp, false);
    r["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &r, &identity).unwrap();
    let remote = &index.tracks()[&u64::MAX];
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.0.join("journal"), &scope).unwrap();
    test_support::insert_verified(
        &mut journal,
        Content {
            source: (remote.fingerprint.clone()).into(),
            track_pid: remote.sync_id,
            path: remote.afc_path.clone(),
            artwork: None,
            collection: None,
        },
    );
    let evidence = JournalEvidence {
        journal: &journal,
        identity: &scope,
    };
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity,
        Some(evidence),
        &BTreeMap::new(),
    )
    .unwrap();
    let Decision::Update { track_id, changes } = &plan.sources[0].decision else {
        panic!(
            "{} {}",
            serde_json::to_string(&plan.sources[0].decision).unwrap(),
            serde_json::to_string(&plan.sources[0].matches[0].catalog_differences).unwrap()
        );
    };
    assert_eq!(track_id, "ffffffffffffffff");
    assert_eq!(
        changes.iter().map(|c| c.field).collect::<Vec<_>>(),
        plan.sources[0].matches[0]
            .catalog_differences
            .iter()
            .map(|c| c.field)
            .collect::<Vec<_>>()
    );
    assert!(changes.iter().any(|c| c.field == "title"));
    let counts = plan.decision_counts();
    assert_eq!((counts.updates, counts.conflicts), (1, 0));
    assert!(!plan.has_conflicts() && plan.phone_deletions == 0);
    let saved = serde_json::to_value(&plan).unwrap();
    assert_eq!(saved["sources"][0]["decision"], "update");
    // Add-only preserves the track's fields.
    let preserved = build_with_policy(
        &source,
        &catalog,
        &index,
        &identity,
        Some(evidence),
        &BTreeMap::new(),
        true,
    )
    .unwrap();
    assert!(matches!(preserved.sources[0].decision, Decision::Keep));
    // A year the source no longer names is cleared by the update.
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item SET composer_pid=0;UPDATE item_extra SET total_time_ms=50;",
    );
    let mut r = receipt(&catalog, &fp, false);
    r["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &r, &identity).unwrap();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity,
        Some(evidence),
        &BTreeMap::new(),
    )
    .unwrap();
    let Decision::Update { changes, .. } = &plan.sources[0].decision else {
        panic!(
            "{}",
            serde_json::to_string(&plan.sources[0].decision).unwrap()
        );
    };
    let year = changes.iter().find(|c| c.field == "year").unwrap();
    assert_eq!(year.desired, json!(0));
    // A composer the source no longer names is offered to --replace-changed.
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item_extra SET year=0,total_time_ms=50;",
    );
    let mut r = receipt(&catalog, &fp, false);
    r["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &r, &identity).unwrap();
    let plan = build(
        &source,
        &catalog,
        &index,
        &identity,
        Some(evidence),
        &BTreeMap::new(),
    )
    .unwrap();
    let Decision::Skip { code, message, .. } = &plan.sources[0].decision else {
        panic!(
            "{}",
            serde_json::to_string(&plan.sources[0].decision).unwrap()
        );
    };
    assert_eq!(*code, "metadata_update_needs_replacement");
    assert!(message.contains("--replace-changed"), "{message}");
    assert_eq!(plan.sources[0].matches[0].ownership, "inserted_by_us");
}

#[test]
fn an_update_request_carries_only_differing_fields_and_clears_what_an_edit_can() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut track = catalog.tracks()[0].clone();
    let mut s = ProjectedFields::from_source(&sources().tracks[0].metadata).unwrap();
    s.genre = Some("Rock".into());
    s.year = Some(2001);
    s.track = Some(3);
    let m = &mut track.metadata;
    m.title = s.title.clone();
    m.artist = s.artist.clone();
    m.album = s.album.clone();
    m.album_artist = s.grouping_artist.clone();
    m.genre = s.genre.clone();
    m.composer = s.composer.clone();
    m.copyright = s.copyright.clone();
    m.year = 2001;
    m.content_rating = u32::from(s.content_rating.unwrap_or(0));
    m.content_rating_level = 0;
    m.is_compilation = s.compilation.unwrap_or(false);
    m.track_number = 3;
    m.track_count = s.track_total.unwrap_or(0);
    m.disc_number = s.disc.unwrap_or(1);
    m.disc_count = s.disc_total.unwrap_or(0);
    m.total_time_ms = s.total_time_ms as u32;
    let request = |s: &ProjectedFields, t: &music_protocol::catalogue::sqlite::Track| {
        update::from_differences(s, t, &differences(s, t))
    };
    assert!(request(&s, &track).unwrap().is_none());
    // The phone's spelling of a shared group name is not a change.
    track.metadata.artist = s.artist.to_ascii_uppercase();
    assert!(request(&s, &track).unwrap().is_none());
    let mut changed = s.clone();
    changed.title = "Renamed".into();
    changed.sort_name = "Renamed".into();
    changed.year = Some(2005);
    changed.genre = Some("Pop".into());
    let edit = request(&changed, &track).unwrap().unwrap();
    assert_eq!(edit.track, track.sync_id);
    assert_eq!(edit.title.as_deref(), Some("Renamed"));
    assert_eq!(edit.sort_title.as_deref(), Some("Renamed"));
    assert_eq!(
        (edit.year, edit.genre.as_deref()),
        (Some(2005), Some("Pop"))
    );
    assert!(edit.artist.is_none() && edit.album.is_none() && edit.track_number.is_none());
    assert_eq!(update::fields(&edit), ["title", "genre", "year"]);
    // Changing the duration is not a metadata edit, and a cleared genre or
    // composer is left to a replacement.
    let mut cleared = s.clone();
    cleared.genre = None;
    let error = request(&cleared, &track).unwrap_err();
    assert!(
        error.downcast_ref::<update::NeedsReplacement>().is_some(),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("no genre"), "{error:#}");
    let mut cleared = s.clone();
    cleared.composer = None;
    track.metadata.composer = Some("Someone".into());
    assert!(
        request(&cleared, &track)
            .unwrap_err()
            .downcast_ref::<update::NeedsReplacement>()
            .is_some()
    );
    // A difference no replacement makes refuses first.
    cleared.total_time_ms += 5000;
    let error = request(&cleared, &track).unwrap_err();
    assert!(error.downcast_ref::<update::NeedsReplacement>().is_none());
    assert!(format!("{error:#}").contains("duration"), "{error:#}");
    track.metadata.composer = s.composer.clone();
    // A cleared copyright, year or track number is an edit that sends "" or 0.
    let mut cleared = s.clone();
    cleared.copyright = None;
    cleared.year = None;
    cleared.track = None;
    track.metadata.copyright = Some("(C) Someone".into());
    let edit = request(&cleared, &track).unwrap().unwrap();
    assert_eq!(update::fields(&edit), ["copyright", "year", "track"]);
    assert_eq!(
        (edit.copyright.as_deref(), edit.year, edit.track_number),
        (Some(""), Some(0), Some(0))
    );
    track.metadata.copyright = s.copyright.clone();
    let mut longer = s.clone();
    longer.total_time_ms += 5000;
    let error = format!("{:#}", request(&longer, &track).unwrap_err());
    assert!(error.contains("duration"), "{error}");
    // A comment is sent as it is, and one the source lost as "", which clears
    // it.
    let mut commented = s.clone();
    commented.comment = Some("Line one\nLine two".into());
    let difference = |current: Value, desired: Value| {
        [FieldDifference {
            field: "comment",
            current,
            desired,
        }]
    };
    let edit = update::from_differences(
        &commented,
        &track,
        &difference(json!(null), json!("Line one\nLine two")),
    )
    .unwrap()
    .unwrap();
    assert_eq!(update::fields(&edit), ["comment"]);
    assert_eq!(edit.comment.as_deref(), Some("Line one\nLine two"));
    let edit = update::from_differences(&s, &track, &difference(json!("Old"), json!(null)))
        .unwrap()
        .unwrap();
    assert_eq!(edit.comment.as_deref(), Some(""));
}

#[test]
fn a_stored_empty_comment_is_no_comment() {
    let comment = |stored: &str, desired: Option<&str>| {
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            12,
            &format!("UPDATE item_extra SET comment={stored} WHERE item_pid=1;"),
        );
        let track = &catalog.tracks()[0];
        let mut s = ProjectedFields::from_source(&sources().tracks[0].metadata).unwrap();
        s.comment = desired.map(str::to_owned);
        source_metadata::additional_differences(&s, &catalog, track)
            .into_iter()
            .filter(|d| d.field == "comment")
            .map(|d| (d.current, d.desired))
            .collect::<Vec<_>>()
    };
    assert!(comment("NULL", None).is_empty());
    assert!(comment("''", None).is_empty());
    assert!(comment("'Kept'", Some("Kept")).is_empty());
    assert_eq!(comment("''", Some("New")), [(json!(null), json!("New"))]);
    assert_eq!(comment("'Old'", None), [(json!("Old"), json!(null))]);
}

#[test]
fn an_emptied_alac_composer_plans_as_a_removed_one() {
    use crate::engine::state::journal::test_support;
    use lofty::{
        config::{ParseOptions, WriteOptions},
        file::AudioFile,
        mp4::{Atom, AtomData, AtomIdent, Mp4File},
    };
    // The fixture, which names no composer, with its composer atom holding
    // `composer`.
    let with_composer = |composer: &str| {
        let mut file = std::io::Cursor::new(fs::read(fixture()).unwrap());
        let mut mp4 = Mp4File::read_from(&mut file, ParseOptions::default()).unwrap();
        mp4.ilst_mut().unwrap().replace_atom(Atom::new(
            AtomIdent::Fourcc(*b"\xa9wrt"),
            AtomData::UTF8(composer.into()),
        ));
        mp4.save_to(&mut file, WriteOptions::default()).unwrap();
        file.into_inner()
    };
    // `bytes` as the one source of a folder of its own.
    let collected = |temp: &Temp, bytes: &[u8]| {
        let path = temp.0.join("track.m4a");
        fs::write(&path, bytes).unwrap();
        inspect(&select(&path, &SelectionOptions::default()).unwrap()).unwrap()
    };
    let (emptied_folder, removed_folder) = (Temp::new(), Temp::new());
    // A bare terminator reads as an empty composer.
    let emptied = collected(&emptied_folder, &with_composer("\0"));
    assert_eq!(emptied.tracks[0].metadata.composer, [""]);
    let removed = collected(&removed_folder, &fs::read(fixture()).unwrap());
    assert!(removed.tracks[0].metadata.composer.is_empty());
    // A plan's rows, less the file identity the two sources differ by.
    let rows = |plan: &Plan| {
        let mut rows = serde_json::to_value(&plan.sources).unwrap();
        for row in rows.as_array_mut().unwrap() {
            let row = row.as_object_mut().unwrap();
            row.remove("source_file_bytes");
            row.remove("source_file_sha256");
        }
        rows
    };
    // An addition: the phone holds other audio.
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let other = different(fingerprint::read(&fixture()).unwrap());
    let index = load(&catalog, &receipt(&catalog, &other, false)).unwrap();
    let add = |sources: &SourceCollection| {
        build(
            sources,
            &catalog,
            &index,
            &identity(),
            None,
            &candidates(sources),
        )
        .unwrap()
    };
    let added = add(&emptied);
    assert!(
        matches!(added.sources[0].decision, Decision::Add { .. }),
        "{}",
        serde_json::to_string(&added.sources[0].decision).unwrap()
    );
    assert_eq!(added.sources[0].projected.as_ref().unwrap().composer, None);
    assert_eq!(rows(&added), rows(&add(&removed)));
    // An update of the track this tool inserted when the file named the
    // phone's composer, which neither source names now.
    let phone_folder = Temp::new();
    let phone_path = phone_folder.0.join("track.m4a");
    fs::write(&phone_path, with_composer("Synthetic Composer")).unwrap();
    let fp = fingerprint::read(&phone_path).unwrap();
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item_extra SET year=0,total_time_ms=50;",
    );
    assert_eq!(
        catalog.tracks()[0].metadata.composer.as_deref(),
        Some("Synthetic Composer")
    );
    let (scope, identity) = journal_scope_for_consumers();
    let mut r = receipt(&catalog, &fp, false);
    r["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
    let index = load_for(&catalog, &r, &identity).unwrap();
    let remote = &index.tracks()[&u64::MAX];
    let mut journal = Journal::create(&phone_folder.0.join("journal"), &scope).unwrap();
    test_support::insert_verified(
        &mut journal,
        Content {
            source: (remote.fingerprint.clone()).into(),
            track_pid: remote.sync_id,
            path: remote.afc_path.clone(),
            artwork: None,
            collection: None,
        },
    );
    let evidence = JournalEvidence {
        journal: &journal,
        identity: &scope,
    };
    let update = |sources: &SourceCollection| {
        build(
            sources,
            &catalog,
            &index,
            &identity,
            Some(evidence),
            &BTreeMap::new(),
        )
        .unwrap()
    };
    let updated = update(&emptied);
    let Decision::Skip { code, .. } = &updated.sources[0].decision else {
        panic!(
            "{}",
            serde_json::to_string(&updated.sources[0].decision).unwrap()
        );
    };
    assert_eq!(*code, "metadata_update_needs_replacement");
    assert_eq!(rows(&updated), rows(&update(&removed)));
}

/// An ID3v2.4 tag of UTF-8 text frames before `audio`.
pub(super) fn tagged_mp3(frames: &[(&str, &str)], audio: &[u8]) -> Vec<u8> {
    let safe = |n: usize| -> [u8; 4] {
        [
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ]
    };
    let mut body = Vec::new();
    for (name, text) in frames {
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(&safe(text.len() + 1));
        body.extend_from_slice(&[0, 0, 3]);
        body.extend_from_slice(text.as_bytes());
    }
    [b"ID3\x04\0\0".as_slice(), &safe(body.len()), &body, audio].concat()
}

/// One MP3 this tool inserted whose catalogue row shows
/// exactly the tags of `inserted`.
struct OwnedMpeg {
    temp: Temp,
    catalog: Catalog,
    identity: IndexIdentity,
    scope: Identity,
    index: DeviceAudioIndex,
    journal: crate::engine::state::journal::Journal,
    inserted: Vec<u8>,
}

const OWNED_TAGS: [(&str, &str); 3] = [
    ("TIT2", "Synthetic Title"),
    ("TPE1", "Synthetic Artist"),
    ("TALB", "Synthetic Album"),
];

impl OwnedMpeg {
    fn new() -> Self {
        let temp = Temp::new();
        let root = temp.0.join("source");
        fs::create_dir(&root).unwrap();
        let inserted = tagged_mp3(&OWNED_TAGS, &mp3_fixture_bytes());
        fs::write(root.join("owned.mp3"), &inserted).unwrap();
        let sources = inspect(&select(&root, &SelectionOptions::default()).unwrap()).unwrap();
        let source = sources.mp3_tracks[0].source.transferred_identity().unwrap();
        // The fields an MP3 with only these three tags is inserted with.
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            13,
            &format!(
                "UPDATE item SET genre_id=0,composer_pid=0,track_number=0;
                 UPDATE item_extra SET location='TEST.mp3',file_size={},total_time_ms=52,track_count=0,year=0;",
                source.file_bytes
            ),
        );
        let (scope, identity) = journal_scope_for_consumers();
        let alac = fingerprint::read(&fixture()).unwrap();
        let mut value = receipt(&catalog, &alac, true);
        value["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
        let m = source.audio.mpeg().unwrap();
        value["other_records"][0]["full_file_sha256"] = json!(hex(&source.full_file_sha256));
        value["other_records"][0]["codec"] = json!({"kind":"mpeg1-layer3","sample_rate":m.sample_rate,"channels":m.channels,"frame_count":m.frame_count,"audio_offset":m.audio_offset,"audio_bytes":m.audio_bytes,"trailing_tag_bytes":m.trailing_bytes});
        let index = load_for(&catalog, &value, &identity).unwrap();
        let path = temp.0.join("journal");
        drop(crate::engine::state::journal::Journal::create(&path, &scope).unwrap());
        let mut journal = crate::engine::state::journal::Journal::open(&path, &scope).unwrap();
        crate::engine::state::journal::test_support::mpeg_verified_readback_fixture(
            &mut journal,
            Content {
                source,
                track_pid: catalog.tracks()[0].sync_id,
                path: catalog.tracks()[0].afc_path.clone().unwrap(),
                artwork: None,
                collection: Some(
                    crate::engine::planning::removal::collection_scope_sha256(&sources).unwrap(),
                ),
            },
            12,
        );
        Self {
            temp,
            catalog,
            identity,
            scope,
            index,
            journal,
            inserted,
        }
    }
    /// The plan for the source file `bytes`, with or without the journal.
    fn plan(&self, bytes: &[u8], journal: bool, add_only: bool) -> Plan {
        let root = self.temp.0.join("source");
        fs::write(root.join("owned.mp3"), bytes).unwrap();
        let sources = inspect(&select(&root, &SelectionOptions::default()).unwrap()).unwrap();
        build_with_policy(
            &sources,
            &self.catalog,
            &self.index,
            &self.identity,
            journal.then_some(JournalEvidence {
                journal: &self.journal,
                identity: &self.scope,
            }),
            &BTreeMap::new(),
            add_only,
        )
        .unwrap()
    }
}

#[test]
fn owned_mp3_with_changed_tags_plans_an_update_of_its_fields_only() {
    let f = OwnedMpeg::new();
    let audio = mp3_fixture_bytes();
    // The file that was sent is kept exactly.
    let exact = f.plan(&f.inserted, true, false);
    assert!(matches!(exact.sources[0].decision, Decision::Keep));
    assert!(exact.sources[0].projected.is_none());
    // The same tags in another tag layout: the frames are the ones sent and
    // the catalogue already shows the tags.
    let relaid = tagged_mp3(
        &[
            OWNED_TAGS[2],
            OWNED_TAGS[0],
            OWNED_TAGS[1],
            ("TSSE", "Re-encoder"),
        ],
        &audio,
    );
    let plan = f.plan(&relaid, true, false);
    let row = &plan.sources[0];
    assert!(
        matches!(row.decision, Decision::Keep),
        "{}",
        serde_json::to_string(&row.decision).unwrap()
    );
    assert_eq!(row.matches.len(), 1);
    assert_eq!(
        row.matches[0].match_basis,
        Some("owned_mpeg_frame_identity")
    );
    assert!(row.matches[0].encoded_audio_equal && !row.matches[0].full_container_equal);
    assert!(row.matches[0].catalog_differences.is_empty());
    // A new title and a genre: an Update of those fields, bound to the file
    // that was sent.
    let retagged = tagged_mp3(
        &[
            ("TIT2", "Retitled"),
            OWNED_TAGS[1],
            OWNED_TAGS[2],
            ("TCON", "Synthetic Genre"),
        ],
        &audio,
    );
    let plan = f.plan(&retagged, true, false);
    let row = &plan.sources[0];
    let Decision::Update { track_id, changes } = &row.decision else {
        panic!("{}", serde_json::to_string(&row.decision).unwrap());
    };
    assert_eq!(track_id, "ffffffffffffffff");
    assert_eq!(
        changes.iter().map(|c| c.field).collect::<Vec<_>>(),
        ["title", "genre", "sort_name"]
    );
    assert_eq!(
        row.matches[0].device_file_sha256.as_deref(),
        Some(hex(&Sha256::digest(&f.inserted).into()).as_str())
    );
    assert_eq!(row.projected.as_ref().unwrap().title, "Retitled");
    let counts = plan.decision_counts();
    assert_eq!((counts.updates, counts.conflicts), (1, 0));
    let updates = crate::engine::execution::metadata_batch::source_updates(&plan).unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].track, u64::MAX);
    assert!(matches!(
        updates[0].audio,
        crate::engine::execution::metadata_batch::UpdatedAudio::Mpeg(file)
            if file == <[u8; 32]>::from(Sha256::digest(&f.inserted))
    ));
    // The source must still be the one planned.
    fs::write(f.temp.0.join("source/owned.mp3"), &relaid).unwrap();
    let error = crate::engine::execution::metadata_batch::source_updates(&plan)
        .err()
        .unwrap();
    assert!(
        format!("{error:#}").contains("changed after planning"),
        "{error:#}"
    );
    // Add-only keeps the track's fields.
    let kept = f.plan(&retagged, true, true);
    assert!(matches!(kept.sources[0].decision, Decision::Keep));
    // Without the journal the frames prove nothing, and nothing is added.
    let unowned = f.plan(&retagged, false, false);
    assert!(
        matches!(&unowned.sources[0].decision, Decision::Conflict { .. }),
        "{}",
        serde_json::to_string(&unowned.sources[0].decision).unwrap()
    );
    // Tags outside the insert-batch profile are not sent.
    let numeric = tagged_mp3(
        &[
            OWNED_TAGS[0],
            OWNED_TAGS[1],
            OWNED_TAGS[2],
            ("TCON", "(17)"),
        ],
        &audio,
    );
    let refused = f.plan(&numeric, true, false);
    assert!(
        matches!(&refused.sources[0].decision, Decision::Skip { code, .. } if *code == "metadata_update_unsupported"),
        "{}",
        serde_json::to_string(&refused.sources[0].decision).unwrap()
    );
}

/// A FLAC tagged with exactly `tags`: the fixture tone's STREAMINFO and
/// frames with its Vorbis comment replaced (and its padding dropped), so the
/// encoded audio stays the same.
pub(super) fn tagged_flac(tags: &[(&str, &str)]) -> Vec<u8> {
    crate::test_fixtures::retagged_flac(
        &fs::read(crate::test_fixtures::workspace_root().join("tests/fixtures/tone-flac.flac"))
            .unwrap(),
        tags,
    )
}

pub(super) const FLAC_TAGS: [(&str, &str); 3] = [
    ("TITLE", "Synthetic Title"),
    ("ARTIST", "Synthetic Artist"),
    ("ALBUM", "Synthetic Album"),
];

/// A FLAC this tool sent as an ALAC converted from it: the ALAC's catalogue
/// row shows exactly the FLAC's tags, and the journal links the FLAC's audio
/// to that track.
struct ConvertedFlac {
    temp: Temp,
    catalog: Catalog,
    identity: IndexIdentity,
    scope: Identity,
    index: DeviceAudioIndex,
    journal: Journal,
    flac: Vec<u8>,
}

impl ConvertedFlac {
    fn new() -> Self {
        let temp = Temp::new();
        let root = temp.0.join("source");
        fs::create_dir(&root).unwrap();
        let flac = tagged_flac(&FLAC_TAGS);
        let path = root.join("song.flac");
        fs::write(&path, &flac).unwrap();
        let sources = inspect(&select(&root, &SelectionOptions::default()).unwrap()).unwrap();
        let prepared = PreparedSource::read_expected(&path, &sources.mp3_tracks[0].source).unwrap();
        // The fields a converted ALAC with only these three tags is
        // inserted with.
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            13,
            &format!(
                "UPDATE item SET genre_id=0,composer_pid=0,track_number=0;
                 UPDATE item_extra SET total_time_ms={},track_count=0,year=0;",
                prepared.projected().unwrap().total_time_ms
            ),
        );
        let (scope, identity) = journal_scope_for_consumers();
        let alac = fingerprint::read(&fixture()).unwrap();
        let mut r = receipt(&catalog, &alac, false);
        r["identity"] = json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)});
        let index = load_for(&catalog, &r, &identity).unwrap();
        let remote = &index.tracks()[&u64::MAX];
        let mut journal = Journal::create(&temp.0.join("journal"), &scope).unwrap();
        let flac_identity = prepared.identity();
        crate::engine::state::journal::test_support::converted_insert_verified(
            &mut journal,
            Content {
                source: remote.fingerprint.clone().into(),
                track_pid: remote.sync_id,
                path: remote.afc_path.clone(),
                artwork: None,
                collection: Some(
                    crate::engine::planning::removal::collection_scope_sha256(&sources).unwrap(),
                ),
            },
            crate::engine::state::journal::Conversion {
                source_path: path,
                source_bytes: flac_identity.file_bytes,
                source_sha256: flac_identity.full_file_sha256,
                source_audio: flac_identity.audio.sha256,
                ffmpeg: "ffmpeg version 8.1.3".into(),
            },
            13,
        );
        Self {
            temp,
            catalog,
            identity,
            scope,
            index,
            journal,
            flac,
        }
    }
    /// The source directory holding only `bytes` as `song.flac`.
    fn sources(&self, bytes: &[u8], flac_to_alac: bool) -> SourceCollection {
        let root = self.temp.0.join("source");
        fs::write(root.join("song.flac"), bytes).unwrap();
        let mut sources = inspect(&select(&root, &SelectionOptions::default()).unwrap()).unwrap();
        sources.flac_to_alac = flac_to_alac;
        sources
    }
    fn plan_with(
        &self,
        bytes: &[u8],
        flac_to_alac: bool,
        journal: &Journal,
        candidates: &BTreeMap<String, Candidate>,
    ) -> Plan {
        build(
            &self.sources(bytes, flac_to_alac),
            &self.catalog,
            &self.index,
            &self.identity,
            Some(JournalEvidence {
                journal,
                identity: &self.scope,
            }),
            candidates,
        )
        .unwrap()
    }
    fn plan(&self, bytes: &[u8], flac_to_alac: bool) -> Plan {
        self.plan_with(bytes, flac_to_alac, &self.journal, &BTreeMap::new())
    }
}

#[test]
fn a_flac_sent_as_alac_stays_its_track_with_or_without_the_flag() {
    let f = ConvertedFlac::new();
    let sent = hex(&f.index.tracks()[&u64::MAX].fingerprint.full_file_sha256);
    for flag in [false, true] {
        let plan = f.plan(&f.flac, flag);
        let row = &plan.sources[0];
        assert!(
            matches!(row.decision, Decision::Keep),
            "{}",
            serde_json::to_string(&row.decision).unwrap()
        );
        assert!(!plan.has_conflicts() && !row.flac_to_alac);
        assert_eq!(plan.flac_to_alac, flag);
        assert_eq!(
            serde_json::to_value(&plan).unwrap().get("flac_to_alac"),
            flag.then_some(&json!(true))
        );
        let [found] = &row.matches[..] else {
            panic!("one match");
        };
        assert_eq!(found.match_basis, Some("flac_conversion"));
        assert_eq!((found.codec, found.ownership), ("alac", "inserted_by_us"));
        assert!(!found.encoded_audio_equal && !found.full_container_equal);
        assert!(found.catalog_differences.is_empty());
        assert_eq!(found.device_file_sha256.as_deref(), Some(sent.as_str()));
        assert!(plan.preserved_unmatched_device_tracks.is_empty());
    }
    // New tags: an Update of the ALAC track, bound to the file that was sent.
    let retitled = tagged_flac(&[("TITLE", "Retitled"), FLAC_TAGS[1], FLAC_TAGS[2]]);
    let plan = f.plan(&retitled, false);
    let Decision::Update { track_id, changes } = &plan.sources[0].decision else {
        panic!(
            "{}",
            serde_json::to_string(&plan.sources[0].decision).unwrap()
        );
    };
    assert_eq!(track_id, "ffffffffffffffff");
    assert_eq!(
        changes.iter().map(|c| c.field).collect::<Vec<_>>(),
        ["title", "sort_name"]
    );
    let updates = crate::engine::execution::metadata_batch::source_updates(&plan).unwrap();
    assert!(matches!(
        updates[..],
        [crate::engine::execution::metadata_batch::SourceUpdate {
            track: u64::MAX,
            audio: crate::engine::execution::metadata_batch::UpdatedAudio::Converted(file),
            ..
        }] if hex(&file) == sent
    ));
    // Add-only keeps the track's fields.
    let kept = build_with_policy(
        &f.sources(&retitled, true),
        &f.catalog,
        &f.index,
        &f.identity,
        Some(JournalEvidence {
            journal: &f.journal,
            identity: &f.scope,
        }),
        &BTreeMap::new(),
        true,
    )
    .unwrap();
    assert!(matches!(kept.sources[0].decision, Decision::Keep));
    // A cover the ALAC was not sent with is never sent on its own.
    let covered = with_front_cover(tagged_flac(&FLAC_TAGS));
    let plan = f.plan(&covered, true);
    assert!(
        matches!(&plan.sources[0].decision, Decision::Skip { code, message, .. }
            if *code == "mp3_cover_update_unsupported" && message.contains("FLAC converted to ALAC")),
        "{}",
        serde_json::to_string(&plan.sources[0].decision).unwrap()
    );
}

/// `flac` with a front cover, a minimal baseline JPEG, right after its
/// STREAMINFO.
fn with_front_cover(mut covered: Vec<u8>) -> Vec<u8> {
    let picture = {
        // A minimal baseline JPEG (one SOF0, one scan).
        let mut jpeg = vec![
            0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 2, 44, 2, 44, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0,
            0xff, 0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0,
        ];
        jpeg.extend([0x11, 0xff, 0, 0x22, 0xff, 0xd0, 0x33, 0xff, 0xd9]);
        let mut p = Vec::new();
        p.extend(3u32.to_be_bytes());
        p.extend(10u32.to_be_bytes());
        p.extend(b"image/jpeg");
        p.extend(0u32.to_be_bytes());
        p.extend([0; 16]);
        p.extend((jpeg.len() as u32).to_be_bytes());
        p.extend(&jpeg);
        p
    };
    // Insert a PICTURE block right after STREAMINFO (34 bytes).
    covered[4] &= 0x7f;
    let mut block = vec![6];
    block.extend(&(picture.len() as u32).to_be_bytes()[1..]);
    block.extend(picture);
    covered.splice(4 + 4 + 34..4 + 4 + 34, block);
    covered
}

#[test]
fn prune_keeps_a_converted_track_while_its_flac_is_in_the_source() {
    let f = ConvertedFlac::new();
    let prune = |sources: &SourceCollection| {
        crate::engine::planning::removal::build(
            &f.catalog,
            &f.index,
            &f.identity,
            Some(JournalEvidence {
                journal: &f.journal,
                identity: &f.scope,
            }),
            crate::engine::planning::removal::Request::Prune { sources },
        )
        .unwrap()
    };
    let retitled = tagged_flac(&[("TITLE", "Retitled"), FLAC_TAGS[1], FLAC_TAGS[2]]);
    let plan = prune(&f.sources(&retitled, false));
    let row = &plan.tracks[0];
    assert_eq!(row.matching_source_paths, ["song.flac"]);
    assert!(row.reasons.contains(&"encoded_audio_present_in_source"));
    assert!(!row.requested);
    // With the FLAC gone, the track is the prune's.
    let root = f.temp.0.join("source");
    fs::remove_file(root.join("song.flac")).unwrap();
    fs::copy(
        crate::test_fixtures::workspace_root().join("tests/fixtures/tone-alac.m4a"),
        root.join("other.m4a"),
    )
    .unwrap();
    let sources = inspect(&select(&root, &SelectionOptions::default()).unwrap()).unwrap();
    let plan = prune(&sources);
    let row = &plan.tracks[0];
    assert!(row.matching_source_paths.is_empty() && row.requested);
}

#[test]
fn a_new_flac_with_the_flag_is_planned_as_an_alac_addition() {
    let f = ConvertedFlac::new();
    let empty = Journal::create(&f.temp.0.join("empty-journal"), &f.scope).unwrap();
    // Another title, so no catalogue track is a plausible earlier version.
    let fresh = tagged_flac(&[("TITLE", "Fresh Title"), FLAC_TAGS[1], FLAC_TAGS[2]]);
    let candidate = |path: &str| {
        BTreeMap::from([(
            "song.flac".to_owned(),
            Candidate {
                ids: CandidateIds {
                    track: 1001,
                    album: Some(1002),
                    artist: Some(1003),
                },
                path: path.into(),
            },
        )])
    };
    let plan = f.plan_with(
        &fresh,
        true,
        &empty,
        &candidate("iTunes_Control/Music/F39/ABCD.m4a"),
    );
    let row = &plan.sources[0];
    let Decision::Add { path, .. } = &row.decision else {
        panic!("{}", serde_json::to_string(&row.decision).unwrap());
    };
    assert_eq!(path, "iTunes_Control/Music/F39/ABCD.m4a");
    assert!(row.flac_to_alac && plan.flac_to_alac);
    let assessment = row.mp3_insertion.as_ref().unwrap();
    assert_eq!(assessment.insertion_route, Some("insert_batch"));
    assert_eq!(
        assessment.required_qualification[0],
        "checked_lossless_flac_to_alac_conversion"
    );
    assert_eq!(
        serde_json::to_value(row).unwrap()["flac_to_alac"],
        json!(true)
    );
    // The FLAC's own extension is no ALAC destination.
    let plan = f.plan_with(
        &fresh,
        true,
        &empty,
        &candidate("iTunes_Control/Music/F39/ABCD.flac"),
    );
    assert!(matches!(
        &plan.sources[0].decision,
        Decision::Conflict { code, .. } if *code == "mp3_candidate_or_grouping_conflict"
    ));
    // Without the flag it goes as FLAC.
    let plan = f.plan_with(
        &fresh,
        false,
        &empty,
        &candidate("iTunes_Control/Music/F39/ABCD.flac"),
    );
    let row = &plan.sources[0];
    assert!(matches!(&row.decision, Decision::Add { path, .. } if path.ends_with(".flac")));
    assert!(
        !row.flac_to_alac
            && serde_json::to_value(row)
                .unwrap()
                .get("flac_to_alac")
                .is_none()
    );
    // Generated candidates follow the flag.
    for (flag, extension) in [(true, ".m4a"), (false, ".flac")] {
        let generated = crate::engine::planning::candidates::generate(
            &f.sources(&fresh, flag),
            &f.catalog,
            Some(&empty),
        )
        .unwrap();
        assert!(generated["song.flac"].path.ends_with(extension), "{flag}");
    }
}

#[test]
fn a_new_flac_with_repeated_artists_and_composers_is_added_with_them_joined() {
    let f = ConvertedFlac::new();
    let empty = Journal::create(&f.temp.0.join("empty-journal"), &f.scope).unwrap();
    // Sent as ALAC with the flag, as FLAC without it.
    let candidates = |flag: bool| {
        let extension = if flag { "m4a" } else { "flac" };
        BTreeMap::from([(
            "song.flac".to_owned(),
            Candidate {
                ids: CandidateIds {
                    track: 1001,
                    album: Some(1002),
                    artist: Some(1003),
                },
                path: format!("iTunes_Control/Music/F39/ABCD.{extension}"),
            },
        )])
    };
    let fresh = [("TITLE", "Fresh Title"), FLAC_TAGS[1], FLAC_TAGS[2]];
    let repeated = [
        ("ARTIST", "Artist D"),
        ("ARTIST", "Synthetic Artist"),
        ("ARTISTSORT", "Artist, Synthetic"),
        ("ARTISTSORT", "D, Artist"),
        ("COMPOSER", "Artist B"),
        ("COMPOSER", "Artist C"),
        ("COMPOSER", "Artist B"),
        ("COMPOSERSORT", "B, Artist"),
        ("COMPOSERSORT", "C, Artist"),
    ];
    for flag in [true, false] {
        let plan = f.plan_with(
            &tagged_flac(&[&fresh[..], &repeated].concat()),
            flag,
            &empty,
            &candidates(flag),
        );
        let row = &plan.sources[0];
        assert!(
            matches!(row.decision, Decision::Add { .. }) && !plan.has_conflicts(),
            "{}",
            serde_json::to_string(&row.decision).unwrap()
        );
        let metadata = row
            .mp3_insertion
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap();
        assert_eq!(metadata.artist, "Synthetic Artist, Artist D");
        assert_eq!(
            metadata.sort_artist.as_deref(),
            Some("Artist, Synthetic, D, Artist")
        );
        assert_eq!(metadata.composer.as_deref(), Some("Artist B, Artist C"));
        assert_eq!(
            metadata.sort_composer.as_deref(),
            Some("B, Artist, C, Artist")
        );
    }
    // Any other repeated field still leaves the file out.
    let retitled = tagged_flac(&[&fresh[..], &[("TITLE", "Other Title")]].concat());
    let plan = f.plan_with(&retitled, true, &empty, &candidates(true));
    assert!(
        matches!(&plan.sources[0].decision, Decision::Skip { message, .. }
            if message.contains("title has 2 values in the file's tag; edit the tag to keep one")),
        "{}",
        serde_json::to_string(&plan.sources[0].decision).unwrap()
    );
}

/// A 0.2 s FLAC of a tone at `frequency` Hz, tagged with exactly `tags`:
/// each frequency is other audio.
fn tone_flac(temp: &Temp, frequency: u32, tags: &[(&str, &str)]) -> Vec<u8> {
    let path = temp.0.join(format!("tone-{frequency}.flac"));
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-n",
            "-f",
            "lavfi",
        ])
        .args(["-t", "0.2", "-i"])
        .arg(format!("sine=frequency={frequency}:sample_rate=44100"))
        .args(["-ac", "2", "-c:a", "flac", "-map_metadata", "-1"])
        .arg(&path)
        .env_remove("FFREPORT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    crate::test_fixtures::retagged_flac(&fs::read(path).unwrap(), tags)
}

#[test]
fn a_refused_flac_is_left_out_and_the_rest_of_its_album_goes_together() {
    let f = ConvertedFlac::new();
    let empty = Journal::create(&f.temp.0.join("empty-journal"), &f.scope).unwrap();
    let root = f.temp.0.join("library");
    fs::create_dir(&root).unwrap();
    let album = [("ARTIST", "Artist A"), ("ALBUM", "Album A")];
    let names = ["01 One.flac", "02 Two.flac", "03 Three.flac"];
    // First by name, so it would have opened the album's groups, and the
    // only one with a cover. A repeated title is refused whatever happens to
    // composers.
    fs::write(
        root.join(names[0]),
        with_front_cover(tone_flac(
            &f.temp,
            330,
            &[("TITLE", "One"), ("TITLE", "Uno"), album[0], album[1]],
        )),
    )
    .unwrap();
    for (name, frequency, title) in [(names[1], 440, "Two"), (names[2], 550, "Three")] {
        fs::write(
            root.join(name),
            tone_flac(&f.temp, frequency, &[("TITLE", title), album[0], album[1]]),
        )
        .unwrap();
    }
    let candidates: BTreeMap<_, _> = names
        .iter()
        .zip(["ABCD", "ABCE", "ABCF"])
        .zip([1010, 1020, 1030])
        .map(|((name, file), id)| {
            (
                name.to_string(),
                Candidate {
                    ids: CandidateIds {
                        track: id + 1,
                        album: Some(id + 2),
                        artist: Some(id + 3),
                    },
                    path: format!("iTunes_Control/Music/F39/{file}.m4a"),
                },
            )
        })
        .collect();
    let mut sources = inspect(&select(&root, &SelectionOptions::default()).unwrap()).unwrap();
    sources.flac_to_alac = true;
    let evidence = Some(JournalEvidence {
        journal: &empty,
        identity: &f.scope,
    });
    // `sync` plans with the full comparison, `push` add-only; both leave it
    // out alike.
    let plan = build(
        &sources,
        &f.catalog,
        &f.index,
        &f.identity,
        evidence,
        &candidates,
    )
    .unwrap();
    let pushed = build_with_policy(
        &sources,
        &f.catalog,
        &f.index,
        &f.identity,
        evidence,
        &candidates,
        true,
    )
    .unwrap();
    for plan in [&plan, &pushed] {
        assert!(!plan.has_conflicts());
        let counts = plan.decision_counts();
        assert_eq!(
            (counts.additions, counts.skipped, counts.conflicts),
            (2, 1, 0)
        );
    }
    let [one, two, three] = &plan.sources[..] else {
        panic!("three rows");
    };
    let Decision::Skip {
        code,
        reason,
        message,
    } = &one.decision
    else {
        panic!("{}", serde_json::to_string(&one.decision).unwrap());
    };
    assert_eq!(*code, "mp3_source_profile_unsupported");
    assert_eq!(
        reason,
        "it lists 2 titles, and the phone keeps one. Edit the tag so it has a single title."
    );
    assert!(message.contains("title has 2 values in the file's tag; edit the tag to keep one"));
    assert_eq!(
        serde_json::to_value(&pushed.sources[0].decision).unwrap(),
        serde_json::to_value(&one.decision).unwrap()
    );
    // Nothing of it is planned: no identity, destination or cover.
    assert!(matches!(
        one.artwork,
        ArtworkDecision::UnavailableUntilSourceValidated
    ));
    assert!(!one.flac_to_alac && one.managed_audio_replacement.is_none());
    // The others share one new album and artist, opened by the first of them.
    let (
        Decision::Add {
            album: a,
            artist: x,
            ..
        },
        Decision::Add {
            album: b,
            artist: y,
            ..
        },
    ) = (&two.decision, &three.decision)
    else {
        panic!("two additions");
    };
    assert_eq!(
        (a.id.as_str(), b.id.as_str()),
        ("00000000000003fe", "00000000000003fe")
    );
    assert_eq!(
        (a.status, b.status),
        ("new_this_source", "pending_earlier_source")
    );
    assert_eq!(
        (x.id.as_str(), y.id.as_str()),
        ("00000000000003ff", "00000000000003ff")
    );
    for row in [two, three] {
        assert!(row.flac_to_alac);
        assert!(matches!(
            row.artwork,
            ArtworkDecision::NoSourceCoverPreserveExisting
        ));
    }
    // The saved plan carries the reason; the report and the terminal show it.
    assert_eq!(
        serde_json::to_value(&plan).unwrap()["sources"][0]["reason"],
        reason.as_str()
    );
    let (reason, message) = (reason.clone(), message.clone());
    let counts = plan.decision_counts();
    let result = crate::engine::PlanResult {
        add_only: false,
        serialized: serde_json::to_vec(&plan).unwrap(),
        additions: counts.additions,
        matched: counts.matched,
        updates: counts.updates,
        conflicts: counts.conflicts,
        skipped: counts.skipped,
        source_exclusions: 0,
        plan,
    };
    let report = crate::cli::plan_report(&result);
    assert_eq!(report["ready"], true);
    assert_eq!(report["skipped"], 1);
    assert_eq!(
        report["actions"][0],
        json!({"action": "skip", "path": names[0], "code": "mp3_source_profile_unsupported",
            "reason": reason, "detail": message})
    );
    let mut out = Vec::new();
    crate::cli::write_plan(&mut out, &result, false, false).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("\nSKIP \"01 One.flac\": {reason}\n")),
        "{text}"
    );
    assert!(text.contains("2 additions, 0 matched, 0 updates, 0 conflicts, 1 skipped"));
    // Replacing the library would leave it off the phone.
    let refused = uninsertable_sources(&sources);
    assert!(
        matches!(&refused[..], [(path, why)] if path == names[0] && why.contains("title has 2 values")),
        "{refused:?}"
    );
}

/// A 0.2 s tone FLAC FFmpeg makes with `audio` options, with `FLAC_TAGS`.
fn encoded_flac(temp: &Temp, name: &str, audio: &[&str]) -> Vec<u8> {
    let path = temp.0.join(name);
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-n",
            "-f",
            "lavfi",
        ])
        .args(["-t", "0.2", "-i", "sine=frequency=440:sample_rate=44100"])
        .args(audio)
        .args(["-c:a", "flac", "-map_metadata", "-1"])
        .args(
            FLAC_TAGS
                .iter()
                .flat_map(|(key, value)| ["-metadata".to_owned(), format!("{key}={value}")]),
        )
        .arg(&path)
        .env_remove("FFREPORT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(path).unwrap()
}

#[test]
fn a_flac_the_flag_cannot_convert_is_skipped_without_holding_the_plan() {
    let f = ConvertedFlac::new();
    for (audio, why) in [
        (
            &["-ac", "2", "-sample_fmt", "s32", "-strict", "experimental"][..],
            "32-bit",
        ),
        (&["-ac", "6"][..], "6 channels"),
    ] {
        let flac = encoded_flac(&f.temp, &format!("{why}.flac"), audio);
        // Changed audio at the converted track's source path: never a
        // replacement, so that track stays.
        let plan = f.plan(&flac, true);
        let row = &plan.sources[0];
        assert!(
            matches!(&row.decision, Decision::Skip { code, reason, message }
                if *code == "flac_conversion_refused" && message.contains(why)
                    && reason.contains(why) && !reason.contains("FFmpeg")),
            "{}",
            serde_json::to_string(&row.decision).unwrap()
        );
        assert!(row.managed_audio_replacement.is_none() && !row.flac_to_alac);
        assert_eq!(plan.preserved_unmatched_device_tracks, ["ffffffffffffffff"]);
        assert!(!plan.has_conflicts());
        let counts = plan.decision_counts();
        assert_eq!(
            (counts.skipped, counts.conflicts, counts.additions),
            (1, 0, 0)
        );
        assert_eq!(
            serde_json::to_value(&row.decision).unwrap()["decision"],
            "skip"
        );
        // Without the flag nothing is converted, so no conversion leaves it out.
        let plan = f.plan(&flac, false);
        assert!(!matches!(
            plan.sources[0].decision,
            Decision::Skip {
                code: "flac_conversion_refused",
                ..
            }
        ));
        // Replacing the library would leave it off the phone.
        let refused = uninsertable_sources(&f.sources(&flac, true));
        assert!(
            matches!(&refused[..], [(path, reason)] if path == "song.flac" && reason.contains(why)),
            "{refused:?}"
        );
    }
}

#[test]
fn a_managed_match_without_a_proven_cover_has_its_cover_sent_again() {
    let row =
        |ownership: &'static str, artwork: ArtworkDecision, decision: Decision| SourceDecision {
            relative_path: "a.m4a".into(),
            managed_audio_replacement: None,
            source_file_bytes: None,
            source_file_sha256: None,
            encoded_audio_sha256: None,
            projected: None,
            mp3: None,
            mp3_insertion: None,
            flac_to_alac: false,
            validation_issues: Vec::new(),
            matches: vec![ExistingMatch {
                track_id: "0000000000000001".into(),
                wire_id: "0000000000000001".into(),
                path: "iTunes_Control/Music/F00/TEST.m4a".into(),
                codec: "alac",
                encoded_audio_equal: true,
                full_container_equal: true,
                match_basis: None,
                ownership,
                catalog_differences: vec![],
                canonicalized_fields: vec![],
                device_file_sha256: None,
            }],
            artwork,
            native_composer: None,
            decision,
        };
    let unproven = || ArtworkDecision::ExistingNeedsVerification {
        sha256: "aa".into(),
        bytes: 900,
        observed_cache_ids: vec![700],
    };
    assert_eq!(
        row("inserted_by_us", unproven(), Decision::Keep).cover_resent(false),
        Some(900)
    );
    let missing = ArtworkDecision::SeparateUpdateNeeded {
        sha256: "aa".into(),
        bytes: 800,
    };
    assert_eq!(
        row("inserted_by_us", missing, Decision::Keep).cover_resent(false),
        Some(800)
    );
    // Add-only, a track this tool does not manage, or a proven cover: none.
    assert_eq!(
        row("inserted_by_us", unproven(), Decision::Keep).cover_resent(true),
        None
    );
    assert_eq!(
        row("unowned", unproven(), Decision::Keep).cover_resent(false),
        None
    );
    let proven = ArtworkDecision::VerifiedJournalMatch {
        sha256: "aa".into(),
        bytes: 900,
        cache_id: 700,
    };
    assert_eq!(
        row("inserted_by_us", proven, Decision::Keep).cover_resent(false),
        None
    );
}
