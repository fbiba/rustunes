use super::*;
pub(super) struct NewValues<'a> {
    pub id: i64,
    pub groups: [i64; 3],
    pub base: i64,
    pub file: &'a str,
    pub track: u64,
    pub bytes: u64,
    pub maps: [(i64, i64); 3],
    pub dates: [f64; 2],
    pub integrity: &'a [u8],
}
pub(super) fn new_rows(
    a: &Snapshot,
    w: &Wire,
    v: NewValues<'_>,
) -> Result<BTreeMap<&'static str, Row>> {
    let mut rows = BTreeMap::new();
    for name in TARGET_TABLES {
        let t = a.table(name)?;
        let mut r = vec![Cell::Integer(0); t.columns.len()];
        set(t, &mut r, "item_pid", Cell::Integer(v.id))?;
        // Exact typed native defaults for the observed ordinary local ALAC
        // profile. Unlisted columns stay integer zero, never copied from after.
        let (nulls, empty) = match name {
            "chapter" => ("chapter_data", ""),
            "item_extra" => (
                "comment grouping description description_long collection_description copyright",
                "version display_version classical_work classical_movement",
            ),
            "item_playback" => ("data_url eq_preset format", ""),
            "item_state" => (
                "database_id contained_persistent_id person_id cloud_album_id cloud_artist_id recommendation_id social_profile_id lyrics_adam_id radio_station_string_id radio_station_hash radio_station_event_id container_unique_id handoff_correlation_id content_item_id lyrics_id vendor_id opaque_id version_hash",
                "universal_cloud_library_id match_redownload_params purchase_history_redownload_params",
            ),
            "item_stats" => ("download_identifier", ""),
            "item_store" => (
                "purchase_history_redownload_params match_redownload_params external_guid feed_url artwork_url store_xid store_flavor",
                "extras_url vpp_org_name sync_redownload_params radio_station_id advertisement_unique_id cloud_album_id cloud_universal_library_id store_canonical_id tv_show_canonical_id tv_season_canonical_id immersive_deep_link_url",
            ),
            "item_video" => (
                "hls_offline_playback_keys",
                "episode_id network_name extended_content_rating movie_info audio_track_locale episode_type_display_name hls_playlist_url hls_key_server_url hls_key_cert_url hls_key_server_protocol",
            ),
            "lyrics" => ("", "lyrics"),
            _ => ("", ""),
        };
        for k in nulls.split_whitespace() {
            set(t, &mut r, k, Cell::Null)?;
        }
        for k in empty.split_whitespace() {
            set(t, &mut r, k, Cell::Text(String::new()))?;
        }
        match name {
            "item" => {
                for (k, n) in [
                    ("media_type", 8),
                    ("title_order", v.maps[0].0),
                    ("title_order_section", v.maps[0].1),
                    ("item_artist_pid", v.groups[2]),
                    ("item_artist_order", v.maps[2].0),
                    ("item_artist_order_section", v.maps[2].1),
                    ("series_name_order_section", 27),
                    ("album_pid", v.groups[0]),
                    ("album_order", v.maps[1].0),
                    ("album_order_section", v.maps[1].1),
                    ("album_artist_pid", v.groups[1]),
                    ("album_artist_order", v.maps[2].0),
                    ("album_artist_order_section", v.maps[2].1),
                    ("composer_order_section", 27),
                    ("genre_order_section", 27),
                    ("disc_number", w.disc),
                    ("track_number", w.track),
                    ("episode_sort_id", w.track),
                    ("base_location_id", v.base),
                    ("keep_local", 1),
                    ("keep_local_status", 2),
                    ("in_my_library", 1),
                ] {
                    set(t, &mut r, k, Cell::Integer(n))?;
                }
                set(
                    t,
                    &mut r,
                    "date_downloaded",
                    Cell::Real(v.dates[1].to_bits()),
                )?;
            }
            "item_extra" => {
                for (k, s) in [
                    ("title", w.title.as_str()),
                    ("sort_title", w.title.as_str()),
                    ("location", v.file),
                ] {
                    set(t, &mut r, k, Cell::Text(s.into()))?;
                }
                for (k, n) in [
                    ("disc_count", w.discs),
                    ("track_count", w.tracks),
                    ("file_size", i64::try_from(v.bytes)?),
                    ("media_kind", 1),
                    ("location_kind_id", 26),
                ] {
                    set(t, &mut r, k, Cell::Integer(n))?;
                }
                set(
                    t,
                    &mut r,
                    "total_time_ms",
                    Cell::Real((w.millis as f64).to_bits()),
                )?;
                set(t, &mut r, "integrity", Cell::Blob(v.integrity.to_vec()))?;
            }
            "item_playback" => {
                for (k, n) in [
                    ("audio_format", 601),
                    ("bit_rate", 1411),
                    ("duration", w.ticks),
                    ("gapless_heuristic_info", 1),
                ] {
                    set(t, &mut r, k, Cell::Integer(n))?;
                }
                for (k, n) in [
                    ("sample_rate", 44100.0_f64),
                    ("start_time_ms", 0.0),
                    ("stop_time_ms", 0.0),
                ] {
                    set(t, &mut r, k, Cell::Real(n.to_bits()))?;
                }
            }
            "item_search" => {
                for (k, n) in [
                    ("search_title", v.maps[0].0),
                    ("search_album", v.maps[1].0),
                    ("search_artist", v.maps[2].0),
                    ("search_album_artist", v.maps[2].0),
                ] {
                    set(t, &mut r, k, Cell::Integer(n))?;
                }
            }
            "item_state" => {
                for (k, s) in [
                    ("title", w.title.as_str()),
                    ("location", v.file),
                    ("store_playlist_id", "0"),
                ] {
                    set(t, &mut r, k, Cell::Text(s.into()))?;
                }
                for (k, n) in [
                    ("persistent_id", v.id),
                    ("is_valid_content_type", 1),
                    ("is_library_added", 1),
                    ("keep_local_enable_state", 1),
                    ("keep_local_managed_status", 2),
                    ("has_non_purgeable_asset", 1),
                    ("media_type", 8),
                    ("is_supported_type", 1),
                ] {
                    set(t, &mut r, k, Cell::Integer(n))?;
                }
            }
            "item_stats" => {
                set(t, &mut r, "date_accessed", Cell::Real(v.dates[0].to_bits()))?;
                set(t, &mut r, "bookmark_time_ms", Cell::Real(0.0_f64.to_bits()))?;
            }
            "item_store" => {
                set(t, &mut r, "sync_id", Cell::Integer(v.track as i64))?;
                set(t, &mut r, "sync_in_my_library", Cell::Integer(1))?;
            }
            _ => {}
        }
        rows.insert(name, r);
    }
    Ok(rows)
}
