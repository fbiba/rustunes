use super::*;
use std::time::{Duration, UNIX_EPOCH};

fn envelope() -> Envelope {
    Envelope {
        database_pid: u64::MAX - 1,
        primary_container_pid: 0x1234_5678_9abc_def0,
        revision: 176,
        // 0.125 s is exact in the plist's binary float timestamp.
        timestamp: UNIX_EPOCH + Duration::new(1_728_928_000, 125_000_000),
    }
}
fn update(sync_id: u64, fields: &[(&str, Value)]) -> TrackUpdate {
    TrackUpdate {
        sync_id,
        item: fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    }
}
fn text(value: &str) -> Value {
    Value::String(value.into())
}
fn int(value: i64) -> Value {
    Value::Integer(value.into())
}

#[test]
fn every_item_key_round_trips_with_exact_types() {
    let full = update(
        0xfedc_ba98_7654_3210,
        &[
            ("title", text("An Example Title")),
            ("sort_name", text("An Example Title")),
            ("artist", text("Artist")),
            ("sort_artist", text("Artist")),
            ("album", text("Album")),
            ("sort_album", text("Album")),
            ("album_artist", text("Album Artist")),
            ("sort_album_artist", text("Album Artist")),
            ("album_pid", int(0x0bad_c0ff_ee00_0001)),
            ("artist_pid", int(0xf000_0000_0000_0002_u64 as i64)),
            ("genre", text("Pop")),
            ("composer", text("Composer")),
            ("sort_composer", text("Composer")),
            ("year", int(2021)),
            ("track_number", int(2)),
            ("track_count", int(0)),
            ("disc_number", int(65535)),
            ("disc_count", int(2)),
            ("is_compilation", Value::Boolean(true)),
            ("content_rating", int(2)),
            ("copyright", text("(C) 2026")),
            ("comment", text("Line one\nLine two")),
        ],
    );
    assert_eq!(full.item.len(), ITEM_KEYS.len());
    let second = update(7, &[("year", int(1999))]);
    let updates = vec![full, second];
    let bytes = encode(&envelope(), &updates).unwrap();
    assert!(bytes.starts_with(b"bplist00"));
    let (decoded_envelope, decoded) = decode(&bytes).unwrap();
    assert_eq!(decoded_envelope, envelope());
    assert_eq!(decoded, updates);
    // Encoding is canonical: a second round gives the same bytes.
    assert_eq!(encode(&decoded_envelope, &decoded).unwrap(), bytes);
}

#[test]
fn wire_shape_is_the_observed_envelope() {
    let bytes = encode(
        &envelope(),
        &[update(
            u64::MAX - 5,
            &[("title", text("T")), ("sort_name", text("T"))],
        )],
    )
    .unwrap();
    let value = Value::from_reader(Cursor::new(&bytes)).unwrap();
    let root = value.as_dictionary().unwrap();
    assert_eq!(
        root.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["operations", "revision", "timestamp"])
    );
    assert_eq!(root["revision"].as_unsigned_integer(), Some(176));
    assert!(matches!(root["timestamp"], Value::Date(_)));
    let operations = root["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 2);
    let db = operations[0].as_dictionary().unwrap();
    assert_eq!(db["operation"].as_string(), Some("update_db_info"));
    assert_eq!(
        db["pid"].as_signed_integer(),
        Some((u64::MAX - 1) as i64),
        "database pid is the signed bit pattern"
    );
    assert_eq!(
        db["db_info"].as_dictionary().unwrap()["primary_container_pid"].as_signed_integer(),
        Some(0x1234_5678_9abc_def0)
    );
    let track = operations[1].as_dictionary().unwrap();
    assert_eq!(track["operation"].as_string(), Some("update_track"));
    assert_eq!(track["pid"].as_signed_integer(), Some(-6));
    // The item dictionary is written in alphabetical key order.
    assert_eq!(
        track["item"]
            .as_dictionary()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["sort_name", "title"]
    );
}

#[test]
fn item_dictionary_keys_are_alphabetical() {
    let bytes = encode(
        &envelope(),
        &[update(
            9,
            &[
                ("year", int(2000)),
                ("album_pid", int(5)),
                ("title", text("A")),
                ("sort_name", text("A")),
                ("copyright", text("c")),
            ],
        )],
    )
    .unwrap();
    let value = Value::from_reader(Cursor::new(&bytes)).unwrap();
    let operations = value.as_dictionary().unwrap()["operations"]
        .as_array()
        .unwrap()
        .clone();
    let keys: Vec<_> = operations[1].as_dictionary().unwrap()["item"]
        .as_dictionary()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn encoder_refuses_invalid_updates() {
    let ok = || vec![update(1, &[("title", text("A")), ("sort_name", text("A"))])];
    encode(&envelope(), &ok()).unwrap();
    let long = "x".repeat(4097);
    let exact = "x".repeat(4096);
    encode(&envelope(), &[update(1, &[("title", text(&exact))])]).unwrap();
    // Clears the receiver applies.
    encode(
        &envelope(),
        &[update(
            1,
            &[
                ("year", int(0)),
                ("track_number", int(0)),
                ("track_count", int(0)),
                ("disc_count", int(0)),
                ("copyright", text("")),
            ],
        )],
    )
    .unwrap();
    // A comment keeps line breaks and tabs, and "" clears it.
    for comment in ["line one\nline two\ttabbed\r\nline three", ""] {
        encode(&envelope(), &[update(1, &[("comment", text(comment))])]).unwrap();
    }
    let refused: Vec<(&str, Vec<TrackUpdate>)> = vec![
        (
            "comment control character",
            vec![update(1, &[("comment", text("a\u{7}b"))])],
        ),
        ("no updates", vec![]),
        (
            "duplicate sync id",
            vec![
                update(1, &[("year", int(2000))]),
                update(1, &[("year", int(2001))]),
            ],
        ),
        ("zero sync id", vec![update(0, &[("year", int(2000))])]),
        ("empty item", vec![update(1, &[])]),
        ("unknown key", vec![update(1, &[("lyrics", text("la"))])]),
        ("empty text", vec![update(1, &[("title", text(""))])]),
        ("empty genre", vec![update(1, &[("genre", text(""))])]),
        ("empty composer", vec![update(1, &[("composer", text(""))])]),
        ("long text", vec![update(1, &[("title", text(&long))])]),
        (
            "control character",
            vec![update(1, &[("title", text("a\nb"))])],
        ),
        ("text type", vec![update(1, &[("title", int(1))])]),
        ("album pid zero", vec![update(1, &[("album_pid", int(0))])]),
        (
            "artist pid text",
            vec![update(1, &[("artist_pid", text("0x1"))])],
        ),
        (
            "album pid unsigned beyond i64",
            vec![update(
                1,
                &[("album_pid", Value::Integer((u64::MAX - 3).into()))],
            )],
        ),
        ("year negative", vec![update(1, &[("year", int(-1))])]),
        ("year 10000", vec![update(1, &[("year", int(10000))])]),
        (
            "track negative",
            vec![update(1, &[("track_number", int(-1))])],
        ),
        (
            "track 65536",
            vec![update(1, &[("track_number", int(65536))])],
        ),
        ("disc zero", vec![update(1, &[("disc_number", int(0))])]),
        (
            "track count 65536",
            vec![update(1, &[("track_count", int(65536))])],
        ),
        (
            "disc count negative",
            vec![update(1, &[("disc_count", int(-1))])],
        ),
        (
            "content rating 3",
            vec![update(1, &[("content_rating", int(3))])],
        ),
        (
            "compilation integer",
            vec![update(1, &[("is_compilation", int(1))])],
        ),
        ("year as text", vec![update(1, &[("year", text("2000"))])]),
    ];
    for (name, updates) in refused {
        assert!(encode(&envelope(), &updates).is_err(), "{name}");
    }
    let mut zero_database = envelope();
    zero_database.database_pid = 0;
    assert!(encode(&zero_database, &ok()).is_err());
    let mut zero_container = envelope();
    zero_container.primary_container_pid = 0;
    assert!(encode(&zero_container, &ok()).is_err());
    let mut zero_revision = envelope();
    zero_revision.revision = 0;
    assert!(encode(&zero_revision, &ok()).is_err());
}

fn dictionary(fields: Vec<(&str, Value)>) -> Value {
    let mut dictionary = Dictionary::new();
    for (key, value) in fields {
        dictionary.insert(key.into(), value);
    }
    Value::Dictionary(dictionary)
}
fn db_update() -> Value {
    dictionary(vec![
        ("operation", "update_db_info".into()),
        ("pid", int(-1_234_567_890_123_456_789)),
        (
            "db_info",
            dictionary(vec![(
                "primary_container_pid",
                int(-2_345_678_901_234_567_890),
            )]),
        ),
    ])
}
fn track(pid: i64, item: Value) -> Value {
    dictionary(vec![
        ("operation", "update_track".into()),
        ("pid", int(pid)),
        ("item", item),
    ])
}
fn session(revision: u32, operations: Vec<Value>) -> Vec<u8> {
    session_with(vec![
        ("revision", revision.into()),
        ("timestamp", Value::Date(UNIX_EPOCH.into())),
        ("operations", Value::Array(operations)),
    ])
}
fn session_with(fields: Vec<(&str, Value)>) -> Vec<u8> {
    let mut bytes = Vec::new();
    dictionary(fields).to_writer_binary(&mut bytes).unwrap();
    bytes
}
fn good_item() -> Value {
    dictionary(vec![("year", int(2001))])
}

#[test]
fn decoder_accepts_a_hand_built_session_and_refuses_variations() {
    let good = session(5, vec![db_update(), track(11, good_item())]);
    let (decoded_envelope, updates) = decode(&good).unwrap();
    assert_eq!(decoded_envelope.revision, 5);
    assert_eq!(decoded_envelope.timestamp, UNIX_EPOCH);
    assert_eq!(updates, vec![update(11, &[("year", int(2001))])]);

    let delete = dictionary(vec![("operation", "delete_track".into()), ("pid", int(11))]);
    let mut wrong_first = Dictionary::new();
    wrong_first.insert("operation".into(), "update_track".into());
    wrong_first.insert("pid".into(), int(11));
    wrong_first.insert("item".into(), good_item());
    let extra_track_key = dictionary(vec![
        ("operation", "update_track".into()),
        ("pid", int(11)),
        ("item", good_item()),
        ("extra", int(1)),
    ]);
    let extra_db_key = dictionary(vec![
        ("operation", "update_db_info".into()),
        ("pid", int(-5)),
        (
            "db_info",
            dictionary(vec![
                ("primary_container_pid", int(-6)),
                ("name", "x".into()),
            ]),
        ),
    ]);
    let second_db_update = db_update();
    let refused: Vec<(&str, Vec<u8>)> = vec![
        ("delete", session(5, vec![db_update(), delete])),
        (
            "track first",
            session(
                5,
                vec![Value::Dictionary(wrong_first), track(12, good_item())],
            ),
        ),
        ("only db update", session(5, vec![db_update()])),
        ("no operations", session(5, vec![])),
        (
            "second db update",
            session(5, vec![db_update(), second_db_update]),
        ),
        (
            "extra track key",
            session(5, vec![db_update(), extra_track_key]),
        ),
        (
            "extra db_info key",
            session(5, vec![extra_db_key, track(11, good_item())]),
        ),
        (
            "unknown item key",
            session(
                5,
                vec![
                    db_update(),
                    track(11, dictionary(vec![("lyrics", "x".into())])),
                ],
            ),
        ),
        (
            "empty item",
            session(5, vec![db_update(), track(11, dictionary(vec![]))]),
        ),
        (
            "duplicate track",
            session(
                5,
                vec![db_update(), track(11, good_item()), track(11, good_item())],
            ),
        ),
        (
            "zero track pid",
            session(5, vec![db_update(), track(0, good_item())]),
        ),
        (
            "zero revision",
            session(0, vec![db_update(), track(11, good_item())]),
        ),
        (
            "extra envelope key",
            session_with(vec![
                ("revision", 5_u32.into()),
                ("timestamp", Value::Date(UNIX_EPOCH.into())),
                (
                    "operations",
                    Value::Array(vec![db_update(), track(11, good_item())]),
                ),
                ("extra", int(1)),
            ]),
        ),
        (
            "timestamp not a date",
            session_with(vec![
                ("revision", 5_u32.into()),
                ("timestamp", "now".into()),
                (
                    "operations",
                    Value::Array(vec![db_update(), track(11, good_item())]),
                ),
            ]),
        ),
        (
            "negative revision",
            session_with(vec![
                ("revision", int(-5)),
                ("timestamp", Value::Date(UNIX_EPOCH.into())),
                (
                    "operations",
                    Value::Array(vec![db_update(), track(11, good_item())]),
                ),
            ]),
        ),
        (
            "revision beyond 32 bits",
            session_with(vec![
                ("revision", Value::Integer((1u64 << 32).into())),
                ("timestamp", Value::Date(UNIX_EPOCH.into())),
                (
                    "operations",
                    Value::Array(vec![db_update(), track(11, good_item())]),
                ),
            ]),
        ),
        ("not a dictionary", {
            let mut bytes = Vec::new();
            Value::Array(vec![]).to_writer_binary(&mut bytes).unwrap();
            bytes
        }),
        ("garbage", b"bplist00garbage".to_vec()),
        ("empty", Vec::new()),
    ];
    for (name, bytes) in refused {
        assert!(decode(&bytes).is_err(), "{name}");
    }
    // An XML property list is not the wire format.
    let mut xml = Vec::new();
    Value::from_reader(Cursor::new(&good))
        .unwrap()
        .to_writer_xml(&mut xml)
        .unwrap();
    assert!(decode(&xml).is_err());
}

#[test]
fn one_session_holds_at_most_499_updates() {
    let updates = |count: u64| -> Vec<TrackUpdate> {
        (1..=count)
            .map(|id| update(id, &[("title", text("Title"))]))
            .collect()
    };
    encode(&envelope(), &updates(499)).unwrap();
    let error = encode(&envelope(), &updates(500)).unwrap_err().to_string();
    assert!(error.contains("500 one file may hold"), "{error}");
}
