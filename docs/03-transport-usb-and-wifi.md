# Transport: reaching the iPhone over USB and Wi-Fi

This chapter describes how a host reaches the stock iPhone Music sync services and how it moves bytes to them. Over USB the host talks to the phone through usbmuxd and lockdown. Over Wi-Fi the same lockdown, ATC, AFC, heartbeat and syslog services are reached by plain TCP with TLS, using only the pairing record created over USB. The chapter covers discovery (Bonjour and the authTag), the way a locked phone sleeps within seconds and how a host keeps it awake, the bulk I/O design of Apple's own Windows host (request sizes, connections, pipelining, readback), measured transfer speeds, and a phone-side resource leak in `afcd` that makes hashing on the phone unusable at scale. Message semantics of ATC belong to chapter 02.

## Contents

1. Scope and provenance
2. Evidence labels and conventions
3. Services and how they are started
4. USB transport on Linux
5. Wi-Fi transport
6. Keeping a Wi-Fi phone awake
7. How Apple's host moves files (AFC)
8. Does Apple read back or hash files?
9. The afcd GetFileHash leak
10. Measured transfer behaviour
11. Notes for implementers
12. Open questions
13. Evidence

## 1. Scope and provenance

Phone: iPhone17,2, iOS 27.0.1, build 24A446.

Host binaries analysed (Windows "Apple Devices" app, static analysis with objdump, strings and a small PE parser; nothing was executed for the static findings):

| Short name | Binary | Version | Image base |
| --- | --- | --- | --- |
| MD | MobileDevice.dll | 1827.100.17.2 (built 2026-02-19), from Apple Devices 1.1540.23042.0 | `0x180000000` |
| AMP | AMPDevicesAgent.exe | 1.6.4.90, Apple Devices 1.1540.23042.0 | `0x140000000` |
| AT | AirTrafficHost.dll | 4025.400.1.5, Apple Devices 1.1540.23042.0 | `0x180000000` |
| AMP2 | AMPDevicesAgent.exe | 1.6.4.94, Apple Devices 1.1540.24088.0 | `0x140000000` |
| AT2 | AirTrafficHost.dll | 4026.110.1.6, Apple Devices 1.1540.24088.0 | `0x180000000` |

The newer package (1.1540.24088.0) ships no MobileDevice.dll. Its AMP contains the same key strings as AMP and the same hash call-site structure (section 8), but its copy and connection code was not re-read. Findings about AMP below refer to 1.6.4.90 unless stated. The binaries are not redistributed; this chapter gives only addresses and findings. A `VA` is the objdump address; `RVA` is the VA minus the image base. Function names are the DLL export where one exists, otherwise `sub_<RVA>` (labels chosen by the analyst).

Live observations come from a Linux host (Fedora, usbmuxd running) and the phone, over USB and over Wi-Fi. Section 13 lists the evidence by kind.

## 2. Evidence labels and conventions

| Label | Meaning |
| --- | --- |
| CONFIRMED | Seen on the wire, on the phone, or in disassembled code or data flow. |
| INFERRED | Follows from confirmed facts or from names and analogy, but was not shown directly. |
| NOT FOUND | Searched for statically and absent. This is weaker than proof of absence, because indirect calls cannot be excluded. |
| OPEN | Not answered by the available evidence. |

Where a later measurement corrects an earlier statement, the later one is used and the correction is noted.

## 3. Services and how they are started

All sync work uses services started through lockdown. The services seen:

| Service | Purpose |
| --- | --- |
| `com.apple.atc` | AirTrafficHost Control: the sync message channel (chapter 02). Frames are a 4-byte little-endian length followed by a binary plist. |
| `com.apple.afc` | Apple File Conduit: file access to the media partition (`iTunes_Control/...`, `Airlock/...`). |
| `com.apple.mobile.heartbeat` | Keeps a host registered with the phone's power management; sends `SleepyTime` before sleep. |
| `com.apple.mobile.assertion_agent` | Creates power assertions on the phone. |
| `com.apple.syslog_relay` | Streams the phone's syslog. |
| `com.apple.mobile.notification_proxy` | Posts and observes notifications. |

Startup sequence on both transports:

1. Reach lockdown (TCP 62078 on the phone; through usbmuxd over USB).
2. `QueryType`, `GetValue`.
3. `StartSession` with the pairing record's HostID. The session switches to TLS (not the legacy mode).
4. `StartService` for each wanted service. The reply names a TCP port on the phone and says whether the service uses TLS (`EnableServiceSSL`). Every service in the table above did so over Wi-Fi.
5. Connect to that port and start TLS with the pairing record's host certificate and key.

Facts about the passive ATC start (CONFIRMED over USB): a host that only starts `com.apple.atc` and sends nothing receives four messages, in this order: `Capabilities`, `InstalledAssets`, `AssetMetrics`, `SyncAllowed`. Each has Session 0. After the host sends one `Ping`, the phone answers with `Pong`, `Capabilities`, `InstalledAssets`, `AssetMetrics`, `SyncAllowed` and a second `AssetMetrics`, with a different session number from the passive start (CONFIRMED over USB; see chapter 02). Over Wi-Fi the same exchange works when a heartbeat connection is open (section 6.2).

### 3.1 ATC connections cannot be reused after an error

CONFIRMED (client behaviour). A length-prefixed stream loses its frame boundary if a read is cancelled or fails part-way: the bytes already consumed are gone and the next read starts inside a payload. A client therefore has to treat an ATC connection as unusable after any read error, write error or cancelled read or write, and open a new one. Recovery by "trying again on the same socket" is not possible. The same rule applies to a send whose preparation failed locally.

## 4. USB transport on Linux

### 4.1 usbmuxd and pairing

CONFIRMED. Over USB the host speaks to the system usbmuxd daemon on its Unix socket. usbmuxd lists devices with a `ConnectionType` of `USB` (and `Network` for Wi-Fi devices it knows). A pairing record, created when the phone was first trusted, lives in `/var/lib/lockdown/<UDID>.plist` on this host and is readable by users. It holds the HostID, the host and root certificates and keys, and `WiFiMACAddress`. With this record alone a client can start a lockdown session without any new pairing step.

A Linux host that already had a record needed no new Trust prompt. The phone reported ProductType `iPhone17,2`, ProductVersion `27.0.1`, BuildVersion `24A446`.

### 4.2 Discovery problems seen on Linux

CONFIRMED (brief):

- After a USB device had been handed back from a virtual machine, the kernel and `lsusb` saw the phone (vendor:product `05ac:12a8`, both interfaces with no kernel driver) and usbmuxd was running, but usbmuxd did not publish the device. Its journal showed a `release_interface` failure (`errno=22`) at the moment of the handoff and no later add event.
- Accepting the Trust prompt on the phone did not change this.
- A physical cable reconnect with the phone unlocked made usbmuxd publish the device at once, and the existing pairing record worked. This was repeated after another handoff: the first attempt found no device, the attempt after a reconnect succeeded. No daemon restart was needed in either case.
- A restart of the system usbmuxd service needs elevated rights and was not used.

The underlying cause of the missed re-discovery was not diagnosed (OPEN). The practical rule is: if usbmuxd lists no device although the kernel does, replug the cable with the phone unlocked.

### 4.3 Windows host connection survival

CONFIRMED. A fresh, unmodified Windows Apple Devices host survived an ordinary phone connection for 120 s with no instrumentation and no manual sync. A run with a dynamic-instrumentation observer attached terminated the host about 0.8 s after the USB attach. The cause of that termination was not identified (OPEN).

### 4.4 AFC primitives that matter on any transport

CONFIRMED from reading an open-source AFC client (the Rust `idevice` crate, version 0.1.68):

- Writes are already chunked at 1 MiB and each reply status is checked.
- Open modes are read-only, read-write (creates) and write-only (creates and truncates). There is no exclusive-create mode, so "stat, then open for writing" is not atomic.
- A file handle is not closed on drop. A client must close explicitly, including after a failed read or write. A stream "flush" or "shutdown" on a handle sends nothing to the phone and proves neither close nor durability.
- A client that collapses every stat error into "missing" or ignores close errors cannot be used to prove that a transfer completed. AFC status 8 is `ObjectNotFound`. A client that proves absence needs that exact status, not any stat error. (The phone returned status 8 for the audio file of a deleted track, read some minutes after the deletion session; chapter 2, section 9.2.)
- A read that can serve as evidence for one file is: stat, open read-only, ordered reads of the exact size, explicit close with its status checked, then stat again. Require a regular file whose attributes are the same before and after.

### 4.5 Services needed before the host does anything else

CONFIRMED over Wi-Fi (not tested over USB): `com.apple.atc` and `com.apple.mobile.assertion_agent` close the connection on the first message unless the host also has a `com.apple.mobile.heartbeat` connection open. The phone's syslog shows `atc` registering with `heartbeatd` for each link (`Watching: ... [pid=53 (atc ...)]`, `ATLegacyMessageLink ... wifi=1`).

## 5. Wi-Fi transport

### 5.1 The protocol works unchanged over TCP

CONFIRMED (phone on the same network and not plugged in). With only the pairing record saved by usbmuxd, a direct TCP connection gives the same services as usbmuxd:

- Lockdown on TCP 62078: `QueryType`, `GetValue`, `StartSession` with the record (TLS), and `UniqueDeviceID` inside the session.
- `StartService` returns a port on the phone for `com.apple.afc`, `com.apple.atc`, `com.apple.mobile.heartbeat` and `com.apple.syslog_relay`, all with `EnableServiceSSL`. Connecting and starting TLS with the record works.
- AFC read `MediaLibrary.sqlitedb` (3,149,824 bytes) in 0.44 s, about 7 MB/s, over that first connection.

The lockdown domain `com.apple.mobile.wireless_lockdown` reads `SupportsWifi` and `SupportsWifiSyncing`, `EnableWifiDebugging` true, and a `BonjourFullServiceName`. No `EnableWifiConnections` key was needed. Setting `EnableWifiConnections` to true (what Finder's "Show this iPhone when on Wi-Fi" does) made no difference.

Apple's host does the same. `_connect_to_port` (MD sub_e8830, string "_connect_to_port") branches on the device's interface type: type 0 (the usbmux `ConnectionType` `Network`) calls `_AMDeviceConnectByAddressAndPort` (MD RVA `0xe6280`), a plain `socket()` and `connect()` to the device `NetworkAddress`; otherwise `USBMuxConnectByPort`. CONFIRMED. No separate Windows service sits in the Wi-Fi data path. NOT FOUND otherwise.

### 5.2 Finding the phone: Bonjour and the authTag

CONFIRMED. The phone advertises `_apple-mobdev2._tcp`:

```
instance : <private-wifi-mac>@<ipv6-link-local>-supportsRP-26
port     : 32498
host     : <device-name>.local
TXT      : identifier=<UUID>, authTag=<...>, authTag#1=<...>
```

- The MAC in the instance name is the phone's private (per-network) Wi-Fi address, not the pairing record's `WiFiMACAddress` (the hardware address). Matching by the hardware MAC, as older tools do, finds nothing. The private address can rotate; matching followed it.
- iOS 26.4 and later publish one `authTag` per paired host. The value is the base64 of the first 8 bytes of `HMAC-SHA256(key, identifier)` where `key = HKDF-SHA512(ikm = HostID, salt = "", info = "", L = 32)`. MobileDevice implements the check as `AMDIsTXTRecordForUDID`. On the test phone `authTag` matched this host's record and `authTag#1` belonged to another paired host.
- Port 32498 is not open. Lockdown is on 62078 regardless of the advertised port.
- A host browses with an in-process mDNS client (no avahi or system daemon is required), matches by authTag (falling back to the MAC for phones that predate per-host tags), and then confirms that a paired session reports the record's UDID before using the address.

### 5.3 A locked phone sleeps within seconds

CONFIRMED. With the phone locked, it answered for a few minutes after it was locked, then stopped answering ping, Bonjour and TCP SYNs to 62078 (15 s timeouts), while its radio still answered ARP. A 60 s reachability log over several hours showed it asleep in every sample not caused by an experiment.

### 5.4 What wakes it

CONFIRMED. Each trial waited until ping stopped, sent one datagram and timed the first reply.

| Datagram | Woke |
| --- | --- |
| Magic packet (private MAC), UDP 9, unicast to its IP | yes, 1.4 s |
| Magic packet (private MAC), UDP 9, subnet broadcast | no (20 s) |
| Magic packet (hardware MAC), UDP 9, unicast | yes, 1.0 s |
| `hello` x 20, UDP 9, unicast | yes, 1.0 s |
| Magic packet (hardware MAC), UDP 7, unicast | yes, 1.4 s |
| `hello` x 20, UDP 7, unicast | yes, 1.0 s |
| `hello` x 20, UDP 40000, unicast | no |
| Magic packet (hardware MAC), UDP 40000, unicast | no |
| TCP SYN to 62078 (repeated) | no |

The radio wakes the system for any unicast datagram to the echo or discard port, whatever it carries. Broadcasts and other ports do not wake it, and neither does a TCP connection attempt. A host sends a unicast datagram to UDP 9 (a magic packet for the record's hardware MAC works) at the phone's last known address before discovery and before every TCP connection.

### 5.5 It falls asleep again almost at once

CONFIRMED. After a wake the phone stays reachable for about 2 to 5 s (up to 11 s once), then sleeps.

- The heartbeat service sends `{Command: SleepyTime}` about 2 s after the heartbeat connects.
- When the phone sleeps it closes every open service connection. An AFC connection opened 0.3 s after a wake failed with EOF on its next request after 25 s idle, after 12 s idle with a wake datagram every second, and after 1 s with a request every second.
- Holding and answering a heartbeat connection does not prevent this. `SleepyTime` still came about 2 s after the wake and the AFC connection died with it.
- With a wake before every connection, short commands (a device query in 1.5 s with a known address and 6 s with Bonjour; a catalogue capture in 4 to 7 s, in 4 of 5 runs) fit in one window. A longer capture lost its AFC connection to a sleep.

### 5.6 Unlocked and locked observations

Unlocked phone, Auto-Lock set to Never, Wi-Fi:

- `assertion_agent` and `com.apple.atc` close on the first message when no heartbeat connection is open, locked or unlocked. The refusals seen on the locked phone were therefore not evidence about the lock. With a heartbeat open, `CommandCreateAssertion` (`AMDPowerAssertionTypeWirelessSync`, timeout 60.0, with a detail) is answered `{}` and the connection stays open.
- An ATC `Ping` is answered with `Pong` and `SyncAllowed`.
- Reading the library catalogue (399 tracks) took 17 s; a dry-run comparison of 399 local files took 144 s; a full read-back of all 398 covers was stopped at cover 308 after 167 s, against about 6 s over USB. The cost was one wake, TCP connection and TLS handshake per cover at about 84 ms ping RTT.

Locked phone, unplugged, on Wi-Fi:

- Keep-awake works. With the heartbeat answered and the assertion `AMDPowerAssertionTypeWirelessSync` active, the locked phone stayed awake with AFC working for 3 minutes in a probe, and for a 287 s verification run. See section 6.
- `heartbeatd` keys host watchers by the pairing HostID. A second process from the same host replaces the first one's heartbeat (log: "Replacing old hostWatcher"). Two programs from one host must not use the phone at once.
- A decided sleep cannot be stopped (section 6.3).
- Timings on the locked phone over Wi-Fi: catalogue capture 29 to 48 s, verification 287 s (about 6 s over USB, which is about 60 ms per AFC round trip), a one-track insert with cover 80 s, a one-track delete 94 s.

### 5.7 Other effects of a phone restart or a wedge

- CONFIRMED. A phone restart deletes the `Airlock` staging folders (section 7.5). The first insert after a restart found "required directory absent". A client must accept the staging folders as absent and create them before the first upload.
- CONFIRMED. A dropped AFC connection (phone slept) must be recognised as such. Wrapping copy errors into text hid this once. A read-only capture can be retried; a write cannot be replayed blindly.
- CONFIRMED. `afcd` can wedge (section 9).

## 6. Keeping a Wi-Fi phone awake

### 6.1 What Apple's host does

CONFIRMED (static analysis of MD, AMP, AT). Apple Devices takes two kinds of assertion during Wi-Fi sync:

1. `AMDevicePowerAssertionCreate` (MD). The host connects, starts a session, starts `com.apple.mobile.assertion_agent` and sends a plist:

   ```
   {CommandKey: CommandCreateAssertion,
    AssertionTypeKey: ..., AssertionNameKey: ...,
    AssertionTimeoutKey: ..., AssertionDetailKey: ...}
   ```

   AMP uses type `AMDPowerAssertionTypeWirelessSync` for the Wi-Fi interface and refreshes it on a timer (`powerAssertionTimerRef`, `powerAssertionRefreshing`). The only call site of the import is `sub_5c6918` (call at `0x1405c6b6c`, AMP).
2. `ATHostConnectionSendPowerAssertion` (AT RVA `0x31b80`). An ATC message on the sync connection: `{Command: PowerAssertion, Session: <current>, Params: {Enabled: <bool>}}`. The host logs "airtraffic> sending PowerAssertion %s".

Apart from the assertion and the heartbeat, no keep-alive traffic or timer whose purpose is to keep the radio awake was found (NOT FOUND). The data path simply keeps the connections busy during a copy.

### 6.2 What the phone accepted

CONFIRMED.

| Attempt | Result |
| --- | --- |
| `assertion_agent` `CommandCreateAssertion`, no heartbeat open, any of types `PreventSystemSleep`, `PreventUserIdleSystemSleep`, `AMDPowerAssertionTypeWirelessSync`; XML or binary plist; integer or real timeout; with or without detail | connection closed on every variant (locked phone, awake at the time) |
| Same, with a heartbeat connection open (unlocked, then locked) | answered `{}`, connection stays open |
| `com.apple.atc` `Ping`, no heartbeat open | connection closed immediately after the first `Ping`, with or without a following `PowerAssertion` (while the phone still answered ping for 11 s) |
| `com.apple.atc` `Ping`, heartbeat open (unlocked) | `Pong`, `Capabilities`, `SyncAllowed` |

The refusals of both services on the locked phone came from the missing heartbeat, not from the lock.

### 6.3 How the keep-awake works in practice

CONFIRMED.

- The assertion `AMDPowerAssertionTypeWirelessSync` with a 60 s timeout, renewed every 20 s, kept a locked phone awake for the longest run (287 s). `assertion_agent` answers one command per connection; a second command closes the connection. Each renewal is therefore a new connection, and the previous one closes only after its successor is open.
- A sleep that is already decided cannot be cancelled. About 1.4 s after a wake with no assertion, the phone's idle timer runs out (log: "PMRD: sleep reason Idle Sleep", `kIOMessageCanSystemSleep`). An assertion created after that point does not stop the sleep. About 5 s later `heartbeatd` sends `SleepyTime` (on WillSleep) and every connection closes. This killed a command that started just as another one exited.
- Mitigation that worked: after taking the assertion, watch the heartbeat for 6 s. If `SleepyTime` arrives, let the phone sleep, wake it again and take the assertion again (up to three times), and repeat the wake datagram every second while connecting. Six back-to-back library captures then passed.
- The watch must come first. Deferring it until just before the first write, so that read-only work could start at once, failed: a command started right after another found the phone had already decided to sleep, and lost its connections. Taking the phone, then watching for 6 s, stays.
- Whether the ATC `PowerAssertion {Enabled: true}` message alone keeps a locked phone awake for a whole sync is OPEN. The power assertion through `assertion_agent` is what was shown to work.

## 7. How Apple's host moves files (AFC)

All of this section is static analysis of AMP 1.6.4.90 and MD unless marked otherwise. The AFC protocol header magic is `CFA6LPAA`; the packet number is at header +0x18 and the packet type at +0x20.

### 7.1 Summary

1. Apple does not pipeline AFC. A per-connection in-flight limit (`MaxOperations`) is initialised to 1 and has no setter. Replies are matched first-in-first-out.
2. Apple uses large requests. One AFC read or write packet carries up to 1 MiB.
3. Over Wi-Fi Apple opens two AFC connections per device: one for metadata operations and one for file data. Over USB it opens one.
4. File copy is double-buffered by a reader thread and a writer thread (three buffers of at least 1 MiB). Files are copied one after another.
5. No read-back or hashing after a copy was found in the copy path.
6. No Wi-Fi socket tuning was found beyond socket timeouts and `TCP_NODELAY`.

The speed over Wi-Fi therefore comes from fewer, larger round trips plus a second connection, not from deeper pipelining or socket options. With a measured idle RTT of about 22 ms, a lock-step ceiling is 64 KiB / 22 ms = about 3 MB/s versus 1 MiB / 22 ms = about 46 MB/s.

### 7.2 Bulk I/O sizes (Q1)

CONFIRMED.

| Item | Value | Where |
| --- | --- | --- |
| FS block size default | `0x100000` (1 MiB) at connection object `+0x60` | `AFCConnectionCreate`, MD RVA `0x1149d0` (`0x180114b1b`) |
| Socket block size default | `0x100000` at `+0x68` | `0x180114b41` |
| IO timeout default | `0x3c` (60 s) at `+0x70` | `0x180114b72` |
| `MaxOperations` | `1` at `+0x78` | `0x180114b7a` |
| Maximum packet length accepted | below `0x100000000` ("Implausible packet length") | `AFCValidateHeader`, MD RVA `0x11a810` (`0x18011a9b3`) |

- `AFCConnectionGetFSBlockSize` is MD RVA `0x111550`, `AFCConnectionGetSocketBlockSize` `0x111560`, `AFCConnectionGetIOTimeout` `0x114c60`.
- No splitting happens at packet level. `AFCFileRefRead` (MD RVA `0x111fd0`) builds one read operation (packet type `0xf`, header `0x38`) for the whole requested length. `AFCFileRefWrite` (RVA `0x1124e0`) wraps the caller's whole buffer in a no-copy CFData and builds one write operation (packet type `0x10`, header `0x30`). One call is one AFC packet.
- Splitting happens only at the socket or TLS write. `AFCSendData` (RVA `0x11c580`) loops `send = min(AFCConnectionGetSocketBlockSize, remaining)` into `sub_11be10`, which calls `SSL_write` on the connection's TLS object (`conn+0xd8`) or `send` if there is none. `AFCReadData` (RVA `0x11c240`) does the same with `SSL_read` or `recv`.
- Header and body are separate writes above 8 KiB. `AFCSendPacket` (RVA `0x11a2c0`) coalesces header and payload into one `0x2000`-byte buffer only when their sum is at most `0x2000` (`0x18011a320`); otherwise it calls `AFCSendHeader` then `AFCSendData`.
- Chunk size chosen by Apple's own callers is 1 MiB:
  - `AMDeviceCopyFile` (MD `sub_eeb80`): `malloc(0x100000)` (`0x1800eed00`), `ReadFile` up to `0x100000`, `AFCFileRefWrite` (`0x1800eedc2`).
  - `unzip_and_transfer` (MD `sub_f65c0`): same (`0x1800f664d`, `0x1800f6aca`).
  - AMP, the track copy engine: the I/O size comes from the volume's `maxIOSize`. `sub_5adf30` (strings "afcVolumeInfo != nullptr", "volumeInfo->maxIOSize != 0") stores `AFCConnectionGetFSBlockSize` (0x100000) at `[volume+0x298]` (`0x1405ae0c5`, `0x1405ae0cb`) and falls back to `0x200000` if it was 0 (`0x1405ae10e`). The copy engine setup `sub_6b1ea0` (strings "bufferSize > 0", "bufferData->maxReadSize != 0", "bufferData->maxWriteSize != 0") reads read size from the source volume and write size from the destination volume (`0x1406b1f7d` to `0x1406b1f9b`), allocates buffers of `max(read, write)` bytes (`0x1406b1fa8`) and defaults either size to `0x10000` only if it is zero (`0x1406b209b`, `0x1406b20ec`). The reader task `sub_6b15bc` reads `min(maxReadSize, remaining)` per call; the writer task `sub_6b1b98` writes `min(maxWriteSize, remaining)` per call. Because the destination is an AFC volume, each AFC write is up to 1 MiB.
- Nothing changes the 1 MiB defaults. `AFCConnectionSetFSBlockSize` and `AFCConnectionSetSocketBlockSize` (RVA `0x1115b0`, `0x1115d0`) send AFC packets `0x19` and `0x1a` and update the fields on success, but no caller exists in MD, and AMP and AT do not import them.

INFERRED: the source volume's read chunk (`[[rdi+0x268]+0x298]`) was not traced; it only affects local disk reads. The phone-side cap on a request was not visible in the binaries. It was settled by measurement: the phone accepted 1 MiB writes in the restore runs of section 10.

### 7.3 Pipelining and concurrency inside AFC (Q2)

CONFIRMED.

- There is a queue and packet numbering, but the in-flight depth is 1. `AFCConnectionSubmitOperation` (MD RVA `0x115310`) appends to the pending array (`conn+0xa0`). `__AFCConnectionPerformOperationCallBack` (RVA `0x113b70`) sends pending operations only while the in-flight array (`conn+0xa8`) is shorter than `MaxOperations`:

  ```
  180113bb7: call CFArrayGetCount            ; in-flight array
  180113bbd: movsxd rcx,[rbx+0x78]           ; MaxOperations
  180113bc1: cmp rax,rcx
  180113bc4: jge 0x180113cda                 ; stop sending when full
  ```

- `+0x78` is written in exactly one place (`AFCConnectionCreate`, `0x180114b7a`, value 1). No exported setter exists (the `AFCConnectionSet*` exports are Context, DisposeSecureContextOnInvalidate, FSBlockSize, FatalError, IOTimeout, SecureContext, SocketBlockSize, CallBack).
- `__AFCConnectionSendOperation` (RVA `0x114050`) stamps packets with a connection counter that increases by one per request (`0x1801140e2` to `0x1801140f1`).
- Replies are matched first-in-first-out, not by packet number. The socket callback (`sub_114270`) calls `sub_114810`, which takes index 0 of the in-flight array and removes it (`0x180114841`).
- The synchronous path is strictly lock-step: `AFCConnectionProcessOperation` (RVA `0x114d80`) takes the IO lock and does send then receive under it (`0x180114f27`, `0x180114f3f`) when the connection is not on a run loop.
- AMP schedules each AFC connection on its own thread named "AFCConnectionRunLoopThread" (`sub_5a3718`, `0x1405a39a4`). Several fibers can queue operations on one connection (`sub_5a6ebc`), but they go on the wire one at a time.

INFERRED: Apple's client has the hooks for depth greater than 1 and never enables them. Apple's host never tests whether `afcd` answers several outstanding requests on one connection. A Linux client did, for stats only: section 10.3.

### 7.4 Two AFC connections over Wi-Fi (Q3)

CONFIRMED.

- `sub_5a4dcc` (called from `sub_5a4730`, log string "created AFC connections %p, %p for %{public}s device %{public}@ (%p)") calls `AMDeviceGetInterfaceType` and computes the count as 2 if the type is 2, else 1 (`0x1405a4e02` to `0x1405a4e15`). Each iteration (`sub_5c2a08`) starts a new lockdown service `com.apple.afc` with `{CloseOnInvalidate: false}` (`AMDeviceSecureStartService` via `sub_5baa58`) and creates an AFC connection with its own TLS context. Each connection is a separate TCP and TLS session.
- Interface type 2 is Wi-Fi ("indirect/wifi"; type 1 is "direct/cable"; AT `sub_32330`, string "Device attached over %{public}s"). `AMDeviceGetInterfaceType` (MD RVA `0xebf10`) maps `device+0x30`: 0 to 2, 1 to 1, 2 to 3, 3 to 4. `AMDeviceCreateFromProperties` (MD RVA `0xeab30`) stores 1 when the usbmux `ConnectionType` is `USB` (`0x1800ead6b`) and 0 when it is `Network` (`0x1800eae5e`).
- Connection [0] carries metadata and connection [1] carries file data. `sub_5a3e3c(volumeInfo, useSecond)` returns `afcConnection[1]` if `useSecond` is set and it exists, else `[0]`. File open (`sub_5a88a4`, `AFCOperationCreateOpenFile` at `0x1405a8b89`) passes `useSecond = 1` (`0x1405a899d`), and the opened file keeps that connection for all reads, writes, seeks and the close (`0x1405a8abe`). Metadata operations pass 0: GetFileInfo (`sub_5a931c`), MakeDirectory (`sub_5a9b28`), SetModTime (`sub_5a9e7c`), RemovePath (`sub_5aa640`), Rename (`sub_5aae48`, `sub_5ab2c8`), LinkPath (`sub_5abbf0`), GetFileHash (`sub_5ac060`), GetFileHashWithRange (`sub_5ac3f8`), ReadDirectory (`sub_5acf38`), GetDeviceInfo (`sub_5adb54`). Over USB `afcConnection[1]` is null and everything shares connection [0].
- Consequence (INFERRED): a slow metadata exchange does not wait behind a 1 MiB data packet, and the reverse.

### 7.5 How audio and artwork are copied during a sync (Q3)

CONFIRMED unless marked.

- Copy driver. The "CopyFiles" worker `sub_228904` (strings "CopyFiles", "copy>  starting track 0x%16.16llx '%{public}s'", "copy> completed track ...", "perf> waiting for track ... to finish transcoding", "max-transcode-threads") is a single loop. It dequeues one work item (`0x140228b0e`), dispatches on the kind (TransferApplication, CopyGenericFile, CopyPromisedFile, CopyPromisedData, CopyIPodGame, or the track file item `sub_228630`) and runs it to completion before the next. `sub_228630` to `sub_225d30` hands the copy to a work thread and blocks on an event (`0x140225f85`). One file is copied at a time. Transcoding of later tracks runs in parallel.
- Per-file copy engine. `sub_6b03c4` (JRCopyFiles) to `sub_6b2248` to `sub_6b1ea0` creates two single-thread queues, allocates three buffers (`mov edi,0x3` at `0x1406b2011`), and queues all three to the reader. The reader task `sub_6b15bc` (set at `0x1406b2157`) fills a buffer from the source and queues the writer task `sub_6b1b98` on the other queue (`0x1406b1a94`), which writes it to the AFC file in chunks of `maxWriteSize` and re-queues the reader. While buffer A is being written over AFC, buffer B is being read from disk. The writer is one thread, so one AFC write is in flight.
- Writes are sequential `Write` packets. `sub_5a7c7c` (strings "positionMode == fsAtMark || positionMode == fsFromStart", "writeData != nullptr", "WriteAtPosition", "Write") uses `AFCFileDescriptorCreateWriteOperation` for "at mark" (`0x1405a7fd5`). For "from start" it uses `AFCFileDescriptorCreateWriteAtPositionOperation` (`0x1405a7e4a`) if the volume supports it, else a `SetPosition` operation followed by the write (two round trips). The support flag is a tri-state at `+0x4b3`.
- Operations per file: GetFileInfo collision checks (connection 0), open (connection 1), one write per at most 1 MiB, close (connection 1), `RenamePath` from the Airlock name to the bare AssetID (connection 0).
- Staging paths (transport view). The AFC copy destination is `Airlock/<Dataclass>/<AssetID>.<ext>`, for example `Airlock/Media/<AssetID>.m4a`, and artwork goes to `Airlock/Media/Artwork/<AssetID>` (resolver at AMP `0x46e954`). After the copy the host renames the file in the same folder to the bare AssetID (AFC RenamePath, `0x45bb8a`). The permanent path announced in `FileComplete` is `iTunes_Control/Music/Fnn/XXXX.ext`, with `nn` a folder index (default 50 folders, `F00` to `F49`) and `XXXX` four random uppercase letters. How the phone moves the file from `Airlock` to the permanent path is chapter 02's subject.
- AT has no direct AFC use: it imports no `AFC*` function. Its ATC socket (`sub_2fb50`, "ATHostAMDeviceSocket") reads with a `0x2800`-byte (10 KiB) buffer via `AMDServiceConnectionReceive` and writes whole messages with `AMDServiceConnectionSend` in a loop (`sub_2f990`).
- NOT FOUND: where the artwork copy differs from the audio copy. The same copy engine and the `CopyGenericFile` and `CopyPromisedData` kinds appear (`sub_22676c`, `sub_22661c`), but their callers were not traced. INFERRED: artwork takes the same two-connection path, since every AFC file open uses connection 1. The Windows-side read chunk size was not traced either.

### 7.6 Socket and transport tuning (Q4)

CONFIRMED.

- The Wi-Fi socket is a plain blocking `socket()` and `connect()` with no `setsockopt` in `_AMDeviceConnectByAddressAndPort`.
- `_apply_socket_timeouts` (MD `sub_e8740`) then sets `SO_SNDTIMEO` and `SO_RCVTIMEO` (level `0xffff`, options `0x1005`, `0x1006`) to `0x61a8` = 25,000 ms (`0x1800e8755`). It is called from `AMDeviceConnect` (lockdown, `0x1800e97fa`) and `AMDeviceSecureStartService` (services, `0x1800ed1f1`). AMP and AT import no `setsockopt`.
- AFC then sets `TCP_NODELAY` and non-blocking mode. `sub_11ba20` (called from `AFCConnectionCreate` at `0x180114aa3`) calls `setsockopt(IPPROTO_TCP, TCP_NODELAY, 1)` (`0x18011ba3d` to `0x18011ba5a`), `ioctlsocket(FIONBIO, 1)` (`0x18011ba65`), and sets `SO_SNDTIMEO` and `SO_RCVTIMEO` to the IO timeout (`0x18011ba85`, `0x18011bacd`). After that all AFC I/O is non-blocking with `select()` waits (`sub_11c0b0`, 60 s) and EWOULDBLOCK (`0x2733`) retry loops (`0x18011c03d`).
- No `SO_SNDBUF`, `SO_RCVBUF` or `SO_KEEPALIVE` on the Wi-Fi service sockets. Those options appear only on the loopback usbmux client sockets: `USBMuxConfigureNewClientSocket` (`sub_1d45e0`: send and receive buffers `0x10000`, keepalive on) and `USBMuxIntNewSocket` (`sub_1d4a80`: 127.0.0.1:27015, buffers `0x10400`). They matter for the USB path only. A function that sets TCP keepalive options (`sub_23f1c0`) belongs to the remote-pairing library, not to lockdown or AFC.
- TLS is statically linked LibreSSL 2.8.3. AFC calls `SSL_write` and `SSL_read` directly with up to the socket block size (1 MiB) per call.
- NOT FOUND: TLS record size control (assume the 16 KiB default) and anything on the Windows side that tunes the Wi-Fi radio.

### 7.7 Other reasons Apple's Wi-Fi sync does not stall (Q6)

CONFIRMED unless marked.

- Requests are large and rare: for a 5 to 30 MB track Apple does about 5 to 30 write round trips instead of 80 to 480 at 64 KiB. Open, close, rename and info checks are one round trip each.
- Metadata and data run on separate TCP and TLS sessions (section 7.4).
- Keep-awake is the power assertion and the heartbeat (section 6).
- Bulk application packages use a different channel, `SZConduit` (streaming zip), via `SZConduitSendPathWithPreflight` with a `PreferWifi` option, tried first in `AMDeviceSecureTransferPath` (MD RVA `0xf8250`; strings "Trying SZConduit for transfer", "PreferWifi", "falling back to AFC"). It is used for app installs only, not for Music assets. Delta transfer for apps uses `afc_stat` and `AFCKeyValueRead` per file ("Copying %s because afc_stat returned %d").
- AFC offers batch operations that Apple's host uses: `ReadDirectory` (packet 3) lists a whole directory in one packet; `GetSizeOfPathContents` (`0x21`) and `RemovePathAndContents` (`0x22`) work recursively on the phone in one request; `DirectoryEnumeratorRef*` (`0x23` to `0x26`) stream a listing; `ReadWithOffset` and `WriteWithOffset` (`0x27`, `0x28`) avoid a separate seek.
- NOT FOUND: any separate Windows service in the Wi-Fi data path, and any keep-alive traffic other than the assertion and the heartbeat.

### 7.8 AFC packet types seen in these binaries

| Type | Operation | Notes |
| --- | --- | --- |
| `0x03` | ReadDirectory | `AFCOperationCreateReadDirectory`, AMP `0x1405ad0b2` |
| `0x0a` | GetFileInfo | collision checks and the one post-copy stat |
| `0x0f` | FileRefRead | header `0x38` = `0x28` + fileref + length |
| `0x10` | FileRefWrite | header `0x30`, payload is the data |
| `0x19` | SetFSBlockSize | never called by Apple's code |
| `0x1a` | SetSocketBlockSize | never called by Apple's code |
| `0x1d` | GetFileHash | path only; returns a 20-byte digest (SHA-1) |
| `0x1f` | GetFileHashWithRange | prefix hash used by copy resume |
| `0x21` | GetSizeOfPathContents | recursive size |
| `0x22` | RemovePathAndContents | recursive delete |
| `0x23` to `0x26` | DirectoryEnumeratorRef* | streaming listing |
| `0x27`, `0x28` | ReadWithOffset, WriteWithOffset | no separate seek |

The AMP call sites of the builders (each has exactly one):

| Builder | AMP call site (RVA) | Function |
| --- | --- | --- |
| `AFCOperationCreateGetFileInfo` | `0x5a94f8`, `0x5aba91` | `sub_5a931c`, `sub_5ab6f0` |
| `AFCOperationCreateGetFileHash` | `0x5ac218` | `sub_5ac060` |
| `AFCOperationCreateGetFileHashWithRange` | `0x5ac5be` | `sub_5ac3f8` |
| `AFCOperationCreateOpenFile` | `0x5a8b89`, `0x5aa3e0` | `sub_5a88a4`, `sub_5aa204` |
| `AFCFileDescriptorCreateReadOperation` / `ReadAtPosition` | `0x5a7a61` / `0x5a78f8` | `sub_5a77b4` |
| `AFCFileDescriptorCreateWriteOperation` / `WriteAtPosition` | `0x5a7fd5` / `0x5a7e4a` | `sub_5a7c7c` |
| `AFCFileDescriptorCreateSetSizeOperation` | `0x5a73e8` | `sub_5a7370` |
| `AFCOperationCreateRenamePath` | `0x5ab0f3`, `0x5ab571` | `sub_5aae48`, `sub_5ab2c8` |
| `AFCOperationCreateRemovePath` / `RemovePathAndContents` | `0x5aa7b4` / `0x5acdeb` | `sub_5aa640` / `sub_5acc6c` |
| `AFCOperationCreateReadDirectory` | `0x5ad0b2` | `sub_5acf38` |
| `AFCOperationCreateGetSizeOfPathContents` | `0x5ac92d` | `sub_5ac798` |
| `AFCOperationCreateGetDeviceInfo` | `0x5adbea` | `sub_5adb54` |
| `AFCOperationCreateSetModTime` | `0x5aa004` | `sub_5a9e7c` |

### 7.9 The shared sync lock and sync notifications

CONFIRMED (static, AMPDevicesAgent 1.6.4.90). Apple's normal Music sync path takes the shared AFC sync lock before it starts the ATC session. The sync function (`0x56fc74` to `0x5712cc`) calls `SetIPodSyncInProgress(true)` (`0x49902c`) before `NotifyAirTrafficSyncStarted` (`0x46cfec`). On the false-to-true transition the setter runs the lock routine `0x498440`, which:

1. posts the notification `com.apple.itunes-mobdev.syncWillStart` (`0x498629`);
2. creates or resolves the root file `com.apple.itunes.lock_sync` and opens it (`0x498743`);
3. tries an AFC lock on it (`0x498791`, `AFCFileDescriptorCreateLockOperation` at `0x5a8191`); on contention it posts `syncLockRequest` (`0x4987f5`) and retries within a timeout;
4. on success posts `syncDidStart` (`0x498a53`) and creates the root flag file `com.apple.itunes.syncing`.

The finish routine (`0x498c4c`) removes the flag file, unlocks (`0x498e1e`, `UnlockOperation` at `0x5a824c`), closes the descriptor (`0x498e69`) and posts `syncDidFinish` (`0x498f51`). All four notification names carry the prefix `com.apple.itunes-mobdev.`; cancellation, suspend and resume names use `com.apple.itunes-client`. The same lock and notification sequence exists in libimobiledevice's `idevicebackup2` (nonblocking exclusive lock, up to 50 retries at 200 ms), which was previously the only public precedent.

OPEN. The live order of lock and notifications was never captured from Apple's host, and no recorded Linux session took this lock. INFERRED: the Linux client does not take it (chapter 2, section 8.2 agrees). The phone accepted every recorded Linux session without it, so the lock appears to be a host-side courtesy toward other Apple clients and not a receiver requirement. A host that must coexist with Apple's own sync on the same phone should take the lock as Apple does, because the contention behaviour (`syncLockRequest`) was never tested.

## 8. Does Apple read back or hash files?

### 8.1 Verification after a copy (Q5 and Q3)

CONFIRMED. The sequence for a Music track is:

1. Copy through the engine of section 7.5. The writer checks the status of every write (call `sub_6aa994`, with a special case for -39 `eofErr` at `0x1406b1c8b`) and the close status.
2. Status triage: copy status codes are special-cased at `0x1402293d6` to `0x14022946f` (compared against -53, -36, -39, -43, -120, -206, -208; meanings not decoded). Failure goes to the log "copy> failed (%.3d) ...".
3. One AFC `GetFileInfo` of the written file (`sub_6aab9c` at `0x140229558`). The stat result is parsed by `sub_5a9008` (`st_size` at `0x1405a90e6`, `st_birthtime` `0x1405a91b3`, `st_mtime` `0x1405a9216`). `sub_361728` stores size and modification date in the host's track record (`cmp [rbx+0x68],rax` at `0x140361928`; date at `0x140361948`). The value is recorded, not compared with the source size. (That the stat result is the AFC stat and that the destination spec is `[rbp+0xf80]` is INFERRED from data flow.)
4. The recorded size is sent back to the phone in the next metadata operation as `location.file_size`. INFERRED: the phone may treat it as a consistency field.
5. Rename from the Airlock name to the bare AssetID.
6. `FileComplete` carries only `AssetID`, `Dataclass`, `AssetPath` (plus optional `Metadata`, `Anchor`). `FileProgress` carries `AssetProgress` and `OverallProgress`; `FileError` carries `ErrorCode` and `ErrorIsVisible`. No size, hash or timestamp is sent in any of them.
7. After `SyncFinished` the phone sends `AssetMetrics` (counts and physical sizes per asset type), which the host validates and logs. AMP's ATC path does no per-track catalogue read.

The summary: AFC write statuses, the close status, one `GetFileInfo`, the rename status, and the phone's own protocol replies. NOT FOUND: a check that size equals the source size, a hash, a read-back, a catalogue re-read. `JRCopyFiles` asserts `"context->options.replaceOption != kCopyFilesDoNotReplaceExistingItems"` (`0x1406b0ac9`): an existing destination is replaced, not skipped. A later `FileError` from the phone only reaches a handler that extracts the fields and logs them (`sub_460530`); there is no re-queue, delete or resend.

The AFC read operations in AMP have one call site each, in the generic read proc `sub_5a77b4`. The copy engine's reader reads only the source. INFERRED: no read-back follows the copy (static analysis cannot exclude one through indirect calls).

### 8.2 Hash entry points (Q1)

CONFIRMED unless marked.

Entry points in MD:

| Item | Detail |
| --- | --- |
| `AFCOperationCreateGetFileHash` | export, ordinal `0x60`, RVA `0x1175b0`. Nothing inside MD references it. |
| `AFCOperationCreateGetFileHashWithRange` | export, ordinal `0x61`, RVA `0x1176c0`. One call, at `0x1800ef3f8` inside `sub_ef170`. |
| Names | strings `GetFileHash` (`0x1802aac88`) and `GetFileHashWithRange` (`0x1802aaca8`) are entries of the AFC operation-name table `sub_17120`, next to `FileRefRead`, `FileRefWrite`, `GetFileInfo`, `RenamePath`, `SetModTime`, `GetSizeOfPathContents`. |
| `sub_ef170` | logs `"amd_get_filesize"`, `"ls: %lli; rs: %lli; lowest: %lli"`, `"running local SHA1(%s)"`, `"running remote SHA1(%s)"` under the logger name `"AMDeviceGetResumeAppendOffset"`. It computes a local SHA-1 of a prefix, asks the phone for the same range and compares. |
| Caller of `sub_ef170` | only `sub_f7280` (logger `"AMDeviceGetLocalOrRemoteOffsetToResume"`), which is not exported and has no reference anywhere in the image. In MD this resume helper is dead code (its purpose is INFERRED from names). |
| App transfer | `AMDeviceSecureTransferPath` (RVA `0xf8250`) uses stat, not hashes. `sub_f5250` ("delta_transfer_directory", "Need to copy %s") skips a file when the type bits, size and mtime all match (`0x1800f54ef` to `0x1800f554f`); after copying it sets the phone file's mtime to the local mtime with `AFCOperationCreateSetModTime` (`sub_f4530`, `0x1800f45bf`). The remote size is parsed with `atoi` (32-bit), which is fine for apps and not a pattern for large audio files. |
| CryptoAPI | MD imports `CryptCreateHash` and friends (17 wrappers at RVA `0x262c30` to `0x263cf0`); none of the music or app transfer paths touches them (INFERRED, not traced). |

Users in AMP and AT:

- AT imports no `AFC*` function and no hashing. Its only crypto imports are `CryptGenRandom`, `CryptAcquireContextA`, `CryptReleaseContext` (random numbers). AT2 is identical in this respect.
- AMP imports both hash operations (IAT `0x140824fd0` for `GetFileHash`, `0x140824fc8` for `GetFileHashWithRange`). Each has one call site: `0x1405ac218` (in `sub_5ac060`, messages `"AFC GetFileHash failed (0x%08X) ..."` and `"JRCFDataGetLength(hashData) == kAFCFileHashSize"`, 20 bytes at `0x1405ac329`) and `0x1405ac5be` (in `sub_5ac3f8`).
- AMP contains no CommonCrypto or BCrypt hash symbols. Any hashing (SHA-1 fallback, MD5 for Store downloads) is code inside the executable.
- The two functions are installed only as file-system procs. The AFC file-spec proc table (magic `'AFCP'` = `0x41464350`) is created lazily by `0x1405ae7e4` (table `0x140b9b220`); `sub_5ac060` is stored at table offset `+0x110` and `sub_5ac3f8` at `+0x118` (`0x1405ae9ba` to `0x1405ae9cf`).
- The only identified consumer is the copy-resume check `sub_6b134c` (RVA `0x6b134c`), called from the reader task at `0x1406b1747`. If source or destination is an AFC spec and the destination exists with length L smaller than the source, it hashes the first L bytes of both through slot `+0x118` (`0x1406b1517`, `0x1406b1551`) and compares 20 bytes (`0x1406b1573`); on equality the copy resumes at L (`cmove` at `0x1406b158c`). The fallback `sub_6ad818` hashes locally (assert `"dhHash.hashSize == kJRFileHashSize"`). It runs only when the copy option byte `[opts+0x10]` equals 3 (`0x1406b173a`). The option byte is built in `sub_2264dc` as `2 + (device kind == 'iPod' && [dev+0x2e02] & 0x20)`. What bit `0x20` means was not identified, so whether the Music path ever runs the resume hash is not proven (OPEN). It cannot run unless a shorter destination already exists.
- NOT FOUND: any consumer of slot `+0x110` (whole-file `GetFileHash`) anywhere in AMP. Indirect uses through other registers cannot be excluded.
- The same structure exists in AMP2: the two imports at IAT `0x140825fc8` and `0x140825fd0`, one call site each (`0x1405ac742`, `0x1405ac39c`), the resume check at `sub_6b14a8`, fallback `sub_6ad96c`. The hash and checksum string sets of AMP and AMP2 are identical.
- Store-style hashes such as `file-hash`, `package-file-hash`, `cover-image-hash`, `partial-md5`, `segment-md5` exist as keys or in download parsing. They arrive with item metadata or belong to Store downloads; INFERRED: none is computed from files on the phone.

Which paths hash on the phone:

| Path | Hash on phone? |
| --- | --- |
| Music sync through ATC (AMP) | No, apart from the possible partial-copy resume prefix hash gated by option 3 |
| App transfer through `AMDeviceSecureTransferPath` (MD) | No; type, size and mtime, then `SetModTime` |
| Resume helper in MD | SHA-1 of a prefix, but unreferenced |
| Backup, diagnostics and crash logs | Not examined |

### 8.3 How Apple knows a track is present

For transport purposes only: Apple does not look at the phone's audio files to decide what to send. It sends metadata operations keyed by persistent ID, and the phone answers with an `AssetManifest` listing the assets it still needs; the host copies exactly those (chapter 02). In a captured reset run the host staged 401 operations (1 `update_db_info`, 399 `insert_track`, 1 `insert_playlist`) and the phone asked for 54 assets.

## 9. The afcd GetFileHash leak

CONFIRMED (proven on the test iPhone over USB, phone just restarted, nothing else using AFC). Probe: a single AFC connection from the `idevice` library.

| Step | Result |
| --- | --- |
| 12,000 x open and close of `iTunes_Control/iTunes/iTunesPrefs` (1,232 bytes) | all succeed, 11.1 s |
| GetFileHash of the same file, with an open every 250 hashes | hash number 2,551 fails with `Afc(NoResources)` after 2.1 s |
| Open on the same connection, then on new connections 5 s and 35 s later | `Afc(NoResources)` |
| Open after 5 minutes with no connections | `Afc(NoResources)` |

- After the failure `GetFileInfo` and `GetDeviceInfo` still work. Every open and every GetFileHash fails. Nothing is logged by `afcd`. Only a phone restart clears it.
- `afcd` is one long-lived process (one PID across all connections; lockdownd hands each connection to it over XPC). Whatever each hash leaks accumulates across connections and runs and is not released when a connection closes or when the host waits.
- Plain opens do not leak, so the leak is in GetFileHash itself. INFERRED: most likely one file descriptor per request, with `afcd`'s limit near 2,560.
- The kernel logged "timed out stuck process ... [afcd], moving to idle band" during the first wedge, which came after two runs were cut short by phone sleeps. That wedge also lasted more than five minutes and was cleared only by a phone restart and one passcode unlock.
- It fits both observed wedges: the client hashed every track in each library capture, again in each verification and again after each sync batch. About 11,000 hashes had once been sent since the previous restart without a wedge, so a fresh `afcd` may replace an idle one that has not yet reached its limit (OPEN). It also fits earlier refused connections noted "soon after another connection hashed files".
- Consequence: a library above about 2,500 tracks would wedge the phone on its first full hash pass. Apple's own sync never asks for hashes (section 8). Do not send GetFileHash or GetFileHashWithRange in routine operation.

Controls after the replacement (one connection each): 12,000 stats, 6,000 open+read+close and 12,000 opens all succeeded and left `afcd` opening files. After all later runs it still opened files. `GetFileInfo`, `ReadDirectory` and `GetFileHashWithRange` were not probed specifically for leaks (OPEN for the last two; stat and open+read+close passed).

## 10. Measured transfer behaviour

All measurements are from the same phone and network. "Wi-Fi" means a locked phone kept awake as in section 6, on a link of about 200 Mbit/s. Ping RTT was about 84 ms on the unlocked phone's first requests after idle, about 22 ms on average when idle and about 3.5 ms back to back while traffic kept the radio awake.

### 10.1 Old and new transfer

Restore of a cleared library: 399 tracks, 8.2 GB, batches of 100.

| | 64 KiB requests, read-back of every upload | 1 MiB requests, phone-side hash |
| --- | --- | --- |
| Upload per track | 7.0 to 7.7 s | 1.74 s |
| One batch of 100 | 768 to 850 s | 276 s |
| Evidence files per batch (client-side logging) | about 130,000 (1.8 GB) | about 8,500 (156 MB) |

- Old: every 64 KiB request was one lock-step round trip (about 10 ms on Wi-Fi under load) and every upload was read back in full, so each track crossed the link twice.
- New: a steady 1 MiB write takes about 40 ms, about 26 MB/s, which is the link speed. Small requests take 3 to 6 ms while traffic keeps the radio awake. The first write on a new connection takes about 250 ms (TCP slow start). The phone's GetFileHash of a 24 MB file took 33 ms. A second AFC connection would not help, because writes already fill the link.
- What remains per batch of 100: about 40 s before the uploads (state capture, metadata, sync session) and 45 to 65 s after (catalogue and index capture, cover checks).
- One 64 KiB write in 199,167 got no reply for 10 s (the old per-request limit); one other took 8.9 s and nine took over 1 s. These are Wi-Fi retransmissions on a healthy connection. A batch cut short by this was undone cleanly. Requests were given 30 s afterwards.
- This answers an earlier open question: the phone accepts 1 MiB AFC writes and reads over Wi-Fi.

### 10.2 After removing phone hashing

Uploads are checked by their write, size, close and rename replies and by a stat of the stored size. A fingerprint cache on the host is keyed by device, path, size, birth time and modification time; an upload stores its source's fingerprint, so the capture after a batch reads no audio back. CONFIRMED: the phone's move from the staging folder to `iTunes_Control/Music` keeps both the birth time and the modification time. Every capture after the restore's four batches and after two single inserts found each new file under the stat its upload had recorded.

Timings for 399 tracks (8.2 GB) and 398 covers, phone plugged in:

| Operation | Hash-based build | Without phone hashing |
| --- | --- | --- |
| Library capture, USB | 13 s | 1.0 s |
| Library capture, Wi-Fi | 27 s | 13.4 to 14.9 s |
| Full verification, USB | 36 s | 15.1 to 17.7 s |
| Full verification, Wi-Fi | 97 s | 64.3 to 66.2 s |
| Clear library, USB | (Wi-Fi: 194 s) | 61.8 s |
| Restore, USB, 398 tracks | 2.1 to 2.4 s per track (earlier build) | 482 s |
| Delete over Wi-Fi | 94 s (one track) | 50 s (three tracks) |
| Mixed sync over Wi-Fi (MP3 batch, two single inserts, covers) | 80 s (one track and cover) | 126.5 s |

- The first capture with an empty fingerprint cache copies everything once: 223 s over USB. A forced full read-back of all audio over USB takes 221 to 225 s; after the restore it read back all 399 uploaded files and every byte matched.
- Restore: each batch of 100 took 65 to 73 s from plan to last upload, and about 10 s more to capture and verify it. 2 min 46 s went to reading and hashing the 399 local sources and planning before the first batch.
- A Wi-Fi verification spends about 20 s capturing state and about 41 s on cover checks (reading 398 originals and renditions).
- Cover decode checks: one external process per group of up to 64 renditions (instead of one per cover, about 0.2 s each) gave identical hashes for all 394 decoded renditions (chapter 06, cover art).
- A full read-back never needed more than four operations beyond one per MiB for a file.

### 10.3 Pipelined stats and the speed pass

CONFIRMED on the phone over Wi-Fi, with 403 tracks (8.2 GB) and 402 covers.

- **`afcd` answers pipelined stats.** A client sent up to 64 `GetFileInfo` requests on one connection before reading any reply. `afcd` answered them in order, and each reply carried its request's packet number, which the client checked. Every index pass, verification and capture runs this way, and none has faulted. Only stats were pipelined: reads, writes and opens stayed one at a time.
- **Fewer connections for uploads.** The phone tolerates a bounded number of operations per AFC connection, so an insert batch's uploads are spread over as few connections as that allowance fits for its 1 MiB writes, instead of one per file.
- **Fewer reads.** A cover whose original and rendition keep their stats is not read again (its earlier proof is reused); a capture that copied nothing is not stat again at its end; a batch reuses the run's last complete capture when the catalogue is unchanged. These are client-side choices; chapter 07 gives them.

| Operation over Wi-Fi | Before the speed pass | After the speed pass |
| --- | ---: | ---: |
| Library dry run (capture and compare 399 sources) | 142 s | 17.6 s |
| Full verification (403 audio files, 402 covers) | 66.5 s | 17.3 s (53.1 s without reusable cover proofs) |
| Insert batch of 12 short tones (10 ALAC with covers, 2 MP3) | 62.8 s | 28.2 s |
| Delete of those 12 tones | 44.1 s | 21.8 s |
| Delete of 399 tracks (4 sessions) | | 58.9 s |
| Restore of 398 tracks with covers in 4 batches of 100 | | 539.7 s |

The restore over Wi-Fi took 539.7 s against 482 s over USB (section 10.2). A dry run whose capture had to copy every file of the phone over Wi-Fi took 9 min 57 s for 8.2 GB, so the copy ran at about 14 MB/s or faster; the next dry run, served from the host cache, took 18.5 s. A command started while the phone was still waking took 72 s to find it, the next 8.4 s. Left as they are: the two catalogue reads that guard each capture (about 0.45 s per 7 MB read over Wi-Fi), the 6 s sleep watch, and writes, which stay one at a time.

## 11. Notes for implementers

Advice the evidence above supports:

1. Use 1 MiB for AFC reads and writes. Send header and payload in one TLS write where possible; Apple's client coalesces only up to 8 KiB and sends larger packets as two writes.
2. Keep one write or read in flight per connection, as Apple does. A second connection for file data is what Apple does over Wi-Fi; on the 200 Mbit/s link tested here it did not help, because 1 MiB writes already saturate it. Stats may be pipelined (up to 64 were tested): `afcd` answers them in order, and each reply's packet number should be checked against its request (section 10.3).
3. Do not write a file and then read it back or hash it on the happy path. Rely on per-write status, the close status, a stat of the stored size and the phone's own completion messages. Keep any full read-back for audits, and run it with the host-side cache of stats. A complete audio pass over USB read 8.83 GB for 434 tracks. The whole run, with the cover checks, took about 6 minutes.
4. Never send GetFileHash or GetFileHashWithRange in routine operation (section 9). Resume by re-sending, or by reading the existing prefix with ordinary reads and comparing on the host.
5. For Wi-Fi: wake the phone with a unicast UDP datagram to port 9 before each connection; keep a heartbeat connection open; take the `AMDPowerAssertionTypeWirelessSync` assertion (60 s timeout, renew every 20 s on a new connection each time); watch the heartbeat for `SleepyTime` for 6 s after taking the phone; and let only one process per host use the phone.
6. Set `TCP_NODELAY`, use non-blocking I/O with a 60 s operation timeout and 25 s send and receive timeouts on lockdown and service sockets, as Apple does. Apple sets no socket buffer sizes or keepalive on the Wi-Fi sockets. Allow 30 s per AFC request on Wi-Fi: occasional retransmission stalls of 9 to 10 s occur on healthy links.
7. Accept that `Airlock` folders can be absent (after a restart) and create them before the first upload. Expect a killed upload to leave its partial file in `Airlock/Media`; the phone neither claims nor cleans it (chapter 05). Close every opened AFC handle explicitly, including after errors. Discard ATC connections after any error or cancellation.
8. Use `ReadDirectory` and `GetSizeOfPathContents` for listings and sizes, and `RemovePathAndContents` for trees, instead of one stat per file.

## 12. Open questions

| # | Question | Status |
| --- | --- | --- |
| 1 | Does an ATC `PowerAssertion {Enabled: true}` alone keep a locked Wi-Fi phone awake for a whole sync? | OPEN. The `assertion_agent` route works (section 6.3). |
| 2 | Does a locked phone on its charger stay awake on Wi-Fi without any assertion? Apple's automatic Wi-Fi sync runs only while charging. | OPEN |
| 3 | Does `afcd` tolerate more than one outstanding request on one connection? Apple never tries. | Answered for stats: 64 `GetFileInfo` requests in flight were answered in order (section 10.3). Reads and writes untested. |
| 4 | Does a third AFC connection help, or does `afcd` throttle? Apple never uses more than two. | OPEN. A second did not help on a 200 Mbit/s link. |
| 5 | Do `GetFileInfo`, `ReadDirectory`, `FileRefRead` or `GetFileHashWithRange` leak in `afcd` like `GetFileHash`? | Partly answered: 12,000 stats and 6,000 open+read+close did not leak. `ReadDirectory` and ranged hash untested. |
| 6 | What is the root cause of usbmuxd not re-publishing the phone after a virtual-machine handoff? | OPEN |
| 7 | What made an instrumented Windows host exit 0.8 s after a USB attach, when an uninstrumented one survived? | OPEN |
| 8 | What bit `0x20` of `[dev+0x2e02]` means, and whether the Music path ever runs Apple's resume hash. | OPEN |
| 9 | Does a fresh `afcd` replace a wedged or idle one before the limit is reached? | OPEN |
| 10 | The artwork copy path of AMP (callers of `CopyGenericFile` and `CopyPromisedData`) and the Windows read chunk size. | NOT FOUND |
| 11 | TLS record size on Apple's AFC connections. | NOT FOUND (assume 16 KiB) |
| 12 | Does a Music sync need the `com.apple.itunes.lock_sync` lock, and what happens when Apple's host and a second host contend for it? | OPEN. No recorded Linux session took the lock (section 7.9). |

## 13. Evidence

Phone: iPhone17,2, iOS 27.0.1, build 24A446.

### 13.1 Linux sessions against the phone

- Host and phone inventory; pairing record inspection; passive ATC start over USB (the four startup messages).
- usbmuxd discovery failures after virtual-machine handoffs and the cable reconnect that fixed them; reuse of AFC primitives; ATC connection reuse after errors; first Ping and Pong.
- Complete audio pass of 434 tracks (8.83 GB) over USB; AFC status 8 on a deleted track's file.
- First Wi-Fi sessions: direct TCP to lockdown, services with TLS, Bonjour and authTag, wake datagram table, `assertion_agent` and ATC refusals without a heartbeat, locked-phone keep-awake runs (3 minutes and 287 s), the first `afcd` wedge.
- `afcd` probe: 12,000 opens, then GetFileHash until failure at request 2,551; controls of 12,000 stats, 6,000 open-read-close and 12,000 opens.
- Restore of a 399-track library (8.2 GB) in batches of 100 with 64 KiB requests and read-back, then with 1 MiB requests; removal of phone hashing; timing tables of section 10.
- Timings before and after pipelined stats and the other speed changes; a deferred sleep watch tried and reverted; a Wi-Fi restore of 398 tracks (section 10.3).
- A dry run whose capture copied every file over Wi-Fi, then one served from the host cache.

### 13.2 Windows host

Apple Devices 1.1540.23042.0 ran in a Windows virtual machine.

- A run with a dynamic-instrumentation observer attached ended about 0.8 s after the USB attach. A fresh uninstrumented host then survived a 120 s phone connection with no manual sync (section 4.3).
- No traffic of Apple's host was captured below the application level. The AFC chunk sizes, connection counts and hash use of Apple's host in sections 7 and 8 come from static analysis, not from a network trace.

### 13.3 Static analysis of Apple's binaries

Disassembly (objdump), string extraction and a small PE parser, with no code executed. The packages and binaries are those of section 1:

| Binary | Version | SHA-256 |
| --- | --- | --- |
| MobileDevice.dll | 1827.100.17.2 (Apple Devices 1.1540.23042.0) | `de08e3324f24dbcbc0597b78f797670f18f53faa58819cda3c32fa3cebcb9d39` |
| AMPDevicesAgent.exe | 1.6.4.90 (1.1540.23042.0) | `ad957f32107b7177969b3cb11b91e3922d27e622b53f9240b65cb457522111f9` |
| AirTrafficHost.dll | 4025.400.1.5 (1.1540.23042.0) | `0d38b01c21d57a9f3ca32c15d4b2b8aa0557d43a724b4745f1fb122322f4eecd` |
| AMPDevicesAgent.exe | 1.6.4.94 (1.1540.24088.0) | `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9` |
| AirTrafficHost.dll | 4026.110.1.6 (1.1540.24088.0) | `230445f73c4887ff46adfb7a9a25ed83fb99b6abc9bc341fcb8349bad3a7664b` |

Findings about AMP in sections 7 and 8 were read in 1.6.4.90. The hash and checksum string sets of 1.6.4.94 match, and the two hash call sites exist there too. Its copy and connection code was not re-read.

### 13.4 Open-source clients

The Rust `idevice` crate 0.1.68 (AFC client, mDNS browsing, `AfcClient` used for the `afcd` probe) and the libimobiledevice `idevicebackup2` sync lock were read for comparison (sections 4.4, 5.2 and 7.9).

### 13.5 Measurements

Transfer, library capture, verification, clear and restore timings (section 10) were measured on one phone and one 200 Mbit/s network. Idle ping RTT was about 22 ms, about 84 ms on the first requests after idle, and about 3.5 ms while traffic kept the radio awake.

### 13.6 Strength of the evidence

The Wi-Fi findings (sections 5, 6, 9, 10), including the pipelined stats of section 10.3, were observed on the phone. The AFC, assertion and socket details of Apple's host (sections 6.1, 7, 8) were read from code and not seen on a network. Items marked INFERRED, NOT FOUND or OPEN were not observed.
