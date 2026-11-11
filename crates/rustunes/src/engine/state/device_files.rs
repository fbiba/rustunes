//! The files a computer's music library leaves on the phone and the phone's
//! own catalogue files, which rustunes reads, and the error that names one
//! when it is not there.
use std::fmt;

/// The library identity the last computer wrote (`frpd` 1.25, 1,232 bytes).
pub const PREFS: &str = "iTunes_Control/iTunes/iTunesPrefs";
/// The same identity bytes in a plist, with the computer's ID lists.
pub const PREFS_PLIST: &str = "iTunes_Control/iTunes/iTunesPrefs.plist";
/// Where Apple Devices and `rustunes init` write each prefs file before
/// renaming it into place; one is left only by a write that stopped.
pub const PREFS_TEMPORARY: &str = "iTunes_Control/iTunes/iTunesPrefs.tmp";
pub const PREFS_PLIST_TEMPORARY: &str = "iTunes_Control/iTunes/iTunesPrefs.plist.tmp";
/// The phone's own SQL catalogue of its library, and the write-ahead log
/// and shared-memory index SQLite keeps beside it.
pub const DATABASE: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb";
pub const DATABASE_WAL: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal";
pub const DATABASE_SHM: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb-shm";
/// The computer's saved catalogue (anchors and database PID).
pub const CDB: &str = "iTunes_Control/iTunes/iTunesCDB";
/// The computer's artwork image library (cache-ID counter).
pub const ARTWORK_DB: &str = "iTunes_Control/Artwork/ArtworkDB";

/// A file rustunes reads is not on the phone. `folder` names the outermost
/// folder on its path that is absent too, if any: `iTunes_Control/iTunes`
/// absent says more about the phone than its file absent.
#[derive(Debug)]
pub struct MissingDeviceFile {
    path: String,
    folder: Option<String>,
    /// For a file of the library identity pair: the other file is on the
    /// phone.
    other_half: bool,
}

impl MissingDeviceFile {
    pub fn new(path: &str, folder: Option<String>, other_half: bool) -> Self {
        Self {
            path: path.to_owned(),
            folder,
            other_half,
        }
    }

    /// A phone without the library identity has never been synced with a
    /// music library on a computer. One with half of it has been written to
    /// (an `init` that stopped, or a restore by Apple's host), and `init`
    /// takes only its own half.
    pub fn never_synced(&self) -> bool {
        other_half(&self.path).is_some() && !self.other_half
    }
}

impl fmt::Display for MissingDeviceFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.folder {
            Some(folder) => write!(
                f,
                "{} is not on the iPhone: the folder {folder} is absent",
                self.path
            )?,
            None => write!(f, "{} is not on the iPhone", self.path)?,
        }
        if self.never_synced() {
            f.write_str(
                ". This iPhone has never been synced with a music library: \
                 rustunes init prepares it (rustunes device check shows what it holds)",
            )?;
        } else if let Some(other) = other_half(&self.path).filter(|_| self.other_half) {
            write!(
                f,
                ". Only {other} of the library identity pair is there: \
                 rustunes device check says whether rustunes init can prepare this iPhone"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for MissingDeviceFile {}

/// The folders on `path`'s way, outermost first, without the file itself.
pub fn folders(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(at, _)| &path[..at])
}

/// The other file of the library identity pair, for a file of it.
pub fn other_half(path: &str) -> Option<&'static str> {
    match path {
        PREFS => Some(PREFS_PLIST),
        PREFS_PLIST => Some(PREFS),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_lists_each_parent_outermost_first() {
        assert_eq!(
            folders(PREFS).collect::<Vec<_>>(),
            ["iTunes_Control", "iTunes_Control/iTunes"]
        );
        assert_eq!(folders("file").count(), 0);
    }

    #[test]
    fn names_the_file_and_an_absent_folder() {
        let file = MissingDeviceFile::new(CDB, None, false);
        assert_eq!(
            file.to_string(),
            "iTunes_Control/iTunes/iTunesCDB is not on the iPhone"
        );
        assert!(!file.never_synced());
        let folder =
            MissingDeviceFile::new(ARTWORK_DB, Some("iTunes_Control/Artwork".into()), false);
        assert_eq!(
            folder.to_string(),
            "iTunes_Control/Artwork/ArtworkDB is not on the iPhone: the folder \
             iTunes_Control/Artwork is absent"
        );
    }

    #[test]
    fn missing_identity_says_the_phone_was_never_synced() {
        for path in [PREFS, PREFS_PLIST] {
            let missing = MissingDeviceFile::new(path, None, false);
            assert!(missing.never_synced());
            assert!(
                missing
                    .to_string()
                    .ends_with("never been synced with a music library: rustunes init prepares it (rustunes device check shows what it holds)"),
                "{missing}"
            );
        }
    }

    /// `init` refuses a phone with only the plist (what a restore by Apple's
    /// host can leave) and takes a lone `iTunesPrefs` only when it wrote it:
    /// half a pair points to `device check`, not to `init`.
    #[test]
    fn half_an_identity_points_to_device_check() {
        for (path, other) in [(PREFS, PREFS_PLIST), (PREFS_PLIST, PREFS)] {
            let missing = MissingDeviceFile::new(path, None, true);
            assert!(!missing.never_synced());
            assert_eq!(
                missing.to_string(),
                format!(
                    "{path} is not on the iPhone. Only {other} of the library identity pair \
                     is there: rustunes device check says whether rustunes init can prepare \
                     this iPhone"
                )
            );
        }
    }
}
