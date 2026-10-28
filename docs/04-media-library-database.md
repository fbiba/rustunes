# The iPhone Music library database and the metadata mapping

This chapter describes how the stock iPhone Music app stores a synced library in `MediaLibrary.sqlitedb`, and how a track's metadata travels from file tags, through the fields of a sync operation, into database columns. It covers the identifiers that tie the layers together (persistent IDs), the rules by which the phone groups tracks into albums, artists, composers and genres, how a track's file is identified on the device, how artwork is registered and cached, and how play statistics and playback history behave across syncs. Every claim carries an evidence label. The findings come from disassembly of Apple's Windows sync client, from captured Apple traffic, and from repeated before/after comparisons of the phone's database across 45 tables.

## Contents

1. Scope and provenance
2. The database and how it was read
3. Identifiers (PIDs) and revisions
4. Table and column map
5. Metadata mapping: source tag to plist key to column
6. Comment and the five sort fields
7. Grouping rules
8. Editing an existing track
9. File identity and on-device paths
10. Artwork
11. Statistics and playback history
12. What survives deletion
13. Notes for implementers
14. Open questions
15. Evidence

## 1. Scope and provenance

**Phone.** iPhone17,2 (iPhone 16 Pro Max), iOS 27.0.1, build 24A446. All receiver observations come from this one device and one library of roughly 400 tracks (ALAC, plus a few MP3 and other formats added later).

**Host binaries analysed.** Apple Devices 1.1540.23042.0, whose `AMPDevicesAgent.exe` is version 1.6.4.90 ("old" below), and Apple Devices 1.1540.24088.0, whose agent is 1.6.4.94 ("new"). Where a function exists in both, addresses are given as old / new. Binaries are not redistributed; only addresses and findings are quoted. Other binaries named in the text are `MobileDevice.dll` 1827.100.17.2 and `AirTrafficHost.dll` 4025.400.1.5 (old package) and 4026.110.1.6 (new package). All virtual addresses are in the `0x1400xxxxx` image of the host agent unless stated otherwise. A bare "RVA" is the address minus the image base `0x140000000`.

**Method.** Three kinds of evidence were combined.

1. Static analysis of the host agent: sync operation builder, tag readers, grouping code.
2. Captures of Apple's own sync sessions (a 399-track reset sync and an incremental sync with one new track), used as the reference for what the host sends.
3. Controlled experiments from a Linux host that sent the same operation format to the phone and compared the complete database before and after, across all 45 tables, together with the write-ahead log (WAL). Section 15 lists the evidence by kind.

**Evidence labels.**

- CONFIRMED: seen in the phone database, on the wire, or in disassembled code or data flow.
- INFERRED: reasoned from confirmed facts but not seen directly.
- OPEN: not determined.

Where a later or explicitly corrected statement exists, it wins and the correction is noted.

**Related chapters.** Operation envelopes, signing and sync sessions are in [chapter 02](02-atc-sync-protocol.md). AFC file transfer, Wi-Fi and the `GetFileHash` problem are in [chapter 03](03-transport-usb-and-wifi.md). Deletion and removal-batch mechanics, and playlists, are in [chapter 05](05-receiver-operations.md). Audio format codes, `avformat_info` and gapless fields are in [chapter 06](06-audio-formats-and-gapless.md). Client-side policy built on these findings is in [chapter 07](07-sync-engine-design.md).

## 2. The database and how it was read

### 2.1 Location and companions

CONFIRMED. The library database is `iTunes_Control/iTunes/MediaLibrary.sqlitedb` on the phone. It is a WAL-mode SQLite file, so the authoritative state is the database file plus its `-wal` file. In one capture the database was 2,580,480 bytes, the WAL 4,037,632 bytes and the shared-memory file `-shm` 32,768 bytes.

Other files in the same area exist but are not authoritative:

- `iTunesCDB` and `ArtworkDB` are written by the Windows host as its own saved copy of the catalogue and of the artwork cache IDs. They are host-generated state. An unchanged `iTunesCDB` after an accepted insertion does not mean the insertion failed. CONFIRMED (the native writer `SaveIPodFilesFiber`, `0x4b4050`, option bit 22 selects the `iTunesCDB` name; phone-side CDB export is OPEN).
- `iTunesPrefs` holds library preferences and identity.

The host sync engine also reads `PlayCounts.plist` from the iTunes folder when present (consumer chain `0x489970 -> 0x3a7a60 -> 0x3a76ac`). Each `tracks[]` row carries `persistentID` plus play, skip, bookmark, rating, liked/played and deleted fields. Unknown PIDs are skipped, and the file is not a catalogue enumerator. CONFIRMED statically; no instance of this file on the phone was observed.

### 2.2 Read-only capture

CONFIRMED. The phone exposes the file over the AFC service. A reader copies `MediaLibrary.sqlitedb` and `MediaLibrary.sqlitedb-wal` to local scratch storage, records the stat attributes of the database, the WAL and the `-shm` file before and after the copy, and opens only the local copy with SQLite (read-only, WAL retained). `-shm` content is not copied. No SQLite connection is ever opened against the phone. A representative capture used 10 stat calls, 2 opens, 102 reads and 2 closes, a 32 MiB cap per file and a 120-second deadline. `PRAGMA integrity_check` returned `ok` on the copies.

This is a stable bounded read, not an atomic filesystem snapshot. A capture is accepted only when two consecutive reads agree on bytes and attributes (ignoring access time).

Observed drift cases, all CONFIRMED:

- The WAL can grow or be checkpointed between the initial stat and the copy. Two MP3 transfers each saw 8,240 bytes of WAL growth during the first post-transfer copy. One fresh read succeeded; a second drift should stop the capture.
- A capture may also see a WAL that became empty while the database kept its rows and schema (a checkpoint).
- With no sync running and no music playing, the phone renumbers all item revisions and removes statistics revision rows a few minutes after a sync (section 3.4). A capture that straddles this fails the stable-read check; retry after it settles.
- `MLArtistHeroImageImportDatabaseRevision` can catch up to the maximum entity revision between two captures with no other change (observed from 78535 to 79040, and from 74317 to 74325). It is background maintenance, not a sync effect: the phone's cloud service records its artist-image imports there (chapter 09, section 5.3).

### 2.3 The 45 tables

CONFIRMED. On iOS 27.0.1 the database holds 197 schema objects: 45 tables, 136 indexes and 16 triggers. Every receiver experiment compared all 45 tables (schema and typed row contents) before and after, and every operation type left a bounded, explainable set of changed tables. The schema was identical across all comparisons. iOS 27.2 adds one index, `AlbumCloudChannelName` on `album`, and changes no table or column (chapter 09, section 8.1). The earliest captures were counted as 44 tables and the rest as 45 for what appears to be the same schema; the reason for the difference was not recorded (OPEN). Section 4 lists the tables that sync operations were seen to touch, with their roles. The remaining tables never changed under any observed sync operation and are not described here.

### 2.4 Database properties

CONFIRMED. The `_MLDatabaseProperties` table holds key/value strings. Keys observed to change:

| Key | Behaviour |
| --- | --- |
| `MLSyncClientSyncAnchor` | Integer, +1 for each accepted sync session. It is the "Media anchor" the host sends in `RequestingSync` (see protocol chapter). |
| `MLSyncClientLastSyncedRevision` | Becomes the highest revision of the session's metadata phase. For a delete session it equals the highest tombstone or playlist revision. |
| `MLArtistHeroImageImportDatabaseRevision` | Follows the highest new revision after a session, and may catch up independently in the background. |
| `MLSyncClientGenerationID` | A new random signed 64-bit value after every session. Not derivable. |
| `MLMaintenanceTaskOperationDate` | Did not change during a 100-target removal session that also collected groups, so that collection belongs to the sync session and not to the maintenance task. Its role otherwise is OPEN. |

Every other property stayed exact across all experiments. Across an iOS update other properties move too, among them `MLCloudDatabaseUserVersion`, which belongs to the iCloud library client. Chapter 09, section 5 lists the version stamps and who writes them.

## 3. Identifiers (PIDs) and revisions

### 3.1 Wire identifiers and their database columns

CONFIRMED. A sync operation names objects by signed 64-bit persistent IDs. The plist writes them as CFNumber SInt64, so about half are negative; the asset manifest uses the bare signed decimal string of the track PID as `AssetID`.

| Wire field | Database column | Native source |
| --- | --- | --- |
| `pid` (operation root) | `item_store.sync_id` | Track qword at `+0` (`0x38f248` constructor) |
| `item.album_pid` | `album.sync_id` | Track `+0x28`, then album qword `+0x20` (RVA `0x463079-0x46308a`, new build) |
| `item.artist_pid` | `album_artist.sync_id` | Track `+0x40`, then artist qword `+0x18` (RVA `0x464137-0x46414b`, new build) |
| container `pid` | `container.sync_id` | playlist and database identity |
| `update_db_info.pid` and `db_info.primary_container_pid` | database and primary library container | unchanged "current" values must be echoed |

Three groups have no wire identity: `item_artist`, `composer` and `genre`. The receiver matches them by the grouping key of their name (section 7). The column `item_artist.sync_id` is not the same namespace as `artist_pid`; internal SQLite row IDs, `item_artist.sync_id` and source strings are not interchangeable with wire PIDs.

The phone also keeps its own internal item identifier (`item_pid`, the row key in `item`). The phone assigns it: new items took `max(item_pid) + 1, +2, +3` in operation order. Where earlier deleted items had left revision tombstones for those numbers, the rows were revived (`deleted` 2 to 0) with new revisions. The wire `pid` is stored as `item_store.sync_id`. Always join through `sync_id`; the internal `item_pid` changes meaning after deletes and refills.

CONFIRMED (one capture). The phone also makes rows of its own, outside any sync. An Apple Music track it had streamed, not in the library, appeared as an `item` with `item_store.sync_id` 0, a nonzero `remote_location_id`, base location 0, an empty `location`, `in_my_library` 0 and `is_subscription` 1. It was filed under a new `album` and `album_artist`, both with `sync_id` 0. Its track artwork was a store image (`artwork_source_type` 600), and its album also had a source-300 token. No sync operation can name such rows. A catalogue reader keeps them out of the library: it reads the groups apart and still counts their artwork IDs as taken. A sync never assigns 0, so sync ID 0 is what marks them. The first sync session after it appeared, a removal, wrote three columns of the entry's `item_state` row that were still at their schema default 0: `contained_persistent_id` and `lyrics_adam_id` became NULL and `is_valid_content_type` 1, as on every synced track. No trigger writes these columns, and nothing else about the entry changed. Session verifiers accept exactly that change, all three together, on a sync ID 0 entry. OPEN: whether the phone re-keys such a group when a sync names a new PID with its name, as it does an emptied group (section 3.2). A client should not add tracks to one.

### 3.2 Allocation

CONFIRMED statically. The host's track constructor (`0x38f248`) honours a supplied PID or calls allocator `0x354114` when it is zero. The album constructor (`0x2d3798`) does the same for the album PID at `+0x20`. The allocator uses a generic generator or catalogue counter and excludes unsigned values below 1000. This establishes how Apple allocates; it does not establish collision safety.

CONFIRMED by experiment. The phone accepts any unused signed 64-bit value from the host as a new track, album or album-artist `sync_id`. A fresh random 64-bit ID that no existing album uses worked for new albums (four metadata sessions). Tracks, albums and album artists named in one batch session can share a single reserved pair of IDs (two batch insert sessions).

Reusing an existing album or artist is done by naming its existing PID in the new track's `album_pid` and `artist_pid`. No separate create-album, create-artist or "insert_artist" operation is needed (static code can build `insert_artist` and `insert_album` for a Reset, but the captured reset staged only `insert_track` operations, one `insert_playlist` and `update_db_info`; an incremental insert carries only `insert_track`). CONFIRMED (the reset and delta reference sessions, 37 album groups with multiple inserts sharing an album PID).

Zero-match caveat. After deletions, a group can survive without members and with the same name. The phone matches groups by name, not by PID (section 7.1). A new PID with an emptied group's name rewrites that group's `sync_id` instead of creating a row.

### 3.3 Identity namespaces in a live library

CONFIRMED. In a snapshot of 401 tracks there were 286 referenced albums and 190 referenced album artists, with no ambiguous exact `(album, album_artist, is_compilation)` name key. That is a property of the snapshot, not a promise. All 400 album-artist PIDs from Apple's reset sync still joined `album_artist.sync_id`, but only 386 of the album PIDs still equalled the live `album.sync_id`: 14 had been regrouped by the receiver. Reuse decisions must be made from a fresh capture and actual joins, never from a host-saved catalogue or an old operation.

All 400 baseline track identities mapped to 400 distinct `item_store.sync_id` values.

### 3.4 Entity revisions

CONFIRMED. Every change is tracked in `entity_revision` with the tuple `(revision, entity_pid, deleted, class, revision_type)`.

- Classes observed: 0 items, 1 containers (playlists), 3 genres, 4 albums, 5 composers. Album artists and item artists have their own classes; their class numbers were not recorded.
- `revision_type` 0 is the live revision; `revision_type` 1 is a statistics or membership revision.
- `deleted` 2 marks a tombstone.
- Within a session, new revisions form one contiguous block directly above the previous maximum. Order for a metadata session: touched groups first, each once, by class (album artist, album, item artist, composer, genre), then the items in operation order. An item updated twice takes two numbers. For a delete session the item tombstones come first (in item-ID order, not operation order), then playlists, then collected groups by class.
- Every edited item gets a new item revision, every group the session touches a new revision of its class.
- Background renumbering: 0.5 to 2.5 minutes after a session, with no further sync and no playback, the phone gave all 399 items new live revisions (contiguously above the old maximum) and removed 336 of 342 statistics revision rows (the same effect was also seen on 443 items and 114 rows). Last-synced, anchor and every other table stayed as they were. Any state saved before such an event differs from a later capture in item revisions and statistics revision rows only. After the update to iOS 27.2 all 414 items got new live revisions in the same contiguous way (chapter 09, section 9).
- Playback on the phone between syncs changes play and skip counts, recent counters and dates and the statistics revisions. These changes are normal, not drift.

## 4. Table and column map

Columns below are those the observations name. Other columns exist.

### 4.1 Item tables

| Table | Columns named | Role |
| --- | --- | --- |
| `item` | `item_pid`, `title_order`, `title_order_section`, `item_artist_order(+_section)`, `album_order(+_section)`, `album_artist_order(+_section)`, `composer_order(+_section)`, `track_number`, `episode_sort_id`, `disc_number`, `is_compilation` | Main row. Order and section columns come from `sort_map`. |
| `item_extra` | `title`, `sort_title`, `comment`, `grouping`, `description`, `copyright`, `year`, `track_count`, `disc_count`, `content_rating`, `location` (file name), `base_location_id`, `location_kind_id`, file size, integrity, `total_time_ms`, `media_kind` | Text and file location. `media_kind` is 1 for songs. |
| `item_store` | `sync_id` | Wire PID for the item. |
| `item_state` | `title`, `has_explicit_content`, library-added, non-purgeable-asset flags | Per-item state. |
| `item_search` | `search_title`, `search_artist`, `search_album`, `search_album_artist`, `search_composer` | Search keys: `sort_map` rows of the plain names. |
| `item_playback` | `audio_format`, `duration`, `codec_type`, `codec_subtype`, `data_kind`, `format` | Audio descriptor. The last four are left 0 or NULL for every row sent by Apple and by the Linux host. |
| `item_stats` | `play_count_user`, `play_count_recent`, `skip_count_recent`, played and skipped dates, and further columns | Statistics (section 11). |
| `item_video`, `chapter`, `lyrics` | | Per-item rows created at insert, removed at delete. |

### 4.2 Group tables

| Table | Columns named | Notes |
| --- | --- | --- |
| `item_artist` | `item_artist`, `sort_item_artist`, `grouping_key`, `representative_item_pid`, `sync_id` | Per-track artist. No wire identity. |
| `album_artist` | `sync_id`, `album_artist`, `sort_album_artist`, `sort_order`, `sort_order_section`, `name_order`, `grouping_key`, `representative_item_pid` | Wire `artist_pid`. |
| `album` | `sync_id`, `album`, `sort_album`, `album_year`, `all_compilations`, `has_compilation_tracks`, `keep_local_status`, `representative_item_pid`, linked album artist | Wire `album_pid`. |
| `composer` | `composer`, `sort_composer`, `grouping_key`, `representative_item_pid` | No wire identity. |
| `genre` | name and grouping key, phone-allocated ID | No wire identity; revision class 3. |
| `sort_map` | `name` (unique), `name_order`, `name_section`, `sort_key` | One row per distinct plain name and per distinct sort string. |

### 4.3 Artwork tables

`artwork`, `artwork_token`, `best_artwork_token`. See section 10.

### 4.4 Containers

CONFIRMED (playlist creation and deletion). Operations and effects are in [chapter 05](05-receiver-operations.md). The container tables are:

| Table | Columns named | Notes |
| --- | --- | --- |
| `container` | `sync_id` (wire playlist PID), name, `date_created`, `date_modified`, `filepath`, `contained_media_type`, `keep_local_status`, source-remote, editable and saveable flags | One row per playlist and per built-in container. |
| `container_item` | `container_item_pid`, `item_pid`, `position`, `uuid`, `position_uuid`, `occurrence_id` | One row per member. `item_pid` is the internal item ID, not the wire PID. |
| `container_item_media_type` | container, media type, count | One row per playlist and media type. Type 8 is songs. |
| `container_seed` | seed row per container | An item-0, order-0 sentinel. |

Facts about a playlist created from the wire:

- The wire `insert_playlist` carries `container` (`name`, `date_created`, `date_modified`), `container_ui` and `item_to_container`, an ordered list of track wire PIDs (`item_store.sync_id`). The `container_ui` values sent were `play_order` 1, `album_field_order` 0, `repeat_mode` 0 and false for `is_reversed`, `shuffle_items` and `has_been_shuffled`. Where the phone stores them was not identified (OPEN).
- Members land at `position` 0, 1, 2 in list order, with empty `uuid` and `position_uuid` and `occurrence_id` of the form `<item_pid>_0`. A count row of type 8 and the seed sentinel are created with them.
- The defaults seen on a new playlist were source-remote 1, editable 0 and saveable 0. They are observed values and say nothing about cloud provenance.
- `container.filepath` is `iTunes_Control/iTunes/OnTheGoPlaylist_<unsigned pid>.plist`, with the PID spelled unsigned. The path is a column value only. A stat of the referenced file found it absent, so a SQL path does not prove a file exists.
- `contained_media_type` is 8 while the playlist holds songs and returns to 0 when its last member is removed. The emptied playlist stays.
- `keep_local_status` is 0 for a new playlist and becomes 2 when the next removal session evaluates it. Albums created by batch inserts start at 2 (section 7.6).
- The live revision (class 1) is written at the new last-synced revision and the membership revision a little above it (91631 and 91633 in one run).
- The wire PID is signed. It is negative whenever its unsigned spelling in the path is above 2^63.

The library holds three built-in containers: the primary library playlist, Photos Memories and Playback History (section 11.4). The constructor in the host agent can also emit smart-playlist, folder, cloud and artwork fields. Nothing in the observations shows the phone accepting them.

### 4.5 Bookkeeping

`entity_revision`, `_MLDatabaseProperties`, `sort_map`.

## 5. Metadata mapping: source tag to plist key to column

### 5.1 What the host sends

CONFIRMED from 400 unique `insert_track` item dictionaries in Apple's traffic (a 399-track reset sync, one incremental insert, and one MP3 insert; almost all ALAC). Key counts across the 400:

| Key | Count of 400 |
| --- | --- |
| `title`, `artist`, `album`, `sort_name`, `sort_artist`, `sort_album`, `album_pid`, `artist_pid`, `is_song`, `total_time_ms`, `date_created`, `date_modified`, `remember_bookmark`, `artwork_checksum` | 400 |
| `artwork_cache_id` | 399 |
| `year` | 396 |
| `track_number`, `disc_number` | 337 |
| `genre` | 336 |
| `composer`, `sort_composer` | 263 |
| `copyright` | 258 |
| `album_artist`, `sort_album_artist` | 66 |
| `lyrics` (a dict carrying only a checksum) | 34 |
| `is_compilation`, `comment`, `content_rating`, `track_count` | 2 each |

Never present: `grouping`, `description`, `sort_title`. The title sort key is `sort_name`.

Dictionary shape for an insert (the full envelope is in the protocol chapter):

```text
{ operation: "insert_track", pid: <signed64>,
  item: { title, artist, album, genre, album_artist, composer, comment, copyright,
          year, track_number, track_count, disc_number, disc_count,
          is_compilation, content_rating, is_song: true, total_time_ms,
          date_created, date_modified, remember_bookmark,
          sort_name, sort_album, sort_artist, sort_album_artist, sort_composer,
          album_pid, artist_pid, artwork_cache_id, artwork_checksum },
  item_stats: { play_count_user, play_count_recent, skip_count_*, date_played,
                date_skipped, has_been_played },
  avformat_info: { sample_rate, bit_rate, audio_format, duration, ... },
  location: { kind, ... } }
```

The builder is `0x140461900` (old) / `0x140461a24` (new). Two setter rules decide which keys appear:

- The string setter `0x1406992fc` (new `0x140699450`) writes nothing when the string is empty, unless the call is flagged "always set". An insert therefore omits empty strings.
- The numeric setter `0x14069950c` (new `0x140699660`) omits zero, unless "always set". An insert therefore omits zero year, track number and so on.
- When the builder is driven by a field mask (an update), selected fields are always set, so an empty string or a zero is sent.

### 5.2 Field mapping table

Source tags: MP4 `ilst` atoms for ALAC, AAC, HE-AAC and xHE-AAC; ID3v2 frames for MP3 (and for WAV and AIFF, where only an `ID3 ` chunk is read by the host agent). Evidence labels refer to the whole row.

| Field | MP4 source | ID3 source | Plist key | Database column(s) | Rule | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| Title | `©nam` | `TIT2` | `title` | `item_extra.title`, `item_state.title`; search: `item_search.search_title` | First value only | CONFIRMED |
| Title sort | `sonm` | `TSOT` | `sort_name` | `item_extra.sort_title`, `item.title_order(+_section)` | Section 6 | CONFIRMED |
| Artist | `©ART` | `TPE1` | `artist` | `item_artist` group; `item_search.search_artist` | First value; scalar display string | CONFIRMED |
| Album | `©alb` | `TALB` | `album` | `album.album` group; `item_search.search_album` | First value; grouping needs `album_pid` | CONFIRMED |
| Album artist | `aART` | `TPE2` | `album_artist` | `album_artist` group; `item_search.search_album_artist` | Sent only when the source has one (66 of 400) | CONFIRMED |
| Composer | `©wrt` | `TCOM` | `composer` | `composer` group; `item_search.search_composer` | Sent only when present (263 of 400) | CONFIRMED |
| Genre | `©gen` (numeric `gnre` fallback) | `TCON` | `genre` | `genre` group | Sent only when present (336 of 400) | CONFIRMED |
| Grouping | `©grp` | `GRP1` | not sent | `item_extra.grouping` stays NULL | Apple never sends it | CONFIRMED |
| Description | `desc` | `TDES` | not sent | `item_extra.description` stays NULL | Apple never sends it | CONFIRMED |
| Comment | `©cmt` | `COMM`, empty description | `comment` | `item_extra.comment` | Section 6 | CONFIRMED |
| Copyright | `cprt` | (not examined) | `copyright` | `item_extra.copyright` | NULL when not sent; `""` on update gives `''` | CONFIRMED |
| Year | `©day` (year part only) | `TDRC` | `year` | `item_extra.year` | Year only: source `2026-03-19` sent year 2026, no release date field | CONFIRMED |
| Track number and total | track tuple | `TRCK` | `track_number`, `track_count` | `item.track_number`, `item.episode_sort_id`, `item_extra.track_count` | Zero is omitted on insert | CONFIRMED |
| Disc number and total | disc tuple | `TPOS` | `disc_number`, `disc_count` | `item.disc_number`, `item_extra.disc_count` | Phone stores disc 1 when an insert names none (every track on the phone has disc 1, including 62 whose source had no disc tag) | CONFIRMED |
| Compilation | `cpil` | (not examined) | `is_compilation` (bool) | `item.is_compilation` | Sent only when true (2 of 400); section 7.3 | CONFIRMED for MP4 |
| Content rating | `rtng` (type 21, 1 byte) | | `content_rating` | `item_extra.content_rating`, `item_state.has_explicit_content` | Unsigned; zero omitted on insert; value 1 is explicit | CONFIRMED |
| Lyrics | `©lyr` | `USLT` | `lyrics` (dict with checksum only) | `lyrics` | Text not sent in the item; separate asset part | CONFIRMED (34 of 400) |
| Cover art | `covr` | `APIC` | `artwork_cache_id`, `artwork_checksum` | `artwork`, `artwork_token`, `best_artwork_token` | Section 10 | CONFIRMED |
| Duration | audio properties | audio properties | `total_time_ms` | `item_extra.total_time_ms` | `floor(duration_samples * 1000 / sample_rate)` for all 399 ALAC inserts (only 230 match nearest rounding) | CONFIRMED |
| Media kind | | | `is_song: true` | `item_extra.media_kind` 1 | Item flags come from the media-kind mask (`0x140462b1b-0x140462d56`); songs send only `is_song` | CONFIRMED |
| Dates | | | `date_created`, `date_modified` | item date columns | Omitted when the host value is zero; all closed Apple inserts carry both; the default policy is not proven | CONFIRMED / OPEN |
| Bookmark | | | `remember_bookmark` | | Boolean, sent for all 400 | CONFIRMED |
| Artist PID | | | `artist_pid` | `album_artist.sync_id` | Section 3 | CONFIRMED |
| Album PID | | | `album_pid` | `album.sync_id` | Section 3 | CONFIRMED |

The MP4 text reader is `0x7182b0 -> 0x718180 -> 0x717d08` and requests one-based data child index 1. It serves `©nam`, `©ART`, `aART`, `©wrt`, `©alb`, `©grp`, `©gen`, `©day` and `©cmt`; the extended reader also uses it for `cprt`. The numeric `gnre` fallback, numeric tuples, artwork arrays and freeform atoms have separate handling.

Frame ids written by Apple's ID3 writer (`0x1403bf318`, new `0x1403bf314`): `TIT2`, `TPE1`, `TPE2`, `TCOM`, `TALB`, `GRP1`, `TIT1`, `MVNM`, `MVIN`, `TRCK`, `TPOS`, `RVA2`, `TDRC`, `TBPM`, `TCON`, `COMM`, `TIT3`, `TCAT`, `TDES`, `TSOT`, `TSOA`, `TSOP`, `TSO2`, `TSOC`, `USLT`, plus v2.2 variants. The ID3 reader is `0x1403be164` (new `0x1403be160`); the MP4 reader is `0x1403c54e0` and `0x1403c5c0c` (new `0x1403c54dc`, `0x1403c5c08`).

### 5.3 First-value projection of multi-value tags

CONFIRMED statically. The native reader takes the first data child of an MP4 atom (ordinal 1). It does not join values with a separator. A file with one `©ART` atom holding three UTF-8 data children `["Artist M, Artist N", "Artist M", "Artist N"]` projects to `Artist M, Artist N` with no suffix. ID3 follows the same single-value rule: when several empty-description `COMM` frames exist the later one overwrites the earlier (no guard seen).

CONFIRMED: a physical multi-value tag never reaches the phone as a list. The plist carries a scalar string.

INFERRED/OPEN: the static ordinal-1 read has not been verified by running Apple's importer on a multi-value file. Duplicate standard atom containers and malformed values should be rejected, not resolved by guessing.

**Vorbis comments (FLAC): repeated artists and composers are joined.** This is a host rule, not Apple behaviour. The host agent has no Vorbis parser (section 6.5), so no Apple reader decides what a repeated Vorbis field becomes. A Vorbis comment block may repeat a field name, and real FLACs hold one `ARTIST` entry per artist and one `COMPOSER` entry per composer: in one library five files had two to four composers each, and six had two or three artists each (CONFIRMED).

- The Linux sender takes the distinct `ARTIST` entries in file order and joins them with ", " into the one `artist` it sends, and the distinct `COMPOSER` entries into the one `composer`. Entries `Artist A`, `Artist B`, `Artist A` give `Artist A, Artist B`.
- Repeated `ARTISTSORT` and `COMPOSERSORT` entries are joined the same way. Without one, the sort name is derived from the joined name (section 6.7).
- An empty entry is no entry: it is dropped before the join, as Apple's omit-empty string setter sends no empty string, and a field left with no entry is absent (a required one is missing). Each remaining entry must be a valid value on its own (no control characters, at most 4,096 bytes). The joined value passes the same checks as a single value. This holds for every value the host reads from a tag as text, in every source format, an ALAC's included: an empty value is the same as no value, so a tag emptied is planned as a tag removed. Empty values drop out before a field's own rule: an ALAC's value is its first data child that is not empty, another format's field keeps one value, and a track or disc number or total, or a compilation flag, written as empty text is none. An MP4 atom whose first data child is empty and a later one is not therefore gives the later one, where the native reader's ordinal-1 read gives an empty string, which the setter omits (INFERRED).
- A single entry is sent as it is, including any separator inside it: `Artist A;Artist B` stays one value (section 5.4).
- Every other repeated Vorbis field is refused, the album artist included, and so is any repeated field in an MP4, ID3, RIFF INFO or AIFF tag. The plan names the field and how many values the file's tag holds, and the tag has to be edited.

CONFIRMED on the phone: five such FLACs, sent as ALAC, were each stored with the joined name as their one composer; each insert's catalogue check compared it. Six FLACs with repeated `ARTIST` entries, sent as ALAC in one batch, were each stored with the joined name (`Artist A, Artist B, Artist C`) as their one artist under their single album artist, and the next plan found nothing to update. INFERRED: joining keeps every artist and composer the file names, where a first-value rule would drop all but one. OPEN: what Apple Music's own FLAC import does with a repeated field.

### 5.4 Multiple artists

CONFIRMED. The wire `artist` is a scalar display string. The collection identity follows the effective album artist (section 7.2), so distinct track artists sharing one explicit album artist land in one album and one `artist_pid`. In one album, tracks with different guest artists shared one album and one Artist K album-artist PID; another album kept different track artists under one album artist. There is no array of performer PIDs to emit. Preserve `A;B` and `A, B` as distinct strings: they are different keys under the observed comparison. Track K's source artist later became `Artist F, Artist G` while its historical insert carried `Artist F;Artist G`, with the explicit album artist `Artist F` unchanged.

### 5.5 Compilation handling in the mapping

CONFIRMED. The MP4 `cpil` reader (RVA `0x3c59bf-0x3c5a65`, new build) looks up `cpil`, reads data child 1, one byte, checks integer type `0x15` and length 1, and records the byte and a presence bit. The wire writer (`0x46293c-0x46295f`) extracts track `+0xc3` bit 1 and emits the boolean `is_compilation`. The Linux parser accepts only the values 0 and 1 and rejects duplicate flags; the native reader alone does not justify accepting arbitrary non-zero bytes. Absent or false `cpil` sends no `is_compilation` field.

### 5.6 Text rules and limits

- Host agent string pool: 255 UTF-16 units per string (Pascal strings, `0x1406d0d30`, new `0x1406d0ea4`, limit `0x1fe` bytes). Applies to values the host agent parses from files. Text is cut at the first NUL in ID3 comments. MP4 `©lyr` is exempt (separate allocation). CONFIRMED.
- A Linux sender writes the plist itself, so the host agent's truncation does not apply. Whether the phone stores more than 255 units is OPEN; cutting at 255 UTF-16 units on a code point boundary is the safe, Apple-matching choice (INFERRED).
- Control characters: the host agent's string conversion (`0x1406c2400`) is a plain `CFStringCreateWithCharacters` with no filtering, so a newline would be sent as is. What the phone does with one was never observed. OPEN.
- Empty versus absent: absent is the normal insert form, and an empty source value is absent (section 5.3). An update sends `""` when the field is selected for change.
- Unicode is preserved exactly (a Cyrillic album artist round-tripped).
- Names are compared for grouping as described in section 7.1.

## 6. Comment and the five sort fields

Status of every claim in this section: CONFIRMED means seen in code or data flow of the host agent, in real Apple traffic, or in the phone database. INFERRED means reasoned from those.

### 6.1 What to send

| Field | Plist key (`item`) | Insert rule | Update rule |
| --- | --- | --- | --- |
| comment | `comment` | Send only when the source has a non-empty comment; omit when empty | Send only when comment is in the change set; `""` is sent when the field is selected |
| sort title | `sort_name` | Always send. Value: explicit sort tag, else the title with a leading article removed | Send when title or sort title changes |
| sort album | `sort_album` | Always send; same rule from the album | Send when album or sort album changes |
| sort artist | `sort_artist` | Always send; same rule from the artist | Send when artist or sort artist changes |
| sort album artist | `sort_album_artist` | Send exactly when `album_artist` is sent (66 of 66 Apple inserts) | Send when album artist or its sort changes |
| sort composer | `sort_composer` | Send exactly when `composer` is sent (263 of 263) | Send when composer or its sort changes |

CONFIRMED key names. `sort_title` never appears in real traffic and is not the builder's key; the title sort is `sort_name`. `grouping` and `description` never appear in the 400 Apple inserts.

CONFIRMED: Apple's real data always sends `sort_name`, `sort_album` and `sort_artist`. The value equals the plain value, except for a stripped leading article: "The Example Album" sends `sort_album` "Example Album", "The Second Album" sends "Second Album", "An Example Title" sends "Example Title". A sort field is never absent on a track that has the plain field.

INFERRED: when the file carries an explicit sort tag (MP4 `sonm`, `soal`, `soar`, `soaa`, `soco`; ID3 `TSOT`, `TSOA`, `TSOP`, `TSO2`, `TSOC`), Apple sends that value verbatim. The binary shows an explicit sort replaces the article-derived one (status byte below 0x80 means explicit), but no real traffic has a source with explicit sort tags: the 796 parsed ALAC source files had no sort atoms. On the phone side, explicit sorts sent from Linux were stored as sent (section 6.10).

### 6.2 Getter and builder findings

All addresses old / new (new is 0xc lower for the getters).

| Field | Getter | Source in track info |
| --- | --- | --- |
| title | `0x1403921e8` / `0x1403921dc` | pool id at `[trackInfo+0xd8]` |
| album | `0x1403927d0` | `[+0xe4]` |
| artist | `0x140392d50` | `[+0xdc]` |
| album artist | `0x140393304` | |
| composer | `0x140394f38` | |
| genre | `0x1403955a8` | |
| comment | `0x140396140` / `0x140396134` | pool id at `[trackInfo+0xfc]`, pool at db `+0x2120`, no fallback |
| sort title | `0x1403926b0` / `0x1403926a4` | sort pool index 0 |
| sort album | `0x140392c28` | index 1 |
| sort artist | `0x140393180` | index 2 |
| sort album artist | `0x1403936a4` | index 3 |
| sort composer | `0x14039528c` | index 4 |
| sort series | `0x140355228` | index 5, not sent by Linux |

The sort getter engine is `0x140391fcc` (new `0x140391fc0`). It reads the sort pool string id at `[trackInfo + 0x1b8 + idx*4]`. If that is empty, it falls back to the plain-name pool string. The plist `sort_X` is therefore the explicit sort when there is one, otherwise the name. Combined with the omit-empty string setter, an insert carries a sort whenever the name is non-empty, which matches the traffic.

Default sort derivation (`0x140354a58`, new `0x140354a4c`; the only function whose new-build shape differs, by one extra local initialisation): it skips leading characters at or below 0x20 and class-0x3c characters (`0x1406bf330`), then calls `0x140355490` (new `0x140355488`). That function compares against an article table at `gAppContext` (`0x140b70640`) + 0x468 (array of 128-byte Pascal strings, count at +0x470, case-insensitive prefix compare through `0x1406c0254`). A match gives sort string = remainder and status 0x81. No match gives no sort string and status 0x80, and the getter then returns the name. The MP4 reader stores status 3 (explicit) when the `ssXX` status atom is absent.

Apply-metadata sites:

- `0x140361dac` (32-bit flags): flag `0x400` is the comment; metadata `+0xe08` (Pascal wide string) goes to the track comment pool `0x2120` and marks change index 14.
- `0x140362a30` (new `0x140362a28`, 64-bit flags): sort flags bits 42 to 47 are `0x40000000000` sonm, `0x80000000000` soal, `0x100000000000` soar, `0x200000000000` soaa, `0x400000000000` soco, `0x800000000000` sosn. Metadata buffers at `+0x23f4`, `+0x25f4`, `+0x27f4`, `+0x29f4`, `+0x2bf4`, `+0x2df4`, each 0x200 bytes (255 units plus length). Status bytes at `+0x2ff4..+0x2ff9`, flag bits 48 to 53 (`ssnm`, `ssal`, `ssar`, `ssaa`, `ssco`, `sssn`).

The library process (AMPLibraryAgent) feeds properties `sort-name`, `sort-name-status` and so on (initialisers from `0x1401b0d64`). Its serializer `0x1402c4cf8` (new `0x1402c4c90`) always sends the sort values plus status bytes; the reader counterpart is `0x1402c34e0`. That process was not available, so what it supplies for non-MP4 and non-ID3 files is INFERRED.

### 6.3 Where each field lands on the phone

CONFIRMED against a captured Linux insert and the library snapshot.

| What was sent | Where it lands | Expected value |
| --- | --- | --- |
| `comment` | `item_extra.comment` | NULL when none was sent; else the text unchanged ("Processed by SoX" is stored as is) |
| `sort_name` | `item_extra.sort_title` | The sent string (never empty) |
| `sort_name` | `item.title_order`, `item.title_order_section` | `sort_map[sort string].name_order` and `.name_section` |
| `title` | `item_search.search_title` | `sort_map[title]` (the plain name, not the sort string) |
| `sort_artist` | `item_artist.sort_item_artist`, `item.item_artist_order(+_section)` | Group sort string and its `sort_map` row |
| `sort_album` | `album.sort_album`, `item.album_order(+_section)` | Same pattern |
| `sort_album_artist` | `album_artist.sort_album_artist`, `item.album_artist_order(+_section)`, group `sort_order`, `sort_order_section`, `name_order` | Same pattern |
| `sort_composer` | `composer.sort_composer`, `item.composer_order(+_section)` | Same pattern |
| plain names | `item_search.search_artist`, `search_album`, `search_composer`, `search_album_artist` | `sort_map[plain name]` row |
| every new string | `sort_map` | One row per distinct plain name and per distinct sort string (a title edit with a sort name added two rows, "The Example Title" and "Example Title, The") |

`item_extra.grouping` and `item_extra.description` stay NULL.

`sort_map(name UNIQUE, name_order, name_section, sort_key)`:

- `sort_key` is an ICU-like collation key computed by the phone. `name_order` is interpolated between neighbours. Verify `name_order` relationally (order follows key order), not as a number.
- `name_section` is 0 to 25 for A to Z by the first letter of the sorted string, 26 for digits and other (for example "99 Example"), 27 for empty. CONFIRMED for Latin text; the rule for non-Latin scripts is INFERRED.
- The phone does no article skipping. It keeps whatever string it is sent (a title edit with a sort name and the captured insert both show this).
- `grouping_key` on a group is the sort-key body of the plain name, so group matching is by name and case-insensitive.
- Example row (an ALAC title edit): a title label beginning with "A" had `name_section` 0, `name_order` 158392475520 and a 36-byte `sort_key`. Its only integer owners were the track's `item.title_order` and `item_search.search_title`. The operation that edited the title carried `title`, `sort_name` and the unchanged `album_pid` only.
- OPEN: a title edit to a label that already exists in `sort_map` (a shared label). The row count, order, section and owners for that case were not observed.

### 6.4 Group behaviour when a sort differs from the name

CONFIRMED.

- An album "The Example Album" with sort "Example Album": the group row `sort_album` carries the sent sort.
- Composer `ARTIST B;Artist C` with sort `ARTIST B;ARTIST C`: a later spelling overwrote `sort_composer` while the group name kept the first spelling. A case-variant incident changed the shared composer sort and the `composer_order` of 7 member tracks. The last writer's sort wins on a group, and the members' orders follow it.
- Artist, album and album-artist display names take the spelling of the last inserted track (case-only differences).
- A title plus `sort_name` update touches only `item_extra.title` and `.sort_title`, `item_state.title`, `item.title_order`, `item_search.search_title`, and adds `sort_map` rows.
- A composer plus `sort_composer` update joins or creates the composer group by name; the old group row is not renamed.

### 6.5 Container mapping tables

| Container | Comment source | Sort sources | Evidence |
| --- | --- | --- | --- |
| MP4 (ALAC, AAC, HE-AAC, xHE-AAC) | `©cmt` | `sonm`, `soal`, `soar`, `soaa`, `soco` | CONFIRMED in the host agent reader |
| MP3 (ID3v2) | `COMM` with an empty description (any language); v2.2 `COM` and `CM1` also handled | `TSOT`, `TSOA`, `TSOP`, `TSO2`, `TSOC` | CONFIRMED reader and writer |
| WAV, AIFF | `COMM` in the `ID3 ` chunk | same ID3 frames | CONFIRMED that the host agent only locates the `ID3 ` chunk; native RIFF INFO or AIFF `ANNO` text is not read by the host agent |
| FLAC (Vorbis comments) | `COMMENT` | `TITLESORT`, `ALBUMSORT`, `ARTISTSORT`, `ALBUMARTISTSORT`, `COMPOSERSORT` | INFERRED: the host agent has no Vorbis parser, the Apple Music library does the import |
| RIFF INFO, AIFF text chunks | `ICMT`, `ANNO` / `COMT` | none standard | INFERRED, the host agent has no parser |

Everything about non-MP4 and non-ID3 containers happens on the Apple Music (library) side, which was not available. Do not assume what Apple Music would store.

**ID3 detail.** Comment = `COMM` with an empty description. Helper `0x1403059ac` (new `0x140305968`) returns true exactly when the description is empty. Encodings: 0 Latin-1, 1 and 2 UTF-16 (BOM skipped), 3 UTF-8 (BOM skipped). The language field is validated by `0x1403058f8` but never compared, so any language wins. The text is read by `0x140305d54` (new `0x140305d10`) into `+0xe0a`, length word at `+0xe08`, maximum `0xff` (255) units, up to the first NUL. `COMM` frames whose description is `iTunXXXX\0` (checked at `0x140305a8c`, new `0x140305a48`: "iTun" plus a four-character code plus NUL, Latin-1) are handled as `NORM`, `SMPB` or `PGAP`; other descriptions are dropped, not mapped to comment. A real MP3 with an empty `COMM` frame produced no `comment` in Apple's insert.

**MP4 detail.** `©cmt` is read through `0x140718158` (new `0x1407182b0`): a UTF-8 or UTF-16BE data atom becomes a Pascal wide string of at most 255 units (UTF-8 path `0x1406c1768` with r9d = `0xff`; UTF-16 clamp at `0x1fe` bytes), flag bit 10, stored at `+0xe08` via `0x1406beea8` (asserts length at most 255). Other atoms and offsets: `©nam` +8, `©ART` +0x408, `©alb` +0x608, `aART` +0x1458 (bit 30), `©wrt` +0x1040 (bit 21), `©grp` +0x1254 (bit 28), `desc` +0x17c8, plus `sonm`, `soal`, `soar`, `soaa`, `soco`, `sosn` with one-byte status atoms `ssnm` and so on. `©lyr` is allocated separately and has no 255 limit. The MP4 writers are `0x1403c3b9c` and `0x1403c42d4` (new `0x1403c3b98`).

Real data: 4 of the 796 parsed ALAC files carried `©cmt` ("Processed by SoX"). The two comment inserts in Apple's traffic carry that exact text.

### 6.6 Limits, one value, control characters

- 255 UTF-16 units for the host agent's string pool (comment, sorts and all plain text). CONFIRMED.
- Only one value is read per field: the last empty-description `COMM` wins (ID3), the first data atom of an MP4 item. Multiple values never reach the phone as a list. CONFIRMED. A FLAC's repeated `ARTIST`, `ARTISTSORT`, `COMPOSER` and `COMPOSERSORT` entries are joined into one value by the Linux sender (section 5.3).
- Control characters and newlines: see section 5.6. Comments with newlines are common in both ID3 and MP4. A two-line comment sent from Linux was stored with its newline, so the phone takes one. What Apple's host does with one is not known.
- No real traffic has a comment above 255 units, a newline, or an explicit sort tag.

### 6.7 What the host must compute itself

1. Strip leading articles. The phone does none (CONFIRMED by a title edit with a sort name, the captured insert and the Linux inserts of section 6.10). Apple does it in the host agent with `0x140354a58` and the article table. Only "The" and "An" are seen stripped in real data (CONFIRMED). The table's contents are not in the executable (OPEN), probably loaded from a locale resource at run time (INFERRED). "A" was not seen stripped or kept in any trace. Use "The " and "An " and decide on "A " deliberately. The Linux tool of chapter 07 strips "A " too.
2. Truncate to 255 UTF-16 units (section 5.6).
3. Add `artist_pid` and `album_pid` to updates exactly as Apple does (section 6.8).
4. The host does not compute `*_order`, `*_section` or `sort_key`. The phone does. Verify relationally.

### 6.8 update_track for these fields

CONFIRMED (builder `0x140461900`, dependency lists):

- Fields are selected by change-mask index: title 2, album 3, artist 4, year 7, genre 8, track 11, comment 14, composer 18, album_artist 71, album_pid 77, sort_name 78, sort_album 79, sort_artist 80, sort_album_artist 81, sort_composer 82, sort_series 83, artist_pid 142.
- A selected string is always written, even when empty. An empty comment or sort in an update is sent as `""`.
- The host agent adds companion fields from two u32 lists (old `0x14097d970` = [4, 80, 71, 81, 31]; old `0x14097be40` = [2, 78, 3, 79, 4, 80, 71, 81, 62, 83]; new `0x14097e970` and `0x14097ce40`, identical contents; read from the `lea rdx` at old `0x140461eb1` and `0x140461ee5`). Any non-zero bytewise intersection of the change mask with the first list adds `artist_pid` (142); with the second list adds `album_pid` (77). The helper `0x38d9e0` does the intersection test.

  | Selected | Added |
  | --- | --- |
  | title + sort_name | `album_pid` |
  | album + sort_album | `album_pid` |
  | artist + sort_artist | `album_pid` and `artist_pid` |
  | track_number alone | nothing |
  | comment (14), composer (18), sort_composer (82) | nothing (they are in neither list) |

  The lists are CONFIRMED; their effect on the wire is INFERRED from the mask logic and the native-shaped title update.
- In real Apple traffic, `update_track` operations carry only `artwork_cache_id` and `artwork_checksum`: 399 in the delta reference session (a 400th carries statistics for an existing track) and 399 candidates hooked in the reset builder during the reset reference session. No real Apple `update_track` with a sort or comment exists. Phone behaviour for the other fields comes from the Linux experiments in section 8, and for comments and sort titles from section 6.10.

Update and clear results for these fields are in section 8.

### 6.9 Address index for comment and sort fields (both builds)

Old is AMPDevicesAgent 1.6.4.90 (Apple Devices 1.1540.23042.0). New is 1.6.4.94 (Apple Devices 1.1540.24088.0). All addresses are virtual addresses in the `0x1400xxxxx` image. Functions were matched between builds by comparing instruction sequences with every address, displacement and branch target removed. Each function listed has an identical sequence in both builds, so every offset inside a function shifts by the same amount as its start. CONFIRMED.

| Function | Old | New | Shift |
| --- | --- | --- | --- |
| Operation builder (insert, update, delete, upload) | `0x140461900` | `0x140461a24` | +0x124 |
| String setter (omits empty) | `0x1406992fc` | `0x140699450` | +0x154 |
| Numeric setter (omits zero) | `0x14069950c` | `0x140699660` | +0x154 |
| Title getter | `0x1403921e8` | `0x1403921dc` | -0xc |
| Album getter | `0x1403927d0` | `0x1403927c4` | -0xc |
| Artist getter | `0x140392d50` | `0x140392d44` | -0xc |
| Album artist getter | `0x140393304` | `0x1403932f8` | -0xc |
| Composer getter | `0x140394f38` | `0x140394f2c` | -0xc |
| Genre getter | `0x1403955a8` | `0x14039559c` | -0xc |
| Comment getter | `0x140396140` | `0x140396134` | -0xc |
| Sort title getter | `0x1403926b0` | `0x1403926a4` | -0xc |
| Sort album getter | `0x140392c28` | `0x140392c1c` | -0xc |
| Sort artist getter | `0x140393180` | `0x140393174` | -0xc |
| Sort album artist getter | `0x1403936a4` | `0x140393698` | -0xc |
| Sort composer getter | `0x14039528c` | `0x140395280` | -0xc |
| Sort series getter | `0x140355228` | `0x140355220` | -0x8 |
| Sort getter engine | `0x140391fcc` | `0x140391fc0` | -0xc |
| Default sort and status derivation | `0x140354a58` | `0x140354a4c` | -0xc (one extra local initialisation in the new build) |
| Article prefix match | `0x140355490` | `0x140355488` | -0x8 |
| Apply metadata, comment (32-bit flags) | `0x140361dac` | `0x140361da4` | -0x8 |
| Apply metadata, sorts (64-bit flags) | `0x140362a30` | `0x140362a28` | -0x8 |
| ID3 reader | `0x1403be164` | `0x1403be160` | -0x4 |
| ID3 writer | `0x1403bf318` | `0x1403bf314` | -0x4 |
| COMM helper, empty description | `0x1403059ac` | `0x140305968` | -0x44 |
| COMM helper, `iTun` description | `0x140305a8c` | `0x140305a48` | -0x44 |
| COMM helper, text read | `0x140305d54` | `0x140305d10` | -0x44 |
| COMM helper, language check | `0x140305bb8` | `0x140305b74` | -0x44 |
| MP4 writers | `0x1403c3b9c`, `0x1403c42d4` | `0x1403c3b98` | -0x4 |
| MP4 readers | `0x1403c54e0`, `0x1403c5c0c` | `0x1403c54dc`, `0x1403c5c08` | -0x4 |
| Pool string reader (255 units) | `0x1406d0d30` | `0x1406d0ea4` | +0x174 |
| MP4 text atom reader | `0x140718158` | `0x1407182b0` | +0x158 |
| Pascal assign (asserts at most 255) | `0x1406beea8` | `0x1406bf01c` | +0x174 |
| UTF-8 to Pascal | `0x1406c1768` | `0x1406c18dc` | +0x174 |
| CFString conversion | `0x1406c2400` | `0x1406c2574` | +0x174 |
| Leading-character class 0x3c test | `0x1406bf330` | `0x1406bf4a4` | +0x174 |
| Library serializer (AMPLibraryAgent link) | `0x1402c4cf8` | `0x1402c4c90` | -0x68 |
| Library reader | `0x1402c34e0` | `0x1402c3478` | -0x68 |
| Update dependency lists (data) | `0x14097d970`, `0x14097be40` | `0x14097e970`, `0x14097ce40` | +0x1000 |
| Chunk walker for WAV and AIFF | `0x14044320c` | `0x140443330` | +0x124 |
| Chunk writers for WAV and AIFF | `0x1404437b8`, `0x140444630` | `0x1404438dc`, `0x140444754` | +0x124 |
| `gAppContext` (article table at +0x468, count at +0x470) | `0x140b70640` | not located | |

The article table itself is data and was not located in the new build. The article strings are not in either executable (section 6.7).

### 6.10 Comments and sorts sent from Linux

CONFIRMED on the phone. Inserts sent from Linux carried `comment` when the source had one, `sort_name`, `sort_album` and `sort_artist` always, and `sort_album_artist` and `sort_composer` with their names, as section 6.1 describes. One insert batch held eleven tracks of every format, among them an MP3 with a comment and every ID3 sort frame. A later batch and a metadata session followed.

- Every value was stored as sent, in the columns of section 6.3: the explicit sort names, a two-line comment with its newline, and the article-stripped sort title, sort artist and sort album of names beginning with "The".
- **An album artist made from the item artist takes the item's sort artist.** A track with the artist "The CLI S9 Band" and no album artist created an album-artist group whose sort name was "CLI S9 Band", the item's sort artist. A second track, "CLI S9 Solo" with the explicit sort artist "Solo, CLI S9" and no album artist, gave its album-artist group the sort name "Solo, CLI S9". So the phone copies the item's sort artist; it does not strip articles itself.
- A group the track joins keeps its stored sort name when the insert sends that stored value (section 6.4: the last writer's sort wins on a group, so a sender that sends anything else re-sorts the group's members).
- A metadata session of four `update_track` operations then added a comment ("Processed by SoX") to two tracks and a sort title to two others in the reference library, and the phone applied each.

## 7. Grouping rules

### 7.1 How the phone matches groups

CONFIRMED. The phone matches each group type independently, by the grouping key of the name:

- `album`: name together with the `album_pid` (and the linked album artist).
- `album_artist` (wire `artist_pid`), `item_artist`, `composer`, `genre`: by name only.
- Comparison ignores ASCII case. The group takes the spelling of the last track filed under it, and every member displays that spelling. Example: artist `Artist E` filed with `ARTIST E` members became `ARTIST E`, and a later member spelled `Artist E` renamed the group back. Known pairs of this kind: `ARTIST A`/`Artist A`, `ARTIST F`/`Artist F`, `ARTIST B`/`Artist B`, `ARTIST H`/`Artist H`, `ARTIST I`/`Artist I`.
- Composers differ: a new composer row keeps the first spelling as its name and the later spelling as its sort name. `ARTIST B;Artist C` then `ARTIST B;ARTIST C` in one batch gave name `ARTIST B;Artist C`, sort `ARTIST B;ARTIST C`. In the same batch an item artist `Artist B, Artist D` became `ARTIST B, Artist D` while the album artist with the same text kept its name.
- Album renames by case are modelled the same way but not yet observed.
- Planning consequence: a group whose name differs from the source only by ASCII case is "no change". Treating it as a change produced 61 spurious metadata updates in one library.
- Whether non-ASCII case or Unicode normalisation forms match is OPEN. The phone's key is a collation key, so behaviour beyond ASCII must not be assumed.

Apple's own matcher, for reference (CONFIRMED statically, new build):

- Album selector RVA `0x2d0450` (reached through `0x2d1968`; its assertions name album grouping). Steps for the ordinary non-empty-album path:
  1. Read the track's album-artist string index at `+0xe0`; if zero, read the artist at `+0xdc`. Keep a separate flag for an explicit album artist (`0x2d04c0-0x2d04de`).
  2. Compare the album text and candidate album kind, plus the media classification getters.
  3. For the ordinary fallback, compare the effective artist with the album's effective-artist slot `+0x130`. When the album artist was absent, also require the track's compilation bit to agree with album slot `+0x138`.
  4. Return the retained matching album, or call the new-album constructor only after no match. The constructor stores effective artist `+0x130`, actual explicit album artist `+0x134` and compilation `+0x138` separately.
- Artist selection takes the same fallback at `0x2dc539-0x2dc564`, scanning existing ordinary artists and calling matcher `0x2dc334` first. That matcher excludes special artist kinds, checks the track and artist classification and compares the selected name. This is direct evidence for album-artist precedence in `artist_pid`.
- Both text comparisons call `IsEqualUText` at `0x6c03c8` with default flags `0x0b`. Its bytecode strips leading UTF-16 units at or below `0x20` under flag 2 and folds ASCII `a-z` under flag 1. The fallback calls `CompareStringW` with locale `0x400` and flags `0x20001`. In one native-to-live comparison, 33 effective artist names differed only in ASCII case and all kept the same wire artist ID.
- Exact Unicode equality and fully ASCII case-insensitive equality are safe conservative subsets. The host's locale-aware comparison is not reproduced by simple Unicode case folding, and punctuation, whitespace and delimiters cannot be freely normalised. `A;B` and `A, B` are different keys.

### 7.2 Effective album artist

CONFIRMED. `effective_artist = explicit album artist (aART) if present and non-empty, else the track artist (©ART)`. The host takes an empty `aART` as none (section 5.3). Do not add an `album_artist` field merely because the database derived one; Apple omits `album_artist` when the source has none (334 of 400 inserts). The collection `artist_pid` follows the effective album artist.

For two native `Album P` tracks that both omitted album artist, the album PIDs were different. Equal album text alone is not a grouping key, and title-only album reuse is unjustified.

### 7.3 Compilations

CONFIRMED statically (selector `0x2d0450`) and by data:

- If both the track and the candidate album carry the compilation flag, the selector compares the actual explicit album artist (`+0x134`), including the both-absent case, after the album title matches. Two compilation tracks with different track artists can therefore group when `aART` is absent.
- The fallback compares the effective artist (`+0x130`). At `0x2d0efe-0x2d0f1a`, an explicit `aART` bypasses the compilation-bit equality test, while keeping the earlier mismatch rejection. Without explicit `aART`, the fallback requires equality with `+0x138`.
- A new album is built only after no candidate matched.
- A helper recognises both a localised name and "Various Artists", but this is not evidence that every compilation must receive that English string or a special PID.

Consequently `(album, aART, cpil)` is not a universal album key.

Observed compilation inserts (the reset reference session, `operations[63]` and `[222]`): Artist J "Track M" with an explicit `aART` holding a localised, non-English form of "Various Artists" and `cpil` true, and Artist F "Track K" with explicit `aART` `Artist F` and `cpil` true. Both dictionaries carry `item.is_compilation: true`, the exact explicit `album_artist`, its `sort_album_artist`, and ordinary signed `album_pid` and `artist_pid`. No guessed "Various Artists", special PID, translation, or split of the item artist into performers is needed.

Mixed membership is native: `operations[230]` inserts "Track T" with `aART` `Artist F`, the same album and artist PIDs as Track K, and no `is_compilation` field, into the same album. The final database confirms:

- "Track M" is the only member of its album; its item compilation flag is 1.
- The album "Album F" has exactly Track K and Track T, item flags `[1, 0]`, joined to the same album and album-artist `sync_id`.
- Album F's `album.all_compilations` is 1 despite the mixed membership. It must not be read as `all(member.is_compilation)`. `has_compilation_tracks` is a derived observation.

Linux compilation experiment: a fixture inserted with an explicit Unicode `aART` and `is_compilation: true` into a fresh album and album-artist group gave a new track flag 1, a new album with `all_compilations` 1, and everything else unchanged. A second fixture with no `cpil` and the same album and album artist joined the same two groups (flags `[true, false]`); the album's `all_compilations` stayed 1; exactly one existing `entity_revision` row for the album was replaced by a new revision (`(74325, 73859102, 0, 4, 0)` became `(74330, 73859102, 0, 4, 0)`).

Not established: a compilation track with a missing or empty `aART`, and an ordinary album receiving its first compilation track (the `all_compilations` transition). An emptied album keeps `all_compilations` 1 and accepts compilation members again: Album F, after its tracks were removed, took Track K back with its ID, `all_compilations` 1 and the track as representative (observed during a later library restore).

Editing `is_compilation` on an existing track changes `item.is_compilation` only; the album row is not touched.

### 7.4 Representative bookkeeping

Each group row (`item_artist`, `album_artist`, `album`, `composer`, `genre`) stores `representative_item_pid`. CONFIRMED.

- A new group's representative is its first inserted item.
- When the representative leaves the group (deleted or moved), the lowest remaining item PID becomes the representative. The change becomes visible from the next session.
- Joining a group that has members does not make the joiner its representative, even with a lower item PID. A later edit of another member may recompute it to the lowest member. The recomputation rules are not fully pinned.
- A group emptied by deletion or by moving its last member keeps its row with representative 0. A later move into it by key reuses it, and the joiner becomes its representative.
- A delete session moves an artist's representative off a member without a cover to the lowest member with one (Artist L). A genre's representative moves the same way (an uncovered member that was not a target represented the album, both artists and the genre; all four moved to the lowest covered survivor). Whether an insert session does the same, and whether composers follow, is unknown.
- What counts for an edit is the representative stored at the start of the session, not one changed earlier in the same session.
- It is not simply the lowest item PID.

Because the rules are not predictable, read representatives from a fresh capture before each session.

### 7.5 Which edits regroup and which do not

CONFIRMED (41 metadata-only sessions). Section 8 has the full table.

- Genre and composer: by name, always safe.
- Album: name together with an `album_pid`.
- Artist: only the group's representative moves; any other member renames the group in place.
- Album artist: only a full regroup by the representative; otherwise only the item's order and search columns move, leaving an inconsistent catalogue.

### 7.6 Emptied albums, artists and groups

CONFIRMED.

- Deleting the last member of a group leaves the group row, with representative 0. No group row is deleted, renamed or given a new revision by a small removal session.
- A later insert that names the emptied group by its old PID reuses the row, and the new item becomes the representative. A later insert that names a new PID with the emptied group's name does not create a row. The phone reuses the row with that name and rewrites its `sync_id` to the new PID (album `4417302958861730216` became `1893650274405916738` in a batch insert session). Clients must reuse an emptied group by its old PID.
- An emptied group also keeps its previous cover registration until refilled (section 10.5).
- Keep-local and statistics bookkeeping: an album at `keep_local_status` 0 that is emptied is recomputed by the phone to status 2, gets a new revision directly above the tombstones, and loses its statistics revision row (revision type 1). Albums already at status 2 did not change. Albums created by batch inserts had `keep_local_status` 2, set at the end of the session that created them; an album left by an interrupted batch stays at 0 until the next session. A metadata session that edited another album recomputed such an album the same way, with its revision one above the session's own block and the last-synced revision left at the session's highest.
- In a 100-target removal session, the phone collected every group that no item belonged to any more: 119 albums (63 it emptied and 56 left earlier), 89 album artists, 100 item artists, 38 composers and 7 genres. Their live revisions became tombstones. Sessions of 16, 32, 200 and 499 targets collected every group they emptied; sessions of up to 8 targets, and one of 90 that emptied the library, collected none. The trigger is unknown. See `05-receiver-operations.md`.
- A re-add of a deleted track's source created a new native track with a new item ID and a new file path. It reused the retained empty album and artist rows. A comparison of all 45 tables showed the ten item-level tables repopulated (INFERRED to be the ten that a delete removes, section 12), the old item's tombstone unchanged, three group representatives set, and only the usual synchronisation properties moved. The opaque `item_extra` integrity value cannot be predicted and was checked only for shape. The receiver dates agreed with the creation time of the newly written permanent file.
- `delete_album` and `delete_artist` operations are accepted and ignored. Emptied groups go only through the phone's own collection.

### 7.7 Clearing a field

CONFIRMED (six sessions on one test track whose album, genre and composer had no other member):

| Field | Clearing form | Result |
| --- | --- | --- |
| `year` | 0 | `item_extra.year` 0; the album is revised and `album.album_year` becomes the members' maximum |
| `track_number` + `track_count` | 0 + 0 | `item.track_number` and `item.episode_sort_id` 0, `item_extra.track_count` 0 |
| `disc_count` | 0 | `item_extra.disc_count` 0 |
| `copyright` | `""` | `item_extra.copyright` `''` (a track inserted without one has NULL) |
| `composer_pid` or `genre_id` | 0 | Ignored: the item gets a new revision and no cell changes |
| `genre` or `composer` | `""` | **Not a clear.** Orders become 0 with section 27; the item keeps its composer and genre |

A genre or composer has no clearing form. Only removing the track and inserting it again without the field clears it. Clearing a comment with `""` was never run (INFERRED analogous to `copyright`, which would give `''` rather than NULL). `""` as a sort update is untested. How Apple's own sender clears genre or composer is unknown (no capture).

## 8. Editing an existing track

### 8.1 Envelope and constants

CONFIRMED.

```text
{ revision: anchor + 1, timestamp: <date>,
  operations: [ { operation: "update_db_info", pid: <database pid>,
                  db_info: { primary_container_pid: <library container pid> } },
                { operation: "update_track", pid: <item_store.sync_id>, item: { ... } },
                ... ] }
```

- All 41 sessions were accepted; the receiver finished within about 0.07 to 0.6 s of staging.
- A session may carry several `update_track` operations, including several for the same track, applied in order.
- After every session: `MLSyncClientSyncAnchor` is the old anchor + 1, `MLSyncClientLastSyncedRevision` and `MLArtistHeroImageImportDatabaseRevision` equal the highest new revision, `MLSyncClientGenerationID` takes a new random value.
- New strings add `sort_map` rows (one per distinct name or sort name). Existing rows are reused.
- Every session zeroes all non-zero `item_stats.play_count_recent` and `skip_count_recent` values. The first session after a break zeroed 65 items; later sessions found none left. This is the same import profile seen when artwork is registered (section 10).
- Native-shaped updates carry companion IDs (section 6.8): a title edit also carries the current `album_pid`. The title-only operation on a fixture is the smallest native form.

### 8.2 Item fields that do not regroup

| Sent | Phone result |
| --- | --- |
| `title`, `sort_name` | `item_extra.title`/`sort_title`, `item_state.title`; `item.title_order` and section from the sort name; `item_search.search_title` from the title |
| current `album_pid` alongside | nothing else |
| `track_number` | `item.track_number` and `item.episode_sort_id` |
| `track_count` | `item_extra.track_count` |
| `disc_number` | `item.disc_number` |
| `disc_count` | `item_extra.disc_count` |
| `year` | `item_extra.year`. When the new year differs from the album's, the album gets a new revision and `album.album_year` becomes the members' maximum year (2018 to 2019 in one edit; unchanged at 2016 when one member was set to 2010). A year equal to the album's leaves the album alone |
| `is_compilation` | `item.is_compilation` only |
| `content_rating` | `item_extra.content_rating` and `item_state.has_explicit_content` |
| `copyright` | `item_extra.copyright` |

A positive `track_number` change within a known count (1 to 2, total 2 unchanged), observed on a generated track, changed exactly `item.track_number` and `item.episode_sort_id` (both to the new value), advanced the target's live revision to the previous global maximum plus one, and changed four synchronisation properties. 42 of 45 tables were exact; the inverse (2 to 1) produced the symmetric result at the next anchor.

### 8.3 Group fields

- `genre`: the item moves to the genre with that name, or a new genre row (phone-allocated ID, class-3 revision).
- `composer` + `sort_composer`: the item moves to the composer with that name, or a new composer row (class 5). Works whether or not the item is the composer's representative. The old row is never renamed.
- Album: `album` + `sort_album` + the `album_pid` of an existing album: the item joins it. With an unused `album_pid`: a new album row with `sync_id = album_pid`, the item's current album artist, the item's year and a class-4 revision, even when an album with the same name already exists (a duplicate name results). `album_pid` without a name re-keys the item's current album in place, changing the sync ID for every member. `album` without `album_pid` does not regroup: the item stays in its old album but its `album_order` and `search_album` move to the new name, leaving the catalogue inconsistent.
- Artist (`item_artist`): `artist` + `sort_artist`. If the item is its group's stored representative, it moves to the group whose key matches the new name, or a new group; joining an existing group rewrites that group's display name to the sent text. If it is not the representative, the receiver renames the shared group in place: `item_artist` and `sort_item_artist` change for every member and the `grouping_key` keeps the old name. Later edits sending the new name do not find that group and create a second one with the same display name. This happened with `artist_pid` present as well.
- Album artist: `album_artist` + `sort_album_artist` without `artist_pid` never regroups; only `album_artist_order` and `search_album_artist` move. With `artist_pid`, for a non-representative item, the receiver re-keys the shared album-artist row (`sync_id` becomes the sent PID, name unchanged). If the PID belongs to another album artist, two rows share one sync ID, which makes a catalogue reader reject the library as ambiguous. The representative sending the full set (`artist`, `sort_artist`, `album_artist`, `sort_album_artist`, `album`, `sort_album`, `album_pid`, `artist_pid`): an unused `artist_pid` creates a new album artist with that sync ID and name, plus a new album and item artist; an existing `artist_pid` joins that album artist. An album-artist or `artist_pid` edit also re-resolves the item's `item_artist` from the current artist display name.

### 8.4 Safe sender recipe (supported by the experiments)

1. Item fields from the table above: any track, many tracks per session.
2. `genre`; `composer` + `sort_composer`: any track.
3. Album: `album` + `sort_album` + `album_pid`, where the PID is the sync ID of the existing album with the same album, album artist and compilation grouping, or a fresh random 64-bit ID unused by any album. Never `album_pid` alone, never `album` alone. An album one operation creates is joined from the next session, never by a later operation of the same session: the phone creates it again under the same sync ID (`05-receiver-operations.md`, section 2).
4. Artist: `artist` + `sort_artist`, without `artist_pid`, only when the track is its item artist's stored representative at the session's start. Each session can move at most one representative per group.
5. Album artist: the full set above, only when the track is the stored representative of both its item artist and its album artist. As for albums, a new album artist is joined from the next session.
6. Never send the current group PIDs with a changed name. Never send a group PID without its name. Clear numbers with 0 and copyright with `""`; never send `""` for another field.
7. Refuse everything else. A two-session move for a non-representative (session 1 moves the representative to the new name, session 2 moves the target, now representative, to it and moves the first back to the old name) is a hypothesis, not tested.

A comment or sort-only change of a shared artist, album or album-artist name through `update_track` is untested. The sort-only effect on the group row and other members is OPEN.

### 8.5 Repairs and damage

Three tests damaged shared groups (in-place renames and re-keys) and were repaired with further sessions that sent the original names or PIDs. After repair the group rows matched their pre-test state except for newer entity revisions. Some test tracks carry inconsistent sort orders as a result. Empty item-artist rows with representative 0 remain; deleting tracks does not remove them.

## 9. File identity and on-device paths

### 9.1 Paths

CONFIRMED.

- Permanent path: `iTunes_Control/Music/Fnn/XXXX.ext`, for example `iTunes_Control/Music/F02/KWZT.m4a` and `iTunes_Control/Music/F40/HQVN.mp3`. The host picks the folder and file name and announces it in `FileComplete.AssetPath`. In `item_extra`, `location` holds the file name only; `base_location_id` identifies the folder (3850 for `F10`). An uncompleted insert row has `base_location_id` 0, empty `location`, `file_size` 0.
- Folder index: low 32 bits of a scaled `QueryPerformanceCounter` helper modulo `numMusicGroups` (default 50, so `F00` to `F49`; formatter `0x48906c`; constructor default at `0x4ada69`). File name: four uppercase letters, each `A + (random_u16 % 26)` (`0x49cb0c`), no title or PID hash. A collision adds a decimal suffix (`ABCD 1.m4a`). The name limit is 31 UTF-16 units and parent plus separator plus name at most 255. These are host limits, not demonstrated receiver limits.
- The collision lookup uses AFC `GetFileInfo`, and the native naming loop treats any non-zero status as "free" without reserving it.
- Staging path: `Airlock/Media/<AssetID>.<ext>`, then a rename to the bare `Airlock/Media/<AssetID>`, then `FileComplete` with the permanent path. Artwork stages at `Airlock/Media/Artwork/<AssetID>`.
- The phone stores `item_extra.location_kind_id`: 26 for ALAC and 42 for MPEG audio. The kind string ("Apple Lossless audio file", "MPEG audio file") is not stored; only the integer. Other kinds are unknown.
- Audio format code and the `avformat_info` group belong to chapter 06. Receiver storage that is confirmed: `item_playback.audio_format` carries `avformat_info.audio_format` (601 for ALAC, 301 for MPEG-1 Layer III); for a 98-frame headerless CBR MP3, `item_playback.duration` was INTEGER 112896 (samples) and `item_extra.total_time_ms` REAL 2560.0. The columns that hold the gapless fields (delay, drain, resynch, heuristic) were not identified (OPEN).
- The `location` dictionary sent by Apple is larger than the minimal one that works: Apple sends `location`, `location_type` ("FILE"), `kind`, `file_size`, `extension` (an OSType such as "M4A " or "MP3 ") and `date_created`; a Delta insert with only `kind` is accepted for ALAC and MP3 (builder `0x140462421-0x140462518`).

### 9.2 How Apple decides a track is present and correct

Static analysis only (CONFIRMED unless marked).

**Q1. Does Apple hash files on the phone?** Practically no. The host agent imports `AFCOperationCreateGetFileHash` (IAT `0x140824fd0`, one call site at `0x1405ac218`) and `AFCOperationCreateGetFileHashWithRange` (IAT `0x140824fc8`, call site `0x1405ac5be`), each installed only as a file-system "proc" (table slots `+0x110` and `+0x118` of the AFC file-spec table at `0x140b9b220`). The only consumer found is the copy-resume check `sub_6b134c` (RVA `0x6b134c`): it hashes the first L bytes of the destination (SHA-1, 20 bytes) and compares with the source, and runs only when the copy option byte at `[opts+0x10]` equals 3. No consumer of whole-file `GetFileHash` was found (NOT FOUND, not proof of absence). `MobileDevice.dll` has one unreferenced SHA-1 resume helper (`sub_ef170`, dead code) and an app transfer that compares `afc_stat` type, size and mtime and then sets the phone file's mtime. `AirTrafficHost.dll` has no AFC and no hashing. The newer build has the same structure (call sites `0x1405ac39c`, `0x1405ac742`). The hash and checksum string sets of the two builds are identical. The phone-side cost of `GetFileHash` is covered in the transport chapter.

**Q2. How does Apple know a track is already there?** It does not look at the phone's files.

- Metadata operations are keyed by `pid`. The merge code (`0x469b04`) matches on `operation` and `pid` (`CFEqual`).
- The phone answers with an `AssetManifest` listing only the assets and asset parts it still needs: entries `{AssetID, AssetType, IsDownload, Variant:{AssetParts}}`, with `AssetID` a bare signed-decimal PID. Asset-part bits: Track = 1, Artwork = 2, Genius = 4, Lyrics = 8. The host copies exactly those. In the reference reset run the host staged 401 operations (1 `update_db_info`, 399 `insert_track`, 1 `insert_playlist`) and the phone asked for 54 assets. The staged `insert_track` carried no audio hash, and its `location` held only `kind`.
- The phone's rule for "needed" is inside iOS and cannot be seen (INFERRED PID-based; artwork, lyrics and Genius parts are requested separately from the track part).
- There is no message that lists the phone's tracks with sizes or hashes. `AssetMetrics` carries only `_Count`, `_PhysicalSize`, `_PurgeableSize` per asset type; `InstalledAssets` contained only `Application._Deleted` in captured sessions.
- Host-side change detection uses per-dataclass sync anchors (`hostLastSyncAnchor`, `hostNextSyncAnchor`, and the device's anchor from `ReadyForSync`). Selector `0x46cf0c`: if host-last differs from device-last, the sync is a Reset after dropping pending `upload_track` operations; otherwise Delta. Delta sends only changed fields, computed against the host's mirror ("modifiedTrackFields"), and drops any `update_track` that ends with two or fewer keys (`operation`, `pid`; `cmp rax,0x2` at `0x14046a0a0`). Replacing a device track compares a 64-bit "store version" at TrackInfo `+0xa8`. Comparisons ("X does not match", in `sub_35a294`) are against the host mirror, never against the phone's file.
- Size-like change detectors are cheap numbers, not hashes: `artwork_checksum` is computed from stored fields (section 10), and the host logs "artwork size for track changed".
- `file-hash`, `package-file-hash` and `cover-image-hash` plist sources exist in the builder (`0x14046483b`, `0x140464803`, `0x140464793`) but come from item metadata (books and Store content), not computed from phone files.

**Q3. Are sizes compared?** No. AFC `GetFileInfo` is used for naming collisions and for one post-copy stat. No code compares the stat'ed phone size with the source size to skip, resend or delete a Music file. `JRCopyFiles` asserts `replaceOption != kCopyFilesDoNotReplaceExistingItems`: an existing destination is replaced, not skipped.

**Q4. What is verified after a transfer?** In order:

1. Status of each AFC write (special-cased -39 `eofErr`) and the close status.
2. One AFC `GetFileInfo` of the written file (`sub_6aab9c`, call at `0x140229558-0x1402295ac`). Its `st_size` (data plus resource fork length) and modification date are stored in the host's track instance (`+0x68`, `+0x220`). The value is recorded, not compared with the source size.
3. The recorded size is sent back as `location.file_size` in later `insert_track` operations ("the file at this path is N bytes as you reported").
4. Rename of the Airlock file to the bare AssetID.
5. `FileComplete` with exactly `AssetID`, `Dataclass` and `AssetPath` (plus `Metadata`/`Anchor`); no size, hash or timestamp. `FileProgress` carries only progress; `FileError` carries `ErrorCode` and `ErrorIsVisible`.
6. After the sync: `SyncFinished`, then the anchor commit and the phone's `AssetMetrics`.

Nothing is read back (INFERRED: indirect calls cannot be excluded). The ATC error vocabulary (`0x455b54`) has no checksum or size-mismatch class: 0 Unknown, 1 InstallFailed, 2 Canceled, 3 FileNotFound, 4 NotAuthorized, 5 AuthorizedButMissingKey, 6 AccountLimitExceeded, 7 Timeout, 8 ConnectionFailed, 9 SyncDisallowed, 10 NotInBackup, 11 IncompatibleFormat, 12 IncompatibleVersion, 13 SystemFailure, 14 InsufficientFreeSpace, 15 UnknownAssetIdentifier, 16 Unavailable. A `FileError` from the phone is only logged by the host.

**Q5. What does the host keep?** Visible statically: per-dataclass anchors (host-last `+0x14`, host-next `+0x18`, device-last `+0x1c`) and per-device "track instances" with the permanent device location (`permLocationStatus`), the stat'ed size and modification date. Where the anchors persist was not traced. INFERRED: the mirror lives in the host media library database. No per-device list of assets already sent keyed by file hash was found.

**Missing or wrong file.** The host sends `FileError` code 3 (`ATErrorFileNotFound`) when the source file is missing. What happens when the phone's database lists a track whose file is missing or truncated is unknowable here: no message lets the host ask. Observed on the phone: the phone accepts `delete_track` for a row whose file is already gone. A removal session killed at the first `Progress` rolled the catalogue back but the file stayed gone. The phone does not put it back.

### 9.3 What the phone does to files after writing

CONFIRMED. The phone trims the spare allocation of a recently written file some minutes later: `st_blocks` changed 40904 to 40656 (128,491 spare bytes to the size rounded up to 4 KiB) while size, mtime, birthtime, path and a phone-computed SHA-1 stayed the same. Any "file unchanged" check must ignore `st_blocks`.

The phone deletes the files of removed tracks itself, minutes after the session; they are present right after it. Once a file is gone, an AFC stat of its path returns status 8 (object not found), which is the reliable "absent" answer. Files that were uploaded and renamed into `Airlock/Media` but never completed stay there; nothing claims them.

### 9.4 Interrupted inserts

CONFIRMED.

- A batch insert interrupted after some `FileComplete` messages still creates all planned rows. Completed rows are ordinary installed tracks. Uncompleted rows have no file location (`base_location_id` 0, empty `location`, `file_size` 0), no non-purgeable asset, and are still library-added and supported. The phone neither removes nor fills them later.
- A metadata-only `delete_track` for those rows removes them (the 45-table model matches).
- A session the host abandons before uploading anything (the host refused an undeclared asset part) dropped the first two rows named in the manifest as tombstones, while the other rows stayed. Their groups stayed with representative 0. Why exactly those two is OPEN.
- When the phone is still importing after the last `FileComplete`, it may send `Ping` instead of `SyncFinished`. The host must answer with `Pong` and keep reading; the import finishes anyway.

### 9.5 Recommendation (supported by the analysis)

Key everything by PID. Keep a host ledger of what was sent: source hash computed on the host, phone path, size reported by `GetFileInfo`, anchors. Verify an upload with write, close and rename statuses, one `GetFileInfo` (require the size to equal the source, parsing the decimal string as u64: Apple's `atoi` in `MobileDevice.dll` truncates at 32 bits), the phone's acceptance (`FileComplete` acknowledged, manifest no longer lists the asset, anchor advances). Do not call `GetFileHash` or `GetFileHashWithRange` in routine operation. Resume by truncating and re-sending. Do not touch mtimes of managed music files without a device test (the music copy path has no `SetModTime` use that was found; the AMP proc `sub_5a9e7c` exists).

### 9.6 Replacing the audio of an existing PID

Status: static analysis of AMPDevicesAgent 1.6.4.94 (SHA-256 `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9`). Addresses in this section are RVAs (image base `0x140000000`). No phone experiment replaced the audio of an existing PID, so the receiver side is OPEN.

**What the host can emit for an existing track.** CONFIRMED statically.

- The ordinary "track modified" notification (`0x6d747269`) selects `update_track` (`0x46a17d` to `0x46a186`) and calls the shared builder `0x461a24` with the set of modified fields (`0x46a1a9`). A resulting dictionary with only the two base keys `operation` and `pid` is dropped. The base helper is `0x4619a4`.
- The builder can emit sample rate, bit rate, total time, format, duration and gapless fields in an update. That proves the dictionary can be built. It does not prove that the phone treats it as a request for new audio.
- The full `location` branch (`0x46249b`) runs only when track-instance byte `+0x45` is 0. It emits:

  | Key inside `location` | Source |
  | --- | --- |
  | `location` | permanent path string (`0x462562`) |
  | `location_type` | instance `+0x3c`, a fourcc sent as a number (`0x462586`) |
  | `kind` | kind string via `0x395a08` (`0x4625b9`) |
  | `file_size` | instance `+0x68`, 64-bit (`0x4625e0`) |
  | `extension` | instance `+0x40` fourcc, only when non-zero |
  | `date_created` | instance `+0x60` as a date, only when non-zero |

  A separate branch emits `location: {kind}` alone. For a non-zero permanent-location status the full-path branch jumps to `0x462684` and does not serialise an empty path as a replace instruction. No `replace_audio` or force-download key exists in this builder.
- Byte `+0x45` is the permanent-location status. Its value 10 is named by an assertion string, `kTrackPermLocationStatus_SyncInProgress` (compare at `0x45d7ac`, string load at `0x45d7c6`). The ordinary location-validity byte is `+0x44`.
- The add and merge routine `0x3580f4` finds an existing destination instance (`0x355d7c`), copies selected metadata (`0x357950` to `0x356fa4`) and emits notification `0x61647472` (`0x3581f4`). The operation generator accepts it only when the instance has status 10 (`0x469da3`) and then emits exactly `{operation: "update_track", pid: <existing PID>}`. This bare update is deliberate and tied to a queued transfer. It does not pass through the two-key rejection. Sending it to a receiver that already holds the audio is not shown to force a replacement.
- The prepared-copy routine `0x22a9b0` sets status 10 (`0x22abad`) when its input flag bit `0x10` is clear under the iPod capability conditions. The manifest download path uses host flags `0x10`, or `0x11` for a transcode (`0x45bff9` to `0x45c02b`). These are internal host copy flags, not wire flags.

**How audio is delivered for an existing PID.** CONFIRMED statically.

- The manifest handler `0x45bd54` resolves the requested PID (`0x35d158`) and branches on the audio bit. It does not require a newly allocated PID.
- In the success callback `0x45b538`, `0x45b8e4` to `0x45a814` allocates a new permanent Music path and `0x45b998` to `0x370740` stores it in the host instance. The old path is never chosen as an overwrite target.
- The transfer sequence is the one in chapter 02: stage `Airlock/Media/<AssetID>.m4a`, verified copy, rename to the bare AssetID, `FileComplete` with the newly allocated root-relative path. Do not overwrite a current Music file over AFC.

**The named version-replacement path deletes first.** CONFIRMED statically.

- Pass `0x5658b8` resolves a device instance and compares a 64-bit "store version" at `instance->object(+0x10)+0xa8` (`0x567ca6`). A lower device value logs that the library version is newer (`0x567ccd`) and jumps to `0x5680a0`. That jump skips the matched-track path that clears the marked-for-deletion bit (`+0xc2` & `0x80`, cleared at `0x568024`).
- The same pass later calls `0x564c34` (`0x5683d3`), which collects marked tracks, logs them and calls `0x564b94`, a tail call to track removal `0x38f7e8`. Only afterwards does the add loop run (`0x568528` to `0x323a54` to `0x32372c` to `0x3580f4`).
- The "store version" is not file mtime, a hash, ALAC change detection or an ATC revision. Its meaning is OPEN.
- The autofill retention loop calls the matcher at `0x5681f9`. A changed transcode bit rate logs that the autofilled track is not kept (`0x568377`). This applies to autofill only.

**How the host decides two tracks are the same.** The matcher `0x564994` tries a source-PID lookup (`0x35da6c`, TrackData map at `+0x2830`), then calls the comparator `0x35a28c` (`0x564ac0`). A rejected candidate falls through to the search wrapper `0x35cdec` (`0x564aee`), which can find a different equivalent instance. A mismatch therefore does not say which PID a later add will use. The default compared-field set is built by `0x488ce8` from the u32 array at `0x910ef0`, as a 32-byte bitset (byte `index / 8`, mask `0x80 >> (index % 8)`). The initial field indices are:

```text
23 0e 27 3e 40 41 a7 3f 47 50 4f 52 51 53 4e 36 9e 9f a0 a1 a2
```

These are native field indices, not ATC keys. The comparator has further unconditional checks. The audio-related ones, when no earlier identity shortcut returned true:

| Check | Branch |
| --- | --- |
| Total time | Instance `+0x64`, inclusive bounds at `0x35a76b` and `0x35a775`. When the search format word `+0x58` is 0, `0x196` (406) or `0x1f6` (502) in the `audio_format` numbering of [chapter 06](06-audio-formats-and-gapless.md), a saturated difference of 500 units is allowed. Otherwise the values must be equal |
| Bit rate | u16 `+0x54` (`0x35a783`). A mismatch is tolerated when either track has bit `0x40` in `+0xc5` |
| Sample rate | f32 `+0x50` (`0x35a88f`). Unequal or unordered values reject |
| Channels | u16 `+0x56` (`0x35a8a3`). A difference is tolerated when either side is 0 |
| Transcode exception | With the sixth argument true and bit 7 of candidate byte `+0x49` set, the checks above are skipped (`0x35a765` to `0x35a921`) |
| Kind and metadata | Instance kind `+0x3c`, track and disc numbers and totals, year and strings. A metadata mismatch also returns false, so false does not mean an audio-only change |

The comparator tests the "store version" only when field `0x8b` is selected, and that field is not in the default set. No file-size, mtime, source-byte or PCM comparison exists in this comparator. Upstream refresh code may update the compared fields first. Not traced.

**Adjacent operation coalescer.** The tail of the operation generator (`0x469c2c`) compares only the last operation already queued. With an equal PID and non-identical dictionaries:

| Previous last operation | New operation | Result |
| --- | --- | --- |
| `update_track` | `insert_track` | Previous update removed, full insert appended |
| `insert_track` | `update_track` | Merged, stays `insert_track` |
| `update_track` | `update_track` | Merged, stays `update_track` |
| `delete_track` | `insert_track` or `update_track` | Both kept in order |
| `insert_track` or `update_track` | `delete_track` | Both kept in order |

A fully equal dictionary is suppressed (`CFEqual`). A different TrackData context appends directly. The merge `0x45b13c` deep-copies the new dictionary, restores the previous `operation` and fills missing values from the previous dictionary, so new scalars win. The coalescer does not turn a delete followed by an insert into an update.

OPEN: which receiver request makes the phone fetch new audio for an installed PID; whether a failed replacement keeps the old playable record; whether statistics and playlist membership survive; and which local-file change makes the host choose the replacement route. Client policy for audio changes is in [chapter 07](07-sync-engine-design.md).

## 10. Artwork

### 10.1 Wire fields

CONFIRMED. A track's cover travels as two item fields:

- `artwork_cache_id` (unsigned 32-bit). A full insert omits a zero value.
- `artwork_checksum` (unsigned 32-bit). A full insert always emits it, including 0. A Linux insert of a track with no cover sent checksum 0 and no cache ID.

A cover is therefore not attached to the audio asset. It is a separate asset part (Artwork = 2) that the phone requests through the manifest.

**The cache ID is a host image-record ID, not a hash.** Native allocation (`0x4db970`) takes `ImageLibrary.nextRecordID` and increments it; the Music record creation (`0x51458c`) copies the new record ID into the track at `+184`. The host's `ArtworkDB` stores the counter at `mhfd+0x1c`; image records carry the ID at `mhii+0x10` and the source track PID at `+0x14`. Because the host writes that file separately, a parsed counter is saved host state, not proof of what the phone allocates. SQL artwork tokens are a different field (section 10.3). A captured ArtworkDB had 300 image records (maximum ID 676, next 677), and its IDs equalled every non-zero cache ID in Apple's 400 baseline inserts.

**The checksum is a size, not a digest.**

- MP4 (CONFIRMED): the host stores the cover count and the sum of complete `data` atom sizes; the getter (`0x2df878`, `0x2df860 -> 0x2df8d0`) returns their wrapping 32-bit sum. For one standard JPEG, `checksum = 1 + (JPEG bytes + 16) = JPEG bytes + 17`. Example: a 187,306-byte JPEG gives 187,323. Independent source extraction and Apple's own same-image insert agree. Batch inserts sent "JPEG bytes + 17" and the phone accepted them.
- ID3 `APIC` (CONFIRMED for one capture): Apple's operation for a 54,360-byte JPEG carried 54,361, which is length + 1. A Linux batch sent 54,361 for the same file and the phone accepted it, and AIFF, FLAC and VBR MP3 covers sent with length + 1 verified.
- General rule (CONFIRMED in code, [chapter 06](06-audio-formats-and-gapless.md) section 14.3): the number of pictures plus the summed size of their containers, the full `data` atom for MP4 and the APIC payload for ID3. The image type is not read, so a PNG counts like a JPEG.

### 10.2 Transfer

CONFIRMED.

**Cover with a new track.** `insert_track` carries `artwork_cache_id` and `artwork_checksum`. The phone's single manifest then requests each covered track with `AssetParts` 3 (audio and cover). The host stages each cover at `Airlock/Media/Artwork/<AssetID>`, reads it back, then sends the audio and one `FileComplete` naming the audio path; that one message claims both parts. The staged cover is gone afterwards. `AssetParts` 7 (the Genius bit added) was requested for one track in a batch, and a host must not refuse the extra bit.

**Cover for an existing track.** An ordinary `update_track` with only `artwork_cache_id` and `artwork_checksum` in `item` (399 of these in Apple's incremental sync):

```text
{ operation: "update_track", pid: <existing PID>,
  item: { artwork_cache_id: 677, artwork_checksum: 187323 } }
```

- The envelope is the usual one (fresh anchor + 1, timestamp, the unchanged `update_db_info`).
- The manifest requests the track with `AssetParts` 2, or 6 when the phone adds the disabled Genius bit (observed). The client sends the cover the same way, as it does for an insertion asked with 7 (INFERRED). The host writes the raw JPEG bytes, without resizing or wrapping, to `Airlock/Media/Artwork/<exact AssetID>` (bare name, no extension, no audio-style rename).
- Completion is exactly `{AssetID: <request string>, Dataclass: "Media", AssetPath: ""}`, without `Variant` or `Metadata`.
- Unrelated artwork (part 2) and Genius (part 4) requests for other tracks are answered with `FileError` code 16 (`Unavailable`), `ErrorIsVisible` false. The native host agent returns 16 for an artwork-only request when no cache ID exists and for a Genius-only request when data is unavailable.
- The session ends with `SyncFinished`.
- A host that leaves after the manifest, before its `FileComplete`, still has the metadata applied (observed): the anchor moves by one and the `artwork_token` of the track and of its album name the new cache ID, with no `artwork` row for it, while `best_artwork_token` keeps the old cover. The next cover update moves both tokens to its own cache ID, and the dangling ID leaves the catalogue (observed).
- Observed: of 1021 AFC exchanges, the only writes were the operation file (290 bytes), the signature (21) and the bare JPEG. No audio was written, renamed or removed. The device log reports original-art import from the staging path and token 677, five generated rendition sizes, and Media transfer success. It also logs a generic "Cannot move asset outside of AFC root" diagnostic while handling the empty `AssetPath`, followed by success through the explicit artwork path. This diagnostic is benign; do not change the empty path to silence it.

**Allocation of new cache IDs from a Linux host.** The ArtworkDB file was byte-identical before and after covered batch inserts, so the phone does not allocate IDs there. Batch IDs 714, 715, 716 were chosen "above every saved, registered and counted ID". Cache IDs must be reserved by the sender, avoiding the union of: the saved ArtworkDB counter, every `artwork_token` and `artwork` ID the live database holds, and IDs the sender has already used.

### 10.3 Where artwork lands

CONFIRMED. For a traced local-art update only five of the then 44 tables changed: the properties, `artwork`, `artwork_token`, `best_artwork_token`, `entity_revision`.

| Table | Rows for a covered track | Rows for a covered album |
| --- | --- | --- |
| `artwork_token` | `entity_type` 0, `artwork_type` 1, `source_type` 300, `variant` 0, token = the cache ID as a canonical decimal string ("677") | `entity_type` 4, `artwork_type` 6 |
| `best_artwork_token` | one row naming the track's own cache ID | one row naming the album's current cover |
| `artwork` | one row per cache ID | An `artwork` row whose token an album uses has `artwork_type` 6 (291 such rows in the baseline library) |

- Source type 300 is the type of the local cover art in the traced branch. Source 500 appears for artist images (for example 17 artist images). The track-token to cache-ID equality was verified only for source 300 (399 non-zero baseline tracks plus the new one, no mismatch, no missing identity, no duplicate join). Do not extend it to other sources.
- A covered track gets a track token. Its album gets its own token naming one member's cover. The artist has no artwork link in this branch.
- Tokens are not tied to ArtworkDB. The live namespace to consult is both tables: `artwork_token` (source 300) had 301 unique IDs while `artwork` retained 577 IDs, including 277 older cached IDs no longer attached to current tracks. Their union was exactly 100 to 677. A reader must inspect both tables, reject malformed or non-canonical tokens, and combine them with saved state and any reservations.

### 10.4 Originals and renditions

CONFIRMED (artwork read-back).

- Permanent original: `iTunes_Control/iTunes/Artwork/Originals/<relative_path>`. It holds exactly the staged JPEG, of any kind (baseline, progressive, greyscale). Two tracks with equal image bytes got separate originals.
- A staged PNG is not kept: the phone replaces it with its own baseline JPEG of the same raster, transparency flattened onto white ([chapter 06](06-audio-formats-and-gapless.md) section 14.4). Nothing in the database records the source format.
- Renditions: `iTunes_Control/iTunes/Artwork/Caches/<edge>x<edge>/<xx>/<hash>`. The phone generates several sizes (five in the log of the existing-track update). A 556×556 rendition was typical for square covers. In one case a decodable 531×531 derivative of a 16,176-byte JPEG was 23,949 bytes. A generated 48 kHz mono MP3 with a small cover produced a 144×144 rendition.
- Non-square covers keep their aspect: a 720×719 JPEG produced a 556×555 file under `Caches/556x556`. The phone fits the longer side to the cache edge. The original still reads back exactly.
- **Grayscale covers get an empty rendition.** A 500×500 progressive grayscale JPEG (one component, sampling 2×2, quality 80) had no 556×556 or 531×531 rendition. The phone created `Caches/480x480/<xx>/<hash>` and left it at 0 bytes, still empty after 11 minutes and across two later reads. An 800×800 grayscale cover (sampling 1×1) got its 556×556 rendition empty. The library holds four grayscale covers. Colour covers, progressive or baseline, rendered normally. The cover shows in the Music app and the track plays normally. Which property of the image causes this, and whether the phone ever fills the rendition later, are OPEN.

### 10.5 Which cover an album shows

CONFIRMED.

- An album takes the cover of its **last covered member in operation order**, even when an earlier member became its representative. A later member without a cover does not clear it (insert batch with one covered and one uncovered member).
- A cover update to one member lets the album take the new cover and drop a dangling earlier ID.
- A covered track inserted into an album that already has live members and a cover lets the album take the new cache ID, as an insert into a new album does. When the new cover had the same bytes as the album's, the album then named the new ID for the same image. After those members were deleted the album kept the registration, and its `artwork` row stayed.
- When a covered track is inserted into a previously emptied album, the album takes the new cover and the old registration leaves the catalogue. Seven albums that carried the IDs of a batch that never arrived (a "dangling" cover, IDs 728 to 735 without an `artwork` row) each took the last member's new cover on refill, and the old ID left the catalogue entirely. The earlier members' cached objects are typed 1 (track-level) and remain.
- The `artwork` rows of a replaced album cover stay in the database, now unreferenced (383, 521, 530 in one test). Whether the phone ever removes them is OPEN.
- After a removal session, an album's cover tokens (entity type 4) stay for albums that no longer exist; the phone drops such tokens at the following session. A removal session deleted every `artwork_token` and `best_artwork_token` row whose entity was already gone when it began (83 rows each: 81 album covers plus one item-artist and one album-artist image). The album tokens of groups that session itself collected were kept until the next session. All 644 `artwork` rows stayed.
- An insert batch killed mid-upload leaves the not-yet-uploaded members with **registered cover tokens and no `artwork` row**. The album's token moved to the last member's (723), which never arrived, but its `best_artwork_token` stayed at the old cover (530), so the album still showed its old image. After the compensating removal the album's token stayed at 723 until a later cover update replaced it (724) and the dangling 723 left the catalogue.

### 10.6 Recent counters and artwork imports

CONFIRMED. Registering covers resets recent play and skip counters. A cover import changed `play_count_recent` values 1, 3, 4 to 0 on the tracks with such counts; another changed two `skip_count_recent` values from 1 to 0, with lifetime counts and dates exact. Seven further cover imports on a library with no positive recent counters showed no change. Treat "recent counters all zero afterwards" as part of any import that registers artwork, and treat lifetime counts and dates as preserved.

### 10.7 Artwork-only repair

CONFIRMED. A track inserted with checksum 0 and no cache ID (an audio-only insert whose M4A still carried its embedded JPEG) plays normally but has no cover: zero artwork relationships for the track and its collections. The repair is the existing-track update of section 10.2: cache ID 677 plus checksum 187,323, `AssetParts` 2, bare staged JPEG, empty-path `FileComplete`. All 401 tracks' joined metadata, paths, identities, availability and playback counters stayed exact; 39 of 44 tables were identical. Cover display was reported after restarting the Music app; no screenshot captured it.

## 11. Statistics and playback history

### 11.1 Fields

CONFIRMED. A track operation carries an `item_stats` dictionary with plist dates `date_played` and `date_skipped`, integer play and skip counts (`*_user` for lifetime, `*_recent` for recent), and a boolean `has_been_played`. A new Linux-inserted track sends zero statistics. Database columns identified:

- `item_stats.play_count_user` (lifetime play count; a fixture with `play_count_user` 5 is recorded).
- `item_stats.play_count_recent`, `item_stats.skip_count_recent`.
- Played, skipped and accessed dates, and a rating, are preserved by every operation seen but their column names are not given. A lifetime skip count `skip_count_user` is INFERRED by symmetry.
- The statistics revision of an item is a separate `entity_revision` row, class 0, revision type 1. It is removed when the item is deleted and removed in bulk by the phone's background renumbering.

### 11.2 Recent counters

CONFIRMED.

- Every metadata session zeroes all non-zero `play_count_recent` and `skip_count_recent` values (first session after a break: 65 items). Lifetime counts, played, skipped and accessed dates, ratings and all other statistics columns stay exact.
- A delete session reset every non-zero surviving recent counter (16 rows: 15 recent-play counters totalling 59 and one recent-skip counter). Rows that were already zero stayed zero.
- A basic MP3 insert reset recent counters on 54 prior rows, all 1 to 0.
- Prior receiver logs explicitly report resetting play counts at metadata-import commit and canonicalising collection representatives (the first signed insert, the audio retry and the artwork registration). This is the explanation (INFERRED); no statistics export or acknowledgement exchange was captured.

A verifier should accept "unchanged" or "complete reset of both recent counters" and refuse partial resets, increases, and any change to lifetime counts, dates or ratings.

### 11.3 Playback happens between syncs

CONFIRMED. Music is played on the phone between sync runs. Play and skip counts, recent counters, date-played and the related statistics revisions will therefore differ from any state saved earlier. Separately, the phone renumbers all item revisions and drops statistics revision rows a few minutes after a sync even with no playback (section 3.4). No playback happens during a single run's capture, write and verify window; changes inside that window are unexpected.

### 11.4 Playback History and library containers

CONFIRMED.

- Three built-in containers exist: the primary library playlist (named after the device), Photos Memories and Playback History. The primary container holds a `container_item` row for every track (399 rows in one library). Playback History held 250 rows.
- Playback History has no wire sync ID, and no host operation creates or edits it. Its rows appear to be written by the phone's own playback (INFERRED). Do not manufacture history rows.
- Tracks that had been played had Playback History entries (one track had one, another two). Deleting a track does **not** remove its `container_item` rows in the primary container or in Playback History. Afterwards three Playback History rows pointed at deleted items. `container_item_media_type` and all container rows were unchanged, and a capture several minutes later showed no further change.
- This differs from ordinary playlists, whose member rows are removed by `delete_track` (see chapter 05).
- Whether `delete_playlist` works on the built-in containers was not tested (OPEN).

### 11.5 Statistics in the host

The host's modified-track path (`0x469b04`) and the `PlayCounts.plist` consumer (section 2.1) are the places where Apple's client reads back play and skip counters. The phone-side export path was not observed (OPEN).

## 12. What survives deletion

Deletion effects on the database belong to `05-receiver-operations.md`; this section lists only what remains in the data model.

CONFIRMED:

- A `delete_track` operation removes the target's rows in `item`, `item_extra`, `item_store`, `item_stats`, `item_playback`, `item_search`, `item_state`, `item_video`, `chapter`, `lyrics`, plus its `artwork_token` and `best_artwork_token` rows (entity type 0). The item's live revision becomes a tombstone (`deleted` = 2) and its statistics revision row (type 1) goes. Representatives of groups it led move to the lowest surviving member.
- Rows that stay: the track's `container_item` rows in the primary library container and in Playback History; emptied albums, album artists, item artists, composers and genres (representative 0); cover tokens of emptied albums until the next session; every `artwork` row; the track's cached original and renditions were not reported removed; `sort_map` rows; empty playlists after their members are removed; an orphan `container_item_media_type` count row after `delete_playlist`; empty test-named item-artist rows.
- The phone removes the audio files of deleted tracks itself, after the session.
- A one-track deletion in a library whose survivors included two MP3 files changed 15 of the 45 tables and left 30 exact. All 650 membership rows and five containers were preserved. MP3 survivors need no special handling in the data model.
- Removal sessions of 16 to 499 targets collected the groups they emptied, and a 100-target one collected every empty group and dropped orphan tokens. Sessions of up to 8 targets, and one of 90 that emptied the library, collected no group (section 7.6).
- In none of the observations did the phone reclaim the library-container rows, the Playback History rows, the unreferenced `artwork` rows, or the cached renditions. Reclamation by the Music app, a maintenance task or a later sync was not seen within the observation windows (minutes after each session). Whether it ever happens is OPEN.

## 13. Notes for implementers

These recommendations are explicitly supported by the observations above.

1. Take the "before" state from a fresh read-only capture (database plus WAL, stable across two reads) in the same run. Never trust an older saved state for playback fields, item revisions or statistics revision rows.
2. Join through `item_store.sync_id`, `album.sync_id` and `album_artist.sync_id`. Never use internal row IDs or `item_artist.sync_id`.
3. Reuse an existing album and artist by PID. For a new track that belongs to an emptied group, use the group's old PID. Within one batch, share one reserved album and artist PID among its tracks.
4. Resolve groups by exact name or, for ASCII names, case-insensitive equality; examine the whole match set. An exact candidate plus a different-ID case alias is ambiguous, not permission to choose the exact one. A zero-match result permits considering a new collection; it does not prove the phone would not match one by locale rules.
5. Preserve source spelling in track metadata. Reuse does not authorise renaming a collection.
6. Project multi-value MP4 and ID3 tags to their first value, as Apple's readers do. Do not join them. A FLAC, which Apple's host does not read, is the exception: join its repeated `ARTIST` and `COMPOSER` entries (and their `ARTISTSORT` and `COMPOSERSORT` entries) in file order with ", ", and refuse any other repeated field (section 5.3).
7. Compute sort fields: explicit sort tag, else the name with a leading "The " or "An " removed (whether Apple also strips "A " is not known). Always send `sort_name`, `sort_album`, `sort_artist`; send `sort_album_artist` exactly with `album_artist`, `sort_composer` exactly with `composer`. Cap text at 255 UTF-16 units.
8. Send `album` always together with `album_pid`; send `artist` or `album_artist` edits only through the representative rules in section 8.4.
9. Send `is_compilation: true` only for a valid `cpil` of 1, and preserve an explicit `aART` (including non-Latin). Do not translate it or derive it by splitting track artists. A compilation without `aART` was not tested.
10. Send `disc_number` 1 when the source has none; the phone stores 1 regardless.
11. For covers: use the checksum rule per format (MP4 JPEG + 17, single-APIC MP3 JPEG + 1); stage at `Airlock/Media/Artwork/<AssetID>`; complete with the audio path in a batch, or with an empty `AssetPath` for an artwork-only update; reserve cache IDs against the live `artwork` and `artwork_token` union. Send a JPEG: a PNG original is replaced by the phone's own JPEG and can never be read back as sent.
12. Expect recent counters to reset on any session that registers artwork or edits metadata; verify lifetime counts and dates unchanged.
13. A phone Ping after the last `FileComplete` must be answered with `Pong`.
14. Ignore `st_blocks` when comparing file attributes over time.
15. Do not hash files on the phone.
16. Treat a changed audio file under an existing PID as a new track plus a removal, not as an update. Nothing shows that an update or a bare `update_track` makes the phone fetch new audio (section 9.6).
17. Read the phone's container rows from a fresh capture before editing a playlist. Send the complete ordered member list at creation. A playlist's SQL `filepath` is not evidence of a file.
18. Recompute nothing the phone derives: `sort_key`, orders, sections, integrity values and dates are receiver outputs. Verify them relationally.

Client-side policy built on these rules (planning, verification and recovery) is in [chapter 07](07-sync-engine-design.md).

## 14. Open questions

- OPEN: the complete list of 45 tables and their columns; only the tables and columns named above are documented. Column names holding gapless fields (delay, drain, resynch, heuristic) and `skip_count_user`, dates and rating are not recorded.
- OPEN: the contents of Apple's article table (only "The" and "An" seen; "A" unknown), and the exact character set of class 0x3c.
- OPEN: how Apple Music maps Vorbis comments, RIFF INFO and AIFF text chunks to comment and sort fields, and what it makes of a repeated Vorbis field (joined, first or last); and how the host treats newlines in comments (the phone stores them).
- OPEN: whether the phone stores values above 255 UTF-16 units.
- OPEN: effect of clearing a comment with `""`, of `""` as a sort, and of a sort-only `update_track` on a shared artist, album or album-artist name.
- OPEN: how Apple's own sender clears a genre or composer (no capture).
- OPEN: the two-session move of a non-representative artist or album artist.
- OPEN: whether the Windows client avoids the in-place rename on artist or album-artist edits (no Windows capture of such an edit exists).
- OPEN: the exact rule for recomputing representatives when an item joins a group; whether a composer's representative moves to a covered member on delete.
- OPEN: whether the phone ever removes unreferenced `artwork` rows or fills empty grayscale renditions; which property of a grayscale cover causes the empty rendition; what the phone keeps of a CMYK or 12-bit JPEG original.
- OPEN: why the phone dropped exactly the first two manifest rows when a session was abandoned.
- OPEN: what triggers group collection (16 to 499 targets collected; up to 8 targets, and 90 targets that emptied the library, did not). The phone's routine for it is known (chapter 09, section 9); the condition under which an import runs it is not.
- OPEN: whether the phone detects a truncated file from `location.file_size`; what bit `0x20` of `[handler+0x2e02]` means (whether the host's resume hash runs for a modern iPhone); where the host persists anchors and its device mirror.
- OPEN: how the phone's background renumbering is triggered, and what `MLMaintenanceTaskOperationDate` governs.
- OPEN: whether leftover container rows (primary library, Playback History) and unreferenced artwork are ever cleaned.
- OPEN: a title edit to a label already present in `sort_map`; where the phone stores `container_ui` values; what the `item_extra` integrity value encodes.
- OPEN: the receiver request that makes the phone fetch new audio for an installed PID, and the meaning of the host's "store version" (section 9.6).
- INFERRED only: ICU-like behaviour of `sort_key` beyond Latin text; `name_section` for non-Latin scripts.

## 15. Evidence

All evidence in this chapter comes from one phone (iPhone17,2) and was gathered on iOS 27.0.1 (build 24A446) except the last capture, on iOS 27.2 (build 24B119). It falls into four kinds.

**Phone database captures.** Read-only copies of `MediaLibrary.sqlitedb` and its WAL (section 2.2). Each receiver experiment compared database plus WAL before and after across all 45 tables (44 in the earliest captures). Main series:

- Baseline library of about 400 tracks; existing-track artwork registration; a Linux ALAC insert that the phone removed when the connection closed before the audio, then its audio; a compilation fixture.
- Playlist creation and member deletion; cover imports that reset recent counters; a re-add of a deleted track's source.
- Track-number edits (1 to 2 and back); MP3 insert and delete; a one-track deletion with MP3 survivors.
- MP3 transfers with WAL growth between stat and copy.
- 41 metadata-only sessions (title, track, disc, year, genre, composer, album, artist, album artist, compilation, content rating, copyright) and the first batch inserts.
- The 100-target removal session; group collection; background renumbering; representative moves; kill-and-recover inserts.
- `delete_playlist`; `delete_album` and `delete_artist` ignored.
- Six field-clearing tests; keep-local bookkeeping; covers on refilled albums; the format runs of [chapter 06](06-audio-formats-and-gapless.md).
- Comments and sort names inserted and updated from Linux; covers named for albums already on the phone; a genre representative moving on delete.
- PNG and progressive or greyscale JPEG covers read back; an unevaluated album recomputed by a metadata session.
- The last capture on iOS 27.0.1 and the first on iOS 27.2: the schema of section 2.3 and the effects of the update (chapter 09).

**Windows traces of Apple Devices 1.1540.23042.0.** Captured under Windows with hooks on the operation builder, the file writers and the ATC channel.

- The reset reference session: Apple's own reset sync. The staged operations were one `update_db_info`, 399 `insert_track` and one `insert_playlist`. The phone asked for 54 assets. The constructor hooks also saw 399 artwork-only `update_track` candidates that were not staged.
- The delta reference session: an incremental sync that added one ALAC track (Track G). The staged operation file (21,916 bytes, revision 10) held one `update_db_info`, one `insert_track` and 400 `update_track` operations: 399 artwork-only updates for existing tracks and one carrying statistics.
- A captured MP3 insert (Track R). Together with the two reference sessions it supplies the 400 unique `insert_track` dictionaries behind section 5.1.
- A tag survey of 796 ALAC source files (four carried a `©cmt` comment, none carried sort atoms).

**Static analysis of the host binaries.** AMPDevicesAgent.exe 1.6.4.90 (Apple Devices 1.1540.23042.0, SHA-256 `ad957f32107b7177969b3cb11b91e3922d27e622b53f9240b65cb457522111f9`) and 1.6.4.94 (Apple Devices 1.1540.24088.0, SHA-256 `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9`). Disassembly was inspected, never executed, for the builder, tag readers, the grouping code, the AFC call sites and the replacement path. Function matching between the two builds is described in section 6.9. `MobileDevice.dll` 1827.100.17.2 and `AirTrafficHost.dll` 4025.400.1.5 and 4026.110.1.6 were searched for hashing and AFC use (section 9.2).

**Controlled Linux experiments.** Operation files built on Linux and sent over USB or Wi-Fi, followed by a full 45-table comparison and a read of the permanent files and cover renditions: the 41-session metadata matrix, the batch inserts and removals, the playlist experiments, the artwork read-backs, the clearing tests, and the comment and sort inserts and updates. Repairs of the three damaged shared groups (section 8.5) are included.

Cover originals and renditions were read back from the phone over AFC. No Apple binary is redistributed. Only addresses, constants and observations are quoted.
