//! Pure encoder and decoder for native metadata-only `update_track` sessions.
//!
//! The operation envelope and item keys are those of the observed receiver
//! matrix: one `update_db_info` followed by one `update_track` per track, each
//! with an item dictionary of explicitly changed fields. This module checks the
//! wire shape and value ranges only. Which combinations of keys are safe for a
//! given catalogue is the planner's and the verifier's decision; serialization
//! conveys no receiver, ownership or application authority.
use anyhow::{Context, Result, bail, ensure};
use plist::{Dictionary, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    time::SystemTime,
};

/// Every item key the encoder and decoder accept, in documentation order.
pub const ITEM_KEYS: &[&str] = &[
    "title",
    "sort_name",
    "artist",
    "sort_artist",
    "album",
    "sort_album",
    "album_artist",
    "sort_album_artist",
    "album_pid",
    "artist_pid",
    "genre",
    "composer",
    "sort_composer",
    "year",
    "track_number",
    "track_count",
    "disc_number",
    "disc_count",
    "is_compilation",
    "content_rating",
    "copyright",
    "comment",
];

const TEXT_KEYS: &[&str] = &[
    "title",
    "sort_name",
    "artist",
    "sort_artist",
    "album",
    "sort_album",
    "album_artist",
    "sort_album_artist",
    "genre",
    "composer",
    "sort_composer",
    "copyright",
    "comment",
];
const MAX_TEXT_BYTES: usize = 4096;
const MAX_UPDATES: usize = 4096;

/// Session identity: the database, its library container and the revision
/// (previous anchor + 1) the receiver will assign.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub database_pid: u64,
    pub primary_container_pid: u64,
    pub revision: u32,
    pub timestamp: SystemTime,
}

/// One `update_track`: the track's `item_store.sync_id` and the item fields to
/// change. `album_pid` and `artist_pid` are `album.sync_id` and
/// `album_artist.sync_id`; numbers are plist integers, `is_compilation` a
/// boolean and every other value a string.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackUpdate {
    pub sync_id: u64,
    pub item: BTreeMap<String, Value>,
}

/// Wire-level checks shared by both directions: at least one update, distinct
/// nonzero sync ids, only known item keys and in-range values. Text is
/// nonempty except a copyright or comment.
pub fn validate(updates: &[TrackUpdate]) -> Result<()> {
    ensure!(!updates.is_empty(), "metadata edit has no track updates");
    ensure!(
        updates.len() <= MAX_UPDATES,
        "metadata edit has too many track updates"
    );
    let mut seen = BTreeSet::new();
    for update in updates {
        ensure!(update.sync_id != 0, "metadata edit track sync id is zero");
        ensure!(
            seen.insert(update.sync_id),
            "metadata edit names track {:#018x} more than once",
            update.sync_id
        );
        validate_item(&update.item).with_context(|| format!("track {:#018x}", update.sync_id))?;
    }
    Ok(())
}

fn validate_item(item: &BTreeMap<String, Value>) -> Result<()> {
    ensure!(!item.is_empty(), "metadata edit item has no fields");
    for (key, value) in item {
        ensure!(
            ITEM_KEYS.contains(&key.as_str()),
            "unsupported metadata edit item key {key}"
        );
        validate_value(key, value)?;
    }
    Ok(())
}

fn validate_value(key: &str, value: &Value) -> Result<()> {
    if TEXT_KEYS.contains(&key) {
        let Value::String(text) = value else {
            bail!("{key} must be a string");
        };
        // An empty copyright or comment clears it; an empty genre or composer
        // only resets the item's sort keys. A comment keeps its line breaks
        // and tabs.
        ensure!(
            !text.is_empty() || matches!(key, "copyright" | "comment"),
            "{key} must not be empty"
        );
        ensure!(
            text.len() <= MAX_TEXT_BYTES,
            "{key} exceeds {MAX_TEXT_BYTES} bytes"
        );
        ensure!(
            !text
                .chars()
                .any(|c| c.is_control() && !(key == "comment" && matches!(c, '\n' | '\r' | '\t'))),
            "{key} contains a control character"
        );
        return Ok(());
    }
    if key == "is_compilation" {
        ensure!(
            matches!(value, Value::Boolean(_)),
            "is_compilation must be a boolean"
        );
        return Ok(());
    }
    let Value::Integer(integer) = value else {
        bail!("{key} must be an integer");
    };
    let signed = integer.as_signed();
    let (low, high) = match key {
        "album_pid" | "artist_pid" => {
            ensure!(
                signed.is_some_and(|v| v != 0),
                "{key} must be a nonzero 64-bit integer"
            );
            return Ok(());
        }
        // 0 clears a year or track number; the phone keeps
        // disc 1 for a track that names none.
        "year" => (0, 9999),
        "track_number" | "track_count" | "disc_count" => (0, 65535),
        "disc_number" => (1, 65535),
        "content_rating" => (0, 2),
        _ => bail!("unsupported metadata edit item key {key}"),
    };
    ensure!(
        signed.is_some_and(|v| (low..=high).contains(&v)),
        "{key} must be between {low} and {high}"
    );
    Ok(())
}

/// Encode a complete binary-plist session. `item` dictionaries are written in
/// alphabetical key order. With `update_db_info`, at most 499 updates fit one
/// operation file.
pub fn encode(envelope: &Envelope, updates: &[TrackUpdate]) -> Result<Vec<u8>> {
    ensure!(
        envelope.database_pid != 0 && envelope.primary_container_pid != 0,
        "metadata edit database or library container identity is zero"
    );
    ensure!(envelope.revision != 0, "metadata edit revision is zero");
    validate(updates)?;
    let mut db_info = Dictionary::new();
    db_info.insert(
        "primary_container_pid".into(),
        (envelope.primary_container_pid as i64).into(),
    );
    let mut db_update = Dictionary::new();
    db_update.insert("operation".into(), "update_db_info".into());
    db_update.insert("pid".into(), (envelope.database_pid as i64).into());
    db_update.insert("db_info".into(), Value::Dictionary(db_info));
    let mut operations = vec![Value::Dictionary(db_update)];
    for update in updates {
        let mut item = Dictionary::new();
        for (key, value) in &update.item {
            item.insert(key.clone(), value.clone());
        }
        let mut operation = Dictionary::new();
        operation.insert("operation".into(), "update_track".into());
        operation.insert("pid".into(), (update.sync_id as i64).into());
        operation.insert("item".into(), Value::Dictionary(item));
        operations.push(Value::Dictionary(operation));
    }
    crate::protocol::operations::ensure_within(operations.len())?;
    let mut root = Dictionary::new();
    root.insert("revision".into(), envelope.revision.into());
    root.insert("timestamp".into(), Value::Date(envelope.timestamp.into()));
    root.insert("operations".into(), Value::Array(operations));
    let mut bytes = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut bytes)?;
    Ok(bytes)
}

/// Decode and fully validate a session. Anything but a binary plist holding
/// exactly one leading `update_db_info` and then only `update_track`
/// operations with the exact key sets is refused.
pub fn decode(bytes: &[u8]) -> Result<(Envelope, Vec<TrackUpdate>)> {
    ensure!(
        bytes.starts_with(b"bplist00"),
        "metadata edit operation is not a binary plist"
    );
    let value = Value::from_reader(Cursor::new(bytes)).context("metadata edit plist")?;
    let root = value
        .as_dictionary()
        .context("metadata edit root is not a dictionary")?;
    exact_keys(root, &["revision", "timestamp", "operations"], "envelope")?;
    let revision = root["revision"]
        .as_unsigned_integer()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v != 0)
        .context("metadata edit revision is not a nonzero 32-bit integer")?;
    let Value::Date(date) = &root["timestamp"] else {
        bail!("metadata edit timestamp is not a date");
    };
    let timestamp = SystemTime::from(*date);
    let operations = root["operations"]
        .as_array()
        .context("metadata edit operations is not an array")?;
    let (first, rest) = operations
        .split_first()
        .context("metadata edit has no operations")?;
    let db_update = first
        .as_dictionary()
        .context("metadata edit first operation is not a dictionary")?;
    exact_keys(
        db_update,
        &["operation", "pid", "db_info"],
        "update_db_info",
    )?;
    ensure!(
        db_update["operation"].as_string() == Some("update_db_info"),
        "metadata edit first operation is not update_db_info"
    );
    let database_pid = wire_pid(&db_update["pid"], "database pid")?;
    let db_info = db_update["db_info"]
        .as_dictionary()
        .context("metadata edit db_info is not a dictionary")?;
    exact_keys(db_info, &["primary_container_pid"], "db_info")?;
    let primary_container_pid =
        wire_pid(&db_info["primary_container_pid"], "library container pid")?;
    let mut updates = Vec::new();
    for operation in rest {
        let operation = operation
            .as_dictionary()
            .context("metadata edit operation is not a dictionary")?;
        exact_keys(operation, &["operation", "pid", "item"], "update_track")?;
        ensure!(
            operation["operation"].as_string() == Some("update_track"),
            "metadata edit operation kind is not update_track"
        );
        let sync_id = wire_pid(&operation["pid"], "track pid")?;
        let item = operation["item"]
            .as_dictionary()
            .context("metadata edit item is not a dictionary")?;
        updates.push(TrackUpdate {
            sync_id,
            item: item.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        });
    }
    validate(&updates)?;
    Ok((
        Envelope {
            database_pid,
            primary_container_pid,
            revision,
            timestamp,
        },
        updates,
    ))
}

fn exact_keys(dictionary: &Dictionary, keys: &[&str], what: &str) -> Result<()> {
    ensure!(
        dictionary.len() == keys.len() && keys.iter().all(|key| dictionary.contains_key(key)),
        "metadata edit {what} has unexpected keys"
    );
    Ok(())
}

/// Ids travel as signed 64-bit integers holding the unsigned value's bits.
fn wire_pid(value: &Value, what: &str) -> Result<u64> {
    let signed = value
        .as_signed_integer()
        .with_context(|| format!("metadata edit {what} is not a signed 64-bit integer"))?;
    ensure!(signed != 0, "metadata edit {what} is zero");
    Ok(signed as u64)
}

#[cfg(test)]
mod tests;
