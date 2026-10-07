//! Cover proofs: what a verified readback measured on a track cover's
//! permanent original and its selected rendition, keyed by the stats both
//! files had. A later `verify` that finds both files with the same stats, and
//! the same rendition selected, reuses the proof instead of reading and
//! decoding them again, as audio reuses a verified copy. Entries live in
//! `cover-proofs.sqlite` beside the fingerprints, in the same scope and mode:
//! `Refresh` (full readback) reads every cover and rewrites its entry.
use super::*;
use crate::engine::state::cache_database;

const DATABASE: &str = "cover-proofs.sqlite";
const TABLE: &str = "CREATE TABLE proofs(device BLOB NOT NULL CHECK(length(device)=32), original_path TEXT NOT NULL CHECK(length(original_path) BETWEEN 1 AND 1024), original_bytes INTEGER NOT NULL CHECK(original_bytes>0), original_birthtime INTEGER NOT NULL, original_mtime INTEGER NOT NULL, original_sha256 BLOB NOT NULL CHECK(length(original_sha256)=32), derivative_path TEXT NOT NULL CHECK(length(derivative_path) BETWEEN 1 AND 1024), derivative_bytes INTEGER NOT NULL CHECK(derivative_bytes>=0), derivative_birthtime INTEGER NOT NULL, derivative_mtime INTEGER NOT NULL, derivative_sha256 BLOB NOT NULL CHECK(length(derivative_sha256)=32), width INTEGER NOT NULL, height INTEGER NOT NULL, rgb24_bytes INTEGER CHECK((rgb24_bytes IS NULL)=(rgb24_sha256 IS NULL)), rgb24_sha256 BLOB CHECK(length(rgb24_sha256)=32), stored INTEGER NOT NULL, last_seen INTEGER NOT NULL, hits INTEGER NOT NULL DEFAULT 0 CHECK(hits>=0), PRIMARY KEY(device,original_path)) STRICT, WITHOUT ROWID";

fn open(directory: &Path) -> Result<Connection> {
    cache_database::open(
        directory,
        &cache_database::Schema {
            label: "cover proof cache",
            file: DATABASE,
            // "ISCP" marks a rustunes cover proof cache database.
            application_id: 0x4953_4350,
            tables: &[("proofs", TABLE)],
        },
    )
}

/// A phone file's size and times, as one stat gives them. Unlike a fingerprint
/// key, an empty file counts: the phone leaves some renditions empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhoneStat {
    pub file_bytes: u64,
    pub birthtime: i64,
    pub mtime: i64,
}

/// What one verified readback measured on a cover's two files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoverProof {
    pub original_path: String,
    pub original: PhoneStat,
    pub original_sha256: [u8; 32],
    pub derivative_path: String,
    pub derivative: PhoneStat,
    pub derivative_sha256: [u8; 32],
    pub width: u16,
    pub height: u16,
    /// The decoded raster's size and SHA-256; None for an empty rendition.
    pub rgb24: Option<(u64, [u8; 32])>,
}

/// Whether any proof is kept for this original, whatever its stat: without
/// one, a reuse attempt need not stat the file. Only inside a reusing scope.
pub(crate) fn cover_proof_known(device: &[u8; 32], original_path: &str) -> bool {
    let Some(cache) = current().filter(Config::reuses) else {
        return false;
    };
    let Some(device) = cache.phone(device) else {
        return false;
    };
    let found = cache.with_in(&cache.shared.covers, open, false, |c| {
        Ok(c.query_row(
            "SELECT 1 FROM proofs WHERE device=?1 AND original_path=?2",
            params![&device[..], original_path],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    });
    match found {
        Ok(found) => found.unwrap_or(false),
        Err(error) => {
            cache.warn(&error);
            false
        }
    }
}

/// Record a proof whose readback, decode and closing state guard all passed.
pub(crate) fn cover_verified(device: &[u8; 32], proof: &CoverProof) {
    let Some(cache) = current() else { return };
    let Some(device) = cache.phone(device) else {
        return;
    };
    let written = cache.with_in(&cache.shared.covers, open, true, |c| {
        let now = unix_now();
        c.execute(
            "INSERT OR REPLACE INTO proofs(device,original_path,original_bytes,original_birthtime,original_mtime,original_sha256,derivative_path,derivative_bytes,derivative_birthtime,derivative_mtime,derivative_sha256,width,height,rgb24_bytes,rgb24_sha256,stored,last_seen)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)",
            params![
                &device[..],
                proof.original_path,
                i64::try_from(proof.original.file_bytes)?,
                proof.original.birthtime,
                proof.original.mtime,
                &proof.original_sha256[..],
                proof.derivative_path,
                i64::try_from(proof.derivative.file_bytes)?,
                proof.derivative.birthtime,
                proof.derivative.mtime,
                &proof.derivative_sha256[..],
                proof.width,
                proof.height,
                proof.rgb24.map(|(bytes, _)| i64::try_from(bytes)).transpose()?,
                proof.rgb24.as_ref().map(|(_, sha)| &sha[..]),
                now,
            ],
        )?;
        Ok(())
    });
    match written {
        Ok(Some(())) => {
            cache.shared.covers_stored.fetch_add(1, Ordering::Relaxed);
        }
        Ok(None) => {}
        Err(error) => cache.warn(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(size: u64, birthtime: i64, mtime: i64) -> Attributes {
        [
            ("st_ifmt", "S_IFREG".to_owned()),
            ("st_size", size.to_string()),
            ("st_birthtime", birthtime.to_string()),
            ("st_mtime", mtime.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }
}
