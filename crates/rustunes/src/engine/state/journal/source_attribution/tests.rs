use super::super::{CompletionEvidence, PreparedAttempt, StartEvidence};
use super::*;
use std::{
    os::unix::fs::DirBuilderExt,
    sync::atomic::{AtomicU64, Ordering},
};

// Synthetic storage/producer-shape tests, never receiver observations.
struct Fixture {
    directory: PathBuf,
    original: PathBuf,
    journal: Journal,
    reference: BindingRef,
    identity: Identity,
    input: Vec<u8>,
    plan: Vec<u8>,
    prior_inputs: Vec<Vec<u8>>,
}
impl Fixture {
    fn new(change_plan: impl FnOnce(&mut Value), change_input: impl FnOnce(&mut Value)) -> Self {
        Self::selected("/historical/collection", &[], change_plan, change_input)
    }
    fn selected(
        root: &str,
        include: &[&str],
        change_plan: impl FnOnce(&mut Value),
        change_input: impl FnOnce(&mut Value),
    ) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "attribution-fixture-{}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let original = directory.join("retained-original.m4a");
        fs::write(
            &original,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
        )
        .unwrap();
        let source = music_protocol::media::fingerprint::read(&original).unwrap();
        let mut prefs = vec![0; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
        prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
        prefs[8] = 1;
        prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
        prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
        let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
        let identity = Identity::for_saved_device([1; 32], &library).unwrap();
        let scope = json!({"root":root,"recursive":true,"include":include,"exclude":[]});
        let collection = hash(&serde_json::to_vec(&scope).unwrap());
        let path = "iTunes_Control/Music/F00/TEST.m4a";
        let mut plan = json!({"schema":"rustunes.additive-plan","mode":"additive_dry_run",
            "source_root":root,"selection":{"base_directory":root,"recursive":true,"include":include,"exclude":[]},"media_anchor":13,
            "device_sha256":hex(&identity.device_digest()),"library_sha256":hex(&identity.library_digest()),"phone_deletions":0,"execution_authorized_by_plan":false,
            "sources":[{"relative_path":"native.m4a","source_file_bytes":source.file_bytes,"source_file_sha256":hex(&source.full_file_sha256),
                "encoded_audio_sha256":hex(&source.audio.sha256),"decision":"add","track_id":"00000000000003e9","path":path,
                "album":{"id":"00000000000007d1"},"artist":{"id":"0000000000000bb9"}}]});
        change_plan(&mut plan);
        let plan = serde_json::to_vec(&plan).unwrap();
        // Native plist is independent of these synthetic JSON documents.
        let dictionary = |entries: Vec<(&str, plist::Value)>| {
            plist::Value::Dictionary(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v))
                    .collect(),
            )
        };
        let op = dictionary(vec![
            ("revision", 14u64.into()),
            (
                "timestamp",
                plist::Value::Date(std::time::UNIX_EPOCH.into()),
            ),
            (
                "operations",
                plist::Value::Array(vec![
                    dictionary(vec![
                        ("operation", "update_db_info".into()),
                        ("pid", 51u64.into()),
                        (
                            "db_info",
                            dictionary(vec![("primary_container_pid", 50u64.into())]),
                        ),
                    ]),
                    dictionary(vec![
                        ("operation", "insert_track".into()),
                        ("pid", 1001u64.into()),
                        (
                            "item",
                            dictionary(vec![
                                ("album_pid", 2001u64.into()),
                                ("artist_pid", 3001u64.into()),
                            ]),
                        ),
                    ]),
                ]),
            ),
        ]);
        let mut operation = Vec::new();
        op.to_writer_binary(&mut operation).unwrap();
        let mut input = json!({"schema":"rustunes.insertion-input","pre_session_attempt":1,
            "source":"native.m4a","plan_sha256":hex(&hash(&plan)),"full_source_sha256":hex(&source.full_file_sha256),
            "encoded_audio_sha256":hex(&source.audio.sha256),"track_id":"00000000000003e9","album_id":"00000000000007d1","artist_id":"0000000000000bb9",
            "path":path,"operation_sha256":hex(&hash(&operation)),"complete_index_sha256":hex(&[3;32]),"current_state_receipt_sha256":hex(&[4;32]),"phone_deletions":0});
        change_input(&mut input);
        let input = serde_json::to_vec(&input).unwrap();
        let journal_path = directory.join("journal");
        let mut j = Journal::create(&journal_path, &identity).unwrap();
        let content = super::super::Content {
            source: source.clone().into(),
            track_pid: 1001,
            path: path.into(),
            artwork: None,
            collection: Some(collection),
        };
        let id = j
            .prepare(PreparedAttempt {
                kind: AttemptKind::Insert,
                content,
                new_album_pid: Some(2001),
                new_artist_pid: Some(3001),
                current_media_anchor: 13,
                operation,
                input_receipt_sha256: hash(&input),
            })
            .unwrap();
        let started = j
            .mark_started(
                id,
                StartEvidence {
                    ready_media_anchor: 13,
                    ready_receipt_sha256: [5; 32],
                    signature: [1; 21],
                },
            )
            .unwrap();
        j.complete(
            started,
            CompletionEvidence {
                track_pid: 1001,
                path: path.into(),
                audio_bytes: source.file_bytes,
                audio_file_sha256: source.full_file_sha256,
                artwork_sha256: None,
                media_anchor: 14,
                receipt_sha256: [7; 32],
            },
        )
        .unwrap();
        drop(j);
        let journal = Journal::open_read_only(&journal_path, &identity).unwrap();
        let reference = journal.active_bindings().unwrap().remove(0).reference;
        Self {
            directory,
            original,
            journal,
            reference,
            identity,
            input,
            plan,
            prior_inputs: Vec::new(),
        }
    }
    fn check(&self) -> Result<Value> {
        check(
            &self.journal,
            self.reference,
            &self.identity,
            &self.input,
            &self.plan,
            &self.original,
            &self.prior_inputs,
        )
    }
    fn refuses(&self, needle: &str) {
        let error = self.check().err().expect("must refuse");
        assert!(format!("{error:#}").contains(needle), "{error:#}");
    }

    // Rebuild private synthetic storage with genuinely completed native Insert
    // intents. Each preceding source changes an encoded ALAC sample byte, so
    // immutable audio identities differ without using tags as identity.
    fn batch(count: usize, gap: bool) -> Self {
        let mut f = Self::new(|_| {}, |_| {});
        let mut records = Vec::new();
        let target = load_attempt(&f.journal.connection, AttemptId(1)).unwrap();
        let original_row: Value = parse(&f.plan).unwrap()["sources"][0].clone();
        for number in 0..count {
            let mut prepared = PreparedAttempt {
                kind: target.prepared.kind,
                content: target.prepared.content.clone(),
                new_album_pid: target.prepared.new_album_pid,
                new_artist_pid: target.prepared.new_artist_pid,
                current_media_anchor: target.prepared.current_media_anchor,
                operation: target.prepared.operation.clone(),
                input_receipt_sha256: [0; 32],
            };
            let mut row = original_row.clone();
            let mut input = parse(&f.input).unwrap();
            if number + 1 < count {
                let mut bytes = fs::read(&f.original).unwrap();
                // The tiny fixture has one mdat; the real fingerprint checks the edited container.
                let offset = bytes.windows(4).position(|part| part == b"mdat").unwrap() + 4;
                bytes[offset] ^= u8::try_from(number + 1).unwrap();
                let archive = f.directory.join(format!("prior-{number}.m4a"));
                fs::write(&archive, bytes).unwrap();
                prepared.content.source = music_protocol::media::fingerprint::read(&archive)
                    .unwrap()
                    .into();
                prepared.content.track_pid = 1101 + number as u64;
                prepared.content.path = format!(
                    "iTunes_Control/Music/F00/PRI{}.m4a",
                    char::from(b'A' + u8::try_from(number).unwrap())
                );
                input["source"] = format!("prior-{number}.m4a").into();
            }
            prepared.new_album_pid = (number == 0).then_some(2001);
            prepared.new_artist_pid = (number == 0).then_some(3001);
            prepared.current_media_anchor = 13 + number as u32 + u32::from(gap && number > 0);
            let mut op = plist::Value::from_reader(Cursor::new(&prepared.operation)).unwrap();
            let dictionary = op.as_dictionary_mut().unwrap();
            dictionary.insert(
                "revision".into(),
                u64::from(prepared.current_media_anchor + 1).into(),
            );
            dictionary
                .get_mut("operations")
                .unwrap()
                .as_array_mut()
                .unwrap()[1]
                .as_dictionary_mut()
                .unwrap()
                .insert("pid".into(), prepared.content.track_pid.into());
            prepared.operation.clear();
            op.to_writer_binary(&mut prepared.operation).unwrap();
            input["track_id"] = format!("{:016x}", prepared.content.track_pid).into();
            input["path"] = prepared.content.path.clone().into();
            input["full_source_sha256"] = hex(&prepared.content.source.full_file_sha256).into();
            input["encoded_audio_sha256"] = hex(&prepared.content.source.audio.sha256).into();
            input["operation_sha256"] = hex(&hash(&prepared.operation)).into();
            row["relative_path"] = input["source"].clone();
            row["track_id"] = input["track_id"].clone();
            row["path"] = input["path"].clone();
            row["source_file_sha256"] = input["full_source_sha256"].clone();
            row["encoded_audio_sha256"] = input["encoded_audio_sha256"].clone();
            records.push((prepared, row, input));
        }
        let mut plan = parse(&f.plan).unwrap();
        plan["sources"] = records
            .iter()
            .map(|(_, row, _)| row.clone())
            .collect::<Vec<_>>()
            .into();
        f.plan = serde_json::to_vec(&plan).unwrap();
        let journal_path = f.directory.join("batch-journal");
        let mut journal = Journal::create(&journal_path, &f.identity).unwrap();
        for (mut prepared, _, mut input) in records {
            input["plan_sha256"] = hex(&hash(&f.plan)).into();
            let input = serde_json::to_vec(&input).unwrap();
            prepared.input_receipt_sha256 = hash(&input);
            let anchor = prepared.current_media_anchor;
            let content = prepared.content.clone();
            let id = journal.prepare(prepared).unwrap();
            let started = journal
                .mark_started(
                    id,
                    StartEvidence {
                        ready_media_anchor: anchor,
                        ready_receipt_sha256: [5; 32],
                        signature: [1; 21],
                    },
                )
                .unwrap();
            journal
                .complete(
                    started,
                    CompletionEvidence {
                        track_pid: content.track_pid,
                        path: content.path,
                        audio_bytes: content.source.file_bytes,
                        audio_file_sha256: content.source.full_file_sha256,
                        artwork_sha256: None,
                        media_anchor: anchor + 1,
                        receipt_sha256: [7; 32],
                    },
                )
                .unwrap();
            f.prior_inputs.push(input);
        }
        f.input = f.prior_inputs.pop().unwrap();
        drop(journal);
        f.journal = Journal::open_read_only(&journal_path, &f.identity).unwrap();
        f.reference = f
            .journal
            .active_bindings()
            .unwrap()
            .into_iter()
            .find(|r| r.binding.content.track_pid == 1001)
            .unwrap()
            .reference;
        f
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn synthetic_native_history_keeps_physical_source_and_historical_path_separate() {
    let f = Fixture::new(|_| {}, |_| {});
    let checked = f.check().unwrap();
    assert_eq!(
        checked["historical_absolute_source_path"],
        "/historical/collection/native.m4a"
    );
    assert_eq!(checked["historical_source_opened"], false);
    assert_eq!(checked["desired_source_association_established"], false);
}
#[test]
fn hash_bound_hidden_child_directory_refuses_even_explicit_include() {
    let path = ".hidden/native.m4a";
    Fixture::selected(
        "/historical/collection",
        &[path],
        |p| p["sources"][0]["relative_path"] = path.into(),
        |i| i["source"] = path.into(),
    )
    .refuses("historical child directory policy");
}
#[test]
fn explicit_root_may_be_hidden_and_backup_named_children_are_ordinary() {
    for root in ["/historical/.hidden", "/historical/Backup"] {
        Fixture::selected(root, &[], |_| {}, |_| {})
            .check()
            .unwrap();
    }
    for path in ["Backup/native.m4a", "deep/foo-backup-bar/native.m4a"] {
        Fixture::selected(
            "/historical/collection",
            &[],
            |p| p["sources"][0]["relative_path"] = path.into(),
            |i| i["source"] = path.into(),
        )
        .check()
        .unwrap();
    }
}
#[test]
fn hash_bound_plan_anchor_must_join_exact_insertion_chronology() {
    Fixture::new(|p| p["media_anchor"] = 99.into(), |_| {}).refuses("historical plan anchor");
}
#[test]
fn completed_normal_second_and_third_children_join_exact_prior_add_prefix() {
    for count in [2, 3] {
        let f = Fixture::batch(count, false);
        let checked = f.check().unwrap();
        assert_eq!(
            checked["verified_prior_add_prefix"]
                .as_array()
                .unwrap()
                .len(),
            count - 1
        );
    }
}
#[test]
fn missing_extra_duplicate_foreign_or_malformed_prior_inputs_refuse() {
    let mut f = Fixture::batch(3, false);
    let saved = f.prior_inputs.clone();
    f.prior_inputs.pop();
    f.refuses("exactly match Add prefix");
    f.prior_inputs = saved.clone();
    f.prior_inputs.push(f.input.clone());
    f.refuses("exactly match Add prefix");
    f.prior_inputs = vec![saved[0].clone(), saved[0].clone()];
    f.refuses("duplicated");
    f.prior_inputs = vec![saved[0].clone(), f.input.clone()];
    f.refuses("prior Add row differs");
    f.prior_inputs = saved;
    f.prior_inputs[0].push(b' ');
    f.refuses("no unique Verified native insertion");
}
#[test]
fn completed_but_discontinuous_prior_add_prefix_refuses() {
    Fixture::batch(3, true).refuses("plan anchor prefix is discontinuous");
}
#[test]
fn stale_active_version_refuses() {
    let mut f = Fixture::new(|_| {}, |_| {});
    f.reference.version += 1;
    f.refuses("current active version");
}
#[test]
fn exact_input_and_plan_bytes_are_required() {
    let mut f = Fixture::new(|_| {}, |_| {});
    f.input.push(b' ');
    f.refuses("producer input bytes differ");
    f.input.pop();
    f.plan.push(b' ');
    f.refuses("plan bytes differ");
}
#[test]
fn hash_bound_unknown_producer_and_plan_schemas_refuse() {
    Fixture::new(|_| {}, |i| i["schema"] = "rustunes.unknown-producer".into())
        .refuses("unknown producer schema");
    Fixture::new(|p| p["schema"] = "rustunes.unknown-plan".into(), |_| {})
        .refuses("unknown plan schema");
}
#[test]
fn a_producer_with_a_missing_or_unsupported_pre_session_number_refuses() {
    Fixture::new(
        |_| {},
        |i| {
            i.as_object_mut().unwrap().remove("pre_session_attempt");
        },
    )
    .refuses("pre-session number unsupported");
    Fixture::new(|_| {}, |i| i["pre_session_attempt"] = 3.into())
        .refuses("pre-session number unsupported");
}
#[test]
fn malformed_hash_bound_producer_values_are_not_coerced() {
    Fixture::new(|_| {}, |i| i["source"] = true.into()).refuses("missing string source");
    Fixture::new(|_| {}, |i| i["plan_sha256"] = "xyz".into()).refuses("invalid hexadecimal digest");
}
#[test]
fn writable_journal_handle_cannot_produce_historical_attribution() {
    let f = Fixture::new(|_| {}, |_| {});
    let writable = Journal::open(&f.directory.join("journal"), &f.identity).unwrap();
    let error = check(
        &writable,
        f.reference,
        &f.identity,
        &f.input,
        &f.plan,
        &f.original,
        &f.prior_inputs,
    )
    .err()
    .expect("writable must refuse");
    assert!(format!("{error:#}").contains("read-only"), "{error:#}");
}
#[test]
fn hash_bound_unsafe_or_ambiguous_source_paths_refuse() {
    Fixture::new(|_| {}, |i| i["source"] = "../escape.m4a".into())
        .refuses("unsafe relative source path");
    Fixture::new(
        |p| {
            let row = p["sources"][0].clone();
            p["sources"].as_array_mut().unwrap().push(row);
        },
        |_| {},
    )
    .refuses("source paths are ambiguous");
}
#[test]
fn historical_plan_cannot_assign_the_same_verified_target_to_another_add() {
    Fixture::new(
        |p| {
            let mut row = p["sources"][0].clone();
            row["relative_path"] = "another-source.m4a".into();
            p["sources"].as_array_mut().unwrap().push(row);
        },
        |_| {},
    )
    .refuses("Add target is ambiguous");
}
#[test]
fn hash_bound_collection_scope_tampering_refuses() {
    Fixture::new(
        |p| {
            p["source_root"] = "/other/collection".into();
            p["selection"]["base_directory"] = "/other/collection".into();
        },
        |_| {},
    )
    .refuses("collection digest differs");
    Fixture::new(|p| p["selection"]["recursive"] = "true".into(), |_| {})
        .refuses("recursive flag invalid");
}
#[test]
fn unrelated_hash_bound_verified_identity_is_not_selected() {
    Fixture::new(|_| {}, |i| i["track_id"] = "00000000000003ea".into())
        .refuses("producer target/path differs");
}
#[test]
fn retained_original_physical_bytes_are_required() {
    let f = Fixture::new(|_| {}, |_| {});
    let mut bytes = fs::read(&f.original).unwrap();
    let at = bytes.len() - 1;
    bytes[at] ^= 1;
    fs::write(&f.original, bytes).unwrap();
    f.refuses("retained original bytes/encoded descriptor differ");
}
