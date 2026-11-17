use serde_json::json;

use super::*;

/// What `device check` prints for `report`.
fn shown(report: &Value) -> String {
    let mut out = Vec::new();
    show_check(report, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn report(readiness: Value) -> Value {
    json!({
        "device": {"name": "iPhone", "product_type": "iPhone99,1", "product_version": "27.2", "build_version": "24Z999"},
        "paths": [],
        "music_folders": 50,
        "staged": ["Sync_00000000.plist", "Sync_00000000.plist.cig"],
        "library_identity": {"state": "valid"},
        "database": null,
        "readiness": readiness,
    })
}

/// Rows left for deleted tracks get one plain warning, with about how many
/// new songs clear them when new songs reach them; the tables and counts stay
/// in the JSON.
#[test]
fn leftover_rows_are_one_plain_warning() {
    let synced = json!({"state": "synced", "initialised_by_rustunes": true});
    let database = |songs: u64, until: Value| {
        let mut report = report(synced.clone());
        report["staged"] = json!([]);
        report["database"] = json!({
            "items": 466, "synced_rows": {}, "db_info_rows": 1, "media_anchor": "9",
            "playlists": 3, "leftover_songs": songs, "songs_until_cleared": until,
        });
        shown(&report)
    };
    let text = database(2, json!(14));
    assert!(
        text.contains(
            "\nWarning: your iPhone's music library has leftover entries from 2 deleted songs. \
             They don't affect your music, and they clear up on their own after you add about 14 \
             more songs.\n"
        ),
        "{text}"
    );
    assert!(!text.contains("item_"), "{text}");
    let text = database(1, Value::Null);
    assert!(
        text.contains(
            "\nWarning: your iPhone's music library has leftover entries from 1 deleted song. \
             They don't affect your music.\n"
        ),
        "{text}"
    );
    assert!(!database(0, Value::Null).contains("Warning"));
}
