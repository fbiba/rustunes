# How Apple's host batches a Music sync: sending and removing tracks

This chapter describes how Apple's own Windows host, the "Apple Devices" application, structures one Music sync: how many ATC sessions it opens, how it splits the operations, the order in which additions, updates and removals go into a session, and how it schedules uploads and conversions. Section 12 compares it with `rustunes`, which sends a sync as a sequence of sessions and verifies each before the next. The protocol itself (messages, the operation plist, signing, completion) is in chapter 02. The receiver's behaviour is in chapter 05, and the tool's design is in chapter 07.

## Contents

1. Scope and provenance
2. Evidence labels and conventions
3. Summary
4. One sync is one session
5. The operation file: at most 500 operations per file
6. Order of operations inside a session
7. How removals are chosen and sent
8. How uploads and conversions are scheduled
9. Waiting, keep-alives and failure
10. Reset is not a clear
11. Wi-Fi and USB
12. Comparison with `rustunes`
13. Open questions
14. Evidence

## 1. Scope and provenance

The findings come from static analysis of the disassembly, along two paths: how the host sends new tracks and how it removes tracks. The two were checked against each other, against a second reading of the key addresses and against the two Apple reference sessions, the reset reference session and the delta reference session (section 14.2). No Apple code was run. No phone was used, except for the tool's own sessions that section 12 cites from chapters 05 and 07.

The main build is Apple Devices 1.1540.23042.0. Every function on the paths below was mapped to the newer build 1.1540.24088.0 by instruction shape, and constants were spot-checked there (section 14.3). AirTrafficHost.dll was read only as far as the message send path.

## 2. Evidence labels and conventions

| Label | Meaning |
| --- | --- |
| CONFIRMED | Seen in disassembled code and data flow, or in a captured Apple session. |
| INFERRED | Follows from confirmed facts, or from names, strings or analogy, but was not shown directly. A log string on its own is INFERRED. |
| OPEN | Not answered by the available evidence. |

Addresses are virtual addresses in `AMPDevicesAgent.exe` 1.6.4.90 (ImageBase `0x140000000`) unless marked otherwise. String addresses are virtual addresses too. In `.rdata` they are `0x1400` above the file offsets that chapter 02 uses: the format string `Sync_%08X.plist` is at file offset `0x9a4538` and virtual address `0x1409a5938`. Chapter 02 also quotes some functions by their 1.1540.24088.0 RVA (for example `0x468fbc` for the writing fiber, `0x140468e94` here). Section 14.3 maps the two builds.

## 3. Summary

| Question | What Apple's host does | Label |
| --- | --- | --- |
| Sessions per sync | One. A sync sends one `RequestingSync` and puts every pending operation of the sync in that session. | CONFIRMED |
| Cap per session | None by count, bytes or time. The only size rule is free space on the phone, computed over the whole pending set. | CONFIRMED |
| Count limit anywhere | At most 500 operations per operation file. Several files go into the same session, with one `revision`. | CONFIRMED in code; never observed on the wire |
| Additions and removals together | The same operation array and session. Order: updates to matched tracks, removals, additions, then playlist operations. | CONFIRMED (code order and the delta reference session); update-before-removal INFERRED |
| Removal operation | One `delete_track {operation, pid}` per track. Never `delete_album` or `delete_artist` for this phone. | CONFIRMED |
| Removed files | The host never touches them over AFC. The phone removes them. | CONFIRMED on the removal path |
| Upload concurrency | One file at a time through a single worker. No wait on the phone between files. | CONFIRMED |
| Upload order | Audio-bearing manifest entries first, in the phone's manifest order, then artwork-only entries. | CONFIRMED |
| Conversion | Starts once the manifest arrives, on a thread pool that works ahead of the file being uploaded. Nothing is converted before the session. | CONFIRMED structure; pool size INFERRED |
| Verification between batches | None. The host sees only session success or failure. | CONFIRMED |
| Retry | None inside a sync. A failed file gets `FileError` and the session goes on. A failed session sends `SyncFailed` and keeps its pending operations for the next sync. | CONFIRMED structure; policy INFERRED |
| Wi-Fi vs USB | No separate session or upload path. | CONFIRMED |

## 4. One sync is one session

CONFIRMED. `ATHostConnectionSendSyncRequest` (import at `0x140823798`) has exactly one call site in the agent, `0x14046d37e`, inside the request-sync function `0x14046cfec`. That function has two callers:

- `SynchronizeDevice` (`0x14056fc74`), the sync master, at `0x140570155`, before the sync pass;
- the operation-writing fiber `0x140468e94`, at `0x140468fd8`, only when no session is active (`[handler+0xf210] == 0`). This is the background sync path.

A request that arrives while a session is in states 2 to 6 cancels the running session first (log "canceling previous sync", `0x14046d1ca`). At most one session is active per device. The session states (`0x140455cac`) are 0 Unknown, 1 Idle, 2 RequestingSync, 3 ReadyForSync, 4 SyncingMetadata, 5 FinishedSyncingMetadata, 6 TransferringAssets, 7 Finished, 8 Failed and 9 Canceled.

The master drives one session in a fixed order. It requests the sync and waits for `ReadyForSync`, runs the reverse-sync and housekeeping steps ("updating play counts from device", "copying playlists from device"), runs the sync pass ("syncing tracks", `0x14056577c`), album artwork and autofill, starts the writing fiber and waits for it (`0x14046dd80`), then sends `FinishedSyncingMetadata` and waits for `AssetManifest` or `SyncFinished` (`0x14046d73c`). None of these steps loops over batches.

CONFIRMED: no count cap. The sync pass `0x14056577c` walks the whole library difference once. Its only limits are byte budgets: "required free bytes", "minimum free bytes", "copying exceeded free bytes" and `remainingFreeBytes > 0`. A search of the sync pass, the writing fiber, the manifest worker, the copy fiber, the sort comparator, the track handler and the queue functions for comparisons with 100, 500, 1000 and 10,000 found only the 500 of section 5. Before writing, the writing fiber sums the size of every track to copy and logs "total bytes to copy = %llu, available free bytes = %lld, required purge bytes = %llu" (`0x1404693ed` to `0x140469500`). The purge figure is passed into `FinishedSyncingMetadata`. That is the whole-session size check, and nothing splits the session by size or count.

CONFIRMED in the reference sessions. The reset reference session staged 401 operations in one session (`update_db_info`, 399 `insert_track`, one `insert_playlist`) and the phone asked for 54 assets in one manifest. The delta reference session staged 402 operations in one file. Each had one `RequestingSync` and one `FinishedSyncingMetadata`.

A second session follows the first only when another sync request arrives during it. CONFIRMED: calling the fiber starter `0x1405712cc` while a sync fiber runs (`[handler+0x6aa9]`) only sets a restart flag (`[handler+0x6aaa] = 1`, at `0x140571502`). When the running fiber ends, the master logs "not sending SyncFinished due to sync restart" and "restarting sync fiber" and starts a fresh fiber. The number of pending tracks never starts a second session.

The per-session counts of the Linux sessions in chapter 02, section 12.6, are the batch sizes of `rustunes` (`--batch-size`), not a limit of Apple's host.

## 5. The operation file: at most 500 operations per file

CONFIRMED in code, both builds. The file-set writer `0x140466bc4` compares the 16-bit device word at `[handler+0x64]` with `0x700` (`0x140466bfe`):

- At `0x700` or above, it loops. Each pass takes `min(500, remaining)` operations (`mov r14d,0x1f4` at `0x140466c1f`, `cmovl` at `0x140466c31`). It calls the per-file writer with chunk index 0, 1, 2 and so on, and stops when nothing remains.
- Below `0x700`, it makes one call with index -1 and the whole array.

The per-file writer `0x1404666f0` builds `{operations, revision, timestamp}`. It names the file `Sync_%08X.plist` after the chunk index when the index is 0 or more, and after the revision otherwise (`cmovns` at `0x1404668c9`). It serialises the file as a binary plist, stages it over AFC, signs it with `GrappaHostSign` and writes `<name>.cig` beside it. Every chunk has its own sidecar, and every chunk carries the same `revision`, the sync anchor. The anchor goes up once per write round (`0x14046914d`), not once per chunk.

`FinishedSyncingMetadata` (built at `0x14046d620`) carries `SyncTypes`, `DataclassAnchors` and `PurgeDataBytes` but no file count. The phone has to find the extra files by name or by listing. INFERRED.

The device word gates three things: chunked files at `0x700` and above, a branch in the upload completion callback at `0x600` (`0x14045baaf`), and album and artist operations below `0x800` (`0x140468387`, `0x14046a184`). For this phone it is at least `0x800`, because Apple's full reset of it (the reset reference session) carried no album or artist operations. So it is also at least `0x700`, and chunking is active for this phone. The delta reference session's file was named `Sync_00000000.plist` with revision 10, which is the chunked naming. Where the word is set, and what it encodes, is OPEN. It reads like a device or database version.

The host-side loop is CONFIRMED (chapter 02, section 8.1). What the phone does with more than one operation file is still OPEN, because no recorded session, Apple's or Linux's, staged more than 500 operations. Apple's largest is the delta reference session's 402.

## 6. Order of operations inside a session

CONFIRMED (the append model). The change handler `0x140469b04` is the host library's synchronous notification callback (dispatched through `[library+0x1e40]` at `0x140367736`). Each library change posts a code, and the handler appends the matching operation to the end of the dataclass's array (`0x14046a646`):

| Code | Operation |
| --- | --- |
| `atri` | `insert_track` |
| `mtri` | `update_track` |
| `dtri`, `htri` | `delete_track` |
| `tdap`, `tdpd`, `atmp` | `insert_playlist`, `update_playlist` |
| `tdrp` | `delete_playlist` |

So the order inside a session is the order in which the host changes its copy of the phone's library. Merging looks only at the last element:

- A new `insert_track` replaces an immediately preceding `update_track` of the same pid (`0x14046a5f7` to `0x14046a624`).
- An `update_track` after an `insert_track` or `update_track` of the same pid is merged into it (`0x14045b018`, "mergedOperation").
- An `update_track` with no keys besides `operation` and `pid` is dropped (`0x14046a0a0`, "skipping update_track operation").
- A `delete_track` is appended and prunes nothing.
- Only playlist, album and artist operations prune earlier inserts or updates of their pid (`0x140469838`, `0x14046a96a`, `0x14046a9cc`).

`update_db_info` is put at index 0 of a non-empty Media array (`0x140468654`), so it lands in the first file. An empty array skips staging and the anchor step (chapter 02, section 8.3). Arrays are kept per dataclass: Media at `[handler+0xf248]`, Ringtone at `+0xf280`, VoiceMemo at `+0xf2b8` and Book at `+0xf2f0`. Music tracks go to Media.

The sync pass `0x14056577c` changes the library in this order:

1. It marks every device-side track the selection no longer wants (bit `0x80` at `[track+0xc2]`, loop `0x140567633` to `0x14056788d`, log "marking track for delete").
2. It walks the matched source and device pairs (`0x140567b03` to `0x140567f85`). For each pair it keeps the device track ("library version is older"), leaves it marked for replacement ("replacing track ... on device since library version is newer"), or syncs its metadata and clears the mark.
3. It deletes every marked track: `call 0x140564b08` at `0x140568297`.
4. It copies the new tracks ("copying track"), each posting `atri`.
5. It copies playlists, then deletes playlists and empty playlists ("copying playlist", "deleting playlist", "deleting empty playlist").

Removal before addition (steps 3 and 4) is CONFIRMED by code order. A replaced track therefore becomes `delete_track` followed by `insert_track` in the same session, as chapter 05 found on the receiver. That metadata updates of step 2 come before the removals in the array is INFERRED from code order. The delta reference session contains no removal to show it.

The master's own steps add their operations around the sync pass. The delta reference session shows the result: index 0 `update_db_info`, index 1 an `update_track` with play and skip counts read from the device, index 2 the new track's `insert_track`, then 399 `update_track` carrying artwork cache fields from "syncing album artwork". Operations are not sorted by kind.

## 7. How removals are chosen and sent

CONFIRMED (mechanism). The host computes the removal set against its own saved copy of the phone's library (`[handler+0x30]`), not against a live listing. The phone sends no track inventory: its startup messages carry `AssetMetrics` (in the delta reference session: a music count of 399 and a size) and `InstalledAssets`, but no list of tracks. The only resynchronisation test is the anchor comparison of section 10. INFERRED, from the absence of any inventory path.

A host-synced track whose instance is no longer selected for the device gets its mark set (`or [track+0xc2],0x80`, `0x1405676c6`). The selection predicate is `0x140496c8c`, which calls `0x140496be4` and `0x14049562c`. Device-origin tracks, autofill tracks and similar special classes have their own predicates (`0x1404c3a28`, `0x1404c38b8`, `0x140588a6c`). What each flag byte at `[track+0xc5]` means is INFERRED.

The delete step `0x140564b08` collects the marked tracks into a list and, for each one, logs "deleting track", calls `0x140564a68`, then calls `0x140560070`.

- `0x140564a68` adds the track's size to a "pending delete bytes" counter (`[ctx+0x3d8]`) and tail-calls `0x14038f7f4`, which removes the track from the in-memory library. The library marks it deleted (`[track+0xc3] |= 1`) and posts `dtri` (`0x14038e6c1`). The change handler builds `delete_track {operation, pid}` through `0x140461900`, if the track belongs to the synced library and `[track+0xc5] & 4` is clear (`0x140469de2`).
- `0x140560070` is a time-throttled progress and cancel poll. Every 5 ms it yields, and it returns `0xffffff80` if the sync was stopped. It never waits for the phone.

So every removed track is one bare `delete_track`, with no per-track wait. Playlists the sync pass deletes become `delete_playlist`. `delete_album` and `delete_artist` exist but need the device word below `0x800`, which is false for this phone, so the host never sends them. Chapter 05 found that the phone ignores both.

CONFIRMED: no file work for a removal. None of the functions on the removal chain (`0x140564b08`, `0x140564a68`, `0x140560070`, `0x14038f7f4`, the library dispatch and `0x140469b04`) calls AFC. The agent does import `AFCOperationCreateRemovePath` and `AFCOperationCreateRemovePathAndContents`, but they sit in generic AFC wrappers reached through function pointers, and no link from the track-removal path to them was found. Their callers are OPEN. The phone removes the audio file itself (chapter 05).

No Apple capture contains a `delete_*` operation. Everything in this section comes from static code.

## 8. How uploads and conversions are scheduled

### 8.1 The manifest becomes one sorted queue

CONFIRMED. The `AssetManifest` worker `0x14045ef4c` deep-copies each entry and stamps it with `Dataclass` and a running `AssetManifestIndex` (counter `[handler+0xf318]`). It appends the entry to one array (`[handler+0xf320]`) and sorts the array with `CFArraySortValues` and the comparator `0x14045d9e8`. If the copy fiber is idle (`[handler+0xf348] == 0`), the worker starts it as `AirTrafficAssetCopyFiber` (body `0x14045ddc0`). Photo entries go to a separate queue.

The comparator orders entries by asset class first (`0x14045649c`; Media, Book, Application, Photo and Data differ). Within Media, an entry whose `AssetParts` has bit 0 (audio) set comes before one with only bit 1 (artwork). Ties keep ascending `AssetManifestIndex`. A normal Media manifest is therefore uploaded as its audio-bearing entries in the phone's order, then its artwork-only entries in the phone's order. The host does not reverse it. The Linux observation that "files go up in reverse operation order" is the order of the phone's manifest, not a sort by the host.

The completions of the reset reference session agree with this order: an audio-plus-artwork entry at index 46 completed before an artwork-only entry at index 1, and the lyrics entry at index 0 completed later (chapter 02, section 12.2).

### 8.2 One worker copies one file at a time

CONFIRMED. The copy fiber loops. On each pass it checks the abort flag (`[handler+0xf381]`) and the cancel bit, takes element 0 of the sorted array and hands it to the per-class handler. For a Media track to download, that is `0x14045bc30`. On error it sends `FileError`. Then it yields. The track handler only queues the copy ("copy>  queueing track"), records the request in a pending-assets dictionary keyed by `AssetID` (`[handler+0xf328]`) and returns at once. The whole manifest is therefore queued within moments of its arrival.

The queue is drained by one worker, `CopyFiles` (`0x140228904`), which takes items by index (`[trackdata+0x2d38]`) and runs each one to completion: "copy>  starting track", `JRCopyFiles` (`0x1402264dc`, call at `0x140229399`), "copy> completed track", then the item's completion procedure (`call rax` at `0x140229bba`). When the queue runs dry while the session is in state 6, the worker sleeps and checks again (`0x140229c84` to `0x140229cdf`), because the copy fiber may still be appending.

The upload engine is strictly serial. The concurrency is elsewhere: the copy fiber filling the queue while the worker drains it, the reader and writer threads inside `JRCopyFiles` for one file (chapter 03, section 7.3), and the conversion pool of section 8.4.

The only producers of this queue are the manifest path (`0x14045bf07`), a classic-iPod path (`0x14039dfe7`) and reverse sync (`0x14035eae0`, `0x14045d090`). Nothing copies or converts a file before the session.

### 8.3 FileComplete right after each file

CONFIRMED structure. The completion callback `0x14045b414` runs on the worker right after a copy. It does the following, in order:

1. It queues the asset's remaining parts, such as artwork, lyrics and Genius data (`0x14045a9e0`).
2. It builds the permanent path (`0x14045a6f0`) and renames the staged file (`0x1406b59dc`).
3. It sets the track's permanent location ("setting perm location of track").
4. It calls the completion gate `0x140456fe8`.

The gate sends `FileComplete` through `ATHostConnectionSendAssetCompleted` (`0x140456f00`) once every requested part of the asset is done. Otherwise it logs "not yet sending ... still need asset parts". On failure the callback turns the error into `FileError` (`0x140456a2c`). `FileComplete` therefore follows the end of its file's copy within the same worker step, with no batching and no wait on the phone.

The ancillary parts are queued by rank (`0x140223348`): track kinds 1 to 11, photo 12, artwork 13, lyrics 14, Genius 15 and other 16. A new ancillary item is inserted by walking back from the end of the queue past items of a higher rank. So artwork lands behind every queued track and ahead of lyrics and Genius. Audio items are appended. The bytes of artwork, lyrics and Genius items are "promised data", produced when the worker reaches the item ("failed to copy artwork data for track" is logged from that procedure, `0x1404589fc`). Artwork is not written ahead of time. Exactly where the artwork part of an audio-plus-artwork asset lands relative to artwork-only entries, and so when that asset's `FileComplete` goes out, is OPEN.

### 8.4 Conversion works ahead of the upload

CONFIRMED structure, INFERRED depth. The `CopyFiles` loop calls a lookahead pump (`0x14022540c`) on every pass. The pump scans the queue from the next item to prepare (`[trackdata+0x2d3c]`) to its end. While fewer tasks are active than the limit, it creates a conversion task on a thread-pool queue (`0x1402255e8`). The limit is read from the preference `max-transcode-threads` (`0x1402289f0`) with a minimum of 1, and is clamped to the processor count minus one on a machine with more than one processor. The default passed is 3; the argument meaning of the reading function is not resolved, so the default is INFERRED. The log lines are "perf> using %u thread(s) for transcoding", "perf> beginning transcode of track" and "perf> transcode of track ... took %.2f seconds".

When the head of the queue is still converting, the worker logs "perf> waiting for track ... to finish transcoding" (`0x140229063`), sleeps, pumps and tries again. No limit on finished conversions waiting for upload was found. INFERRED from the absence of a check.

Conversion therefore starts when the manifest is queued, runs on up to N threads ahead of the file being uploaded, and overlaps the uploads. Before the session the host computes only size estimates, for the free-space check of section 4.

## 9. Waiting, keep-alives and failure

The host waits for the phone in two places per session, and nowhere between files or between operations:

- **For `ReadyForSync`.** A 120.0 s argument (constant at `0x140a0e2e8`), polled every 0.05 s (`0x140a0e020`). CONFIRMED.
- **For `AssetManifest` or `SyncFinished` after `FinishedSyncingMetadata`.** No deadline. CONFIRMED: the wait is called with -1.0 (`0x140a0e610`), and the deadline helper `0x14046cbb4` turns a negative value into `DBL_MAX` (`0x14046cbc8`, `0x140a0e5c0`). The loop (`0x14046db19` to `0x14046dbc1`) polls the state every 0.05 s, records Ping and Progress activity (`[handler+0xf358]`) and treats state 6 or above as success.

`FinishedSyncingMetadata` is sent once, from state 4 (`ATHostConnectionSendMetadataSyncFinished`, call at `0x14046da00`). A second call finds state 5 and goes straight to the wait.

CONFIRMED: Ping. A timer runs while the session is in states 2 to 6 (callback `0x14045e82c`). It sends `ATHostConnectionSendPing` (`0x14045e8ed`) and does not wait for the Pong. A 15.0 s constant (`0x140a0e210`) is passed to the timer creation. That it is the period is INFERRED.

CONFIRMED: messages during uploads do not wait for a reply. `ATHostConnectionSendAssetCompleted`, `FileError` (`0x140456fb9`) and `FileProgress` (`0x14046e3f2`, from the throttled progress callback `0x14046e174`) only queue the message. In AirTrafficHost.dll, `ATHostConnectionSendAssetCompleted` (RVA `0x310e0`) calls `ATHostMessageLinkSendMessage` (RVA `0x33670`), which hands the message to `dispatch_async_f` and returns.

CONFIRMED: no overlap between sessions. Before a new session writes its operations, the writing fiber checks the asset-transfer bit (`[trackdata+0x2cc8] & 0x20`). If a transfer is running it logs "airtraffic> aborting asset transfer for device", aborts it (`0x14022bca4`) and polls every 0.05 s until the bit clears (`0x140468f3e` to `0x140468fad`). Apple's host cancels an earlier transfer rather than overlap two sessions.

On success, meaning state 6 or 7 after the metadata phase, the host commits each dataclass (`0x14046ce50`, called from `0x14046dcc8` to `0x14046dcec`). It sets its last anchor to the next anchor, empties the pending array and zeroes the written count. The commit happens when the manifest or `SyncFinished` arrives, before any asset is copied. CONFIRMED.

Failure, CONFIRMED structure and INFERRED policy:

- **One file fails.** The worker logs "copy>    failed (%.3d)" (`0x1402297a0`), calls the completion procedure with the error, sends `FileError` (`0x140456a2c`, `0x140456c74`), cleans up the partial file (`0x140223b38`) and moves to the next file. No retry counter was found.
- **A cancel (-128) or a disk-full class error.** These take the abort path instead (`0x140229416` to `0x140229470`). The exact classes are not fully resolved.
- **The phone sends `FileError`.** The host only logs it (chapter 02, section 14.2).
- **The session fails, times out or is cancelled.** The host sends `SyncFailed` with a mapped reason (`0x14046cd0c`, `0x14046cdf4`) and reports the error code (`0x1404991c0`). The pending arrays stay as they are. The writing fiber zeroes the written count at its end (`0x14046937c` to `0x140469391`), so the next sync's session would resend the whole pending array.
- **No follow-up inside a sync.** No code starts a second session or re-plans after a failure. The `cmp r14d,0x468` in the master (`0x1405711a6`) belongs to an error report, not to a retry.

If the phone applied a session but the host never saw it succeed, the next `RequestingSync` finds the anchors different and takes Reset (section 10). INFERRED from the anchor test only.

## 10. Reset is not a clear

CONFIRMED. There is no operation that clears the library. The selector `0x14046cf0c` compares the host's last anchor with the device's (`[struct+0x1c]` and `[struct+0x14]`). If they match, it picks Delta. If they differ, it picks Reset (`SyncTypes` Media 2) and calls `0x140469a28`, which removes every pending operation except `upload_track`. While a dataclass is in Reset the change handler ignores library notifications (`cmp [struct+0x20],2` at `0x14046a46d`).

At write time `0x1404685b0` calls `0x14046823c`, which replaces the array with a fresh list built from the host's copy of the device library. The list holds album and artist inserts (only below device word `0x800`), then one `insert_track` per track (skipping `[track+0xc5] & 4`), then `insert_playlist`, with `update_db_info` first. No removals are sent. The reset reference session, Apple's reset of this phone, had exactly this shape: `update_db_info`, 399 `insert_track` and one `insert_playlist`.

The sync pass stops with "not syncing because library is empty" (error `0x458`, `0x140565c4f`) when the selected source library has no tracks, so a sync cannot empty the phone. `EraseDevice` and `AMSEraseDevice` belong to the whole-device restore path, not to Music sync.

## 11. Wi-Fi and USB

CONFIRMED. No branch in the master, the writing fiber, the manifest worker, the copy fiber or the `CopyFiles` worker depends on the interface. The differences are around the session:

- The sync-allowed check (`0x140492c80`) refuses to sync "via Wi-Fi because host is running on battery power".
- During a Wi-Fi sync the host sends the ATC `PowerAssertion` (`0x140557d8d`) and holds a wireless-sync power assertion.
- Interface switches (`0x14048e83c`, `0x14048e924`) swap the device's interface.

The AFC connection count and request sizes are in chapter 03, section 7.

## 12. Comparison with `rustunes`

Facts only. Chapter 07 gives the reasons behind the tool's choices.

| Aspect | Apple's host | `rustunes` |
| --- | --- | --- |
| Sessions for one sync of N additions and M removals | One | With batch size B (`--batch-size`, 1 to 499, default 499): `floor(M/B)` full removal sessions, then `ceil((M mod B + N)/B)` insert sessions, the first of which also removes the `M mod B` left before its additions. An insert batch holds at most 100 converted FLACs, fewer when their conversions would not fit in `TMPDIR`. At B = 1 each new ALAC goes through the single-insert session and each MP3 or converted FLAC in an insert batch of its own. Updates to tracks already on the phone go in metadata sessions of their own, as many as the phone's group rules need (chapter 07, section 4.7) |
| Anchor steps | One | One per session |
| Additions and removals | One array: updates, removals, additions, playlists | Removals first: full removal sessions alone, then the rest of the removals and the first additions in one session. A removal of a track in an ordinary playlist stays in a removal session |
| Operations per file | At most 500, several files in one session | One file per session, at most 500 operations with `update_db_info`: `--batch-size` counts operations, at most 499, and a metadata session holds at most 499 `update_track` |
| Check between batches | None. Trusts its saved copy of the phone's library and the anchors | Reads the phone's catalogue after every session and verifies it before the next |
| Concurrent uploads | One file at a time | One file at a time, with pipelined stats (chapter 03, section 10.3) |
| Conversion | During the session, up to N threads working ahead of the upload | FLAC to ALAC on up to three threads (processors but one) in member order: a batch's before its session starts, and the next batch's while that session runs (chapter 07, section 9.5). 16 FLACs in one batch took 16.5 s from the first conversion to the session |
| Removed files | Left to the phone | Left to the phone |
| Group removal operations | Never sent for this phone | Never sent (chapter 05 found them ignored) |
| Failure inside a session | `FileError` for that file and the session goes on. `SyncFailed` keeps pending operations for the next sync | Journal states and resume (chapter 07) |

What is known about the phone's side of larger sessions comes from Linux sessions only. Removal sessions of 4 to 499 and insert sessions of up to 499 with covers were applied as expected, each with one anchor step (chapter 05), and Apple's reset reference session put 399 inserts into one session. Sessions of removals followed by insertions were applied as one, with one anchor step: probes of up to 18 removals and 2 insertions, then the tool's own syncs of 5 and 2, and of 249 removals and 250 covered insertions in one 500-operation file (chapter 05, section 5). After a 200-track insert session the phone gave every prior track new sort-key orders, in the same relative order. No session of any kind has staged more than 500 operations, so the phone's handling of a multi-file session is untested.

## 13. Open questions

1. How the phone consumes a session with more than one operation file: discovery, order and atomicity. The largest recorded session staged one file of 500 operations (Linux, 499 inserts).
2. What the device word at `[handler+0x64]` holds and where it is set. It gates chunking (`0x700`), a completion branch (`0x600`) and album and artist operations (`0x800`).
3. Whether metadata updates of matched tracks really precede removals in the emitted array. This rests on code order alone, because no Apple capture contains a removal.
4. Whether the phone ever sends two `AssetManifest` messages in one session. The host can append to an active queue, but every capture shows one.
5. Where the artwork part of an audio-plus-artwork asset lands in the upload queue, and so when that asset's `FileComplete` goes out.
6. Whether a second write round can happen inside one session (the writing fiber loops when operations arrive during a write, `0x1404692fa`), and how the phone treats a repeated `Sync_00000000.plist` name.
7. The default of `max-transcode-threads`, the role of the 15.0 s Ping constant, and which copy errors abort the whole queue rather than one file.
8. Who calls the two AFC remove wrappers (`0x1405aa640`, `0x1405acc6c`), and whether Music sync can reach them.
9. Why the delta reference session waited 6.5 s between writing the operation file's sidecar and sending `FinishedSyncingMetadata`.
10. AirTrafficHost.dll in build 1.1540.24088.0 was not read at the instruction level.

## 14. Evidence

Phone: iPhone17,2, iOS 27.0.1, build 24A446.

### 14.1 Static analysis

Disassembly (objdump), string extraction, a `.pdata` function-boundary parser and a shape-hash map between builds, with no code executed. The binaries are those of chapter 03, section 13.3:

| Binary | Version | SHA-256 |
| --- | --- | --- |
| AMPDevicesAgent.exe | 1.6.4.90 (Apple Devices 1.1540.23042.0) | `ad957f32107b7177969b3cb11b91e3922d27e622b53f9240b65cb457522111f9` |
| AirTrafficHost.dll | 4025.400.1.5 (1.1540.23042.0) | `0d38b01c21d57a9f3ca32c15d4b2b8aa0557d43a724b4745f1fb122322f4eecd` |
| AMPDevicesAgent.exe | 1.6.4.94 (1.1540.24088.0) | `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9` |

Re-checked after the analysis:

- the single `ATHostConnectionSendSyncRequest` call site, `0x14046d37e`;
- the `0x700` comparison and the `0x1f4` chunk loop in `0x140466bc4`;
- the call to the delete step at `0x140568297`;
- the deadline helper `0x14046cbb4`, together with the doubles 0.0, -1.0, 0.05, 15.0, 120.0 and `DBL_MAX`;
- the log strings quoted in sections 4 to 9.

### 14.2 Apple reference sessions

Captured with Apple Devices 1.1540.23042.0 in a Windows virtual machine, at the application level (chapter 02, section 19.2):

- **Reset reference session.** A Reset with 401 operations in one file and one session, a 54-entry manifest, and the completion order quoted in section 8.1. The collector's event cap cut the capture off before the session ended.
- **Delta reference session.** A Delta with 402 operations in one file named `Sync_00000000.plist`, revision 10. The event cap was reached before the manifest.

Neither session contains a removal, and neither shows the whole upload phase.

### 14.3 Build 1.1540.24088.0

Each function maps by shape hash to a unique match with the same instruction count. The small constants of the 18 functions checked are identical between builds.

| Role | 1.1540.23042.0 | 1.1540.24088.0 |
| --- | --- | --- |
| Sync master `SynchronizeDevice` | `0x14056fc74` | `0x14056fdb0` |
| Sync fiber starter | `0x1405712cc` | `0x140571408` |
| Sync pass | `0x14056577c` | `0x1405658b8` |
| Delete marked tracks | `0x140564b08` | `0x140564c34` |
| Track delete helper | `0x140564a68` | `0x140564b94` |
| Request sync | `0x14046cfec` | `0x14046d114` |
| Delta or Reset selector | `0x14046cf0c` | `0x14046d034` |
| Writing fiber | `0x140468e94` | `0x140468fbc` |
| File-set writer (500 per file) | `0x140466bc4` | `0x140466ce8` |
| Per-file writer | `0x1404666f0` | `0x140466814` |
| `update_db_info` insertion | `0x140468654` | `0x14046877c` |
| Reset array builder | `0x14046823c` | `0x140468364` |
| Change handler | `0x140469b04` | `0x140469c2c` |
| `FinishedSyncingMetadata` and wait | `0x14046d73c` | `0x14046d864` |
| Commit | `0x14046ce50` | `0x14046cf78` |
| `SyncFailed` and cancel | `0x14046cd0c` | `0x14046ce34` |
| Manifest worker | `0x14045ef4c` | `0x14045f070` |
| Sort comparator | `0x14045d9e8` | `0x14045db0c` |
| Copy fiber | `0x14045ddc0` | `0x14045dee4` |
| Track handler | `0x14045bc30` | `0x14045bd54` |
| `FileComplete` gate | `0x140456fe8` | `0x14045710c` |
| Completion callback | `0x14045b414` | `0x14045b538` (same calls, different register allocation) |
| Ping timer callback | `0x14045e82c` | `0x14045e950` |
| `CopyFiles` worker | `0x140228904` | `0x14022887c` |
| Lookahead pump and task creator | `0x14022540c`, `0x1402255e8` | `0x140225384`, `0x140225560` |

Spot checks in 1.1540.24088.0:

- `mov eax,0x700` and `cmp WORD PTR [rbp+0x64],ax` at `0x140466d1a`;
- `mov r14d,0x1f4` at `0x140466d43`;
- the `0x800` gate at `0x1404684aa`;
- the delete call at `0x1405683d3`;
- the anchor comparison at `0x14046d055`;
- the 15.0 s Ping constant at `0x140a0f210`.
