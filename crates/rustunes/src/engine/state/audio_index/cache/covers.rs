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
impl PhoneStat {
    pub(crate) fn new(attrs: &Attributes) -> Option<Self> {
        let number = |key: &str| -> Option<i64> { attrs.get(key)?.parse().ok() };
        if attrs.get("st_ifmt").map(String::as_str) != Some("S_IFREG") {
            return None;
        }
        Some(Self {
            file_bytes: u64::try_from(number("st_size")?).ok()?,
            birthtime: number("st_birthtime")?,
            mtime: number("st_mtime")?,
        })
    }
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

/// The proof last verified for this original at exactly this stat, if any.
/// Only inside a reusing scope; any trouble is a miss.
pub(crate) fn verified_cover(
    device: &[u8; 32],
    original_path: &str,
    original: &PhoneStat,
) -> Option<CoverProof> {
    let cache = current().filter(Config::reuses)?;
    let device = &cache.phone(device)?;
    let found = cache.with_in(&cache.shared.covers, open, false, |c| {
        let row = c
            .query_row(
                "SELECT original_sha256, derivative_path, derivative_bytes, derivative_birthtime, derivative_mtime, derivative_sha256, width, height, rgb24_bytes, rgb24_sha256 FROM proofs
                 WHERE device=?1 AND original_path=?2 AND original_bytes=?3 AND original_birthtime=?4 AND original_mtime=?5",
                params![
                    &device[..],
                    original_path,
                    i64::try_from(original.file_bytes)?,
                    original.birthtime,
                    original.mtime
                ],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                        r.get::<_, Option<Vec<u8>>>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((sha, path, bytes, birthtime, mtime, derivative_sha, width, height, rgb_bytes, rgb_sha)) =
            row
        else {
            return Ok(None);
        };
        let digest = |v: Vec<u8>| -> Result<[u8; 32]> {
            v.try_into()
                .map_err(|_| anyhow::anyhow!("cover proof digest is not 32 bytes"))
        };
        let rgb24 = match (rgb_bytes, rgb_sha) {
            (Some(bytes), Some(sha)) => Some((u64::try_from(bytes)?, digest(sha)?)),
            (None, None) => None,
            _ => bail!("cover proof raster is incomplete"),
        };
        Ok(Some(CoverProof {
            original_path: original_path.to_owned(),
            original: *original,
            original_sha256: digest(sha)?,
            derivative_path: path,
            derivative: PhoneStat {
                file_bytes: u64::try_from(bytes)?,
                birthtime,
                mtime,
            },
            derivative_sha256: digest(derivative_sha)?,
            width: u16::try_from(width)?,
            height: u16::try_from(height)?,
            rgb24,
        }))
    });
    let proof = match found {
        Ok(proof) => proof.flatten()?,
        Err(error) => {
            cache.warn(&error);
            return None;
        }
    };
    // Use is bookkeeping for pruning; failing to record it keeps the hit.
    let _ = cache.with_in(&cache.shared.covers, open, false, |c| {
        c.execute(
            "UPDATE proofs SET last_seen=?3, hits=hits+1 WHERE device=?1 AND original_path=?2",
            params![&device[..], original_path, unix_now()],
        )?;
        Ok(())
    });
    cache.shared.covers_reused.fetch_add(1, Ordering::Relaxed);
    Some(proof)
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

/// Remove proofs no lookup or write has touched since `cutoff`.
pub(super) fn prune(directory: &Path, cutoff: i64, dry_run: bool) -> Result<u64> {
    if !directory.join(DATABASE).exists() {
        return Ok(0);
    }
    let c = open(directory)?;
    Ok(if dry_run {
        c.query_row(
            "SELECT count(*) FROM proofs WHERE last_seen<?1",
            [cutoff],
            |r| r.get::<_, i64>(0),
        )? as u64
    } else {
        let removed = c.execute("DELETE FROM proofs WHERE last_seen<?1", [cutoff])? as u64;
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        removed
    })
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

    fn proof(rgb24: Option<(u64, [u8; 32])>) -> CoverProof {
        CoverProof {
            original_path: "iTunes_Control/iTunes/Artwork/Originals/ab/cd".into(),
            original: PhoneStat::new(&attrs(1000, 10, 20)).unwrap(),
            original_sha256: [1; 32],
            derivative_path: "iTunes_Control/iTunes/Artwork/Caches/556x556/ab/cd".into(),
            derivative: PhoneStat::new(&attrs(rgb24.map_or(0, |_| 500), 11, 21)).unwrap(),
            derivative_sha256: [2; 32],
            width: 556,
            height: 556,
            rgb24,
        }
    }

    #[tokio::test]
    async fn a_cover_proof_is_reused_only_at_the_stat_it_was_verified_at() {
        let dir = std::env::temp_dir().join(format!("cover-proofs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let device = [7; 32];
        let decoded = proof(Some((556 * 556 * 3, [3; 32])));
        let empty = CoverProof {
            original_path: "iTunes_Control/iTunes/Artwork/Originals/ef/01".into(),
            ..proof(None)
        };
        let reuse = Config::new(dir.clone(), Mode::Reuse);
        reuse.select(device, phone_key("phone"));
        scope(Some(reuse.clone()), async {
            assert!(verified_cover(&device, &decoded.original_path, &decoded.original).is_none());
            assert!(!cover_proof_known(&device, &decoded.original_path));
            cover_verified(&device, &decoded);
            cover_verified(&device, &empty);
            assert!(cover_proof_known(&device, &decoded.original_path));
            assert!(!cover_proof_known(&[8; 32], &decoded.original_path));
            let found = verified_cover(&device, &decoded.original_path, &decoded.original);
            assert_eq!(found.as_ref(), Some(&decoded));
            assert_eq!(
                verified_cover(&device, &empty.original_path, &empty.original).as_ref(),
                Some(&empty)
            );
            // Another stat, path or device is a miss.
            let moved = PhoneStat {
                mtime: 99,
                ..decoded.original
            };
            assert!(verified_cover(&device, &decoded.original_path, &moved).is_none());
            assert!(verified_cover(&device, "other", &decoded.original).is_none());
            assert!(verified_cover(&[8; 32], &decoded.original_path, &decoded.original).is_none());
        })
        .await;
        assert_eq!(reuse.shared.covers_reused.load(Ordering::Relaxed), 2);
        assert_eq!(reuse.shared.covers_stored.load(Ordering::Relaxed), 2);

        // Full readback never reuses, and still stores.
        let refresh = Config::new(dir.clone(), Mode::Refresh);
        refresh.select(device, phone_key("phone"));
        scope(Some(refresh.clone()), async {
            assert!(verified_cover(&device, &decoded.original_path, &decoded.original).is_none());
            assert!(!cover_proof_known(&device, &decoded.original_path));
            cover_verified(&device, &decoded);
        })
        .await;
        assert_eq!(refresh.shared.covers_stored.load(Ordering::Relaxed), 1);

        // A release that names the phone by another digest still finds its
        // proofs; the digest it no longer uses finds none.
        let renamed = Config::new(dir.clone(), Mode::Reuse);
        renamed.select([9; 32], phone_key("phone"));
        scope(Some(renamed), async {
            assert!(verified_cover(&[9; 32], &decoded.original_path, &decoded.original).is_some());
            assert!(verified_cover(&device, &decoded.original_path, &decoded.original).is_none());
        })
        .await;

        // Outside a scope nothing is read or written.
        assert!(verified_cover(&device, &decoded.original_path, &decoded.original).is_none());

        assert_eq!(prune(&dir, unix_now() + 1, true).unwrap(), 2);
        assert_eq!(prune(&dir, unix_now() + 1, false).unwrap(), 2);
        assert!(!reuse.shared.warned.load(Ordering::Relaxed));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_rendition_has_a_stat_but_a_directory_does_not() {
        assert_eq!(
            PhoneStat::new(&attrs(0, 1, 2)),
            Some(PhoneStat {
                file_bytes: 0,
                birthtime: 1,
                mtime: 2
            })
        );
        let mut directory = attrs(0, 1, 2);
        directory.insert("st_ifmt".into(), "S_IFDIR".into());
        assert!(PhoneStat::new(&directory).is_none());
        let mut timeless = attrs(5, 1, 2);
        timeless.remove("st_birthtime");
        assert!(PhoneStat::new(&timeless).is_none());
    }
}
