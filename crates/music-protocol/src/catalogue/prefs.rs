//! The library-identity pair a computer writes to a phone it adopts:
//! `iTunes_Control/iTunes/iTunesPrefs` and `iTunesPrefs.plist`.
//!
//! The phone never reads either file (docs/10 §5). A computer
//! does: Apple Devices compares the library persistent ID at offset 12 with
//! its own and, when they differ, treats the phone as another library's
//! (docs/10 §6.2). Writing the pair is what keeps a later Apple Devices
//! connection from adopting the phone silently and replacing its music.
//!
//! The image is the one Apple Devices 1.1540.24088.0 wrote when it adopted a
//! phone with Music sync off (docs/10 §8.2): its default image (docs/10
//! §5.1) with `configured`, the two identity components, a copy of the
//! library ID at 0x60, the user and computer names, and four flag bytes.

use anyhow::{Result, ensure};
use plist::{Dictionary, Value};

use super::identity::{self, LibraryIdentity};

const PREFS_BYTES: usize = 1232;
const NAME_BYTES: usize = 64;
const USER_NAME: &str = "rustunes";
/// Where the computer name starts in the image; nothing before it depends
/// on the name.
const HOST_NAME_AT: usize = 0x1c0;

/// Bytes Apple Devices' adoption image holds besides the magic, version,
/// identity and names.
const FLAGS: [(usize, u8); 26] = [
    (0x08, 0x01), // configured
    (0x0b, 0x01),
    (0x23, 0x01),
    (0x30, 0x01),
    (0x35, 0x02),
    (0x4b, 0x01),
    (0x4c, 0x01),
    (0x59, 0x01),
    (0x5b, 0x04),
    (0x68, 0x02),
    (0x7a, 0x01),
    (0x99, 0x02),
    (0x9d, 0x01),
    (0x9e, 0x01),
    (0xa9, 0x01),
    (0xab, 0x01),
    (0xae, 0x01),
    (0xf4, 0xff),
    (0x10f, 0x01),
    (0x117, 0x0f),
    (0x11d, 0x01),
    (0x11f, 0x01),
    (0x124, 0x01),
    (0x125, 0x01),
    (0x126, 0x80),
    (0x06, 0x19), // minor version 25
];

/// The lists Apple Devices' plist carries next to `iPodPrefs`, all empty for
/// a phone with nothing synced.
const ID_LISTS: [&str; 18] = [
    "AudiobookPlaylistIDs",
    "AudiobookTrackIDs",
    "BookTrackIDs",
    "LibraryBookTrackIDs",
    "MoviePlaylistIDs",
    "MovieTrackIDs",
    "MusicAlbumIDs",
    "MusicArtistIDs",
    "MusicGenreIDs",
    "MusicPlaylistIDs",
    "MusicTrackIDs",
    "PodcastChannelIDs",
    "PodcastPlaylistIDs",
    "PodcastTrackIDs",
    "TVShowAlbumIDs",
    "TVShowPlaylistIDs",
    "TVShowSeriesIDs",
    "TVShowTrackIDs",
];

/// No Debug: both files carry the library identity.
pub struct Prefs {
    pub raw: Vec<u8>,
    pub plist: Vec<u8>,
}

impl Prefs {
    /// The identity both files carry, checked as every reader checks it.
    pub fn identity(&self) -> Result<LibraryIdentity> {
        let raw = identity::parse(&self.raw)?;
        raw.ensure_matches(&identity::parse(&self.plist)?)?;
        Ok(raw)
    }
}

/// The pair for a library whose persistent ID is `catalog_id`, on a computer
/// whose persistent user ID is `user_id`, named `host_name`. Their XOR is the
/// `LibraryID` sessions send.
pub fn adoption(catalog_id: u64, user_id: u64, host_name: &str) -> Result<Prefs> {
    ensure!(
        catalog_id != 0 && user_id != 0 && catalog_id ^ user_id != 0,
        "library identity components and their XOR must be nonzero"
    );
    ensure!(
        !host_name.is_empty()
            && host_name.len() < NAME_BYTES
            && host_name.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
        "host name must be 1..=63 printable ASCII characters"
    );
    let mut raw = vec![0; PREFS_BYTES];
    raw[..4].copy_from_slice(b"frpd");
    raw[4] = 0x01;
    for (at, value) in FLAGS {
        raw[at] = value;
    }
    raw[0x0c..0x14].copy_from_slice(&catalog_id.to_le_bytes());
    raw[0x14..0x1c].copy_from_slice(&user_id.to_le_bytes());
    raw[0x60..0x68].copy_from_slice(&catalog_id.to_le_bytes());
    for (at, name) in [
        (0x180, USER_NAME),
        (HOST_NAME_AT, host_name),
        (0x240, USER_NAME),
        (0x280, host_name),
    ] {
        raw[at..at + name.len()].copy_from_slice(name.as_bytes());
    }
    // Apple's writer emits the keys sorted, as CFPropertyList does.
    let mut entries: Vec<(&str, Value)> = ID_LISTS
        .iter()
        .map(|key| (*key, Value::Array(Vec::new())))
        .collect();
    entries.push(("DevicesVersion", "1.6.4".into()));
    entries.push(("iPodPrefs", Value::Data(raw.clone())));
    entries.sort_unstable_by_key(|(key, _)| *key);
    let dictionary: Dictionary = entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    let mut plist = Vec::new();
    Value::Dictionary(dictionary).to_writer_xml(&mut plist)?;
    let prefs = Prefs { raw, plist };
    prefs.identity()?;
    Ok(prefs)
}

/// The pair `adoption` makes for `catalog_id` and `user_id` whose
/// `iTunesPrefs` is `bytes` or begins with them, whatever computer name it
/// was made with: a whole file, or what an interrupted write of one left.
/// None when no such pair exists.
pub fn adoption_starting(bytes: &[u8], catalog_id: u64, user_id: u64) -> Option<Prefs> {
    // The name runs to its NUL, or to the end of `bytes` when they stop
    // inside it. Bytes that stop before it fit every name; any one will do.
    let host_name = match bytes.get(HOST_NAME_AT..) {
        None | Some([]) => USER_NAME,
        Some(field) => {
            let field = &field[..field.len().min(NAME_BYTES)];
            let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
            std::str::from_utf8(&field[..end]).ok()?
        }
    };
    let prefs = adoption(catalog_id, user_id, host_name).ok()?;
    prefs.raw.starts_with(bytes).then_some(prefs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pair_parses_to_the_given_identity() {
        let prefs = adoption(0x0123_4567_89AB_CDEF, 0x1111_2222_3333_4444, "rustunes").unwrap();
        assert_eq!(prefs.identity().unwrap().library_id(), "10326745BA9889AB");
        assert_eq!(prefs.raw.len(), 1232);
    }

    #[test]
    fn the_raw_image_is_apples_adoption_image_with_these_values() {
        let prefs = adoption(0x1122_3344_5566_7788, 0x99AA_BBCC_DDEE_FF01, "host").unwrap();
        let raw = &prefs.raw;
        assert_eq!(
            &raw[..0x20],
            [
                0x66, 0x72, 0x70, 0x64, 0x01, 0x00, 0x19, 0x00, 0x01, 0x00, 0x00, 0x01, 0x88, 0x77,
                0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x01, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99,
                0x00, 0x00, 0x00, 0x00
            ]
        );
        assert_eq!(&raw[0x60..0x68], &raw[0x0c..0x14]);
        assert_eq!(&raw[0x180..0x189], b"rustunes\0");
        assert_eq!(&raw[0x1c0..0x1c5], b"host\0");
        assert_eq!(&raw[0x240..0x249], b"rustunes\0");
        assert_eq!(&raw[0x280..0x285], b"host\0");
        let set = raw
            .iter()
            .enumerate()
            .filter(|(at, byte)| {
                **byte != 0
                    && !(0x0c..0x1c).contains(at)
                    && !(0x60..0x68).contains(at)
                    && !(0x180..0x2c0).contains(at)
            })
            .count();
        // frpd (4), major (1), and the 26 flag bytes.
        assert_eq!(set, 4 + 1 + 26);
    }

    #[test]
    fn the_plist_has_apples_keys_in_order() {
        let prefs = adoption(1, 2, "rustunes").unwrap();
        let text = String::from_utf8(prefs.plist.clone()).unwrap();
        assert!(text.starts_with("<?xml"));
        let value = Value::from_reader_xml(prefs.plist.as_slice()).unwrap();
        let dictionary = value.as_dictionary().unwrap();
        let keys: Vec<_> = dictionary.keys().map(String::as_str).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        assert_eq!(keys.len(), 20);
        assert_eq!(dictionary["DevicesVersion"].as_string(), Some("1.6.4"));
        assert_eq!(
            dictionary["iPodPrefs"].as_data(),
            Some(prefs.raw.as_slice())
        );
    }

    #[test]
    fn a_whole_or_cut_short_image_names_its_pair_under_any_computer_name() {
        let prefs = adoption(0x1122, 0x3344, "Mac mini").unwrap();
        let whole = adoption_starting(&prefs.raw, 0x1122, 0x3344).unwrap();
        assert_eq!(whole.raw, prefs.raw);
        assert_eq!(whole.plist, prefs.plist);
        // Before the name, inside it, just after it, and every other length.
        for length in [
            0,
            12,
            HOST_NAME_AT,
            HOST_NAME_AT + 3,
            HOST_NAME_AT + 9,
            0x2c0,
        ] {
            let start = adoption_starting(&prefs.raw[..length], 0x1122, 0x3344).unwrap();
            assert!(start.raw.starts_with(&prefs.raw[..length]), "{length}");
        }
        assert!(
            (0..=prefs.raw.len()).all(|length| adoption_starting(
                &prefs.raw[..length],
                0x1122,
                0x3344
            )
            .is_some())
        );
        // Another identity, a changed byte, or a byte too many is no such pair.
        assert!(adoption_starting(&prefs.raw, 0x1122, 0x3345).is_none());
        let mut changed = prefs.raw.clone();
        changed[0x30] ^= 1;
        assert!(adoption_starting(&changed, 0x1122, 0x3344).is_none());
        let mut longer = prefs.raw.clone();
        longer.push(0);
        assert!(adoption_starting(&longer, 0x1122, 0x3344).is_none());
        let mut nameless = prefs.raw.clone();
        nameless[HOST_NAME_AT..HOST_NAME_AT + 8].fill(0);
        assert!(adoption_starting(&nameless, 0x1122, 0x3344).is_none());
    }

    #[test]
    fn refuses_zero_identities_and_unprintable_names() {
        assert!(adoption(0, 1, "x").is_err());
        assert!(adoption(1, 0, "x").is_err());
        assert!(adoption(7, 7, "x").is_err());
        assert!(adoption(1, 2, "").is_err());
        assert!(adoption(1, 2, "tab\there").is_err());
        assert!(adoption(1, 2, "é").is_err());
        assert!(adoption(1, 2, &"x".repeat(64)).is_err());
        assert!(adoption(1, 2, &"x".repeat(63)).is_ok());
    }
}
