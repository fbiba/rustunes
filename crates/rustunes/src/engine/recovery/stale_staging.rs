//! An operation a killed host left staged on the phone.
//!
//! A session stages its operation pair, then sends FinishedSyncingMetadata;
//! the phone takes the pair only on that message. A host killed between the
//! two leaves the pair there for good: the Started record cannot settle (the
//! phone could still apply it) and every later send refuses the full staging
//! folder. Every outgoing frame is traced before it is sent, so a send trace
//! without that message proves the phone was never told to apply the pair.
//! Then the pair, read back byte for byte against the journal's operation and
//! signature, is removed, and the record settles NotApplied.
use crate::{
    engine::execution::device,
    engine::execution::native_insert::{Control, create_trace},
    engine::recovery::retained::frame,
    engine::state::journal::Identity,
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::identity::LibraryIdentity;
use music_protocol::protocol::transfer::Transfer;
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

const STAGING: &str = "iTunes_Control/Sync/Media";
const PAIR: [&str; 2] = ["Sync_00000000.plist", "Sync_00000000.plist.cig"];
const FRAME_CAP: u64 = 16 * 1024 * 1024;

/// A Started record's operation pair and the trace of the session that
/// staged it.
pub(crate) struct Unfinished<'a> {
    pub send: &'a Path,
    pub operation: &'a [u8],
    pub signature: Option<[u8; 21]>,
}

/// Remove `staged`, the names listed in the staging folder, when they are
/// `record`'s pair and its session never sent the finish; then confirm the
/// folder empty. The connection is traced under `run/staged-removal`.
/// Returns the names removed: none when nothing is staged. Refuses
/// otherwise, before removing anything.
pub(crate) async fn remove_unfinished(
    identity: &Identity,
    library: &LibraryIdentity,
    run: &Path,
    staged: &[String],
    record: Unfinished<'_>,
    control: &Control,
) -> Result<Vec<String>> {
    if staged.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        staged.iter().all(|name| PAIR.contains(&name.as_str())),
        "a staged file is not an operation pair"
    );
    let signature = record
        .signature
        .context("the record holds no signature to check the pair against")?;
    ensure!(
        !finish_traced(record.send)?,
        "its session sent FinishedSyncingMetadata, so the phone may still apply it"
    );
    let trace = run.join("staged-removal");
    create_trace(&trace)?;
    let deadline = control
        .deadline
        .min(Instant::now() + Duration::from_secs(30));
    let provider = timeout_at(deadline, device::provider())
        .await
        .context("discovery deadline")??;
    ensure!(
        Identity::new(&provider.udid, library)?.device_digest() == identity.device_digest(),
        "discovered device differs from the journal"
    );
    // Locked: it can remove only the files it reads back as this pair.
    let mut afc = Transfer::connect_bounded(
        &provider,
        &trace,
        deadline,
        24,
        Arc::clone(&control.cancelled),
    )
    .await?;
    let removed = afc
        .remove_staged_operation(record.operation, &signature)
        .await?;
    let left: Vec<String> = afc
        .names(STAGING)
        .await?
        .into_iter()
        .filter(|name| name != "." && name != "..")
        .collect();
    ensure!(
        left.is_empty(),
        "staging still holds {} after the pair was removed",
        left.join(", ")
    );
    Ok(removed
        .iter()
        .map(|path| path.rsplit('/').next().unwrap_or(path).to_owned())
        .collect())
}

/// Whether the session traced in `send` sent FinishedSyncingMetadata. It must
/// have sent RequestingSync, or it is not the session that was Started.
fn finish_traced(send: &Path) -> Result<bool> {
    let commands = outgoing_commands(send)?;
    ensure!(
        commands.iter().any(|command| command == "RequestingSync"),
        "the send trace holds no RequestingSync, so it is not the Started session's"
    );
    Ok(commands
        .iter()
        .any(|command| command == "FinishedSyncingMetadata"))
}

/// The command of every outgoing frame traced in `send`, in sequence order.
/// A frame is traced before it is sent, so every frame sent is here; one
/// that does not parse may be any message, cut off, and refuses.
pub(crate) fn outgoing_commands(send: &Path) -> Result<Vec<String>> {
    let entries =
        fs::read_dir(send).with_context(|| format!("reading the send trace {}", send.display()))?;
    let mut frames = Vec::new();
    for (count, entry) in entries.enumerate() {
        ensure!(count < 4096, "send trace entry bound exceeded");
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.ends_with("-outgoing.atc"))
        else {
            continue;
        };
        let name = name.to_owned();
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file() && meta.len() <= FRAME_CAP,
            "an outgoing frame in the send trace is not a bounded regular file"
        );
        let frame = frame(&fs::read(&path)?)
            .with_context(|| format!("an outgoing frame does not parse: {}", path.display()))?;
        let command = frame
            .as_dictionary()
            .and_then(|root| root.get("Command"))
            .and_then(|command| command.as_string())
            .with_context(|| format!("an outgoing frame names no command: {}", path.display()))?;
        frames.push((name, command.to_owned()));
    }
    frames.sort();
    Ok(frames.into_iter().map(|(_, command)| command).collect())
}

#[cfg(test)]
mod tests;
