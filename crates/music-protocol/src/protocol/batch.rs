//! Several new tracks in one metadata session. Each track is serialized by its
//! own single-insert encoder (ALAC or MPEG) at the same anchor and timestamp;
//! this only joins them, so every `insert_track` stays byte-for-byte what a
//! single insert would send, apart from the two fields that name its cover.
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use std::collections::BTreeSet;
use std::io::Cursor;

/// The same bound as a batched delete operation.
pub const MAX_OPERATION: usize = 4 * 1024 * 1024;

/// Join single-insert envelopes into one. They must share the revision,
/// timestamp and `update_db_info`, each carry exactly one `insert_track`, and
/// name distinct track PIDs. The order of `singles` is the operation order.
/// With `update_db_info`, at most 499 inserts fit one operation file.
pub fn combine(singles: &[Vec<u8>]) -> Result<Vec<u8>> {
    ensure!(!singles.is_empty(), "no inserts to combine");
    let mut header: Option<(Value, Value, Value)> = None;
    let mut operations = Vec::with_capacity(singles.len() + 1);
    let mut pids = BTreeSet::new();
    for bytes in singles {
        let root = Value::from_reader(Cursor::new(bytes))?;
        let root = root
            .as_dictionary()
            .context("insert envelope is not a dictionary")?;
        ensure!(root.len() == 3, "insert envelope has unexpected keys");
        let revision = root.get("revision").context("insert revision missing")?;
        let timestamp = root.get("timestamp").context("insert timestamp missing")?;
        let ops = root
            .get("operations")
            .and_then(Value::as_array)
            .context("insert operations missing")?;
        ensure!(
            ops.len() == 2,
            "single insert must carry one database update and one insert"
        );
        let database = &ops[0];
        let insert = ops[1]
            .as_dictionary()
            .context("insert operation is not a dictionary")?;
        ensure!(
            database
                .as_dictionary()
                .and_then(|d| d.get("operation"))
                .and_then(Value::as_string)
                == Some("update_db_info"),
            "single insert does not start with update_db_info"
        );
        ensure!(
            insert.get("operation").and_then(Value::as_string) == Some("insert_track"),
            "single insert does not carry insert_track"
        );
        let pid = insert
            .get("pid")
            .and_then(Value::as_signed_integer)
            .context("insert pid missing")?;
        ensure!(pids.insert(pid), "track {:016x} inserted twice", pid as u64);
        match &header {
            None => {
                header = Some((revision.clone(), timestamp.clone(), database.clone()));
                operations.push(database.clone());
            }
            Some((r, t, d)) => ensure!(
                r == revision && t == timestamp && d == database,
                "inserts disagree on revision, timestamp or catalogue identity"
            ),
        }
        operations.push(Value::Dictionary(insert.clone()));
    }
    let (revision, timestamp, _) = header.context("no inserts to combine")?;
    crate::protocol::operations::ensure_within(operations.len())?;
    let mut envelope = Dictionary::new();
    envelope.insert("revision".into(), revision);
    envelope.insert("operations".into(), Value::Array(operations));
    envelope.insert("timestamp".into(), timestamp);
    let mut bytes = Vec::new();
    Value::Dictionary(envelope).to_writer_binary(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_OPERATION,
        "insert batch operation exceeds size limit"
    );
    Ok(bytes)
}

/// One session's removals and insertions in one operation file, in the order
/// Apple's host stages a sync: `update_db_info`, every `delete_track` of
/// `removal` (one session's deletes), then every `insert_track` of
/// `insertion` (from [`combine`]). Both must carry the same revision,
/// timestamp and `update_db_info`, and no track may be both removed and
/// inserted.
pub fn with_removals(removal: &[u8], insertion: &[u8]) -> Result<Vec<u8>> {
    let read = |bytes: &[u8], kind: &str| -> Result<(Value, Value, Vec<Value>)> {
        let root = Value::from_reader(Cursor::new(bytes))?;
        let mut root = root
            .into_dictionary()
            .with_context(|| format!("{kind} envelope is not a dictionary"))?;
        ensure!(root.len() == 3, "{kind} envelope has unexpected keys");
        let mut take = |key: &str| {
            root.remove(key)
                .with_context(|| format!("{kind} {key} missing"))
        };
        let (revision, timestamp) = (take("revision")?, take("timestamp")?);
        let operations = take("operations")?
            .into_array()
            .with_context(|| format!("{kind} operations missing"))?;
        Ok((revision, timestamp, operations))
    };
    let (revision, timestamp, removals) = read(removal, "removal")?;
    let (other_revision, other_timestamp, insertions) = read(insertion, "insertion")?;
    ensure!(
        revision == other_revision && timestamp == other_timestamp,
        "removal and insertion disagree on revision or timestamp"
    );
    ensure!(
        removals.len() > 1 && insertions.len() > 1 && removals[0] == insertions[0],
        "removal and insertion must each start with the same update_db_info and carry operations"
    );
    let named = |operation: &Value, kind: &str| -> Result<i64> {
        let operation = operation
            .as_dictionary()
            .with_context(|| format!("{kind} operation is not a dictionary"))?;
        ensure!(
            operation.get("operation").and_then(Value::as_string) == Some(kind),
            "expected only {kind} operations"
        );
        operation
            .get("pid")
            .and_then(Value::as_signed_integer)
            .with_context(|| format!("{kind} pid missing"))
    };
    let mut pids = BTreeSet::new();
    for operation in &removals[1..] {
        ensure!(
            pids.insert(named(operation, "delete_track")?),
            "track removed twice"
        );
    }
    for operation in &insertions[1..] {
        let pid = named(operation, "insert_track")?;
        ensure!(
            pids.insert(pid),
            "track {:016x} both removed and inserted",
            pid as u64
        );
    }
    let operations: Vec<Value> = removals
        .into_iter()
        .chain(insertions.into_iter().skip(1))
        .collect();
    crate::protocol::operations::ensure_within(operations.len())?;
    let mut envelope = Dictionary::new();
    envelope.insert("revision".into(), revision);
    envelope.insert("operations".into(), Value::Array(operations));
    envelope.insert("timestamp".into(), timestamp);
    let mut bytes = Vec::new();
    Value::Dictionary(envelope).to_writer_binary(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_OPERATION,
        "session operation exceeds size limit"
    );
    Ok(bytes)
}

/// One single insert with its track's cover named, as reference iTunes sends
/// an inserted track with artwork: `artwork_cache_id` and `artwork_checksum`
/// in the `insert_track` item. The phone then asks for the track's audio and
/// artwork together (AssetParts 3).
pub fn with_artwork(single: &[u8], cache_id: u32, checksum: u32) -> Result<Vec<u8>> {
    ensure!(cache_id >= 100, "artwork cache ID is reserved");
    let mut root = Value::from_reader(Cursor::new(single))?;
    let insert = root
        .as_dictionary_mut()
        .and_then(|r| r.get_mut("operations"))
        .and_then(Value::as_array_mut)
        .filter(|ops| ops.len() == 2)
        .and_then(|ops| ops[1].as_dictionary_mut())
        .context("single insert has no insert_track")?;
    ensure!(
        insert.get("operation").and_then(Value::as_string) == Some("insert_track"),
        "single insert has no insert_track"
    );
    let item = insert
        .get_mut("item")
        .and_then(Value::as_dictionary_mut)
        .context("insert_track has no item")?;
    // A single insert without a cover sends checksum 0 and no cache ID.
    ensure!(
        !item.contains_key("artwork_cache_id")
            && item
                .get("artwork_checksum")
                .is_none_or(|c| c.as_unsigned_integer() == Some(0)),
        "single insert already names artwork"
    );
    item.insert("artwork_cache_id".into(), u64::from(cache_id).into());
    item.insert("artwork_checksum".into(), u64::from(checksum).into());
    let mut bytes = Vec::new();
    root.to_writer_binary(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::mpeg::{Envelope, Ids, Timing, TrackFields, serialize};
    use std::time::{Duration, UNIX_EPOCH};

    fn single(track: u64, anchor: u32, seconds: u64) -> Vec<u8> {
        serialize(
            Envelope {
                database_pid: 11,
                primary_container_pid: 12,
                before_anchor: anchor,
                timestamp: UNIX_EPOCH + Duration::from_secs(seconds),
            },
            Ids {
                track,
                album: 5000,
                artist: 6000,
            },
            TrackFields {
                title: "Title",
                sort_title: "Title",
                artist: "Artist",
                sort_artist: "Artist",
                album: "Album",
                sort_album: "Album",
                bit_rate_kbps: 128,
                sample_rate: 44100,
                channels: 2,
                timing: Timing::Coded {
                    samples: 44100,
                    total_time_ms: 1000,
                },
            },
        )
        .unwrap()
    }

    fn operations(bytes: &[u8]) -> Vec<Value> {
        let root = Value::from_reader(Cursor::new(bytes)).unwrap();
        root.as_dictionary().unwrap()["operations"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[test]
    fn one_file_joins_at_most_499_inserts() {
        let singles: Vec<_> = (1..=500).map(|track| single(1000 + track, 40, 7)).collect();
        let joined = combine(&singles[..499]).unwrap();
        assert_eq!(operations(&joined).len(), 500);
        let error = combine(&singles).unwrap_err().to_string();
        assert!(error.contains("501 operations"), "{error}");
    }

    #[test]
    fn inserts_join_after_one_database_update_in_order() {
        let (a, b) = (single(1001, 40, 7), single(1002, 40, 7));
        let joined = combine(&[a.clone(), b.clone()]).unwrap();
        let root = Value::from_reader(Cursor::new(&joined)).unwrap();
        let root = root.as_dictionary().unwrap();
        assert_eq!(root["revision"].as_unsigned_integer(), Some(41));
        let ops = operations(&joined);
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0], operations(&a)[0]);
        // Each insert is exactly what its single envelope carried.
        assert_eq!(ops[1], operations(&a)[1]);
        assert_eq!(ops[2], operations(&b)[1]);
    }

    #[test]
    fn a_cover_adds_only_its_two_item_fields_and_still_combines() {
        let plain = single(1001, 40, 7);
        let covered = with_artwork(&plain, 140, 1017).unwrap();
        let (before, after) = (operations(&plain), operations(&covered));
        assert_eq!(before[0], after[0]);
        let mut item = after[1].as_dictionary().unwrap()["item"]
            .as_dictionary()
            .unwrap()
            .clone();
        assert_eq!(
            item.remove("artwork_cache_id")
                .unwrap()
                .as_unsigned_integer(),
            Some(140)
        );
        assert_eq!(
            item.remove("artwork_checksum")
                .unwrap()
                .as_unsigned_integer(),
            Some(1017)
        );
        let mut plain_item = before[1].as_dictionary().unwrap()["item"]
            .as_dictionary()
            .unwrap()
            .clone();
        assert_eq!(
            plain_item
                .remove("artwork_checksum")
                .unwrap()
                .as_unsigned_integer(),
            Some(0)
        );
        assert_eq!(item, plain_item);
        let joined = combine(&[covered.clone(), single(1002, 40, 7)]).unwrap();
        assert_eq!(operations(&joined)[1], after[1]);
        assert!(with_artwork(&covered, 141, 1017).is_err());
        assert!(with_artwork(&plain, 99, 1017).is_err());
    }

    /// One session's `delete_track` of `tracks`, as the catalogue builds it.
    fn removal(tracks: &[u64], anchor: u32, seconds: u64) -> Vec<u8> {
        let mut ops = vec![operations(&single(1, anchor, seconds))[0].clone()];
        for track in tracks {
            let mut delete = Dictionary::new();
            delete.insert("operation".into(), "delete_track".into());
            delete.insert("pid".into(), (*track as i64).into());
            ops.push(Value::Dictionary(delete));
        }
        let mut bytes = Vec::new();
        Value::Dictionary(
            [
                ("revision".to_owned(), Value::from(u64::from(anchor) + 1)),
                (
                    "timestamp".to_owned(),
                    Value::Date((UNIX_EPOCH + Duration::from_secs(seconds)).into()),
                ),
                ("operations".to_owned(), Value::Array(ops)),
            ]
            .into_iter()
            .collect(),
        )
        .to_writer_binary(&mut bytes)
        .unwrap();
        bytes
    }

    #[test]
    fn removals_go_before_insertions_after_one_database_update() {
        let gone = removal(&[2001, 2002], 40, 7);
        let added = combine(&[single(1001, 40, 7), single(1002, 40, 7)]).unwrap();
        let session = with_removals(&gone, &added).unwrap();
        let root = Value::from_reader(Cursor::new(&session)).unwrap();
        assert_eq!(
            root.as_dictionary().unwrap()["revision"].as_unsigned_integer(),
            Some(41)
        );
        let ops = operations(&session);
        assert_eq!(ops[..3], operations(&gone)[..]);
        assert_eq!(ops[3..], operations(&added)[1..]);
        // One file holds 500 operations, update_db_info among them.
        let many: Vec<u64> = (2001..2251).collect();
        let half: Vec<_> = (1001..1250).map(|track| single(track, 40, 7)).collect();
        assert!(with_removals(&removal(&many, 40, 7), &combine(&half).unwrap()).is_ok());
        let half: Vec<_> = (1001..1251).map(|track| single(track, 40, 7)).collect();
        let error = with_removals(&removal(&many, 40, 7), &combine(&half).unwrap()).unwrap_err();
        assert!(error.to_string().contains("501 operations"), "{error}");
    }

    #[test]
    fn a_session_refuses_halves_that_disagree_or_overlap() {
        let added = combine(&[single(1001, 40, 7)]).unwrap();
        assert!(with_removals(&removal(&[2001], 41, 7), &added).is_err());
        assert!(with_removals(&removal(&[2001], 40, 8), &added).is_err());
        assert!(with_removals(&removal(&[1001], 40, 7), &added).is_err());
        assert!(with_removals(&removal(&[2001, 2001], 40, 7), &added).is_err());
        assert!(with_removals(&removal(&[], 40, 7), &added).is_err());
        // Either half in the other's place refuses.
        assert!(with_removals(&added, &removal(&[2001], 40, 7)).is_err());
    }

    #[test]
    fn mismatched_or_repeated_inserts_refuse() {
        let a = single(1001, 40, 7);
        assert!(combine(&[]).is_err());
        assert!(combine(&[a.clone(), a.clone()]).is_err());
        assert!(combine(&[a.clone(), single(1002, 41, 7)]).is_err());
        assert!(combine(&[a.clone(), single(1002, 40, 8)]).is_err());
        let mut delete = Vec::new();
        Value::Dictionary(
            [
                ("revision".to_owned(), Value::from(41u64)),
                (
                    "timestamp".to_owned(),
                    Value::Date((UNIX_EPOCH + Duration::from_secs(7)).into()),
                ),
                (
                    "operations".to_owned(),
                    Value::Array(vec![operations(&a)[0].clone()]),
                ),
            ]
            .into_iter()
            .collect(),
        )
        .to_writer_binary(&mut delete)
        .unwrap();
        assert!(combine(&[a, delete]).is_err());
    }
}
