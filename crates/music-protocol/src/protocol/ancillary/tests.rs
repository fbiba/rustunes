use super::*;

#[test]
fn metadata_policy_requires_existing_target_and_allows_only_foreign_ancillary() {
    let snapshot = synthetic("id", 28);
    assert!(snapshot.metadata("12345").is_err());
    let policy = snapshot.metadata("-1").unwrap();
    assert!(policy.ensure_session("id", 28).is_ok());
    assert!(policy.ensure_session("id", 27).is_err());
    assert!(
        policy
            .validate_manifest(&manifest(&[]), 2)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        policy
            .validate_manifest(&manifest(&[("-888", 2), ("999", 4)]), 2)
            .unwrap(),
        ["-888", "999"]
    );
    for records in [
        vec![("-1", 0)],
        vec![("-1", 1)],
        vec![("-1", 2)],
        vec![("-1", 4)],
        vec![("-1", 5)],
        vec![("-888", 1)],
        vec![("999", 2)],
        vec![("-888", 2), ("-888", 4)],
        vec![("77", 4)],
    ] {
        assert!(policy.validate_manifest(&manifest(&records), 2).is_err());
    }
    assert!(
        policy
            .validate_manifest(&manifest(&[("-888", 2)]), 3)
            .is_err()
    );
}

fn identity(a: u64, b: u64) -> LibraryIdentity {
    let mut raw = [0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4..6].copy_from_slice(&1u16.to_le_bytes());
    raw[6..8].copy_from_slice(&25u16.to_le_bytes());
    raw[8] = 1;
    raw[12..20].copy_from_slice(&a.to_le_bytes());
    raw[20..28].copy_from_slice(&b.to_le_bytes());
    crate::catalogue::identity::parse(&raw).unwrap()
}

/// Synthetic wire peers only; production cannot construct or mutate ID maps.
pub(crate) fn synthetic(library_id: &str, anchor: u32) -> EligibleSnapshot {
    let catalog = crate::test_support::catalogue::collection_catalog(anchor);
    let mut snapshot = EligibleSnapshot::new(&catalog, &identity(1, 2));
    snapshot.library_id = library_id.to_owned();
    snapshot.tracks.insert(-888i64 as u64, Some(100));
    snapshot.tracks.insert(999, None);
    snapshot.occupied.extend([-888i64 as u64, 999]);
    snapshot
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}
pub(crate) fn manifest(records: &[(&str, u64)]) -> Value {
    dict(vec![
        ("Command", "AssetManifest".into()),
        ("Session", 2u64.into()),
        ("Type", 0u64.into()),
        ("Id", 10u64.into()),
        (
            "Params",
            dict(vec![(
                "AssetManifest",
                dict(vec![(
                    "Media",
                    Value::Array(
                        records
                            .iter()
                            .map(|(id, part)| {
                                dict(vec![
                                    ("AssetID", (*id).into()),
                                    ("AssetType", "Music".into()),
                                    ("IsDownload", true.into()),
                                    ("Variant", dict(vec![("AssetParts", (*part).into())])),
                                ])
                            })
                            .collect(),
                    ),
                )]),
            )]),
        ),
    ])
}
fn set(value: &mut Value, path: &[&str], replacement: Value) {
    let mut current = value;
    for key in &path[..path.len() - 1] {
        current = current.as_dictionary_mut().unwrap().get_mut(key).unwrap();
    }
    current
        .as_dictionary_mut()
        .unwrap()
        .insert(path[path.len() - 1].into(), replacement);
}

#[test]
fn valid_manifest_accepts_fresh_foreign_entities_and_own_audio_in_any_order() {
    let snapshot = synthetic("id", 13);
    let policy = snapshot.audio("-12345").unwrap();
    assert!(policy.ensure_session("id", 13).is_ok());
    assert!(policy.ensure_session("other", 13).is_err());
    assert!(policy.ensure_session("id", 12).is_err());
    assert!(
        policy
            .validate_manifest(&manifest(&[("-12345", 1)]), 2)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        policy
            .validate_manifest(&manifest(&[("-888", 2), ("-12345", 1), ("999", 4)]), 2)
            .unwrap(),
        ["-888", "999"]
    );
    assert_eq!(
        policy
            .validate_manifest(&manifest(&[("999", 4), ("-888", 2), ("-12345", 1)]), 2)
            .unwrap(),
        ["999", "-888"]
    );
}

#[test]
fn refuses_zero_partial_mixed_duplicate_unknown_and_unregistered_art() {
    let snapshot = synthetic("id", 13);
    let policy = snapshot.audio("-12345").unwrap();
    for records in [
        vec![],
        vec![("-888", 2)],
        vec![("-12345", 2)],
        vec![("-12345", 3)],
        vec![("-12345", 5)],
        vec![("-12345", 1), ("-12345", 1)],
        vec![("-12345", 1), ("-888", 2), ("-888", 4)],
        vec![("-12345", 1), ("-888", 1)],
        vec![("-12345", 1), ("-888", 3)],
        vec![("-12345", 1), ("-888", 8)],
        vec![("-12345", 1), ("999", 2)],
        vec![("-12345", 1), ("42", 4)],
        vec![("-12345", 1), ("-31", 4)],
        // A valid prefix never authorizes partial refusals on a later failure.
        vec![("-12345", 1), ("-888", 2), ("42", 4)],
    ] {
        assert!(
            policy.validate_manifest(&manifest(&records), 2).is_err(),
            "{records:?}"
        );
    }
    for bad in [
        "0",
        "00",
        "-0",
        "+999",
        "0999",
        " 999",
        "18446744073709551615",
    ] {
        assert!(
            policy
                .validate_manifest(&manifest(&[("-12345", 1), (bad, 4)]), 2)
                .is_err()
        );
    }
    for own in ["0", "+12345", "-888", "999", "-1", "-31"] {
        assert!(snapshot.audio(own).is_err());
    }
}

#[test]
fn audio_profiles_are_explicit_and_audio_only_stays_exact() {
    let snapshot = synthetic("id", 13);
    let audio_only = snapshot.audio("-12345").unwrap();
    let genius = snapshot.audio_with_disabled_genius("-12345").unwrap();
    assert_eq!(
        audio_only.manifest_profile(),
        AudioManifestProfile::AudioOnly
    );
    assert_eq!(
        genius.manifest_profile(),
        AudioManifestProfile::AudioWithDisabledGenius
    );
    assert!(
        audio_only
            .validate_manifest(&manifest(&[("-12345", 1)]), 2)
            .is_ok()
    );
    assert!(
        audio_only
            .validate_manifest(&manifest(&[("-12345", 5)]), 2)
            .is_err()
    );
    assert!(
        genius
            .validate_manifest(&manifest(&[("-12345", 1)]), 2)
            .is_ok()
    );
    assert!(genius.ensure_session("id", 13).is_ok());
    assert!(genius.ensure_session("other", 13).is_err());
    assert!(genius.ensure_session("id", 12).is_err());
    for own in ["0", "00", "+12345", "-012345", "-888", "999", "-1", "-11"] {
        assert!(snapshot.audio_with_disabled_genius(own).is_err(), "{own}");
    }
}

#[test]
fn audio_genius_accepts_observed_mask_and_ordinary_audio_without_own_refusal() {
    let snapshot = synthetic("id", 13);
    let policy = snapshot.audio_with_disabled_genius("-12345").unwrap();
    for part in [1, 5] {
        assert!(
            policy
                .validate_manifest(&manifest(&[("-12345", part)]), 2)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            policy
                .validate_manifest(&manifest(&[("-12345", part), ("-888", 2)]), 2)
                .unwrap(),
            ["-888"]
        );
    }
}

#[test]
fn audio_genius_refuses_unqualified_masks_and_whole_invalid_manifests() {
    let snapshot = synthetic("id", 13);
    let policy = snapshot.audio_with_disabled_genius("-12345").unwrap();
    for part in [0, 2, 3, 4, 6, 7, 8, 9, 13, 16, 17, u64::MAX] {
        assert!(
            policy
                .validate_manifest(&manifest(&[("-12345", part)]), 2)
                .is_err(),
            "{part}"
        );
    }
    for part in [1, 5] {
        for records in [
            vec![("-12345", part), ("-12345", 1)],
            vec![("-12345", part), ("-888", 5)],
            vec![("-12345", part), ("-888", 1)],
            vec![("-12345", part), ("999", 2)],
            vec![("-12345", part), ("42", 4)],
            vec![("-12345", part), ("-888", 2), ("42", 4)],
        ] {
            assert!(
                policy.validate_manifest(&manifest(&records), 2).is_err(),
                "{records:?}"
            );
        }
    }
    assert!(policy.validate_manifest(&manifest(&[]), 2).is_err());
    assert!(
        policy
            .validate_manifest(&manifest(&[("-888", 2)]), 2)
            .is_err()
    );
    let mut wrong = manifest(&[("-12345", 1)]);
    set(&mut wrong, &["Session"], 3u64.into());
    assert!(policy.validate_manifest(&wrong, 2).is_err());
}

#[test]
fn refuses_wrong_session_shape_direction_and_excessive_count() {
    let snapshot = synthetic("id", 13);
    let policy = snapshot.audio("-12345").unwrap();
    let good = manifest(&[("-12345", 1)]);
    assert!(policy.validate_manifest(&good, 3).is_err());
    assert!(policy.validate_manifest(&good, 0).is_err());
    for (path, value) in [
        (vec!["Command"], "Other".into()),
        (vec!["Session"], 3u64.into()),
        (vec!["Session"], "2".into()),
        (vec!["Type"], 1u64.into()),
        (vec!["Id"], (-1i64).into()),
        (vec!["Extra"], true.into()),
        (
            vec!["Params", "AssetManifest", "Book"],
            Value::Array(vec![]),
        ),
    ] {
        let mut bad = good.clone();
        set(&mut bad, &path, value);
        assert!(policy.validate_manifest(&bad, 2).is_err());
    }
    for (key, value) in [
        ("IsDownload", false.into()),
        ("AssetType", "Book".into()),
        ("AssetID", (-12345i64).into()),
        ("Extra", true.into()),
    ] {
        let mut bad = good.clone();
        let asset = bad
            .as_dictionary_mut()
            .unwrap()
            .get_mut("Params")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .get_mut("AssetManifest")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .get_mut("Media")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .first_mut()
            .unwrap();
        set(asset, &[key], value);
        assert!(policy.validate_manifest(&bad, 2).is_err());
    }
    let over = vec![("-12345", 1); MAX_MANIFEST_ASSETS + 1];
    assert!(
        policy
            .validate_manifest(&manifest(&over), 2)
            .unwrap_err()
            .to_string()
            .contains("count")
    );
}

#[test]
fn production_constructor_binds_full_identity_snapshot_and_filters_availability() {
    let catalog = crate::test_support::catalogue::collection_catalog(13);
    let id = identity(1, 2);
    let snapshot = EligibleSnapshot::new(&catalog, &id);
    snapshot.ensure_current(&catalog, &id).unwrap();
    assert!(snapshot.ensure_current(&catalog, &identity(4, 7)).is_err()); // same XOR, other full identity
    assert!(
        snapshot
            .ensure_current(&crate::test_support::catalogue::collection_catalog(14), &id)
            .is_err()
    );
    assert_eq!(snapshot.tracks.len(), 1);
    assert_eq!(snapshot.tracks[&(-1i64 as u64)], None);
    assert_ne!(snapshot.binding_sha256(), [0; 32]);
    let original = catalog.tracks()[0].clone();
    assert!(eligible_track(&original));
    for field in 0..6 {
        let mut t = original.clone();
        match field {
            0 => t.sync_id = 0,
            1 => t.afc_path = None,
            2 => t.availability.is_placeholder = true,
            3 => t.availability.is_library_added = false,
            4 => t.availability.has_non_purgeable_asset = false,
            _ => t.availability.is_supported_type = false,
        }
        assert!(!eligible_track(&t));
    }
}

#[test]
fn manifest_bound_includes_own_asset_and_allows_at_most_1023_foreign_requests() {
    let mut snapshot = synthetic("id", 13);
    let ids: Vec<String> = (100_000..101_023).map(|id| id.to_string()).collect();
    for id in &ids {
        snapshot.tracks.insert(id.parse().unwrap(), None);
    }
    let policy = snapshot.audio("-12345").unwrap();
    let mut records = vec![("-12345", 1)];
    records.extend(ids.iter().map(|id| (id.as_str(), 4)));
    assert_eq!(
        policy
            .validate_manifest(&manifest(&records), 2)
            .unwrap()
            .len(),
        1023
    );
    records.push(("101023", 4));
    assert!(
        policy
            .validate_manifest(&manifest(&records), 2)
            .unwrap_err()
            .to_string()
            .contains("count")
    );
}

#[test]
fn artwork_policy_requires_existing_target_and_accepts_only_own_artwork() {
    let snapshot = synthetic("id", 13);
    // Existing target without an old art registration is allowed: its update
    // introduces the registration. A new PID and a collection are not targets.
    let policy = snapshot.artwork("-1").unwrap();
    assert_eq!(
        policy
            .validate_manifest(&manifest(&[("-888", 2), ("-1", 2), ("999", 4)]), 2)
            .unwrap(),
        ["-888", "999"]
    );
    // The phone may add the disabled Genius bit (observed).
    assert!(policy.validate_manifest(&manifest(&[("-1", 6)]), 2).is_ok());
    for part in [0, 1, 3, 4, 7, 8, 10] {
        assert!(
            policy
                .validate_manifest(&manifest(&[("-1", part)]), 2)
                .is_err()
        );
    }
    for id in ["-12345", "-31", "0", "+999"] {
        assert!(snapshot.artwork(id).is_err());
    }
    assert!(snapshot.audio("-1").is_err());
}

/// Genuine typed parser fixture, not a production origin/ownership constructor.
pub(crate) fn pending_fixture(anchor: u32) -> (Catalog, LibraryIdentity, EligibleSnapshot) {
    let catalog = crate::test_support::catalogue::modified_collection_catalog(
        anchor,
        "UPDATE item SET base_location_id=0;
         UPDATE item_extra SET location='';
         UPDATE item_state SET has_non_purgeable_asset=0;",
    );
    let id = identity(1, 2);
    let snapshot = EligibleSnapshot::new(&catalog, &id);
    (catalog, id, snapshot)
}

#[test]
fn pending_audio_factory_requires_pathless_row_and_keeps_new_collision_guard() {
    let (catalog, id, snapshot) = pending_fixture(140);
    assert!(snapshot.audio_with_disabled_genius("-1").is_err());
    let policy = snapshot
        .pending_audio_with_disabled_genius(&catalog, &id, "-1")
        .unwrap();
    assert!(policy.ensure_session(id.library_id(), 140).is_ok());
    assert!(policy.ensure_session(id.library_id(), 139).is_err());
    assert!(
        snapshot
            .pending_audio_with_disabled_genius(&catalog, &identity(4, 7), "-1")
            .is_err()
    );
    assert!(
        snapshot
            .pending_audio_with_disabled_genius(&catalog, &id, "-01")
            .is_err()
    );
    assert!(
        snapshot
            .pending_audio_with_disabled_genius(&catalog, &id, "-12345")
            .is_err()
    );
    let playable = crate::test_support::catalogue::collection_catalog(140);
    assert!(
        EligibleSnapshot::new(&playable, &id)
            .pending_audio_with_disabled_genius(&playable, &id, "-1")
            .is_err()
    );
    for sql in [
        "UPDATE item_state SET is_placeholder=1;",
        "UPDATE item_state SET is_library_added=0;",
        "UPDATE item_state SET is_supported_type=0;",
    ] {
        let base = "UPDATE item SET base_location_id=0;
            UPDATE item_extra SET location=''; UPDATE item_state SET has_non_purgeable_asset=0;";
        let bad = crate::test_support::catalogue::modified_collection_catalog(
            140,
            &format!("{base}{sql}"),
        );
        assert!(
            EligibleSnapshot::new(&bad, &id)
                .pending_audio_with_disabled_genius(&bad, &id, "-1")
                .is_err()
        );
    }
}

#[test]
fn pending_audio_manifest_atomic_masks_foreign_registration_and_duplicate_refusals() {
    let (catalog, id, mut snapshot) = pending_fixture(140);
    // Synthetic foreign peers only, as in the existing dynamic wire tests.
    snapshot.tracks.insert(-888i64 as u64, Some(100));
    snapshot.tracks.insert(999, None);
    let policy = snapshot
        .pending_audio_with_disabled_genius(&catalog, &id, "-1")
        .unwrap();
    for mask in [1, 5] {
        let accepted = policy
            .validate_manifest(&manifest(&[("-1", mask), ("-888", 2), ("999", 4)]), 2)
            .unwrap();
        assert_eq!(accepted.own_parts(), mask);
        assert_eq!(
            accepted.foreign_asset_ids(),
            ["-888".to_owned(), "999".to_owned()].as_slice()
        );
    }
    for records in [
        vec![("-1", 4)],
        vec![("-1", 7)],
        vec![("-1", 9)],
        vec![("-1", 5), ("-1", 1)],
        vec![("-888", 2)],
        vec![("-1", 5), ("999", 2)],
        vec![("-1", 5), ("-888", 1)],
        vec![("-1", 5), ("-888", 2), ("-888", 4)],
        vec![("-1", 5), ("77", 4)],
    ] {
        assert!(policy.validate_manifest(&manifest(&records), 2).is_err());
    }
    assert!(
        policy
            .validate_manifest(&manifest(&[("-1", 5)]), 99)
            .is_err()
    );
    assert!(
        policy
            .validate_manifest(&manifest(&vec![("-1", 5); MAX_MANIFEST_ASSETS + 1]), 2)
            .is_err()
    );
}

// Synthetic foreign registrations for pending duplex wire tests only. These
// never stand for producer origin or actual phone artwork/provider evidence.
pub(crate) fn pending_wire_fixture(anchor: u32) -> (Catalog, LibraryIdentity, EligibleSnapshot) {
    let (catalog, identity, mut snapshot) = pending_fixture(anchor);
    snapshot.tracks.insert(-888i64 as u64, Some(100));
    snapshot.tracks.insert(999, None);
    (catalog, identity, snapshot)
}

#[test]
fn audio_batch_requires_distinct_new_identities() {
    let snapshot = synthetic("library", 11);
    let ids = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let policy = snapshot.audio_batch(&ids(&["-12345", "-12346"])).unwrap();
    assert_eq!(policy.own_asset_ids(), ids(&["-12345", "-12346"]));
    assert!(snapshot.audio_batch(&[]).is_err());
    assert!(snapshot.audio_batch(&ids(&["-12345", "-12345"])).is_err());
    assert!(snapshot.audio_batch(&ids(&["-12345", "0"])).is_err());
    // An existing track's identity is occupied.
    assert!(snapshot.audio_batch(&ids(&["-12345", "-888"])).is_err());
}

#[test]
fn audio_batch_with_artwork_requires_each_declared_mask() {
    let snapshot = synthetic("library", 11);
    let own = [("-12345".to_owned(), true), ("-12346".to_owned(), false)];
    let policy = snapshot.audio_batch_with_artwork(&own).unwrap();
    assert_eq!(
        policy.manifest_profile(),
        AudioManifestProfile::AudioWithArtwork
    );
    // The covered track asks for Track and Artwork, the other for Track only,
    // in any order, beside a declined foreign artwork request.
    assert!(
        policy
            .validate_manifest(&manifest(&[("-12346", 1), ("-888", 2), ("-12345", 3)]), 2)
            .is_ok()
    );
    // The phone may add the disabled Genius bit to either (observed).
    assert!(
        policy
            .validate_manifest(&manifest(&[("-12345", 7), ("-12346", 5)]), 2)
            .is_ok()
    );
    for records in [
        // The covered track without its artwork, or the other with artwork.
        vec![("-12345", 1), ("-12346", 1)],
        vec![("-12345", 3), ("-12346", 3)],
        vec![("-12345", 5), ("-12346", 1)],
        vec![("-12345", 3), ("-12346", 7)],
        // Lyrics are never declared.
        vec![("-12345", 11), ("-12346", 1)],
        // Every own track must be requested.
        vec![("-12345", 3)],
    ] {
        assert!(
            policy.validate_manifest(&manifest(&records), 2).is_err(),
            "{records:?}"
        );
    }
    // An ordinary audio batch still refuses the artwork mask.
    let ids = [own[0].0.clone(), own[1].0.clone()];
    let plain = snapshot.audio_batch(&ids).unwrap();
    assert!(
        plain
            .validate_manifest(&manifest(&[("-12345", 3), ("-12346", 1)]), 2)
            .is_err()
    );
}
