# Library ownership and the first sync

This chapter describes what decides which computer an iPhone's music belongs to, and what happens the first time a computer syncs a phone. It covers the state of a phone that no computer has synced, the four files Apple's host keeps on the phone, how Apple's host decides that a phone is its own, when it starts a sync and what it asks for, the first session it runs with a phone it does not know, and what it does to a phone that another program filled. It ends with what the phone's own code allows a first host to do. The protocol is in chapter 02, the transport in chapter 03, the database in chapter 04, the receiver in chapter 05, Apple's session structure in chapter 08 and the phone's sync service in chapter 09.

## Contents

1. Scope and provenance
2. Evidence labels and conventions
3. Summary
4. A phone no computer has synced
5. The host's files on the phone
6. Library ownership in Apple's host
7. When Apple's host syncs, and what it asks for
8. Apple's first session with a phone it does not know
9. A phone filled by another program, then connected to Apple's host
10. What the phone allows a first host
11. Guidance for a sync client
12. Open questions
13. Evidence

## 1. Scope and provenance

| Item | Value |
| --- | --- |
| Phone | iPhone17,2, iOS 27.2 (24B119), with a music library that Apple's host had synced before |
| Phone firmware read | iOS 27.2 (24B119): the files of chapter 09, section 1, and `usr/libexec/lockdownd` |
| Host | Apple Devices 1.1540.24088.0 for Windows: `AMPDevicesAgent.exe` 1.6.4.94, `AirTrafficHost.dll` 4026.110.1.6, `MobileDevice.dll` 1860.2.5.1 and `AMP.Services.dll` |

Limits of this chapter:

- **No phone that no computer has synced was examined.** Everything said about such a phone was read in code.
- **Apple's host was read, not run,** with one exception: the guarded capture of section 8, which was stopped before the host could send `FinishedSyncingMetadata`. No session of Apple's host with a new phone ran to its end, and the phone's answers in that capture were not recorded.
- **Only the agent was read in depth.** `AMPDevicesAgent.exe` does the syncing. Of the interface layer `AMP.Services.dll` only the setup path was read. The Apple Devices window itself, Finder on macOS and iTunes for Windows were not read.
- **Identities are left out.** Values that identify a library, computer or phone are replaced by placeholders such as `<LibraryID>`.

The binaries are not redistributed. Only addresses, constants, field names and conclusions are published.

## 2. Evidence labels and conventions

| Label | Meaning |
| --- | --- |
| CONFIRMED | Read in disassembled code or data at the address given, or seen in the capture or on the phone when the text says "observed". |
| INFERRED | Follows from confirmed facts, names or strings, but was not shown directly. |
| OPEN | Not answered by the available evidence. |

A CONFIRMED code path is one read in the binary, not one seen to run.

Conventions:

- **Windows addresses** are virtual addresses in `AMPDevicesAgent.exe` 1.6.4.94 (base `0x140000000`). Chapters 02 and 08 cite build 1.6.4.90 by default. Chapter 08, section 14.3 maps the two. Addresses in the three DLLs (base `0x180000000`) name their file.
- **Phone addresses** follow chapter 09, section 2: AirTrafficDevice near `0x22b790000`, AirTraffic near `0x2559e0000` and MusicLibrary near `0x1d1a00000` in the 24B119 shared cache, the Music sync bundle from `0`, and `lockdownd` from `0x100000000`.
- **Host function names** such as `SetupDevice`, `ConfigureDevice`, `SetHomeMusicLibrary`, `SetIPodPrefs` and `SynchronizeDevice` are labels. Some repeat the agent's own log and assertion strings. The others were chosen by the analyst. None is a symbol.
- **"Prefs byte `0xNN`"** is the byte at that offset of the 1,232-byte structure of section 5.1.
- **"Configured"** is the host's word for a phone whose prefs byte 8 is set: a phone some Apple host has set up.
- **"Home library"** is the host's word for the music library a phone syncs with. Its log line is "device is on home music library" or "device is NOT on home music library".
- **"Host-synced"** describes a track, playlist, album or album artist whose row in the phone's database has `sync_id != 0`, whatever host sent it.

## 3. Summary

1. **The phone has no notion of an owning library or of a first sync.** Its sync service starts for any paired host, accepts any `LibraryID`, compares no anchor and keeps no state that changes how a later session is handled. CONFIRMED in the code of iOS 27.2 (section 10).
2. **Apple's host keeps what it knows about a phone on the phone**, in four files it writes over AFC: `iTunesPrefs`, `iTunesPrefs.plist`, `iTunesCDB` and `ArtworkDB`. The phone never reads them. The host keeps no database of devices on the computer. CONFIRMED (section 5).
3. **Ownership is one comparison.** The host compares a 64-bit library ID stored in the phone's `iTunesPrefs` with the ID of its own music library. Nothing from lockdown, from the sync protocol or from the phone's database takes part. CONFIRMED (section 6).
4. **A phone with no prefs is adopted without a question and synced at once.** Observed: Apple Devices wrote the prefs pair and started a session within seconds of the USB attach, with no click (section 8).
5. **Apple's first session is a Reset.** It carries `update_db_info` with newly minted IDs and one hidden, empty library playlist, at anchor 2. Music sync is off for a newly adopted phone, so there is no track. Observed up to the staged operation file (section 8).
6. **Apple's host can empty a phone that another program filled.** For such a phone the host holds an empty copy of the device library, and a Reset built from an empty copy makes the phone remove every host-synced track. With no prefs on the phone this happens at attach. With prefs that name another library it needs the owner to press Sync, and no "switch library" question comes first. Read in code twice, independently. Not observed (section 9).
7. **The phone accepts a first session from any host.** It reports the anchor `"0"`, takes a Delta or a Reset, creates its `db_info` row from the host's `update_db_info`, and creates the music folders itself. The host must create the staging folder (section 10).

## 4. A phone no computer has synced

No such phone was read. The table is what the code of iOS 27.2 says about it.

| Item | State | Label |
| --- | --- | --- |
| `iTunes_Control/iTunes/MediaLibrary.sqlitedb` | Present once anything has opened the library. A new database: all tables, no `db_info` row, no sync property, no playlist (chapter 04, section 2.5). | CONFIRMED contents; INFERRED that it exists before a host connects |
| Media anchor the phone reports | `"0"` (chapter 02, section 7.3) | CONFIRMED |
| `iTunes_Control`, `iTunes_Control/iTunes` | Created by `medialibraryd` with the database | CONFIRMED |
| `iTunes_Control/Music` and `Music/Fnn` | Absent until the first track arrives. The phone creates them when it moves that track (chapter 02, section 12.3). | CONFIRMED code; INFERRED absence |
| `iTunes_Control/Sync`, `Sync/Media` | Absent. No code on the phone creates them. | CONFIRMED |
| `Airlock` | Absent outside a session (chapter 02, section 12.3) | CONFIRMED code |
| `iTunesPrefs`, `iTunesPrefs.plist`, `iTunesCDB`, `iTunes_Control/Artwork/ArtworkDB` | Absent. Only a host writes them. | CONFIRMED that no phone code writes them |
| `iTunes_Control/iTunes/Artwork/Originals` and `Caches` | The phone's own artwork store. Absent or empty. | INFERRED |
| `Music/Downloads` on the AFC root | Probably present. The phone creates it after validating the database (`0x1d1aa80ac`). | INFERRED |
| Lockdown `com.apple.mobile.iTunes / iTunesSetupComplete` | Absent or false. Nothing on the phone reads it (chapter 03, section 3.2). | INFERRED value |
| Record of hosts in the `atc` defaults | None (chapter 09, section 6.4) | CONFIRMED code |
| Startup messages | Built as on any phone. `AssetMetrics` may come after `SyncAllowed` (chapter 02, sections 4.3 and 4.4). | CONFIRMED code |

Conditions that hold for every phone and matter here (chapter 03, section 3.2): the owner has finished Setup Assistant, the phone is activated, and it has been unlocked since it started.

Two other states are easy to mistake for this one:

- **A phone restored by Apple's host.** The restore routine saves the default prefs image with `configured` set and both library IDs zero (`0x1404eb2fc-0x1404eb321`). INFERRED. Such a phone has a prefs pair, but one that names no library.
- **A phone whose library the phone rebuilt** (chapter 09, section 4.4). Its database is new, but the host's four files are still there and describe the old library.

OPEN: what a real phone of this kind lists under `iTunes_Control`, and whether it already holds playlists of its own, such as built-in smart playlists.

## 5. The host's files on the phone

CONFIRMED. Only `AMPDevicesAgent.exe` contains the names of these files. `AirTrafficHost.dll`, `MobileDevice.dll`, `AppleMobileDeviceProcess_main.dll` and `AMP.Services.dll` do not. No code on the phone reads or writes them (chapter 04, section 2.1). They are the host's own per-device store, kept on the device so that any Apple host that connects finds the state the last one left.

| File | Holds | Written |
| --- | --- | --- |
| `iTunes_Control/iTunes/iTunesPrefs` | The host's sync settings for this phone and the identity of the library that adopted it | When a phone is adopted, when the owner applies sync settings, and at every attach of a configured phone |
| `iTunes_Control/iTunes/iTunesPrefs.plist` | The same settings, plus the host's lists of synced items | With `iTunesPrefs` |
| `iTunes_Control/iTunes/iTunesCDB` | The host's copy of the device library, its last sync anchors and its pending operations | After a sync, and at detach |
| `iTunes_Control/Artwork/ArtworkDB` | The host's artwork records and the counter for artwork cache IDs | The first time the host handles artwork for the phone, then with `iTunesCDB` |

The host writes each file to a temporary name in the same folder and renames it over the target. It does not read the old contents first and does not compare.

### 5.1 `iTunesPrefs`

CONFIRMED. The file is the memory image of one packed little-endian structure of 1,232 bytes (`0x4d0`). It has no header of its own. Reader `0x1404c9788`, serialiser `0x1404c99f8`, default initialiser `0x1404c9160`.

| Offset | Width | Field | Notes |
| --- | --- | --- | --- |
| `0x00` | 4 | magic `frpd` | Constant `0x64707266`. Other, shorter `frpd` files exist for photo settings. |
| `0x04` | 2 | major version, 1 | The reader accepts 1 or lower. A higher value is treated as no file. |
| `0x06` | 2 | minor version, 25 | Older minors are upgraded when read. |
| `0x08` | 1 | `configured` | 0 in the default image. The host saves the file only when it is not 0. |
| `0x09` | 1 | `launchApplication` | Name INFERRED |
| `0x0a` | 1 | `automaticUpdateMode` | 0 is manual. The master switch for automatic content sync (section 7.1). |
| `0x0b` | 1 | `musicUpdateMode` | Default 1. Name INFERRED |
| `0x0c` | 8 | home music library persistent ID | The ownership field (section 6.2). Written only by `SetHomeMusicLibrary` and the "universal" branch. |
| `0x14` | 8 | the host's `persistentUserID` | Written with the field above. Stored, never compared. |
| `0x21` | 1 | `hidePlaylists` | Bit 0 recomputed from the host's library at every save. Name INFERRED |
| `0x23` | 1 | `haveCleanedOrphanedFiles` | No effect on iOS. Name INFERRED |
| `0x34`, `0x5a`, `0x98`, `0x9c`, `0xa8`, `0x11c`, `0x11e` | 1 each | sync photos, podcasts, movies, music, TV shows, audiobooks, books | CONFIRMED from the setup log lines "prefs are configured to ... sync" |
| `0x60` | 8 | home podcast library ID | Width INFERRED: the host copies `0x0c` into it when it is zero. |
| `0x9d` | 1 | `syncMusicVideosWithMusic` | No effect while `0x9c` is 0 |
| `0xae` | 1 | `dontSyncWhenConnected` | 1 in the default image. Stops the automatic sync at attach (section 7.2). Written only from the settings window. |
| `0x140` | 64 | name of the home music library folder | |
| `0x180` | 64 | user name on the host | |
| `0x1c0` | 64 | computer name | |

The remaining bytes are other settings. The default image is zero except for the magic, the version and about twenty single bytes, among them `0x0b` and `0xae`. `configured` and both IDs are zero in it. The structure holds no time stamp, counter or sync number: nothing in it changes from one sync to the next.

How the host reads it (`0x1404c9788`):

1. It starts from the default image.
2. It reads up to 1,232 bytes of `iTunesPrefs`. A missing file, a wrong magic or a major version above 1 leaves the defaults. A short file leaves the default tail. Nothing is logged and nobody is asked.
3. It reads `iTunesPrefs.plist`, or `iTunesPrefs.plist.tmp` when that fails. If its embedded structure is valid and its minor version is not newer than the raw file's, the embedded copy replaces the raw one. Either file alone is enough.
4. If `configured` is 0 the phone is unconfigured (section 6.3).

When it writes: the save routine returns early for an unconfigured phone ("not saving prefs files because prefs are not configured", `0x140490efe`). For a configured phone the setup step asks for a save at every attach (`0x1404ad152`), so both files are rewritten each time the phone is connected, even when nothing changed. The identity fields `0x0c`, `0x14` and `0x60` are not touched by that rewrite. An ordinary Music sync does not write the prefs.

### 5.2 `iTunesPrefs.plist`

CONFIRMED (builder `0x140490af4`). An XML property list with:

| Key | Value |
| --- | --- |
| `DevicesVersion` | The string `"1.6.4"` |
| `iPodPrefs` | Data: the same 1,232 bytes as `iTunesPrefs` |
| `MusicTrackIDs`, `MusicPlaylistIDs`, `MusicArtistIDs`, `MusicAlbumIDs`, `MusicGenreIDs`, and the like for podcasts and audiobooks | Arrays of 64-bit integers, always written |
| The same for movies, TV shows and books | Only for devices that support the type |

The arrays are the host's lists of what it syncs to this phone. A missing key reads as an empty list, and an unreadable file is not an error.

For a Wi-Fi capable device the host also checks this file when the same device registers again. If the file is then missing it logs "iTunesPrefs.plist not found, assuming device was erased" (`0x1404afc58`), forgets the device and sets it up again. That is the host treating a missing prefs file as an expected state.

### 5.3 `iTunesCDB`

CONFIRMED. Writer `0x14047b060`, reader `0x140483850`.

- **Layout.** An `mhbd` root header of 244 bytes with minor version 117, then `mhsd` datasets. In the form written for an iPhone (major version 2) everything after the header is one zlib stream. The reader skips dataset types it does not know.
- **The device library.** Track, album, artist and playlist datasets. This is the host's copy of what is on the phone. The host never reads `MediaLibrary.sqlitedb`. It has no other source for the phone's contents.
- **The library ID.** Root offset `+0x48` is the persistent ID of the device library. It is the `pid` the host sends in `update_db_info` (chapter 02, section 8.4).
- **Dataset 11: the anchors.** Four 32-bit numbers: the host's last sync anchor for Media, Ringtone, VoiceMemo and Book (writer `0x140475a64`, reader `0x14048142c`).
- **Dataset 12: pending operations.** The operations the host has queued and not yet sent, one array per dataclass (writer `0x140475e54`).

When it is read: at every attach, by the setup step (`0x1404ac15c`). The copy of the device library and the four anchors in the host's memory are rebuilt from it each time.

When it is missing: the host looks for an `iTunesDB`. With neither file it takes the new-library branch (`0x1404acb06`). That branch makes an empty copy with one library playlist named after the device, and the four anchors keep their initial value 1 (`0x14046ade0`). Nothing is logged as an error. A missing file is an ordinary state.

When it is damaged: the host logs "failed to read database for device", shows a message and attaches no library to the device (`0x1404ac35b`, `0x1404ac458`). A broken `iTunesCDB` is worse for Apple's host than none.

When it is written: after a sync ends, after "done updating device", and when the phone is detached (`0x1405711e0`, `0x140499650`, `0x1404925e0`), and only for a configured phone whose copy changed. It is not written at attach. On a phone Apple's host has just adopted, the file first appears when the first sync has finished.

Two things follow. The anchor Apple's host sends is not kept on the computer, and a phone that lacks the file meets a host whose last anchor is 1 (chapter 02, section 6.3). And a host whose copy is empty will, in a Reset, send an empty library (section 9).

### 5.4 `ArtworkDB`

CONFIRMED. Loader `0x1404a0928`, writer `0x1404da140`.

- **Layout.** An `mhfd` root header of 132 bytes and three sections: images, albums and file formats. The root holds `nextRecordID` at `+0x1c`. A new library starts it at 100 (`0x1404dac87`). The host takes artwork cache IDs from this counter (chapter 04).
- **Creation.** The file is created the first time the host handles artwork for a phone that has none: an empty file of 696 bytes with the counter at 100 (INFERRED size, from the writer). The host creates the folder `iTunes_Control/Artwork` at the same moment. A phone whose syncs never carried a cover has no `ArtworkDB`.
- **Damage.** A file that cannot be read is replaced by a new empty one when the host next adds artwork.

The phone does not use `iTunes_Control/Artwork`. It keeps the covers it imports under `iTunes_Control/iTunes/Artwork/Originals`.

### 5.5 Folders and other files the host creates

CONFIRMED (folder resolver `0x1404c7ec8`, get-or-create walker `0x1404c8378`).

- **At every attach** the host creates `iTunes_Control`, `iTunes_Control/iTunes` and `iTunes_Control/Music` when they are missing (`0x14048e16f`, `0x14048e19c`, `0x14048e1eb`). For a phone it does not create `F00` to `F49` then.
- **When it writes the first operation file of a session** it creates `iTunes_Control/Sync` and `Sync/Media` when they are missing (`0x140469265`).
- **When it stages an asset** it creates `Airlock` and `Airlock/<dataclass>` when they are missing (`0x14046e6d5`, `0x14046e786`), and a music folder `Fnn` chosen at random among 50 (chapter 02, section 12.5).
- **At sync start** it creates the lock files of chapter 03, section 7.9. In the capture of section 8 it also removed and recreated a file named `iTunesLock`.

Every one of these is "stat, then create". The host does not rely on the phone having any folder.

## 6. Library ownership in Apple's host

### 6.1 The identities

CONFIRMED. Three numbers exist only on the computer:

| Value | Where | Use |
| --- | --- | --- |
| Persistent ID of the host's music library | The Apple Music library | Copied to prefs `0x0c` of a phone the host adopts |
| `persistentUserID` | Host preference, minted once when absent (`0x14048abe8`) | Copied to prefs `0x14` |
| `WirelessBuddyID` | Host preference | Wi-Fi sync (chapter 03, section 5.8) |

`HostInfo.LibraryID` is the first XOR the second, as 16 hex digits (`0x14048ac88`; chapter 02, section 7.2). The host computes it from its own state and never reads it from a phone. Apart from the pairing record, the host keeps nothing about a particular phone on the computer. Its record of a device is built in memory at attach from the phone's files.

### 6.2 The ownership test

CONFIRMED (`0x1404bc894`, comparison at `0x1404bc8e2`). At attach the host compares prefs `0x0c` of the phone with the persistent ID of its own current music library. Equal means "on the home music library". That is the whole test:

- `persistentUserID` is stored on the phone but not compared.
- No lockdown value takes part.
- Nothing from the phone's database or from an ATC message takes part. The test runs before the host opens the sync service.

One host preference changes it. When `universal-iPhone-library` is set, the host adopts any iPhone at attach without a question and overwrites prefs `0x0c` and `0x60` (`0x1404bc7b0`, `0x1404bc8d9`). Its default is 0 and no Apple binary that was read writes it.

### 6.3 Adoption

`SetHomeMusicLibrary` (`0x1404bc2cc`) is the one function that writes the two IDs into a phone's prefs. It has two forms. CONFIRMED structure:

| Form | What it does | Who calls it |
| --- | --- | --- |
| Adopt (second argument 0) | Writes the IDs and nothing else. | The setup step, for every unconfigured phone, with no dialog (`0x1404ab4d3`). A drag of items onto a phone in manual mode that is not on the home library (`0x1404a1c99`). OPEN: whether a confirmation is shown first. |
| Switch (second argument 1) | Writes the IDs, marks the music and the ordinary playlists in the host's copy of the device for deletion, and keeps the content options of the sync, so that the host's own music is sent. | The settings step, after the owner confirms the switch dialog (`0x1404be529`, `0x1404be5c3`). The sync itself when the "switch home music library" option is set (`0x1405707d7`). |

So a phone without prefs is claimed by the first Apple host it meets, silently. Section 8 shows this happening. Only a phone that already names another library gets the question.

### 6.4 What the switch question covers

CONFIRMED (request filter `0x140571744`, dialog helper `0x1403dd780`, call at `0x140571948`). Every sync request passes through a filter before it starts. The filter asks the "switch library" question only when all of these hold:

- the request holds a content option: music, movies, TV shows, audiobooks or books (mask `0x18e0`, test at `0x140571838`);
- the phone is not on the home library;
- the request may show a window, and the owner has not ticked "do not ask again" (prefs `0x4f`).

"Yes" sets the switch option. "No", or a request that may not show a window, removes the content options. In both cases the sync then starts (`0x140571c82`). No path through the filter cancels a sync.

The question therefore guards one thing: the host's library being sent to a phone of another library. It does not guard the session, and it is not asked for a request that holds no content option. Section 9 shows why that matters.

### 6.5 Lockdown values the host writes

CONFIRMED. None of these is read by the phone's sync code (chapter 03, section 3.2).

| Domain and key | Value | When |
| --- | --- | --- |
| `com.apple.mobile.iTunes` / `iTunesSetupComplete` | `true` | At every attach of a phone on which it is not already true (`0x1404ab1c2`), and at the end of `ConfigureDevice` (`0x1404a03e0`) |
| `com.apple.purplebuddy` / `SetupState` | `"SetupUsingiTunes"` | In `ConfigureDevice`, only while the phone is still in Setup Assistant (`0x1404a0399`) |
| `com.apple.mobile.iTunes.store` / `LibraryCUID`, `Storefront`, `AppleID`, and others | Account values | For a configured phone on the home library with a store account |
| The Wi-Fi sync keys | | When the owner turns "Sync over Wi-Fi" on (chapter 03, section 5.8) |

No path of a Music sync erases a device. `AMSEraseDevice` is called only from the restore thread (`0x1404e7dbc`).

## 7. When Apple's host syncs, and what it asks for

### 7.1 Sync options

Every sync request is a set of option bits. The names come from the host's own table (`0x1409e0630`), which it uses to log "requested sync options" and "effective sync options". CONFIRMED.

| Bit | Name | Bit | Name |
| --- | --- | --- | --- |
| 0 | AllowUI | 13 | SyncPhotos |
| 1 | IsUserInvoked | 19 | ReverseSyncPurchases |
| 2 | IsDeviceInvoked | 28 | UpdatePlayCounts |
| 5 | UpdateMusic | 29 | CopyOnTheGoPlaylists |
| 6 | UpdateMovies | 30 | CopyVoiceMemos |
| 7 | UpdateTVShows | 31, 32, 35 | rentals |
| 8 | UpdatePodcasts | 42 | UpdateSpeechClips |
| 11 | UpdateAudiobooks | 44 | SwitchHomeMusicLibrary |
| 12 | UpdateBooks | 24, 25 | backup |

The default options of a phone (`0x140492a58`) are what a plain Sync uses:

- The five content options (bits 5, 6, 7, 11, 12) only when prefs `0x0a` is set and the phone is on the home library (`0x140492aa0`).
- **UpdatePlayCounts whenever a device flag is set** (`0x140492cb6`). The attach path for iPhones passes the constant 1 for that flag (`0x140558930`). The test has no "home library" term. INFERRED: the flag is set for every iPhone.
- CopyOnTheGoPlaylists for a configured phone under a similar flag.

So the default options of an iPhone always hold UpdatePlayCounts, whichever library the phone belongs to and whether or not any content sync is on.

### 7.2 What starts a sync

Starts the host makes on its own all pass through one decision routine (`0x140492da8`). It checks reasons in a fixed order and logs the one that decides. CONFIRMED. The ones that matter here, in order:

| Reason (log text) | Result |
| --- | --- |
| "device is not configured" | no sync |
| "not currently allowed by AirTraffic": no `SyncAllowed` has arrived yet | no sync; the routine runs again when `SyncAllowed` arrives |
| "device was set up as new" | sync |
| "device preference to sync when connected is disabled": prefs `0xae` is not 0 | no sync |
| a content type is on, automatic mode is on and the phone is on the home library | sync |
| play counts, for a phone on the home library | sync |
| "no sync settings are enabled for current library" | no sync |

Its callers are the end of the setup step (`0x1404ad3dc`), `ConfigureDevice` (`0x1404a03f6`) and the handler of the phone's `SyncAllowed` (`0x14045f97f`).

What this means for a configured phone that is not on this host's library: connecting it starts no sync. With prefs `0xae` at 1, the default, the routine stops there. INFERRED: with `0xae` at 0 it would stop at "no sync settings are enabled", because every later reason needs the home library.

Connecting such a phone is still not free of writes. At every attach of a configured phone the host sets `iTunesSetupComplete`, rewrites the prefs pair (section 5.1) and creates the three folders of section 5.5 when they are missing.

Starts that do not pass through the decision routine:

| Start | Entry | Options |
| --- | --- | --- |
| The owner presses Sync | `0x14048f030` | The default options, plus AllowUI and IsUserInvoked. INFERRED that this entry is the Sync button. |
| The owner applies sync settings | `SetIPodPrefs`, `0x1404c0242` | The changed content options. When the change touches media it adds bits 28 and 29. |
| The owner drags items onto the phone | `0x1404a382c` | |
| Back up, restore | | Backup only opens no sync session |
| The phone sends `RequestingSync` | `0x140460450` | The default options |
| The lockdown notification `com.apple.itunes-client.syncResumeRequest` | `0x140497f2e` | The default options |

The last two come from the phone. The phone's code sends `RequestingSync` only when another process on the phone asks for it (chapter 02, section 7.4), and the notification name occurs in none of the phone's files that were read. OPEN: what posts it.

### 7.3 Which dataclasses the session names

CONFIRMED (`SynchronizeDevice`, `0x14056fdb0`, masks at `0x140570128-0x140570258`).

- The host opens a sync session when the options hold any of bits 5 to 8, 11 to 13, 15, 19, 28 to 32 or 35 (mask `0x9f008b9e0`).
- **Media, VoiceMemo, Book and Keybag are requested together** (mask `0x2d`) when the options hold any of bits 5 to 8, 11, 12, 28 to 32 or 35 (mask `0x9f00019e0`).
- Photo is added for the photo options. Keybag is dropped for a device without that capability.
- ReverseSyncPurchases alone adds Media and Book as a second, weaker mask.

UpdatePlayCounts is bit 28. With section 7.1 this gives the central fact of this chapter: **every default sync of an iPhone requests the Media dataclass**, even when every content sync is off and the phone belongs to another library. Observed in the capture of section 8: the request named Media although the host's own `SyncedDataclasses` was empty.

A session leaves Media out only when the options hold none of those bits, for example a backup or a photos-only sync.

### 7.4 Reset or Delta, and what a Reset holds

CONFIRMED.

- **The choice** (`0x14046d034`). For a dataclass in the main mask, the host compares its last anchor with the anchor the phone reported in `ReadyForSync`. Different means Reset, equal means Delta. A dataclass that is only in the weaker mask gets Delta whatever the anchors are. The last anchor comes from dataset 11 of `iTunesCDB`, or is 1 (section 5.3).
- **The contents** (`0x140468364`). A Reset sends the host's copy of the device library: an `insert_track` for each of its tracks and an `insert_playlist` for each of its playlists, with `update_db_info` first. The builder reads no ownership flag and no music setting, and it does not stop at an empty copy.
- **What fills the copy.** The copy comes from `iTunesCDB` at attach. The host's own music is added to it only by the track stage of the sync, which runs only when a content option is set (`0x140570c28`). In that stage an empty host library raises the error "not syncing because library is empty", which `SynchronizeDevice` treats as success (`0x140570c72`).
- **The file.** The operation file is written by a separate writer (`0x140468fbc`) near the end of the sync. It runs only when a dataclass has something pending. During a Reset the queue is dropped except for `upload_track`. A pending Keybag is enough to start the writer, and the writer then builds the Media Reset, puts `update_db_info` first, writes the file and advances the anchor (`0x1404667cd`, `0x14046915d`, `0x140469275`).
- **The message.** `FinishedSyncingMetadata` is sent when the session is in the right state and no stage failed (`0x14046d864`). It does not look at the operation list. The sync type it carries is the one the selector chose.

OPEN: a device without the Keybag capability and with nothing queued gets no file, yet the message still says Reset. Chapter 02, section 14.5 gives what the phone's code would do with it.

## 8. Apple's first session with a phone it does not know

### 8.1 The capture

One capture of Apple Devices 1.1540.24088.0 was made to see this session. It is the only run-time evidence in this chapter about Apple's host.

- **The phone** had its music library in place. Before the run the prefs pair and `iTunesCDB` were moved to another folder over AFC, after a backup. To the host that is a phone with no prefs and no saved copy, which is all it can know about a phone before the session (sections 5 and 6). The phone's database was not changed, so the phone itself was not in the state of section 4.
- **The host** ran in a Windows virtual machine with no network.
- **The guard.** A Frida script inside `AMPDevicesAgent` recorded every outgoing ATC message and every AFC path operation. It ended the process before the host could send `FinishedSyncingMetadata` for Media, before any message after a `.cig` file was opened, and before any message it could not name. A rehearsal was ended at the first `HostInfo`.
- **Afterwards** the staged files and the prefs Apple had written were removed and the original files moved back. All 45 tables of the phone's database had the same content before and after, the anchor had not moved, and the host's files matched the backup. Only the phone's own play-count export and the write-ahead log differed.

The guard recorded what the host sent. It did not record what the phone answered.

### 8.2 What the host did

CONFIRMED (observed). There was no click. Within seconds of the USB attach the host ran its setup and started a session, in the rehearsal and in the real run. In order:

| Step | Operation |
| --- | --- |
| 1 | Opens the prefs pair and other settings files to read them. The prefs are absent. |
| 2 | Sends `HostInfo` twice, with Session 0. |
| 3 | Reads `ArtworkDB` and a Books purchase list. Removes and recreates `iTunesLock`. |
| 4 | Sends `Ping`. |
| 5 | Writes `iTunesPrefs.tmp` and renames it to `iTunesPrefs`. Does the same for `iTunesPrefs.plist`. |
| 6 | Sends `SyncStatus` messages with the texts of its progress window. |
| 7 | Sends `RequestingSync`, with Session 1. |
| 8 | Asks about backup encryption in its window. The backup was skipped. |
| 9 | Reads `PlayCounts.plist` and removes it. |
| 10 | Reads `AirFair/sync/afsync.rq` and its signature. |
| 11 | Writes `iTunes_Control/Sync/Media/Sync_00000000.plist`, then its `.cig`. |
| 12 | Enters the function that sends `FinishedSyncingMetadata`. The guard ends the process here. |

The prefs are written at step 5, before the request, not at the end of the sync. They held `configured` = 1, the host's two IDs and default settings, with automatic sync off.

The messages:

```text
Command: HostInfo            # Session 0, sent twice
Params:
  LocalCloudSupport: false
  HostInfo:
    Type: iTunes
    Version: 13.2.4.123
    WindowsOSVersion: "10.0"
    SyncHostName: <computer name>
    LibraryID: <LibraryID>
    SyncedDataclasses: []
    SyncedAssetTypes: []
    Wakeable: 0

Command: RequestingSync      # Session 1
Params:
  Dataclasses: [Media, Book, Keybag]
  DataclassAnchors: {Media: "1", VoiceMemo: "1"}
  HostInfo: <as above, plus Grappa: data, 80 bytes>
```

The arguments of the `FinishedSyncingMetadata` that was never sent:

```text
SyncTypes: {Media: 2, Book: 0, Keybag: 1}
DataclassAnchors: {Media: "2", VoiceMemo: "1"}
```

The operation file, staged with a `.cig`:

```text
revision: 2
timestamp: <date>
operations:
  - operation: update_db_info
    pid: <new database PID>
    db_info: {primary_container_pid: <new playlist PID>}
  - operation: insert_playlist
    pid: <the same new playlist PID>
    container: {is_hidden: true, name: <device name>, date_created: <date>, date_modified: <date>}
    container_ui: {is_reversed: false, album_field_order: 0, shuffle_items: false,
                   play_order: 1, repeat_mode: 0, has_been_shuffled: false}
    item_to_container: []
```

Notes on what was seen:

- The database PID was new. It was not the `db_pid` of the phone's database.
- Media was requested although `SyncedDataclasses` and `SyncedAssetTypes` were empty.
- The Grappa value was 80 bytes. The reference sessions of chapter 02, made with build 1.6.4.90, carried 84.
- `SyncStatus` is a host message with one parameter, `StatusMessage`. The texts were "Starting sync" and "(Step 1 of 4) Waiting for sync to start", then "(Step 2 of 4) Backing up".
- The host sent no `Capabilities` message.

### 8.3 How the code accounts for it

CONFIRMED in code. That these paths are the ones that ran is INFERRED.

1. **Adoption.** The setup step finds `configured` at 0 and calls `SetHomeMusicLibrary` in its adopt form, with no dialog (`0x1404ab4d3`).
2. **Setup completes itself.** The interface layer calls `CompleteSetup` with no input from the owner (`AMP.Services.dll` `0x180174930`). That reaches `ConfigureDevice` (`0x14049fd24`), which marks the phone "set up as new" (`0x1404a03e5`) and saves the prefs. OPEN: what starts that part of the interface.
3. **The sync starts.** The decision routine answers "device was set up as new" (section 7.2) and the sync begins with the default options.
4. **Media is in it** because of UpdatePlayCounts (section 7.3). That is also why the host read and removed `PlayCounts.plist`.
5. **Reset at anchor 2.** With no `iTunesCDB` the host's last anchor is 1. The phone reported another number, so the selector chose Reset, and the next anchor is 2. A phone no computer has synced reports `"0"`, which also differs from 1.
6. **The file's contents** are the host's copy of the device library, which the new-library branch had just made: one library playlist named after the device, no track. The host minted the database PID because it found no `iTunesCDB` to take one from (section 5.3).

"Syncs when connected" is thus a property of unconfigured phones. A configured phone gets the decision of section 7.2 instead.

### 8.4 What this session does

To a phone no computer has synced, by the phone's code (chapter 05, section 5), INFERRED:

- The phone creates its `db_info` row with the host's database PID and the playlist PID.
- It creates the library playlist, hidden and empty. It is hidden because the host's dictionary says so. The phone forces a playlist hidden only when its PID equals the `primary_container_pid` already stored when the import starts, and at that moment there is none.
- It stores the anchor `"2"`.
- A Reset removes host-synced items the host did not send again. There are none, so nothing is removed.
- The session needs no asset, so the phone sends no manifest (chapter 02, section 12.2).

Afterwards the host writes `iTunesCDB` for the first time, when the sync has finished. `ArtworkDB` follows when a sync first carries a cover.

To the phone of the capture the same file would have done something else: it would have removed every host-synced track. That is why the guard existed, and it is the subject of the next section.

## 9. A phone filled by another program, then connected to Apple's host

Take a phone whose music was synced by a program that is not Apple's host. Its tracks are host-synced tracks like any others: they carry a `sync_id`. What Apple's host makes of the phone depends only on the host files it finds.

| Host files on the phone | What Apple's host takes it for | When it is connected | When the owner presses Sync |
| --- | --- | --- | --- |
| None | An unconfigured phone | Adopts it and runs the session of section 8: a Reset that holds one empty playlist. The phone removes every host-synced track. No click is needed. | |
| A prefs pair that names another library, no `iTunesCDB` | A configured phone of another library, contents unknown | No sync. Rewrites the prefs pair without changing the IDs. | A Reset that holds one empty playlist, with no question first. The phone removes every host-synced track. |
| A prefs pair that names another library, and an `iTunesCDB` whose Media anchor equals the phone's | A configured phone of another library, contents known | No sync. | A Delta. INFERRED from the selector; not examined further. |
| The files Apple's own host left, with an older anchor | Its own phone, changed behind its back | A sync, if automatic sync is on. | A Reset that sends the host's copy of the device, brought up to date from the host's library when music sync is on. Host-synced tracks that are not in it are removed. This is the reset reference session of chapter 02. |

The first row is the capture of section 8, observed up to the operation file. The phone's part, the removal, was read in the phone's code (chapter 05, section 5) and not observed.

The second row was read in code only, by two independent readings of the agent that agree. The chain:

1. Pressing Sync uses the default options. For a phone of another library with content sync off these hold no content option (section 7.1).
2. With no content option the filter does not ask the switch question (section 6.4).
3. The default options hold UpdatePlayCounts, so the session requests Media (section 7.3).
4. With no `iTunesCDB` the host's last anchor is 1 and its copy of the device library is one empty playlist (section 5.3). The phone reports its own anchor, which is not 1. The selector chooses Reset (section 7.4).
5. The track stage does not run, because no content option is set. Nothing is added to the copy.
6. The Reset file holds `update_db_info` with a new database PID, and the empty playlist.
7. The phone treats sync type 2 as "the host sends everything it owns again" and removes every host-synced track, playlist, album and album artist that was not sent again (chapter 02, section 10.5). It also replaces its `db_info` row with the host's new PID.

Conditions the chain depends on:

- **The phone's anchor is not exactly 1.** At exactly 1 the selector chooses Delta.
- **The Keybag capability.** It is what starts the writer when nothing else is pending (section 7.4). The capture requested Keybag for this model.
- **No `iTunesDB` with content on the phone.** The host reads that file when `iTunesCDB` is missing.
- **The Apple Devices window adds no check of its own** before it calls the agent. OPEN: that code was not read.
- **Finder and iTunes behave the same.** OPEN: neither was read.

What stays on the phone in either row: items no computer synced, such as purchases and downloads from Apple Music. They carry no `sync_id` and a Reset does not select them.

What the owner sees when they turn music sync on instead: the switch question. "Yes" is the documented "Remove and Sync": the host marks the phone's music for deletion and sends its own library. "No" removes the content options and syncs anyway, with UpdatePlayCounts still set, which is the chain above.

What the code says is harmless for a phone that carries a prefs pair naming another library: connecting it, charging it, pairing, and a backup. None of them opens a Media session. Pressing Sync is not harmless. None of this was observed.

## 10. What the phone allows a first host

Read in the code of iOS 27.2. None of it was exercised. The paragraphs point to where each fact is set out.

- **The service starts for any paired host.** Lockdown checks pairing, activation and unlock, and nothing about earlier syncs (chapter 03, section 3.2).
- **Any `LibraryID` is accepted.** The phone records it with the host's name and asset types. When a later host with another `LibraryID` claims the same asset types, the phone moves the claim to the new record. That is bookkeeping. No refusal, removal or question follows from it (chapter 09, section 6.4).
- **The phone reports `"0"`** as its Media anchor and accepts any anchor from the host (chapter 02, section 7.3).
- **Delta and Reset are both accepted.** No anchor is compared. On a library with no host-synced item a Reset removes nothing (chapter 02, sections 6.3 and 10.5).
- **`update_db_info` creates the `db_info` row** with the IDs the host sends. Without the operation the phone keeps no row (chapter 02, section 8.4).
- **The anchor of `FinishedSyncingMetadata` becomes the phone's anchor**, whatever it is (chapter 09, section 6.4).
- **The staging folder is the host's job.** The phone never creates `iTunes_Control/Sync/Media`. It does create `Airlock/Media` when the session begins and the music folders when it moves a file. It never creates `Airlock/Media/Artwork` (chapter 02, sections 8.2 and 12.3).
- **A session that needs no asset ends without a manifest** (chapter 02, section 12.2).
- **A session stopped before `FinishedSyncingMetadata` changes nothing in the database** (chapter 02, section 14.3). The phone stays a phone no computer has synced, apart from its record of the host.
- **An iCloud music library changes the picture.** When it is on, the phone lists `Music` among the asset types it takes from no computer and ignores a host's playlist operations (chapter 02, section 4.4; chapter 05, section 5).
- **Built-in smart playlists.** At the end of the first session that transfers an asset, the phone deletes the built-in smart playlists it created itself, once (chapter 05, section 5).
- **No host file is needed.** The phone reads none of the four files of section 5.

## 11. Guidance for a sync client

### 11.1 `rustunes` today

`rustunes` syncs a phone that Apple's host has synced at least once (chapter 07). It reads the library identity from the prefs pair and sends that same `LibraryID`. It requires `iTunesCDB`, and `ArtworkDB` when it sends covers, and it repeats the `db_info` IDs it finds. It requires the staging folder and takes the music folders from the phone's database. It writes none of the four host files and creates no folder under `iTunes_Control`.

On a phone no computer has synced it stops at the first missing file, with "stat prefs: object not found", before it sends anything. That is the safe failure. It is also a stricter stance than Apple's host takes: for Apple's host every one of those absences is an ordinary state with a default.

Because `rustunes` sends the identity it found and leaves the host files alone, a phone it has synced still looks to Apple's host like that host's own phone with an older anchor: the last row of the table in section 9. INFERRED from section 7.4 and from the reset reference session of chapter 02: a later sync with Apple's host resets the phone to what that host holds, and music that only `rustunes` put there is removed.

### 11.2 A client that wants to be the first host

The phone's code allows it (section 10). What follows is what a client would have to decide. None of it has been tried on a phone.

1. **Know that the phone is new.** The phone's own test for "host-synced" is `sync_id != 0`. A precise check is: no such row in `item_store`, `container`, `album` and `album_artist`, the anchor `"0"`, no `db_info` row, no prefs pair and no `iTunesCDB`.
2. **Mint an identity and keep it.** A `LibraryID`, a database PID and a library-playlist PID. The phone will store whatever is sent, so they must be stable from then on.
3. **Send `update_db_info` first.** Without it the phone has no `db_info` row, and every later session needs one.
4. **Choose the session type knowingly.** Apple sends a Reset at anchor 2 with a hidden library playlist. A Delta works as well. Never send a Reset to a phone that holds host-synced items the session does not carry.
5. **Create `Sync` and `Sync/Media`.** Do not require `Music/Fnn` to exist.
6. **Never send `FinishedSyncingMetadata` without the staged file**, and do not wait for a person while the session is open (chapter 02, sections 14.3 and 14.5).
7. **Look at `DisabledAssetTypes` first.** A phone that lists `Music` takes no music from a computer.
8. **Decide about the host files, and tell the owner.** This is the part with no good answer. With no prefs on the phone, Apple's host adopts it at the next connection and empties it without a click. With a prefs pair that names the client's own library, Apple's host leaves it alone at connection but empties it when the owner presses Sync. An `iTunesCDB` with the right anchor might turn that into a Delta, but the file is large, undocumented in parts, and worse than nothing when wrong (section 5.3). Whatever a client writes, the owner should be told plainly that syncing the phone with Apple's host afterwards can remove the music the client put there.

## 12. Open questions

| # | Question | What would settle it |
| --- | --- | --- |
| 1 | What does a phone no computer has synced really hold: which folders, which playlists, and is the database there before a host connects? | A read-only listing and a database copy from such a phone |
| 2 | Does the phone behave as section 10 says in a first session? | One session on such a phone |
| 3 | Does Apple's host really empty a phone of another library when the owner presses Sync (section 9, second row)? | A guarded capture like that of section 8 with a prefs pair in place, or a run on a phone that can be lost |
| 4 | Does the Apple Devices window check anything before it asks the agent to sync? | Reading the interface layer |
| 5 | Do Finder and iTunes for Windows take the same decisions? | Reading them |
| 6 | Would an `iTunesCDB` that holds the phone's anchor make Apple's host choose Delta, and what would the host then do with its copy? | Reading the Delta path for a phone of another library; an experiment |
| 7 | What starts the part of the interface that completes setup with no input (section 8.3)? | Reading `AMP.Services.dll` further |
| 8 | What does the phone do with a Reset that comes with no operation file? | INFERRED in chapter 02, section 14.5; an experiment |
| 9 | Which process on the phone asks `atc` to send `RequestingSync`, and what posts `syncResumeRequest`? | Reading the callers outside the files that were read |
| 10 | Does a track that a host synced and that also has another source survive a Reset? | Reading the removal routine (`0x1d1b121c4`) |
| 11 | Does Apple's host send a `Capabilities` message with its `LibraryID` at any point? None was seen before the guard ended the capture. | A capture that runs further |
| 12 | Which playlists are the "built-in smart playlists" the phone deletes after its first asset transfer? | A database copy before and after a first session |

## 13. Evidence

### 13.1 Static analysis of the Windows host

Apple Devices 1.1540.24088.0, read with `objdump`, string extraction and scripts that follow direct calls. No code was executed for these readings.

| Binary | Version | SHA-256 |
| --- | --- | --- |
| `AMPDevicesAgent.exe` | 1.6.4.94 | `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9` |
| `AirTrafficHost.dll` | 4026.110.1.6 | `230445f73c4887ff46adfb7a9a25ed83fb99b6abc9bc341fcb8349bad3a7664b` |
| `MobileDevice.dll` | 1860.2.5.1 | `929e4c3582eede3cde35eabb31ad8211defcd425448fd75b5353073d6bd46812` |
| `AMP.Services.dll` | | `de043b7e985286648fb7b28e306db2cef954a2eecd36e4476a990b2f0f096658` |

Subjects: the prefs reader, writer and default image; the `iTunesCDB` and `ArtworkDB` readers and writers; the folder resolver; the lockdown wrappers and every key they are called with; the setup step; the ownership test and its callers; the sync options, the decision routine, the request filter, `SynchronizeDevice`, the selector, the Reset builder and the operation-file writer.

The Media path of sections 7 and 9 was read twice. The second reading was made from its own disassembly, without the first reading's notes, and was asked to refute it. It confirmed the chain and corrected three details, which are the ones given here: the operation file is written by the end-of-sync writer and not by the track stage, the writer starts because of Keybag, and the "library is empty" error is not on this path.

This reading corrected four statements in other chapters, which now say so in place: the host keeps its anchors in the phone's `iTunesCDB` and not on the computer (chapters 04 and 09); the two anchors of the selector were swapped in chapter 08, section 10; a Reset keeps only the pending `upload_track` operations (chapter 02, section 6.3); and package 1.1540.24088.0 does ship `MobileDevice.dll` (chapters 01 and 03).

### 13.2 Static analysis of the phone's firmware

iOS 27.2 (24B119) for iPhone17,2, obtained and read as in chapter 09, section 13.1, with `usr/libexec/lockdownd` (SHA-256 `1aee64771b85cb3c5be083c3862f9039dd0c0149f9ed224209176531d20b4b22`) in addition. Subjects: the service rules of `lockdownd`; the startup messages, `RequestingSync`, `FinishedSyncingMetadata`, the host record, the idle timeout and the host wake-up in AirTrafficDevice; the Airlock in AirTraffic; prepare, reconcile and the asset bookkeeping in the Music sync bundle; the import, Reset, `update_db_info`, database creation and validation in MusicLibrary.

The search for the host's file names covered every file of the shared cache as raw bytes, in ASCII and UTF-16, and about 3,500 executables of the system volume.

The cache images were read without their shared stub islands, which are not in the extracted files. Call targets were resolved by hand from selector tables. Where a statement depends on that, the chapter that makes it says INFERRED.

### 13.3 The guarded capture

One capture of Apple Devices 1.1540.24088.0 in a Windows virtual machine without network, with the phone attached over USB (section 8.1). A Frida guard (Frida 17.22.2) in `AMPDevicesAgent` recorded the outgoing messages at the link writer of `AirTrafficHost.dll`, the arguments of the host-info, sync-request and metadata-finished exports, and every AFC path operation of `MobileDevice.dll`. It ended the process on the conditions of section 8.1. Every file under `iTunes_Control/iTunes`, `ArtworkDB` and the staged Airlock files were copied twice before the run and compared after it.

### 13.4 Observations on the phone

- Two passive probes on iOS 27.2, one with the Apple TV app installed and one with it deleted, gave the two `DisabledAssetTypes` lists of chapter 02, section 4.4.
- One insert and one delete of a generated test track with `rustunes`, with the Apple TV app installed, each ended in `SyncFinished`. The saved frames gave the session numbers of chapter 02, section 5.2.

### 13.5 Strength of the evidence

- What Apple's host writes, reads and compares is read in code and, for the first session, observed up to the staged file.
- What Apple's host does to a phone of another library (section 9, second row) is read in code only, twice.
- What the phone does with any of it is read in code only. No session described in this chapter was allowed to reach the phone's import.
- Nothing here was checked against Finder or iTunes.
