//! Staging a Started metadata operation on the phone. The caller's journal says
//! whether a Started record exists; this checks that the record carries the
//! signature this session made for its exact bytes, and only then lifts the AFC
//! write lock and writes the operation pair. No `Transfer` writes before that.
use super::super::transfer::{METADATA, SIGNATURE, Transfer, regular_size};
use super::*;
use std::io::Cursor;

/// A durable Started record's exact operation bytes and the signature this
/// session produced for them.
pub trait StartedOperation {
    fn operation(&self) -> &[u8];
    fn signature(&self) -> &[u8; 21];
}

impl Session {
    /// Unlock `afc`'s writes for `started`, then write METADATA and SIGNATURE.
    /// Refuses outside Ready, and refuses a signature this session did not make;
    /// both checks run before any packet is sent. The phase does not change:
    /// the caller checks its control, then `finish_metadata`.
    pub async fn stage_metadata(
        &mut self,
        afc: &mut Transfer,
        started: &impl StartedOperation,
    ) -> Result<()> {
        ensure!(
            self.phase == Phase::Ready,
            "metadata staging outside Ready state"
        );
        self.require_own_signature(started)?;
        afc.unlock_writes();
        put_verified(afc, METADATA, started.operation()).await?;
        put_verified(afc, SIGNATURE, started.signature()).await
    }

    /// Unlock a further connection for `started`. Refuses a signature this
    /// session did not make. Any phase.
    pub fn permit_writes(&self, afc: &mut Transfer, started: &impl StartedOperation) -> Result<()> {
        self.require_own_signature(started)?;
        afc.unlock_writes();
        Ok(())
    }

    /// Grappa signing is a deterministic HMAC, so this session reproduces its
    /// own signature for the record's bytes; another session's differs.
    fn require_own_signature(&self, started: &impl StartedOperation) -> Result<()> {
        ensure!(
            self.prepared.metadata_mode == MetadataMode::RevisionDelta,
            "a Started operation requires an ordinary metadata session"
        );
        ensure!(
            self.signer.sign(started.operation())? == *started.signature(),
            "Started signature was not made by this session"
        );
        Ok(())
    }
}

async fn put_verified(afc: &mut Transfer, path: &str, bytes: &[u8]) -> Result<()> {
    let expected: [u8; 32] = Sha256::digest(bytes).into();
    let uploaded = afc
        .write_new(path, &mut Cursor::new(bytes), bytes.len() as u64)
        .await?;
    ensure!(uploaded == expected, "metadata write digest differs");
    Ok(())
}

/// The operation pair a first session signs, as its own Started permit.
struct FirstStarted {
    operation: Vec<u8>,
    signature: [u8; 21],
}
impl StartedOperation for FirstStarted {
    fn operation(&self) -> &[u8] {
        &self.operation
    }
    fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}

const PREFS: &str = "iTunes_Control/iTunes/iTunesPrefs";
const PREFS_PLIST: &str = "iTunes_Control/iTunes/iTunesPrefs.plist";
/// Where `Transfer::write_new_through_temporary` writes each prefs file first.
const PREFS_TEMPORARY: &str = "iTunes_Control/iTunes/iTunesPrefs.tmp";
const PREFS_PLIST_TEMPORARY: &str = "iTunes_Control/iTunes/iTunesPrefs.plist.tmp";
const SYNC_MEDIA: &str = "iTunes_Control/Sync/Media";

/// The only files a first session removes: what an earlier attempt of it
/// may have left part-written.
const LEFTOVERS: [&str; 4] = [SIGNATURE, METADATA, PREFS_TEMPORARY, PREFS_PLIST_TEMPORARY];

impl Session {
    /// The rest of a phone's first session, from Ready.
    ///
    /// 1. Remove `leftovers`: what an earlier attempt of this first session
    ///    left, each path with the exact bytes that attempt was writing there
    ///    (its staged pair, its prefs temporaries). Each one present must hold
    ///    those bytes or the start of them, or nothing more is done. This
    ///    runs on the still-locked connection, at a Ready whose Media anchor
    ///    the phone reported as this session's (0 for a first session): the
    ///    phone applied no earlier attempt's pair.
    /// 2. Sign `operation` and let `persist` keep the signed pair, before
    ///    anything is written.
    /// 3. Under that Started permit, complete `prefs` when given: each file
    ///    absent is written through `.tmp`; one present must already hold
    ///    exactly its bytes, and is left as it is.
    /// 4. Create `iTunes_Control/Sync/Media` level by level where missing
    ///    (the phone never creates it itself, docs/02 §8.2),
    ///    require it empty, stage the pair, let `check` stop the session
    ///    before its point of no return, finish and wait for SyncFinished.
    pub async fn finish_first_session(
        &mut self,
        afc: &mut Transfer,
        operation: Vec<u8>,
        leftovers: &[(&str, &[u8])],
        prefs: Option<&crate::catalogue::prefs::Prefs>,
        persist: impl FnOnce(&[u8], &[u8; 21]) -> Result<()>,
        check: impl Fn() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            self.phase == Phase::Ready,
            "first session outside Ready state"
        );
        if let Some((path, _)) = leftovers.iter().find(|(path, _)| !LEFTOVERS.contains(path)) {
            bail!("{path} is not a file a first session leaves; nothing removed");
        }
        for (path, bytes) in leftovers {
            afc.remove_interrupted_write(path, bytes)
                .await
                .with_context(|| format!("removing {path}, left by an earlier attempt"))?;
        }
        let started = FirstStarted {
            signature: self.sign_metadata(&operation)?,
            operation,
        };
        persist(&started.operation, &started.signature)?;
        self.permit_writes(afc, &started)?;
        if let Some(prefs) = prefs {
            afc.ensure_directory("iTunes_Control").await?;
            afc.ensure_directory("iTunes_Control/iTunes").await?;
            for (path, bytes) in [(PREFS, &prefs.raw), (PREFS_PLIST, &prefs.plist)] {
                if !holds(afc, path, bytes).await? {
                    afc.write_new_through_temporary(path, bytes).await?;
                }
            }
        }
        afc.ensure_directory("iTunes_Control").await?;
        afc.ensure_directory("iTunes_Control/Sync").await?;
        afc.ensure_directory(SYNC_MEDIA).await?;
        ensure!(
            afc.names(SYNC_MEDIA)
                .await?
                .iter()
                .all(|name| name == "." || name == ".."),
            "{SYNC_MEDIA} is not empty; nothing staged"
        );
        self.stage_metadata(afc, &started).await?;
        check()?;
        self.finish_metadata().await?;
        self.wait_first_session().await
    }
}

/// Whether `path` is on the phone. If it is, it must hold exactly `bytes`.
async fn holds(afc: &mut Transfer, path: &str, bytes: &[u8]) -> Result<bool> {
    let Some(attributes) = afc.stat(path).await? else {
        return Ok(false);
    };
    let size = bytes.len() as u64;
    let other = || format!("{path} is on the phone with other contents; nothing written");
    regular_size(&attributes, size).with_context(other)?;
    ensure!(
        afc.read_hash(path, size, size.max(1)).await?[..] == Sha256::digest(bytes)[..],
        other()
    );
    Ok(true)
}
