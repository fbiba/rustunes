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
fn parse(bytes: &[u8]) -> Result<Value> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_JSON,
        "historical JSON bound exceeded"
    );
    let value: UniqueJson =
        serde_json::from_slice(bytes).context("historical JSON must have unique object keys")?;
    ensure!(value.0.is_object(), "historical JSON must be an object");
    Ok(value.0)
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

fn producer(
    initial: &Binding,
    attempt: &Attempt,
    identity: &Identity,
    input_bytes: &[u8],
    original_source: Option<&Path>,
) -> Result<Value> {
    let source = &initial.content.source;
    ensure!(
        hash(input_bytes) == attempt.prepared.input_receipt_sha256,
        "producer input bytes differ from exact insertion receipt"
    );
    let input = parse(input_bytes)?;
    let schema = text(&input, "schema")?;
    match schema {
        "rustunes.insertion-input" => {
            ensure!(source.is_alac(), "ALAC producer codec differs");
            ensure!(
                matches!(input["pre_session_attempt"].as_u64(), Some(1 | 2)),
                "ALAC producer pre-session number unsupported"
            );
            ensure!(
                digest(&input["full_source_sha256"])? == source.full_file_sha256
                    && digest(&input["encoded_audio_sha256"])? == source.audio.sha256,
                "ALAC producer source identity differs"
            );
        }
        "rustunes.mpeg-insertion-input" => {
            ensure!(!source.is_alac(), "MPEG producer codec differs");
            ensure!(
                digest(&input["content_sha256"])? == hash(&encode_content(&initial.content)?),
                "MPEG producer Content digest differs"
            );
            ensure!(
                digest(&input["device_sha256"])? == identity.device_digest()
                    && digest(&input["library_sha256"])? == identity.library_digest(),
                "MPEG producer identity scope differs"
            );
            let inspected = music_protocol::media::mpeg::read(
                original_source.context("original MPEG source required")?,
            )?;
            ensure!(
                input["source_fingerprint"] == serde_json::to_value(inspected.fingerprint)?,
                "MPEG producer fingerprint differs from original source"
            );
            ensure!(
                input["automatic_replay"] == false,
                "MPEG producer replay policy differs"
            );
        }
        _ => bail!("unknown producer schema cannot establish historical attribution"),
    }
    ensure!(
        input["phone_deletions"].as_u64() == Some(0)
            && text(&input, "track_id")? == format!("{:016x}", initial.content.track_pid)
            && text(&input, "path")? == initial.content.path,
        "producer target/path differs"
    );
    ensure!(
        digest(&input["operation_sha256"])? == hash(&attempt.prepared.operation),
        "producer operation digest differs"
    );
    digest(&input["complete_index_sha256"])?;
    digest(&input["current_state_receipt_sha256"])?;
    operation_ids(attempt, &input)?;
    relative(text(&input, "source")?)?;
    Ok(input)
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

/// Explicit prior ALAC Add inputs, in the plan's existing execution order.
/// Retired predecessors use their immutable initial versions, never new permits.
fn check_prefix(
    journal: &Journal,
    identity: &Identity,
    target: &Attempt,
    input: &Value,
    plan: &Value,
    rows: &[Value],
    supplied: &[Vec<u8>],
) -> Result<Value> {
    let mut anchor = u32::try_from(
        plan["media_anchor"]
            .as_u64()
            .context("historical plan anchor missing")?,
    )?;
    let adds = rows
        .iter()
        .filter(|r| r["decision"] == "add")
        .collect::<Vec<_>>();
    let position = adds
        .iter()
        .position(|r| r["relative_path"] == input["source"])
        .context("historical target Add missing")?;
    ensure!(
        position <= 32 && supplied.len() == position,
        "historical prior input list must exactly match Add prefix"
    );
    let mut seen = BTreeSet::new();
    let mut previous_id = 0;
    let mut checked = Vec::new();
    for (row, bytes) in adds[..position].iter().zip(supplied) {
        let sha = hash(bytes);
        ensure!(seen.insert(sha), "historical prior input is duplicated");
        let mut statement = journal.connection.prepare(
            "SELECT r.id FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id AND v.version=1 JOIN attempts a ON a.id=r.first_attempt WHERE a.input_receipt=?1 AND r.first_attempt=v.attempt AND a.kind=1 AND a.state=2 LIMIT 2")?;
        let ids = statement
            .query_map([sha.as_slice()], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            ids.len() == 1,
            "historical prior input has no unique Verified native insertion"
        );
        let (binding, attempt, _) = native_origin(
            journal,
            BindingRef {
                id: super::BindingId(ids[0]),
                version: 1,
            },
            identity,
            false,
        )?;
        let prior = producer(&binding, &attempt, identity, bytes, None)?;
        // Every prior Add is an ALAC insertion of the same plan and
        // collection; an MPEG prefix is refused.
        ensure!(
            binding.content.source.is_alac()
                && digest(&prior["plan_sha256"])? == digest(&input["plan_sha256"])?
                && binding.content.collection == target.prepared.content.collection,
            "historical prior producer/plan/collection differs"
        );
        prior_row(row, &prior, &binding, rows)?;
        selection_path(text(&prior, "source")?, &plan["selection"])?;
        ensure!(
            attempt.prepared.current_media_anchor == anchor
                && attempt.id.as_i64() > previous_id
                && attempt.id.as_i64() < target.id.as_i64(),
            "historical plan anchor prefix is discontinuous"
        );
        anchor = binding.media_anchor;
        previous_id = attempt.id.as_i64();
        checked.push(json!({"binding_id":ids[0],"initial_version":1,"insertion_attempt":attempt.id.as_i64(),
            "producer_input_sha256":hex(&sha),"initial_receipt_sha256":hex(&binding.receipt_sha256),
            "base_anchor":attempt.prepared.current_media_anchor,"verified_anchor":anchor,"relative_path":prior["source"]}));
    }
    ensure!(
        anchor == target.prepared.current_media_anchor,
        "historical plan anchor does not join insertion chronology"
    );
    Ok(checked.into())
}

// Byte-only descriptive row input joins; this value grants no capability.
pub(super) struct RowInputs {
    pub(super) facts: InitialFacts,
    pub(super) projected: Value,
    pub(super) receipt: Value,
}

pub(crate) fn check(
    journal: &Journal,
    expected: BindingRef,
    identity: &Identity,
    input_bytes: &[u8],
    plan_bytes: &[u8],
    original_source: &Path,
    prior_inputs: &[Vec<u8>],
) -> Result<Value> {
    let (initial, _, journal_sha) = native_origin(journal, expected, identity, true)?;
    let physical = original_physical(original_source, &initial)?;
    let row = checked_inputs(
        journal,
        expected,
        identity,
        input_bytes,
        plan_bytes,
        Some(original_source),
        prior_inputs,
    )?;
    ensure!(
        original_physical(original_source, &initial)? == physical,
        "historical input changed before checked result"
    );
    ensure!(
        journal.recheck_read_only_source()? == journal_sha,
        "historical input changed before checked result"
    );
    Ok(row.receipt)
}

pub(super) fn descriptive_row(
    journal: &Journal,
    expected: BindingRef,
    identity: &Identity,
    input_bytes: &[u8],
    plan_bytes: &[u8],
    prior_inputs: &[Vec<u8>],
) -> Result<RowInputs> {
    let (initial, _, _) = native_origin(journal, expected, identity, true)?;
    ensure!(
        initial.content.source.is_alac(),
        "descriptive row requires ALAC"
    );
    checked_inputs(
        journal,
        expected,
        identity,
        input_bytes,
        plan_bytes,
        None,
        prior_inputs,
    )
}

fn checked_inputs(
    journal: &Journal,
    expected: BindingRef,
    identity: &Identity,
    input_bytes: &[u8],
    plan_bytes: &[u8],
    original_source: Option<&Path>,
    prior_inputs: &[Vec<u8>],
) -> Result<RowInputs> {
    let (initial, attempt, journal_sha) = native_origin(journal, expected, identity, true)?;
    let source = initial.content.source.clone();
    let input = producer(&initial, &attempt, identity, input_bytes, original_source)?;
    let relative_path = text(&input, "source")?;
    relative(relative_path)?;
    let plan_sha = hash(plan_bytes);
    ensure!(
        digest(&input["plan_sha256"])? == plan_sha,
        "plan bytes differ from exact producer plan receipt"
    );
    let plan = parse(plan_bytes)?;
    ensure!(
        plan["schema"] == "rustunes.additive-plan" && plan["mode"] == "additive_dry_run",
        "unknown plan schema cannot establish attribution"
    );
    ensure!(
        digest(&plan["device_sha256"])? == identity.device_digest()
            && digest(&plan["library_sha256"])? == identity.library_digest(),
        "historical plan scope differs"
    );
    ensure!(
        plan["phone_deletions"].as_u64() == Some(0)
            && plan["execution_authorized_by_plan"] == false,
        "historical additive plan policy differs"
    );
    let root = historical_absolute(text(&plan, "source_root")?)?;
    let selection = &plan["selection"];
    let base = historical_absolute(text(selection, "base_directory")?)?;
    // A directory selection is its own base. A file selection's base is the
    // file's directory and the file is its one relative path; the collection
    // digest below still binds the exact root.
    ensure!(
        root == base || root == base.join(relative_path) && !relative_path.contains('/'),
        "historical root/base attribution differs"
    );
    let recursive = selection["recursive"]
        .as_bool()
        .context("selection recursive flag invalid")?;
    let include = selectors(&selection["include"])?;
    let exclude = selectors(&selection["exclude"])?;
    selection_path(relative_path, selection)?;
    let scope = json!({"root":root,"recursive":recursive,"include":include,"exclude":exclude});
    let scope_bytes = serde_json::to_vec(&scope)?;
    let collection_sha = hash(&scope_bytes);
    ensure!(
        initial.content.collection == Some(collection_sha),
        "historical root/options collection digest differs from immutable Content"
    );
    let rows = plan["sources"]
        .as_array()
        .context("historical source rows missing")?;
    ensure!(
        !rows.is_empty() && rows.len() <= 10000,
        "historical source row bound exceeded"
    );
    let mut seen = BTreeSet::new();
    let mut selected_row = None;
    for row in rows {
        let path = text(row, "relative_path")?;
        relative(path)?;
        ensure!(
            seen.insert(path.to_lowercase()),
            "historical source paths are ambiguous"
        );
        if path == relative_path {
            selected_row = Some(row);
        }
    }
    let row = selected_row.context("source path missing from exact historical plan")?;
    let same_target_adds = rows
        .iter()
        .filter(|row| {
            row["decision"] == "add"
                && (row["track_id"].as_str() == input["track_id"].as_str()
                    || row["path"].as_str() == Some(initial.content.path.as_str()))
        })
        .count();
    ensure!(same_target_adds == 1, "historical Add target is ambiguous");
    ensure!(
        row["decision"] == "add"
            && text(row, "track_id")? == text(&input, "track_id")?
            && text(row, "path")? == initial.content.path,
        "historical row is not the exact original Add target"
    );
    ensure!(
        row["source_file_bytes"].as_u64() == Some(source.file_bytes)
            && digest(&row["source_file_sha256"])? == source.full_file_sha256
            && digest(&row["encoded_audio_sha256"])? == source.audio.sha256,
        "historical plan source identity differs"
    );
    ensure!(
        text(&row["album"], "id")? == text(&input, "album_id")?
            && text(&row["artist"], "id")? == text(&input, "artist_id")?,
        "historical grouping identity differs"
    );
    let prefix = check_prefix(
        journal,
        identity,
        &attempt,
        &input,
        &plan,
        rows,
        prior_inputs,
    )?;
    if !source.is_alac() {
        ensure!(
            digest(&input["collection_sha256"])? == collection_sha
                && row["mp3"]["fingerprint"] == input["source_fingerprint"],
            "MPEG producer/plan collection or fingerprint differs"
        );
        let (audio_offset, trailing_bytes) = match (source.audio.mpeg(), source.audio.encoded()) {
            (Some(audio), _) => (audio.audio_offset, Some(audio.trailing_bytes)),
            (None, Some(audio)) => (audio.audio_offset, None),
            (None, None) => anyhow::bail!("MPEG descriptor missing"),
        };
        ensure!(
            row["mp3"]["audio_offset"].as_u64() == Some(audio_offset)
                && trailing_bytes
                    .is_none_or(|bytes| row["mp3"]["trailing_tag_bytes"].as_u64() == Some(bytes)),
            "MPEG plan physical layout differs"
        );
    }
    ensure!(
        journal.recheck_read_only_source()? == journal_sha,
        "historical input changed before checked result"
    );
    let active = bindings::binding_at(&journal.connection, expected, true)?;
    let audio = if let Some(e) = source.audio.encoded() {
        let mut audio = crate::engine::state::audio_index::codec_json(
            &crate::engine::state::audio_index::OtherCodec::Encoded(e.clone()),
        );
        audio["sha256"] = json!(hex(&source.audio.sha256));
        audio
    } else if let Some(mpeg) = source.audio.mpeg() {
        json!({"codec":"mpeg1-layer3","sha256":hex(&source.audio.sha256),"frame_bytes_sha256":hex(&mpeg.frame_bytes_sha256),
            "sample_rate":mpeg.sample_rate,"channels":mpeg.channels,"frame_count":mpeg.frame_count,"audio_offset":mpeg.audio_offset,
            "audio_bytes":mpeg.audio_bytes,"trailing_bytes":mpeg.trailing_bytes})
    } else {
        let alac = source.audio.alac()?;
        json!({"codec":"alac","sha256":hex(&source.audio.sha256),"alac_config_sha256":hex(&alac.alac_config_sha256),
            "sample_sizes_sha256":hex(&alac.sample_sizes_sha256),"media_sha256":hex(&alac.media_sha256),
            "timescale":alac.timescale,"duration_ticks":alac.duration_ticks,"media_bytes":alac.media_bytes})
    };
    let facts = InitialFacts {
        reference: expected,
        content: initial.content.clone(),
        insertion: attempt.id,
        root: root.clone(),
        base: base.clone(),
        relative_path: relative_path.to_owned(),
        options: crate::engine::planning::source_collection::SelectionOptions {
            recursive,
            include: include.iter().cloned().collect(),
            exclude: exclude.iter().cloned().collect(),
        },
    };
    Ok(RowInputs {
        projected: row["projected"].clone(),
        facts,
        receipt: json!({
            "schema":"rustunes.checked-historical-source-attribution","classification":"historical_attribution_candidate",
            "binding_id":expected.id.as_i64(),"active_binding_version":expected.version,"insertion_attempt":attempt.id.as_i64(),
            "insertion_receipt_sha256":hex(&initial.receipt_sha256),"insertion_anchor":initial.media_anchor,
            "active_receipt_sha256":hex(&active.receipt_sha256),"active_anchor":active.media_anchor,
            "journal_sha256":hex(&journal_sha),"producer_input_sha256":hex(&hash(input_bytes)),"plan_sha256":hex(&plan_sha),
            "producer_schema":input["schema"],"plan_media_anchor":plan["media_anchor"],"verified_prior_add_prefix":prefix,"source_root":root,"base_directory":base,"options":scope,
            "relative_path":relative_path,"historical_absolute_source_path":base.join(relative_path),"collection_sha256":hex(&collection_sha),
            "track_id":format!("{:016x}",initial.content.track_pid),"destination":initial.content.path,
            "immutable_physical_source":{"file_bytes":source.file_bytes,"full_file_sha256":hex(&source.full_file_sha256),"audio":audio},
            "physical_content_sha256":hex(&initial.content_sha256()?),"historical_source_opened":false,"retained_original_source_checked":original_source.is_some(),
            "latest_metadata_association_established":false,"desired_source_association_established":false,
            "mutation_authorized":false,"replacement_authorized":false,"start_authorized":false,"completion_authorized":false
        }),
    })
}

/// Reject duplicate keys at every level instead of accepting a last-key-wins
/// interpretation of a malformed historical producer or plan document.
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with unique keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(
                    serde_json::Number::from_f64(v)
                        .ok_or_else(|| E::custom("invalid JSON float"))?,
                )))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<UniqueJson>()? {
                    if values.len() >= 10000 {
                        return Err(serde::de::Error::custom("JSON array exceeds bound"));
                    }
                    values.push(v.0);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, v)) = map.next_entry::<String, UniqueJson>()? {
                    if values.len() >= 1024 || values.insert(key, v.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate or excessive JSON keys"));
                    }
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        d.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests;
