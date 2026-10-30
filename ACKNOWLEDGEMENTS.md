# Acknowledgements

This file credits the prior work, libraries and tools that `rustunes` and its reverse-engineering write-up (`docs/`) rely on. Licence facts are stated as recorded when each source was examined. Where a fact was not checked, the entry says so.

## 1. Prior work this project learned from

No code from any project in this section was copied into `rustunes`, except that the `idevice` crate is used as a normal dependency (see section 2). Each entry says what was taken.

| Project | Licence | What was taken |
| --- | --- | --- |
| go-tunes (`yinyajiang/go-tunes`, commit `3ee38393877a8adc5f5a928ddc834ee26bf5512b`) | No licence file in the snapshot. GitHub reported no licence for the upstream, its four direct forks or one further copy. | Public Go and C++ sources were read locally. They supplied the recipe for the 21-byte signature sidecar (the arithmetic core, later reduced to standard primitives and re-implemented) and the fixed 84-byte host request that makes the phone answer with an 83-byte value. Nothing was vendored, and its bundled Apple DLL was not executed. |
| AirCard-Linux (`shinkuan/AirCard-Linux`, commit `7686fd21e3e598d2217c5e6da88229cd1bca4e07`) | MIT | Ideas and protocol facts only: the ATC framing (little-endian 32-bit length followed by a binary property list) and the shape of a host session. |
| AirCard-Windows (`Lumid-Off/AirCard-Windows`, commit `35e891bdf24251a7769d360aa402a51ec257670a`) | MIT | Protocol facts only: which `ATHostConnection*` entry points exist and in which order a host calls them. |
| libimobiledevice (pinned at commit `fa0f79190142bc309307967c058f89c1b36eb6b8`; three headers and `idevicebackup2.c` only) | LGPL-2.1-or-later, according to the file headers | Reference reading only: AFC modes and locks, and the `com.apple.itunes.lock_sync`, `syncWillStart`, `syncLockRequest` and `syncDidStart` notification names. No code copied. The project does not depend on it. |
| `idevice` (`jkcoxson/idevice`, version 0.1.68, source commit `d32c8189c51c2789496b0768039419c3705498c3`) | MIT, copyright 2026 Jackson Coxson | Used as a dependency for the transport and service layer: pairing records, Lockdown sessions, service start, AFC and the framing for `com.apple.atc`. Its sources were also read as reference. |
| AirCard-iOS (`Mak5er/AirCard-iOS`, commit `40640c4d827aeab8d798285b57d24fcf6be070d0`) and AirLift (`0xjohnnydev/airlift`, commit `c684cd41ca0ded2d1ab780c15f6ead05509ce062`) | No licence recorded | Read as web pages only. Not downloaded. Their findings were not used beyond confirming that neither exports host state or a signer. |

## 2. Dependencies

Direct Rust dependencies of the workspace (`Cargo.toml` and `crates/*/Cargo.toml`). The licence is the `license` field of the crate's manifest in the local cargo registry, at the version locked in `Cargo.lock`. Transitive dependencies are not listed here. Run `cargo metadata` or a licence-listing tool for the complete set.

| Crate | Locked version | Licence |
| --- | --- | --- |
| aes | 0.8.4 | MIT OR Apache-2.0 |
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| cbc | 0.1.2 | MIT OR Apache-2.0 |
| clap | 4.6.7 | MIT OR Apache-2.0 |
| flate2 | 1.1.10 | MIT OR Apache-2.0 |
| hmac | 0.12.1 | MIT OR Apache-2.0 |
| idevice | 0.1.68 | MIT |
| image | 0.25.10 | MIT OR Apache-2.0 |
| jpeg-encoder | 0.7.1 | (MIT OR Apache-2.0) AND IJG |
| libc | 0.2.190 | MIT OR Apache-2.0 |
| lofty | 0.22.4 | MIT OR Apache-2.0 |
| mdns-sd | 0.21.5 | Apache-2.0 OR MIT |
| plist | 1.10.1 | MIT |
| png | 0.18.1 | MIT OR Apache-2.0 |
| rusqlite | 0.40.2 | MIT |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 |
| sha1 | 0.10.6 | MIT OR Apache-2.0 |
| sha2 | 0.10.9 | MIT OR Apache-2.0 |
| symphonia-bundle-mp3 | 0.5.4 | MPL-2.0 |
| symphonia-core | 0.5.4 | MPL-2.0 |
| tokio | 1.53.2 | MIT |
| tracing | 0.1.44 | MIT |
| tracing-subscriber | 0.3.23 | MIT |

Notes:

- `rusqlite` is built with its `bundled` feature, which compiles SQLite through `libsqlite3-sys` 0.38.2 (MIT). The SQLite source itself is in the public domain according to its authors. That was not checked against the vendored copy.
- `idevice` is built with its `ring` feature. `ring` 0.17.14 is licensed `Apache-2.0 AND ISC` in its manifest.
- `flate2` uses the `miniz_oxide` backend. `miniz_oxide` 0.9.1 is licensed `MIT OR Zlib OR Apache-2.0` in its manifest.
- `png` and `jpeg-encoder` turn a PNG cover into the JPEG that is sent (chapter 07, section 9.3). `jpeg-encoder` carries the IJG licence as well, as its manifest states.
- `symphonia-core` and `symphonia-bundle-mp3` decode the last frames of an MP3 for Apple's gapless rule (chapter 06, section 13). They are MPL-2.0, a file-level copyleft licence; they are used unmodified as dependencies.
- `lofty` is a normal dependency: it writes the tags of an ALAC converted from FLAC (chapter 07, section 9.5).
- `tracing` and `tracing-subscriber` write the log each run keeps (`idevice` already logs through `tracing`). `tracing-subscriber` is built with its `fmt` feature only.
- The test-only features (`clap` and `tokio` test utilities) use crates listed above.

## 3. Tools used

These tools were used to build, observe, analyse and verify. None is distributed with the project.

- AI coding agents, used heavily throughout development: for the protocol analysis, the code, the tests and the documentation.
- Frida (version 17.22.2 on the Windows guest) for dynamic instrumentation of Apple Devices for Windows.
- GNU binutils `objdump` for disassembly of the Windows binaries.
- ffmpeg and ffprobe for decoding and inspecting audio.
- LAME for generating MP3 test files.
- `usbmuxd` for the USB multiplexer on Linux.
- SQLite for reading and comparing the phone's `MediaLibrary.sqlitedb`.
- QEMU, with the QEMU guest agent and the human monitor, for the Windows 11 reference guest, and a SPICE viewer to drive it.
- Python 3 (3.13 on the guest, and the Linux system Python run in isolated mode) for the offline analysis tooling.
- OpenSSL, for independent checks of the signing arithmetic.
- `ipsw` (blacktop/ipsw, version 3.1.739) for reading Apple's iPhone firmware: decrypting its images with keys from Apple's key service, and the symbols, disassembly and strings of the dyld shared cache and the system executables.
- The public firmware index at ipsw.me, to locate Apple's restore images on Apple's update server.
- 7-Zip (version 26.04) for extracting files from the firmware's disk images.
- apfs-fuse (sgan81/apfs-fuse, GPL-2.0), whose LZBITMAP decompressor was the reference for a small private decoder used only to read compressed firmware files. The decoder is not part of the project's code and is not distributed.

Licences of these tools were not checked for this file.

At run time `rustunes` calls FFmpeg, if it is installed, to decode cover renditions for verification, for `validate --decode`, and to convert FLAC to ALAC under `--flac-to-alac`. It is run as a separate program and is not distributed with the project.

## 4. Trademarks and Apple software

Apple, iPhone, iTunes, Apple Music, Apple Devices, Finder and Bonjour are trademarks of Apple Inc. Other names are the property of their owners. They are used here only to identify the products this project interoperates with.

`rustunes` is an independent project. It is not affiliated with, endorsed by or sponsored by Apple Inc. No Apple code, binary, firmware, key, table or disassembly is redistributed in this repository. The documents under `docs/` give addresses, constants, field names and conclusions only. The signing step is an interoperability re-implementation of one observed profile, written with standard cryptographic crates.
