# The ATC Music sync protocol (`com.apple.atc`)

This chapter describes the wire protocol that Apple's AirTraffic (ATC) service speaks between a host computer and an iPhone while the host adds, updates and removes tracks and playlists in the stock Music library. It covers message framing, the unsolicited startup messages, the Media session lifecycle, the operation file and its signature sidecar, the asset exchange, completion semantics and error behaviour. Everything here was recovered by observing a real phone, by tracing Apple's Windows "Apple Devices" application while it synced, and by static analysis of that application's binaries. A Linux client built from these findings has inserted, updated and deleted tracks and playlists on the phone, so the sequences below are known to work end to end. One part is not understood: the algorithm Apple's host uses to sign operation files. The chapter states exactly what was recovered instead and what the Linux client uses.

## Contents

1. Scope and provenance
2. Conventions and evidence labels
3. Framing and envelope
4. Startup messages
5. Session numbering, Ping and Pong
6. Media session lifecycle
7. The RequestingSync message
8. The operation file
9. Track operations
10. Playlist and container operations
11. Signing
12. Metadata finished, manifest and asset exchange
13. Completion, progress and teardown
14. Errors, interruption and recovery
15. Native session templates
16. Differences over Wi-Fi
17. Notes for implementers
18. Open questions and facts not confirmed
19. Evidence

## 1. Scope and provenance

Device observed:

| Item | Value |
| --- | --- |
| Phone | iPhone17,2 |
| iOS | 27.0.1, build 24A446 |
| Library | stock Music app, 398 to 430 tracks over the investigation, ALAC and later MP3 and other formats |

Host binaries analysed (Windows, x86-64 PE32+). The binaries are not redistributed with this repository. Only addresses, constants and conclusions are published.

| Package | AMPDevicesAgent.exe | AirTrafficHost.dll | MobileDevice.dll |
| --- | --- | --- | --- |
| Apple Devices 1.1540.23042.0 | 1.6.4.90 (12,409,712 bytes, SHA-256 `ad957f32107b7177969b3cb11b91e3922d27e622b53f9240b65cb457522111f9`) | 4025.400.1.5 (322,000 bytes, SHA-256 `0d38b01c21d57a9f3ca32c15d4b2b8aa0557d43a724b4745f1fb122322f4eecd`) | 1827.100.17.2 (4,020,688 bytes, SHA-256 `de08e3324f24dbcbc0597b78f797670f18f53faa58819cda3c32fa3cebcb9d39`) |
| Apple Devices 1.1540.24088.0 | 1.6.4.94 (SHA-256 `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9`) | 4026.110.1.6 (SHA-256 `230445f73c4887ff46adfb7a9a25ed83fb99b6abc9bc341fcb8349bad3a7664b`) | not analysed |

Address conventions:

- Addresses in `AMPDevicesAgent.exe` (written "AMP" below) are relative virtual addresses (RVAs). The virtual address is `0x140000000` plus the RVA. Some analysis output prints the full virtual address (`0x1404xxxxx`). This chapter gives RVAs unless a full address is shown.
- Addresses in `AirTrafficHost.dll` are RVAs. The preferred image base is `0x180000000`.
- Unless a build is named, an address belongs to AMPDevicesAgent 1.6.4.90 or AirTrafficHost 4025.400.1.5. Build 1.6.4.94 shifted most AMP addresses by a small offset. For example the `ReadyForSync` handler moved from `0x45ee20` to `0x45ef44`. Section 11 lists the verified mapping. The signer function (`0x248b0`) and the incoming-message callback in the DLL (`0x307a0`) did not move.

Methods used, in order of strength:

1. Passive and active capture from Linux of raw `com.apple.atc` frames, plus the phone's syslog and read-only copies of its Music database.
2. Frida tracing of the running Windows host during real syncs, which recorded the host's outgoing message dictionaries, the staged operation file and sidecar bytes, and the asset manifest.
3. Offline static analysis of the binaries, including checked instruction-level data flow.

Limits of the Windows traces: the traced objects are the dictionaries passed to the host's send functions, not the framed bytes on the wire. Every trace was cut short by an event cap, so the tail of those sessions (final completion messages) was not recorded. The Linux observations of the same protocol are complete raw frames.

## 2. Conventions and evidence labels

- **CONFIRMED**: seen on the wire, in the phone's database or syslog, or in disassembled code and data flow.
- **INFERRED**: reasoned from names, strings or analogy.
- **OPEN**: not settled.

Where an earlier reading was later corrected, the corrected statement is used. Corrections that matter are called out in place.

"Reference session" means a sync run by Apple's own host on Windows; the two recorded ones are the reset reference session and the delta reference session (section 19.2). "Linux session" means a sync run by the Linux client against the same phone.

Plist notation: examples are shown as YAML-like text. Integers are plist integers, `<date>` is a plist date, `<data N bytes>` is a plist data value. Persistent IDs (PIDs) are signed 64-bit plist integers.

## 3. Framing and envelope

### 3.1 Frame

CONFIRMED (Linux capture of the phone's startup messages; Linux outgoing frames; static analysis of the host's link writer).

```text
+--------------------+-------------------------------+
| u32 length, LE     | binary plist (format 200), N  |
+--------------------+-------------------------------+
```

- The length is a little-endian unsigned 32-bit integer and counts only the payload.
- The payload is a binary property list (`bplist00`).
- No other framing exists. There is no magic number, checksum or type byte.
- The host's writer (`AirTrafficHost.dll` RVA `0x33310-0x334aa`) calls `CFPropertyListCreateData` with format 200 and appends the four length bytes with no byte swap. The length is little-endian because the host is x86-64.
- The ATC service is reached through lockdown (`StartService com.apple.atc`), over TLS when the pairing record requires it. This differs from the big-endian length framing of most lockdown plist services. Transport details are in [chapter 3](03-transport-usb-and-wifi.md).

Example: a Ping with Session 1 is a 73-byte plist, so the frame is 77 bytes.

### 3.2 Envelope

Every message is a dictionary.

| Key | Type | Direction | Example | Meaning | Evidence |
| --- | --- | --- | --- | --- | --- |
| `Command` | string | both | `SyncAllowed`, `RequestingSync` | Message name. | CONFIRMED |
| `Session` | integer (CFNumber type 9, signed 32-bit) | both | `0`, `1`, `2` | ATC session counter. See section 5. | CONFIRMED |
| `Params` | dictionary | both, optional | `{Dataclasses: [...]}` | Command arguments. Absent for Ping and Pong. | CONFIRMED |
| `Type` | integer | phone to host | `0` | Observed as 0 in every phone message. Meaning not established. | CONFIRMED value, OPEN meaning |
| `Id` | integer | phone to host | `1`..`4` at startup, `42` for one `ReadyForSync` | Phone-side message counter. Increments across the connection. | CONFIRMED |

The host's generic constructor (`ATCFMessageCreate`, DLL RVA `0x2f2e0-0x2f3b7`, 4025.400.1.5) builds `{Session, Command, [Params]}` and nothing else. Host messages carry no `Type` and no `Id`. CONFIRMED by disassembly and by Linux frames that the phone accepted.

### 3.3 Host command builders

The DLL exports a builder for each command. The mapping from builder arguments to wire keys is CONFIRMED from the builders' disassembly (4025.400.1.5). Square brackets mark optional keys.

| Builder (DLL RVA) | `Command` | `Params` |
| --- | --- | --- |
| `ATHostConnectionSendPing` (`0x31b30`) | `Ping` | none; uses the current session counter without incrementing it |
| `ATHostConnectionSendHostInfo` (`0x31970`) | `HostInfo` | `{HostInfo: dict, LocalCloudSupport: bool}`. Not seen in any native Media sync trace. |
| `ATHostConnectionSendSyncRequest` (`0x31dd0`) | `RequestingSync` | `{Dataclasses, DataclassAnchors, [HostInfo]}`. Increments the session counter first. |
| `ATHostConnectionSendMetadataSyncFinished` (`0x31a60`) | `FinishedSyncingMetadata` | `{SyncTypes, DataclassAnchors}` |
| `ATHostConnectionSendAssetCompleted` (`0x310e0`) | `FileComplete` | `{AssetID, Dataclass, [AssetPath]}` |
| `ATHostConnectionSendAssetCompletedWithMetadata` (`0x311e0`) | `FileComplete` | as above plus `Metadata` and `Anchor` together, only when a path and metadata are both present |
| progress builder (`0x31810`) | `FileProgress` | `AssetProgress` and `OverallProgress` only |
| error builder (`0x316d0`) | `FileError` | `AssetID`, `Dataclass`, `ErrorCode`, `ErrorIsVisible` |
| `ATHostConnectionSendPowerAssertion` (`0x31b80`) | `PowerAssertion` | `{Enabled: bool}` |
| `ATHostConnectionSendMessage` (`0x31a50`) | any | forwards a prepared message to the link writer (`0x33670`) |

ASCII literals for the envelope keys sit at DLL file offsets `0x395f0` (`Session`), `0x395f8` (`Command`) and `0x39600` (`Params`).

The `FileComplete` form with `Metadata` and `Anchor` exists in the host, but no captured Music sync used it. Apple's host calls it from AMP RVA `0x456e8f` only when a path and metadata are both present, and asserts `metadata == nullptr || newSyncAnchor != 0`. The Linux client never sends it. OPEN: what the phone does with `Metadata`.

## 4. Startup messages

CONFIRMED (passive Linux capture, repeated in later sessions).

When a client starts `com.apple.atc` and sends nothing, the phone sends four unsolicited frames:

| Order | `Command` | Payload bytes (first capture) | `Session` | `Id` |
| --- | --- | --- | --- | --- |
| 1 | `Capabilities` | 184 | 0 | 1 |
| 2 | `InstalledAssets` | 146 | 0 | 2 |
| 3 | `AssetMetrics` | 691 | 0 | 3 |
| 4 | `SyncAllowed` | 382 | 0 | 4 |

If the client first sends a Ping, the phone answers with a Pong and then sends the same four messages (plus a second `AssetMetrics`), all stamped with the session it chose. See section 5.

### 4.1 Capabilities

```text
Command: Capabilities
Params:
  GrappaSupportInfo: {version: 1, deviceType: 0, protocolVersion: 1}
```

CONFIRMED. The phone advertises support for the Grappa authentication layer. Grappa is Apple's private host-to-device session handshake. Its role in Media sync is described in section 11.

### 4.2 InstalledAssets

```text
Command: InstalledAssets
Params:
  Application: {_Deleted: []}
```

CONFIRMED. The message carries no Music track inventory. It was identical in every capture, including Windows reference sessions.

### 4.3 AssetMetrics

```text
Command: AssetMetrics
Params:
  Media:
    Music: {_Count: 398, _PhysicalSize: 8346488832}
```

CONFIRMED. These are aggregates only. No per-track entries exist in any capture.

Correlation observed: after one 17,452,907-byte track was added, `_Count` rose by one and `_PhysicalSize` by 17,453,056 bytes. That is the file size rounded up to a multiple of 4096. The metrics can therefore confirm that an insertion landed, but they cannot identify the track. The phone sends `AssetMetrics` again after the metadata phase of a sync. Apple's host validates and logs the counts after `SyncFinished`.

### 4.4 SyncAllowed

```text
Command: SyncAllowed
Params:
  ManualSync: true
  AutoSync: true
  EducationModeEnabled: <bool>
  AppRemovalAllowed: <bool>
  PurgeAllowed: <bool>
  DataProtected: false
  AppInstallationAllowed: <bool>
  SupportsFavorites: true
  DisabledAssetTypes: [Photo, Movie, TVEpisode, HomeVideo]
```

CONFIRMED key set. The boolean values not shown were not recorded. `SyncAllowed` is the phone's statement that it will accept a sync from this host. A client should wait for it before sending `RequestingSync`. Apple's host also keeps state strings `AutoSync`, `ManualSync`, `PurgeAllowed` and `DataProtected` and honours the disabled asset types. INFERRED from strings that the host refuses to sync a disabled type.

## 5. Session numbering, Ping and Pong

### 5.1 Observed values

| Capture | Ping | `RequestingSync` | `ReadyForSync` | Pong |
| --- | --- | --- | --- | --- |
| Windows reference, first connection | Session 0 | Session 1 | Session 1 (`Id` 42) | not seen |
| Windows reference (delta session), later connection | Session 1 | Session 2 | Session 2 | Session 1 |
| Linux, ordinary sessions | Session 1 | Session 2 | Session 2 | Session 2 |

CONFIRMED. All Linux sessions on this phone, over USB and Wi-Fi, used Ping with Session 1 and `RequestingSync` with Session 2. The phone stamped its Pong, startup messages and Ready with Session 2.

### 5.2 Host behaviour (static)

- The host connection object holds a session counter at offset `+0x14`. A new connection is zero-allocated.
- `SendPing` (DLL `0x31b30-0x31b7e`) uses the counter without incrementing it. `SendSyncRequest` increments it (`0x31df3`).
- Incoming `ReadyForSync`, `AssetManifest` and `SyncFinished`/`SyncFailed` messages are each compared with this counter by command-specific code (for example `SyncFinished` at `0x30cce-0x30d10`). A message with another session is not forwarded. CONFIRMED.
- The Pong path (`0x30c22-0x30c4c`) branches to return without comparing the Session. A valid Pong is accepted whatever its Session. CONFIRMED.

Correction: an early Linux probe required the Pong's Session to equal the Ping's. It failed on the first live run because the phone replied with Session 2. The echo requirement was wrong. A client must record the Session values it sent and received separately and must not require equality. How the phone chooses 2 is OPEN.

### 5.3 Grappa session ID versus ATC session number

The host keeps a second, unrelated number: the Grappa session ID at connection offset `+0x24` (`ATHostConnectionGetGrappaSessionId`, DLL `0x30f70`). It is set during request preparation (section 11) and is not the `Session` field. The two must not be confused. CONFIRMED.

### 5.4 Ping from the phone

CONFIRMED (Linux product run). After the last `FileComplete` of an insert batch, while the phone imported the files, the phone sent

```text
{Command: Ping, Session: 2, Type: 0, Id: <n>}
```

instead of a `Progress` or `SyncFinished` message. A host that treats this as unexpected will abandon the session, and the phone finishes the import anyway. The client answers with `{Session: 2, Command: Pong}`, the same shape the phone uses for the host's Pong, and keeps reading. The same Ping can be expected at any point after the metadata phase.

## 6. Media session lifecycle

### 6.1 Sequence

CONFIRMED (every Linux session from the first signed insert on; Windows reference sessions for the host-side steps).

```text
host                                         phone
 |---- Ping (Session 1) ------------------->|
 |<--- Pong, Capabilities, InstalledAssets, |
 |     AssetMetrics, SyncAllowed, AssetMetrics (Session 2)
 |---- RequestingSync (Session 2) --------->|
 |<--- ReadyForSync (Session 2) ------------|
 |     host writes Sync_00000000.plist and  |
 |     Sync_00000000.plist.cig over AFC     |
 |---- FinishedSyncingMetadata ------------>|
 |<--- AssetManifest (if assets are needed) |   or SyncFinished directly
 |<--- Progress ... (OverallProgress)       |
 |---- AFC upload of each requested asset   |
 |---- FileComplete / FileError per asset ->|
 |<--- SyncFinished ------------------------|
 |---- close ATC connection                 |
```

Notes on the sequence:

- The metadata is not sent inside an ATC message. It is a file written through AFC (the Apple File Conduit service) into a staging directory, followed by `FinishedSyncingMetadata`. Section 8 describes the file.
- The phone applies the metadata after it receives `FinishedSyncingMetadata` and finds the staged file. The receiver finished within about 0.07 to 0.6 s of the operation being staged in metadata-only sessions. For an insert batch the single `AssetManifest` arrived 92 ms after staging, and `SyncFinished` came 37 ms after the last `FileComplete`.
- If every operation needs no asset, the phone skips the manifest and sends `SyncFinished` directly. A metadata-only session (updates and deletes) was seen to finish this way. Apple's host handles this case explicitly: its metadata waiter (`0x46d864-0x46dea7`) waits for `AssetManifest` or `SyncFinished`.
- Linux sessions send exactly: Ping, RequestingSync, FinishedSyncingMetadata, then FileComplete and FileError messages as needed. A one-track artwork update sent six messages in all: Ping, RequestingSync, FinishedSyncingMetadata, FileComplete, FileError, FileError.
- `Progress` messages begin after `FinishedSyncingMetadata`. Over USB the first one followed it by 34 to 152 ms.

### 6.2 Host state machine (AMPDevicesAgent)

CONFIRMED by the state-name formatter at AMP RVA `0x455cac-0x455d36` (1.6.4.90).

| Value | Name |
| --- | --- |
| 0 | `Unknown` |
| 1 | `Idle` |
| 2 | `RequestingSync` |
| 3 | `ReadyForSync` |
| 4 | `SyncingMetadata` |
| 5 | `FinishedSyncingMetadata` |
| 6 | `TransferringAssets` |
| 7 | `Finished` |
| 8 | `Failed` |
| 9 | `Canceled` |

These are internal host states, not wire fields.

- The `ReadyForSync` handler (`0x45ee20-0x45ef48`) acts only in state 2 and moves to state 3.
- The `SyncFinished` handler (`0x45f7e0-0x45f812` in 1.6.4.94) accepts state 5 or 6 and moves to state 7 with zero error. It does not parse a success field from `Params`.
- The incoming-message dispatcher (AMP `0x4613bc`, caller `0x46f021`) passes the full dictionary to a handler chosen by `Command`. The static table includes `ReadyForSync` to `0x45ee20`, `AssetManifest` to `0x45f610` and `SyncFinished` to `0x45f6bc`. The `AssetManifest` handler unwraps `Params.AssetManifest` before calling the manifest worker (`0x45ef4c`).
- The dispatcher's string table lists the inbound commands the host handles: `SyncAllowed`, `ReadyForSync`, `AssetManifest`, `SyncFinished`, `SyncFailed`, `Progress`, `Ping`, `FileError`, `AssetMetrics`, `InstalledAssets` and `ConnectionInvalid`. No message in the list carries a per-track inventory with sizes or hashes. CONFIRMED statically (AMP `0x4613bc`, strings at `0x461510` to `0x46168d`).

### 6.3 Dataclasses, sync types and anchors

Dataclass names seen: `Media`, `Book`, `Keybag`, `VoiceMemo`. Music lives in `Media`.

Host-side initialisation (static, AMP `0x46b1f8-0x46b218` and `0x46acb8-0x46ae7a`): the first dataclass entry is `Media` with mask 1. Each entry holds a host-last anchor (`+0x14`), a host-next anchor (`+0x18`), a device-last anchor (`+0x1c`), a chosen sync type (`+0x20`) and a pending operation array. Fresh entries start with both host anchors at 1. This is the initial state of a new host-side entry. It is not a rule for a new independent host.

Anchor wire format: a decimal string produced by `%u` (`0x46baa8`), nonzero. Example `"7"`.

Sync type values (`FinishedSyncingMetadata.Params.SyncTypes`), CONFIRMED by branch-proven name mapping (`0x46c95a-0x46c991`):

| Value | Host name | Meaning |
| --- | --- | --- |
| 0 | `ATSyncNoSync` | dataclass not synced in this session |
| 1 | `ATSyncDeltaSync` | incremental changes |
| 2 | `ATSyncResetSync` | the host sends the whole library (reset) |
| 3 | `ATSyncPrepOnly` | preparation only |

Host selector (`0x46cf0c`, called from `0x46d495`): when the dataclass is eligible and the host's last anchor differs from the device's last anchor, the host picks Reset (2) and drops `upload_track` operations. Otherwise it picks Delta (1). The reset reference session had Media host anchor `"7"`, device anchor `"8"`, and sent `SyncTypes {Media: 2, Book: 0, Keybag: 1}` with 401 operations. The delta reference session had matching anchors `"9"` and `"9"` and sent `{Media: 1, Book: 0, Keybag: 1}`. A Linux client must stop on an anchor mismatch instead of sending Reset, because the reset reference session rebuilt the library from the host's catalog.

Anchor progression for one Media sync, CONFIRMED in Linux sessions:

| Step | Media anchor value |
| --- | --- |
| `RequestingSync.DataclassAnchors.Media` | current anchor `A`, the phone's `MLSyncClientSyncAnchor` |
| `ReadyForSync.DataclassAnchors.Media` | `A` (a mismatch means the client's view is stale; one observed case: client had 10, phone reported 11) |
| operation file `revision`, and `FinishedSyncingMetadata.DataclassAnchors.Media` | `A + 1` |
| the phone afterwards | `A + 1` once the session is applied. A session rolled back at its first `Progress` leaves the anchor at `A` (section 14.3). |

The anchor the phone holds also lives in its Music database (chapter 4). The anchor in the host-side saved catalog file (`iTunesCDB`) is a host-generated snapshot and can be stale. A client must take the live anchor from `ReadyForSync`, not from a saved file.

Apple's host commits `hostNext` to `hostLast` once it receives an `AssetManifest` (`0x46cf78`) or reaches state 6 or 7 (`0x46dba4-0x46dbaa`). This happens before all assets complete. A matching anchor therefore does not prove that every asset arrived.

## 7. The RequestingSync message

### 7.1 Shape

CONFIRMED. This is the exact request the Linux client sends and the shape Apple's host sent in every reference session.

```text
Session: 2
Command: RequestingSync
Params:
  Dataclasses: [Media, Book, Keybag]
  DataclassAnchors: {Media: "A", VoiceMemo: "1"}
  HostInfo:
    Type: iTunes
    Version: 13.2.4.123
    WindowsOSVersion: "10.0"
    SyncHostName: <string>
    LibraryID: <16 uppercase hex digits>
    SyncedDataclasses: [Media]
    SyncedAssetTypes: [Music]
    Wakeable: 0                  # integer
    Grappa: <data 84 bytes>      # added by the host's preparation step
```

Field notes:

| Key | Type | Evidence and notes |
| --- | --- | --- |
| `Dataclasses` | array of string | Order is `Media`, `Book`, `Keybag` in Apple's reference sessions. A Music-only list was not tested. |
| `DataclassAnchors.Media` | string | Anchor `A` as a decimal string. |
| `DataclassAnchors.VoiceMemo` | string | `"1"` in all successful sessions. The phone's `ReadyForSync` does not refresh VoiceMemo (it lists only Media and Book). The saved catalog file stores the value. CONFIRMED value, OPEN why the host sends VoiceMemo. |
| `HostInfo.Type` | string | `iTunes` |
| `HostInfo.Version` | string | `13.2.4.123` in the analysed host. A third-party Books client uses `13.7.0.161`. This string is a compatibility profile, not a discovery of the running OS. The phone requires at least `10.5.0.115` by numeric comparison, or the literal `IGNORE`, and otherwise answers `SyncFailed` with `ErrorCode` 12 (chapter 09, section 6.2). |
| `HostInfo.WindowsOSVersion` | string | `10.0` (the Books client uses a `MacOSVersion` key with a different value). |
| `HostInfo.SyncHostName` | string | free text chosen by the host. |
| `HostInfo.LibraryID` | string | 16 uppercase hex digits (`%016llX`). Section 7.2. |
| `HostInfo.SyncedDataclasses` / `SyncedAssetTypes` | arrays | `[Media]` and `[Music]` in all Music sessions. |
| `HostInfo.Wakeable` | integer | 0 |
| `HostInfo.Grappa` | data | 84 bytes. Section 11. |

The HostInfo builder is AMP `0x46bf50-0x46c2a4`. Asset types are built at `0x46bdd4` from host settings. The phone's `AutoSync` and `ManualSync` flags and the host's disabled types shape the list.

### 7.2 LibraryID

The `LibraryID` identifies which host library the phone is synced with. Apple documents that an iPhone syncs with one iTunes library at a time and that choosing "Remove and Sync" replaces the device content with the new host's content. A fresh `LibraryID` is therefore a risk for existing music. Apple's own HostInfo derivation (AMP `0x48ab60-0x48abbe`, CONFIRMED by disassembly):

1. take a nonzero catalog-derived value,
2. XOR it with the host's persistent user ID (preference `persistentUserID`, read at `0x6edc10`, generated and stored if absent),
3. format the result as `%016llX`.

A third-party Books client sends a new UUID for every operation. That is not Apple's behaviour and is not suitable for Music. The Linux client reuses the existing identity: it derives the two components from the phone's own preference files and sends the same `LibraryID` as the host that originally synced the library. In the phone's code (iOS 27.2) the field is compared with the library of a sync already in progress, and a different one is answered with `SyncFailed` `ErrorCode` 9. No other use that changes the sync was found (chapter 09, sections 6.3 and 6.4). No session with a wrong `LibraryID` was run.

### 7.3 Phone reply: ReadyForSync

CONFIRMED.

```text
Session: 2   Type: 0   Id: <n>
Command: ReadyForSync
Params:
  DataclassAnchors: {Media: "A", Book: "0"}
  DeviceInfo: {Grappa: <data 83 bytes>}
```

- The anchors are the phone's current values.
- `DeviceInfo.Grappa` is an opaque 83-byte value. Both inspected replies start with bytes `01 01` and have byte 18 equal to `0x40`. It has no plist prefix. It is the input to the signing calculation in section 11.
- Apple's host parses the dataclass anchor into the entry's device-last field. A missing anchor yields zero. The host logs a missing `DataclassAnchors` dictionary but still moves to state 3.
- Ready arrives within the same session as the request. A client must check `Session` and `Media` anchor before continuing and must discard any Ready from another session.

## 8. The operation file

### 8.1 Wrapper

CONFIRMED. Metadata changes travel as a binary plist file staged over AFC, then announced with `FinishedSyncingMetadata`. The wrapper builder is AMP RVA `0x4666f0-0x466bc3` (1.6.4.90); in 1.6.4.94 it is `0x466814`.

```text
revision:   <integer>      # next Media anchor, A + 1
timestamp:  <date>         # CFDate, creation time
operations: [ <operation>, ... ]
```

- `timestamp` is a plist date. The writer calls `CFAbsoluteTimeGetCurrent` and `CFDateCreate` (`0x699d1c-0x699dfe`). An early reading took it for a number. That was wrong.
- The serialiser (`0x69c13c-0x69c1b0`) calls `CFPropertyListCreateData` with format 200 (binary plist).
- Apple's writer splits the operations into files of at most 500. CONFIRMED in the code of both builds. The file-set writer (`0x466bc4`; `0x466ce8` in 1.6.4.94) compares the per-device word at `[handler+0x64]` with `0x700` (`0x466bfe`). At `0x700` or above it loops, taking `min(500, remaining)` operations per pass (`0x466c1f`, `0x466c31`), and calls the wrapper builder with chunk index 0, 1, 2 and so on. Each file is named after its chunk index and gets its own `.cig`, and every file carries the same `revision`, the sync anchor. Below `0x700` the writer makes one file named after the revision. For this phone the word is at least `0x700`, because the delta reference session's file was `Sync_00000000.plist` with revision 10. The split happens inside one session, and `FinishedSyncingMetadata` carries no file count. No session, Apple's or Linux's, has staged more than 500 operations, so the phone's handling of several files is OPEN (section 18, item 11). Chapter 08 gives the whole session structure.

### 8.2 Location and name

CONFIRMED statically, then CONFIRMED live by the Linux client.

- Directory: `iTunes_Control/Sync/Media` on the AFC root. The folder codes resolve as `synm` to `sync/Media`, `sync` to `Sync` under the control folder, and the control folder to `iTunes_Control` for non-iPod devices (AMP `0x4690dc-0x46913d`, `0x4c81a1-0x4c81cf`, `0x4c7ece-0x4c7eff`, `0x4c8e3b-0x4c8e88`).
- Operation file: `Sync_%08X.plist` (string at AMP file offset `0x9a4538`), so `Sync_00000000.plist` for index 0.
- Signature sidecar: the same name plus `.cig`, `Sync_00000000.plist.cig` (section 11).
- Write order: operation file first, then sidecar. Both are written with create, write at offset 0, set end-of-file to the data length, close (AMP `0x6acd48`). The writer is not atomic. There is no rename. If the operation write fails the host does not write the sidecar.
- The phone consumes both files after `FinishedSyncingMetadata`. After a session the directory is empty again (observed after each session whose staging area was inspected; before the first Linux session the directory listing held only `.` and `..` and both index-0 names returned `ObjectNotFound`).
- The AFC API used does not offer atomic create-exclusive or no-clobber rename. A client must therefore check that the staging directory is empty before the session and again after `ReadyForSync`, and must own the connection exclusively.

Apple's host also takes a sync lock on the device during a Music sync: it opens `com.apple.itunes.lock_sync` on the AFC root, takes an AFC lock, posts `com.apple.itunes-mobdev.syncWillStart`, `syncLockRequest`, `syncDidStart` and later `syncDidFinish` through the notification proxy, and creates a `com.apple.itunes.syncing` flag (AMP `0x498440`, `0x498c4c`). CONFIRMED statically for Apple's host. Chapter 3, section 7.9 gives the full routine.

INFERRED: the Linux client does not take this lock. No Linux session is recorded as taking it, and the live order of the lock and the notifications was never captured from Apple's host. The phone accepted every recorded Linux session without it, so the lock looks like a courtesy toward other Apple clients and not a receiver requirement. OPEN: how the lock interacts with a concurrent Apple sync.

### 8.3 Operation order within a file

For a non-empty Media file Apple's preparation (`0x468fbc` calling `0x46877c` at `0x469180`) inserts an `update_db_info` operation at index 0, or replaces an existing first `update_db_info`. An empty operation list skips this and skips the staging and the anchor increment altogether (`0x468fbc`). The Linux client uses:

```text
operations:
  - update_db_info (unchanged database PID and primary container PID)
  - <one or more track or playlist operations>
```

Every accepted Linux session (insert, update, delete, playlist) used this shape. CONFIRMED.

### 8.4 update_db_info

```text
operation: update_db_info
pid: <database PID, signed 64-bit>
db_info:
  primary_container_pid: <PID of the library's primary playlist>
```

CONFIRMED in both reference sessions and every Linux session. Both values repeat the phone's existing identities. They are read from the phone's catalog; they are never allocated by the client. The database PID is also stored in the saved catalog file header (`+0x48`). The primary container is the playlist whose object flag `+0x238 & 4` is set. An unchanged `update_db_info` does not request a playlist replacement or a reset.

### 8.5 Pruning of earlier operations

CONFIRMED statically for Apple's host (AMP 1.6.4.90, `0x140469838`, `0x14046aaa5-0x14046aadb`, `0x14046a96a`, `0x14046a9cc`). The host queues operations as the library changes and merges them before staging. When a `delete_playlist` arrives for a PID, the host walks the pending array backwards. It deletes every earlier `insert_playlist` and `update_playlist` for that PID and logs `airtraffic> removing %@ operation for persistent id %@`. If it removed an insert the delete itself is dropped. Likewise `delete_album` and `delete_artist` drop an earlier `insert_album` or `insert_artist`. A group created and deleted between two syncs is never sent. A client that builds its own operation list should apply the same rule, or must accept that it sends both operations.

## 9. Track operations

Operation names in AMP (string table at file offsets `0x99fe28-0x99ff78`): `delete_track`, `update_track`, `insert_track`, `update_album`, `insert_album`, `delete_artist`, `update_artist`, `insert_artist`, `delete_playlist`, `update_playlist`, `insert_playlist`, `update_db_info`, `delete_album`. The host constructor for track operations is at AMP `0x461900-0x464d33`, with the base builder `0x461880-0x4618fe` creating `{operation, pid}`. Host source assertions name `kAirTrafficSyncOperation_InsertTrack`, `UpdateTrack`, `DeleteTrack` and `UploadTrack`. `upload_track` was never captured.

### 9.1 Common shape

```text
operation: insert_track | update_track | delete_track
pid: <signed 64-bit integer>      # CFNumber SInt64; the host's persistent ID
```

The PID is stored as a signed 64-bit integer (CFNumber type 4, `0x69a614`), so about half of all PIDs are negative. A client must keep the bit pattern when converting to or from unsigned hexadecimal. The phone records the wire PID as the track's `item_store.sync_id`. The AssetID in the manifest is the same PID as a signed decimal string (section 12.2).

### 9.2 delete_track

```text
{operation: delete_track, pid: <PID>}
```

CONFIRMED. Exactly these two keys. The phone accepts `delete_track` for a row whose file is already gone. See [chapter 5](05-receiver-operations.md) for the database effects.

Session behaviour, CONFIRMED in the recorded Linux deletions:

- The session is metadata only. The phone sends `SyncFinished` with no asset exchange except the ancillary refusals of section 14.2. The Media anchor advances by one (for example 64 to 65 and 100 to 101 in two recorded sessions).
- The host does not remove the audio file. In the first recorded deletion the file was still present when it was read immediately after `SyncFinished` (59,890 bytes, bytes unchanged). A later read returned AFC status 8 (`ObjectNotFound`). The phone removes the file itself, after a delay. A client must not treat a present file just after `SyncFinished` as a failed delete, and must not remove the file over AFC.
- A deleted track's album and artist rows can stay behind with no member. The phone matches groups by name, so a later insert with the same names reuses them (section 12.6).

### 9.3 insert_track

CONFIRMED from the two Apple reference sessions (399 inserts in the reset session, one in the delta session) and from every Linux insert.

```text
operation: insert_track
pid: -3456789012345678901
item:
  title: <string>
  artist: <string>
  album: <string>
  genre: <string>
  album_artist: <string>             # omitted when the source has none
  composer, copyright: <string>      # omitted when empty
  album_pid: <signed 64-bit>         # existing or new album identity
  artist_pid: <signed 64-bit>        # album artist identity
  track_number: <integer>
  track_count: <integer>
  disc_number: <integer>
  total_time_ms: <integer>
  is_song: true
  remember_bookmark: false
  content_rating: <integer>          # omitted when 0
  year: <integer>                    # omitted when 0
  sort_name / sort_artist / sort_album: <string>          # always, when the name is not empty
  sort_album_artist, sort_composer: <string>          # exactly when album_artist, composer are sent
  comment: <string>                  # omitted when the source has none
  date_created, date_modified: <date>  # omitted when unknown
  artwork_checksum: <integer>        # always emitted, may be 0
  artwork_cache_id: <integer>        # omitted when 0
item_stats:
  date_played, date_skipped: <date>
  play_count_user, skip_count_user, play_count_recent, skip_count_recent: <integer>
  has_been_played: <bool>
avformat_info:
  audio_format: 601                  # observed for 16-bit stereo ALAC
  sample_rate: 44100.0               # real
  bit_rate: 1411
  duration: 7291644                  # integer, sample count in the observed ALAC
  gapless_encoding_delay: 0          # the four gapless keys are emitted together
  gapless_encoding_drain: 0
  gapless_heuristic_info: 1
  gapless_last_frame_resynch: 0
  volume_normalization_energy: <integer>   # omitted when 0
  analysis_inhibit_flags: 2          # present on every captured ALAC insert
location:
  kind: "Apple Lossless audio file"
  # no path and no file size at insertion time
```

Field evidence:

| Field | Type | Example | Evidence |
| --- | --- | --- | --- |
| `item.title`, `artist`, `album`, `genre`, `album_artist` | string | | Setter mapping at AMP `0x4620c8-0x462124`, `0x462299-0x4622af`, `0x462faa-0x462fc0`; CONFIRMED in captured dictionaries |
| `item.track_number`, `disc_number`, `total_time_ms` | integer | `5`, `1`, `144989` | Numeric helper `0x69950c`; CONFIRMED |
| `item.is_song`, `remember_bookmark`, `content_rating` | bool, bool, integer | `true`, `false`, `1` | CONFIRMED in all 399 inserts of the reset reference session (`is_song` true, `remember_bookmark` false). No media-type or category key was observed. |
| `item.sort_*` | string | `sort_name: "Example Title"` | CONFIRMED. Apple writes `sort_name` etc. and no `sort_title` key. Captured sort strings equal the plain artist, composer and album-artist strings; three title/album examples strip a leading article. The derivation in the host is traced in chapter 4, section 6. |
| `item.comment` | string | `comment: "Processed by SoX"` | CONFIRMED. Sent only when the source has a non-empty comment; two captured inserts carry one. The phone stores it unchanged, newlines included (chapter 4, sections 6 and 6.10). |
| `item.album_pid`, `artist_pid` | integer | | CONFIRMED. A new album PID can be introduced inside `insert_track` without a separate `insert_album`. Equal album text is not a grouping key: two tracks titled alike had different album PIDs. |
| `avformat_info.audio_format` | integer | `601` | CONFIRMED for ALAC 16-bit stereo. No general codec mapping is known. Other formats use other values; see [chapter 6](06-audio-formats-and-gapless.md). |
| `avformat_info.sample_rate` | real | `44100.0` | Floating-point setter `0x6999cc`; CONFIRMED |
| `avformat_info.duration` | integer | `6394020` | For ALAC this equals the sample count per channel; CONFIRMED on two references. Units for other formats are OPEN. |
| `avformat_info.bit_rate` | integer | `1411` | Observed for 16-bit stereo ALAC. |
| `location.kind` | string | `"Apple Lossless audio file"` | CONFIRMED. In the staged insert the host sends only `kind`. The wider static builder also knows `location`, `location_type`, `file_size` and `extension` (`0x4623e5-0x462540`) and sends `file_size` when an internal flag is set. Those keys were never seen in a captured insert. |
| `item_stats` | dictionary | | CONFIRMED in captured inserts and updates. |

Observed on every captured ALAC insert, and sent by the Linux client: `gapless_heuristic_info` 1, delay, drain and last-frame resynch all 0, `analysis_inhibit_flags` 2. These are observed values, not recovered defaults, and their bit meanings are OPEN. A generated 3-second 440 Hz stereo ALAC tone (132,300 samples per channel at 44,100 Hz) was inserted with `duration: 132300`, `total_time_ms: 3000`, zero `item_stats`, `artwork_checksum: 0` and no `album_artist`, and the phone accepted it.

For an MP3 source the same dictionary shape carries `audio_format` 301 and a `channels` key. A prepared (not yet receiver-observed) wire proposal for a headerless constant-bit-rate MPEG-1 Layer III file at 44.1 kHz, stereo, 128 kbps with 59 frames is `duration: 67968` (59 frames times 1,152 samples) and `total_time_ms: 1541` (the floor of 1541.2). INFERRED for that exact file; chapter 6 gives the MP3 values the phone accepted. Apple's own MP3 insert carries the gapless group with values its host computes from the file, and Linux inserts compute the same group by Apple's rule (chapter 6, sections 12.7 and 13).

Rules observed for optional values (static writer branches checked against captured rows):

- Zero normalisation energy and zero analysis flags are omitted. Fourteen inserts of the reset reference session omit normalisation.
- The gapless fields are emitted together only when an internal mask bit is set. Otherwise the whole group is skipped.
- Year zero is omitted. `content_rating` zero is omitted on a full insert.
- `date_created` and `date_modified` are skipped when the internal timestamps are zero. All captured Apple inserts carry both dates. The Linux client sets both to the insertion time. That is a choice, not a native default.
- `artwork_cache_id` is omitted when zero. `artwork_checksum` is always emitted, including 0. A Linux insert of a track without a separate cover registration sends `artwork_checksum: 0` with no cache ID, and the embedded cover in the file is still picked up by the phone.
- `total_time_ms` is `floor(duration_samples * 1000 / sample_rate)`. All 399 captured ALAC inserts follow this rule. Nearest-rounding matches only 230 of them.

Artwork fields, CONFIRMED (chapter 4, section 10 has the full account):

- `artwork_checksum` is a size, not a digest. For one MP4 cover it is the JPEG length plus 17 (a 187,306-byte JPEG gives 187,323). For an ID3 `APIC` cover it is the length plus 1 (54,360 gives 54,361). In general it is the number of pictures plus the summed size of their containers, and the image type is not read, so a PNG counts the same way (chapter 6, section 14.3).
- `artwork_cache_id` is a host-assigned image record ID. The phone does not allocate it. A sender picks a value above every cache ID already in the phone's database and in its saved ArtworkDB (batch inserts used 714, 715 and 716).
- With both fields set, the phone's manifest asks for the track with `AssetParts` 3. The sender stages the cover at `Airlock/Media/Artwork/<AssetID>` first, reads it back, then sends the audio and one `FileComplete` naming the audio path. That one message claims both parts.

This table is the set of fields Apple's host emits. It is not a proven minimal schema for the receiver. Chapter 4 and chapter 5 describe what the phone does with each field.

### 9.4 update_track

CONFIRMED.

```text
operation: update_track
pid: <existing PID>
item: { <only the changed keys> }
```

Observed uses:

- Metadata edit: `item` holds the changed fields (title, track number and so on). Several `update_track` operations, including more than one for the same track, can appear in one file. They apply in order. Apple's own title edit sends `title`, `sort_name` and the track's current `album_pid` in `item`; the extra `album_pid` changes nothing else on the phone. Field-by-field receiver rules are in [chapter 5](05-receiver-operations.md).
- Artwork association: `{operation: update_track, pid, item: {artwork_cache_id: <u32>, artwork_checksum: <u32>}}`. Apple's delta reference session carried 399 such operations for existing tracks. A Linux artwork update sent a 290-byte file with `update_db_info` and one such `update_track` (cache ID 677, checksum 187323).
- Statistics: an `update_track` with `item_stats` carries play and skip counts and dates.

Observation about reference sessions: Apple's staged delta sessions are noisy. The delta reference session staged one `update_db_info`, one `insert_track` and 400 `update_track` operations (399 artwork-only, one statistics-only). The reset reference session staged `update_db_info`, 399 `insert_track` and one `insert_playlist`. A client that sends only the operations it needs was accepted by the phone in every Linux session. Omitting the extra updates is therefore known to work.

In the reset reference session the track constructor produced 399 additional `update_track` candidates carrying only artwork cache fields, but they were not part of the staged array. Constructor candidates and staged operations are different sets.

### 9.5 PID allocation

- Apple's host honours a PID supplied in its track structure (`0x38f248`, input `+0x18`) or calls an allocator (`0x354114`) when it is zero. The album and artist constructors do the same (`0x2d3798`, album `+0x20`). The allocator excludes unsigned values below 1000. CONFIRMED statically. How the generator is seeded and whether the phone accepts arbitrary PIDs is not fully known.
- The Linux client allocates fresh random 64-bit values for new track, album and artist PIDs and checks them against all PIDs read from the phone's catalog and its own records. The phone accepted these in every session. CONFIRMED.
- The phone stores wire PIDs as `sync_id` values. Its internal item row IDs are separate and must not be sent. Chapter 4 describes the identity model.

### 9.6 Albums and artists

Apple's host sends `insert_album`, `update_album`, `insert_artist`, `update_artist` and the matching deletes as bare `{operation, pid}` or fuller dictionaries only when a per-device switch (a word at handler offset `+0x64`) is below `0x800`. Evidence that it is not below that for this phone: Apple's full reset of this phone (the reset reference session) carried no `insert_album` or `insert_artist`, and none of the four retained Windows sessions with `update_db_info` has an album or artist operation. The phone builds album and artist groups from the track metadata. CONFIRMED. The value held by the switch was not decoded. Section 10.4 covers deletes of albums and artists.

### 9.7 Replacing the audio of an existing track

OPEN. No closed session replaced the audio of a track that already exists on the phone. The static findings are in AMP 1.6.4.94 (SHA-256 `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9`, RVAs as in section 1):

- **Selecting update.** The modified-track notification `0x6d747269` selects `update_track` instead of `insert_track` (`0x46a17d` to `0x46a186`). The shared builder (`0x461a24`) can emit audio-format values (sample rate, bit rate, total time, format, duration, gapless and analysis fields). A dictionary with only `operation` and `pid` is skipped. This shows that the dictionary can be built. It does not show that the phone invalidates audio when it receives one.
- **The `location` branch of `update_track`.** When the track instance byte `+0x45` is 0, the builder (`0x46249b`) emits a root-level `location` dictionary with `location` (the permanent path), `location_type` (a fourcc as a 32-bit integer), `kind`, `file_size` (64-bit), an optional `extension` and an optional `date_created`. A separate branch emits `location: {kind: ...}` only. An empty path is not serialised as a replacement instruction. No `replace_audio` or force-download key exists in the inspected builder.
- **A bare update tied to a queued transfer.** When a transfer is queued for an existing PID and the instance has permanent-location status 10 (`SyncInProgress`, assertion string in `0x45d7ac`), the add and merge routine (`0x3580f4`) emits notification `0x61647472`, and the generator (`0x469da3`) produces exactly `{operation: update_track, pid: <existing PID>}`. This is a deliberate native bare update. It is not shown to force a download from a receiver that already holds the audio. The host copy flag `0x10` seen in this path is internal, not an ATC wire flag.
- **Audio delivery allocates a new path.** The manifest handler (`0x45bd54`) resolves the requested PID and does not require it to be new. The completion callback (`0x45b538`) allocates a fresh permanent destination (`0x45a814`) and stores it. It never selects the old path as an overwrite target. The sequence is the one in section 12.3.
- **Coalescing.** The host queue merges adjacent `update_track` operations for the same PID and can replace an adjacent update with a full insert. A `delete_track` followed by an `insert_track` for the same PID stays two operations at this stage. The final wire order was not observed.
- **A host path that deletes first.** The synchronisation pass `0x5658b8` has a branch logged as "replacing track ... since library version is newer". The branch skips the step that clears a track's marked-for-deletion bit (`+0xc2 & 0x80`, `0x568024`), and the pass then deletes marked tracks (`0x564c34`, `0x564b94`) before it adds the source tracks again (`0x568528`, `0x3580f4`). The compared value is a "store version" at `object+0xa8`. It is not the file time, a hash or the ATC anchor.
- **What the host compares.** The matcher (`0x564994`, `0x35a28c`) compares identity, total time (a window of 500 units for some formats), bit rate, sample rate, channel count, kind and metadata. It does not compare file size, file time, source bytes or PCM. A mismatch does not by itself mean "replace the audio".

Consequence for a client: do not write over a current Music file through AFC, and do not treat a changed `update_track` format or `location` dictionary as a replacement command. What the phone does with new audio for an existing PID (continuity of the PID, disposal of the old file, effect on play counts and playlists) was never observed.

## 10. Playlist and container operations

Operation names appear inside the operation file. They are not ATC commands. The shared playlist constructor is AMP `0x464eb4..0x465a1e` (1.6.4.94; slice SHA-256 `9beef1730a360cabdbfb14f686b4a0769cfac8721e9fa3435d4babe1e6cb657a`). In 1.6.4.90 the matching code is at `0x464d90`. The database effects of each operation are in [chapter 5](05-receiver-operations.md).

| Operation | Payload | Evidence |
| --- | --- | --- |
| `insert_playlist` | `pid`, `container`, `container_ui`, `item_to_container` | CONFIRMED: sent from Linux twice (one-member and two-member ordinary playlists); also present once in Apple's reset session |
| `update_playlist` | same constructor, existing PID, complete ordered member list | INFERRED from code (`0x469c2c`, `0x469e42`); encoded by test tooling but never sent to a phone |
| `delete_playlist` | `{operation, pid}` | CONFIRMED on the phone |
| `delete_track` | `{operation, pid}` | CONFIRMED. Removing a track from the library also removes its playlist membership. It is not a command to remove only a membership. |

No add-member, remove-member or reorder operation was found in the bounded evidence. A longer or shorter `item_to_container` list in `update_playlist` is the only candidate. This is a search result, not proof that no such operation exists.

### 10.1 insert_playlist

CONFIRMED (Linux).

```text
revision: 115
timestamp: <date>
operations:
  - operation: update_db_info
    pid: <database PID>
    db_info: {primary_container_pid: <PID>}
  - operation: insert_playlist
    pid: <playlist PID>
    container:
      name: "Test Playlist"
      date_created: <date>
      date_modified: <date>
    container_ui:
      play_order: 1
      album_field_order: 0
      repeat_mode: 0
      is_reversed: false
      shuffle_items: false
      has_been_shuffled: false
    item_to_container: [5038471926650183427, 2971064853318807615]
```

- `item_to_container` is the ordered list of member wire PIDs. It lists track PIDs, not database row IDs. Convert unsigned PIDs to signed integers without changing the bit pattern. The new playlist's own PID in the example is negative for that reason.
- A session carrying this operation has no asset transfer. It runs Ping, RequestingSync, metadata files, FinishedSyncingMetadata, and ends with `SyncFinished`. The audited creation session had 15 ATC frames, Ready anchor 114 and finished anchor 115. The staged operation file was 543 bytes and the sidecar 21 bytes. The phone asked for two ancillary assets for unrelated tracks, which the client declined with `FileError` (section 14.2).
- Apple's reset reference session carried one `insert_playlist` for the hidden primary playlist with 399 members. That is a reset transaction and not a template.
- The constructor knows many more keys, all CONFIRMED as present in code and none sent: `description`, `description_long`, `uuid`, `distinguished_kind`, `parent_pid`, `is_hidden`, `cloud_id`, `liked_state`, `date_liked`, `source_device_guid`, `artwork_cache_id`, `is_external_vendor_playlist`, `external_vendor_identifier`, `external_vendor_display_name`, `external_tag`, `smart_is_folder`, `smart_is_dynamic`, `smart_is_filtered`, `smart_is_genius`, `smart_enabled_only`, `smart_is_limited`, `smart_limit_kind`, `smart_limit_value`, `smart_limit_order`, `smart_evaluation_order`, `smart_reverse_limit_order`, `smart_criteria`, `genius_seed_tracks`. In the ordinary non-smart branch, a zero `distinguished_kind` and false `is_hidden` can be omitted. Omission does not prove the receiver's defaults.

### 10.2 update_playlist

The constructor shares the insert path with an existing PID. Triggers in the host: change codes `tdpd` and `atmp` (AMP `0x14046a25f`, `0x140469e13`). The host builds a candidate dictionary, compares it with the cached copy using `CFEqual`, and logs `skipping update_playlist operation` when equal (`0x46a3c2-0x46a450`). INFERRED uses are rename, reorder and membership change. None was tested on a phone.

### 10.3 delete_playlist

CONFIRMED.

```text
{operation: delete_playlist, pid: <playlist PID>}
```

- Static: the constructor tests the three playlist operation kinds, calls `0x461880(kind, [playlist+0x60])`, and on delete returns that dictionary at `0x1404658cc` (AMP 1.6.4.90). Delete bypasses the container, UI and member construction.
- Dispatch: change code `tdrp` at `0x14046a20d`, behind the same predicate as insert and update (`0x140464d34`). The predicate reads as "this playlist is synced to this device". It is true when `0x1404c4304(playlist, [handler+0x374])` holds, or when the playlist kind word is `0x20` or `0x23` with two handler flags set.
- On the phone (Linux sessions), the operation placed after `update_db_info` removed an empty playlist and a two-member ordinary playlist. The tracks stayed. See chapter 5.
- Host-side rules visible in strings: the host never deletes the primary playlist (`sync> ignoring attempt to delete primary playlist`), distinguished playlists, or non-empty playlist folders. It stops when all device playlists are deleted. These apply to Apple's host, not necessarily to what the phone accepts.

### 10.4 delete_album and delete_artist

The host builds both as bare `{operation, pid}`, the same shape as `delete_track`. The album constructor is at `0x140465c04` (returns at `0x14046609b`), the artist one at `0x1404658fc`. Dispatch codes: `tdra` for `delete_album` (`0x14046a238`) and `t2ra` for `delete_artist` (`0x140469ec7`), both behind the `[handler+0x64] < 0x800` switch.

CONFIRMED on the phone (six sessions): both operations complete the session and advance the anchor, but no album or artist row is removed, whether the PID is the group's `sync_id` or its row ID. The phone collects emptied groups on its own in some sessions. This matches the finding in section 9.6 that Apple's host never sends album or artist operations to this phone. Do not rely on `delete_album` or `delete_artist`.

### 10.5 Reset path

The reset builder (AMP `0x468330-0x468420`) assembles the full library operation list. For `Media`, and only when the switch word is below `0x800` (`0x468387`), it sends one `insert_artist` per artist and one `insert_album` per album before the tracks. Its playlist step selects `insert_playlist` (`0x46860d`, `0x468617`). Not relevant to a Linux client that stays in Delta.

## 11. Signing

### 11.1 What the host does

CONFIRMED by static analysis and by the Frida traces.

After writing the operation file, the host's writer compares its dataclass argument with the literal `Media` (`0x4669b9-0x4669e9`). For other dataclasses it stops. For `Media` it reads the connection's Grappa session ID (DLL `ATHostConnectionGetGrappaSessionId`, `0x30f70`, which returns connection field `+0x24`). The two branches are:

1. Session ID is zero: skip signing. The writer returns success with only the `.plist` written (`0x466b7e`, `0x466cc2`). CONFIRMED statically.
2. Session ID is nonzero: sign the serialised operation bytes with an internal routine (log tag `GrappaHostSign`), then write the signature through the raw-byte writer `0x6ad344` to `<operation file name>.cig` (`0x466ad4-0x466b09`; the suffix is appended at `0x6bbdc0 -> 0x731b00`). A signing or sidecar write failure replaces the return status and propagates as an error.

In the ordinary Apple sync the second branch ran. The Frida trace of the delta reference session caught the sidecar call: a 21-byte value named `Sync_00000000.plist.cig` written with flags 3, whose writer returned status 0. CONFIRMED. The operation file was 21,916 bytes with SHA-256 `3a88640ecb0159bc992e69b0af38327d286c60c2afd316c18cbe0331d7c34cc1`; the sidecar SHA-256 was `2a8fe30e59b89d49570307817ed2d0e86d330ddb5f827bf722ef43c61c66c58d`.

Build mapping for the signing seam (1.6.4.90 to 1.6.4.94, verified instruction-for-instruction after relocation):

| Seam | 1.6.4.90 | 1.6.4.94 |
| --- | --- | --- |
| `ReadyForSync` handler | `0x45ee20` | `0x45ef44` |
| unwrapped manifest handler | `0x45ef4c` | `0x45f070` |
| operation writer | `0x4666f0` | `0x466814` |
| operation serialiser | `0x69c13c` | `0x69c290` |
| signature-file writer | `0x6ad344` | `0x6ad498` |
| ordinary metadata signer | `0x248b0` | `0x248b0` |
| full incoming message callback (DLL) | `0x307a0` | `0x307a0` |

### 11.2 Where the session ID comes from

`ATHostConnectionSendSyncRequest` (DLL `0x31dd0`) clears any previous session ID at `0x31e17-0x31e1c`, then passes the address of field `+0x24` to an opaque preparation routine at `0x11110` (`0x31e2d-0x31e31`). On success it also copies the produced bytes into the HostInfo as `Grappa` (a CFData on a mutable copy). On failure it logs and continues without `Grappa`. The `ReadyForSync` branch (`0x308ce-0x30994`) reads the numeric ID, takes `DeviceInfo.Grappa`, and passes ID, pointer and length to an opaque routine at `0x3430`. An error there fails the host's session. If the ID is zero the Ready processing is skipped.

The host therefore performs a Grappa exchange: a request blob in `RequestingSync.HostInfo.Grappa` and a reply blob in `ReadyForSync.DeviceInfo.Grappa`. The routines at `0x11110` and `0x3430` were not decoded. The Ready blob has no plist structure and no interpretation of its bytes was derived directly from the native code.

### 11.3 What was not recovered

OPEN. The signer called by the host is AMP function `0x248b0`. Its behaviour was analysed statically and not recovered:

- It is reached by a series of indirect calls through relocated tables, with state-dependent branches that cannot be resolved from the file alone. The selector leaves (`0xa1954`, `0xa1752`, `0xa1602`) return the fixed values 113, 102 and 63 for all 256 possible inputs; the first indirect callees resolve to a byte-fill routine (`0xa10b0`, called with zero and 128) and an obfuscated dispatcher (`0xb8e60-0xbc71d`).
- Seven descriptor calls marshal a fixed value, the numeric session ID, the operation pointer and length, and output buffers (`0x24983`, `0x24a42`, `0x24af1`, `0x24b9e`, `0x24c57`, `0x24d0e`, `0x24d98`). The argument encoding is understood (five variable expressions equal input plus `0x221901db30730e2e` modulo 2^64) but the signature algorithm behind the descriptors was not.
- Several branches depend on writable global words (`0xad6474` with file initialiser `0x71c`, `0xad6388` with `0x26c6470e`, `0xad7470`, `0xb6eac8`, `0xad73d4`; in 1.6.4.94 `0xad7474` and `0xad7388`) whose live values were never read.
- The native sidecar of the delta reference session is not an unkeyed SHA-1 of the operation file. Both the 20-byte prefix and suffix layouts were compared and rejected.
- The native session's request blob is 84 bytes with byte 18 equal to `0x01`. The public computation below does not reproduce the native sidecar from the native tuple.

No portable implementation of the native signer exists. No Apple code was executed outside Apple's own process.

### 11.4 What the Linux client uses

CONFIRMED. A public third-party project (go-tunes, commit `3ee38393877a8adc5f5a928ddc834ee26bf5512b`, no licence) contains a standalone calculation for a fixed host request. That project writes ringtone operations with zero anchors and its own host identity. None of that is reused: only the request bytes and the signing calculation are, together with the library's existing identity and live anchors. A compact form built from standard primitives was recovered from it and verified independently. The Linux client sends that project's fixed 84-byte host Grappa request in `HostInfo.Grappa` instead of Apple's per-session one, and signs with the calculation below. CONFIRMED for the recorded sessions: the phone accepted every signed operation file produced this way in the explicitly recorded Linux sessions. These are the first signed insert, the audio retry and the artwork registration, the playlist creation session, the delete sessions, the playlist delete sessions and the batch insert, metadata and removal sessions. No signed file was refused in any of them. Sessions that were not individually recorded are not claimed.

The fixed request is 84 bytes, SHA-256 `6c3f39f3efbf1d2a29a983f58a974c43f1f5b05b529f8ba0c116f9d51842d348`, with byte 18 equal to `0x04`. Apple's request in the delta reference session has byte 18 equal to `0x01`. The meaning of that byte is OPEN.

The calculation, for an 83-byte Ready blob `R` and the exact serialised operation bytes `P`. Slices are zero-based and half-open.

1. `D` = AES-128-CBC decryption of `R[19:83]` with IV `R[2:18]` and key equal to the byte `0x95` repeated 16 times, no padding. `D` has 64 bytes.
2. `K` = AES-128-ECB encryption of one block under key `7776eb98ab6ce602e8e892e41ba7e915` applied to `D[1:17] XOR D[39:55] XOR c6c7c4c5cacbc8c9cecfcccdb2b3b0b1`.
3. The sidecar has 21 bytes: `01` followed by `HMAC-SHA1(K, P) XOR D[18:38] XOR f9fafbfcfdfefff0f1f2f3f4f5f6f7e8e9eaebec`.

Verification performed: the third-party C++ reduced to this form and compiled on Linux without Apple libraries; an independent Python/OpenSSL version matched it on 16 synthetic tokens and operations, including all 64 decoded bytes and 32 complete sidecars; SHA-1 passed 13 known-answer cases; the substitution tables and MixColumns steps of the original tables were checked exhaustively (160 tables at all 256 inputs, 147,456 MixColumns byte checks) against standard AES, and 512 further blocks matched OpenSSL. An implementation using only standard AES, CBC, HMAC and SHA-1 primitives reproduces the sidecar.

Properties and limits:

- The fixed key `0x95` repeated is not a general function of the host's IV. Replacing it with the delta reference session's host-IV bytes plus a derived key fails to reproduce the native sidecar.
- The Ready blob prefix and control bytes are ignored by the calculation, which assumes 83 readable bytes. A client should accept only the exact observed envelope (83 bytes, prefix `01 01`, byte 18 equal to `0x40`) and reject anything else. This narrows the supported inputs. It does not authenticate the reply.
- The operation bytes are signed exactly as written. They must not be re-serialised between signing and staging.
- A signature is tied to the session: every session needs a fresh Ready blob. A blob from a closed connection must not be reused.
- Signed files were accepted for inserts, metadata updates, artwork updates, deletes and playlist creation in the recorded sessions listed above.

### 11.5 Whether the phone requires the signature

OPEN. No session with the sidecar omitted, or with a wrong sidecar, was ever sent. All that is known is that signed files were accepted, that Apple's host signs when it holds a nonzero Grappa session ID, and that its own fallback (zero ID, no sidecar, success) exists in the code. Nothing establishes that the phone enforces the signature. Do not assume an unsigned file is accepted.

The published Books-oriented clients that were examined (AirCard-Linux, AirCard-iOS, AirLift) carry a fixed request table but no operation-signing implementation. They do not supply a Music signer.

## 12. Metadata finished, manifest and asset exchange

### 12.1 FinishedSyncingMetadata

CONFIRMED.

```text
Session: 2
Command: FinishedSyncingMetadata
Params:
  DataclassAnchors: {Media: "A+1", VoiceMemo: "1"}
  SyncTypes: {Media: 1, Book: 0, Keybag: 1}
```

The `SyncTypes` for the reset reference session were `{Media: 2, Book: 0, Keybag: 1}`. A Linux delta session uses `Media: 1`. The message carries exactly these two keys in every observed session.

Sending it ends the metadata phase. Once the phone reads the staged files the changes are applied. A session killed immediately after this message may be rolled back (section 14.3).

### 12.2 AssetManifest

CONFIRMED (Windows callback argument; Linux frames).

The phone sends one `AssetManifest` per session when it needs assets. In the Windows trace the host's handler receives the unwrapped `Params.AssetManifest` dictionary:

```text
Media:
  - AssetType: Music
    IsDownload: true
    AssetID: "-8123456789012345678"        # signed decimal string of the track PID
    Variant: {AssetParts: 3}
  - ...
```

| Key | Type | Meaning |
| --- | --- | --- |
| `Media` | array of dictionaries | One entry per requested asset. The largest recorded count is 54 (the reset reference session). The Linux client rejects a manifest of more than 1,024 entries. INFERRED: 1,024 is a client-side safety bound taken from one design statement. No capture approaches it and no phone limit was observed. |
| `AssetType` | string | `Music` |
| `IsDownload` | bool | `true` for host-to-phone transfer. Apple's host acts only on `IsDownload` entries. |
| `AssetID` | string | The track PID as a bare signed decimal. No prefix. Apple's parser also accepts `track:<n>` and a form with other prefixes (artwork, lyrics, genius), but the live phone sends the bare form. |
| `Variant.AssetParts` | integer | Bit mask of the parts the phone wants. |

The earlier static candidate `track:<pid>` is superseded by the observed bare form. The parser (`0x456084-0x45637b`) requires one or two `:`-separated components and reads one signed or unsigned 64-bit decimal.

`AssetParts` bits, CONFIRMED by the host's copy fiber (`0x45dfe9`, `0x45e01c`, `0x45e09e`, `0x45e120`):

| Bit | Value | Part |
| --- | --- | --- |
| 0 | 1 | Track (audio) |
| 1 | 2 | Artwork |
| 2 | 4 | Genius |
| 3 | 8 | Lyrics |

Observed values: 1, 2, 3, 4, 7, 8, 10. Interpretation: 3 is audio plus artwork, 10 is artwork plus lyrics, 7 is audio plus artwork plus Genius. The phone may ask for a Genius part on top of a track's declared parts. For a track the client inserts, the phone asked for part 1 when the insert declared no artwork and for part 3 when it declared artwork. The phone also asks for artwork and Genius parts of unrelated existing tracks (section 14.2). A single manifest lists every asset of a batch (92 ms after staging in the three-track batch) and no other asset type.

Ordering: the manifest array is not in completion order. In the reset reference session, the audio-plus-artwork request at index 46 completed before the artwork request at index 1, and the lyrics request at index 0 completed later.

### 12.3 Staging audio: Airlock

CONFIRMED by static data flow and by Linux sessions.

For an audio asset the host copies the file through AFC to a staging path, renames it, then reports the final path. Native sequence (AMP worker `0x22887c`, dispatcher `0x4a3c44`, helper `0x46e954`, callback `0x45b538`):

1. Upload the source to `Airlock/Media/<AssetID>.<ext>`, where the extension comes from the source (`.m4a` for ALAC and AAC; `.mp3` for MP3). `Airlock` is a directory on the AFC root.
2. Close the file and verify its size and hash (Linux; Apple's host does not read back).
3. Rename to the bare AssetID, `Airlock/Media/<AssetID>`, in the same folder (`0x45bb8a`, AFC `RenamePath` through `0x5ab44c`).
4. Send `FileComplete` naming the permanent path (section 12.4).

The phone then moves the file from the Airlock to that permanent location. CONFIRMED by Linux batch sessions: every permanent path contained exactly the uploaded bytes after the session. The phone's move is not visible in the host code.

Artwork-only requests (AssetParts 2 for the client's own track) use a different staging location, CONFIRMED live and statically: the cover image bytes go unchanged to `Airlock/Media/Artwork/<exact AssetID>` with no extension and no rename. Completion uses an empty path (section 12.4).

The Airlock directory already exists on the phone with an `Artwork` subdirectory. A client that fails mid-session leaves its staged files in place. The Linux client never deletes them automatically.

### 12.4 FileComplete

CONFIRMED.

For an audio asset:

```text
Session: 2
Command: FileComplete
Params:
  AssetID: "-8123456789012345678"
  Dataclass: Media
  AssetPath: iTunes_Control/Music/F39/PXMR.m4a
```

For an artwork-only asset for the client's own track:

```text
Params: {AssetID: "<PID>", Dataclass: "Media", AssetPath: ""}
```

- The path is root-relative with forward slashes. The host's path builder removes one leading backslash and swaps separators (`0x45710c`, `0x456d98`).
- No `Variant`, hash, size or timestamp is sent. The wire gives the phone no length check on the uploaded file.
- The phone logs a generic "Cannot move asset outside of AFC root" diagnostic while handling the empty `AssetPath` of an artwork completion. The explicit artwork staging path is then used and the import succeeds. Treat this line as context, not as a failure.
- Completion order is not manifest order. Send the client's own audio `FileComplete` first, then the other messages. Linux sends the completion for its own audio before the `FileError` refusals.

### 12.5 Permanent filenames

CONFIRMED statically for Apple's host; the phone imposes none that was found.

- Folder: `iTunes_Control/Music/F00` through `F49` (50 folders). The index is a performance-counter value modulo the number of music groups (default 50, formatted by `0x48906c`). Constructor `0x4ada69` defaults the count to 50, and a mount-dependent branch can reduce it.
- Filename: four uppercase letters, each `'A' + (random_u16 % 26)` (`0x49cb0c`), with the extension: `m4a` for M4A. If the name exists the host adds a decimal suffix, for example `ABCD 1.m4a`. The final filename is limited to 31 UTF-16 units and the full path to 255.
- There is no title or PID hash in the name.
- The host's collision test issues AFC `GetFileInfo` and treats any nonzero status as "free". The Linux client requires an explicit `ObjectNotFound` for both Airlock names and the permanent path, verifies the parent directory, and checks that no catalogue row references the path.

The wire gives the final path to the phone through `FileComplete.AssetPath`.

### 12.6 Insert order and batching

CONFIRMED (Linux batch sessions). A single operation file may carry many `insert_track` operations. The Linux sessions carried up to 499 per session. Apple's host puts every insert of a sync in one session and splits only the operation file, at 500 operations (section 8.1). The reset reference session carried 399 inserts in one session. Files go up in reverse operation order, which is the order of the phone's manifest (chapter 08, section 8.1). A batch of three tracks, all in one new album and artist, produced one manifest for all three, three `FileComplete` messages and `SyncFinished` 37 ms after the last. The phone created one album, one album artist and one item artist once, and revived deleted item rows. Chapter 5 covers the database effects.

Re-adding a deleted track, CONFIRMED: a track that had been deleted was inserted again from the same source file. The `insert_track` carried a new track PID, and the phone placed the file at a new permanent path. It reused the empty album and artist rows that the deletion had left, and it preserved the 425 other tracks. The phone matches albums and artists by name, so the new `insert_track` should carry the existing `album_pid` and `artist_pid` of those groups.

## 13. Completion, progress and teardown

### 13.1 Progress

CONFIRMED (Windows full-incoming seam, 247 messages in the delta reference session).

```text
Command: Progress
Params: {OverallProgress: 0.004 ... 0.988}   # real
```

The phone sends `Progress` messages after `FinishedSyncingMetadata`, with no other parameters. Values rose from about 0.004 to 0.988 in the part of the delta reference session that was recorded (the capture was cut off after that). A client must read and ignore them.

The host-sent `FileProgress` message carries `AssetProgress` and `OverallProgress` only (DLL `0x31810`). Apple's host sent two per asset in the reference session. The Linux client does not send it.

### 13.2 SyncFinished

CONFIRMED. The phone sends `SyncFinished` with the session number in force. The host's handler does not read a success field in `Params`. In Linux sessions the message has no parameters that the client reads. Apple's host accepts it in state 5 (metadata finished) or 6 (transferring assets). The DLL's incoming path checks the `SyncFinished`/`SyncFailed` session against the connection counter and forwards only a match. A captured `SyncFinished` with a stale session is not a valid terminal. A client should require the current session.

While it waits for this message, Apple's Devices window shows "Syncing (Step 4 of 4)" and "Waiting for changes to be applied". INFERRED: this is host state 5, `FinishedSyncingMetadata`.

A `SyncFailed` command is checked the same way in the DLL and is named in the analysed sources, but it was never observed on this phone. The phone's code (iOS 27.2) sends it with an `ErrorCode` parameter, and with `RequiredVersion` for a host whose version is too old. Chapter 09, section 6.3 lists the codes.

### 13.3 What "saved" means

The Apple host, on receiving `SyncFinished`, advances to state 7 and schedules a save of a host-written catalog file (`iTunesCDB`) over AFC (`0x45ed0c -> 0x499578`, `SaveIPodFilesFiber` at `0x4b4050`, writer `0x47b060` with option bit 22). It then clears the sync flag and posts `syncDidFinish` once outstanding saves finish. CONFIRMED statically. Consequences for any client:

- `SyncFinished` does not prove that the saved catalog file changed. An unchanged `iTunesCDB` after an accepted insertion does not indicate failure. A minimal client does not trigger the host save.
- The saved file and `iTunesPrefs` are a host-generated snapshot, not the phone's live database.
- The live evidence of success is: terminal `SyncFinished` for the current session, exact readback of the permanent file, the aggregate `AssetMetrics` count, and the phone's own database (chapter 4).

No terminal acknowledgement from the phone to the host after `SyncFinished` was found. The third-party Books client, for comparison, waits two seconds and releases the connection without reading a final message. For Music, the Linux client reads `SyncFinished` before closing.

### 13.4 Teardown

Linux closes the ATC connection after reading `SyncFinished`. Apple's host releases the connection after anchor commit. There is no `Goodbye` message in any capture. Disconnecting before `SyncFinished` is a cancellation (section 14).

## 14. Errors, interruption and recovery

### 14.1 Connection unusable after an error

The ATC client must treat the connection as unusable after any error or cancellation. CONFIRMED lesson from a cancelled read that left a partial frame in the buffer: a new read call that cleared the buffer lost the frame boundary. The rule: set a flag at the start of every read or write and clear it only after a complete frame has been read or written and flushed. Any error, timeout or dropped future leaves the flag set; later operations fail without I/O. A client discards the connection and restarts the whole session. The 16 MiB frame bound was kept.

### 14.2 FileError and unrequested asset parts

CONFIRMED.

```text
Session: 2
Command: FileError
Params:
  AssetID: "<PID>"
  Dataclass: Media
  ErrorCode: 16        # signed 32-bit
  ErrorIsVisible: false
```

- The serialiser emits exactly these four keys plus the current session. There is no `Variant`, path, metadata or deletion instruction.
The host names these error codes (AMP `0x455b54` to `0x455c54`; CONFIRMED statically, code 18 exists and was not read):

| Code | Name | Code | Name |
| --- | --- | --- | --- |
| 0 | Unknown | 9 | SyncDisallowed |
| 1 | InstallFailed | 10 | NotInBackup |
| 2 | Canceled | 11 | IncompatibleFormat |
| 3 | FileNotFound | 12 | IncompatibleVersion |
| 4 | NotAuthorized | 13 | SystemFailure |
| 5 | AuthorizedButMissingKey | 14 | InsufficientFreeSpace |
| 6 | AccountLimitExceeded | 15 | UnknownAssetIdentifier |
| 7 | Timeout | 16 | Unavailable |
| 8 | ConnectionFailed | 17 | Unused |

There is no size-mismatch or checksum class. When Apple's host receives a `FileError` from the phone, its handler (`0x460530`) only logs the dataclass, asset ID and code. It does not re-queue, delete or resend.

- Code 16 is `ATErrorUnavailable`. Apple's host sends it for an artwork-only request when the track has no artwork cache ID, and for a Genius-only request when the data is absent or Genius sync is disabled (`0x45ab04`). Code 3 appeared in a reference session for a file the host could not find.
- The phone retains the requested variant per AssetID. A FileError for an unrelated asset left that track intact in every observed case.
- A phone-side log line `ATError21` appeared when a client closed the connection without delivering audio: the transfer failed through a Store fallback and the phone removed the unfinished item.

During Linux sessions the phone asked for artwork (part 2) and Genius (part 4) of two unrelated tracks. The client declined them with `FileError` code 16, `ErrorIsVisible: false`, after completing its own audio. The unrelated tracks survived. A client must apply this refusal only to reviewed ancillary-only requests (artwork-only or Genius-only) of tracks that already exist. It must never send it for its own audio, for mixed parts of its own track, or for unknown IDs. If a manifest contains any record outside the expected set, the client should stop before sending any `FileComplete` or `FileError`.

### 14.3 Interruption

CONFIRMED observations from Linux sessions killed at chosen points. The effects on the database are in chapter 5. Protocol-visible rules:

| Session killed at | Outcome |
| --- | --- |
| the phone's first `Progress` (34 to 152 ms after `FinishedSyncingMetadata`) | rolled back: anchor and rows unchanged, staging empty. In one case a deleted track's audio file was also gone and was not restored. |
| `AssetManifest` | applied |
| `SyncFinished` | applied |
| mid-upload of an insert batch | rows without a file location remain; the unfinished items are removed later; the client compensates |
| the client declining or abandoning after the manifest, before uploading | the anchor still advances. The rows the manifest named first were dropped, others stayed without a file. |
| after staging the operation file and sidecar, before `FinishedSyncingMetadata` | nothing applied: the anchor did not move, and both files stayed in `iTunes_Control/Sync/Media` at every later check. The phone takes them only on that message. |

In one product session the host refused an undeclared part (AssetParts 7 for a track that had declared 3, the extra bit being Genius) and abandoned the session before uploading. A client should accept a Genius bit on top of a track's declared parts. The Linux client now does.

A pair left staged blocks the next host, which needs the directory empty. A client that can prove it never sent `FinishedSyncingMetadata` (its own trace of outgoing frames) can read both files back and remove them when they are exactly its own (chapter 07, section 7.2). A drop before the pair was staged was not tested.

Cancelling an in-flight read while sleeping on a different future is unsafe. Keep a single in-flight read for the life of the connection.

### 14.4 Interrupted insert: the first signed insert

CONFIRMED. The first live signed insert (revision 11) was accepted by the phone, which logged a non-reset import of one album, artist and track and a commit. The client's guard stopped before audio because the manifest contained three records (audio, one foreign artwork request, one foreign Genius request) and the policy of that session allowed only the audio. With no audio delivered and the connection closed, the phone removed the new item through the Store fallback. The album and artist collections created by the insert remained, with no track. The retry used revision 12 from the live anchor 11, identical operations and the same reserved PIDs. A retry must not replay the old revision, must not allocate new collection PIDs, and must reconcile the exact owned collections.

### 14.5 Empty delta

CONFIRMED statically. When there are no operations Apple's host stages nothing and does not increment the anchor, but still sends `FinishedSyncingMetadata` with `SyncTypes Media: 1` and the unchanged next anchor.

## 15. Native session templates

The Linux client builds both outgoing messages that frame a Media session before connecting. They are the compatibility profile taken from Apple's host, and the exact fields below are what the phone accepted in the audio retry and artwork sessions (anchors 11/12 and 12/13).

| Field | Value and type |
| --- | --- |
| `Session` | integer 2 in both templates |
| Request `Command` | `RequestingSync` |
| Request `Params` | `HostInfo`, `DataclassAnchors`, `Dataclasses` only |
| `HostInfo` | `Type: iTunes`, `Version: 13.2.4.123`, `WindowsOSVersion: 10.0`, integer `Wakeable: 0`, string `LibraryID`, string `SyncHostName`, 84-byte `Grappa` data |
| Synced scope | `SyncedDataclasses: [Media]`, `SyncedAssetTypes: [Music]` |
| Requested dataclasses | `[Media, Book, Keybag]` in this order |
| `DataclassAnchors` in both messages | string `Media: A`, string `VoiceMemo: 1` (the completion template is built with the same A; the client advances it to A+1 only when it sends) |
| Completion `Command` | `FinishedSyncingMetadata` |
| Completion `Params` | `DataclassAnchors`, `SyncTypes` only |
| `SyncTypes` | integer `Media: 1`, `Book: 0`, `Keybag: 1` |

Notes:

- The `Version` and `WindowsOSVersion` fields are a wire compatibility profile, not claims about the host OS. A different advertisement or a Music-only dataclass list needs separate verification.
- No Book or Keybag metadata operations or transfers occur. The request lists them because Apple's host does. The Book count on the phone was zero.
- The VoiceMemo anchor is not refreshed by `ReadyForSync`. It comes from the saved host catalog (a dataset in `iTunesCDB` holding four u32 values in the order Media, Ringtone, VoiceMemo, Book; the observed payload started `[10, 1, 1, 1]`). The Media anchor comes from the phone's live database instead of the saved catalog.
- The templates do not predict assets. Pending asset IDs are only known from the manifest after the metadata phase. The client must not bake in historical asset IDs.

Sequence of the six Linux message types in a normal Media session:

1. `Ping` (Session 1).
2. `RequestingSync` (Session 2, anchor A).
3. wait for `ReadyForSync` with Session 2 and Media anchor A; derive the sidecar signer from its Grappa blob.
4. stage `Sync_00000000.plist`, then `Sync_00000000.plist.cig`, and read both back.
5. `FinishedSyncingMetadata` (Session 2, anchor A+1).
6. read `AssetManifest` (if any), upload and complete own assets, refuse reviewed foreign assets, read `SyncFinished`.

An artwork-only update of an existing owned track uses the same envelope; only the manifest handling differs. The manifest must request exactly the own track's artwork (part 2); the client answers with the empty-path `FileComplete` after staging the bare image at `Airlock/Media/Artwork/<PID>` and reading it back, and refuses the foreign requests.

## 16. Differences over Wi-Fi

The ATC protocol is unchanged over TCP. CONFIRMED. The same messages, framing, session numbers and asset exchange work over a Wi-Fi connection, and the operation file and AFC steps are identical. Transport details (pairing record use, Bonjour, TLS, sleep behaviour, transfer speed) are in [chapter 3](03-transport-usb-and-wifi.md). The protocol-visible differences are:

- The `com.apple.atc` service, as well as `com.apple.mobile.assertion_agent`, closes the connection on the first message when the host has no `com.apple.mobile.heartbeat` connection open. With a heartbeat open, Ping gets Pong and the startup messages. A test without a heartbeat concluded that the phone refused a `PowerAssertion` message; that conclusion is superseded, because the missing heartbeat was the cause.
- Apple's host takes a power assertion during Wi-Fi sync. It sends `{Command: PowerAssertion, Session: <current>, Params: {Enabled: <bool>}}` on the ATC link (DLL `0x31b80`, log `airtraffic> sending PowerAssertion %s`), and a separate `com.apple.mobile.assertion_agent` request of type `AMDPowerAssertionTypeWirelessSync`. With a heartbeat held, the phone answered the assertion service's `CommandCreateAssertion` with `{}`. Whether the ATC `PowerAssertion` message keeps a locked phone awake is OPEN.
- The Linux client does not send `PowerAssertion`.
- Message order, session numbers and the sidecar are the same over USB and Wi-Fi in the observed sessions.

## 17. Notes for implementers

The following advice is supported by the evidence in this chapter. It describes protocol behaviour, not an architecture. Client-side policy (planning, journaling, recovery) is in [chapter 7](07-sync-engine-design.md).

1. Frame with a little-endian u32 length and a binary plist. Bound the frame size. Keep exactly one in-flight read for the life of the connection.
2. Treat any error or cancellation as the end of the connection. Reconnect and restart the whole session.
3. Send Ping with Session 1 and RequestingSync with Session 2. Record received Session values and do not require equality with what was sent.
4. Read the phone's current Media anchor from `ReadyForSync` and use that value. Stop on a mismatch with the client's expected anchor. Never fall back to Reset.
5. Send the existing library identity in `LibraryID`. Never generate a new one.
6. Write the operation file and sidecar exactly as serialised, then read both back before `FinishedSyncingMetadata`. Keep the staging directory empty before and after the session. Do not assume atomic create or rename over AFC.
7. Put `update_db_info` first, with the phone's existing database and primary container PIDs.
8. Send only the operations you need. Apple's extra statistics and artwork updates are not required.
9. Allocate new PIDs from the full 64-bit space and check collisions against everything read from the phone and everything the client has reserved.
10. Wait for the manifest, validate every entry before replying, and complete your own audio before sending any refusals. Refuse nothing that you must keep.
11. After the manifest, answer the phone's Ping with Pong and ignore Progress.
12. Treat `SyncFinished` for the current session as the only terminal. Then verify the permanent file by reading it back.
13. Over Wi-Fi, keep a heartbeat connection open before opening `com.apple.atc`.
14. Do not use `delete_album` or `delete_artist`. Use `delete_playlist` only for playlists known to be removable and empty or ordinary.

## 18. Open questions and facts not confirmed

1. OPEN: whether the phone requires the `.cig` sidecar. Only signed files were sent.
2. OPEN: Apple's signing algorithm and Grappa exchange (`0x248b0`, `0x11110`, `0x3430`). The Linux client uses a different, public fixed-request calculation that the phone accepts. The calculation does not reproduce Apple's own sidecars.
3. OPEN: the meaning of byte 18 of the 84-byte request (fixed request `0x04`, Apple's `0x01`) and of the first two bytes (`01 01`) and byte 18 (`0x40`) of the Ready blob.
4. OPEN: why the phone chooses Session 2 for its Pong, and the meaning of the `Type` field.
5. OPEN: what a wrong `LibraryID` triggers. The phone's code compares it only with a sync already in progress (section 7.2). Only the existing identity was sent in Music sessions.
6. OPEN: the meaning of `VoiceMemo: "1"` in the request, the role of `Book` and `Keybag` in the dataclass list, and whether a Music-only request is accepted.
7. OPEN: the contents of the `Metadata` and `Anchor` fields of `FileComplete`.
8. OPEN: what `upload_track` and `update_playlist` do on this phone, and whether any add-member or remove-member operation exists.
9. OPEN: the per-device switch that controls album and artist operations (`[handler+0x64]`).
10. OPEN on the wire: the phone's failure messages. The parameters and codes of `SyncFailed` are known from the phone's code (chapter 09, section 6.3); none was observed.
11. OPEN: whether files above 500 operations per file (the host's chunking) behave as a multi-file sequence.
12. OPEN: how the phone assigns persistent IDs to items it ignores, and whether PIDs below 1000 or with other structure are rejected. Random 64-bit values worked.
13. OPEN: when the phone sends its own `Ping` mid-session and how often.
14. OPEN: whether the host's lock on `com.apple.itunes.lock_sync` is a requirement for any Music sync. INFERRED that the Linux client takes no lock; no recorded session took it and the live order of lock and notifications was never captured.
15. INFERRED, not tested: the `ATSyncPrepOnly` (3) sync type. The chunked operation-file branch is CONFIRMED in the host's code (section 8.1). How the phone handles it is item 11.
16. OPEN: what the phone does with new audio for an existing PID (section 9.7).
17. INFERRED: the 1,024-entry manifest bound is a client limit. The phone's own maximum was never observed.
18. OPEN: the receiver's behaviour for a `location` dictionary inside `update_track` (section 9.7). It was never sent.

## 19. Evidence

This chapter rests on five kinds of evidence. Nothing here depends on a file that is not described below.

### 19.1 Linux sessions against the phone

Device: iPhone17,2, iOS 27.0.1, build 24A446. A Linux client spoke to the phone over USB through usbmuxd, and also over Wi-Fi. Raw `com.apple.atc` frames, AFC requests and replies, the phone's syslog and copies of its Music database before and after each session were kept.

- Passive capture of the four startup messages (section 4).
- First Ping and Pong with Session 2, which corrected the session-echo assumption of section 5.2.
- The first Media handshake; the first signed insert, stopped by the client's own manifest guard (section 14.4); the audio retry, which completed the insert of a one-track ALAC fixture; the artwork update of that track.
- Playlist creation with one and two members; single-track deletion; re-add of a deleted track (section 12.6).
- 41 signed metadata-only sessions (titles, track numbers, albums, artists, with restorations); the first batch deletion; batch inserts with and without covers.
- More batch inserts of ALAC and MP3 tracks; the phone's own Ping (section 5.4); sessions killed at chosen points (section 14.3).
- Clear and restore of a 399-track library in batches of 100.
- Playlist deletion (`delete_playlist`, six `delete_album` and `delete_artist` sessions); the first Wi-Fi sessions (section 16).

### 19.2 Windows traces of Apple's own host

Apple Devices 1.1540.23042.0 (AMPDevicesAgent 1.6.4.90) ran in a Windows virtual machine with the phone attached. A Frida observer recorded the dictionaries passed to the host's send functions and the files it staged.

| Session | What it recorded |
| --- | --- |
| Reset reference session | Reset sync of the whole library: 401 staged operations (`update_db_info`, 399 `insert_track`, one `insert_playlist`), a 54-entry manifest, the 21 asset completions and 6 errors that preceded the cutoff. Media anchors 7 (host) and 8 (device). The collector reached its 4,096-event cap about 12 s after the Sync input. |
| Delta reference session | Delta sync after Apple's host imported one new ALAC track (Track G). Anchors 9 and 9, revision 10, `SyncTypes {Media: 1, Book: 0, Keybag: 1}`; a 21,916-byte operation file with one `insert_track`, one `update_db_info` and 400 `update_track`; the 21-byte sidecar; 247 distinct `Progress` messages. The cap was reached about 9 s after the Sync input, before any manifest or completion. |
| Uninstrumented connection | One uninstrumented connection, used to confirm that the host survives without the observer. |

The traces hold the host's message dictionaries, not framed wire bytes. Both runs were cut short by the event cap, so the final messages of each session were not recorded.

### 19.3 Static analysis of Apple's binaries

AMPDevicesAgent.exe 1.6.4.90 and 1.6.4.94, AirTrafficHost.dll 4025.400.1.5 and 4026.110.1.6, and MobileDevice.dll 1827.100.17.2, with the sizes and SHA-256 values in section 1. Disassembly and string tables were read directly. Several conclusions were checked by scripts that pin individual instructions (16,861 and 9,106 retained instructions with 49 and 104 explicit pins for the existing-audio and audio-change-selection studies of section 9.7). Build 1.6.4.94 was compared with 1.6.4.90 instruction by instruction at the signing seam (section 11.1). The operation-file chunking of section 8.1 was read in both builds. No Apple code was executed outside Apple's own process.

The phone's side of the version rule, the `LibraryID` comparison and the `SyncFailed` codes was read in the iOS 27.0.1 and 27.2 firmware. Chapter 09, section 13 describes that evidence.

### 19.4 Phone database and file captures

Read-only copies of `MediaLibrary.sqlitedb` and its write-ahead log, taken over AFC before and after sessions, compared across all 45 tables; reads of the permanent audio files and covers by exact size and hash; the `iTunesCDB` and `ArtworkDB` host files on the phone. Chapter 4 and chapter 5 describe the schema and the receiver effects that these captures show.

### 19.5 Public third-party source

The `go-tunes` project at commit `3ee38393877a8adc5f5a928ddc834ee26bf5512b` supplied the fixed host request and the signing calculation of section 11.4. It was compiled and checked on Linux and reduced to standard primitives. It is not redistributed here.

### 19.6 Strength of the evidence

- The wire sequence, the operation shapes of sections 9 and 10, the manifest, completion and error messages and the interruption table were observed on the phone.
- The operation names, builder arguments, state machine, anchor selection and sync lock were read from code and, where marked, matched to Windows traces.
- The signing calculation is observed to work and is not Apple's algorithm.
- Items marked INFERRED or OPEN in sections 5 to 18 were not observed.
