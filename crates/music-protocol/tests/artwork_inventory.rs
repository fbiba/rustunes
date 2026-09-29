//! Public parser coverage; the original unit assertions run once in music-protocol.
use music_protocol::media::artwork_inventory;

#[test]
fn application_consumer_cannot_treat_truncated_artwork_as_an_empty_inventory() {
    for bytes in [b"".as_slice(), b"mhfd", &[0_u8; 64]] {
        assert!(artwork_inventory::parse(bytes).is_err());
    }
}
