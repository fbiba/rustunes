# Reverse engineering the iPhone Music sync

This directory is the curated write-up of the reverse engineering behind
`rustunes`, a Linux command-line tool that fills the stock iPhone
Music library over USB or Wi-Fi without Apple software. The work was carried
out against one iPhone17,2 running iOS 27.0.1 (build 24A446), later updated
to iOS 27.2 (build 24B119), the phone's firmware of both builds, and two
versions of Apple's Windows "Apple Devices" application, 1.1540.23042.0 and
1.1540.24088.0.

The chapters below are the public record of that work. Each chapter ends
with an "Evidence" section that describes the evidence it rests on: its kind,
device and binary build. Credits and licences for the prior work and the
tools used are in [`../ACKNOWLEDGEMENTS.md`](../ACKNOWLEDGEMENTS.md).

## Chapters

| # | Document | Contents |
|---|----------|----------|
| 1 | [Overview and methodology](01-overview-and-methodology.md) | Problem statement, environment, methods (passive capture, Frida instrumentation, static analysis, controlled phone experiments), timeline, safety statement, glossary |
| 2 | [ATC sync protocol](02-atc-sync-protocol.md) | The `com.apple.atc` wire protocol: framing, startup messages, Media sessions, operations, Ready and completion, signing |
| 3 | [Transport: USB and Wi-Fi](03-transport-usb-and-wifi.md) | usbmuxd and lockdown, Bonjour discovery, phone sleep behaviour, AFC transfer sizes and pipelining (pipelined stats included), measured speeds, the `GetFileHash` leak |
| 4 | [Media library database](04-media-library-database.md) | `MediaLibrary.sqlitedb` model, persistent IDs, metadata and sort-field mapping, grouping rules, artwork, statistics |
| 5 | [Receiver operations](05-receiver-operations.md) | What the phone does on insert, update, delete and playlist operations, including interruption and recovery |
| 6 | [Audio formats and gapless](06-audio-formats-and-gapless.md) | Accepted formats and codes, transcoding decisions, MP3 gapless computation and what the phone stored when it was sent, cover art and which cover image types the phone keeps |
| 7 | [Sync engine design](07-sync-engine-design.md) | How the CLI's journal, planning, verification and recovery are designed, and why: `sync`, `push` and `pull`, change plans, caches, journals carried across versions, FLAC to ALAC, JSON output for other programs |
| 8 | [How Apple's host batches a sync](08-apple-host-sync-batching.md) | Sessions per sync, the 500-operation file limit, the order of updates, removals and additions, upload and conversion scheduling, failure handling, and a comparison with `rustunes` |
| 9 | [iOS updates and database versions](09-ios-updates-and-database-versions.md) | How the phone versions and migrates `MediaLibrary.sqlitedb`, the other version stamps, the version rules of the phone's sync service and of Apple's host, what changed in iOS 27.2 and what the update did to a library |

## Evidence labels

Every chapter marks its claims:

- **CONFIRMED**: seen on the wire, in the phone's database, or in the
  disassembled code and data flow of the host binaries or the phone's
  firmware.
- **INFERRED**: reasoned from names, strings or analogy, not observed directly.
- **OPEN**: not settled; usually needs a controlled phone experiment.

Where the raw record conflicted, the later or explicitly corrected statement was
used, and the chapter says so when the correction matters.

## What is not covered here

Chapters 1 to 6 describe Apple's protocol, transport, library and receiver
behaviour. Chapter 7 describes the design of the sync tool itself, chapter
8 how Apple's host structures a sync, compared with the tool, and chapter 9
how an iOS update changes the library database. Chapter 1
explains the methods and the retention policy in section 10.

The raw evidence is private and is not published: the traces, database
captures, disassembly dumps, Apple binaries and the chronological working
notes. Apple's binaries and their derivatives are Apple's code, and the
captures contain the device owner's library and device identity. The chapters
publish addresses, constants, field names and conclusions only.

## Reading order

Start with chapter 1 for context and vocabulary. Implementers of a sync client
need chapters 2, 3, 5 and 8. Anyone writing metadata or converting audio for the
phone needs chapters 4 and 6, and anyone reading the phone's database across
iOS updates needs chapter 9. Chapter 7 is for readers who want to see how the
findings are turned into a safe sync engine.
