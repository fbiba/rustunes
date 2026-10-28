# Receiver Operations: What the iPhone Does with Each Sync Operation

## Abstract

This chapter records the behaviour of the iPhone's Music library receiver when it is handed the operations that the Apple Devices host sends in a metadata sync session: `insert_track`, `update_track`, `delete_track`, `insert_playlist`, `delete_playlist`, `delete_album` and `delete_artist`. It covers what rows each operation creates, changes or removes, what is applied atomically and what is not, how batches of operations behave, and what remains on the phone when a session is interrupted or abandoned. Almost every statement comes from before-and-after captures of the phone's databases (all 45 tables compared) around a single session. The main findings are these. One session can carry many operations and is applied as one unit, with one revision block. Albums and artists are matched by name, not by identifier. A metadata edit regroups only in narrow cases. Deleting a track leaves its groups, its library-container rows and its cached artwork behind. `delete_playlist` works on this phone, while `delete_album` and `delete_artist` are accepted and ignored. A session killed early is rolled back or applied depending on how far the phone had got. No capture exists of the phone replacing the audio of an existing track; the host-side facts and the safe route are in section 2.

## Scope and provenance

All observations were made on one device.

| Item | Value |
|---|---|
| Phone | iPhone17,2 (iPhone 16 Pro Max), iOS 27.0.1, build 24A446 |
| Host binaries analysed | Apple Devices 1.1540.23042.0, `AMPDevicesAgent.exe` 1.6.4.90; Apple Devices 1.1540.24088.0, `AMPDevicesAgent.exe` 1.6.4.94 |
| Observing host | A Linux host speaking the same ATC session protocol and AFC file transfer as the Windows app, over USB and Wi-Fi |

The host binaries are not redistributed. Virtual addresses below are given with the binary version they were read from. Where no version is given for an address in the `0x1404…` range it comes from 1.6.4.90. Short addresses such as `0x464eb4` are offsets in the 1.6.4.94 image and equal the low part of the virtual address (image base `0x140000000`).

Evidence labels used throughout:

- **CONFIRMED**: observed on the phone, or read in disassembled code.
- **INFERRED**: follows from confirmed facts but was not directly tested.
- **OPEN**: not known.

Where a later observation corrects an earlier one, the later statement is used and the correction is mentioned.

Related chapters: the wire format of each operation and of the surrounding session (ATC messages, asset manifests, `FileComplete`, file staging paths) is chapter 02. The database tables and columns named here are described in chapter 04. This chapter assumes both and does not repeat them.

### Method

Each experiment sent one session, then compared a full capture of the phone's `MediaLibrary.sqlitedb` (with its write-ahead log), `iTunesPrefs`, `iTunesCDB` and `ArtworkDB` taken before and after. All 45 SQL tables were compared row by row. Captures were repeated until two consecutive reads agreed. In one test, two idle captures taken minutes apart without any session were identical. The phone does change some state on its own; see "Changes the phone makes between sessions" below. Audio files were compared byte for byte.

## Common session facts

These hold for every operation kind below. They are CONFIRMED unless marked.

- A session is a metadata-only sync that carries `update_db_info` followed by the operations. `update_db_info` repeats the database pid and `db_info.primary_container_pid` unchanged.
- The envelope is `{revision: anchor + 1, timestamp, operations: [...]}`. The metadata is staged as `iTunes_Control/Sync/Media/Sync_00000000.plist` with a `.cig` signature (chapter 02).
- After every accepted session `MLSyncClientSyncAnchor` is the old anchor plus 1. `MLSyncClientLastSyncedRevision` and `MLArtistHeroImageImportDatabaseRevision` become the highest revision the session wrote. `MLSyncClientGenerationID` takes a new random value. These are the "four sync properties".
- The phone writes the session's revisions as one contiguous block directly above the previous maximum revision in `entity_revision`.
- Accepted sessions finished quickly. A session without audio finished within 0.07 to 0.6 s of the metadata being staged. A four-operation delete session was accepted 91 ms after staging.
- Every session zeroes every nonzero `item_stats.play_count_recent` and `item_stats.skip_count_recent`. This is the phone's import behaviour at the commit of a metadata session (its own log reports resetting play counts) and is independent of the operations sent. CONFIRMED for insert, update, delete, playlist and cover-only sessions. Examples: the first session after a break zeroed 65 items; a one-track delete zeroed 15 recent-play counters (59 plays in total) and one recent-skip counter on 16 surviving rows; a playlist delete zeroed nine tracks; a cover-only session reset two `skip_count_recent` values from 1 to 0. Lifetime counts (`play_count_user`, `skip_count_user`), dates and ratings stay. A sender that decides on these counters must read them before the session.
- A session that carries no `insert_track` may still be followed by an asset manifest. Where this happened, the manifest named only assets the host does not own, and the host declined them with `FileError` code 16 (chapter 02).

### Changes the phone makes between sessions

These are normal and must not be read as damage.

- **Playback.** Playing music changes statistics and play history and can change the revision rows of played items. A played track has a second `entity_revision` row of revision type 1 (statistics). 
- **Background renumbering.** With no session running and no music playing, the phone gave all 399 items new live revisions and removed 336 of 342 statistics (type 1) revision rows, 0.5 to 2.5 minutes after a metadata session. Anchor, last-synced revision and all other tables stayed. A similar refresh was also seen right after a session: all 425 live item revisions advanced to a new consecutive block directly above the latest tombstone, with every other cell unchanged. The trigger is OPEN. A sender that compares revisions between runs must expect them to change.
- **Maintenance property catch-up.** `MLArtistHeroImageImportDatabaseRevision` can move between two sessions with no session in between. In one case it went from 78535 to 79040, the highest `entity_revision` at that time. The anchor (132), the last-synced revision (79038) and the other 44 tables were identical. A sender that insists on byte-identical catalogues between its own sessions reads this as drift.
- **Growing write-ahead log.** The log of the live database grows while the phone is otherwise idle (852,872 to 861,112 bytes between two reads; 8,240 bytes of growth between two captures taken right after a transfer). A copy taken straight after a session can differ from the next copy. Capture repeatedly until two reads agree.
- **Allocation trimming.** Some minutes after writing a file the phone trims its spare allocation (see the insert section).
- **File removal.** The phone deletes the audio files of deleted tracks by itself shortly after the session (see the delete section).

---

## 1. Insert (`insert_track`)

### What is sent

One session carries `update_db_info` plus one `insert_track` per new track. Each `insert_track` has `item`, `avformat_info`, `location.kind` and `item_stats` (field list in chapter 02). The operation names the new track's `pid`, the track's album by `album_pid` and its artist by `artist_pid`. Several tracks of one batch may name the same new album and artist pid, and the phone creates the album and artists once.

A covered track also sends `artwork_cache_id` and `artwork_checksum`. For an ALAC track the checksum is the cover JPEG length plus 17. For an MP3 track it is the length plus 1.

The phone answers with one asset manifest. The host stages each audio file at `Airlock/Media/<id>.m4a` (or `.mp3`), reads it back, renames it to `Airlock/Media/<id>`, and answers with one `FileComplete` per asset that names the permanent path, for example `iTunes_Control/Music/F10/QZXA.m4a`. A cover is staged at `Airlock/Media/Artwork/<id>` first. The permanent original lands at `iTunes_Control/iTunes/Artwork/Originals/<relative_path>`.

#### Smallest records the phone accepted

CONFIRMED (ALAC and MP3). The field tables are in chapter 06. These are the smallest sets that were accepted and stored.

An ALAC track (a 3 s, 44.1 kHz stereo tone): `audio_format` 601, `bit_rate` 1411, `sample_rate` 44100.0 (a real), `duration` 132300 (sample frames), `gapless_encoding_delay`, `gapless_encoding_drain` and `gapless_last_frame_resynch` 0, `gapless_heuristic_info` 1, `analysis_inhibit_flags` 2; in `item` a `total_time_ms` of 3000, `is_song` true, `remember_bookmark` false and `artwork_checksum` 0; every `item_stats` counter 0; `location` holding only `kind` "Apple Lossless audio file". No item dates, no path, no normalization energy.

An MP3 track (49 headerless CBR frames): `avformat_info` with only `audio_format` 301, `bit_rate` 128 (integer), `sample_rate` 44100.0 (real) and `channels` 2; `location` holding only `kind` "MPEG audio file"; `artwork_checksum` 0 without a cache id; both timing scalars, the whole gapless group and the analysis fields omitted. The phone accepted it and stored duration 0 and `total_time_ms` 0.0 (see the timing table below).

A bare `update_track` (only `operation` and `pid`) is what the Apple Devices host queues for an existing track whose transfer is pending. It was never sent to the phone in these experiments.

### What the phone does

**One manifest for the whole batch.** CONFIRMED. Three inserts led to one `AssetManifest` 92 ms after staging. It requested all three audio assets (part 1 each) and nothing else. The session finished 37 ms after the third `FileComplete`, 168 ms after staging. Batches of up to eight tracks behaved the same. Whether the phone ever splits a larger request over several manifests is OPEN.

**Sessions of 200 and 499 inserts.** CONFIRMED (over Wi-Fi). One session of 200 covered ALAC test tones (20 new albums, 4 new artists, 201 operations with `update_db_info`) and one of 499 (50 albums, 10 artists, 500 operations, the most one operation file holds) were applied in full: every row with its file and cover, and the anchor one step each. How many manifests the phone sent for them was not recorded.

**Sort keys spaced out again.** CONFIRMED. After the 200-track session every prior item had new `title_order`, `item_artist_order`, `album_order` and `album_artist_order` values, most also new `composer_order` and `genre_order` values (277 and 350 of 414), and every album artist new `sort_order` and `name_order` values. Each column kept its relative order: no two rows changed places, and equal values stayed equal. The `*_order_section` columns stayed. No earlier insert batch, of up to 100 tracks, changed a prior track's composer order, which was checked each time. A one-track session did the same: the last of 185 one-track sessions in a row, each adding a tone to the same few albums, spaced every prior sort key out again in the same way. So it follows from the space left between sort keys, not from the size of the session. A model of any insert session must let these orders move as long as their order holds.

**Part masks.** CONFIRMED. A track with a cover is requested with `AssetParts` 3 (audio and cover). One `FileComplete` claims both parts. For one track in each of two eight-track batches the phone asked for `AssetParts` 7, the declared 3 plus the Genius bit (4). A sender must accept the Genius bit on top of the declared parts. Why the phone asked for it for one track only is OPEN.

**Rows created for each batch.** CONFIRMED (three tracks into one new album).

| What | Result |
|---|---|
| Counters | `MLSyncClientSyncAnchor` and the other sync properties move once per session. |
| Collections | One album, one album artist and one item artist are created once, shared by the batch, each with the first inserted item as representative. A new album has `keep_local_status` 2. |
| Items | One new item per `insert_track`, with internal pid `max(item_pid) + 1`, `+2`, `+3` in operation order. If the phone holds an earlier tombstone for that pid the row is revived (`deleted` 2 to 0) with a new revision. The sent `pid` is stored as `item_store.sync_id`. |
| Item side tables | The usual rows in `item_extra` (location file name, size, integrity), `item_playback`, `item_search`, `item_state`, `item_stats`, `item_store`, `item_video`, `chapter`, `lyrics`. |
| Sort map | One new `sort_map` row per distinct new name (album, artist, each title). |
| Not created | No `container_item` row is added to the library container. Single inserts behave the same. |

**Revision order.** CONFIRMED. The metadata phase of the session writes the artist revisions first (82039 and 82041 in one run, with last-synced 82044 at the end of that phase). The item revisions follow in operation order (82045 to 82047), and the album revision is highest (82048). For a single MP3 insert the offsets from the previous maximum were: album artist +1, item artist +3, item +5, album +6, last-synced +4, hero image +6.

**Disc number.** CONFIRMED. An insert that names no disc is stored with disc 1. Every track on the test phone has disc 1, including those from sources with no disc tag.

**File names.** Each permanent path held exactly the uploaded bytes.

#### Existing, emptied and renamed groups

- **An existing album with members** keeps its row and its representative. Its revision is bumped (82048 to 82068) when a track joins. CONFIRMED.
- **An emptied album or artist is reused.** CONFIRMED. Before the experiment, the albums and artists of earlier tests were empty (representative item 0, rows kept). Two cases were tried.
  - A track naming the emptied album and artist by their old pids reused both rows: same `album_pid`, same `sync_id`, and the new item became the representative.
  - A track naming new pids but the same names as an emptied album and artist did not create new rows. The phone reused the rows with those names and rewrote their `sync_id` to the new pids (for the album, 4417302958861730216 to 1893650274405916738).
- **Groups are matched by name.** Consequence: a sender that gives an emptied group a new pid makes the phone rewrite the group's identity. A sender must reuse the old pid.
- **Matching ignores ASCII case, and the last spelling wins.** CONFIRMED. One batch filed `Artist E` under one item artist with tracks spelled `ARTIST E`, and the group ended as `ARTIST E`, the spelling of the last member. A later batch added a member spelled `Artist E` and the phone renamed the group back, so a prior track now showed the new spelling. Every member shows the spelling of the last track filed. Album renames are expected to behave the same but were not observed (INFERRED). A planner must read a spelling difference in case as no change.
- **Composers behave differently.** CONFIRMED. Two tracks of one batch had composer `ARTIST B;Artist C` and, later, `ARTIST B;ARTIST C`. Both joined one new composer lookup named `ARTIST B;Artist C` (the first spelling) with sort name `ARTIST B;ARTIST C` (the later one). In the same batch an item artist `Artist B, Artist D` became `ARTIST B, Artist D` (the batch's spelling), while the album artist `Artist B, Artist D` kept its name.
- **Representative on joining.** The first inserted item becomes representative of a new group. A later member does not take over.

#### Mixed ALAC and MP3 batches

CONFIRMED. Six inserts at one anchor: two ALAC tracks into an existing album, two into a new album, and two 128 kbit/s MP3 tracks into another new album. One manifest requested all six assets (part 1 each). The session finished 333 ms after staging and 32 ms after the last `FileComplete`. Every file matched. The MP3 rows match single MP3 inserts: `location.kind` "MPEG audio file", `location_kind_id` 42, `audio_format` 301, bit rate 128, the sent duration. The MP3 files are staged as `<id>.mp3` and completed with `.mp3` paths.

Another run put two MP3 tracks (48 kHz mono 320 kb/s with a cover, and 32 kHz stereo 64 kb/s without one) into one batch under a new artist. Both were accepted.

#### Compilation tracks

CONFIRMED (fixtures; plus one captured Apple Devices reset). `is_compilation` is sent as a plist boolean and only when true. A false value is omitted on the wire and stored as 0.

- A compilation track with an explicit, non-ASCII `album_artist` inserted into a fresh album and album-artist pair created a new album with `all_compilations` 1 and `has_compilation_tracks` true. Nothing else changed.
- An ordinary track with no `is_compilation` that named the same album pid and album-artist pid joined the same two groups. The album kept `all_compilations` 1 although its members were flagged `[1, 0]`. The phone replaced exactly one existing `entity_revision` row, the album's, with a newer revision. The phone did not create a second album or artist because the compilation flag differed.
- Apple Devices' own reset of the test phone holds the same shape: Track K (compilation) and Track T (ordinary) share Album F and its album artist Artist F, with item flags `[1, 0]` and `all_compilations` 1. Track M is the only member of its album and has flag 1.
- Consequence: `all_compilations` is not "all members are compilations". Do not recompute it from member flags. The host matches an album by name and album artist; when an explicit album artist is present it does not compare the compilation bit (selector `0x2d0450`, fallback at `0x2d0efe` to `0x2d0f1a`, 1.1540.24088.0). Without an explicit album artist the compilation bit must agree.
- Missing or empty album artists on compilations, and an ordinary track inserted first followed by a compilation track into the same album, were not tested (OPEN).

#### MP3 durations and timing evidence

CONFIRMED on the phone, with the limits stated.

| Source | Sent | Stored |
|---|---|---|
| MP3 with both timing fields omitted (49 frames, headerless CBR 128) | no duration | `duration` 0, `total_time_ms` 0.0 |
| MP3 with 98 headerless CBR frames (112,896 coded samples) | two integer timing fields | `item_playback.duration` INTEGER 112896, `item_extra.total_time_ms` REAL 2560.0 |
| MP3 with 59 headerless frames (67,968 samples) | submitted coded duration | catalogue total time 1,541 ms |
| MP3 with 54 frames (62,208 samples, tagged) | submitted coded duration | catalogue duration 1,410 ms |
| MP3 whose first frame is an Info frame (9,691 frames, 11,164,032 samples) | total time 253,152 ms | 253,152 ms, as Apple Devices sends it |

Reading the table: the phone stores the coded sample count the host sends. The millisecond total is the sample count divided by the sample rate, with the fraction truncated (1,541 ms from 67,968 samples at 44.1 kHz). A zero or omitted duration is stored as zero. A positive value from a headerless constant-bitrate file is stored as sent.

Timing facts from the source side:

- A complete frame scan gives the exact coded duration: 1,152 samples per MPEG-1 Layer III frame. This is the only duration that was proven to be accepted.
- An Info frame counts as a physical frame in Apple's value. In the Info-frame file the Info header declared 9,690 audio frames and encoder delay and padding 576 and 0, which would give 11,162,304 declared samples. Apple's total time uses all 9,691 frames (11,164,032 samples). The two quantities differ and must be kept apart.
- None of these runs sent a gapless group or tested gapless playback. MP3s sent with the delay, drain, resynch and heuristic word computed by Apple's rule had every value stored as sent, and Track R played through into the next track (chapter 06, sections 12.7 and 13).
- The table covers MPEG-1 Layer III at 32, 44.1 or 48 kHz, one or two channels. MPEG-2 Layer III, VBR files with a Xing or `VBRI` header and a LAME-tagged file were sent and played later (chapter 06, section 12). MPEG-2.5 and free-format files were not tested.

#### MP3 with an Info frame and a cover

CONFIRMED. The reference MP3, Track R (MPEG-1 Layer III, 44.1 kHz stereo, 4,105,105 bytes, first frame an Info frame, an empty comment frame, and a 54,360-byte, 720 by 719 front-cover JPEG) was inserted.

- The phone created the row in a new album (Album T) under an existing item artist and asked for the audio and the cover.
- The row matched what Apple Devices sends: `location.kind` "MPEG audio file", total time 253,152 ms, no comment field. Apple Devices' own record for this file (Track R) carries `duration` 11,162,592 (9,691 frames of 1,152 samples minus a declared delay of 528 and drain of 912) while its millisecond total equals the physical extent of all 9,691 frames. Chapter 06 gives the full group. Trim constants of one file do not carry over to another file.
- The cover original read back exactly. Its rendition was 556 by 555, under `Caches/556x556`. The phone keeps a non-square cover's aspect and fits the longer side to the cache edge.
- Another MP3 with a cover (48 kHz mono) got a 144 by 144 rendition.
- Track R was also deleted and inserted again with Apple's full gapless group. The phone stored the group as sent (chapter 06, section 12.7).

#### Covers in a batch

CONFIRMED. Three covered tones: two into a new album A, one into a new album B. Each `insert_track` named a cache id (714, 715, 716, above every saved and registered id) and a checksum.

- Each covered track got one `artwork_token` row (entity type 0, artwork type 1, source 300) and a `best_artwork_token` with exactly its own cache id. One `artwork` row was added per cache id (632 to 635). Two tones with equal bytes got separate originals.
- **An album takes the cover of its last covered member in operation order**, although the first member became representative. Album A took tone 2's cover (715), album B took 716. Each album has one `artwork_token` (entity type 4, artwork type 6). An `artwork` row used by an album token has artwork type 6.
- A later member without a cover does not clear the album's cover (in the observed batch the covered track came first, the uncovered one last, and the album kept the cover).
- `ArtworkDB` was byte-identical before and after. Cache ids are not allocated there.
- A cover used by no album is typed 1 and is read back like an album's cover. Both keep the same original and renditions.

**Covers for an album already on the phone.** CONFIRMED. A covered track may name its cover in the insert even when its album already has live members.

- An MP3 batch added three tracks to an album with a live member, without covers. A fifth covered MP3 then went into the same album with its cover in the batch: the cover verified, and the album took it.
- An MP3 and an AAC whose cover bytes equalled the album's existing original, and a FLAC with another cover, were added to an album already on the phone. The first two named their covers (two new cache ids); the FLAC was sent without one. Both covers verified, and the album then registered the second new cache id, whose bytes were the same image it showed before. After the three tracks were deleted, the album kept that registration, and the phone kept its `artwork` row.

**Cover image formats.** CONFIRMED. The phone keeps a JPEG original of any kind byte for byte, but replaces a PNG original with its own baseline JPEG of the same raster (chapter 06, section 14.4).

**Albums refilled after being emptied.** CONFIRMED. An emptied album keeps its old cover registration. When new covered members arrive, the album takes the last member's new cover in place of the old one. The old `artwork` rows stay, now unreferenced. If an earlier batch left the album pointing at a cover id that never arrived (a "dangling" id with no `artwork` row), the refill replaces it and the dangling id leaves the catalogue entirely. A single cover update for a refilled album behaves the same.

**Grayscale covers.** CONFIRMED. For a progressive, grayscale 500 by 500 JPEG (one component, sampling 2 by 2, quality 80) the phone made no 556 by 556 or 531 by 531 rendition. It created `Caches/480x480/<xx>/<hash>` and left it at 0 bytes, still empty 11 minutes later and at two later checks. The original read back exactly. The Music app shows the cover and plays the track. A second grayscale cover (800 by 800, sampling 1 by 1) got a 556 by 556 rendition that was also left empty. So any grayscale cover gets an empty rendition at whichever size the phone picks. Colour covers in the same batches rendered normally. Which property of the file triggers this, and whether the phone later fills the rendition, are OPEN. Of 399 tracks in the test library, four have grayscale covers.

#### The phone trims a new file's allocation later

CONFIRMED. A track written by one batch differed in a later batch's check only by `st_blocks`: 40904 to 40656 (from 128,491 spare bytes to its 20,814,357 bytes rounded up to 4 KiB). Size, mtime, birthtime, path and the phone-computed SHA-1 were unchanged. A check that a file is unchanged must ignore `st_blocks`.

#### The phone pings during a batch

CONFIRMED. After the eighth `FileComplete` of an eight-track batch, while the phone imported the files, it sent `{Command: Ping, Session: 2, Type: 0, Id}` instead of a Progress or `SyncFinished`. A host that only expected `Pong` and `SyncFinished` abandoned the session. The phone finished the import anyway: all eight rows held their files and covers and played. The correct reply is `{Session: 2, Command: Pong}`, the shape the phone uses for the host's own Ping, followed by continued reading.

### Interruption and recovery

**A batch whose host stops after the first file.** CONFIRMED. Three tones of one new album. The host completed the first asset, then stopped answering and dropped the session 5 s later.

- The anchor still moved (202 to 203), and all three rows exist, with the planned internal pids and the sent sync ids, in one album whose representative is the completed item.
- The completed row is an ordinary installed track (location `QZYA.m4a`, 102,366 bytes, base location 3850, F10).
- The two unfinished rows have no file location: `base_location_id` 0, `location` empty, `file_size` 0, no non-purgeable asset, still marked library-added and supported. No file exists at their planned paths.
- A later capture shows the same rows. The phone neither fills nor removes them by itself.

**Rows without audio ("locationless rows") and their removal.** CONFIRMED. One metadata-only session with `delete_track` for the two locationless rows removed them (404 to 402 items, anchor 203 to 204). The completed track, its file, the album and its artists were unchanged. The session's only manifest held two foreign requests that the host declined. Nothing asked for the deleted rows' audio. The before and after databases matched the ordinary 45-table removal model, so no special operation is needed. A recovery that wants to undo a partly applied batch sends `delete_track` for exactly the rows without audio.

**A batch killed mid-upload.** CONFIRMED.

- Seven covered tracks were re-added in two batches. The second batch (three tracks of one album) was killed with SIGKILL as its second file began, after one `FileComplete`. All three rows existed. The completed row held its audio and cover. The other two had no file location but did register their planned covers as track tokens, with no `artwork` row. The album's token moved from its old cover to the last member's planned cover, which never arrived, while its `best_artwork_token` stayed on the old cover, so the album still showed its old image. No cover stage was left in `Airlock/Media/Artwork`.
- Four covered members of a new album went up in reverse operation order (the last member first). The process was killed 47 s in, as the second upload began, 1.2 s after the first. The album already registered the first-uploaded member's cover, so it pointed at a cover that had arrived. The other three rows had no location and no artwork. Removing those three rows left the surviving track as representative with the album keeping its cover. That album stayed at `keep_local_status` 0 with a stats revision row (see deletes).
- Four covered MP3s of a new album, about 10 MB each, killed 45 s in as the second upload began: one track had arrived, and the other three rows had no file location. One `delete_track` session removed the three, as for ALAC.
- Files uploaded and renamed into `Airlock/Media` but never completed were not claimed later. In one case two such files stayed unclaimed until removed by hand. The kills at chosen messages below left three more (two partial audio files, each a whole number of 1 MiB writes long, and one cover); at later checks none had been claimed or cleaned, and they were removed by hand. Whether the phone ever claims such files is OPEN.

**Kills at chosen messages, one track and mixed batches.** CONFIRMED (over Wi-Fi). The host process was killed when a chosen phone message arrived after `FinishedSyncingMetadata`, or after a chosen number of 1 MiB writes.

| Session | Killed at | Phone state afterwards |
|---|---|---|
| One-track insert | The phone's first `Progress`, 54 ms after `FinishedSyncingMetadata` | Rolled back: anchor unchanged, no new row, staging and `Airlock` empty |
| One-track insert (12.8 MB ALAC) | 300 ms after `AssetManifest`, 2 of 13 MiB sent | Applied: the row exists without a file location |
| One ALAC and one MP3, both covered | 300 ms after `AssetManifest`, before any audio | Applied: both rows exist without file locations. Unlike the abandoned session below, the phone dropped no row |
| The same shape | The fourth 1 MiB write of the ALAC, after the MP3 and its cover had arrived | Applied: the MP3 complete with its cover, the ALAC row without a file location |

In each applied case one `delete_track` session for the rows without audio returned the library to its earlier state. The same ALAC and MP3 shape, not killed, finished in one session.

**An album the killed session left unevaluated.** CONFIRMED. The phone sets a new album's `keep_local_status` to 2 at the end of the session that created it. The last batch above, killed mid-upload, left its new album at 0 with a statistics revision row. The next session of another kind finished that work (section 2). An emptying delete does the same (section 3).

**An abandoned session drops its first rows.** CONFIRMED. An eight-track covered batch at anchor 226 went into a library that had just been emptied. The phone's manifest named all eight assets, the first two ahead of the rest, and asked `AssetParts` 7 for one of them. The host refused that part mask as undeclared and abandoned the session before uploading anything.

- The anchor still moved (226 to 227). Six rows existed without file location, as in the interrupted case above.
- The two rows the manifest named first were gone: their item revisions were tombstones (`deleted` 2), written after the other items' revisions, and no file existed at their planned paths. Why exactly these two is OPEN.
- Their groups stayed. The new albums had representative 0, each holding its own member's cover token and a new revision after the tombstones. The album artist, item artist and genre planned only for the first of them stayed with representative 0.
- The session deleted every token row whose entity was already gone: 89 album tokens, 17 artist images (source 500) left by earlier deletions, and all 106 `best_artwork_token` rows. Every `artwork` row (644) stayed. This matches what a removal session does with such tokens.
- Six locationless rows were then removed by one `delete_track` session.

**A phone restart deletes `Airlock`.** CONFIRMED. After a restart the staging directory was gone and the first insert was refused by the host side with "required directory absent" until the staging folders were created again. A sender must create `Airlock/Media` and `Airlock/Media/Artwork` after the session has started and before the first upload, and must not assume they persist.

**The permanent filename must match the codec.** CONFIRMED. An MP3 asset whose `FileComplete` named an `.m4a` permanent path was refused with "unsupported permanent filename". The row stayed without audio until a later `delete_track` removed it. MPEG audio needs a `.mp3` permanent path and ALAC needs `.m4a`; see chapter 06 for the extension of each format.

### Observed runs

| Batch shape | Outcome |
|---|---|
| 3 ALAC, one new album and artist | One manifest, 3 `FileComplete`, finished 168 ms after staging; collections created once |
| 6: 2 into existing album, 2 into new album, 2 MP3 into new album | One manifest, finished 333 ms after staging |
| 2 ALAC into emptied album and artist | Reused by old pid; reused by name with `sync_id` rewritten |
| 3 ALAC, host stops after first file | All 3 rows exist, 2 without location |
| `delete_track` for the 2 locationless rows | Removed (404 to 402 items) |
| 3 covered ALAC, 2 albums | `AssetParts` 3; album takes last covered member's cover |
| 7 covered tracks in batches of 4 and 3 into emptied albums | First batch complete; second killed after one file, 2 locationless rows with planned covers |
| 8 covered tracks into emptied library, session abandoned | First 2 rows deleted, 6 locationless |
| The same 8 tracks re-sent with new covers | Phone sent Ping; all verified; dangling covers released |
| 8 covered tracks incl. a grayscale cover | Rendition left at 0 bytes |
| 8 tracks written minutes earlier | `st_blocks` of a prior track trimmed |
| Case-variant artist and composer names across batches | Group takes last spelling |
| 1 MP3 with Info frame and cover (twice) | First: audio refused by the host, row left without audio; second: stored correctly, 556 by 555 rendition |
| 2 MP3 (48 kHz mono + cover, 32 kHz stereo) | Accepted |
| 4 covered members of a new album over Wi-Fi, killed 47 s in | 3 locationless rows; album keeps arrived cover |
| 1 covered and 1 uncovered member of a new album | Album keeps the covered member's cover |
| 11 tracks: 3 ALAC, 4 MPEG-1 MP3, MPEG-2, AAC, WAV, AIFF, with gapless groups, comments and sort names | One session; every value stored as sent |
| 4 covered MP3s of a new album, killed 45 s in | 1 arrived, 3 locationless rows |
| Covered MP3, AAC and FLAC into albums already on the phone | Covers in the batch verified; album takes the new registration |
| ALAC, AAC, FLAC and MP3 with PNG, progressive and greyscale covers | JPEGs kept; PNGs replaced by the phone's JPEG |
| One-track inserts and ALAC plus MP3 batches killed at chosen messages | First `Progress`: rolled back; after `AssetManifest`: applied, locationless rows |
| 16 covered ALAC converted from FLAC in one session; the same in 16 one-track sessions | Applied |
| 200 covered ALAC tones, 20 new albums, in one session (201 operations) | Applied; every prior sort key spaced out again in the same order |
| 499 covered ALAC tones, 50 new albums, in one session (500 operations) | Applied |
| 185 one-track sessions in a row, tones into five new albums | Applied; the last spaced every prior sort key out again in the same order |
| Removals then insertions in one session: 5 + 2; 249 + 250 with covers; 50 + 50 killed after 20 uploads | Applied, one anchor step each; the 499-target one collected every empty group; the killed one left 30 rows without audio |

### Notes for implementers

- Send one session with all tracks of a batch. Several tracks may name one new album and artist.
- Reuse an emptied album or artist by its old pid and compare names ignoring ASCII case.
- Treat the phone's Ping as a request for Pong.
- Accept Genius-bit part masks. Files may be uploaded in reverse operation order; the phone accepted that.
- After an interrupted batch, look for rows with `base_location_id` 0 and remove them with `delete_track`.
- Do not require a rendition file to be non-empty for grayscale covers, and ignore `st_blocks` when comparing files.
- Send `is_compilation` only when true. Do not recompute an album's `all_compilations` from its members.
- Treat a rolled-back or abandoned session as possibly having left a file or rows behind, and read the phone before the next step. Client-side policy (journal, compensation, recovery) is in chapter 07.

---

## 2. Metadata update (`update_track`)

### What is sent

```text
{ revision: anchor + 1, timestamp: <date>,
  operations: [ { operation: "update_db_info", pid: <database pid>,
                  db_info: { primary_container_pid: <library container pid> } },
                { operation: "update_track", pid: <item_store.sync_id>, item: { ... } },
                ... ] }
```

`pid` is the track's `item_store.sync_id`. `album_pid` is `album.sync_id`. `artist_pid` is `album_artist.sync_id`. The item artist, composer and genre have no wire identity; the phone matches them by the grouping key of their name. The metadata is staged and signed as for any metadata session (chapter 02). The session carries no audio and no `FileComplete`.

The Apple Devices builder was read in 1.1540.24088.0 (`AMPDevicesAgent.exe`, builder `0x461a24`, dispatcher `0x469c2c`). The change event `mtri` selects `update_track`. Field selection is by index:

| Index | Item key |
|---|---|
| 2 | `title` |
| 3 | `album` |
| 4 | `artist` |
| 11 | `track_number` |
| 71 | `album_artist` |
| 77 | `album_pid` |
| 78 | `sort_name` (there is no `sort_title` key) |
| 79 | `sort_album` |
| 80 | `sort_artist` |
| 81 | `sort_album_artist` |
| 142 | `artist_pid` |

The builder adds dependent fields on top of the selection (CONFIRMED in code):

- `title` and `sort_name` add `album_pid`.
- `album` and `sort_album` add `album_pid`.
- `artist` and `sort_artist` add `album_pid` and `artist_pid`.
- `track_number` alone adds nothing.

An empty string is serialized in selected-field mode. Nothing in the code shows what the phone does with it; see "Clearing".

### What the phone does

One session may carry several `update_track` operations, including more than one for the same track. They are applied in order. CONFIRMED (41 sessions).

**Revisions.** Every edited item gets a new item revision (class 0). Every group row the session touches gets a new revision of its class. The new revisions form one block. Within the block, all touched groups come first, each once, by class (album artist, album, item artist, composer, genre), then the items in operation order. A session with four updates (a new item artist, a new genre, an album year, a new album) produced new album 87066, year-revised album 87067, artist 87068, genre 87069, items 87070 to 87073. Within a class the order is OPEN. An item updated twice takes a second number; a group joined later in the session takes none.

**New strings** add `sort_map` rows, one per distinct new name or sort name. Existing rows are reused.

#### Item fields that do not regroup

CONFIRMED.

| Sent | Phone result |
|---|---|
| `title`, `sort_name` | `item_extra.title`/`sort_title` and `item_state.title`; `item.title_order` and section from the sort name; `item_search.search_title` from the title |
| current `album_pid` alongside | nothing else |
| `track_number` | `item.track_number` and `item.episode_sort_id` |
| `track_count` | `item_extra.track_count` |
| `disc_number` | `item.disc_number` |
| `disc_count` | `item_extra.disc_count` |
| `year` | `item_extra.year`. If the new year differs from the album's, the album gets a new revision and `album.album_year` becomes the members' maximum year. A year equal to the album's leaves the album alone. |
| `is_compilation` | `item.is_compilation` only; the album row is untouched |
| `content_rating` | `item_extra.content_rating` and `item_state.has_explicit_content` |
| `copyright` | `item_extra.copyright` |

Title change on an inserted fixture: the session returned in 1.33 s with no asset transfer. 38 tables were exact and seven changed: the title, sort title, search and order fields of the item; its live revision; two new `sort_map` rows; and the four sync properties. Album, artist, statistics and artwork rows were unchanged. The title and the sort title had different order values joined to their own new `sort_map` rows. The exact inverse (1.135 s) restored the original values and reused the original sort map. All of these changes used a title and a sort title that differed. A title that equals its sort title is the ordinary case at insert, where the title and the search key share one `sort_map` row. Whether an update of such a title writes one shared `sort_map` row or two is OPEN. Both temporary sort-map rows remained stored but unreferenced. The live revision advanced from 75348 to 75349.

Track number change: `track_number` 1 to 2 with `track_count` 2 unchanged. `track_number` and `episode_sort_id` both became 2. One live revision advanced to the previous global maximum plus one, and the four sync properties advanced. Every other cell, including statistics, groups and artwork, stayed. The inverse (2 to 1) behaved the same.

A composer repair (`composer` and `sort_composer` on one existing track, no audio) changed the shared composer lookup, all eight corresponding item order keys and one search key. A one-pid operation may therefore change many rows when the field belongs to a shared lookup.

#### Clearing fields

CONFIRMED (six sessions on one test track whose album, genre and composer had no other member). Anchors 522 to 528, audio unchanged.

| Sent | Result |
|---|---|
| `year` 0 | `item_extra.year` 0; the album is revised and its year becomes the members' maximum (0 for a sole member) |
| `track_number` 0 with `track_count` 0 | `item.track_number`, `item.episode_sort_id` and `item_extra.track_count` set to 0 |
| `disc_count` 0 | `item_extra.disc_count` 0 |
| `copyright` `""` | `item_extra.copyright` `''` (a track inserted without one has NULL) |
| `composer_pid` 0 | ignored: the item gets a new revision, no cell changes |
| `genre_id` 0 | ignored: same |
| `""` for `composer` or `genre` | **not a clear**: order keys become 0 with section 27, the item keeps its composer and genre |

A genre or composer has no clearing form. Only removing the track and inserting it again without one clears either. How Apple's own host clears a genre or composer, if it can, is OPEN (no capture exists).

#### Group fields

Each group row (`item_artist`, `album_artist`, `album`, `composer`) stores a `representative_item_pid`. That stored value decides what an artist or album-artist edit does.

**Genre and composer: matched by name, always safe.**

- `genre`: the item moves to the genre with that name, or to a new genre row with a phone-allocated id and a class-3 revision.
- `composer` with `sort_composer`: the item moves to the composer with that name, or to a new composer row (class 5). This works whether or not the item is the composer's representative. The old row is never renamed.

**Album: name together with `album_pid`, always safe.**

- `album` + `sort_album` + the `album_pid` of an existing album: the item joins that album.
- `album` + `sort_album` + an unused `album_pid`: a new album row with `sync_id` equal to the sent pid, the item's current album artist, the item's year and a class-4 revision. This happens even if an album with the same name exists, so a duplicate name results.
- `album_pid` alone, without a name, **re-keys the item's current album in place**, changing the sync id for every member.
- `album` without `album_pid` **does not regroup**. The item stays in its old album, but its `album_order` and `search_album` move to the new name. The catalogue becomes inconsistent.

**Item artist: only the representative moves.** With `artist` and `sort_artist`:

- If the item is its group's stored representative, it moves to the group whose grouping key matches the new name, or to a new group. Joining an existing group rewrites that group's display name to the sent text.
- If the item is not the representative, the phone **renames the shared group in place**. `item_artist` and `sort_item_artist` change for every member and the `grouping_key` keeps the old name. A later edit that sends the new name does not find that group and creates a second one with the same display name. Sending `artist_pid` as well did not help.
- What counts is the representative stored at the start of the session. In a two-operation session where the first operation moved the representative out, the second target became the group's only member, yet the second operation still renamed the group in place. Groups created earlier in the same session can be joined by key.
- It is not "lowest item pid": a target with the lowest pid in its group that was not the representative still caused a rename.

**Album artist: only through a full regroup by the representative.**

- `album_artist` + `sort_album_artist` without `artist_pid` never regroups, representative or not. Only `album_artist_order` and `search_album_artist` move. The catalogue becomes inconsistent.
- With `artist_pid` for an item that is not the album artist's representative, the phone **re-keys the shared album-artist row**: `sync_id` becomes the sent pid and the name is unchanged. If the pid belongs to another album artist, two rows share one sync id, which breaks identity assumptions (a reader must refuse such a library).
- The representative with the full set (`artist`, `sort_artist`, `album_artist`, `sort_album_artist`, `album`, `sort_album`, `album_pid`, `artist_pid`): an unused `artist_pid` creates a new album artist with that sync id and the sent name, and a new album and item artist with it. An existing `artist_pid` joins that album artist.
- **A group created earlier in the session is not joined by its pid.** CONFIRMED (over Wi-Fi). Two tracks of one album, each the representative of its own album artist, sent the full set with one new `artist_pid` and one new `album_pid` in a session of 24 updates. The phone created an album-artist row and an album row for each operation, both pairs under the same two sync ids; the item artist, matched by key, was created once. A catalogue reader must refuse the result (two rows share one sync id). Inserts differ: tracks of one insert session that name one new album and artist get one row of each (section 1). A sender lets the first operation create the groups and joins them by their pids from the next session.
- An `album_artist` or `artist_pid` edit also re-resolves the item's `item_artist` from the current artist display name. This showed only after an earlier in-place rename had left a stale key.

The difference between two full-regroup runs that gave different results is fully explained by this rule: one target was the representative of all three groups, the other was not.

**Representative bookkeeping.** CONFIRMED.

- When the representative leaves a group, the lowest remaining item pid becomes the representative. That change is visible only from the next session.
- An emptied group row stays with representative 0. Nothing deletes it. A later move into it by key reuses it, and the joiner becomes its representative.
- Joining a group that has members does not make the joiner its representative, even with a lower pid. A later edit of another member may recompute the representative to the lowest member. The exact rule is OPEN. A sender must read representatives from a fresh capture before each session and never predict them.

#### Tag-only updates for other formats

CONFIRMED. `update_track` is not limited to ALAC and MP3. A FLAC track retagged in place (title and year, audio bytes unchanged) and a fresh AAC track retagged the same way each received one metadata session, and the phone applied the changes as it does for ALAC. For AAC the audio identity has to be derived from the AudioSpecificConfig rather than the whole MP4 sample entry, because a remux rewrites the `esds` bit-rate fields without changing the audio; a sender that hashes the whole sample entry mistakes a retag for changed audio (INFERRED from that run).

#### Changing the audio of an existing track

OPEN on the phone. No capture exists of the phone replacing the audio of a track under an existing pid. The host-side facts below are CONFIRMED in code (`AMPDevicesAgent.exe` 1.1540.24088.0, static analysis; image base `0x140000000`).

- `update_track` does not carry audio. The builder (`0x461a24`) can serialize format fields (sample rate, bit rate, total time, duration, gapless and analysis values) and a full `location` dictionary (`location`, `location_type`, `kind`, `file_size`, `extension`, `date_created`). The full `location` is emitted at `0x46249b` only when the instance's permanent-location status byte (`+0x45`) is 0. The builder has no `replace_audio` or force-download key. Sending changed format fields is not shown to invalidate stored audio.
- The manifest handler (`0x45bd54`) serves an audio request for any pid, new or not. The completion callback (`0x45b538`) allocates a fresh permanent path (`0x45a814`) and never uses the old path as an overwrite target. The transfer is the usual one: `<AssetID>.m4a` in `Airlock/Media`, read back, rename to the bare id, `FileComplete` with the new path.
- A queued transfer for an existing track makes the host emit a bare `{operation: "update_track", pid}`. This happens when the instance's permanent-location status is 10 (assertion text `permLocationStatus == kTrackPermLocationStatus_SyncInProgress`, `0x45d7ac`; notification `0x61647472` from `0x3580f4`, accepted at `0x469da3`). Whether the phone asks for new audio after such an update, when it already holds a file, is unknown. The bare update was never sent to the phone.
- One named host path ("library version is newer", `0x5658b8`, compare at `0x567ca6`) marks the track for deletion, collects it (`0x564c34`, track removal `0x38f7e8`) and only then runs the add loop (`0x568528`, `0x323a54`, `0x3580f4`). That is delete plus add. The compared "version" is a store-version value, not a file time, hash or sync anchor (log string at `0x98bf80`).
- The matcher that decides whether a library track equals a device track (`0x35a28c`, search wrapper `0x35cdec`) compares total time (a window of 500 units either way for formats 0, 0x196 and 0x1f6, otherwise equality), bit rate (a difference is tolerated when track byte `+0xc5` has bit 0x40), sample rate (equal), channel count (zero on either side matches), then kind, track and disc numbers and totals, year and strings. It compares no file size, modification time, bytes or PCM. A mismatch alone does not prove a same-pid replacement.
- The host's operation queue (tail of `0x469c2c`) looks only at the last queued operation and only when its pid is equal. A fully identical dictionary is suppressed.

| Last queued | New | Result |
|---|---|---|
| `update_track` | `insert_track` | The update is removed and the full insert is appended |
| `insert_track` | `update_track` | Merged; stays `insert_track`; new scalars win, prior-only nested fields stay |
| `update_track` | `update_track` | Merged; stays `update_track` |
| `delete_track` | `insert_track` | Both kept, in order |
| `delete_track` | `update_track` | Both kept, in order, bare update included |
| `insert_track` or `update_track` | `delete_track` | Both kept, in order |

A delete followed by an insert is never turned into an update. For a sender the safe route is `delete_track` for the old pid and a new `insert_track` with a new pid (INFERRED; each half is confirmed on its own, and the re-add of the same audio after a delete is confirmed in section 3). Do not overwrite a file under `iTunes_Control/Music` over AFC: a fresh staged path does not protect the old row either. Chapter 07 describes how the client orders the two operations.

### Interruption and recovery

- A metadata session that the host abandons before anything is staged changes nothing (one such first attempt was observed).
- Every complete metadata session sent was accepted. A one-track edit of a comment and a year, killed at the phone's first `Progress`, was rolled back: anchor and rows unchanged. The same edit killed at `SyncFinished` was applied (both CONFIRMED). No half-applied metadata session was observed. See cross-cutting rules for early kills of other sessions.
- **A session finishes an album an earlier session left.** CONFIRMED. The applied edit above touched another album than the one a killed insert batch had left at `keep_local_status` 0 (section 1). That album changed only in `keep_local_status` (0 to 2), its live revision moved to one above the session's own block (the edit used two revisions, the last-synced revision became the higher of them, and the album's revision was one above that), and its statistics revision row (type 1) was deleted. A delete session that empties such an album does the same (section 3). A model of a metadata session must allow this for an album the edit leaves alone.
- Damaged groups were repaired with further sessions: sending the group's current `album_pid` or `artist_pid` with the old name restored a renamed item artist and re-keyed album-artist row; sending the old `album_pid` re-keyed an album back; sending a track's `artist_pid` with its original value removed a duplicated sync id. Group rows then equalled their earlier state apart from newer revisions.
- Test edits left some tracks with inconsistent sort orders. Empty item-artist rows with representative 0 remained after those tracks were deleted, because deleting a track does not remove empty group rows.

### Observed runs

| Shape | Outcome |
|---|---|
| 41 sessions of 1 to 3 updates (fields, groups, repairs) | All accepted in 0.07 to 0.6 s; rules above |
| 4 updates in one session (new artist, new genre, album year, new album) | Group revisions by class, then items in operation order |
| Idle phone after a metadata session | All 399 item revisions renumbered 0.5 to 2.5 minutes later |
| Title and sort title change, then exact inverse | 38 tables exact, 7 changed; sort-map rows reused on restore |
| `track_number` 1 to 2, then 2 to 1 | `track_number` and `episode_sort_id` change; one revision |
| `year`, numbers, counts, copyright, `composer_pid`, `genre_id` cleared | Clearing forms as in the table |
| 4 updates: a comment on two tracks, a sort title on two others | Accepted; stored as sent |
| Comment and year of one track, killed at first `Progress`, then at `SyncFinished` | Rolled back, then applied; an unevaluated album of another session finished |

### Sender recipe

The 41 sessions support the following rules for a sender. Read `representative_item_pid` from a fresh capture before each session.

1. Item fields from the "do not regroup" table are safe on any track, many tracks per session.
2. `genre`, and `composer` with `sort_composer`, are safe on any track.
3. Album: `album` + `sort_album` + `album_pid`, using the sync id of the existing album with the same album, album artist and compilation grouping, or a fresh random 64-bit id unused by any album. Never `album_pid` alone and never `album` alone.
4. Artist: `artist` + `sort_artist` without `artist_pid`, and only when the track is its item artist's stored representative at the session start. A session can move at most one representative per group, so moving a whole group takes one session per member, each moving the then-current representative.
5. Album artist: the full set from the representative rule, only when the track is the stored representative of both its item artist and its album artist.
6. Never send the current group pids with a changed name. Never send a group pid without its name. Clear a year, track number, track or disc count with 0 and a copyright with `""`. Never send `""` for another field.
7. Refuse everything else, including artist or album-artist edits for non-representative tracks.

A multi-session way to move a non-representative track is proposed but not tested (INFERRED): for a group with representative A and target B, session 1 moves A to the new name, session 2 moves B (now the representative) to it and A back to the old name. The old group keeps its key, so A rejoins it. This is OPEN.

---

## 3. Delete (`delete_track`)

### What is sent

`{operation: "delete_track", pid: <item_store.sync_id>}`, after `update_db_info`. Operations of one session are applied in one batch. No audio, no `FileComplete`. Change codes `dtri` and `htri` select `delete_track` in the host (`0x140469dcd`, 1.6.4.90).

### What the phone does to one track

CONFIRMED (first batched delete: four ALAC targets; 45 tables compared).

- **Once per session:** the four sync properties move as usual.
- **Per target:** the item's revision row (class 0, type 0) becomes a tombstone (`deleted` 2) with a new revision. A statistics revision row (type 1) is removed where one exists. This applies even with zero counters (confirmed on a shared-album covered pair).
- **Rows removed** in `item`, `item_extra`, `item_playback`, `item_search`, `item_state`, `item_stats`, `item_store`, `item_video`, `chapter`, `lyrics`, and the target's `artwork_token` and `best_artwork_token` rows (entity type 0).
- **Groups the target represented** move their representative to the lowest surviving member. Groups left with no members keep their rows with representative 0. In a small session no group row is deleted, renamed or revised.
- **Not touched:** the target's `container_item` rows in the library container and in Playback History stay, as do `container_item_media_type` and every container row. Three Playback History rows then pointed at deleted items. A capture minutes later showed no further change. This differs from ordinary playlist members (see below).
- **Audio files.** The files were still present right after the session and absent when checked again minutes later. The phone removes them itself. The host does not unlink them. No claim about reclaimed storage is made. In the one-track playlist-member deletions the target file was byte-exact immediately after the native response and absent in two later canonical stat checks.
- Nothing else changed: no `sort_map` row, genre, artwork or other table.

Order of tombstone revisions: one contiguous block directly above the previous maximum. The order follows the targets' item ids, not the operation order. The first batched delete could not tell the two apart because they agreed. A session whose operations were sorted by track id showed the item-id order. CONFIRMED.

### Batched deletes

CONFIRMED. One session carried 4, 6, 8 and up to 100 `delete_track` operations. Per-target effects are the same. Representatives are recomputed against the survivors of the whole batch.

- Three consecutive sessions of 8, 8 and 6 matched the model in all 45 tables and every target file was gone within the bounded wait (1 min 17 s in total, including checks).
- The verifier must accept the library-container and Playback History rows staying behind. An "empty library" still has them. This resolved a worry that clearing a library would be blocked by those rows.
- Asset absence must be observed after the session ends with a bounded wait, not immediately.

#### Group collection in a 100-target session

CONFIRMED. A session of 100 `delete_track` operations (anchor 222 to 223) also **collected every group no item belonged to any more**:

- 119 albums (63 it emptied plus 56 earlier left empty with representative 0), 89 album artists, 100 item artists, 38 composers and 7 genres. No group with a member was touched, and afterwards no empty group was left.
- Each collected group's live revision became a tombstone, and its stats revision (type 1) was removed where present. Tombstones followed the item tombstones without a gap, by class: items, then albums, album artists, item artists, composers, genres.
- **Cover tokens stayed.** All 311 album `artwork_token` rows (entity type 4, source 300) and their `best_artwork_token` and `artwork` rows remained. For the 81 collected albums that had covers, these tokens now named albums that no longer existed.
- `MLMaintenanceTaskOperationDate` did not change, so the collection belongs to the sync session.
- **The next session drops the orphan tokens.** The following 100-target session collected 185 more groups. It also deleted every `artwork_token` and `best_artwork_token` row whose entity was already gone when it began (83 rows each, including one item-artist image and one album-artist image of source 500). It kept the tokens of the groups it collected itself and every `artwork` row (644).
- **What triggers collection is OPEN.** Sessions of 6 and of up to 8 targets never collected. Other 100-target sessions collected 177 to 221 groups. A 90-target session that emptied the whole library collected none (78 albums, 65 album artists, 66 item artists, 45 composers, 2 genres and 90 album cover tokens remained without members) although it dropped 178 orphan tokens. So "the library is empty" is not the trigger. Sessions of 16 targets (one album and, once, its artist), 32 (two albums and their artist), 200 (20 albums, 4 album artists and item artists) and 499 (50 albums, 10 of each artist) each collected every group they emptied, while a 4-target session left its emptied album and artist (representative 0). All of these were test albums whose tracks shared one artist. The routine the phone uses is known: MusicLibrary in iOS 27.2 deletes collections without members in `+[ML3Collection removeOrphanedCollectionsInLibrary:usingConnection:]`, which it calls from the end of an import among other places. The condition there was not read. The migration of the update to iOS 27.2 collected four groups left empty by earlier sessions (chapter 09, section 9).
- Consequence for a re-add: an emptied album kept `all_compilations` 1 after its members were deleted. When a compilation track (Track K) was inserted into it later, the album kept its pid and `all_compilations` 1 and took the new track as representative. An ordinary track joined the same album in a later batch. A sender must not assume that an album without members is an ordinary album.

#### An emptied album loses its stats revision

CONFIRMED. A delete of five test tracks emptied four albums. Three kept representative 0 and no new revision. The fourth was the album of an insert batch that had been killed mid-upload and then compensated. It was the only album at `keep_local_status` 0 and one of three albums with a stats revision row (class 4, type 1). The phone recomputed it: `keep_local_status` 2, its revision moved directly above the five item tombstones (106606 to 106622), and it **deleted that album's stats revision row**. The verifier must allow this only when the album's revision moves.

#### Coverless artist representative moves

CONFIRMED. A one-target delete (removing a locationless row left by an interrupted MP3 insert) also changed two groups no target belonged to. The item artist and album artist Artist L (members 59, 60, 62) moved their representative from item 59 to item 60. Item 59 was the library's one track without a cover (no `artwork_token` with entity type 0). Items 60 and 62 had covers. No membership, order key, statistic or revision of the group or its members changed. So a delete session moves an artist's or album artist's representative off a member without a cover, to its lowest member with one. Whether an insert session does this too, and which sessions re-evaluate representatives at all, are OPEN.

**Genres too.** CONFIRMED. Six test tones of one album, artist and genre were on the phone: one without a cover, one MP3 without a cover, three covered, and one more without a cover. A delete session removed three of them (the MP3, one covered tone and the last uncovered one). The first uncovered tone, which was not a target, represented all four groups: the album, the item artist, the album artist and the genre. After the session every one of them pointed at the lowest surviving member with a cover. The album and the artists match the rules above; the genre shows that a genre's representative moves the same way. No composer was involved, so whether composers follow the rule is OPEN.

#### Two MP3 targets in one session

CONFIRMED. A session deleted two MP3 tracks (48 kHz mono with a cover, 32 kHz stereo without one) that were the only members of their groups. The per-track model applied to both. Their rows, the one cover's `artwork_token` and `best_artwork_token`, and their audio files went. The four emptied groups stayed with representative 0. This small session collected nothing.

An MP3 target also behaves like an ALAC one when deleted alone: ten item rows removed, three singleton groups' representatives set to 0, live revision replaced by a tombstone at the previous maximum plus one, the generation id changed. The file was retained at first and absent later. Mixed sessions with ALAC and MP3 targets were also accepted.

#### Covered deletes and what the phone keeps

CONFIRMED.

- **Shared album, one member deleted** (two tracks sharing an album, album artist and item artist): the target's rows and its track artwork associations were removed. The album, album-artist and item-artist representatives moved to the sole survivor. All cached artwork rows remained, including the deleted track's now-unregistered cache object. The historical artwork files and the raw `ArtworkDB` stayed byte-identical.
- **Last member deleted:** 28 tables unchanged, 17 changed. Ten item and dependent rows and the target's two artwork associations were removed. Album, album artist and item artist rows remained with representative 0. The empty album retained its artwork association. All cached artwork SQL rows and `ArtworkDB` stayed exact.
- **A played track** (a fixture that had been played): its ordinary and statistics revision rows were replaced by one tombstone. A fresh capture also showed all 425 live item revisions advancing by one block (background renumbering).
- Deleting a catalogue entry therefore establishes no artwork cleanup and no reclaimed artwork storage.

#### What the phone keeps after deletion

| Kept | Notes |
|---|---|
| Library-container `container_item` rows and Playback History rows | Survive for every deleted track; Playback History rows may point at deleted items. Never observed cleaned. |
| Emptied albums, album artists, item artists, composers, genres | Representative 0. Sessions of 16 to 499 targets were seen to collect them; sessions of 4 to 8 targets and one 90-target session did not. |
| Cover tokens and `artwork` rows | Album tokens of collected albums stay until the following session; `artwork` rows stayed in every observation. |
| Cached artwork files and `ArtworkDB` | Unchanged by deletions. |
| `sort_map` rows | Never removed. |
| Orphan `container_item_media_type` row of a deleted populated playlist | See playlists. |
| Audio files | Removed by the phone shortly after the session. Storage reclaimed: not measured. |

#### Re-adding the same audio after deletion

CONFIRMED. After a deleted fixture, the same source was added again. The planner chose a fresh public track pid. The phone reused the old internal item key: its old tombstone became a live revision, and the three retained empty groups' representatives changed from 0 to the new member. Container, history, sort-map and artwork tables stayed exact. A re-add into the retained empty groups left every other track and the library otherwise unchanged. A repeat plan reported the track as kept. Covered re-adds into retained cover registrations, and re-adding after the collection described above, work as described in the insert section.

### Interruption and recovery

CONFIRMED. Killing the host process at chosen points of 100-target and one-target sessions:

| Kill point | Outcome |
|---|---|
| The phone's first `Progress` (34 to 152 ms after `FinishedSyncingMetadata`) | Rolled back: anchor and last-synced revision unchanged, rows present, staging empty. Seen four times (three 100-track sessions and one single track). |
| `AssetManifest` | Applied. |
| `SyncFinished` | Applied. |
| A session of three `delete_playlist` operations, killed at its first `Progress` | Applied. |

So the first `Progress` kill is a rollback for track-delete sessions but not for the playlist-only session. Two of the track sessions followed playback on the phone and two did not, so playback is not the cause.

**Files can be gone after a rollback.** A one-track session killed at its first `Progress` was rolled back in the catalogue, but the track's file `iTunes_Control/Music/F14/TNXQ.m4a` was already gone, in two captures minutes apart. The 100-target rollbacks had kept every file. The phone does not put the file back. A library read that expects every row to have a file then finds an indexed row with no audio. When it was sent again, the phone accepted `delete_track` for a row whose file was gone, and the after-state was the ordinary removal. When in a session the phone deletes a target's file is OPEN (at least by the first `Progress`).

**Session reuse after an error.** No evidence exists of a phone refusing a second session after an abandoned one. In every case above a later fresh session was accepted, and sessions were never resumed in place (OPEN).

### Playlist members deleted with the track

CONFIRMED. Eight ordinary playlists were created from library tracks, then one session deleted nine of their members:

| Playlist | Members before | Removed | Case |
|---|---|---|---|
| 1 | 3 | first | head |
| 2 | 3 | second | middle |
| 3 | 3 | third | tail |
| 4 | 1 | the only member | sole, empties |
| 5 | 3 | middle | middle |
| 6 | 2 | first | head; same track as in 5 |
| 7 | 5 | 2nd and 4th | two apart |
| 8 | 2 | both | pair, empties |

Besides the per-track removal, the phone did the following (45 tables compared):

- **Membership rows.** Each target's `container_item` row in each playlist went (ten rows; one track was in two playlists). Library-container and Playback History rows stayed.
- **Positions.** The survivors closed up in order from position 0. Each kept its `container_item_pid`, item, empty `uuid` and `position_uuid`, and `occurrence_id`. Only rows whose position changed appear in the delta (head: 1 to 0 and 2 to 1; tail: nothing moved; two-gap: 2 to 1 and 4 to 2).
- **Counts.** Each playlist's `container_item_media_type` count became the number of survivors. The row was deleted when the playlist emptied.
- **Media type.** `contained_media_type` went 8 to 0 for the emptied playlists. No other container cell changed, `date_modified` included.
- **Revisions.** Each affected playlist lost both its class-1 revision rows (live type 0 and membership type 1) and got one new live revision. The eight new revisions (91663 to 91670) followed the nine item tombstones (91654 to 91662) without a gap, in an order that followed neither the playlist ids nor the targets. An emptied playlist ends with a live revision only.
- **Keep-local evaluation.** A playlist created two minutes earlier was still at `keep_local_status` 0 and was evaluated in this session (0 to 2) with a new membership revision above everything the session wrote, although none of its members were targets. A fresh playlist has both revisions already. This also happened to a playlist untouched by the batch.
- **The emptied playlist stays** with no members, no count row and `contained_media_type` 0.

Single-track observations agree. The deletion of the sole member of a playlist left the playlist, with only `contained_media_type` 8 to 0, its member and count rows removed, the seed row kept, and its two live revision rows replaced by one full revision. Deleting the tail of a two-member playlist left the survivor's identity, occurrence and position exact and changed the count 2 to 1. See the playlist section for the follow-up cases.

### Clear with playlist members, interrupted

CONFIRMED. A ninth playlist took 42 members spread over the library. Deleting the remaining 390 tracks (members of seven playlists) took six runs of at most 100 targets, with kills.

- Playlists followed the rules above every time. The spread playlist went 42 to 30 to 19 to 9 to 0, closing up each time. No playlist was evaluated for keep-local (all were already at 2).
- Revision block order within one session: item tombstones, then the playlists' new revisions, then the collected groups' tombstones by class. The 390 survivors had new live revisions (written between sessions, above last-synced), so the block began above them.
- 100-target sessions collected 179, 186 and 221 groups. The 90-target final session collected none.
- A restore from the same source then worked (a compilation member joined the emptied compilation album).

### Observed runs

| Shape | Outcome |
|---|---|
| 4 targets, mixed representatives and last members | Model above; containers untouched |
| 8 + 8 + 6 targets in three sessions | Matched model, files gone in the bounded wait |
| 100 targets (two consecutive sessions) | Collected 119 + 89 + 100 + 38 + 7 groups, then 185; tokens dropped by the next session |
| 6 targets (locationless rows) | No collection |
| 1 target, a locationless MP3 row | Coverless artist representative moved |
| 2 MP3 targets | Per-track model |
| 9 playlist members in 8 playlists | Positions closed up; keep-local evaluated |
| 390 tracks in 4 sessions with kills | First `Progress`: rolled back; `AssetManifest`/`SyncFinished`: applied |
| 1 track killed at first `Progress` | Rolled back, file gone; later `delete_track` for the row accepted |
| 100 targets while playlists existed | 177 groups collected; a new playlist untouched by the batch was evaluated for keep-local |
| 5 tracks emptying 4 albums | One album lost its stats revision row |
| 3 of 6 tones of one album, artist and genre | Album, artists and genre represented by the lowest covered survivor |
| 16, 32, 200 and 499 test tracks, each session emptying whole albums | Every emptied group collected |
| 4 test tracks emptying one album | Album and artist left at representative 0 |

### Notes for implementers

- Treat the library-container and Playback History rows of deleted tracks as expected leftovers.
- Do not send `delete_album` or `delete_artist`; the phone ignores them. Expect emptied groups to stay.
- Observe file absence after the session with a bounded wait. Accept a missing file for a row whose delete was interrupted.
- A sender that matches existing tracks to the library should treat an emptied album as an ordinary album that can take any members.
- A sender that removes tracks no longer in a source tree should compare the audio identity (the typed frame layout for MP3), not the whole-file hash: a tag edit that keeps the frame layout leaves the same audio. This is a host-side rule, not phone behaviour.

---

## 4. Playlists

### What is sent

**Creation.** Two operations. The shape (CONFIRMED on the phone):

```text
revision: current_media_anchor + 1
timestamp: <creation time>
operations:
  - operation: update_db_info
    pid: <database pid>
    db_info: { primary_container_pid: <existing primary container pid> }
  - operation: insert_playlist
    pid: <new playlist pid, signed 64-bit>
    container:    { name, date_created, date_modified }
    container_ui: { play_order: 1, album_field_order: 0, repeat_mode: 0,
                    is_reversed: false, shuffle_items: false,
                    has_been_shuffled: false }
    item_to_container: [ <track wire pid>, ... ]     # ordered, complete
```

Track pids are wire pids (`item_store.sync_id`), not internal row ids. The order of `item_to_container` is the playlist order. Unsigned hexadecimal pids are sent as signed 64-bit integers (the bit pattern is preserved, so a pid may be negative). No audio or artwork is transferred: the session had zero asset transfers and two unrelated foreign asset requests declined with `FileError` 16. A 15-frame ATC session was audited.

**Static capabilities.** The same constructor (`0x464eb4` to `0x465a1e`, 1.6.4.94) can emit these conditional fields: `description`, `description_long`, `uuid`, `distinguished_kind`, `parent_pid`, `is_hidden`, `cloud_id`, `liked_state`, `date_liked`, `source_device_guid`, `artwork_cache_id`, `is_external_vendor_playlist`, `external_vendor_identifier`, `external_vendor_display_name`, `external_tag`, `smart_is_folder`, `smart_is_dynamic`, `smart_is_filtered`, `smart_is_genius`, `smart_enabled_only`, `smart_is_limited`, `smart_limit_kind`, `smart_limit_value`, `smart_limit_order`, `smart_evaluation_order`, `smart_reverse_limit_order`, `smart_criteria`, `genius_seed_tracks`. A key's presence in the constructor is not evidence that the phone accepts it. In the ordinary branch, zero `distinguished_kind` and false `is_hidden` may be omitted.

**Update (ordering).** `update_playlist` uses the same constructor with an existing pid and the complete ordered member list (`0x469c2c` dispatch, `0x46a3c2` to `0x46a450`, log string at `0x46a416`). The host compares the candidate with its cached dictionary using `CFEqual` and skips an identical update ("skipping update_playlist operation"). No dedicated add-member, remove-member, append or position-patch operation was found in the retained captures. No `update_playlist` was captured from Apple Devices, and **no `update_playlist` was ever sent to the phone in these experiments**. Whether reordering, renaming or appending by a full-list update works, and whether an identical update is a receiver no-op, are OPEN.

**Deletion.** `{operation: "delete_playlist", pid: <playlist pid>}`. The host builds `delete_playlist`, `delete_album` and `delete_artist` as bare `{operation, pid}` dictionaries, the same shape as `delete_track`. The builders: `140461880(kind, pid)` creates the dictionary (`delete_playlist` returns it at `1404658cc`; `delete_album` at `14046609b`; `delete_artist` from `1404658fc`) (1.6.4.90). The pid is the host's persistent id, which the phone stores as the row's `sync_id`. A playlist created from Linux has `container_pid` equal to `sync_id`. An album's or album artist's `sync_id` is the pid that a track insert named for it. Item artists have `sync_id` 0 (209 rows in one library), so `delete_artist` could only name an album artist.

Operation names are strings at `0x99fe28` to `0x99ff78` (1.6.4.90). The host chooses operations in a change handler at `140469b04` (1.6.4.90). Each branch requires the change's library to be the synced library.

| Change code | Operation | Branch | Extra condition |
|---|---|---|---|
| `dtri`, `htri` | `delete_track` | `140469dcd` | |
| `mtri`, `adtr`, `ALmf`, `ARmf`, ... | `update_track` | `46a17d` (1.6.4.94) | the builder drops a result that holds only `operation` and `pid` |
| `tdap` | `insert_playlist` | `14046a3c2` | playlist predicate `140464d34` |
| `tdpd`, `atmp` | `update_playlist` | `14046a25f`, `140469e13` | predicate; `tdpd` skipped when equal to the cached dictionary |
| `tdrp` | `delete_playlist` | `14046a20d` | predicate |
| `tdaa`, `ALcc` | `insert_album`, `update_album` | `14046a184` | `WORD [rdi+0x64] < 0x800` |
| `tdra` | `delete_album` | `14046a238` | `WORD [rdi+0x64] < 0x800` |
| `t2aa`, `ARcc` | `insert_artist`, `update_artist` | `140469eeb` | same switch |
| `t2ra` | `delete_artist` | `140469ec7` | same switch |

The playlist predicate (`140464d34`) reads as "this playlist is synced to this device". It returns true when `1404c4304(playlist, [handler+0x374])` holds, or when the playlist's kind word is `0x20` or `0x23` and `[handler+0x38c] != 0` and `[handler+0xe018] == 0`. It is the same for insert, update and delete.

**The album and artist switch is off for this phone.** `[handler+0x64]` belongs to the sync handler (it is a per-device value, not a field of the album record). The host sends album and artist operations only when it is below `0x800`. The full-library reset path (`140468330` to `140468420`) sends one `insert_artist` per artist and one `insert_album` per album before the tracks, under that condition. Apple Devices' own full reset of this phone holds `update_db_info`, 399 `insert_track` and one `insert_playlist`, with no album or artist operation. None of the four retained Windows sessions with `update_db_info` has one. So for this phone the value is at least `0x800`, the host never sends it album or artist operations, and the phone derives groups from track metadata. What the value holds was not decoded.

### What the phone does

**Creation.** CONFIRMED (one-member list; two-member list; eight lists).

- One new `container` row and its `container_seed` row (an item-0, order-0 seed sentinel), a `container_item` row per member at positions 0, 1, … with `container_item_pid`, the item pid, empty `uuid` and `position_uuid`, and an occurrence id of the form `<item_pid>_0`, and a `container_item_media_type` row (media type 8, count n).
- The native defaults seen were source-remote 1, editable 0, saveable 0. These are observed values; they prove nothing about cloud provenance or what the Music app shows.
- Revisions: the live revision is written at the new last-synced revision, and the membership revision a little above it (91631 and 91633 in one run). All old container and member rows, track metadata, statistics, empty playlists, audio and covers were preserved: 38 tables exact, seven changed with only the additions and sync-property changes for the two-member list.
- The container's `filepath` names `iTunes_Control/iTunes/OnTheGoPlaylist_<unsigned pid>.plist`. A SQL path does not prove a file exists: AFC stats of the referenced paths found them absent, and no file was read.
- A playlist created minutes earlier was still at `keep_local_status` 0 and was evaluated (0 to 2, with a new membership revision) by the next delete session; the playlists created earlier were already at 2.

**Single-member creation.** CONFIRMED. A one-member playlist: new container, membership at position 0, media type 8 count 1, seed sentinel. Anchor 109 to 110.

**Member deletion by track deletion.** There is no operation that removes a member only; the member goes when the track is deleted. CONFIRMED cases:

| Case | Result |
|---|---|
| Sole member | Playlist stays; `contained_media_type` 8 to 0; member and count rows removed; seed kept; two live revisions replaced by one full revision; 4 collection representatives to 0; target tombstone. Anchor 110 to 111. |
| Tail of two | Survivor's `container_item_pid`, occurrence and position unchanged; count 2 to 1; 28 tables exact, 17 changed; 429 tracks remain. Anchor 115 to 116. |
| Sole member after a prior tail deletion | The playlist, having lost its membership revision earlier, stays; `contained_media_type` 8 to 0; the membership revision stays absent; the shared genre's representative moves to the sole surviving member; 19 tables changed, 26 exact. Anchor 122 to 123. |
| Head, middle, tail, sole, several per list, in several lists | Survivors close up; see the delete section. |

Head deletion with one survivor was planned as a dedicated experiment and is superseded by the head cases of the larger batched run.

**Playlist deletion.** CONFIRMED (sessions of `update_db_info` plus bare deletes).

| Run | Sent | Result |
|---|---|---|
| 1 | `delete_playlist`, empty playlist | Removed |
| 2 | `delete_playlist`, playlist with 2 members | Removed; members' tracks kept |
| 3 | `delete_album`, empty album, by `sync_id` | No change |
| 4 | `delete_album`, same album, by album row id | No change |
| 5 | `delete_album` plus `delete_artist` of its album artist, by `sync_id` | No change |
| 6 | `delete_playlist` ten times, all empty | All removed, tombstones at consecutive revisions in operation order |

`delete_playlist` removed the `container` and `container_seed` rows (and, for the populated one, both `container_item` rows). No `item` row, indexed file or other playlist changed. The playlist's class-1 `entity_revision` rows were replaced by one tombstone (`deleted` 2) at the new last-synced revision. The four sync properties moved. The populated playlist's `container_item_media_type` row (media type 8, count 2) **stayed** as an orphan without a container, and was still present in later snapshots. The removal of the orphan row, and its effect on a later `insert_playlist` with the same pid, are OPEN.

`delete_album` and `delete_artist` did nothing except move the anchor and generation id. No revision was written, and the rows stayed with representative 0. The sessions completed normally. This matches the disassembly: the host sends them only for a per-device value under `0x800`. Emptied groups can therefore only be removed by the phone's own collection (see the delete section).

A session of three `delete_playlist` operations for emptied playlists, killed at its first `Progress`, had been applied: three containers and seed rows gone, tombstones 96988 to 96990 in operation order, anchor 481 to 482, nothing else changed.

**Which playlists can be refused or deleted.**

- On the phone, only ordinary synced playlists were tried: `sync_id` equal to its id, source-remote 1, no smart criteria, not smart, not folder, hidden, distinguished, collaborative, external-vendor or cloud, and not the library container. Whether `delete_playlist` works on a playlist made on the phone (`sync_id` 0), a smart playlist or a folder is OPEN.
- Strings in the host (1.6.4.90) show its own rules: `sync> ignoring attempt to delete primary playlist` (`9d2318`), `sync> ignoring attempt to delete distinguished playlist "%s" (%d)` (`9d2350`), `sync> ignoring attempt to delete non-empty playlist folder "%s"` (`9d2050`), `sync> deleting empty playlist "%s"` (`9d2a18`, `9db7b0`), `sync> not deleting library playlists since all playlists on device were deleted` (`9d27e0`). Host settings named `AllowDeletePlaylist` and `dontAskForPlaylistRemoval` (`94e668`, `958b28`) exist. These are rules of the host's own sync, not of the phone. Several belong to an older iPod-style path rather than the ATC path.
- Playlist members that are not music (media type other than 8), a track appearing twice in one list, and members of the primary container or Playback History were not tested as removal targets; a sender should refuse them (INFERRED from absence of evidence).
- Primary playlist, Photos Memories and Playback History are the phone's built-in containers. After all test playlists were deleted the phone held only these three.
- The primary container holds one `container_item` row for each track of the library as Apple Devices reset it (399 rows). Tracks inserted afterwards are not added: a 439-track library still had 399 primary members. Playback History held 250 rows. It has no wire id, no host operation creates or edits it, and the phone writes it when music plays (INFERRED). A primary-container `update_playlist` with the original order followed by new track pids was prepared as a way to add tracks to the primary list. It was never sent (OPEN). Do not create history rows.

**Pruning of earlier operations.** The host drops a pending `insert_playlist` or `update_playlist` for a pid when it later queues `delete_playlist` for it. The routine at `140469838` (1.6.4.90) walks the pending operations backwards, logs `airtraffic> removing %@ operation for persistent id %@`, deletes matching entries (`1406a1318`), and returns false if it removed an insert. The merge cases at `14046aaa5` to `14046aadb` call it. `delete_album` (`14046a96a`) and `delete_artist` (`14046a9cc`) drop an earlier `insert_album` or `insert_artist` the same way. So a group created and deleted before a session is never sent. A sender that builds one session may do the same (INFERRED).

### Interruption and recovery

- A `delete_playlist` session killed at its first `Progress` was applied (see above), unlike track-delete sessions.
- A creation session was not interrupted in any observed run. Its state after a kill is OPEN. INFERRED: read the container table before retrying, because a blind resend could create a second playlist.

### Observed runs

| Shape | Outcome |
|---|---|
| `insert_playlist`, 1 member | Created; anchor 109 to 110 |
| `insert_playlist`, 2 members `[U,V]` | Created; anchor 114 to 115 |
| delete sole member | Empty playlist remains; anchor 110 to 111 |
| delete tail of two | Survivor unchanged; count 2 to 1 |
| delete sole survivor | Empty playlist remains; genre representative moved |
| 8 playlist creations, then 9 member deletions in one session | Positions close up |
| `delete_playlist` on empty and populated playlists | Removed; count row left orphaned |
| `delete_album`, `delete_artist` | Ignored |
| `delete_playlist` ×10 | All removed |
| `delete_playlist` ×3, killed at first `Progress` | Applied |

---

## 5. Cross-cutting rules

**Ordering of effects inside one session** (CONFIRMED). An `insert_track` batch writes the artist revisions first, then the item revisions in operation order, then the album revision. A delete batch writes item tombstones in item-id order, then playlist live revisions, then (when the phone collects) group tombstones by class: albums, album artists, item artists, composers, genres. An update batch writes touched groups by class first, then the items in operation order. The four sync properties are set last to the highest revision written.

**One anchor step per session.** A session of any size moves the anchor by exactly 1, even if it carries 100 operations, and a session that does nothing observable (`delete_album`, `delete_artist`) still moves the anchor and the generation id. The anchor moves even for an abandoned or interrupted session that left rows behind.

**Removals and insertions in one session.** CONFIRMED (over Wi-Fi, covered ALAC test tones). One operation file of `update_db_info`, then `delete_track` operations, then `insert_track` operations, the order Apple's host gives a sync (chapter 08, section 6), was applied as one session in three runs: one removal and one insertion into a new album; one removal that emptied an album and one insertion into another; 18 removals that emptied three albums and 2 insertions that refilled two of them. Each took about 45 s and moved the anchor by one. The tool's own syncs also sent such sessions: 5 removals and 2 insertions (one refilling the album the removals emptied, one into a new album); 249 removals and 250 covered insertions, 500 operations in one file, in about 75 s; and 50 removals and 50 insertions killed after 20 uploads. Each moved the anchor by one.

- The phone asked for the audio and cover of every inserted track in one manifest, as in a session of insertions only, and removed each removed track's file shortly after the session.
- The removed items' tombstones come first, numbered one after another above the highest earlier revision. The insertions' revisions follow, and the last-synced and hero-image revisions are set to the last of those. Each `FileComplete` then gives its item and its album's statistics new revisions above them, in upload order.
- An album or artist the session empties and refills stays the same row with the same `sync_id`; its representative becomes the new member.
- The 18-removal session collected no emptied group: the album and artist it emptied and did not refill stayed, with representative 0. Removal-only sessions of 16 to 499 targets collected every group they emptied, except one 90-target session that emptied the whole library (section 3).
- The session of 249 removals and 250 insertions collected the four albums it emptied, and also every group earlier sessions had left empty: six albums and four artists, the 18-removal session's among them.
- A session killed after 20 of its 50 uploads had applied all of it: the 50 removals, and 50 new rows of which 30 had no audio, as in an interrupted session of insertions only.

**What "saved completion" means.** A session's effect is observed after it finishes by comparing the full catalogue and files with a model of what the operation must do. A terminal reply or a "finished" message from the phone does not establish an effect by itself. Two separate observations are needed: the catalogue delta and the physical state of the files. For a delete the target file may still be present at first and absent later, so a verifier needs a bounded wait or two canonical stat checks. If the catalogue delta matches the model, the phone applied the session as a whole: all operations, and the anchor move, or none. No half-applied catalogue state of a delete or update session was observed. Inserts are the exception that has a middle state, below.

**Atomicity.**

- Delete and update sessions are all-or-nothing in the catalogue (CONFIRMED for 100-target sessions: either the anchor moved with every effect, or nothing changed). The files are not covered by that atomicity: a delete session rolled back at its first `Progress` had already removed a target's file.
- An insert session is atomic for catalogue rows but not for files. If the host stops after the first file, the anchor still moves and every planned row exists, only some with audio. The rows without audio are "locationless".
- An abandoned insert session can drop its first rows. The phone deleted the first two rows named in its own manifest. This is the one case where the phone removed rows nobody asked it to.

**Early kill: rolled back versus applied.**

| Session kind | Kill point | Result |
|---|---|---|
| Track deletes | First `Progress` | Rolled back (files may already be gone) |
| Track deletes | `AssetManifest`, `SyncFinished` | Applied |
| `delete_playlist` only | First `Progress` | Applied |
| Inserts | First `Progress` (one track) | Rolled back |
| Inserts | After first `FileComplete` | Applied; all rows exist; unfinished rows are locationless |
| Inserts | Before any upload | Anchor moved; rows exist; first two rows dropped in one run (an abandoned session), none in another (killed after `AssetManifest`) |
| Metadata update | First `Progress` | Rolled back |
| Metadata update | `SyncFinished` | Applied |
| Cover only | `SyncFinished` | Applied |

A recovery procedure needs to read the phone after a kill, not assume either outcome, and must be ready to re-send a rolled-back batch under a new revision (the anchor did not move) and to remove locationless rows after a partial insert.

**Retry and session reuse.** After a rolled-back session the next fresh session at the unchanged anchor was accepted. No evidence exists of reusing the same session after an error, or of the phone rejecting a session for an earlier error (OPEN).

**Normal changes between runs.** Playback, the phone's own renumbering of item revisions, removal of statistics revision rows, trimming of file allocation, and background evaluation of keep-local status are all normal. So is a session finishing the keep-local evaluation of an album an earlier, interrupted session left (sections 1 and 2). They are not a sign that a previous operation failed. A planner should compare content (names, ids, file bytes), not revisions, block layouts or `st_blocks`.

**Case.** Name matching of artists and albums ignores ASCII case. Group display names take the last spelling.

**Identity.** Albums and artists are matched by name. Item pids are phone-assigned (`max(item_pid) + 1`, `+2`, ...); the pid a sender chooses becomes `sync_id`. A pid that belongs to a deleted item's tombstone is revived (`deleted` 2 to 0) with a new revision, so a re-added track takes the slot of a deleted one and no gaps are left. A new row in a revived slot inherits no artwork and no container membership.

**Playback counters.** Any session zeroes nonzero recent play and skip counts. A sender that uses those counters for decisions must read them before the session.

---

## 6. Open questions

- Whether the phone splits a very large insert request over several manifests; what a batch of hundreds does.
- Why the phone asks for the Genius part bit for some tracks only, and why it deleted exactly the first two rows of an abandoned insert session.
- Whether staged files in `Airlock/Media` that were never completed are ever claimed or cleaned (none was, at later checks).
- Whether the phone ever removes `artwork` rows left unreferenced by a replaced album cover.
- Which property of a grayscale cover leaves its rendition empty, and whether it is ever filled.
- What makes the phone collect emptied groups: sessions of 16 to 499 targets collected, sessions of up to 8 did not, nor a 90-target session that emptied the library or a session of 18 removals and 2 insertions.
- Whether the leftover library-container and Playback History rows, and the orphan playlist count row, are ever cleaned.
- When the phone deletes a target's file during a delete session, and whether a rollback of a larger batch could also lose files.
- Why a session killed at the first `Progress` is rolled back for track deletes but applied for `delete_playlist`.
- What `[handler+0x64]` holds and whether a device with a lower value acts on album and artist deletes.
- Whether `delete_playlist` works on playlists made on the phone, smart playlists or folders; whether `update_playlist` reorders or appends members; whether an identical update is a no-op.
- Two-session move of a non-representative item artist or album artist; how to clear a genre or composer.
- The exact rule for recomputing representatives when an item joins a group; whether an insert session moves a coverless representative; whether a composer's representative moves to a covered member on delete, as artists and genres do.
- What the phone does with the MP3 gapless values beyond storing them as sent (Apple's host-side rule is chapter 06, section 13).
- Which other kinds of session finish an album an interrupted insert left unevaluated (seen: a metadata session, an emptying delete).
- What the phone does when a cover's metadata arrives without its image (a cover-only session killed at `AssetManifest`); not observed.
- Whether the phone replaces the audio of a track under an existing pid at all, and what the host's bare `update_track` does to a track that already has a file.
- Whether `update_playlist` on the primary container adds tracks to it.
- An ordinary track inserted first and a compilation track added to its album afterwards; compilations without an explicit album artist.
- Whether updating a title that equals its sort title writes one shared `sort_map` row or two.
- Behaviour of primary container and Playback History members as deletion targets, duplicate members, and non-music members.

---

## 7. Evidence

The chapter rests on three kinds of evidence.

**Sessions against the phone.** A Linux host spoke the ATC session protocol and AFC to one iPhone17,2 (iPhone 16 Pro Max), iOS 27.0.1, build 24A446, over USB and over Wi-Fi. Each experiment was a single session. Before and after it, the phone's `MediaLibrary.sqlitedb` with its write-ahead log, `iTunesPrefs`, `iTunesCDB` and `ArtworkDB` were captured, repeating until two reads agreed, and all 45 tables were compared row by row with their schemas. Permanent audio files and cover originals were read back byte for byte, renditions were listed, and file presence was checked again minutes later. Tracks and covers were also checked by playing and viewing them in the Music app. No session wrote to the phone's databases directly.

- Compilation and ordinary inserts with explicit album artists; composer repair
- Covered deletes, last-member deletes, title change and exact restoration, playlist creation and member deletion, re-add of a deleted track
- Track number change and restoration, MP3 insert with coded duration, MP3 delete, sole member after a tail delete
- Mixed ALAC and MP3 additions; catalogue drift between sessions
- Batched inserts, batched deletes and 41 metadata sessions
- 100-target deletes and group collection, playlist members, kill-point matrix, covers, case variants, MP3 with an Info frame and a cover, grayscale covers
- `delete_playlist`, `delete_album`, `delete_artist`, one-track kill, restart of the phone
- Clearing fields, tag-only updates of FLAC and AAC, kills over Wi-Fi, emptied album losing its statistics revision
- MP3s with gapless groups, comments and sort names; an eleven-track mixed-format insert; covers for albums already on the phone; a killed MP3 batch; a genre representative moving on delete
- PNG and other JPEG-kind covers; one-track inserts, mixed ALAC and MP3 batches, a cover and metadata edits killed at chosen messages; an unevaluated album finished by the next session
- Insert and delete sessions of 16 to 499 tracks; removals and insertions in one session

**Reference captures.** Sessions recorded from Apple Devices on Windows served as the reference for what the host sends: the full reset of the test phone (`update_db_info`, 399 `insert_track` and one `insert_playlist`) and the single MP3 insert.

**Static analysis of the host.** `AMPDevicesAgent.exe` 1.6.4.90 (Apple Devices 1.1540.23042.0) and 1.6.4.94 (Apple Devices 1.1540.24088.0) were disassembled for the host-side rules: operation names and builders, the change-code dispatch, field selection for `update_track`, the playlist predicate, queue pruning and coalescing, and the audio-replacement paths. Instruction pins were verified byte by byte for the audio-replacement path (16,861 instructions, 49 pins), the audio-change selection (9,106 instructions, 104 pins) and the update field selection (4,376 instructions, 27 pins). Addresses in this chapter are those of the version named beside them.

What the evidence does not cover is stated as OPEN in the text and collected in section 6.
