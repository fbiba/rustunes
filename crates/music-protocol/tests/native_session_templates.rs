//! Offline factory/validator integration through the real library interfaces.
use music_protocol::protocol::templates;
use music_protocol::{
    catalogue,
    protocol::{session, signing as grappa},
};
#[path = "../src/test_support/catalogue.rs"]
pub mod catalogue_fixture;

mod qualified_factory {
    use super::*;
    use plist::Value;

    fn identity(catalog: u64, user: u64) -> catalogue::identity::LibraryIdentity {
        let mut prefs = [0; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
        prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
        prefs[8] = 1;
        prefs[12..20].copy_from_slice(&catalog.to_le_bytes());
        prefs[20..28].copy_from_slice(&user.to_le_bytes());
        catalogue::identity::parse(&prefs).unwrap()
    }

    fn get<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
        path.iter().fold(value, |current, key| {
            current.as_dictionary().unwrap().get(key).unwrap()
        })
    }

    #[test]
    fn feeds_existing_validator_with_current_anchor_then_advances_once() {
        let id = identity(0x1234, 0xab);
        for anchor in [0, 10, 13, u32::MAX - 1] {
            let pair = templates::build(&id, "Linux Music", anchor, 1).unwrap();
            assert_eq!(
                get(&pair.finished, &["Params", "DataclassAnchors", "Media"]).as_string(),
                Some(anchor.to_string().as_str())
            );
            let prepared = session::prepare(
                pair.request.clone(),
                &pair.finished,
                id.library_id(),
                anchor,
            )
            .unwrap();
            assert_eq!(prepared.request(), &pair.request);
            assert_eq!(prepared.next_anchor().unwrap(), anchor + 1);
        }
    }

    #[test]
    fn uses_dynamic_identity_and_host_but_exact_qualified_fields_and_types() {
        let id = identity(0x0102_0304_0506_0708, 0x8070_6050_4030_2010);
        let pair = templates::build(&id, "Linux Μουσική", 27, 1).unwrap();
        let host = get(&pair.request, &["Params", "HostInfo"]);
        assert_eq!(host.as_dictionary().unwrap().len(), 9);
        for (key, expected) in [
            ("LibraryID", "8172635445362718"),
            ("SyncHostName", "Linux Μουσική"),
            ("Version", "13.2.4.123"),
            ("WindowsOSVersion", "10.0"),
            ("Type", "iTunes"),
        ] {
            assert_eq!(get(host, &[key]).as_string(), Some(expected));
        }
        assert_eq!(get(host, &["Wakeable"]).as_unsigned_integer(), Some(0));
        assert_eq!(
            get(host, &["Grappa"]).as_data(),
            Some(grappa::HOST_REQUEST.as_slice())
        );
        assert_eq!(
            get(&pair.request, &["Session"]).as_unsigned_integer(),
            Some(2)
        );
        assert_eq!(
            get(&pair.finished, &["Session"]).as_unsigned_integer(),
            Some(2)
        );
        assert_eq!(
            get(&pair.request, &["Params", "DataclassAnchors", "VoiceMemo"]).as_string(),
            Some("1")
        );
        // Exact remaining keys, arrays and integer SyncTypes are enforced by
        // the production validator; this test intentionally invokes it too.
        session::prepare(pair.request, &pair.finished, id.library_id(), 27).unwrap();
    }

    #[test]
    fn refuses_overflow_unqualified_nonmedia_state_and_invalid_host() {
        let id = identity(1, 2);
        assert!(templates::build(&id, "host", u32::MAX, 1).is_err());
        for voice in [0, 2, u32::MAX] {
            assert!(templates::build(&id, "host", 13, voice).is_err());
        }
        for host in ["", "  ", "bad\nname", "bad\0name", "bad\u{7f}name"] {
            assert!(templates::build(&id, host, 13, 1).is_err());
        }
        assert!(templates::build(&id, &"x".repeat(1025), 13, 1).is_err());
        assert!(templates::build(&id, &"x".repeat(1024), 13, 1).is_ok());
    }

    #[test]
    fn factory_templates_round_trip_and_reject_wrong_identity_or_stale_anchor() {
        let id = identity(1, 2);
        let other = identity(2, 4);
        let pair = templates::build(&id, "host", 13, 1).unwrap();
        for value in [&pair.request, &pair.finished] {
            let mut encoded = Vec::new();
            value.to_writer_binary(&mut encoded).unwrap();
            assert_eq!(
                Value::from_reader(std::io::Cursor::new(encoded)).unwrap(),
                *value
            );
        }
        assert!(
            session::prepare(pair.request.clone(), &pair.finished, other.library_id(), 13).is_err()
        );
        assert!(session::prepare(pair.request, &pair.finished, id.library_id(), 12).is_err());
    }

    fn cdb(database_pid: u64, voice: u32) -> Vec<u8> {
        let mut bytes = vec![0; 244 + 144];
        let total = bytes.len() as u32;
        for (offset, value) in [
            (0, 0x6462686d),
            (4, 244),
            (8, total),
            (12, 1),
            (16, 117),
            (20, 1),
            (244, 0x6473686d),
            (248, 96),
            (252, 144),
            (256, 11),
            (340, 7),
            (344, 1),
            (348, voice),
            (352, 1),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[0x48..0x50].copy_from_slice(&database_pid.to_le_bytes());
        bytes
    }

    #[test]
    fn copied_cdb_qualifies_saved_voice_but_uses_current_sql_media_and_identity() {
        let catalog = catalogue_fixture::collection_catalog(13);
        let id = identity(1, 2);
        let bytes = cdb(catalog.database_pid(), 1);
        let pair = templates::build_from_copied_cdb(&bytes, &catalog, &id, "host").unwrap();
        assert_eq!(
            get(&pair.request, &["Params", "DataclassAnchors", "Media"]).as_string(),
            Some("13")
        );
        assert_eq!(
            get(&pair.request, &["Params", "DataclassAnchors", "VoiceMemo"]).as_string(),
            Some("1")
        );
        assert_eq!(
            get(&pair.request, &["Params", "HostInfo", "LibraryID"]).as_string(),
            Some(id.library_id())
        );
        for bytes in [
            cdb(0, 1),
            cdb(1234, 1),
            cdb(catalog.database_pid(), 0),
            cdb(catalog.database_pid(), 2),
            bytes[..300].to_vec(),
        ] {
            assert!(templates::build_from_copied_cdb(&bytes, &catalog, &id, "host").is_err());
        }
    }
}
