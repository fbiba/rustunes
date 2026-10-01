//! A catalogue as a session that removes some of its tracks leaves it, so
//! the rest of that session can be checked against it as against any prior
//! state.
use super::{Album, Artist, ArtworkInventory, Catalog};
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

impl Catalog {
    /// This catalogue without the tracks `removed`, as a session that deletes
    /// them leaves it: their rows, statistics, comments, title sorts, artist
    /// and composer references and track artwork registrations go, and each
    /// album and album artist counts one member fewer per removed track. A
    /// group no remaining track belongs to stays, as the phone keeps it in a
    /// session that also inserts (observed with 18 removals), unless `after`
    /// lacks it: the phone collects such groups in some sessions, and keeps a
    /// collected album's registration as an orphan or drops it. A removed
    /// track's cache ID leaves the namespace only when nothing else holds it
    /// (no cached object, no remaining registration) and `after` lacks it.
    /// Identity, anchor, source hashes and the groups no sync made stay this
    /// catalogue's own.
    pub fn without_tracks(&self, removed: &BTreeSet<u64>, after: &Catalog) -> Result<Catalog> {
        let tracks: Vec<_> = self
            .tracks
            .iter()
            .filter(|t| !removed.contains(&t.sync_id))
            .cloned()
            .collect();
        ensure!(
            tracks.len() + removed.len() == self.tracks.len(),
            "a removed track is not in the catalogue"
        );
        let kept = |track: &u64| !removed.contains(track);
        let keyed = |map: &BTreeMap<u64, [u8; 32]>| {
            map.iter()
                .filter(|(id, _)| kept(id))
                .map(|(id, v)| (*id, *v))
                .collect::<BTreeMap<_, _>>()
        };
        let track_item_artists: BTreeMap<u64, i64> = self
            .track_item_artists
            .iter()
            .filter(|(id, _)| kept(id))
            .map(|(id, artist)| (*id, *artist))
            .collect();
        let track_composers: BTreeMap<_, _> = self
            .track_composers
            .iter()
            .filter(|(id, _)| kept(id))
            .map(|(id, r)| (*id, r.clone()))
            .collect();

        let mut albums = BTreeMap::new();
        let mut collected_albums = BTreeMap::new();
        for (id, album) in &self.albums {
            let members: Vec<_> = tracks
                .iter()
                .filter(|t| t.album_sync_id == album.sync_id)
                .collect();
            let gone = self
                .tracks
                .iter()
                .filter(|t| t.album_sync_id == album.sync_id && removed.contains(&t.sync_id))
                .count();
            ensure!(
                members.len() + gone == album.live_track_count,
                "album {:016x} counts members the catalogue does not hold",
                album.sync_id
            );
            if members.is_empty() && !after.albums.contains_key(id) {
                collected_albums.insert(album.sync_id, *id);
                continue;
            }
            albums.insert(
                *id,
                Album {
                    live_track_count: members.len(),
                    has_compilation_tracks: members.iter().any(|t| t.metadata.is_compilation),
                    ..album.clone()
                },
            );
        }
        let mut artists = BTreeMap::new();
        for (id, artist) in &self.artists {
            let members = tracks
                .iter()
                .filter(|t| t.artist_sync_id == artist.sync_id)
                .count();
            let gone = self
                .tracks
                .iter()
                .filter(|t| t.artist_sync_id == artist.sync_id && removed.contains(&t.sync_id))
                .count();
            ensure!(
                members + gone == artist.live_track_count,
                "album artist {:016x} counts members the catalogue does not hold",
                artist.sync_id
            );
            if members == 0 && !after.artists.contains_key(id) {
                continue;
            }
            artists.insert(
                *id,
                Artist {
                    live_track_count: members,
                    ..artist.clone()
                },
            );
        }
        let used_artists: BTreeSet<i64> = track_item_artists.values().copied().collect();
        let item_artists = self
            .item_artists
            .iter()
            .filter(|(id, _)| used_artists.contains(id) || after.item_artists.contains_key(id))
            .map(|(id, row)| (*id, row.clone()))
            .collect();
        let used_composers: BTreeSet<i64> = track_composers
            .values()
            .filter_map(|r| r.lookup_id)
            .collect();
        let composers = self
            .composers
            .iter()
            .filter(|(id, _)| used_composers.contains(id) || after.composers.contains_key(id))
            .map(|(id, row)| (*id, row.clone()))
            .collect();

        let old = &self.artwork;
        let track_cache_ids: BTreeMap<u64, u32> = old
            .track_cache_ids
            .iter()
            .filter(|(id, _)| kept(id))
            .map(|(id, cache)| (*id, *cache))
            .collect();
        let mut album_cache_ids = old.album_cache_ids.clone();
        let mut orphan_album_cache_ids = old.orphan_album_cache_ids.clone();
        for (album, internal) in &collected_albums {
            if let Some(cache) = album_cache_ids.remove(album)
                && after.artwork.orphan_album_cache_ids.get(internal) == Some(&cache)
            {
                orphan_album_cache_ids.insert(*internal, cache);
            }
        }
        let held: BTreeSet<u32> = track_cache_ids
            .values()
            .chain(album_cache_ids.values())
            .chain(orphan_album_cache_ids.values())
            .chain(&old.cached_ids)
            .copied()
            .collect();
        let released: BTreeSet<u32> = old
            .track_cache_ids
            .iter()
            .filter(|(id, cache)| {
                removed.contains(id)
                    && !held.contains(cache)
                    && !after.artwork.occupied_cache_ids.contains(cache)
            })
            .map(|(_, cache)| *cache)
            .collect();
        let artwork = ArtworkInventory {
            occupied_cache_ids: old
                .occupied_cache_ids
                .difference(&released)
                .copied()
                .collect(),
            track_cache_ids,
            tracks_with_artwork_registration: old
                .tracks_with_artwork_registration
                .iter()
                .filter(|id| kept(id))
                .copied()
                .collect(),
            album_cache_ids,
            orphan_album_cache_ids,
            ..old.clone()
        };

        Ok(Catalog {
            source_hashes: self.source_hashes.clone(),
            user_version: self.user_version,
            database_pid: self.database_pid,
            primary_container_pid: self.primary_container_pid,
            anchor: self.anchor,
            albums,
            artists,
            unsynced_albums: self.unsynced_albums.clone(),
            unsynced_artists: self.unsynced_artists.clone(),
            composers,
            track_composers,
            title_sorts: self
                .title_sorts
                .iter()
                .filter(|(id, _)| kept(id))
                .map(|(id, v)| (*id, v.clone()))
                .collect(),
            comments: self
                .comments
                .iter()
                .filter(|(id, _)| kept(id))
                .map(|(id, v)| (*id, v.clone()))
                .collect(),
            item_artists,
            track_item_artists,
            stats_except_recent_play: keyed(&self.stats_except_recent_play),
            stats_except_recent_play_and_skip: keyed(&self.stats_except_recent_play_and_skip),
            stats_except_playback: keyed(&self.stats_except_playback),
            playback_dates: self
                .playback_dates
                .iter()
                .filter(|(id, _)| kept(id))
                .map(|(id, d)| (*id, *d))
                .collect(),
            represented_sync_ids: self
                .represented_sync_ids
                .iter()
                .filter(|id| kept(id))
                .copied()
                .collect(),
            music_folders: self.music_folders.clone(),
            tracks,
            artwork,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::catalogue::{artwork_catalog, modified_collection_catalog};
    use std::collections::BTreeSet;

    /// The fixture's one track, `item_pid` 1.
    const TRACK: u64 = -1i64 as u64;

    #[test]
    fn a_removed_track_takes_its_rows_and_leaves_its_groups_empty() {
        let before = artwork_catalog(500, 600);
        let projected = before
            .without_tracks(&BTreeSet::from([TRACK]), &before)
            .unwrap();
        assert!(projected.tracks().is_empty());
        assert!(projected.stats_except_playback().is_empty());
        assert!(projected.title_sort(TRACK).is_none() && projected.comment(TRACK).is_none());
        assert!(
            projected.track_composers().is_empty() && projected.track_item_artist(TRACK).is_none()
        );
        assert!(!projected.represented_sync_ids().contains(&TRACK));
        assert_eq!(projected.albums()[&31].live_track_count, 0);
        assert_eq!(projected.artists()[&21].live_track_count, 0);
        assert!(
            projected.item_artists().contains_key(&41) && projected.composers().contains_key(&71)
        );
        // The track's registration goes; its cached object keeps its ID, and
        // the album keeps its registration.
        assert!(projected.artwork().track_cache_ids().is_empty());
        assert!(projected.artwork().occupied_cache_ids().contains(&500));
        assert_eq!(
            projected
                .artwork()
                .album_cache_ids()
                .values()
                .collect::<Vec<_>>(),
            [&600]
        );
        assert_eq!(
            projected.persisted_media_anchor(),
            before.persisted_media_anchor()
        );
        // Removing a track the catalogue lacks refuses.
        assert!(
            before
                .without_tracks(&BTreeSet::from([7]), &before)
                .is_err()
        );
    }

    #[test]
    fn groups_the_phone_collected_leave_with_their_last_member() {
        let before = artwork_catalog(500, 600);
        let after = modified_collection_catalog(
            13,
            "DELETE FROM item WHERE item_pid=1;
            DELETE FROM item_extra WHERE item_pid=1;
            DELETE FROM item_store WHERE item_pid=1;
            DELETE FROM item_state WHERE item_pid=1;
            DELETE FROM item_stats WHERE item_pid=1;
            DELETE FROM album WHERE album_pid=31;
            DELETE FROM album_artist WHERE album_artist_pid=21;
            DELETE FROM item_artist WHERE item_artist_pid=41;
            DELETE FROM composer WHERE composer_pid=71;",
        );
        let projected = before
            .without_tracks(&BTreeSet::from([TRACK]), &after)
            .unwrap();
        assert!(!projected.albums().contains_key(&31) && !projected.artists().contains_key(&21));
        assert!(
            !projected.item_artists().contains_key(&41) && !projected.composers().contains_key(&71)
        );
        // Groups that kept or never had members stay.
        assert!(projected.albums().contains_key(&32) && projected.artists().contains_key(&22));
        assert!(projected.artwork().album_cache_ids().is_empty());
    }
}
