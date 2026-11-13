# How an iOS update changes the Music library database

This chapter describes how the phone versions its `MediaLibrary.sqlitedb`, what it does to the database when iOS is updated, and what a host has to know about it. It covers the phone's version number and migrations, the other version stamps that can rewrite library contents, the version rules of the phone's sync service and of Apple's Windows host, the difference between iOS 27.0.1 and iOS 27.2, the database of iOS 26, and what that update did to a real library. Section 11 compares the findings with `rustunes`. The database layout is in chapter 04, the protocol in chapter 02, the receiver's effects in chapter 05, how Apple's host batches a sync in chapter 08, and the tool's design in chapter 07.

## Contents

1. Scope and provenance
2. Evidence labels and conventions
3. Summary
4. The database version on the phone
5. Other version stamps in the database
6. The sync service on the phone
7. Apple's Windows host
8. What changed between iOS 27.0.1 and 27.2
9. What the update did to a library
10. Guidance for a sync client
11. Comparison with `rustunes`
12. Open questions
13. Evidence

## 1. Scope and provenance

The phone of the earlier chapters was updated:

| Item | Value |
| --- | --- |
| Phone | iPhone17,2 |
| Before the update | iOS 27.0.1, build 24A446, database version 2400060 |
| After the update | iOS 27.2, build 24B119, database version 2410000 |

The findings come from static analysis of Apple's iPhone17,2 firmware for both builds and of Apple Devices for Windows, and from two read-only captures of the phone's database: the last one on iOS 27.0.1 and the first one on iOS 27.2. Section 8.4 rests on read-only captures of a second iPhone17,2 on iOS 26.6.2 and 26.7.3. No Apple code was run and no phone experiment was made for this chapter.

Firmware files read. The SHA-256 values are of each file as extracted from the firmware, decompressed. The binaries are not redistributed. Only addresses, constants and conclusions are published.

| File | 24A446 SHA-256 | 24B119 SHA-256 |
| --- | --- | --- |
| `System/Library/Caches/com.apple.dyld/dyld_shared_cache_arm64e` (main file of the shared cache) | `52625b286e607aeae86b4fb7e768df0be5e574b8565d3a53c5b69cf4e3d38e96` | `9dd6add7ed49bed71a73dd08c966b0888e46bafecec0d1464507b30d2fa35253` |
| `usr/libexec/atc` | `0fdace76d3745dfe057cab69990ab1b1153db17ca27b0095ba7cd3e1143abdac` | `ebabafb7161eb88633fe4ae0f70a1299a43bb5f5e2a4f9c43216770557490287` |
| `System/Library/PrivateFrameworks/MusicLibrary.framework/Support/medialibraryd` | `5acc0d27c2272777e46c0aaa034558a03d31efc348949d4ac958f2bd88a8d85b` | `a27ab88082fe4cad64c7d9ccb886ea116e8180b4bc2f90b2d6e9015fe7d99831` |
| `System/Library/DataClassMigrators/MediaLibrary.migrator/MediaLibrary` | `339b982a1c933723969f0183771fc44356d3b35b45be48cfed1ecaa71f7aef69` | `2325701fe5c4e2b69f825ab5c8d209218c0bcb2918d9b3383b878420a91483ba` |
| `System/Library/SyncBundles/MusicLibrary.syncBundle/MusicLibrary` | `d4f03ed6770a7fe4a81542df202b6ee9a6b8084801c736733505ea2722ddddb4` | `a0998dff005259806ef1093306cf13f09be9000be4abe2e9638dd11df71646df` |
| `System/Library/PrivateFrameworks/iTunesCloud.framework/Support/itunescloudd` | `d90d834fe5cdbd7afb831f3bcd0a2aaffb1efa44ad708da53578cf8605aa1dbf` | `41a776fe27a82570cf1cdd72e10b6b031ada97f488ee38fbfee938a66c27d3a4` |

Images of the shared cache that were read, with their source versions:

| Image | 24A446 | 24B119 |
| --- | --- | --- |
| MusicLibrary | 4026.100.84 | 4026.200.21 |
| MediaLibraryCore | 4026.100.85 | 4026.200.1 |
| AirTrafficDevice, AirTraffic, ATFoundation | 4026.110.1 | 4026.200.21 |
| MediaPlayer, iTunesCloud | 4026.110.2, 4026.110.1 | 4026.210.1 |
| HomeSharing | 4026.100.51 | 4026.200.7 |
| MusicKitInternal | 4026.110.3 | 4026.210.2 |

The Windows binaries are those of chapter 02, section 1: Apple Devices 1.1540.23042.0 (AMPDevicesAgent 1.6.4.90, AirTrafficHost 4025.400.1.5) and 1.1540.24088.0 (AMPDevicesAgent 1.6.4.94, AirTrafficHost 4026.110.1.6). When this was checked, 1.1540.24088.0 was the newest build in the Microsoft Store.

## 2. Evidence labels and conventions

| Label | Meaning |
| --- | --- |
| CONFIRMED | Seen in disassembled code, constants or data flow, or in the phone's database captures. |
| INFERRED | Follows from confirmed facts, names or strings, but was not shown directly. |
| OPEN | Not answered by the available evidence. |

Nothing in this chapter was exercised on the phone. A CONFIRMED code path is one read in the binary, not one observed to run, unless the database captures show its effect.

Address conventions:

- Addresses in the shared cache are unslid virtual addresses in the 24B119 cache. MusicLibrary code lies near `0x1d1a00000` and AirTrafficDevice code near `0x22b790000`. A 24A446 address is marked as such.
- `medialibraryd` and `itunescloudd` addresses are virtual addresses in their own images (base `0x100000000`). The migrator and the Music sync bundle are bundles whose addresses start at `0`.
- Windows addresses are virtual addresses in `AMPDevicesAgent.exe` 1.6.4.90 (base `0x140000000`) and `AirTrafficHost.dll` (base `0x180000000`), as in chapter 08.
- Objective-C names come from the images' symbols and selector references. Names that begin with `_ML3` are C symbols of the cache.
- Version numbers are decimal: `0x249f3c` is 2400060, `0x24c610` is 2410000.

## 3. Summary

| Question | Answer | Label |
| --- | --- | --- |
| What is the database version? | `PRAGMA user_version` of `MediaLibrary.sqlitedb`, a constant compiled into MusicLibrary: 2400060 in iOS 27.0.1, 2410000 in iOS 27.2. A database on iOS 26.6.2 and 26.7.3 is at 2350000, with a narrower `album` and `item_store` (section 8.4). | CONFIRMED; iOS 26 from captures only |
| When does it change? | With an iOS update whose MusicLibrary changes the database. It does not follow the framework's version. | CONFIRMED for the two builds; the rule INFERRED |
| An older database? | Migrated in place, one exact step at a time, all steps in one transaction. | CONFIRMED |
| A newer database? | Refused with error 3. Nothing is changed or deleted. | CONFIRMED |
| A database that cannot be migrated? | `medialibraryd` truncates it and builds an empty one: new identity, no sync properties, anchor `"0"`, current version. | CONFIRMED |
| Who migrates, and when? | `medialibraryd`, at the request of the post-update data migrator or of the first program that opens the library. It validates again before every transaction, database operation and import. | CONFIRMED; the order on this phone OPEN |
| Does the sync protocol carry or check the database version? | No. The phone checks only the host's `HostInfo.Version`, which must be at least `10.5.0.115`. | CONFIRMED |
| Does Apple's host read the phone's database? | No. | CONFIRMED for the sync path |
| How does Apple's host treat a new iOS version? | As the newest it knows. Its per-version table has lower bounds only, the highest at iOS 26.0, and no refusal. | CONFIRMED |
| What changed in iOS 27.2? | One index on `album`. No table, column, trigger or view. | CONFIRMED |
| Can an update rewrite data without a version change? | Yes. A new collator version or ordering language rebuilds the sort map and can merge collections. Neither changed in 27.2. | CONFIRMED |
| What does a host see after an update? | The same anchor, a revision above the last synced one for every item, and emptied groups collected. | CONFIRMED on this phone |

## 4. The database version on the phone

### 4.1 One constant, exact equality

CONFIRMED. The version is SQLite's `PRAGMA user_version` of `MediaLibrary.sqlitedb`. MusicLibrary defines the current value in `_ML3MigrationGetLatestVersion` (`0x1d1ac907c`):

```text
0x1d1ac907c  mov  w0, #0xc610
0x1d1ac9080  movk w0, #0x24, lsl #16     ; 2410000
0x1d1ac9084  ret
```

The same function in 24A446 (`0x1d11284b8`) returns 2400060. The compiler also writes the constant directly into the other functions that need it (section 5.4).

`+[ML3MusicLibrary(Schema) userVersionMatchesSystemUsingConnection:]` (`0x1d1aec5a0`) reads the file's version and compares it with 2410000 (`cmp w19, w22; b.eq`). A difference logs "[Schema] Database version is %d, not %d". Up to date means exactly equal. There is no compatibility range, no minimum readable version and no second schema number. A missing file reads as version 0.

INFERRED, from the 321 versions of the migration chain: a version reads as `MM m dddd`, so 2400060 is 24, 0, 0060 and 2410000 is 24, 1, 0000. Within one `MM m` group the last digits count up in tens. A new `m` starts again at 0000. The number does not map to an iOS release or to the framework version: MusicLibrary moved from 4026.100.84 to 4026.200.21 while the database moved from 2400060 to 2410000. Nor does the number say what a step changes: the step to 2400060 added two columns, the step to 2410000 only an index.

### 4.2 An older database: a chain of one-step migrations

CONFIRMED. `_ML3MigrationFunctionForLibraryVersion` (`0x1d1a0eec8`) is a compiled switch on the exact version of the file. It returns one of 320 step functions named `_ML3MigrationFunction<from>to<to>`, or nothing for a version that is not a key. Each version has exactly one next step. The chains begin at six versions (992140, 1002402, 1134019, 1140220, 1163070 and 1350000) and all end at 2410000. 24A446 has 319 steps. The only one 24B119 adds is `_ML3MigrationFunction2400060to2410000`.

The driver `_ML3MigrationMigrateLibraryToCurrentVersion` (`0x1d1ac9088`) runs the whole chain inside one `performTransactionWithBlock:`:

1. Look up the step for the current version. If there is none, fail with error 20, "No migration path exists to upgrade from (%d) to system version (%d)".
2. Run the step. If it fails, fail with error 30, "Failed to upgrade from version %d."
3. Repeat until the version is 2410000.

Steps do little themselves. They set flags on an `ML3MigrationDirectives` object, and the driver does the work once at the end: it re-creates every index and trigger from the current definitions, runs `ANALYZE`, updates the sort map, the search map and the collection representatives, and runs a sanity pass. The sanity pass checks that every table exists and repairs contents (`sanitizeDatabaseContentsUsingConnection:removeOrphanedAssets:itemTablesEntryCounts:`, `0x1d1a43138`). A failed sanity pass fails the migration.

Two cases skip the chain. A file at version 0 is built directly from the current schema and stamped 2410000. Its `item_state` is not the one a migrated library has: one old step adds 12 columns that the current schema lacks (chapter 04, section 4.1). That step is `_ML3MigrationFunction2320030to2340000`, so only a library first built below 2340000 has the 12 columns. That the step is reached only from below 2340000 is INFERRED from its name and its `user_version` write. OPEN: which iOS release first shipped 2340000, and so which phones carry the 12 columns. A Home Sharing library with an old version is not migrated.

### 4.3 A newer database: refused

CONFIRMED. `-[ML3MusicLibrary(Validation) _validateDatabaseUsingConnection:error:]` (`0x1d1a712f8`) compares the file's version with 2410000 at `0x1d1a71354` (`cmp w0, w26; b.le`). A higher version fails with error code 3:

```text
Media library database is too new for this device. (version = %d, latest known version is %d)
```

The file is not opened read-only, not migrated and not deleted. `medialibraryd` then refuses transactions, operations and imports on it (section 4.5). Only an internal build of iOS asks the user whether to rebuild the library (`-[ML3DatabaseValidation runValidation]`). INFERRED: a newer database appears only after iOS is downgraded.

### 4.4 A failed migration: truncated and rebuilt

CONFIRMED. When migration fails (a version with no step, a failed step or a failed sanity pass), validation reports error 5, "Failed to migrate database from version %d to %d", and logs "Recreating un-migratable database...". The rebuild runs only in `medialibraryd`: it is guarded by a daemon flag (`___daemonProcessInfo`) that only `medialibraryd`'s `main` sets. In any other process validation simply fails.

The daemon calls `-[ML3DatabaseConnection deleteDatabase]` (`0x1d1bac82c`). It truncates the file in place (`_ML3TruncateDatabase`, which keeps the name and inode) and then builds the tables from the current schema. Corruption, disk errors that an integrity check confirms, and the service's `recreateDatabaseAtPath:` request take the same truncate-and-rebuild path. On a customer phone no copy of the old file is kept; only internal devices back it up first.

What each event leaves:

| Event | `_UUID` | `db_info` | `MLSyncClient*` properties | `entity_revision` | Anchor the phone reports |
| --- | --- | --- | --- | --- | --- |
| In-place migration | same | same | unchanged | continues | unchanged |
| Truncate and rebuild | new | no row | none | empty | `"0"` |
| Sanity repair of inconsistent item tables | same | same | `LastSyncedRevision`, `SyncAnchor`, `GenerationID`, `HasPendingAssets` deleted | continues | `"0"` |
| Restore from a backup | from the backup | from the backup | `LastSyncedRevision` and `SyncAnchor` deleted; `MLRestoreToken` set | from the backup | `"0"` |

A rebuilt database has the current version, so its version cannot reveal the rebuild. Its `_MLDatabaseProperties` holds only `OrderingLanguage` and a new random `_UUID`. Its `db_info` row appears only when a host sends `update_db_info`.

The sanity repair runs when the counts of `item`, `item_extra`, `item_stats`, `item_store` and `item_video` disagree. It logs "Sanity check: resetting sync anchors". It runs after a migration and in the phone's maintenance task, so an update that finds half-written item rows resets the sync state without rebuilding anything.

A database removed from outside, over AFC, is not noticed. No image watches the files (no vnode source, `kevent`, `NSFilePresenter` or `fstat` on them), and the only signal that makes clients close their connections, `MLMediaLibraryWillDeleteDatabaseNotification`, is posted only by the truncation above (MusicLibrary `0x1d1aa84f0`). CONFIRMED. Until a restart, `medialibraryd` and its clients go on using the unlinked files through their open and pooled connections, and the daemon's set of validated paths (section 4.5) and each client's validation cache (MusicLibrary `0x1d1b9e4b8`) can skip validation. The daemon may then create an empty file with no tables that nothing builds, and reads fail with plain errors such as "no such table". INFERRED from the CONFIRMED caches. After a restart the first open validates, finds `user_version` 0 and builds the tables; chapter 10, section 13.4 records this on one phone.

INFERRED from upstream SQLite (the phone's `libsqlite3` was not read): an old `-wal` cannot be replayed into the new database, because SQLite deletes a `-wal` it finds beside an empty main file. The risk is an old `-shm` still named while a live process holds it, which a new main file would trust; unlinking `-shm` first and the main file last, then restarting, leaves no such pairing. A rename changes nothing for open connections, which follow the inode, and leaves the old files in the tree. The folder also holds files of other services: `lockdownd` opens `iTunes_Control/iTunes/IC-Info.sidr` without creating it and passes the folder to its FairPlay set-up (`lockdownd` `0x100008cec`, `0x100012858`; the purpose is INFERRED).

### 4.5 Who runs the migration, and when

CONFIRMED unless marked.

- **`medialibraryd` owns the file.** Its `main` (`0x100007454`) turns off automatic validation, sets the daemon flag and starts the XPC service `com.apple.medialibraryd.xpc`. launchd starts it on demand. It does not validate at launch.
- **Validation is a request to the daemon.** `-[MLDMediaLibraryService validateDatabaseAtPath:withCompletionHandler:]` (`0x100002174`) runs MusicLibrary's `ML3DatabaseValidation` on a serial queue and remembers each validated path for the daemon's lifetime.
- **The daemon validates before all work.** It validates before every transaction (`beginTransactionForDatabaseWithContext:completionHandler:`, call at `0x10000317c`), every database operation (call at `0x10000e25c`) and every import (`performImport:fromSource:withUUID:completionHandler:`, call at `0x10000e9c0`). When validation fails it refuses: "Validation failed - skipping starting a transaction", "Database validation failed. Aborting database import."
- **Every client validates on open.** `-[ML3MusicLibrary connectionWillOpenDatabase:]` (`0x1d1a31c58`) validates when a process opens its first connection to the library. Outside Home Sharing libraries this is a synchronous XPC call to `medialibraryd`, so the client waits until the daemon answers. No lock file and no "migration in progress" property exist.
- **The data migrator asks after an update.** The plug-in `/System/Library/DataClassMigrators/MediaLibrary.migrator` (class `MLMMediaLibraryMigrator`) asks for validation of every library in `performMigration` (`0xd38`, call at `0x10e4`). A failure is logged ("[Migrator] Failed to validate media library %{public}@ during migration") and the plug-in still reports success. Its `estimatedDuration` (`0x1460`) is (2410000 minus the library's version) / 200 seconds per library.
- **Sync and cloud services follow.** `atc` starts at boot, and the launchd plists of `atc` and `itunescloudd` list the data migrator's completion event (`com.apple.datamigrator.datamigrationcompletecontinuerestore`) among their launch events. The Music sync bundle opens the library during `atc`'s startup and `itunescloudd` at the end of its own startup, so either would trigger the migration if the migrator had not.

INFERRED: in the normal case the database is migrated before any host connects. OPEN: which of these ran the migration on this phone. The database records no trace of the caller.

### 4.6 One transaction, visible in the write-ahead log

CONFIRMED. The chain runs in one SQLite transaction. An interrupted migration rolls back to the old version, so only two states are observable: old and new. On the phone after the update the main database file still said 2400060 and the write-ahead log held 2410000, because the last checkpoint was before the update. A copy of the main file without its WAL shows the old version and the old schema. Chapter 04, section 2.2 copies both.

## 5. Other version stamps in the database

### 5.1 The sort collation and the ordering language

CONFIRMED. Two stamps in `_MLDatabaseProperties` can rewrite library contents across an update without any change of `user_version`.

- **`MLSortMapUnicodeVersion`** is 4 bytes: `ucol_getVersion` of the collator that builds sort keys (`_iPhoneSortKeyBuilderCopyVersionData`, `0x1d1a6fc08`). At the end of every validation, `validateSortMapUnicodeVersionOnConnection:` (`0x1d1a7b6a8`) compares it with the system's value. On a difference it logs "System unicode version does not match version stored in the library -- upgrading sort map" and rebuilds the sort map (`inTransactionUpdateSortMapOnConnection:forceRebuild:forceUpdateOriginals:`, `0x1d1a7a078`). The rebuild rewrites every `name_order` and `name_section` and the order and `grouping_key` columns that depend on them. It then removes collections without members and merges collections whose names now collate equal ("Unicode version has changed after sort map update. Coalescing mismatched collections..."). INFERRED: a merge can remove the persistent IDs of albums or artists.
- **`OrderingLanguage`** is the language the sort map was built for. When it differs from the device's language, validation first runs a preprocessing pass ("Requires pre-processing because of a language mismatch") that rebuilds the sort map, creates indexes and re-localises the names of the system playlists.

Neither fired in the update to 27.2. The collator version (`99880000`) and the ordering language were the same in both captures, and the 1,116-row `sort_map` was identical, although the compatibility version of `libicucore` moved from 78132 to 78138.

INFERRED: only the collator version is compared. A change of the section tables (`MLSortMapTables.plist`) or of collation data that leaves `ucol_getVersion` unchanged would not cause a rebuild.

The same rebuild explains the sort keys that chapter 05, section 1 sees spaced out again after an insert. New names are inserted between existing `name_order` values. When no room is left the phone logs "initial sort map insert failed - commit what we have and force a rebuild", rebuilds the whole map and retries the insert (`-[ML3SortMap insertStringsIntoSortMap:didReSortMap:]`, `0x1d1a7d4a4`).

### 5.2 The cloud library version

CONFIRMED. `MLCloudDatabaseUserVersion` is not a schema version. It is the data version of the iCloud Music Library client, written by `itunescloudd`. At the end of its startup, `-[ICDServer _migrateToLatestUserVersionForConfigurations:]` (call at `0x10006a128`) walks a ladder of about 70 cloud migration steps ("Migrating to version %ld from userVersion=%ld, sagaDatabaseRevision=%ld, jaliscoDatabaseRevision=%ld"). 24B119 adds steps 810000 and 810010; the latest value is 810010 (`0xc5c1a`).

- The 810000 step clears `album.cloud_channel_name` for released albums.
- The 810010 step looks for duplicate `store_saga_id` values and asks for a full cloud resync if it finds any.
- INFERRED from the branch structure: both data changes run only on an internal build with a cloud library (`MSVDeviceOSIsInternalInstall`, and a nonzero cloud revision). The version number is written either way.

On this phone the value went from 800000 to 810000 to 810010 in two commits after the migration, and nothing else changed with it. Nothing in validation, `medialibraryd`, `atc` or the Music sync bundle compares it. The sync bundle writes it only from a paired Apple Watch's sync parameters.

### 5.3 Other properties that move on their own

CONFIRMED. These `_MLDatabaseProperties` keys are written by `itunescloudd`. None of them affects an ATC sync.

| Key | Meaning |
| --- | --- |
| `MLEnableICMLConfig` | A one-time request to turn the iCloud Music Library on or off for an account. `itunescloudd` reads and deletes it. Present on this phone only between two commits after the update. |
| `MLArtistHeroImageImportDatabaseRevision`, `MLArtistHeroImageImportDate` | The revision and date up to which artist images were imported. This is the background catch-up that chapter 04, section 2.2 observes. |
| `MPDateLastSynced` | The date of the last sync of playback positions through iCloud key-value storage. It is not the date of a music sync. |
| `MLUbiquitousBookmarkEntityRevisionAnchor`, `MPUbiquitousBookmarkDomainVersionAnchorToken`, `MPUbiquitousBookmarkDomainVersionRemoteToken` | Progress markers of the same playback-position sync. |
| `MLCloudDatabaseUpdateClientBuildVersion` | The iOS build at the last cloud library update. When the build differs, `itunescloudd` clears its last update time and so refreshes the cloud library after an update. Absent on this phone, which has no cloud library. |

### 5.4 Where the compiled-in version is compared

CONFIRMED. MusicLibrary 24B119 builds the constant 2410000 at 12 places:

- the latest-version function, the migration driver, the validation and the up-to-date check;
- the build of a new database and the new migration step;
- three places in `performMainentanceTasksUsingActivity:`, which skips maintenance when the versions differ;
- `cleanupArtworkWithOptions:`, which skips artwork cleanup when they differ;
- two purgeable-storage functions, which do not purge when they differ.

Outside MusicLibrary, `medialibraryd`'s storage statistics skip a library whose version differs (`0x100005844`), and the migrator uses the constant for its estimate (section 4.5). All of them were changed together. Maintenance, cleanup and purging therefore never run on a database that could not be migrated.

## 6. The sync service on the phone

### 6.1 `atc` and the Music sync bundle

CONFIRMED.

- **`atc` is a launcher.** `/usr/libexec/atc` has 0x3c8 bytes of code. It logs the iOS build, waits for SpringBoard and runs `-[ATDeviceService run]` from AirTrafficDevice. launchd starts it at boot. It compares no version.
- **The Music client is a bundle.** `ATDeviceService` loads every bundle in `/System/Library/SyncBundles`. The Music client is `MusicLibrary.syncBundle` (`com.apple.mobileipod.sync`, principal class `MLSyncClient`).
- **The bundle never touches SQLite itself.** It has no `sqlite3_` call, no `PRAGMA` and no reference to the version or validation functions. It reads the library through `ML3MusicLibrary`, so its first read validates through `medialibraryd`.
- **`medialibraryd` applies the host's operations.** In `reconcileSync:withNewAnchor:progressCallback:error:` (`0x1a184c`) the bundle builds an `ML3MutableDatabaseImport` and sets three things on it: the Reset flag, which is `syncType == 2` (`setResetSync:`, `0x1a2468`); a cloud flag, which is `[ICCloudClient isAuthenticated]` (`setSagaEnabled:`, `0x1a2480`); and the host's new anchor (`setSyncAnchor:`, `0x1a24a4`). It then calls `-[MLMediaLibraryService performImport:fromSource:withProgressBlock:completionHandler:]` with source 5. A sync type of 0 imports nothing. The daemon runs `ML3ITunesSyncImportOperation` in one writer transaction, and lists and reads the operation files in `iTunes_Control/Sync/Media` itself (`0x1d1b93ce8`). A commit hook (`enqueueBlockForTransactionCommit:`, hook at `0x1d1b94e0c`) writes three properties, and only when the import succeeded: `MLSyncClientLastSyncedRevision`, `MLSyncClientSyncAnchor` and a new random `MLSyncClientGenerationID`.
- **The host never sees the schema.** The importer reads each operation through a fixed vocabulary of host keys (`ML3ITunesSyncTrackImportItem` and its siblings) and maps it onto the phone's current tables. A schema change is invisible to the host unless Apple adds a key. Chapter 05 describes the effects of each operation.
- **Prepare cannot fail and writes nothing to the database.** `-[MLSyncClient prepareForSyncWithHostAnchor:progressCallback:grappaID:hostVersion:error:]` (`0x1a0f48`) ignores the host's anchor and version. Its database work runs in a read-only transaction (`0x1a102c`). It removes stale `Sync_*.plist` files but not their `.cig` sidecars (`0x1a10bc`), removes and recreates `/var/tmp/Media/` (`0x1a1164`), writes the play counts changed since `MLSyncClientLastSyncedRevision` as plist files and always returns success. When `MLSyncClientLastSyncedRevision` is 0 it leaves the playlist part out and logs "Not saving out playlist data because this a first sync" (`0x1a1314`). That is the only test for a first sync found in the bundle. The play counts go to `iTunes_Control/iTunes/PlayCounts.plist` and its `.cig`, written again at every prepare (MusicLibrary `0x1d1a34fec`, `0x1d1a350fc`). The playlist part goes to `OnTheGoPlaylist`, `GeniusPlaylist` and per-playlist `…_%llu` files (resources 3 to 7, `0x1d1a8f080`), which only this export and the `container.filepath` property write; no code on the phone reads them. No image read here reads `VoiceMemos.plist` or the zero-byte `iTunesControl` either; whether another component reads `VoiceMemos.plist` is OPEN.
- **`atc` keeps a record of each host and closes an idle link.** The record is one entry per `LibraryID` under `Hosts` in the `com.apple.atc` defaults (section 6.4). In a session `atc` sends `SyncAllowed` twice: at link start, and again after it accepts `RequestingSync` (chapter 02, sections 4.4 and 6.1). It closes a link on which nothing arrived for 45 s, or 180 s for a host that announced `LocalCloudSupport` (`-[ATLegacyMessageLink _checkMessageTimeouts]`, `0x22b7a40f0`; chapter 02, section 14.3).
- **The legacy path keeps no other host state.** `ATDeviceSettings` keeps host information, sync state and last-sync times per library in the same `com.apple.atc` defaults (`init` `0x22b78bb6c`), but only the paired-device protocol on `com.apple.atc2`, used for a Watch, calls it. The legacy manager that serves `com.apple.atc` never does. The Music bundle's `syncLibraryID` and `isDeviceSetupComplete` (bundle `0x1accf8`, `0x1bab78`) also lie on other paths. Before it reads the sync types of `FinishedSyncingMetadata`, the legacy manager hands its `PurgeDataBytes` to CacheDelete (chapter 02, section 12.1).

### 6.2 The only version rule: `HostInfo.Version`

CONFIRMED. `-[ATLegacyDeviceSyncManager _handleRequestingSyncMessage:fromLink:]` (`0x22b798838`) checks the `Version` of the host's `HostInfo`:

1. If it is missing, the sync fails.
2. If it is the literal `IGNORE`, the phone logs "Explicitly ignoring version check" and continues.
3. Otherwise the phone compares `"10.5.0.115"` with it by `compare:options:` with `NSNumericSearch`. If the minimum is greater, the sync fails.

A failure logs "Sending sync disallowed, incompatible version: %{public}@ vs. %{public}@ required" and sends `SyncFailed` with the constant parameters `{ErrorCode: 12, RequiredVersion: "10.5.0.115"}` (`0x26be987c0`). Apple Devices sends `13.2.4.123` (chapter 02, section 7.1). The rule and the literal are identical in 24A446 and 24B119.

Nothing else in `atc`, AirTrafficDevice or the Music sync bundle compares a version with anything the host sends: not `user_version`, not the schema, not the iOS build, not the collation. The bundle keeps a settings version for the Media dataclass (`_migrateSyncSettings`, latest 2), which is local bookkeeping.

### 6.3 `SyncFailed` codes

CONFIRMED in the code. None of these messages was observed on the phone. The names are those of Apple Devices' own code-to-name table (`0x140455c78` in AMPDevicesAgent 1.6.4.94).

| Trigger | Parameters | Name |
| --- | --- | --- |
| `HostInfo.Version` missing or below `10.5.0.115` | `ErrorCode` 12, `RequiredVersion` `"10.5.0.115"` | IncompatibleVersion |
| The phone is in multi-user mode (and the host is not Configurator) | `ErrorCode` 9, `EducationModeEnabled`, `ManagedOrganizationName` | SyncDisallowed |
| Another library is syncing: a current link exists, is not this link, and has an identifier that differs from the host's `LibraryID` (`0x22b798bb4`). The phone sets that identifier only from a `Capabilities` message sent by the host with a `LibraryID` (`0x22b798718`), so the check never fires against a host that sends none | `ErrorCode` 9 | SyncDisallowed |
| The host cancelled before the sync started | `ErrorCode` 2 | Canceled |
| The Grappa session could not be established | `ErrorCode` 4 | NotAuthorized |
| `FinishedSyncingMetadata` with no active session or the wrong session number | `ErrorCode` 23 | RequestNotValid |
| An operation file's `.cig` signature fails verification | `ErrorCode` 4, at the end of the session | NotAuthorized |
| Any other error of the client or the import | `ErrorCode` set to that error's code | as the code says |

No code means "database too new" or "schema not supported". OPEN: what the phone reports when `medialibraryd` cannot validate its library. INFERRED: the import fails and its error code is forwarded in the last row's form.

### 6.4 Anchors and Reset

CONFIRMED. The phone never asks for a Reset. `-[MLSyncClient currentSyncAnchor]` (`0x1a0ee8`) returns the property `MLSyncClientSyncAnchor`, or `"0"` when it is absent, and `ReadyForSync` carries that string as `DataclassAnchors.Media`. The phone does not compare it with the host's anchor. Apple's host makes the choice (section 7.3).

The anchor the phone stores is the string of the host's `FinishedSyncingMetadata`, copied without a check (`0x1d1b94e9c`). The phone does not read the `revision` of the operation file: `_syncOperationsFromPlistFile:` reads only `operations` (`0x1d1b95264`). No code in AirTrafficDevice, the bundle or the importer compares an anchor with anything, and none requires a Reset when no anchor is stored. A Delta against an anchor of `"0"` is accepted by the code. INFERRED: a `FinishedSyncingMetadata` that finds no operation file still commits an import of zero files and stores its anchor, because the signature check passes for an empty list (`0x1a14f4`).

What zeroes the anchor the phone reports:

| Cause | Where |
| --- | --- |
| Restore from a backup | `_clearDatabasePropertiesForRestore:` in the sync bundle |
| A "clear sync data" request | `-[MLSyncClient clearSyncData]` (`0x1a96c0`) deletes only this property |
| The sanity repair of section 4.4 | `sanitizeDatabaseContentsUsingConnection:...` |
| A truncated and rebuilt database | a new database has no such property |
| Three old migration steps | 1140310 to 1140320, 1280250 to 1280260 and 2100020 to 2100030 set it to 0 |

An iOS update does not. `-[MLSyncClient reconcileUpgradeWithError:]` (`0x1a3f48`) leaves the anchor alone, and the step to 2410000 does not touch it. On this phone the anchor was the same before and after the update.

`ReadyForSync` carries no library identity, no database ID and no revision. CONFIRMED: the phone uses the host's `LibraryID` in two places only. One is the refusal of a second library while one is syncing (section 6.3). The other is its record of hosts. `-[ATUserDefaults updateHostInfo:disabledAssetTypes:]` (`0x22b78a85c`) keeps one entry per `LibraryID` under `Hosts` in the `com.apple.atc` defaults: the host's name, operating system, `Wakeable` flag and synced dataclasses. It is written when `RequestingSync` or a `HostInfo` message arrives, before the version check, so a refused session leaves it too. When a host with another `LibraryID` claims the same dataclasses, the phone removes those asset types from the other hosts' entries and deletes an entry left with none. A `LastSync` time is added only after a session that ended without error on a link whose identifier is set. The only reader of the record is the service that wakes a host over Wi-Fi (chapter 03, section 5.8). No refusal, Reset, removal or prompt depends on it. INFERRED: the defaults are `/var/mobile/Library/Preferences/com.apple.atc.plist`, which a computer cannot reach through AFC.

### 6.5 What the phone reports to the host

CONFIRMED. The data a host receives that comes from the database:

- **Anchors** (section 6.4).
- **`AssetMetrics`**: `Music: {_Count, _PhysicalSize, _PurgeableSize}`. It is computed by a query on track properties, not on columns, so a renamed column would not change it.
- **`PlayCounts.plist`**, written during prepare. Its keys are a fixed vocabulary, and its `version` key is the constant 0, a file-format number.
- **Tracks deleted on the phone.** The phone writes the `persistentID` of a deleted track into `iTunes_Control/iTunes/iTunesSyncDeletes.plist` only when the track has a `sync_id` (`+[ML3Track _writeSyncDeletesToPlistWithLibrary:forPersistentIDs:]`, MusicLibrary `0x1d1a4aec4`). Prepare reads that file (`saveTrackMetadataSinceRevision:`, `0x1d1a34664`), merges each entry into the tracks it reports to the host, and writes an empty dictionary back (`0x1d1a34904`). No code applies the file to the database, so a stale one at most reports old deletions to the host once.

No `user_version`, table list or sort order reaches the host. `Capabilities.GrappaSupportInfo` (`version`, `deviceType`, `protocolVersion`) goes only into the Grappa exchange, unchanged in 24B119.

## 7. Apple's Windows host

### 7.1 No read of the phone's database

CONFIRMED. `AirTrafficHost.dll` has no `MediaLibrary`, `sqlitedb`, `user_version`, `PRAGMA` or `sqlite3_` string, in ASCII or UTF-16, in either build. `AMPDevicesAgent.exe` links SQLite 3.36.0 for the host's own library files. Its `PRAGMA user_version` code (`0x14033e928`) opens those files, never a file of the phone. INFERRED, with medium-high confidence: no path of the host reads the phone's database (not every AFC read was traced).

### 7.2 iOS versions as lower bounds

CONFIRMED. `0x14051f258` derives per-device flags from the iOS version. It compares a 16-bit version with `0x700`, `0x800`, `0x840`, `0x900`, `0x930`, `0xA00`, `0xB00`, `0xC00`, `0xF40` and `0x1A00` (iOS 7.0 to 26.0), always as a lower bound (`jb` or `jae`). There is no upper bound and no refusal: iOS 27 is treated like iOS 26. INFERRED: the 16-bit value is `major << 8 | minor << 4 | patch`.

The legacy SQL path for old devices (`UserVersionCommandSets`, `0x14053cf60`) counts down from 26 and takes the newest command set it knows. It is skipped for phones that report `SupportsAirTraffic` (`0x1405bd254`, `0x140520061`). Firmware catalogues fall back to a `Universal` entry for an unknown build.

### 7.3 Reset or Delta from anchors only

CONFIRMED. The selector (`0x14046d034` in AMPDevicesAgent 1.6.4.94, `0x14046cf0c` in 1.6.4.90; chapter 08, section 14.3) chooses Reset when the host's last anchor for the dataclass (entry `+0x14`) differs from the anchor the phone reported (entry `+0x1c`), Delta otherwise, and nothing when the dataclass is not in the session. The host does not keep its last anchor on the host: it reads it at attach from the phone's `iTunesCDB`, and a new entry starts at 1 when that file is absent (chapter 10, section 5.3). The anchor reader (`0x14045637c`) only logs a missing or unreadable anchor; it does not refuse. It takes such an anchor as 0, which differs from any last anchor the host holds (at least 1), so that dataclass gets a Reset (chapter 02, section 7.3). A Reset needs only that the dataclass is in the session's main mask, the one whose anchors `RequestingSync` carries; a dataclass only in the weaker mask gets Delta (chapter 10, section 7.4).

### 7.4 No change for iOS 27.2

CONFIRMED. Between 1.1540.23042.0 and 1.1540.24088.0:

- `AirTrafficHost.dll` changed one function. Sending on a closed link now logs and returns failure. Its other string changes are a build path and certificate dates.
- `AMPDevicesAgent.exe` has the same strings and imports. After address normalisation the version table, the device setup handler, the anchor reader and the Reset or Delta selector are unchanged.

INFERRED, with medium confidence: no behaviour of the agent changed, since 365 functions differ in text and only the relevant ones were read. Apple shipped no host update for iOS 27.2.

## 8. What changed between iOS 27.0.1 and 27.2

### 8.1 The schema

CONFIRMED. The only schema change in the code and in the captures is one index:

```sql
CREATE INDEX IF NOT EXISTS AlbumCloudChannelName ON album (cloud_channel_name ASC)
```

| | 27.0.1 capture | 27.2 capture |
| --- | --- | --- |
| `user_version` (WAL applied) | 2400060 | 2410000 |
| `schema_version` | 43465 | 43466 |
| Tables, indexes, triggers | 45, 136, 16 | 45, 137, 16 |

The two `sqlite_schema` dumps differ by that one line. The other SQL changes in MusicLibrary are a new query, `SELECT album_pid FROM album WHERE cloud_channel_name=?`, and a pair of parentheses added to the `contains_classical_work` UPDATE. The columns `cloud_channel_name` and `is_followed` were added by the previous step, 2400050 to 2400060, and exist in 27.0.1. INFERRED, from names and strings: the change supports subscriptions to pre-release album channels.

### 8.2 The migration step

CONFIRMED. `_ML3MigrationFunction2400060to2410000` (`0x1d1a284d4`) is 30 instructions:

```text
setNeedsToRecreateIndexes: 1
setNeedsToRecreateTriggers: 1
setCurrentUserVersion: 2410000
executeUpdate: "PRAGMA user_version = 2410000;"
```

It runs no other SQL. The driver then runs the whole index list, whose statements are `CREATE INDEX IF NOT EXISTS`, so only the missing index is created. INFERRED: the new index statement is a member of that list (it is new in 24B119 and no code references it directly). The triggers are re-created with unchanged text.

In the WAL the migration is one 44-page commit: the header page with the new version and schema cookie, the new index, 31 rewritten index pages, `sqlite_stat1` from `ANALYZE`, and the rows of the sanity pass's collection (section 9).

### 8.3 Frameworks and daemons

| Component | Change from 24A446 to 24B119 | Label |
| --- | --- | --- |
| MusicLibrary | The new step and index. Album cloud-channel imports and a deferred item-update import mode. A Siri shared-playlist index. The `ML3ImportSession` constructor gained a `bool`, which the sync importer passes as false. Migration, validation and rebuild code and their messages are unchanged. | CONFIRMED |
| MediaLibraryCore | Strings and symbols identical. The code differs only in relocated addresses. | CONFIRMED for strings and symbols; INFERRED for the code |
| AirTrafficDevice, ATFoundation | No change beyond relocation. `10.5.0.115` and `IGNORE` unchanged. | CONFIRMED |
| AirTraffic | `-[ATAirlock processCompletedAsset:]` moves uploaded files with `mkdirat`, `renameatx_np` and `unlinkat` relative to the AFC root. Paths under `iTunes_Control` behave as before. | CONFIRMED; the purpose, keeping files under the AFC root, INFERRED |
| `atc`, `MusicLibrary.syncBundle` | Code byte-identical. Only build metadata and signatures differ. | CONFIRMED |
| MediaLibrary migrator | One instruction: the constant of its estimate. | CONFIRMED |
| `medialibraryd` | A Siri shared-playlist index task and its entitlement. Validation, recovery and import unchanged. | CONFIRMED for named functions |
| `itunescloudd` | The cloud-channel feature and the cloud steps 810000 and 810010. | CONFIRMED |
| launchd plists | Only `com.apple.mediaremoted.plist` gained a Mach service. | CONFIRMED |

No message, key, required field or default that a host sends or receives changed.

### 8.4 An iOS 26 database

CONFIRMED from read-only captures of a second iPhone17,2, one no computer had synced, on iOS 26.6.2 (build 23G90) and again after it updated itself to iOS 26.7.3 (build 23H131). The firmware of these builds was not read.

| | iOS 26.6.2 and 26.7.3 | iOS 27.0.1 (section 8.1) |
| --- | --- | --- |
| `user_version` | 2350000 | 2400060 |
| Tables, indexes, triggers | 45, 133, 16 | 45, 136, 16 |

The update from 26.6.2 to 26.7.3 left the version and the whole `sqlite_schema` unchanged. The 45 tables are those of iOS 27, and `item_state` has the 64 columns of a migrated database (chapter 04, section 4.1). Against the 27.0.1 schema:

- `album` lacks the eight columns at the end of 27.0.1's table: `date_released`, `is_prerelease`, `release_event_aux_info`, `album_type` (default 1 in 27.0.1), `content_timestamp`, `expected_release_date`, `cloud_channel_name` and `is_followed`. It lacks the indexes `AlbumIsFollowed`, `AlbumIsPreRelease` and `AlbumReleaseEventAuxInfo` on three of them, and its `CREATE TABLE` names the table without quotes.
- `item_store` lacks `expected_release_date`, the last column of 27.0.1's table.
- The triggers `on_insert_item_setInMyLibraryColumn` and `on_update_item_setInMyLibraryColumn` set `item.in_my_library` with one OR: `home_sharing_id OR (store_saga_id AND cloud_in_my_library) OR purchase_history_id OR (sync_id AND sync_in_my_library) OR is_ota_purchased`. 27.0.1 nests `CASE` expressions in the same order, so a store saga ID decides alone: an item with one that is not in the cloud library gets 0 there, and 1 on iOS 26 when a later term holds. A synced track, with no saga ID, gets `sync_in_my_library` in both.

INFERRED: the migration steps from 2350000 to 2400060 add the columns and indexes and rewrite the triggers. Section 8.1 places `cloud_channel_name` and `is_followed` in the step from 2400050. OPEN: which steps add the others.

## 9. What the update did to a library

CONFIRMED from the 27.2 capture. The WAL holds ten commits after the last checkpoint before the update:

| Commit | Change |
| --- | --- |
| 1, 3, 9, 10 | `MLSiriIndexValidityRevision` rewritten |
| 2 | The migration of section 8.2. In the same transaction four groups that an earlier session of the tool had emptied were deleted (two albums, one album artist, one item artist) and given tombstone revisions |
| 4 | `MLEnableICMLConfig` inserted |
| 5, 6 | `MLCloudDatabaseUserVersion` 800000 to 810000, then 810010 |
| 7 | `MLEnableICMLConfig` deleted |
| 8 | All 414 items given a new live revision, one each, contiguous |

What follows from it:

- **Emptied groups are collected by a known routine.** `+[ML3Collection removeOrphanedCollectionsInLibrary:usingConnection:]` deletes collections that no item belongs to. MusicLibrary 24B119 calls it from the sanity pass of a migration, from a sort-map rebuild, from the maintenance task and from the end of an import (`ML3ImportSession::_finishImport`). INFERRED: the sanity pass collected the four groups here, since the maintenance date did not change and no import ran. Chapter 05 records that some sync sessions collect emptied groups and others do not. The end of an import is the likely route there, but the condition was not read (OPEN).
- **The sync state did not move.** The anchor, `MLSyncClientLastSyncedRevision`, `MLSyncClientGenerationID` and `_UUID` were unchanged, and the file's birth time was the same. The library was migrated in place, not rebuilt.
- **The revision counter did move.** Between the two captures the highest revision rose from 131531 to 132540 with no sync: 414 from commit 8, 4 from the collection and the rest while the phone was in use before the update. OPEN: what gave every item a new revision. It came after the cloud steps and resembles the background renumbering of chapter 04, section 3.4.
- **The next session reports all of it.** Prepare exports what changed since the last synced revision, so the next session after the update reports every item and the four tombstones as changed on the phone. A client that treats revisions above its last synced one as foreign edits has to expect this after an update.

## 10. Guidance for a sync client

These follow from the findings above. They are not Apple's rules.

1. **The protocol needs no knowledge of the database version.** Send the operation vocabulary of chapter 02, and send a `HostInfo.Version` of at least `10.5.0.115`. Apple's is `13.2.4.123`. Do not rely on `IGNORE`.
2. **A changed version says that Apple changed something, not what.** If you read the phone's database, check the tables and columns you read, with their types. An index-only or additive change leaves those reads valid. No check can see a column whose meaning changes under the same name.
3. **Copy the WAL with the database.** After an update the new version and schema may exist only in the WAL.
4. **Expect the phone to change data on its own across an update:** collected empty groups, a new revision for every item, new cloud properties. These are not edits made by another host.
5. **Detect a replaced library by its identity, not its version.** An anchor of `"0"`, or any anchor other than the one your last session set, a new `_UUID` and a missing `db_info` row are the signs. A rebuilt database has the current version.
6. **Treat sort-dependent state as stale** (orders, sections, collection identities) when `MLSortMapUnicodeVersion` or `OrderingLanguage` changed.
7. **Do not gate on the cloud properties** of section 5.2 and 5.3. They change with updates and accounts and say nothing about the schema.
8. **A downgrade cannot be fixed by a host.** The phone refuses a database newer than its code.

## 11. Comparison with `rustunes`

DESIGN. The tool follows the guidance of section 10 where it reads the phone's database:

- **Catalogue read.** The reader in `music-protocol` accepts any `user_version` at or above 2350000, the lowest version it was tested with (iOS 26.6.2 and 26.7.3, section 8.4), when every table and column it reads is present with its type. It reads no column that iOS 26 lacks. It refuses an older version or a missing column. The exception is `item_state.is_supported_type`. An `item_state` with none of the 12 columns that only the old migration step adds, the shape of a database built new (chapter 04, section 4.1), may lack it, and then every track reads as supported: 1, the default a migrated table declares and the value the one migrated library examined held in every row. A table with any of the 12, in any letter case, must have the column as spelled. NULL in `is_placeholder`, `is_library_added` or `has_non_purgeable_asset` reads as 0 only in an `item_state` of the built-new shape that declares that column without a default. A migrated table declares them DEFAULT 0, and NULL there is refused. Reading NULL as 0 is a choice of the reader, not an observed meaning: for the purchase-history entries of the rebuilt library the phone's own trigger would write `is_library_added` 1, and the phone did write 1 there later, when the import of `rustunes init`'s session repopulated `item_state`; a track a sync inserts gets integers there (OBSERVED once; chapter 04, section 4.1). It reports whether the version is one the tool was tested with on a phone (2350000, 2400060 and 2410000). An untested version is read, not refused, and the tool says so once per command with the warning `untested_catalogue_version` (chapter 07, section 10.5).
- **Layout fingerprint.** For the models that compare every table, the library offers a SHA-256 of each table's name and columns, without indexes, triggers or the version. The 27.0.1 and 27.2 captures have the same fingerprint, while their hashes of the whole `sqlite_schema` differ. A database built new at 2410000 has a fingerprint of its own (`BUILT_NEW_LAYOUT_SHA256`, chapter 04, section 4.1), and so does iOS 26's at 2350000 (`IOS_26_LAYOUT_SHA256`, section 8.4). The checks that use a fingerprint require the same version and whole schema in the two snapshots they compare, so an index-only update like 27.2's passes and a column change fails, and a version at or above the one their fingerprint was seen at: 2400060 for the two iOS 27 fingerprints, 2350000 for iOS 26's. Those that build no `item_state` row and name no column iOS 26 lacks, the revision housekeeping check after an insert batch and the playback bridges of recovery, accept any of the three (`layout_observed`). The full-table model of an interrupted ordinary insert's recovery requires the migrated iOS 27 one (`layout_qualified`), because its expected rows model the 64-column `item_state` and iOS 27's `album`. Recovery proves an insertion into a database built new, or into iOS 26's, with the catalogue verifier instead, the check that verified the insertion when it was written. On those databases new tracks also spread over Apple's 50 music folders besides the ones in use, as on a phone with no music folder (chapter 04, section 4.4), since no model expects a track's folder to be registered already.
- **Database and WAL together.** The tool copies both (chapter 04, section 2.2).
- **Host version.** It sends `HostInfo.Version` `13.2.4.123`.
- **Anchor.** It requires the anchor in `ReadyForSync` to equal the one it saved ("Ready anchor differs from saved state"), so a zeroed anchor stops a session before any write.

OBSERVED on the iOS 26.7.3 phone of section 8.4: `rustunes device check` read it as never synced, `rustunes init` prepared it, and a single `push` and then a full `sync` of a library were verified, with tracks spread over Apple's 50 folders.

OPEN for the tool: detecting a rebuilt phone library from `_UUID`, the `db_info` row and a zeroed anchor (section 4.4), and keying sort-dependent cached state on `OrderingLanguage` and `MLSortMapUnicodeVersion` (section 5.1).

## 12. Open questions

1. Which process ran the migration on this phone: the data migrator, `atc`'s Music bundle or `itunescloudd`.
2. What gave every item a new revision after the update (commit 8), and whether it is the background renumbering of chapter 04.
3. What the phone sends to a host when `medialibraryd` cannot validate the library (too new or impossible to migrate).
4. Whether the cloud steps 810000 and 810010 change data on any customer phone.
5. Whether the new index statement is a member of the index list re-run after a migration.
6. Whether a collation change that leaves `ucol_getVersion` unchanged can change sort keys.
7. The condition under which the end of an import collects emptied groups.
8. The values of `GrappaSupportInfo` under iOS 27.2. The code is unchanged; the values were not captured.
9. What `itunescloudd` changed beyond the cloud steps (it was not compared function by function), and what the changed Photos sync bundle (`MobileSlideShow.syncBundle`) does differently.
10. On the host: whether the phone's `MinITunesVersion`, `MinBuildID` and `DBVersion` capability keys have a refusing consumer, and what the flags set at iOS 15.4 and 26.0 mean.
11. Which iOS release first shipped database version 2340000, the version whose migration step adds the 12 extra `item_state` columns (section 4.2).
12. Which migration steps between 2350000 and 2400060 add the `album` and `item_store` columns iOS 26 lacks (section 8.4), and what a database iOS 26 builds new looks like. The tool's checks that compare every table would refuse a layout they have no fingerprint for.

## 13. Evidence

### 13.1 Static analysis of the iPhone firmware

Apple's iPhone17,2 restore images for builds 24A446 and 24B119, downloaded from Apple's update server. Only the system cryptex (which holds the dyld shared cache) and the system volume were downloaded, as byte ranges of the restore archives. They were decrypted with keys from Apple's key service, and the files were read from their APFS images. The files and their hashes are in section 1.

Reading used `ipsw` 3.1.739: shared-cache symbols, image extraction, disassembly, string and cross-reference searches, and class dumps. Scripts written for this work resolved Objective-C selector stubs, constant strings and constant dictionaries of the cache. Builds were compared by symbol and string sets, by section bytes and by function-by-function disassembly with addresses removed. No Apple code was executed.

Re-checked after the analysis:

- the instructions of `_ML3MigrationGetLatestVersion` in 24B119 (2410000) and 24A446 (2400060);
- the body of `_ML3MigrationFunction2400060to2410000`: two setters with 1, a setter with `0x24c610` and one constant statement;
- in the 24B119 cache, the strings "Media library database is too new for this device", "Recreating un-migratable database...", `10.5.0.115`, the `AlbumCloudChannelName` statement, `MLSortMapUnicodeVersion` and "initial sort map insert failed - commit what we have and force a rebuild";
- the first instructions of the sync bundle's `currentSyncAnchor` (`0x1a0ee8`), prepare (`0x1a0f48`), `reconcileUpgradeWithError:` (`0x1a3f48`) and `clearSyncData` (`0x1a96c0`), and the `MLSyncClientSyncAnchor` and `"0"` operands of the first.

A later reading of the same 24B119 files, made for chapter 10, added to section 6 the import's flags and commit hook, what prepare does, the host record, the condition of the concurrency check and the idle timeout.

### 13.2 Static analysis of the Windows host

The binaries and hashes of chapter 02, section 1. String extraction (ASCII and UTF-16), `objdump` disassembly and a function-by-function comparison of both builds with addresses and registers normalised. Re-checked after the analysis: no `sqlite`, `user_version`, `MediaLibrary` or `PRAGMA` string in either `AirTrafficHost.dll`.

### 13.3 Phone database captures

Two read-only captures of `MediaLibrary.sqlitedb` and its WAL, taken as in chapter 04, section 2.2: the last on iOS 27.0.1 and the first on iOS 27.2, which the tool's failed `sync` retained. Copies were compared by schema dump, `_MLDatabaseProperties`, `sort_map` and revisions. The 27.2 WAL was replayed commit by commit to give the order of section 9.

For section 8.4, `rustunes device check` captures of the second phone's database and WAL, taken the same way on iOS 26.6.2 and on 26.7.3. Their `sqlite_schema` dumps were compared with each other and, entry by entry, with the 27.0.1 schema, and the tool's layout fingerprint of the 26.6.2 capture with that of a test database built from the 27.0.1 schema with the differences of section 8.4 applied.

### 13.4 Strength of the evidence

- The version rules, the migration chain, the refusal of a newer database, the rebuild, the validation points, the structure of `atc` and the sync bundle, the `HostInfo.Version` rule and the `SyncFailed` parameters were read in code. None was exercised on the phone.
- The schema change and the effects of the update on the library are shown by the captures.
- The order of processes after an update is partly INFERRED.
