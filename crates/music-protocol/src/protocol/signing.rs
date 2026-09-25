//! Candidate metadata signatures for one published, fixed host request.
//!
//! This is a table-free reconstruction using standard cryptographic primitives.
//! It does not support the different host request seen in the native reference
//! and does not establish that an iPhone will accept this request or signature.

use aes::{
    Aes128,
    cipher::{BlockDecryptMut, BlockEncrypt, KeyInit, KeyIvInit, block_padding::NoPadding},
};
use anyhow::{Result, ensure};
use hmac::{Hmac, Mac};
use sha1::Sha1;

pub const MAX_OPERATION_BYTES: usize = 4 * 1024 * 1024;

/// The complete request is part of the supported contract, not a fresh host ID.
pub const HOST_REQUEST: [u8; 84] = [
    0x01, 0x01, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
    0x11, 0x11, 0x04, 0x40, 0xbc, 0x27, 0x85, 0xe0, 0xdb, 0xf1, 0x66, 0x36, 0x1e, 0x07, 0x98, 0x0a,
    0x5e, 0xa4, 0x8d, 0xba, 0x95, 0xb3, 0xb8, 0xea, 0x26, 0x5d, 0x62, 0xae, 0xfe, 0xa5, 0x1b, 0xb7,
    0xb1, 0x90, 0xe0, 0xb7, 0x71, 0x26, 0x29, 0x0a, 0xd3, 0x9b, 0xb1, 0x3f, 0xec, 0xc0, 0x8c, 0x25,
    0xa9, 0x56, 0x1c, 0x51, 0x7a, 0xc1, 0x1e, 0x64, 0x90, 0x5d, 0xa0, 0x29, 0xe6, 0x1b, 0xdf, 0xd0,
    0xba, 0x22, 0xc3, 0x13,
];

const WRAP_KEY: [u8; 16] = [
    0x77, 0x76, 0xeb, 0x98, 0xab, 0x6c, 0xe6, 0x02, 0xe8, 0xe8, 0x92, 0xe4, 0x1b, 0xa7, 0xe9, 0x15,
];
const KEY_XOR: [u8; 16] = [
    0xc6, 0xc7, 0xc4, 0xc5, 0xca, 0xcb, 0xc8, 0xc9, 0xce, 0xcf, 0xcc, 0xcd, 0xb2, 0xb3, 0xb0, 0xb1,
];
const TAG_XOR: [u8; 20] = [
    0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff, 0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xe8,
    0xe9, 0xea, 0xeb, 0xec,
];

/// No Debug implementation: derived session material must not enter diagnostics.
pub struct FixedHostSession {
    key: [u8; 16],
    tag_mask: [u8; 20],
}

fn decode_reply(request: &[u8], reply: &[u8]) -> Result<[u8; 64]> {
    ensure!(request == HOST_REQUEST, "unsupported Grappa host request");
    ensure!(
        reply.len() == 83,
        "Grappa reply must contain exactly 83 bytes"
    );
    // These checks intentionally narrow the public algorithm to the observed
    // outer shape. They do not authenticate the reply or validate its plaintext.
    ensure!(
        reply[..2] == [1, 1] && reply[18] == 64,
        "unsupported Grappa reply envelope"
    );
    let mut decoded: [u8; 64] = reply[19..83].try_into()?;
    cbc::Decryptor::<Aes128>::new_from_slices(&[0x95; 16], &reply[2..18])
        .map_err(|_| anyhow::anyhow!("invalid fixed Grappa cipher dimensions"))?
        .decrypt_padded_mut::<NoPadding>(&mut decoded)
        .map_err(|_| anyhow::anyhow!("invalid Grappa ciphertext length"))?;
    Ok(decoded)
}

impl FixedHostSession {
    pub fn from_reply(request: &[u8], reply: &[u8]) -> Result<Self> {
        let decoded = decode_reply(request, reply)?;
        let mut key = [0_u8; 16];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = decoded[1 + i] ^ decoded[39 + i] ^ KEY_XOR[i];
        }
        Aes128::new(&WRAP_KEY.into()).encrypt_block((&mut key).into());
        let mut tag_mask = [0_u8; 20];
        for (i, byte) in tag_mask.iter_mut().enumerate() {
            *byte = decoded[18 + i] ^ TAG_XOR[i];
        }
        Ok(Self { key, tag_mask })
    }

    /// Sign the exact serialized operation bytes without parsing or rewriting.
    pub fn sign(&self, operation: &[u8]) -> Result<[u8; 21]> {
        ensure!(
            !operation.is_empty() && operation.len() <= MAX_OPERATION_BYTES,
            "operation length must be between 1 byte and 4 MiB"
        );
        let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("invalid Grappa HMAC key length"))?;
        mac.update(operation);
        let tag = mac.finalize().into_bytes();
        let mut sidecar = [0_u8; 21];
        sidecar[0] = 1;
        for (i, byte) in sidecar[1..].iter_mut().enumerate() {
            *byte = tag[i] ^ self.tag_mask[i];
        }
        Ok(sidecar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Value;

    fn vectors() -> Vec<Value> {
        Value::from_reader(std::io::Cursor::new(include_bytes!(
            "../../tests/fixtures/grappa-fixed-vectors.plist"
        )))
        .unwrap()
        .into_array()
        .unwrap()
    }

    fn bytes<'a>(vector: &'a Value, name: &str) -> &'a [u8] {
        vector.as_dictionary().unwrap()[name].as_data().unwrap()
    }

    #[test]
    fn independently_verified_vectors_match_each_stage() {
        let vectors = vectors();
        assert_eq!(vectors.len(), 4);
        for vector in vectors {
            let request = bytes(&vector, "request");
            let reply = bytes(&vector, "reply");
            assert_eq!(request, HOST_REQUEST);
            assert_eq!(
                decode_reply(request, reply).unwrap(),
                bytes(&vector, "decoded")
            );
            let session = FixedHostSession::from_reply(request, reply).unwrap();
            assert_eq!(session.key, bytes(&vector, "key"));
            assert_eq!(
                session.sign(bytes(&vector, "operation")).unwrap(),
                bytes(&vector, "sidecar")
            );
        }
    }

    #[test]
    fn refuses_other_requests_including_native_mode_one() {
        let vector = &vectors()[0];
        let reply = bytes(vector, "reply");
        for i in 0..HOST_REQUEST.len() {
            let mut request = HOST_REQUEST;
            request[i] ^= 1;
            assert!(FixedHostSession::from_reply(&request, reply).is_err());
        }
        let mut native_mode = HOST_REQUEST;
        native_mode[18] = 1;
        assert!(FixedHostSession::from_reply(&native_mode, reply).is_err());
        assert!(FixedHostSession::from_reply(&HOST_REQUEST[..83], reply).is_err());
    }

    #[test]
    fn refuses_unsupported_reply_shapes_without_panicking() {
        let vector = &vectors()[0];
        let reply = bytes(vector, "reply");
        for length in [0, 1, 2, 18, 19, 64, 82, 84, 4096] {
            let mut malformed = reply.to_vec();
            malformed.resize(length, 0);
            assert!(FixedHostSession::from_reply(&HOST_REQUEST, &malformed).is_err());
        }
        for i in [0, 1, 18] {
            let mut malformed = reply.to_vec();
            malformed[i] ^= 1;
            assert!(FixedHostSession::from_reply(&HOST_REQUEST, &malformed).is_err());
        }
    }

    #[test]
    fn bounds_operation_and_preserves_exact_bytes() {
        let vector = &vectors()[0];
        let session = FixedHostSession::from_reply(&HOST_REQUEST, bytes(vector, "reply")).unwrap();
        assert!(session.sign(&[]).is_err());
        assert!(session.sign(&vec![0; MAX_OPERATION_BYTES + 1]).is_err());
        assert!(session.sign(&vec![0; MAX_OPERATION_BYTES]).is_ok());
        let original = session.sign(b"<plist/>\n").unwrap();
        assert_ne!(original, session.sign(b"<plist/>").unwrap());
        assert_ne!(original, session.sign(b"<plist/>\r\n").unwrap());
        assert_ne!(original, session.sign(b"<plist/>\n\0").unwrap());
    }
}
