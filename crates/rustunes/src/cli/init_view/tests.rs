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

/// A synced phone whose staging folder holds an interrupted sync's files is
/// not ready, in plain words, with what to do.
#[test]
fn an_interrupted_sync_is_not_ready() {
    let text = shown(&report(json!({
        "state": "interrupted_sync",
        "initialised_by_rustunes": true,
        "staged": ["Sync_00000000.plist", "Sync_00000000.plist.cig"],
    })));
    assert!(
        text.contains("In Sync/Media: Sync_00000000.plist, Sync_00000000.plist.cig\n"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "Verdict: not ready: an interrupted sync left files on the iPhone that stop \
             rustunes from changing its music. If a rustunes command was interrupted, run it \
             again: it finishes the job and removes these files\n"
        ),
        "{text}"
    );
    let ready = shown(&report(
        json!({"state": "synced", "initialised_by_rustunes": true}),
    ));
    assert!(ready.ends_with("Verdict: initialised by rustunes; ready to sync\n"));
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
