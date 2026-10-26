//! Human and machine views of the same freshly captured device catalogue.
use std::io::Write;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use music_protocol::catalogue::sqlite::{Catalog, Track};

#[derive(Default)]
pub struct Filter {
    pub query: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

pub enum View {
    List(Filter),
    Show(u64),
    Stats,
}

/// Fixed-width hexadecimal IDs remain strings in JSON, avoiding JavaScript's
/// integer precision limit and confusion with internal SQLite row identifiers.
pub fn parse_id(text: &str) -> Result<u64> {
    ensure!(
        text.len() == 16 && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "track ID must contain exactly 16 hexadecimal digits from library list"
    );
    let id = u64::from_str_radix(text, 16)?;
    ensure!(id != 0, "track ID must be nonzero");
    Ok(id)
}

pub fn write(
    catalog: &Catalog,
    view: &View,
    json_output: bool,
    out: &mut impl Write,
) -> Result<()> {
    let body = match view {
        View::Stats => stats(catalog)?,
        View::Show(id) => {
            let Some(track) = catalog.tracks().iter().find(|t| t.sync_id == *id) else {
                bail!("track {id:016x} is absent from the current catalogue");
            };
            json!({"track": track_json(catalog, track)})
        }
        View::List(filter) => {
            let mut tracks: Vec<_> = catalog
                .tracks()
                .iter()
                .filter(|t| matches(t, filter))
                .collect();
            tracks.sort_by(|a, b| {
                (
                    &a.metadata.artist,
                    &a.metadata.album,
                    a.metadata.disc_number,
                    a.metadata.track_number,
                    &a.metadata.title,
                    a.sync_id,
                )
                    .cmp(&(
                        &b.metadata.artist,
                        &b.metadata.album,
                        b.metadata.disc_number,
                        b.metadata.track_number,
                        &b.metadata.title,
                        b.sync_id,
                    ))
            });
            json!({"matched_tracks":tracks.len(), "total_tracks":catalog.tracks().len(),
                "tracks":tracks.into_iter().map(|t|track_json(catalog,t)).collect::<Vec<_>>()})
        }
    };
    if json_output {
        let report = json!({"schema":"rustunes.library",
            "media_anchor":catalog.persisted_media_anchor(),"catalog_database_sha256":hex(&catalog.source_hashes().database),
            "catalog_wal_sha256":catalog.source_hashes().wal.map(|v|hex(&v)),"data":body});
        serde_json::to_writer_pretty(&mut *out, &report)?;
        writeln!(out)?;
    } else {
        match view {
            View::List(_) => {
                writeln!(out, "ID                TITLE · ARTIST · ALBUM")?;
                for track in body["tracks"].as_array().expect("constructed array") {
                    writeln!(
                        out,
                        "{}  {} · {} · {}",
                        track["id"].as_str().unwrap(),
                        display(track["title"].as_str().unwrap()),
                        display(track["artist"].as_str().unwrap()),
                        display(track["album"].as_str().unwrap())
                    )?;
                }
                writeln!(
                    out,
                    "{} of {} tracks",
                    body["matched_tracks"], body["total_tracks"]
                )?;
            }
            View::Show(_) => {
                let t = &body["track"];
                for (label, key) in [
                    ("ID", "id"),
                    ("Title", "title"),
                    ("Sort title", "sort_title"),
                    ("Artist", "artist"),
                    ("Album", "album"),
                    ("Album artist", "album_artist"),
                    ("Genre", "genre"),
                    ("Composer", "composer"),
                    ("Copyright", "copyright"),
                    ("Comment", "comment"),
                    ("Year", "year"),
                    ("Track", "track_number"),
                    ("Track total", "track_count"),
                    ("Disc", "disc_number"),
                    ("Disc total", "disc_count"),
                    ("Duration (ms)", "duration_ms"),
                    ("Bytes", "file_bytes"),
                    ("Device path", "afc_path"),
                    ("Artwork registration", "artwork_cache_id"),
                    ("Content rating code", "content_rating"),
                    ("Compilation", "compilation"),
                    ("Play count", "play_count_user"),
                    ("Recent play count", "play_count_recent"),
                    ("Skip count", "skip_count_user"),
                    ("Recent skip count", "skip_count_recent"),
                ] {
                    let value = &t[key];
                    let text = value.as_str().map(display).unwrap_or_else(|| {
                        if value.is_null() {
                            "—".into()
                        } else {
                            value.to_string()
                        }
                    });
                    writeln!(out, "{label}: {text}")?;
                }
            }
            View::Stats => {
                writeln!(
                    out,
                    "{} tracks · {} albums · {} artists",
                    body["tracks"], body["albums_with_tracks"], body["artists_with_tracks"]
                )?;
                writeln!(
                    out,
                    "{} bytes · {} ms total duration",
                    body["file_bytes"], body["duration_ms"]
                )?;
                writeln!(
                    out,
                    "{} tracks have local artwork registrations",
                    body["tracks_with_artwork_registration"]
                )?;
                writeln!(out, "Media revision: {}", catalog.persisted_media_anchor())?;
            }
        }
    }
    Ok(())
}

fn matches(track: &Track, filter: &Filter) -> bool {
    let m = &track.metadata;
    filter
        .artist
        .as_ref()
        .is_none_or(|v| m.artist == *v || m.album_artist == *v)
        && filter.album.as_ref().is_none_or(|v| m.album == *v)
        && filter.query.as_ref().is_none_or(|v| {
            let query = v.to_lowercase();
            [&m.title, &m.artist, &m.album, &m.album_artist]
                .iter()
                .any(|s| s.to_lowercase().contains(&query))
        })
}

fn track_json(catalog: &Catalog, track: &Track) -> Value {
    let m = &track.metadata;
    json!({"id":format!("{:016x}",track.sync_id),"album_id":format!("{:016x}",track.album_sync_id),
        "artist_id":format!("{:016x}",track.artist_sync_id),"title":m.title,
        "sort_title":catalog.title_sort(track.sync_id),"artist":m.artist,"album":m.album,
        "album_artist":m.album_artist,"genre":m.genre,"composer":m.composer,"copyright":m.copyright,
        "comment":catalog.comment(track.sync_id),
        "track_number":m.track_number,"track_count":m.track_count,"disc_number":m.disc_number,"disc_count":m.disc_count,
        "year":m.year,"duration_ms":m.total_time_ms,"file_bytes":m.file_size,"afc_path":track.afc_path,
        "content_rating":m.content_rating,"content_rating_level":m.content_rating_level,"compilation":m.is_compilation,
        "play_count_user":track.playback.play_count_user,"play_count_recent":track.playback.play_count_recent,
        "skip_count_user":track.playback.skip_count_user,"skip_count_recent":track.playback.skip_count_recent,
        "has_been_played":track.playback.has_been_played,"user_rating":track.playback.user_rating,
        "user_rating_is_derived":track.playback.user_rating_is_derived,"liked_state":track.playback.liked_state,
        "artwork_cache_id":catalog.artwork().track_cache_ids().get(&track.sync_id),
        "availability":{"placeholder":track.availability.is_placeholder,"library_added":track.availability.is_library_added,
        "non_purgeable_asset":track.availability.has_non_purgeable_asset,"supported_type":track.availability.is_supported_type}})
}
fn stats(catalog: &Catalog) -> Result<Value> {
    let bytes = catalog.tracks().iter().try_fold(0u64, |sum, t| {
        sum.checked_add(t.metadata.file_size)
            .context("catalogue total file size overflows")
    })?;
    Ok(json!({"tracks":catalog.tracks().len(),
        "albums_with_tracks":catalog.albums().values().filter(|v|v.live_track_count>0).count(),
        "artists_with_tracks":catalog.artists().values().filter(|v|v.live_track_count>0).count(),
        "file_bytes":bytes,
        "duration_ms":catalog.tracks().iter().map(|t|u64::from(t.metadata.total_time_ms)).sum::<u64>(),
        "tracks_with_artwork_registration":catalog.artwork().track_cache_ids().len()}))
}
fn display(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_width_ids_preserve_high_bits_and_reject_ambiguous_decimal_inputs() {
        assert_eq!(parse_id("ffffffffffffffff").unwrap(), u64::MAX);
        for text in [
            "42",
            "-1",
            "0x1000000000000000",
            "0000000000000000",
            "gggggggggggggggg",
        ] {
            assert!(parse_id(text).is_err());
        }
        assert_eq!(display("name\x1b[31m\n"), "name\\u{1b}[31m\\n");
    }
    #[test]
    fn json_filters_and_missing_id_are_explicit() {
        let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
        let mut out = Vec::new();
        write(
            &catalog,
            &View::List(Filter {
                artist: Some("absent".into()),
                ..Filter::default()
            }),
            true,
            &mut out,
        )
        .unwrap();
        let json: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["data"]["matched_tracks"], 0);
        assert_eq!(json["data"]["total_tracks"], 1);
        assert_eq!(json["media_anchor"], 13);
        assert!(write(&catalog, &View::Show(42), false, &mut Vec::new()).is_err());
        write(&catalog, &View::Stats, true, &mut Vec::new()).unwrap();
    }
}
