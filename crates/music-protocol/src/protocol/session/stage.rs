//! Staging a Started metadata operation on the phone. The caller's journal says
//! whether a Started record exists; this checks that the record carries the
//! signature this session made for its exact bytes, and only then lifts the AFC
//! write lock and writes the operation pair. No `Transfer` writes before that.
use super::super::transfer::{METADATA, SIGNATURE, Transfer};
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
