//! Encoding consumes the actual parser's public catalogue value.
use music_protocol::catalogue::{self, mutation};
#[path = "../src/test_support/catalogue.rs"]
pub mod catalogue_fixture;

#[test]
fn parsed_catalogue_mutation_keeps_its_saved_anchor() {
    let catalog = catalogue_fixture::collection_catalog(13);
    let target = catalog.tracks()[0].sync_id;
    let encoded = mutation::delete(&catalog, target, std::time::UNIX_EPOCH).unwrap();
    let value = plist::Value::from_reader(std::io::Cursor::new(&encoded)).unwrap();
    assert_eq!(
        value.as_dictionary().unwrap()["revision"].as_unsigned_integer(),
        Some(14)
    );
    assert_eq!(catalog.persisted_media_anchor(), 13);
    assert_eq!(
        encoded,
        mutation::delete(&catalog, target, std::time::UNIX_EPOCH).unwrap()
    );
    assert!(mutation::delete(&catalog, 0, std::time::UNIX_EPOCH).is_err());
}
