//! Recover a candidate existing Music library identity from copied preferences.
//!
//! This module only parses bytes. The identity does not establish device
//! acceptance, a current Media anchor, or permission to replace library content.

use std::io::Cursor;

use anyhow::{Context, Result, anyhow, ensure};
use plist::Value;
use sha2::{Digest, Sha256};

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const PREFS_BYTES: usize = 1232;

/// The two persisted identity components and their reference-host XOR encoding.
/// Deliberately omits `Debug` to avoid including opaque identities in logs.
pub struct LibraryIdentity {
    catalog_id: u64,
    user_id: u64,
    library_id: String,
}

impl LibraryIdentity {
    pub fn library_id(&self) -> &str {
        &self.library_id
    }

    /// Stable local journal scope over both components, never the lossy XOR.
    /// This is a binding digest, not device acceptance or deletion authority.
    pub fn binding_sha256(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"music-protocol/library-binding\0");
        hash.update(self.catalog_id.to_le_bytes());
        hash.update(self.user_id.to_le_bytes());
        hash.finalize().into()
    }

    /// Reject conflicting copies, including different components with equal XOR.
    pub fn ensure_matches(&self, other: &Self) -> Result<()> {
        ensure!(
            self.catalog_id == other.catalog_id && self.user_id == other.user_id,
            "copied preferences contain conflicting library identities"
        );
        Ok(())
    }
}

/// Parse exact current-format raw iTunesPrefs or a plist dictionary's iPodPrefs.
/// Unsupported, incomplete, unconfigured, and zero identities are rejected.
pub fn parse(input: &[u8]) -> Result<LibraryIdentity> {
    ensure!(
        input.len() <= MAX_INPUT_BYTES,
        "copied preferences exceed the 1 MiB input limit"
    );

    let document;
    let raw = if input.starts_with(b"frpd") {
        input
    } else {
        document = Value::from_reader(Cursor::new(input)).map_err(|_| {
            anyhow!("copied preferences are neither raw frpd data nor a valid plist")
        })?;
        document
            .as_dictionary()
            .and_then(|dictionary| dictionary.get("iPodPrefs"))
            .and_then(Value::as_data)
            .context("copied preference plist must contain an iPodPrefs data value")?
    };

    ensure!(
        raw.len() == PREFS_BYTES,
        "raw preferences must contain exactly 1232 bytes"
    );
    ensure!(&raw[..4] == b"frpd", "raw preference magic is invalid");
    ensure!(
        u16::from_le_bytes([raw[4], raw[5]]) == 1 && u16::from_le_bytes([raw[6], raw[7]]) == 25,
        "raw preference version must be 1.25"
    );
    ensure!(raw[8] == 1, "raw preferences must be configured");

    // Pinned AMP's serializer copies handler+0xdef0 unchanged; its home-Music
    // setter stores these u64s at handler+0xdefc and handler+0xdf04.
    let catalog_id = u64::from_le_bytes(raw[12..20].try_into()?);
    let user_id = u64::from_le_bytes(raw[20..28].try_into()?);
    ensure!(
        catalog_id != 0 && user_id != 0,
        "persisted library identity components must be nonzero"
    );
    let combined_id = catalog_id ^ user_id;
    ensure!(combined_id != 0, "derived library identity must be nonzero");

    Ok(LibraryIdentity {
        catalog_id,
        user_id,
        library_id: format!("{combined_id:016X}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;

    fn raw(catalog_id: u64, user_id: u64) -> Vec<u8> {
        let mut bytes = vec![0; PREFS_BYTES];
        bytes[..4].copy_from_slice(b"frpd");
        bytes[4..6].copy_from_slice(&1_u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&25_u16.to_le_bytes());
        bytes[8] = 1;
        bytes[12..20].copy_from_slice(&catalog_id.to_le_bytes());
        bytes[20..28].copy_from_slice(&user_id.to_le_bytes());
        bytes
    }

    fn plist(value: Value, binary: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        if binary {
            value.to_writer_binary(&mut bytes).unwrap();
        } else {
            value.to_writer_xml(&mut bytes).unwrap();
        }
        bytes
    }

    fn wrapped(data: Vec<u8>, binary: bool) -> Vec<u8> {
        let mut dictionary = Dictionary::new();
        dictionary.insert("iPodPrefs".into(), Value::Data(data));
        // Other preference lists are unrelated to identity and are preserved by
        // the caller's copy; their presence must not change this calculation.
        dictionary.insert("MusicTrackIDs".into(), Value::Array(Vec::new()));
        plist(Value::Dictionary(dictionary), binary)
    }

    #[test]
    fn reads_little_endian_fields_and_formats_uppercase_padded_xor() {
        let identity = parse(&raw(0x1234, 0xab)).unwrap();
        assert_eq!(identity.library_id(), "000000000000129F");
        let identity = parse(&raw(0x0102_0304_0506_0708, 0x8070_6050_4030_2010)).unwrap();
        assert_eq!(identity.library_id(), "8172635445362718");
    }

    #[test]
    fn accepts_binary_and_xml_data_dictionaries() {
        let bytes = raw(0x1234, 0xab);
        let identity = parse(&bytes).unwrap();
        for binary in [true, false] {
            let parsed = parse(&wrapped(bytes.clone(), binary)).unwrap();
            assert_eq!(parsed.library_id(), "000000000000129F");
            identity.ensure_matches(&parsed).unwrap();
        }
    }

    #[test]
    fn rejects_short_and_extended_raw_or_embedded_data() {
        let bytes = raw(1, 2);
        for length in [0, 4, 8, 27, 28, PREFS_BYTES - 1, PREFS_BYTES + 1] {
            let mut malformed = bytes.clone();
            malformed.resize(length, 0);
            assert!(parse(&malformed).is_err(), "raw length {length}");
            assert!(
                parse(&wrapped(malformed, true)).is_err(),
                "data length {length}"
            );
        }
    }

    #[test]
    fn rejects_wrong_magic_in_raw_and_embedded_data() {
        let mut bytes = raw(1, 2);
        bytes[..4].copy_from_slice(b"drpf");
        assert!(parse(&bytes).is_err());
        assert!(parse(&wrapped(bytes, true)).is_err());
    }

    #[test]
    fn rejects_unsupported_versions() {
        for (major, minor) in [(0_u16, 25_u16), (2, 25), (1, 24), (1, 26)] {
            let mut bytes = raw(1, 2);
            bytes[4..6].copy_from_slice(&major.to_le_bytes());
            bytes[6..8].copy_from_slice(&minor.to_le_bytes());
            assert!(parse(&bytes).is_err(), "version {major}.{minor}");
        }
    }

    #[test]
    fn rejects_unconfigured_and_non_boolean_flags() {
        for configured in [0, 2, 255] {
            let mut bytes = raw(1, 2);
            bytes[8] = configured;
            assert!(parse(&bytes).is_err());
        }
    }

    #[test]
    fn rejects_zero_components_or_zero_xor() {
        for (catalog_id, user_id) in [(0, 2), (1, 0), (0, 0), (7, 7)] {
            assert!(parse(&raw(catalog_id, user_id)).is_err());
        }
    }

    #[test]
    fn rejects_wrong_plist_shape_and_data_type() {
        assert!(parse(&plist(Value::Array(Vec::new()), true)).is_err());
        assert!(parse(&plist(Value::Dictionary(Dictionary::new()), true)).is_err());
        let mut dictionary = Dictionary::new();
        dictionary.insert("iPodPrefs".into(), Value::String("frpd".into()));
        assert!(parse(&plist(Value::Dictionary(dictionary), true)).is_err());
    }

    #[test]
    fn checks_input_limit_before_attempting_plist_parse() {
        let error = parse(&vec![0; MAX_INPUT_BYTES + 1]).err().unwrap();
        assert_eq!(
            error.to_string(),
            "copied preferences exceed the 1 MiB input limit"
        );
    }

    #[test]
    fn rejects_conflicting_copies_even_when_xor_matches() {
        let first = parse(&raw(1, 2)).unwrap();
        let different = parse(&wrapped(raw(4, 8), true)).unwrap();
        assert!(first.ensure_matches(&different).is_err());
        let collision = parse(&wrapped(raw(4, 7), false)).unwrap();
        assert_eq!(first.library_id(), collision.library_id());
        assert!(first.ensure_matches(&collision).is_err());
    }

    #[test]
    fn journal_binding_matches_raw_and_plist_but_distinguishes_xor_collision() {
        let raw_identity = parse(&raw(1, 2)).unwrap();
        let plist_identity = parse(&wrapped(raw(1, 2), true)).unwrap();
        let collision = parse(&raw(4, 7)).unwrap();
        assert_eq!(
            raw_identity.binding_sha256(),
            plist_identity.binding_sha256()
        );
        assert_ne!(raw_identity.binding_sha256(), collision.binding_sha256());
        assert_ne!(
            raw_identity.binding_sha256(),
            Sha256::digest(raw_identity.library_id()).as_slice()
        );
    }
}
