//! Construct the qualified native Music request and completion templates from
//! the current identity and anchors.
//!
//! These are inputs to `session::prepare`, not ready-to-send frames.
//! Both templates carry the current Media anchor. The session checks a fresh
//! ReadyForSync and advances only the outgoing metadata completion to A + 1.

use anyhow::{Result, ensure};
use plist::Value;

use super::signing as grappa;
use crate::catalogue::identity::LibraryIdentity;

/// Qualify saved non-Media state from a bounded CDB/DB copy against the current
/// SQL database PID, then construct the request from current identity/anchor.
///
/// All three inputs must be captured on the same verified device/library. CDB
/// database PID and preference LibraryID are distinct identifiers: these bytes
/// alone cannot authenticate their association. The caller's stable capture
/// and full identity binding establish that association. The stale CDB Media
/// anchor is deliberately not substituted for the current SQL/Ready anchor.
pub fn build_from_copied_cdb(
    copied_cdb: &[u8],
    catalog: &crate::catalogue::sqlite::Catalog,
    identity: &LibraryIdentity,
    host_name: &str,
) -> Result<Templates> {
    let saved = crate::catalogue::anchor::parse(copied_cdb)?;
    ensure!(
        saved.database_pid() != 0 && saved.database_pid() == catalog.database_pid(),
        "saved CDB and current catalog database identities disagree"
    );
    build(
        identity,
        host_name,
        catalog.persisted_media_anchor(),
        saved.saved_voice_memo_anchor(),
    )
}

/// The only non-Media anchor qualified by the closed native and Linux sessions.
pub const QUALIFIED_VOICE_MEMO_ANCHOR: u32 = 1;

/// Transport-independent inputs for the existing bounded session machinery.
/// No Debug implementation: request values contain the private library identity.
pub struct Templates {
    pub request: Value,
    pub finished: Value,
}

/// Construct the observed iTunes compatibility profile for an existing library.
///
/// `identity` and `media_anchor` must come from the caller's fresh, reconciled
/// device snapshot. The explicit VoiceMemo value is retained unchanged; values
/// other than the qualified 1 are refused rather than silently overwritten.
/// `host_name` is supplied by the caller, never read from a captured host.
///
/// Version and WindowsOSVersion below are qualified protocol advertisements,
/// not local software/OS detection. Narrowing the requested dataclasses or
/// changing those advertisements requires a separate qualification.
///
/// This does not discover or authorize unavailable ancillary asset IDs. Those
/// remain a separate reviewed policy for the session's actual manifest.
pub fn build(
    identity: &LibraryIdentity,
    host_name: &str,
    media_anchor: u32,
    voice_memo_anchor: u32,
) -> Result<Templates> {
    ensure!(
        !host_name.trim().is_empty()
            && host_name.len() <= 1024
            && !host_name.chars().any(char::is_control),
        "host name must contain 1..=1024 bytes of non-control text"
    );
    ensure!(
        media_anchor.checked_add(1).is_some(),
        "Media anchor has no next revision"
    );
    ensure!(
        voice_memo_anchor == QUALIFIED_VOICE_MEMO_ANCHOR,
        "unqualified VoiceMemo anchor; preserve and review the current non-Media state"
    );

    let anchors = dict([
        ("Media", media_anchor.to_string().into()),
        ("VoiceMemo", voice_memo_anchor.to_string().into()),
    ]);
    let host = dict([
        ("Grappa", Value::Data(grappa::HOST_REQUEST.to_vec())),
        ("Version", "13.2.4.123".into()),
        ("LibraryID", identity.library_id().into()),
        ("SyncHostName", host_name.into()),
        ("SyncedDataclasses", strings(&["Media"])),
        ("WindowsOSVersion", "10.0".into()),
        ("Type", "iTunes".into()),
        ("Wakeable", 0u64.into()),
        ("SyncedAssetTypes", strings(&["Music"])),
    ]);
    let request = dict([
        ("Session", 2u64.into()),
        ("Command", "RequestingSync".into()),
        (
            "Params",
            dict([
                ("HostInfo", host),
                ("DataclassAnchors", anchors.clone()),
                ("Dataclasses", strings(&["Media", "Book", "Keybag"])),
            ]),
        ),
    ]);
    let finished = dict([
        ("Session", 2u64.into()),
        ("Command", "FinishedSyncingMetadata".into()),
        (
            "Params",
            dict([
                ("DataclassAnchors", anchors),
                (
                    "SyncTypes",
                    dict([
                        ("Media", 1u64.into()),
                        ("Book", 0u64.into()),
                        ("Keybag", 1u64.into()),
                    ]),
                ),
            ]),
        ),
    ]);
    Ok(Templates { request, finished })
}

fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(key, value)| (String::from(key), value))
            .collect(),
    )
}

fn strings(entries: &[&str]) -> Value {
    Value::Array(entries.iter().map(|value| (*value).into()).collect())
}
