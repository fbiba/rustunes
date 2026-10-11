//! Checked source attribution of an inserted binding.
//! No replacement, completion or Start authority.
use super::{
    Attempt, AttemptId, AttemptKind, AttemptState, Binding, BindingReceiptKind, BindingRef,
    Identity, Journal, bindings, encode_content, load_attempt,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
};

const MAX_JSON: usize = 4 * 1024 * 1024;

/// Private typed facts produced only by the existing checked attribution path.
/// The descriptive receipt is never decoded to manufacture these fields.
#[derive(Clone)]
pub(super) struct InitialFacts {
    pub(super) reference: BindingRef,
    pub(super) content: super::Content,
    pub(super) insertion: AttemptId,
    pub(super) root: PathBuf,
    pub(super) base: PathBuf,
    pub(super) options: crate::engine::planning::source_collection::SelectionOptions,
    pub(super) relative_path: String,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|n| format!("{n:02x}")).collect()
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn digest(value: &Value) -> Result<[u8; 32]> {
    let mut out = [0; 32];
    if let Some(s) = value.as_str() {
        ensure!(
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid hexadecimal digest"
        );
        for (i, n) in out.iter_mut().enumerate() {
            *n = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
        }
    } else {
        let values = value
            .as_array()
            .context("digest must be exact hex or bytes")?;
        ensure!(values.len() == 32, "digest byte count differs");
        for (n, value) in out.iter_mut().zip(values) {
            *n = u8::try_from(value.as_u64().context("digest byte invalid")?)?;
        }
    }
    ensure!(out != [0; 32], "zero evidence digest");
    Ok(out)
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}
fn relative(s: &str) -> Result<PathBuf> {
    ensure!(
        !s.is_empty()
            && s.len() <= 4096
            && !s.starts_with('/')
            && !s.contains('\\')
            && !s.chars().any(char::is_control)
            && s.split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "unsafe relative source path"
    );
    Ok(PathBuf::from(s))
}
fn historical_absolute(s: &str) -> Result<PathBuf> {
    ensure!(
        s.starts_with('/')
            && s.len() <= 4096
            && !s.contains('\\')
            && !s.chars().any(char::is_control)
            && (s == "/"
                || s[1..]
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")),
        "historical root/base must have canonical absolute spelling"
    );
    Ok(PathBuf::from(s))
}
fn selectors(v: &Value) -> Result<Vec<String>> {
    let values = v.as_array().context("selection set must be an array")?;
    ensure!(values.len() <= 10000, "selection set bound exceeded");
    let mut out = Vec::new();
    for value in values {
        let s = value.as_str().context("selection path must be a string")?;
        relative(s)?;
        out.push(s.to_owned());
    }
    ensure!(
        out.windows(2).all(|pair| pair[0] < pair[1]),
        "selection set must be sorted and unique"
    );
    Ok(out)
}

/// A read seam over the parent's validated private snapshot. It never returns a
/// Started permit or treats a batch-inserted binding as native.
pub(crate) fn native_origin(
    journal: &Journal,
    expected: BindingRef,
    identity: &Identity,
    require_active: bool,
) -> Result<(Binding, Attempt, [u8; 32])> {
    let snapshot = journal.recheck_read_only_source()?;
    journal.ensure_identity(identity)?;
    // The recheck above pins the copy's bytes, so its schema is validated
    // once per copy, not once per binding the planner asks about.
    let copy = journal
        ._read_only_copy
        .as_ref()
        .context("historical attribution requires a read-only journal snapshot")?;
    if copy.schema_checked.get().is_none() {
        super::validate_schema(&journal.connection)?;
        let _ = copy.schema_checked.set(());
    }
    bindings::binding_at(&journal.connection, expected, require_active)?;
    let (first, provenance, event, kind): (Option<i64>, i64, Option<i64>, i64) = journal.connection.query_row(
        "SELECT r.first_attempt,r.provenance,v.attempt,v.receipt_kind FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id AND v.version=1 WHERE r.id=?1",
        [expected.id.as_i64()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(
        provenance == 1 && first.is_some() && first == event && kind == 1,
        "explicit native insertion ancestry required; batch-inserted and pulled bindings refuse"
    );
    let initial = bindings::binding_at(
        &journal.connection,
        BindingRef {
            id: expected.id,
            version: 1,
        },
        false,
    )?;
    ensure!(
        initial.receipt_kind == BindingReceiptKind::InsertVerification
            && initial.content.artwork.is_none(),
        "initial native insertion receipt/content differs"
    );
    let id = AttemptId(first.context("native insertion attempt missing")?);
    let attempt = load_attempt(&journal.connection, id)?;
    let native: bool = journal.connection.query_row(
        "SELECT binding_id IS NULL AND binding_version IS NULL FROM attempts WHERE id=?1",
        [id.as_i64()],
        |r| r.get(0),
    )?;
    ensure!(
        native
            && attempt.state == AttemptState::Verified
            && attempt.prepared.kind == AttemptKind::Insert
            && attempt.prepared.content == initial.content
            && initial.media_anchor
                == attempt
                    .prepared
                    .current_media_anchor
                    .checked_add(1)
                    .context("anchor overflow")?,
        "initial attempt must be the exact Verified native Insert"
    );
    let count: u32 = journal.connection.query_row(
        "SELECT count(*) FROM attempts WHERE kind=1 AND state=2 AND content=?1",
        [encode_content(&initial.content)?],
        |r| r.get(0),
    )?;
    ensure!(count == 1, "initial verified insertion is ambiguous");
    ensure!(
        journal.recheck_read_only_source()? == snapshot,
        "journal snapshot changed during ancestry check"
    );
    Ok((initial, attempt, snapshot))
}

fn original_physical(
    path: &Path,
    binding: &Binding,
) -> Result<music_protocol::media::fingerprint::TransferredSource> {
    ensure!(
        path.is_absolute(),
        "retained original source must be an absolute explicit path"
    );
    for p in path.ancestors() {
        ensure!(
            !fs::symlink_metadata(p)?.file_type().is_symlink(),
            "retained original source symlink refused"
        );
    }
    let source = if binding.content.source.is_alac() {
        music_protocol::media::fingerprint::read(path)?.into()
    } else {
        music_protocol::media::mpeg::read(path)?.transferred_identity()?
    };
    ensure!(
        source == binding.content.source,
        "retained original bytes/encoded descriptor differ from immutable insertion Content"
    );
    Ok(source)
}

fn operation_ids(attempt: &Attempt, input: &Value) -> Result<()> {
    let value = plist::Value::from_reader(Cursor::new(&attempt.prepared.operation))?;
    let root = value
        .as_dictionary()
        .context("operation dictionary required")?;
    ensure!(
        root.get("revision")
            .and_then(plist::Value::as_unsigned_integer)
            == Some(u64::from(attempt.prepared.current_media_anchor) + 1),
        "operation revision differs from insertion anchor"
    );
    let ops = root
        .get("operations")
        .and_then(plist::Value::as_array)
        .context("operation array required")?;
    ensure!(ops.len() == 2, "insertion operation count differs");
    let db = ops[0]
        .as_dictionary()
        .context("database operation required")?;
    let own = ops[1]
        .as_dictionary()
        .context("insert operation required")?;
    ensure!(
        db.get("operation").and_then(plist::Value::as_string) == Some("update_db_info")
            && own.get("operation").and_then(plist::Value::as_string) == Some("insert_track"),
        "insertion operation shape differs"
    );
    let pid = |v: Option<&plist::Value>| -> Result<u64> {
        let v = v.context("missing persistent operation ID")?;
        let n = v
            .as_unsigned_integer()
            .or_else(|| v.as_signed_integer().map(|n| n as u64))
            .context("invalid persistent operation ID")?;
        ensure!(n != 0, "zero persistent operation ID");
        Ok(n)
    };
    ensure!(
        pid(own.get("pid"))? == attempt.prepared.content.track_pid,
        "operation track identity differs"
    );
    let item = own
        .get("item")
        .and_then(plist::Value::as_dictionary)
        .context("insert item required")?;
    for (field, input_field) in [("album_pid", "album_id"), ("artist_pid", "artist_id")] {
        ensure!(
            format!("{:016x}", pid(item.get(field))?) == text(input, input_field)?,
            "operation grouping identity differs"
        );
    }
    Ok(())
}

fn selection_path(path: &str, selection: &Value) -> Result<()> {
    relative(path)?;
    let recursive = selection["recursive"]
        .as_bool()
        .context("selection recursive flag invalid")?;
    let include = selectors(&selection["include"])?;
    let exclude = selectors(&selection["exclude"])?;
    let selected = |selector: &str| {
        path == selector
            || path
                .strip_prefix(selector)
                .is_some_and(|tail| tail.starts_with('/'))
    };
    ensure!(
        (include.is_empty() || include.iter().any(|s| selected(s)))
            && !exclude.iter().any(|s| selected(s))
            && (recursive || !path.contains('/')),
        "source path is outside the historical selection options"
    );
    // Only child directory ancestors are excluded: the explicitly supplied
    // historical root itself may deliberately have a hidden name.
    for directory in path.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
        ensure!(
            !directory.starts_with('.'),
            "source path violates historical child directory policy"
        );
    }
    Ok(())
}

fn prior_row(row: &Value, input: &Value, binding: &Binding, rows: &[Value]) -> Result<()> {
    let source = &binding.content.source;
    ensure!(
        rows.iter()
            .filter(|r| r["decision"] == "add"
                && (r["track_id"] == input["track_id"] || r["path"] == input["path"]))
            .count()
            == 1,
        "historical prior Add target is ambiguous"
    );
    ensure!(
        row["decision"] == "add"
            && text(row, "relative_path")? == text(input, "source")?
            && text(row, "track_id")? == text(input, "track_id")?
            && text(row, "path")? == binding.content.path
            && row["source_file_bytes"].as_u64() == Some(source.file_bytes)
            && digest(&row["source_file_sha256"])? == source.full_file_sha256
            && digest(&row["encoded_audio_sha256"])? == source.audio.sha256
            && text(&row["album"], "id")? == text(input, "album_id")?
            && text(&row["artist"], "id")? == text(input, "artist_id")?,
        "historical prior Add row differs from immutable insertion"
    );
    Ok(())
}

// Byte-only descriptive row input joins; this value grants no capability.
pub(super) struct RowInputs {
    pub(super) facts: InitialFacts,
    pub(super) projected: Value,
    pub(super) receipt: Value,
}

/// Reject duplicate keys at every level instead of accepting a last-key-wins
/// interpretation of a malformed historical producer or plan document.
struct UniqueJson(Value);
