//! Saved plans are untrusted descriptions. Recompute every decision from the
//! complete snapshot, current sources and journal before accepting one.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::identity;
use serde_json::Value;
use sha2::{Digest, Sha256};

mod unique_json;

pub(super) fn unique_value(bytes: &[u8]) -> Result<Value> {
    Ok(serde_json::from_slice::<unique_json::UniqueJson>(bytes)?.0)
}

fn strings(value: &Value) -> Result<BTreeSet<String>> {
    let values = value.as_array().context("missing source selectors")?;
    ensure!(values.len() <= 4096, "source selector bound exceeded");
    let mut result = BTreeSet::new();
    for value in values {
        ensure!(
            result.insert(value.as_str().context("selector must be text")?.to_owned()),
            "duplicate source selector"
        );
    }
    Ok(result)
}

pub fn pid(value: &Value) -> Result<u64> {
    let text = value.as_str().context("track/group ID missing")?;
    ensure!(
        text.len() == 16
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "IDs must contain exactly 16 lowercase hexadecimal digits"
    );
    let id = u64::from_str_radix(text, 16)?;
    ensure!(id >= 1000, "reserved track/group ID");
    Ok(id)
}

pub(super) fn read_stable(path: &Path) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && before.len() <= 32 * 1024 * 1024,
        "plan must be a bounded nonsymlink regular file"
    );
    let mut file = File::open(path)?;
    let same = |after: fs::Metadata| {
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && after.is_file()
    };
    ensure!(same(file.metadata()?), "plan changed while opening");
    let mut bytes = Vec::new();
    (&mut file)
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len()
            && same(file.metadata()?)
            && same(fs::symlink_metadata(path)?),
        "plan changed while reading"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn saved_descriptions_reject_duplicate_json_keys_at_every_level() {
        for bytes in [
            br#"{"execution_authorized_by_plan":false,"execution_authorized_by_plan":false}"#.as_slice(),
            br#"{"sources":[{"description":{"execution_authorized":false,"execution_authorized":false}}]}"#.as_slice(),
            br#"{"sources":[{"description":{"changed_fields":[{"field":"year","field":"year"}]}}]}"#.as_slice(),
        ] {
            assert!(serde_json::from_slice::<unique_json::UniqueJson>(bytes).is_err());
        }
        assert!(
            serde_json::from_slice::<unique_json::UniqueJson>(
                br#"{"sources":[{"description":{"execution_authorized":false}}]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_noncanonical_ids_and_ambiguous_selectors() {
        for value in [
            json!("ABCDEF1234567890"),
            json!("1234"),
            json!("0000000000000001"),
            json!(42),
        ] {
            assert!(pid(&value).is_err());
        }
        assert_eq!(pid(&json!("ffffffffffffffff")).unwrap(), u64::MAX);
        assert!(strings(&json!(["a", "a"])).is_err());
        assert!(strings(&json!([1])).is_err());
    }
}
