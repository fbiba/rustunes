//! The sort names and comment an insert sends, chosen against the catalogue
//! it is sent to. The sender and the insert verifier both take them from here.
use anyhow::{Context, Result};

use crate::{catalogue::sqlite::Catalog, media::metadata::ScalarMetadata};

/// What an insert sends besides the names: Apple's `sort_*` fields and
/// `comment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presentation<'a> {
    pub sort_title: &'a str,
    pub sort_artist: &'a str,
    pub sort_album: &'a str,
    /// Sent only with an album artist.
    pub sort_album_artist: Option<&'a str>,
    /// Sent only with a composer.
    pub sort_composer: Option<&'a str>,
    pub comment: Option<&'a str>,
}

/// The presentation of `source` inserted into `before` under the album and
/// album artist the plan gave it. The phone keeps one sort name per group and
/// takes the last one sent, so a group the track joins is sent its stored
/// sort name and no other track's order changes: the album and album artist
/// the plan reuses by id, the track artist and composer the phone files it
/// under by name. A new group takes the track's own (its sort tag, or its
/// name without a leading article).
pub fn presentation<'a>(
    source: &ScalarMetadata<'a>,
    before: &'a Catalog,
    album_pid: u64,
    artist_pid: u64,
) -> Result<Presentation<'a>> {
    let album = before
        .albums()
        .values()
        .find(|album| album.sync_id == album_pid)
        .and_then(|album| album.sort_name.as_deref());
    let album_artist = before
        .artists()
        .values()
        .find(|artist| artist.sync_id == artist_pid)
        .and_then(|artist| artist.sort_name.as_deref());
    let composer = match source.composer {
        Some(name) => match before.composer_for_label(name)? {
            Some(row) => Some(
                row.sort_name
                    .as_deref()
                    .context("existing composer has no qualified sort label")?,
            ),
            None => source.composer_sort(),
        },
        None => None,
    };
    Ok(Presentation {
        sort_title: source.title_sort(),
        sort_artist: before
            .item_artist_sort_for_name(source.artist)?
            .unwrap_or_else(|| source.artist_sort()),
        sort_album: album.unwrap_or_else(|| source.album_sort()),
        sort_album_artist: source
            .album_artist_sort()
            .map(|own| album_artist.unwrap_or(own)),
        sort_composer: composer,
        comment: source.comment,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::metadata::default_sort;

    fn source<'a>(title: &'a str, artist: &'a str, album: &'a str) -> ScalarMetadata<'a> {
        ScalarMetadata {
            title,
            artist,
            album,
            album_artist: None,
            genre: None,
            composer: None,
            copyright: None,
            year: None,
            content_rating: None,
            compilation: None,
            track: None,
            track_total: None,
            disc: None,
            disc_total: None,
            comment: None,
            sort_title: None,
            sort_artist: None,
            sort_album: None,
            sort_album_artist: None,
            sort_composer: None,
        }
    }

    #[test]
    fn a_default_sort_name_leaves_out_a_leading_article() {
        for (name, sort) in [
            ("The Example Album", "Example Album"),
            ("An Example Title", "Example Title"),
            ("a day", "day"),
            ("THE  Second Album", "Second Album"),
            ("The", "The"),
            ("The ", "The "),
            ("Theory", "Theory"),
            ("Anna", "Anna"),
            ("Ädam", "Ädam"),
        ] {
            assert_eq!(default_sort(name), sort, "{name:?}");
        }
    }

    #[test]
    fn a_new_group_takes_the_tracks_sort_and_a_joined_one_keeps_its_own() {
        let catalog = crate::test_support::catalogue::modified_collection_catalog(
            12,
            "UPDATE album SET sort_album='Album, The' WHERE album_pid=31;
            UPDATE album_artist SET sort_album_artist='Artist, The' WHERE album_artist_pid=21;
            UPDATE item_artist SET sort_item_artist='Track Artist, The' WHERE item_artist_pid=41;
            UPDATE composer SET sort_composer='Composer, The' WHERE composer_pid=71;",
        );
        let mut new = source("The Title", "The New Artist", "The New Album");
        new.album_artist = Some("The New Artist");
        new.composer = Some("The New Composer");
        new.comment = Some("A comment");
        let fresh = presentation(&new, &catalog, 900, 800).unwrap();
        assert_eq!(
            fresh,
            Presentation {
                sort_title: "Title",
                sort_artist: "New Artist",
                sort_album: "New Album",
                sort_album_artist: Some("New Artist"),
                sort_composer: Some("New Composer"),
                comment: Some("A comment"),
            }
        );
        // Explicit sort tags replace the defaults.
        new.sort_title = Some("Title, The");
        new.sort_album_artist = Some("New Artist, The");
        let tagged = presentation(&new, &catalog, 900, 800).unwrap();
        assert_eq!(
            (tagged.sort_title, tagged.sort_album_artist),
            ("Title, The", Some("New Artist, The"))
        );
        // The album and album artist the plan reuses, and the track artist
        // and composer the phone files the track under, keep their sorts.
        let mut joining = source("Title", "SYNTHETIC ARTIST", "Synthetic Album");
        joining.album_artist = Some("Synthetic Artist");
        joining.composer = Some("Synthetic Composer");
        joining.sort_album = Some("Ignored");
        let joined = presentation(&joining, &catalog, (-31i64) as u64, (-21i64) as u64).unwrap();
        assert_eq!(
            joined,
            Presentation {
                sort_title: "Title",
                sort_artist: "Track Artist, The",
                sort_album: "Album, The",
                sort_album_artist: Some("Artist, The"),
                sort_composer: Some("Composer, The"),
                comment: None,
            }
        );
        // Without an album artist none is sent.
        assert_eq!(
            presentation(&source("T", "A", "B"), &catalog, 900, 800)
                .unwrap()
                .sort_album_artist,
            None
        );
    }
}
