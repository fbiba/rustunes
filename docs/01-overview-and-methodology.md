# Reverse engineering Apple's Music sync for iPhone: overview, methodology and provenance

## Abstract

The stock Music app on an iPhone can be filled only by Apple's own hosts: Apple Devices on Windows and Finder on macOS. No Linux tool did this. The project set out to build one, as a native Rust program that adds, updates and removes tracks in the phone's own Music library over USB and over Wi-Fi.

The work took place on one iPhone 16 Pro Max (ProductType `iPhone17,2`, iOS 27.0.1, build 24A446, later updated to iOS 27.2, build 24B119). It combined five methods:

1. passive observation of what the phone says on its own;
2. dynamic instrumentation of Apple Devices for Windows running in a virtual machine;
3. static analysis of the Apple Devices binaries;
4. controlled experiments from Linux against the phone, each verified by comparing the phone's `MediaLibrary.sqlitedb` before and after;
5. static analysis of the phone's own firmware for both iOS builds, after the update changed the version of the phone's database.

The principal result is that the phone accepts a Linux client that speaks the same AirTrafficConnect (ATC) session as Apple Devices, stages a signed operation file and the audio over the Apple File Conduit (AFC) service, and then lets the phone's own receiver register the tracks. The phone applies exact, observable rules when it does so. Those rules are the substance of the later chapters.

This chapter is the method and the history. It states what was observed, how, with which tools, what each method could and could not show, and in what order the findings arrived. It also names the third-party sources consulted and the limits of what is published. Protocol, transport, database and format results are summarised in one paragraph each and are developed in chapters 02 to 06. Chapter 07 describes the design of the CLI built on them. Chapter 08 describes how Apple's host batches a sync, and chapter 09 how an iOS update changes the library database.

Evidence labels used throughout this document set:

- **CONFIRMED**: seen directly in code, constants or data flow of the Apple binaries, in real captured Apple traffic, or in the phone's database or files after a controlled operation.
- **INFERRED**: reasoned from names, strings, analogy or partial evidence, not seen directly.
- **OPEN**: not established. Stated so that a reader does not assume it.

## 1. Problem statement and prior work

### 1.1 The problem

Apple's documentation describes Music synchronisation as a function of Apple Devices (Windows) and Finder (macOS). The reviewed Windows guides offer two modes: synchronise the entire library or selected artists, albums, genres and playlists, and manually manage music. Deselecting previously synchronised music and applying removes it from the phone. The guides do not describe an additive, single-file operation that preserves an existing synchronised collection. CONFIRMED as a documentation finding. Whether the Windows client's behaviour matches its documentation was then established by experiment.

The goal was therefore:

- put local audio files into the stock Music app on the phone, with metadata, artwork and playback working;
- do so incrementally, without disturbing tracks already on the phone;
- remove and update tracks later;
- run from Linux, with no Apple software on the host.

### 1.2 What existed

Five public code bases were examined. None supplied a Music implementation. The table lists what each contributed.

| Source | What it is | What it contributed | Why it was not enough |
| --- | --- | --- | --- |
| libimobiledevice (pinned at commit `fa0f79190142bc309307967c058f89c1b36eb6b8`) | C library and tools for iOS devices | A reference for AFC modes and locks and for the `com.apple.itunes.lock_sync` and `syncWillStart`, `syncLockRequest`, `syncDidStart` notifications used by `idevicebackup2` | It implements backup and restore. It has no ATC client and no Music sync. Only three headers and `idevicebackup2.c` were saved as reference. |
| `idevice` (Rust crate, version 0.1.68, source commit `d32c8189c51c2789496b0768039419c3705498c3`) | Rust implementation of usbmuxd, Lockdown, AFC, notification proxy and related services | The transport and service layer of the project: pairing records, Lockdown sessions, service start, AFC open/read/write/lock, and the framing used to talk to `com.apple.atc` | It provides services, not the Music protocol. The ATC messages, operation files and signing had to be found elsewhere. |
| AirCard-Linux (commit `7686fd21e3e598d2217c5e6da88229cd1bca4e07`) | Linux tool that uses ATC to push Books content | The first working example of ATC framing from Linux: little-endian 32-bit length followed by a binary property list; the shape of a host session (announce dataclass, send library identity, finish metadata) | Books only. It selects a published request token and inserts it into a hardcoded Books request. It has no host peer response, session key or operation signing. |
| AirCard-Windows (commit `35e891bdf24251a7769d360aa402a51ec257670a`) | Windows tool that drives Apple's `AirTrafficHost.dll` | Which `ATHostConnection*` entry points exist and in which order a host calls them; this fixed the first list of seams to instrument | Books and an unrelated filesystem exploit. It calls Apple's DLL and so hides the wire envelope, session values and signing. |
| go-tunes (commit `3ee38393877a8adc5f5a928ddc834ee26bf5512b`) | Go program with a C++ implementation of the `.plist.cig` sidecar calculation for ringtone sync | The algorithm lead for the 21-byte signature sidecar, and a fixed 84-byte host request that makes the phone answer with an 83-byte Grappa value | It targets ringtones, and its host identity, zero anchors and unrelated database operation cannot be replayed on a real library. Its snapshot carries no licence file. |

Two further public projects, AirCard-iOS and AirLift, were read as web pages only and were not downloaded. Their helpers return client-request bytes from Apple's private `ATGrappaSession` or a published token. Neither exports host state or a signer.

CONFIRMED: a bounded source inspection found no reusable Linux-compatible producer of Music operation `.plist.cig` files in the AirCard and AirLift sources. A targeted search across the saved AirCard-Linux project (17 Rust files) and `idevice` (133 Rust files) corroborated this. That is a statement about the files searched, not about all public implementations.

Consequently the Music operation format, the session's anchors, the signing of the metadata operation file and the receiver's rules all had to be recovered from Apple's own software and from the phone. go-tunes later supplied only the arithmetic core of the signature, and that core was reduced to standard primitives (section 3.6 and chapter 02).

### 1.3 The approach that follows from this

Apple Devices for Windows is the only available complete implementation of the Music host. Treating it as a reference leads to three activities:

- watch it work (dynamic instrumentation of the running process);
- read it (static analysis of its binaries);
- compare what it does with what the phone's database shows afterwards.

A Linux client built from those findings is then tested against the same phone, one operation at a time, using the same database comparison.

## 2. Environment

### 2.1 Hosts and device

| Item | Value |
| --- | --- |
| Linux host | Fedora, kernel 7.2.8-200.fc44.x86_64 at the start (7.2.10 later), x86_64 |
| Toolchain | Rust and Cargo 1.93.0 |
| Device library crate | `idevice` 0.1.68 |
| USB multiplexer | system `usbmuxd`, socket `/run/usbmuxd`; no `libimobiledevice` command-line tools were installed (`idevice_id`, `ideviceinfo`, `idevicesyslog`, `idevicepair` were absent from PATH) |
| Windows reference host | Windows 11 guest under QEMU, launched outside libvirt (quickemu configuration); QEMU guest agent and human monitor (HMP) sockets available |
| Phone | iPhone 16 Pro Max, ProductType `iPhone17,2`, iOS 27.0.1, build 24A446 (later updated to iOS 27.2, build 24B119), USB id `05ac:12a8` |
| Initial phone library | 398 Music items, 8,346,488,832 bytes (aggregate from the phone's `AssetMetrics`) |

All values are CONFIRMED unless otherwise marked.

### 2.2 The Windows reference installation

The reference was the Microsoft Store package Apple Devices, installed as `AppleInc.AppleDevices_<version>_x64__nzyj5cx40ttqa`. Apple Music for Windows is the package `AppleInc.AppleMusicWin`. (An early inventory query for `AppleInc.AppleMusic` returned nothing. That negative result was an incorrect package name, not evidence of absence, and it was corrected.)

Two builds of Apple Devices were examined.

| | "old" build | "new" build |
| --- | --- | --- |
| Package version | 1.1540.23042.0 | 1.1540.24088.0 |
| `AMPDevicesAgent.exe` | 1.6.4.90, 12,409,712 bytes, SHA-256 `ad957f32107b7177969b3cb11b91e3922d27e622b53f9240b65cb457522111f9` | 1.6.4.94, SHA-256 `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9` |
| `AirTrafficHost.dll` | 4025.400.1.5, 322,000 bytes, SHA-256 `0d38b01c21d57a9f3ca32c15d4b2b8aa0557d43a724b4745f1fb122322f4eecd` | present (the instrumentation pins a different SHA-256 for it) |
| `MobileDevice.dll` | 1827.100.17.2, 4,020,688 bytes, SHA-256 `de08e3324f24dbcbc0597b78f797670f18f53faa58819cda3c32fa3cebcb9d39` | not saved |

The Windows host was updated from the old build to the new one during the work. Helpers written against the old build had their address pins invalidated at that point (the failed title-update capture of section 3.2.1). Static analysis was done on the old build and cross-checked on the new one (section 3.3).

The components relevant to Music:

- `AppleDevices.exe` is the UI.
- `AMPDevicesAgent.exe` is the background agent that performs device sync. It builds the per-track operation dictionaries, writes the operation files, signs them and answers the phone's asset requests.
- `AirTrafficHost.dll` is Apple's ATC host library.
- `MobileDevice.dll` is Apple's device library, loaded by the `AppleMobileDeviceProcess` support process. It also contains the Wi-Fi paths studied in the last phase.

The Windows guest was driven through the QEMU guest agent (files, process queries, command execution), through HMP (keystrokes and framebuffer screenshots) and through a SPICE viewer. The Music source files were exposed to the guest through a read-only SPICE folder share of the Linux Music directory. The iPhone was passed through as a QEMU `usb-host` device (`05ac:12a8`). Moving it between the guest and Linux was done by `device_del` and `device_add` against the device's recorded QOM path, with a restoration command saved first, and one physical re-plug was sometimes needed for Linux to rediscover the phone.

### 2.3 Reference library and safety setup

The device owner's library contained several hundred ALAC `.m4a` files. Some local tags had been edited after earlier synchronisations, so the local directory was not a verified copy of what the phone held. This was recorded as a source-baseline correction. Consequences for method:

- the Windows host was kept in the Apple Devices default "entire library" selection and its bitrate conversion option stayed off, so a reference synchronisation transferred existing tracks unchanged;
- no experiment removed existing phone content; physical checks (visible and playable) were made after each risky step;
- individual reference tracks were chosen by the owner and hashed (size and SHA-256) before any use.

### 2.4 Instrumentation toolchain

- Windows guest: an application-local Python 3.13.16 (embeddable x64) and Frida 17.22.2, installed from official sources, with no `frida-tools`, server, network forward or global install.
- Linux: the project's own Rust crates, plus the `ffmpeg`, `ffprobe` and `lame` programs for audio checks, and Python (run in isolated mode) for the offline tooling of sections 3.3 and 3.7. The firmware of section 3.7 was read with `ipsw` 3.1.739 and 7-Zip.

## 3. Methods

### 3.1 Passive observation (Linux)

**What was done.** Linux connected to the phone through `usbmuxd` using the pairing record saved when the phone was first paired. It opened a Lockdown session, started `com.apple.atc` and read the frames the phone sends unprompted. It sent nothing. Filtered syslog (`com.apple.syslog_relay`) was captured alongside.

**What it yields.** CONFIRMED (passive capture): four binary-plist frames with little-endian `u32` length prefixes arrive unsolicited: `Capabilities`, `InstalledAssets`, `AssetMetrics`, `SyncAllowed`. Envelope keys are `Command`, `Session` (0), `Params`, `Type` (0) and an incrementing `Id`. The phone advertises Grappa support (`version 1`, `deviceType 0`, `protocolVersion 1`). `AssetMetrics.Media.Music` reports the aggregate count and size. `SyncAllowed` advertises ManualSync and AutoSync and lists the disabled asset types (Photo, Movie, TVEpisode, HomeVideo).

**Limits.**

- The messages contain only aggregates. They do not inventory individual tracks.
- `InstalledAssets` contained only an empty deleted-application list.
- The syslog was noisy and produced no semantic evidence about Music sync.
- Linux saw no device through `usbmuxd` until the owner accepted the Trust prompt and physically re-plugged the cable. The host service had to rediscover the device itself, and no restart was performed.

Later runs used the same channel actively: after a Ping, the phone replies with Pong and repeats its startup messages. A diagnostic correction was needed here. An early check required the Pong to echo the host's session number. It does not. The host library does not compare Pong's session either (the first Ping and Pong sessions).

### 3.2 Dynamic instrumentation of the Windows host (Frida)

**Purpose.** Capture what Apple Devices sends and receives during a real synchronisation, at the level of parsed objects and staged files, because the wire frames themselves are not visible from the Windows side.

**Tooling** (written for this project; none is a production dependency):

- a Frida script that installs nothing until a collector arms it. Armed, it observes without replacing functions or altering arguments: `SendHostInfo`, `SendSyncRequest`, `SendMetadataSyncFinished`, `SendAssetCompleted`, `SendAssetCompletedWithMetadata` and `ATHostMessageLinkSendMessage` in `AirTrafficHost`; the `AirTrafficHost` incoming callback (complete parsed message, before command and session filtering); and in `AMPDevicesAgent` the `ReadyForSync` and unwrapped `AssetManifest` callbacks, the operation-file writer and its operation array, the raw signature writer (a copy of the produced bytes and filename, with the native return status as a separate record), and the ordinary Media signer entry (session id and operation length only). CoreFoundation objects are serialised to temporary binary plists by calling the loaded CoreFoundation, and the script releases only its own copies;
- a guest-local collector that attaches, arms, writes events and attachments to a new private directory, enforces the time and event budget, and records why it detached;
- a process-lifetime monitor that is a separate read-only baseline. It uses no Frida and no device calls. It pins the target's process id, exact start time, installed package and SHA-256, and the current Application event record id, then samples liveness for 120 seconds;
- offline seam checkers that run the real guarded hooks against fixtures and check, among other things, the 32-byte instruction fingerprints at each hook address against the pinned binaries; compare the current hook source against preserved earlier versions and replay what a bounded capture would record; and keep a regression for the function-import resolver (see below);
- tests of the collector's callbacks, guards and cleanup against a fake Frida runtime.

#### 3.2.1 The crash problem

Apple Devices' `AMPDevicesAgent` crashed repeatedly while instrumented. CONFIRMED observations, each with a different fault signature:

| Session | Fault | Context |
| --- | --- | --- |
| Idle observer | `c0000005` in `ntdll.dll` offset `164a57`, about 86 s after an idle armed observer | No Music activity. An exact replay did not reproduce it within the same window. |
| One-file import and reference Sync | `c0000005` in `frida-agent.dll+0x9f7c7`, about 0.94 s after the first Sync input | Six host-call attachments (758 bytes) captured. |
| Marker diagnostic Sync | `frida-agent.dll_unloaded+0x9c7e0`, then a UI fault | The 4096-event budget was exhausted first. |
| Title-update capture | `c0000005` in `ntdll.dll+0x165497`, about 0.8 s after USB handoff, with the new build | Only the armed and detach records were written. No Sync input had been sent. |

No call stack or dump survived in any case, because the temporary dumps were absent from the preserved Windows Error Reporting reports. The causes are OPEN. Timing correlations were recorded as correlations. A subsequent uninstrumented connection survived its monitor window, which shows the host is not unconditionally unstable on connection but does not explain the instrumented failures.

One bug was found and fixed on the way. A zero-hook self-test failed because `CFStringCreateWithCString` returned NULL. The cause was an import resolved by slot: Frida's reported slot `AMP+0x8239b0` pointed at `CFDateCompare`, not at the named import. Resolving the function by name from the loaded CoreFoundation module fixed the scratch serialisation, and the import-resolver regression keeps it. This supports address selection as the cause of the scratch failure. It does not explain the crashes above.

#### 3.2.2 The bounded 120-second monitoring discipline

Because instrumentation and the host's own faults were hard to tell apart, every risky step was bracketed by read-only monitors that never attach to the target:

- a monitor pinned to the exact process id and start time, sampling for 120 seconds (121 samples), with exit code 0 (GREEN, no matching fault event), 1 (RED, a new access-violation event matching the pinned process) or 2 (INCOMPLETE: target missing, different failure, or an event-query bound);
- a closed `ready.json` written only after the first successful liveness poll, so no experiment started against an unready monitor;
- separate monitors for startup, for the Sync and for after the Sync, whose windows are never merged. A GREEN window says nothing about time outside it. For example, the title-update capture's monitor completed GREEN about three minutes before its crash and was explicitly not credited with covering it;
- a hard event budget of 4096 records in the collector, and a fixed collector deadline that was not extended after being reached;
- one input per experiment (one Open, one Sync Enter), sent by one actor only after fresh process, module, hash, setting and screenshot checks;
- a physical check on the phone after each attempt: previous music still present and playable.

#### 3.2.3 What the captures delivered, and what they did not

Succeeded (CONFIRMED):

- the host-call sequence for a Music Sync: Ping, SyncStatus messages, then `ATHostConnectionSendSyncRequest` with dataclasses `Media`, `Book` and `Keybag`, an anchors dictionary and iTunes HostInfo with Music selected (the first reference Sync);
- the original operation dictionaries and the operation file written for a real Delta sync: in the delta reference session (Track G) the staged operation file was 21,916 bytes and contained one `insert_track`, one `update_db_info` and 400 `update_track` operations, with no deletion and no playlist operation. Request and Ready anchors were both 9, and the new revision 10, with SyncTypes.Media 1 for Delta;
- complete incoming `ReadyForSync` and `AssetManifest` objects;
- an opaque raw signature sidecar. The delta reference session produced 274 attachments (273 plists and one typed raw signature blob, 84,187 bytes in total) before the 4096-event budget ended the trace. The sidecar's byte shape (21 bytes) was learned from it;
- Apple's own full Reset from the staged data of the reset reference session: one `update_db_info`, 399 `insert_track` operations and one primary playlist.

Never succeeded:

- no trace ever recorded the signer's inputs, key material or intermediate values. The signer was entered (session id and length were observed) but its algorithm was not captured at runtime;
- no trace reached the end of an asset transfer and the terminal acknowledgements. Every full-sync trace ended at the event budget or at a fault before the tail;
- The title-update capture and the title update on a connected phone produced no accepted title-update exchange. The second was stopped because a navigation Enter was sent without verifying its target. Neither counts as evidence.

The terminal parts of the protocol were therefore reconstructed from static analysis and from controlled Linux experiments (sections 3.3 and 3.4).

### 3.3 Static analysis of the Windows binaries

**What was done.** The Apple binaries were copied out of the guest and read offline. No Apple code was executed by this method. The products were:

- full disassembly (`objdump -d -M intel`) of `AMPDevicesAgent.exe` and `AirTrafficHost.dll`;
- PE headers and `.pdata` function tables;
- string dumps;
- `amp-names.json`, a map from CoreFoundation string constants to their addresses;
- hand-annotated excerpts, and the offline tooling described below.

**Address convention.** All addresses cited in the chapters are virtual addresses in the `0x1400xxxxx` image of `AMPDevicesAgent.exe` (image base `0x140000000`). `AirTrafficHost.dll` and `MobileDevice.dll` use image base `0x180000000`. Where an RVA is given it is the VA minus the image base. Offsets in the string dump were file offsets, and `0x1400` was added to obtain the `.rdata` RVA. Function names such as `sub_<RVA>` are project labels, not Apple symbols.

"Old" always means build 23042 (`AMPDevicesAgent.exe` 1.6.4.90) and "new" means build 24088 (1.6.4.94). Functions were located in the new build by a shape hash and then compared instruction by instruction with absolute addresses removed. In the MP3 gapless region every function examined is 100 % identical in both builds, and the whole region is displaced by a constant: for example the Xing/Info/VBRI parser is at `0x14041cf50` (old) and `0x14041d074` (new), a shift of `+0x124`, while the iTunSMPB/iTunNORM text parsers shift by `-0x4` and the COMM-frame tests by `-0x44`. Chapter 06 gives the addresses it needs. Other chapters give old addresses unless stated.

**Offline tooling** (written for this project, Python run in isolated mode, none loads or executes Apple code):

- a PE reader and annotator that parses the sections of `AMPDevicesAgent.exe` and labels the disassembly with CoreFoundation string names and ASCII strings read from the binary;
- a fast range printer over the full disassembly, using the same annotation, addressed by virtual address;
- a function-bounds finder that reads the `.pdata` RUNTIME_FUNCTION table to give the begin and end of the function containing an address;
- a cross-build address mapper that matched functions of the old build to the new build by a shape hash (mnemonics and register operands, with numeric literals, displacements and branch targets stripped) and reported unique matches or the candidate list;
- a survey tool that read captured `insert_track` and `update_track` plists and counted item keys per operation, including sort fields and comment-like keys;
- a read-only dumper of the `ilst` text atoms of M4A files (title, artist, album, album artist, composer, comment, lyrics, the sort atoms, copyright, genre) as JSON lines;
- an offline model of how `AMPDevicesAgent.exe` 1.6.4.90 derives the MP3 gapless fields sent in `insert_track.avformat_info`, written from static analysis only, in which every place where the binary reads a value that only a real decoder pass could confirm is marked as an assumption;
- a generator of annotated excerpts for the MP3 gapless region.

**Annotated excerpts** generated from the disassembly covered five areas:

- the sync decision code around `0x140468654`, used for the container deletion and Reset analysis;
- the per-track dictionary builder at about `0x140461900`, which decides which item keys are emitted;
- the track record accessor code around `0x14045b061`;
- every function involved in the MP3 gapless fields;
- the old-to-new address map for that region.

**What static analysis yielded.** CONFIRMED (selected, with the chapter that develops each):

- the operation dictionary shapes (`insert_track`, `update_track`, `delete_track`, `update_db_info`, `delete_playlist`) and which item keys the builder emits (chapter 02, chapter 05);
- how the operation file is wrapped and where it is staged, how anchors and sync types are initialised, and the Ready response and asset identifiers (chapter 02);
- the supported-format table and the conditions that select each `audio_format` (chapter 06);
- the MP3 gapless computation: delay, drain, duration, resynchronisation and the tail-silence heuristic, down to its float constants (chapter 06);
- the comment and five sort fields and how they are taken from source tags (chapters 05 and 06);
- the Wi-Fi transfer strategy of the Windows host (1 MiB transfers, which hash calls it makes and when) and its method for deciding that a file on the phone is intact (chapter 03);
- that the host builds `delete_playlist`, `delete_album` and `delete_artist` as bare `{operation, pid}` dictionaries (chapter 05).

**Limits.**

- Static results describe what the code can do. They do not prove the phone accepts it. Each such claim was paired with a phone experiment (section 3.4) before it was used in the product, and the pairing is recorded.
- Registers and call arguments cannot be inspected for values that exist only at runtime. Where the binary reads a decoder result, the dependence is marked in the chapter.
- The static work has no source for symbol names. Function labels are project inventions.
- An early assumption that two builds are structurally equal had to be proved function by function (above) rather than assumed.

**Not published.** The Apple binaries (`*.exe`, `*.dll`), the full disassembly, PE and string dumps, the annotated excerpts and the captured Windows traces are Apple's code or derived from it and from the owner's library. They are retained privately and are not distributed (section 10). The published chapters give addresses, constants, field names and conclusions only. To verify a claim, disassemble your own copy of Apple Devices 1.1540.23042.0 (or 1.1540.24088.0) with `objdump -d -M intel` and compare the cited addresses. The hashes in section 2.2 identify the exact binaries.

### 3.4 Controlled experiments on the phone

**Pattern.** Each experiment followed the same loop. The loop is what makes the results trustworthy, because the phone's own database is the oracle.

1. Fix a question and a scope: one fixture track, or one small operation, or a known batch.
2. Capture the phone's `MediaLibrary.sqlitedb` and its write-ahead log (`-wal`) read-only over AFC, before. Hash the copy and check SQLite integrity.
3. Build the operation (insert, update or delete), stage it, run the ATC session, and record every frame sent and received.
4. After the session closes, capture the database and WAL again.
5. Compare all 45 tables row by row, before and after, and attribute every changed row to the operation or to background activity (for example playback counters and the phone's own revision renumbering).
6. Read the files back over AFC and compare them with the source: complete bytes and SHA-256 for audio, extent for covers, plus a stable attribute check.
7. Ask the owner to listen on the phone: the track exists, plays, shows its title and cover, and previously existing tracks are still present and play. Reboot persistence was checked for some runs.

**Fixtures.** The first experiments used real tracks of the owner (about 16 to 24 MB ALAC), each hashed before staging. Later experiments used generated tones and synthetic tracks (including a 2-second artwork-free ALAC, a 20,645-byte and a 41,121-byte MP3, and tone batches), so that a failure or a deletion could not touch real music. Counted survivors were verified in full after each deletion run: for example 425 original tracks and their audio fingerprints, paths and stable attributes after the deletion of two tracks that shared an album with artwork.

**How evidence was retained.** Every session wrote its frames, database copies and hashes into a new directory. Large objects are retained privately (section 10). Verification outcomes are retained as JSON receipts and short checkpoint notes (section 10). Failed attempts were kept next to successes and were not overwritten.

**Limits.**

- One phone, one iOS build for the experiments, and the two Apple Devices builds of section 2.2. Anything about other iOS versions, codecs outside those tested, or the cloud library is OPEN. The update to iOS 27.2 is covered only by static analysis and two database captures (section 3.7); no experiment ran on iOS 27.2.
- The oracle is the phone's catalogue and files, and the owner's listening. A change in a table can be a side effect. Unexplained rows are treated as unexplained, not as noise.
- Some observations depend on timing in the phone (for example the phone later trims a new file's allocation, or renumbers items after a session). Chapter 05 records these.

### 3.5 Evidence labelling and supersession

The raw record was kept chronologically, so an early statement can be wrong and a later one corrects it. The rules used across this document set:

1. A claim is CONFIRMED only for the build, device and profile on which it was observed. Anything beyond is INFERRED or OPEN.
2. A later correction supersedes the earlier statement even when the earlier text is still in the raw record. The chapters use only the corrected statement. Where the correction is instructive it is mentioned (for example the package-name error in section 2.2, the Pong session echo in 3.1, the source-baseline correction of the local files in 2.3).
3. A negative result is a record of what was tried, not a proof of absence. A query that returned nothing, a capture that stopped at the event budget, or a source search of 150 files, are all bounded.
4. Static findings and mock tests never count as device acceptance. UI idle and a green monitor never count as protocol completion.
5. The owner's report ("it plays") is evidence of exactly the checked behaviour on the checked tracks. It does not extend to the rest of the library.

### 3.6 The signer, in short

The one step that no capture and no source delivered directly was signing the Media operation file. The route that worked:

1. An observed Windows session gave the sidecar's shape (21 bytes) and the 83-byte Grappa value the phone returns.
2. go-tunes' standalone C++ calculation had matching lengths. The first comparison did not reproduce the native session's sidecar, which showed the calculation depended on the fixed host context, not only on the operation bytes.
3. The calculation was reduced to standard AES-128-CBC (one block), a recovered fixed key and an input XOR, and HMAC-SHA1. It was reproduced independently in Python and OpenSSL (13 known-answer cases for the SHA-1 helper, 512 independent AES blocks and 32 complete comparisons with the C++ source).
4. A new Rust implementation using standard RustCrypto primitives signs only the complete known fixed 84-byte request and an 83-byte reply with the observed outer envelope. No upstream tables or code were vendored.
5. Whether the phone accepts such a session was settled by experiment (the first Media handshake): it did.

Chapter 02 gives the protocol detail. What remains OPEN is the general algorithm that the Windows host uses for arbitrary sessions. The project signs only the observed profile.

### 3.7 Static analysis of the iPhone firmware

**What was done.** The phone updated from iOS 27.0.1 to 27.2 during the work, and the version of its `MediaLibrary.sqlitedb` changed from 2400060 to 2410000. The phone's own firmware was then read for both builds to learn how the phone and Apple's host treat that version. No Apple code was executed by this method.

- **Download.** Apple publishes iPhone restore images on its update server, and a public index of those images gave their addresses. Only the needed members of each restore archive were downloaded, as byte ranges: the system cryptex, which holds the dyld shared cache with the system frameworks, and the system volume, which holds the daemons, sync bundles and data migrators.
- **Decryption and extraction.** The images are encrypted. `ipsw` fetched their keys from Apple's key service and decrypted them, and 7-Zip extracted files from their APFS file systems. Some cache files use APFS's LZBITMAP compression, which 7-Zip cannot decode. A small decoder written for this work, following apfs-fuse's implementation, decompressed them. It is not part of the project's code (section 7).
- **Reading.** `ipsw` gave symbols, disassembly, strings, cross-references and class dumps of the shared cache and of the executables. Python scripts, run in isolated mode, resolved Objective-C selector stubs, constant strings and constant dictionaries. The two builds were compared by symbol and string sets, by section bytes and by disassembly function by function with addresses removed.
- **Data.** The last read-only database capture on iOS 27.0.1 and the first on iOS 27.2 were compared, and the 27.2 write-ahead log was replayed commit by commit. The Windows binaries of section 3.3 were read again for their handling of iOS and database versions.

**Address convention.** Shared-cache addresses are unslid virtual addresses in the 24B119 cache. Daemon addresses are virtual addresses in their own images. Chapter 09, section 2 gives the details.

**What it yielded.** CONFIRMED, all in chapter 09: the phone's version constant and its exact-equality check; the chain of one-step migrations; the refusal of a newer database and the rebuild of one that cannot be migrated; where `medialibraryd` validates the database; the device side of the sync service and its only version rule (`HostInfo.Version` at least `10.5.0.115`); the `SyncFailed` codes; the events that zero the phone's anchor; and the one change of iOS 27.2, a new index.

**Limits.** The code was read, not run. Nothing in chapter 09 was exercised on the phone, and the order in which the phone's processes act after an update is partly INFERRED. Only the two builds were compared.

**Not published.** The firmware, the decrypted images, the extracted files and every dump made from them are Apple's code or derived from it. They are retained privately and are not distributed (section 10). Chapter 09, section 1 gives the SHA-256 of every file read. To check a claim, download the same builds for iPhone17,2 and disassemble the cited addresses.

## 4. Summary of the technical results

One paragraph per later chapter (07 describes the CLI rather than Apple's behaviour). Each states CONFIRMED results only for the profile tested.

**Chapter 02, the ATC sync protocol** (`02-atc-sync-protocol.md`). A host opens `com.apple.atc` through Lockdown, sends HostInfo and RequestingSync for the `Media` dataclass with the library identity and the saved anchor, receives `ReadyForSync` in a new session, stages an operation plist (and its `.cig` sidecar) under `iTunes_Control/Sync/Media`, finishes the metadata phase, answers an `AssetManifest` by uploading each requested asset, and receives completion. Operations are `update_db_info`, `insert_track`, `update_track`, `delete_track`, `delete_playlist` and others. The phone's anchor advances once per session. A fixed 84-byte host Grappa request and a signature sidecar are sufficient for the Media class. CONFIRMED in every Linux session, from the first Media handshake on.

**Chapter 03, USB and Wi-Fi transport** (`03-transport-usb-and-wifi.md`). The same Lockdown, AFC and ATC services run over USB through `usbmuxd` and over TCP with only the pairing record the phone saved. The phone advertises `_apple-mobdev2._tcp`; since iOS 26.4 it publishes one `authTag` per paired host (an HMAC-SHA256 over the identifier keyed from the host id), and the instance name carries the phone's private Wi-Fi address rather than its hardware address. A locked phone sleeps within seconds. Throughput over Wi-Fi was about 26 MB/s with 1 MiB requests. `afcd` answers up to 64 pipelined stat requests on one connection in order, which Apple's own host never uses. Hashing files on the phone through AFC `GetFileHash` leaks a resource in `afcd`: after about 2,550 requests every open fails until the phone restarts. The project therefore does not request hashes from the phone. CONFIRMED.

**Chapter 04, the media library database** (`04-media-library-database.md`). `MediaLibrary.sqlitedb` has 45 tables. One inserted track writes the usual rows in `item`, `item_extra`, `item_playback`, `item_search`, `item_state`, `item_stats`, `item_store`, `item_video`, `chapter` and `lyrics`, plus collection rows (album, album artist, item artist), sort-map entries and revision bumps. Item ids continue at the previous maximum plus one, and tombstoned ids are revived. The WAL must be read together with the database. Comments and sort names land in fixed columns and the phone strips no articles itself. Readback after each session is built from these rules.

**Chapter 05, receiver operations** (`05-receiver-operations.md`). The phone's receiver applies each operation by rules that were learned from controlled runs: batched inserts and deletes in one session; which item fields regroup albums and artists and which do not; how the "representative" of a collection moves; that emptied albums and artists are matched by name for reuse and are collected only by the phone; that `delete_playlist` works while `delete_album` and `delete_artist` have no effect; and what an interrupted session leaves behind, from kills at chosen phone messages: a kill at the phone's first `Progress` rolls a track or metadata session back, while a later kill leaves it applied (with rows without audio for an unfinished insert).

**Chapter 06, audio formats and gapless** (`06-audio-formats-and-gapless.md`). Apple Devices describes each file by an `audio_format` and a `location.kind`. ALAC and constant-bitrate MPEG-1 Layer III were first, and a later pass covered the other formats the phone advertises (MPEG-2 and 2.5 Layer III, AAC-LC, HE-AAC, HE-AAC v2, xHE-AAC and others). MP3 timing is computed from Xing, Info or VBRI headers with LAME tag fields and a tail-silence search, and when the sender gets this wrong the receiver stores a zero duration (the first generated MP3). Several formats were synchronised and played on the phone, and MP3s sent with the gapless values computed by Apple's rule were stored as sent and played through. Apple Devices sends JPEG and PNG covers unchanged, but the phone keeps no PNG original: it substitutes its own JPEG.

**Chapter 07, sync engine design** (`07-sync-engine-design.md`). How the CLI's journal, planning, verification and recovery are designed, and why: a mirroring `sync` of one library directory, `push` and `pull`, a journal carried forward from the phone across versions, and recovery from every kill point tried. It builds on the receiver rules of chapters 04 to 06.

**Chapter 08, how Apple's host batches a Music sync** (`08-apple-host-sync-batching.md`). Apple Devices sends a whole sync in one ATC session: updates, removals and additions in one operation array, split only into operation files of at most 500 operations. It uploads one file at a time in the order of the phone's manifest, converts on a thread pool working ahead of the upload, and never waits for the phone between files. Removals are bare `delete_track` operations; the phone removes the files. CONFIRMED in the code of both builds, and one session per sync in both reference sessions; how the phone takes several operation files in one session is OPEN.

**Chapter 09, iOS updates and database versions** (`09-ios-updates-and-database-versions.md`). The database version is `PRAGMA user_version`, a constant of the phone's MusicLibrary framework: 2400060 in iOS 27.0.1 and 2410000 in 27.2. The phone migrates an older database in place, one exact step at a time in one transaction. It refuses a newer one, and it truncates and rebuilds one it cannot migrate, which gives the library a new identity and an anchor of `"0"` but the current version. Neither the protocol nor Apple's host uses the version. The phone checks only that the host's `HostInfo.Version` is at least `10.5.0.115`, and Apple Devices never reads the phone's database and treats any iOS above 26.0 as 26.0. iOS 27.2 added one index and nothing else. The update kept the anchor, collected emptied groups and gave every item a new revision. CONFIRMED in the firmware of both builds and in database captures before and after the update; nothing was exercised on the phone.

## 5. Timeline

Entries marked "failed", "abandoned" or "no result" did not produce the evidence they were designed for.

### 5.1 Phase A: host inventory and access

| Question | Outcome |
| --- | --- |
| Can a versioned workspace be set up? | First `git init` failed on a read-only sandbox path; the owner initialised it. |
| Can the host reach `usbmuxd`? | Initially refused by the execution sandbox; reachable under approved execution. |
| Is a Windows VM running with Apple apps? | Yes. Apple Devices and Apple Music installed at 1.1540.23042.0. The first negative package query was a wrong name (failed, corrected). |
| Can Linux enumerate and pair with the phone? | After VM release, Trust acceptance and a physical re-plug, Lockdown session opened: `iPhone17,2`, iOS 27.0.1, 24A446. |
| What does the phone say on its own? | Four unsolicited ATC frames; Music aggregate 398 items. See section 3.1. |
| Which fixture tracks are safe? | Fixtures hashed and decode-verified. |
| Can a bounded observer and accessibility inspection be prepared? | Observer attached and detached in an idle test; accessibility report captured; the scratch serializer test failed (see section 5.2). |
| Is the idle observer stable? | Failed: `AMPDevicesAgent` access violation 86 s after arming. Not reproduced on exact replay (section 5.2). |

### 5.2 Phase B: Windows reference captures

| Question | Outcome |
| --- | --- |
| Does the crash reproduce on exact replay? | No, in one 120 s window; no cause. |
| Does the scratch CFString serializer work in the target? | Failed twice: `CFStringCreateWithCString` returned NULL. |
| Why? | Bad import slot; named-export resolution fixed it (scratch plist of 87 bytes decodes exactly). |
| Can one file be imported into Apple Music under observation? | Failed: the picker rejected a forward-slash path. |
| One-file import then one reference Sync | Import worked. Sync crashed the agent; 6 attachments (758 bytes). Phone: test track absent, library intact. |
| Marker diagnostic Sync | Event budget (4096) exhausted, agent crashed. Phone showed the new entry but playback said "not currently available in your country or region" (no audio asset). |
| Reduced markers (the reset reference session) | Budget exhausted again at 1712 attachments; Apple Devices reported the source media for a library track could not be found. Source access repaired (next row). |
| Is the Windows-created phone asset exactly the source? | Yes: 15,284,613 bytes, SHA-256 equal to the source; read over AFC from Linux. A follow-up stat of `Sync_00000000.plist` found nothing at that time (no result). |
| Does one uninstrumented Sync complete? | Yes, to native idle; both 120 s monitors GREEN; owner confirms all tracks play. |
| Capture one insertion (Track G, 17,452,907 bytes): the delta reference session | Import and one Sync input. Trace hit its 4096 budget at 8.8 s: 274 attachments incl. an operation file of 21,916 bytes and a raw signature sidecar. All tracks play. |
| Can Linux ping the ATC service and recognise Pong? | Yes after a diagnostic fix (Pong does not echo the session). |
| Capture a controlled title update (new build) | Failed: agent crashed on USB handoff with `ntdll.dll+0x165497`; no Sync was sent. |
| Does a connection without Frida survive? | Yes, for the bounded window. |
| Title update on a connected phone | Abandoned: a navigation Enter was not verified; no capture. |
| Resumed orchestration and current-state checks | Connected capture refused to send Sync because only 2.05 s of its monitor window remained. Offline signer, anchor and timing work done. |

### 5.3 Phase C: first native Linux session

| Question | Outcome |
| --- | --- |
| Can Linux establish a Media session? | Yes. Ping and RequestingSync with the fixed 84-byte request and the saved library identity and Media anchor 10 returned `ReadyForSync` in session 2, anchor 10, with an 83-byte Grappa reply. No operation was sent. |
| Insert one Linux-created ALAC track | Metadata accepted, then the phone removed the unfinished track when the connection closed before audio (failure recovery, not a delete). |
| Deliver the audio after the interrupted insert | Succeeded; the track plays. Anchor 11. Cover missing. |
| Register artwork on the existing track | Succeeded with `update_track` carrying only artwork cache id and checksum. No audio resent, other rows preserved. |
| Plan for an owned track's update and delete | Plan only; deletions deferred until the owner was present. |

### 5.4 Phase D: insertion and library-level static analysis

| Question | Outcome |
| --- | --- |
| How would Apple replace audio under an existing id? | Static: queued transfers and manifest requests exist; the coalescer keeps delete and insert separate; the receiver trigger for replacement remained unqualified. No execution. |
| Can a full-library rebuild be previewed offline? | Yes, against a 425-track snapshot (Media revision 62). |
| Compilation albums | Two blocked sources got an evidenced explicit-album-artist profile; Linux creation worked; final 427-track readback verified. |

### 5.5 Phase E: artwork

Artwork work started with the artwork registration above and continued in a series of artwork sessions: shared-album covers, permanent cover verification (25 managed covers), recent-skip counters changing during artwork import and a post-deletion artwork collection that exceeded the caller's time budget (an invalid budget, not a receiver timeout). Artwork renditions are covered in chapter 04.

### 5.6 Phase F: metadata updates

| Question | Outcome |
| --- | --- |
| Which fields can `update_track` change, and how to test a title edit? | Static analysis identified scalar fields and dependencies; plan prepared. |
| Title and sort-name change on a generated fixture; public removal | Verified; the fixture was then re-added. |
| Positive track-number change | Track number 1 to 2 at anchor 117 to 118, then restored at anchor 119; verified by SQL, audio and artwork. |
| How does the receiver apply each `update_track` field? | 41 signed metadata-only sessions. Rules for which edits regroup albums or artists; sender recipe. |
| Clearing fields | Same series of sessions. |

### 5.7 Phase G: deletion

| Question | Outcome |
| --- | --- |
| Native single-track deletion | Fixture B deleted and verified; reusable verification built. |
| Deleting tracks that share an album with artwork | Fixture C then D deleted; the catalogue went 426 to 425 tracks at anchor 72; all 425 original audio fingerprints verified. |
| Public explicit delete; re-add | Verified; re-add produced a new track id and path. Interrupted verification recovered. |
| Collection graphs | Eight generated graph fixtures deleted and verified; container-member deletion and full clear unsupported at that time. |
| Several `delete_track` in one session | Batched delete rules; product `delete` and `prune` switched to verified batches. |
| 100-target session; coverless artist representative; two MP3 targets | Group collection and representative movement observed. |
| Container deletion (`delete_playlist`, `delete_album`, `delete_artist`) | Read statically first, then rechecked. On the phone `delete_playlist` works; the other two do nothing. |
| Whole-library `clear`, then restore | The library was cleared through verified removal batches (USB clear 61.8 s) and the 8.2 GB restored in batches of 100 (USB restore 482 s for 398 tracks). |
| An emptied album's stats revision | Recorded; see chapter 05. |
| Removing tracks whose source file is gone | A genre's representative moved to the lowest covered survivor, like an artist's; see chapter 05. |

### 5.8 Phase H: playlists

| Question | Outcome |
| --- | --- |
| Delete one member of a generated playlist | Sent once; the catalogue stayed consistent (427 tracks); reconciled. |
| Playlist creation and member deletion | Playlist Q `[U,V]` created (anchor 115); V deleted at anchor 116; 429 tracks (428 ALAC, one MPEG) all read back. |
| Playlist command and operation reference | Compiled from the sessions above. Same-list and append were not run. |
| Ordinary playlist members in removal batches | Members removable; interrupted clear recovered. |

### 5.9 Phase I: MP3 and other formats

| Question | Outcome |
| --- | --- |
| First generated MP3 (20,645 bytes, 49 frames, 128 kb/s) | Transferred and stored as a file, but the receiver recorded `duration` 0 and `total_time_ms` 0: native timing not established. |
| Positive coded-duration MP3 (41,121 bytes) | Transferred once; the receiver stored the coded duration (see chapter 06). |
| Basic public MP3: ID3 artwork, Info frames, readback, deletion, mixed albums | Narrow profile verified over many checkpoints. |
| MP3 with Info frame and cover; mixed album batch killed mid-upload | Recorded; see chapter 05. |
| Which formats does Apple Devices sync? | Static analysis of the format table; cross-checked on the new build. |
| Sync every format the phone advertises | 13 test tracks in several formats (FLAC among them) played normally, then HE-AAC, HE-AAC v2 and two xHE-AAC tracks played; covers verified; test tracks deleted afterwards. |
| Comment and sort fields; MP3 gapless | Static analysis; the old and new builds identical for the gapless region. |
| Send MP3s with Apple's gapless values, and comments and sort names | Every value stored as sent; Track R re-sent with Apple's exact values and played through into the next track. |
| Which cover images does Apple Devices send, and what does the phone keep? | JPEG and PNG sent unchanged, other types as PNG; the phone keeps any JPEG byte for byte and replaces a PNG with its own JPEG. |
| FLAC sent as ALAC converted on the host | Converted, sent, updated, replaced and removed on the phone; see chapter 07. |

### 5.10 Phase J: Wi-Fi

| Question | Outcome |
| --- | --- |
| Do Lockdown, AFC and ATC work unchanged over TCP? | Yes, with only the saved pairing record (TLS). `MediaLibrary.sqlitedb` (3,149,824 bytes) read in 0.44 s. |
| Find the phone without a daemon | Bonjour plus `authTag` match. |
| Locked-phone sleep | Sleeps within seconds; only specific wake methods work. |
| Faster transfers | 1 MiB requests (about 26 MB/s) and no phone-side hashing; USB restore 482 s for 398 tracks. |
| Hash leak in `afcd` | About 2,550 `GetFileHash` requests exhaust AFC opens until the phone restarts; the project dropped phone-side hashing. |
| Can stats be pipelined? | Yes: 64 in flight on one connection, answered in order; a Wi-Fi restore of 398 tracks took 539.7 s. |

### 5.11 Phase K: static-analysis deep dives

Eight offline studies of the Windows binaries: container deletion, the Wi-Fi transfer strategy, how the host identifies a file already on the phone, the supported-format table, the comment and sort fields, MP3 gapless, the cover image types, and how the host batches a sync (chapter 08). Each used the tooling described in section 3.3, and its findings were then checked against the phone as listed in the phases above.

### 5.12 Phase L: interruptions and upgrades

| Question | Outcome |
| --- | --- |
| What does a kill at a chosen phone message leave? One-track inserts, a cover, ALAC and MP3 batches, metadata edits | A kill at the first `Progress` rolled the session back; a later kill left it applied, with rows without audio for an unfinished insert. Each was settled from a fresh observation without resending; see chapters 05 and 07. |
| Does the next session finish what a killed one left? | Yes: a metadata session finished the keep-local evaluation of an album a killed insert had left; see chapter 05. |
| Can the phone's library survive a new version of the tool? | A journal written under an earlier schema and other hash definitions was rebuilt from a fresh capture of the phone; all 399 tracks carried, and a fresh host bound all 399 to library files; see chapter 07. |

### 5.13 Phase M: an iOS update

| Question | Outcome |
| --- | --- |
| Does the tool still work after the phone updates to iOS 27.2? | No. `sync` stopped at the catalogue read because the database version had changed from 2400060 to 2410000. The read-only captures before and after the update were kept. |
| How do the phone and Apple's host treat the database version? | Firmware of both builds and Apple Devices read statically (section 3.7). The phone migrates its own database, and neither the protocol nor Apple's host uses the version. iOS 27.2 added one index; see chapter 09. |

## 6. Clean-room posture of the implementation

The CLI that results is a Rust reimplementation of observed behaviour.

- It contains no Apple code, binaries, keys, tables or disassembly.
- The signing arithmetic is expressed with standard cryptographic crates and checked with public synthetic vectors. No go-tunes code is vendored.
- It does not load Apple's DLLs and does not depend on libimobiledevice, netmuxd or any system daemon other than the existing `usbmuxd` for the USB path. The Wi-Fi path uses only the pairing record.
- Its verification rests on phone observations (database comparison and complete readback), not on Apple code.

## 7. Safety, ethics and licences

- **Own device and installation.** All work used one phone, one Apple Devices installation and one music library belonging to the author. No third party's device or account was involved.
- **Preservation checks.** Experiments were designed so that existing tracks survived and were checked physically after risky steps. Deletion tests used generated fixtures; real tracks were not deleted by them. Device deletion was barred while the owner was absent and was authorised once the owner was present, for the first single-track deletion. A whole-library clear and restore tested that path with a verified restore.
- **No DRM circumvention.** Only unprotected files from the author's own library were transferred. The project does not touch FairPlay-protected purchases or Apple Music subscription content, and it does not decrypt or strip protection from anything.
- **Apple's protocol check.** The phone's receiver verifies a signature on the Media operation file. The project passes it by reimplementing a published calculation for one observed profile. This is an interoperability reimplementation of an authentication step of Apple's sync protocol. This document describes it as such and offers no legal opinion on it.
- **No Apple code redistributed.** Apple binaries, firmware images, disassembly, string and PE dumps and Windows traces are not in the public repository (section 10).
- **Third-party sources consulted** and the licence facts recorded for each (also in `../ACKNOWLEDGEMENTS.md`):

| Source | Licence recorded |
| --- | --- |
| AirCard-Linux (`shinkuan/AirCard-Linux`, commit `7686fd21e3e598d2217c5e6da88229cd1bca4e07`) | MIT (`LICENSE` file in the examined copy) |
| `idevice` (`jkcoxson/idevice`, commit `d32c8189c51c2789496b0768039419c3705498c3`) | MIT, copyright 2026 Jackson Coxson (`LICENSE.txt` in the examined copy) |
| AirCard-Windows (`Lumid-Off/AirCard-Windows`, commit `35e891bdf24251a7769d360aa402a51ec257670a`) | MIT (`LICENSE` file in the examined copy) |
| libimobiledevice (commit `fa0f79190142bc309307967c058f89c1b36eb6b8`; headers and `idevicebackup2.c` only) | File headers state LGPL-2.1-or-later; reference material only, no code copied, no dependency |
| go-tunes (`yinyajiang/go-tunes`, commit `3ee38393877a8adc5f5a928ddc834ee26bf5512b`) | No licence file in the snapshot; GitHub reported no licence for the upstream, its four direct forks, or one further copy. Used for local analysis only; nothing vendored; its bundled Apple DLL was not executed |
| AirCard-iOS (`Mak5er/AirCard-iOS`, commit `40640c4d827aeab8d798285b57d24fcf6be070d0`) and AirLift (`0xjohnnydev/airlift`, commit `c684cd41ca0ded2d1ab780c15f6ead05509ce062`) | Read as web pages only. No licence was recorded. |
| apfs-fuse (`sgan81/apfs-fuse`; its LZBITMAP decompressor) | GPL-2.0. A private decoder that follows it was used only to read the firmware (section 3.7). It is not part of the project's code and is not distributed |

## 8. Glossary

| Term | Meaning |
| --- | --- |
| Airlock | The phone's staging area for synced content, in the AFC-visible tree under `iTunes_Control/Sync` and related directories. Operation files and signatures for Media are staged there. |
| AFC (Apple File Conduit) | The `com.apple.afc` service. A file-like API (open, read, write, lock, stat, list) over the device's media partition. Used to stage files and to read back databases. |
| `afcd` | The phone-side daemon behind AFC. It has a resource leak with `GetFileHash` (chapter 03). |
| AMPDevicesAgent | `AMPDevicesAgent.exe`, Apple Devices' background agent. Builds operations and drives device sync. |
| anchor | A numeric counter that identifies the last synchronised revision of a data class. The host sends its saved anchor; the phone advances it once per accepted session. |
| ATC / AirTraffic | AirTrafficConnect, the sync protocol carried over the `com.apple.atc` service. `AirTrafficHost.dll` is Apple's host side. |
| AssetManifest | The phone's message listing the assets it wants uploaded in a session. |
| `audio_format` | A numeric format code in an `insert_track` (for example 301 for MPEG-1 Layer III, 502 for AAC-LC). |
| cig, `.plist.cig` | The 21-byte signature sidecar that accompanies a staged Media operation plist. |
| dataclass | A sync category: `Media`, `Book`, `Keybag` and others. |
| Delta, Reset | Sync types for a Media session. Delta (type 1) sends changes. Reset (type 2) resends the whole host library. |
| dyld shared cache | The single file set in iOS that holds the system frameworks, among them MusicLibrary and AirTrafficDevice. Its addresses are the ones chapter 09 cites. |
| Grappa | Apple's host and device authentication exchange for ATC. The host sends a request; the phone returns an opaque value used for signing. Fixed 84-byte request, 83-byte reply in this work. |
| HMP | QEMU's human monitor, used to send keys, take screenshots and move USB devices. |
| Info, Xing, VBRI | Optional first-frame headers of an MP3. Xing and Info carry frame counts and LAME tag fields (delay, padding). VBRI is the Fraunhofer header. In the Windows host's parser only Xing and VBRI count as VBR headers; Info does not. |
| Lockdown | The `lockdownd` service. Authenticates the host with the pairing record and starts other services. |
| `medialibraryd` | The phone's daemon that owns `MediaLibrary.sqlitedb`. It validates and migrates the database and applies the host's operations (chapter 09). |
| PID | A 64-bit persistent identifier of a library entity (track, album, artist, playlist) in the phone database, usually shown signed in operations. |
| pairing record | The credential the host stores when a phone is trusted. Linux reads it from `/var/lib/lockdown/<UDID>.plist`. |
| QGA | QEMU guest agent. |
| representative | The item that stands for a collection (album, artist) in the phone's database. Which item is the representative decides which edits regroup items. |
| rendition | One size or format of a cover image derived from the source artwork and stored in the artwork cache. |
| revision | The database's monotonically increasing change counter, used for sync and tombstones. |
| SMPB | The `iTunSMPB` tag: gapless information written by iTunes. |
| `sync_id` | An identifier the host sets on a track to relate it to its own library. |
| `usbmuxd` | The USB multiplexer daemon that carries TCP-like streams to the phone. |
| `user_version` | SQLite's `PRAGMA user_version`. In `MediaLibrary.sqlitedb` it is the database version: 2400060 in iOS 27.0.1, 2410000 in iOS 27.2 (chapter 09). |
| WAL | SQLite's write-ahead log, `MediaLibrary.sqlitedb-wal`. The live state is the database plus the WAL. |

## 9. Map of the document set

### 9.1 Documents

| File | Content |
| --- | --- |
| `01-overview-and-methodology.md` | This chapter: problem, environment, methods, timeline, ethics, glossary. |
| `02-atc-sync-protocol.md` | Messages, session flow, anchors, operation dictionaries, signing, the staged operation file. |
| `03-transport-usb-and-wifi.md` | usbmuxd, Lockdown, AFC, the Wi-Fi path, Bonjour and the authTag, locked-phone behaviour, throughput, the `GetFileHash` leak. |
| `04-media-library-database.md` | The 45-table `MediaLibrary.sqlitedb`: what each operation writes, revisions, collections, sort maps, artwork cache and renditions. |
| `05-receiver-operations.md` | How the phone applies insert, update and delete, single and batched, playlists and containers, interrupted sessions and recovery. |
| `06-audio-formats-and-gapless.md` | Format table, ALAC and MP3 timing, gapless fields, covers and cover image types, tags, comment and sort fields. |
| `07-sync-engine-design.md` | How the CLI's journal, planning, verification and recovery are designed, and why; its commands, caches and upgrades; its JSON output for other programs. |
| `08-apple-host-sync-batching.md` | How Apple's host batches a sync: one session, operation files of at most 500, the order of updates, removals and additions, upload and conversion scheduling, failure handling, and a comparison with the CLI. |
| `09-ios-updates-and-database-versions.md` | How the phone versions, migrates and rebuilds its database, the other version stamps, the version rules of the phone's sync service and of Apple's host, what iOS 27.2 changed and what the update did to a library. |

## 10. Evidence and retention

The raw traces, database captures, disassembly dumps, Apple binaries, firmware images and the chronological working notes behind this work are retained privately and are not published. The Apple binaries and their derivatives are Apple's code. The traces and database copies contain the device owner's library and device identity.

The chapters are the public record. Each chapter ends with an "Evidence" section that describes the evidence the chapter rests on: its kind (wire capture, database comparison, static analysis, controlled phone experiment), the device and the binary build. A reader who wants to repeat a claim needs their own phone and, for the static results, their own copy of Apple Devices 1.1540.23042.0 or 1.1540.24088.0, or of Apple's iPhone17,2 firmware 24A446 and 24B119. The hashes in section 2.2 and in chapter 09, section 1 identify the exact binaries. Addresses, constants, field names and conclusions are published in the chapters so that a claim can be checked against a disassembly of those builds.
