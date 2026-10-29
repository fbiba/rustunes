# 07. Sync engine design

## Abstract

Chapters 01 to 06 describe what the phone does. This chapter describes what
`rustunes` does about it. The tool is a command line program that
mirrors a Linux music directory into the stock iPhone Music library over
USB or Wi-Fi. It needs no Apple software, no Windows machine and no system
daemon beyond the USB multiplexer (USB) or nothing at all (Wi-Fi). `sync`
mirrors the configured library directory, `push` only sends, and `pull`
rebuilds the tool's own records from the phone.

The design rests on five decisions.

1. **The tool only touches what it can prove it owns.** Ownership is a row in a
   local journal that records the tool's own verified insertion, or an
   explicit `pull` that matched a phone track to a library file by its
   encoded audio. Similar tags, a matching file name or a matching source
   path never confer ownership.
2. **Every phone write is one signed session that is journaled before it is
   sent and settled by observation afterwards.** The tool never resends an
   uncertain session. It reads the phone again and decides what happened.
3. **Identity is the encoded audio, not the tags.** A retag is a metadata
   edit. Different audio at the same source path is a replacement. These are
   different operations with different costs and different losses.
4. **Verification is proportional.** The tool trusts a stat-keyed fingerprint
   cache for files it already proved, copies only what is new, and reads every
   byte back only when asked. It never asks the phone to hash files, because
   that exhausts the phone's file service.
5. **Refusal is a feature.** Whatever the tool has not verified on the phone
   (smart playlists, group deletes, unusual formats) it refuses before it sends
   anything, and says why.

Claims are marked **DESIGN** (a decision the tool makes, true by construction
and by tests), **CONFIRMED** (observed on a real phone), **PLANNED** (designed,
not built) or **OPEN** (unresolved). Section 12 lists which facts
come from phone runs and which are decisions.

Numbers such as "399 tracks" come from the reference library used for the
phone runs. Names in examples are placeholders.

## Contents

1. Goals and non-goals
2. The two crates
3. The journal
4. Planning
5. Execution
6. Verification
7. Recovery
8. Wi-Fi and USB in the engine
9. The formats pipeline
10. Output for other programs
11. Limits and open design questions
12. Evidence

## 1. Goals and non-goals

### 1.1 Goals

- **Incremental sync into the stock library.** One configured library
  directory is compared with the phone's current library. Missing files are
  added, changed tags are updated, tracks the tool sent whose source file has
  left the directory are removed after a listed confirmation, changed audio is
  replaced on request, and everything else is left alone. `push` sends a
  directory or a single file without removing anything. The result is the
  phone's own Music library, not a second database or a side app.
- **No Apple software at run time.** The tool speaks the same services the
  Apple host speaks (chapters 02 and 03) and reads the phone's catalogue
  database over the file service.
- **Never disturb what the tool does not own.** Unrelated tracks, their play
  counts, ratings, playlists and artwork must survive every operation, and the
  tool checks that they did.
- **Safe to interrupt.** The process can be killed at any moment, including
  mid-session, and the next run must be able to say exactly what happened and
  leave the library consistent.
- **Honest previews.** `plan`, `sync --dry-run` and `apply --check-only` show
  every addition, update, removal, kept track, excluded input and conflict. A
  dry run reserves no identifiers and changes no ownership.
- **Same behaviour over USB and Wi-Fi.** The transport differs, the engine does
  not (section 8).
- **Survive new versions.** A journal another version of the tool wrote, under
  another schema or other hash definitions, is carried forward from the phone,
  never refused into a clear and resend (section 3.5).
- **Drivable by other programs.** `--message-format json` prints every
  command's progress, notices, prompts, reports and result as one JSON record
  per line, for a frontend such as a GUI. Section 10 specifies the record
  stream.

### 1.2 Non-goals

- **No backup of the phone.** The tool does not copy the phone's audio or
  catalogue aside before it removes anything. Removed audio exists only in the
  sources. A removal (a mirroring `sync`, `delete`, `prune`, `clear`,
  `--replace-library`) is therefore permanent unless the file is still in the
  sources. This is a deliberate choice: a backup of a multi-gigabyte library
  per run would dominate every run's cost, and the sources are the intended
  master copy. The consequence is stated in each destructive proposal.
  `pull --tracks` copies phone tracks that no library file holds into the
  library, once and on request; it is a way to adopt them, not a backup taken
  before a removal.
- **No source file is ever modified.** Inspection, validation and planning read
  sources only. The tool does not retag, normalise or rename them.
- **No playlist authoring.** Creating or editing playlists is outside the
  tool. It preserves ordinary playlist memberships when it can model the
  change, and removes members when it removes a track.
- **No cloud, purchase or subscription handling.** Rows that are placeholders,
  cloud items, or of an unsupported type are blocked, not guessed at. Rows the
  phone made itself have sync ID 0: an Apple Music track it streamed, and the
  album and album artist it files one under
  ([chapter 4](04-media-library-database.md), section 3.1). They are left
  alone: no command lists, changes or removes them. One that holds a file on
  the phone is refused, and the catalogue is not read.
- **No transcoding by default.** A file is sent as it is. The one exception
  for audio is FLAC to ALAC, behind an explicit flag (section 9.5). A PNG,
  GIF, BMP or TIFF cover is sent as a JPEG of the same raster, because the
  phone keeps only JPEG covers (section 9.3).
- **No claim about what iOS can play.** The tool's source profiles are
  implementation limits. A profile is widened only after a track of that kind
  has been sent, verified, played and removed on a phone.

### 1.3 What is refused on purpose

These refusals happen during planning, before any phone write. Each has a
reason the research gave.

| Refused | Why |
| --- | --- |
| Removing members of smart playlists, folders, or playlists made on the phone; a playlist that holds a track twice or holds non-music items | The removal model only covers ordinary playlists with contiguous positions and music members. Other shapes have not been observed. DESIGN, refusal verified in tests |
| `delete_album` and `delete_artist` as ways to remove groups | The phone accepts both and changes nothing, by sync ID or by row ID. Emptied albums and artists stay until the phone collects them itself. CONFIRMED |
| Artist or album-artist edits on a track that is not the group's stored representative | The receiver renames or re-keys the shared group. The planner refuses rather than rename someone else's tracks (section 4.7) |
| Clearing a genre or composer by a metadata edit | The receiver ignores it. The planner leaves the file out, keeps its track, and offers replacement instead |
| Changing content rating or duration by edit | Not modelled. The file is left out and its track kept |
| Tag edits to tracks the tool did not insert | Not owned (section 4.8) |
| Prune with a filtered or partial selection | An omitted file is not a deletion request. Only the exact original, non-empty, unfiltered directory can authorise a prune |
| A mirroring removal from an empty library, or while a library file cannot be read | An unmounted or emptied directory is not a request to empty the phone. Nothing is removed and the sync stops (section 4.5) |
| Sources outside a verified format profile | Layer I and II MPEG audio, protected files, containers the phone does not list, pictures other than JPEG, PNG, GIF, BMP or TIFF, files over 64 MiB, sample rates and depths beyond the phone's advertised limits (section 9). Such a source is left out with its reason and the rest is sent (section 4.1), as is, under `--flac-to-alac`, a FLAC deeper than 24 bits or with more than two channels (section 9.5) |
| Replacing album or artist groups wholesale | Groups are matched by name, ignoring ASCII case, and the phone collects emptied ones itself. The tool never edits a group it does not need to |
| Adding a track to an album or album artist the phone made itself (sync ID 0) | What the phone does when a sync names a new PID with such a group's name is not known. The track is a grouping conflict, as with an ambiguous group, and a metadata edit refuses to join such a group. DESIGN, refusal verified in tests |
| Operation files above 500 operations | `--batch-size` counts operations and is at most 499, so with `update_db_info` no staged file exceeds the 500 operations Apple's host puts in one file; metadata sessions are packed to the same bound, and a Clear of more than 499 playlists is refused (section 5.1). Insert and removal sessions of 200 and of 499 are verified on the phone, and 499 is the default. What the phone does with a session of several operation files is untested, so none is staged |
| A second write while one is outstanding | One native write at a time, enforced in Rust and by SQL triggers (section 3.4) |

## 2. The two crates

DESIGN. The code is one Cargo workspace with two crates.

| Crate | Role | Authority |
| --- | --- | --- |
| `music-protocol` | Reusable protocol, media and catalogue mechanisms | None. Observations and encoded operations convey no permission to execute |
| `rustunes` | The command line, the sync engine, the journal, planning, execution, verification, recovery | All of it. This is where "may we send this?" is decided |

`music-protocol` is independent of the application (DESIGN). It
names no part of `rustunes`, reads no file outside its own directory, keeps its
own test fixtures, and uses its own hash domains and document tags
(`music-protocol/...`). It is packaged and its tests pass outside the
workspace, so another program can build its own engine on it.

### 2.1 What lives in `music-protocol`

The library is what another program would need to talk to the phone's library
without adopting this tool's policies.

- `protocol`: ATC framing and messages, session phases, exact operation
  serialisation, the fixed-host signer, AFC file access (open, read, stat,
  pipelined stats) with its connection rules and its write lock (section 5.3),
  the Airlock staging folders, the Lockdown steps (chapters 02 and 03).
- `media`: readers for ALAC, AAC family, MP3 and the other formats; encoded
  identities (`EncodedAlac`, `EncodedMpeg1Layer3`, `EncodedAudio`); the MP3 tag
  projection and Apple's MP3 timing and gapless rule (chapter 06); artwork
  facts; the format table that holds each format's `audio_format`, kind and
  phone limits.
- `catalogue`: parsing the phone's SQLite catalogue and its companion files,
  the raw full-table reader and the typed model of the 45 tables used to
  compare states, the delete encoders and the removal model (what a session
  of `delete_track` or `delete_playlist` must change).

The rule for the library is that it may describe, parse and encode. It does not
choose, own, or remember. A function that encodes a delete operation does not
establish that the phone will accept it, and a parser that finds a track does
not establish that the tool may touch it.

### 2.2 What lives in `rustunes`

- `cli`: argument parsing, progress, human output and the JSON record stream
  of `--message-format json`, the host's settings (the library directory),
  completions.
- `engine::planning`: source selection, validation, matching, additive,
  mirror and removal plans, metadata scheduling, saved plans and change plans,
  replacement proposals.
- `engine::execution`: the batch senders (insert, removal, metadata), single
  insertion, artwork, coordinations (Clear, replace-library, replace-track),
  `pull`, FLAC-to-ALAC conversion, device connection (USB and Wi-Fi).
- `engine::verification`: models of what each operation must change in all 45
  tables, and the physical audio and artwork checks.
- `engine::recovery`: settling interrupted work, the `Window` rule for
  playback between runs, retained-evidence recovery.
- `engine::state`: the journal and its carry-forward, the fingerprint, cover
  proof and source caches, the audio index, run directories and the files of
  retained evidence.

### 2.3 Why the split is where it is

- **Planning, journals, ownership, durable attempts, completion and recovery
  are policy.** They decide what is safe. They belong with the command line
  and are not a reusable mechanism. A separate engine crate was considered and
  rejected: nothing else would use it, and splitting it would force public
  interfaces for state that is deliberately internal.
- **Protocol and media code is mechanism.** It has no state directory, no
  journal and no opinion about ownership. Keeping it free of those makes it
  usable by a tool that only inspects a library or builds operation bytes.
- **The interfaces keep the split real.** The library writes its evidence to
  paths the caller supplies, rather than into the application's directories.
  Its read-back and comparison functions take explicit expectations rather
  than reading them from journal bindings. The shared mechanisms are in the
  library (the AFC open, read and stat calls, the raw catalogue reader, the
  removal model, the MP3 projection and timing); the application's durable
  attempt types and its evidence-file policy stay in the application.
- **Independent checks are kept independent.** Catalogue verification,
  physical audio verification and artwork verification establish different
  facts and are not merged for tidiness.

### 2.4 The ordering every write follows

Every write follows this sequence.

```text
validate and freeze the operation
  -> persist an immutable attempt            (journal: Prepared)
  -> mark Started before any device write    (journal: Started)
  -> execute a bounded session
  -> return pending verification evidence
  -> verify catalogue, audio and artwork independently
  -> commit completion                       (journal: Verified)
```

A session receipt describes what the protocol saw. It never by itself means a
track is verified.

## 3. The journal

### 3.1 Why a local journal exists

DESIGN, from what the phone does (chapters 02, 04 and 05).

- **The phone has no rollback across sessions.** A session either commits or,
  if the host abandons it early enough, rolls back. Once committed it stays.
  Nothing on the phone records which tool put a track there or why.
- **Interrupted sessions leave rows.** A host that stops mid-upload leaves
  catalogue rows with no file location. A host that stops after the phone
  applied a session leaves a changed library and no receipt.
- **Resume needs the exact Started content.** To decide whether an unfinished
  session was applied, the tool must know precisely what it intended to send:
  the operation bytes, the anchor it started from, the identifiers it
  reserved, and the catalogue it saw before. A summary is not enough. The
  journal therefore stores the exact content a send was built from.
- **Ownership needs a durable record.** Whether a track is the tool's to
  update, replace or prune cannot be read from the phone. It is recorded at
  insertion and checked again by independent verification.
- **Identifiers must never be reused.** Track, album and artist identifiers,
  destination paths and artwork cache identifiers are reserved before use and
  stay reserved after cancellation, completion or deletion.

### 3.2 What it records

The journal is one SQLite database in the tool's state directory
(`$XDG_STATE_HOME/rustunes`, by default under `.local/state` in the
home directory, at `devices/<device>/<library>/journal/journal.sqlite`),
scoped to one device and one library by a digest of the device and of the
library's persisted preferences. A printable library identifier alone is not
trusted as identity.

| Record | Holds |
| --- | --- |
| Scope | Device and library digests, a nonce |
| Reservations | Track, album and artist identifiers, paths, artwork cache identifiers; once consumed, never freed |
| Attempts | A single-track insertion or cover (artwork) update: Prepared, Started, Verified, Cancelled, Compensated, NotApplied (a Started insertion or cover that a fresh observation proved the phone never applied; its reservations stay), or Abandoned (a Started cover whose session ended after the phone took its metadata and before its image arrived; nothing is bound and its reservations stay); the frozen content and operation |
| Bindings and versions | A binding ties one source file's encoded audio to one phone track identifier and permanent path for one lifetime on the phone. It comes from the tool's verified insertion, or from a `pull` that matched the phone's file to a library file. Later tag or artwork changes add versions to the same binding. A verified deletion retires it |
| Source paths | The absolute path each binding lifetime's source had, recorded when it is bound: an insert batch's target, a single insertion, a pull. The mirror's removal and changed-audio replacement read it (section 4.5) |
| FLAC conversions | For each insert target converted from FLAC: the FLAC's path, size, file SHA-256 and audio identity, and the FFmpeg version line (section 9.5) |
| Tombstones | A retired binding stays readable so that the same audio can be added again as a new lifetime with a new identifier and path |
| Insert batches and targets | One signed session that adds several tracks: the operation, the anchor, the fresh Before receipts, each target's exact catalogue content (with its cover) and any new album or artist identity |
| Removal batches and targets | One signed session that removes several rows: the operation, anchor, Before receipts, each target's row, path, file bytes and hash, and the binding it retires. A removal batch may instead delete ordinary playlists, or be the removal half of a session that also inserts, paired with that session's insert batch (section 5.1) |
| Metadata batches and targets | One signed metadata-only session that edits named fields of existing tracks |
| Coordinations and steps | A Clear, a replace-library or a replace-track, with its phase, a revision for compare-and-swap progress, its exact intent, its starting anchor and an ordered list of the batches it ran |
| Insert targets removed by the phone | Rows the phone deleted itself after a session was abandoned (section 5.5) |

Invariants (DESIGN):

1. Ownership exists only in a binding the tool created by its own verified
   insertion or by an explicit `pull` that matched the track's audio to a
   library file. An explicit deletion of an unowned track never creates one.
2. At most one active binding exists for a given encoded audio. Earlier
   bindings, versions and receipts are never overwritten.
3. Only a verified deletion retires a binding. A missing file, a failed read or
   an omission from a plan is not a deletion.
4. Cancelling or failing never releases a reservation.
5. A state transition and the binding it implies commit in one transaction
   before success is reported.
6. A Started attempt never receives a second permit to send. There is no retry
   transition.
7. Journal receipts are local evidence. They are not attestations by the phone.

### 3.3 One schema, and why its records exist

DESIGN. The journal is created from one schema file. Every
open compares the stored table definitions with it exactly and checks the
SQLite application identifier. There is no schema version and no migration
code: a change to the journal is made by editing that one schema. A journal
whose definitions differ is Superseded and is carried forward from the phone
(section 3.5), never converted in place.

Single-track attempts, bindings, reservations and typed content, for ALAC and
MP3, are the core of the journal. Each other record exists for one class of
operation:

| Record | Why |
| --- | --- |
| Every route (clear, replacement, metadata editing, compensation) on one journal; attempts accept the Compensated state | One journal serves every route, so a journal used for one route never keeps another from starting |
| Coordinations, the parent of a clear or a replacement | A clear or replacement is many sessions. It needs one durable parent that records progress with compare-and-swap and owns the journal while active |
| Removal batches and their targets. A removal batch can compensate one Started insertion | A batch removes up to 499 rows in one session, which is what makes a full clear practical |
| Insert batches and targets; metadata batches and targets; coordinations link all four kinds of step. At most one native write of any kind outstanding, with the exceptions in section 3.4 | Adding a library one track per session is too slow. A batch carries many tracks and their covers. A batch can end Partial, so the journal must model "some rows reached the catalogue with a file" |
| The insert-target outcome "removed by the phone" | When the host abandons an insert session the phone may delete rows it had already created. Without this outcome such a batch could never be settled cleanly |
| `removal_playlists`: ordinary playlists one removal batch deletes, each by container identifier and the name it had | `clear` should end with an empty catalogue, which includes the emptied ordinary playlists. A batch removes tracks or playlists, never both |
| FLAC conversions, one row per converted insert target | Later runs match a FLAC source to the ALAC track converted from it without converting again (section 9.5) |
| The NotApplied state for single attempts | Without it a Started insertion or cover the phone never applied would have no way out, and every later write would refuse (section 7.2) |
| The Abandoned state for single covers | Without it a cover whose session ended after the manifest, with no image sent, would have no way out: `resume` would call it uncertain, and every later write would refuse (section 7.2) |
| Source paths per binding, the FLAC of a pulled binding, and the receipt kind of a pull | The mirror's removal and replacement name a track's source without retained run history, and `pull` records what it bound (section 3.5) |
| `with_insert_batch`: a removal batch paired with the insert batch of the same session, with triggers that keep the halves in one operation, before state and evidence and move them together | A sync's last removals and first additions go in one session (section 5.1) |

### 3.4 One writer, one outstanding write

- Only one native write of any kind (single insert, insert batch, removal
  batch, metadata batch, artwork) may be outstanding. Rust code and SQL
  triggers both enforce it. The exceptions are the removal batch that
  compensates a Started insertion or a Partial insert batch, and the two
  halves of one session (section 5.1).
- While a coordination is Active it owns the journal. Ordinary writes refuse
  unless the process holds its permit. Reads stay available, and `status` names
  the command that continues the coordination.
- A Prepared or Started batch blocks unrelated writes in the same way.
- One run per phone. The rules above cannot tell a session a live run is
  still sending from one a killed run left, and settling it (what `resume`
  does, and what an outstanding session's message tells the user to run)
  would act on a session still under way. So a run locks each phone it
  reaches until it exits, and another run that reaches the same phone is
  refused at once, naming the process that holds it. The lock is a `flock` on
  a private file in `locks/`, named by the device binding as `devices/` is,
  under the default state directory whatever `--state-dir` says. The
  operating system releases it with the process, even a killed one. Commands
  that do not reach the phone, such as `status`, take no lock.

### 3.5 Carrying a journal forward

DESIGN, CONFIRMED on the phone.

- **What is superseded.** A journal another version wrote: another schema,
  audio identities that this build computes differently from their stored
  components, or identity digests that place it at another location (a
  redefined device or library digest). Opening it fails with a typed error
  that names `pull`. It is never converted in place.
- **What carries it.** `pull` builds a new journal from a fresh capture of the
  phone. `sync`, `push`, `delete`, `metadata set` and the replacements do the
  same on their own before their work, and report what they carried. `verify`,
  `resume` and `status` name `pull` when they meet a superseded journal.
- **What is carried.** Each track the earlier journal bound and the capture
  still shows keeps its cover record, source path and FLAC conversion.
  A recorded source path that no longer exists gives way to the FLAC the track
  was converted from, then to the one library file holding its audio. It is
  found by track identifier, then by audio when the phone numbered it
  anew. Every other phone track whose audio exactly one library file holds is
  bound to that file and managed from then on (an MP3 must match byte for
  byte). The rest stay unbound and untouched. Unfinished work of the earlier
  journal is reported, not resumed: the new journal holds what the phone shows.
- **Covers are proven again.** A cover record names the cache identifier the
  image went under. After the phone was cleared and the library sent again,
  every cover is under a new one, and a matched track has no record at all.
  A pull reads each such phone cover back (grouped, as `verify` does) against
  the cover sync sends for the source file (an ALAC's own cover; for an MP3, a
  FLAC or any other format, the cover its insert batch sends), or the earlier
  record when the source has none to read, and a cover read back exactly goes
  into the binding. Without this the next sync would send every cover again,
  one phone session each; one that differs or cannot be read is still sent,
  and the pull reports how many. The sync and plan dry runs name each cover a
  run sends again (`COVER ... send the cover again`).
- **How the earlier journal is read.** From a private copy, by the columns
  every version has kept (bindings, with identities recomputed from their
  stored components, recorded and batch-input source paths, conversions,
  reservations and unfinished work). After an identity-digest change it is the
  journal elsewhere in the state directory whose bindings the capture confirms
  most.
- **Installation.** The new journal is built beside the one in use, reopened,
  and renamed into place. The one it replaces stays as `journal.previous`.
  `pull --dry-run` writes the pulled journal into its run directory and leaves
  the one in use.
- **The caches survive too.** The fingerprint and source caches recompute
  every audio identity they reuse from its stored components, and the
  fingerprint and cover proof caches key a phone's files by the SHA-256 of its
  UDID, which no version redefines (section 6.2). The first run after an
  upgrade therefore reads no phone file again.

Why: another build may have another schema, compute audio identities
differently or define the digests and hash domains anew. Without carrying, it
could not open the journal, and the phone would have to be cleared and its
library sent again. CONFIRMED: a journal of another schema was carried forward
on the real phone: 399 tracks carried, every source path recovered, `verify`
confirmed 399 audio files and 398 covers, and a mirroring `sync` matched all
399 with nothing to send or remove. A variant build with redefined identity
domains, run on an isolated copy of the state, found the journal where the
other digests had placed it, carried all 399 to its own location, and reused
798 cached fingerprints with none stored (16.7 s). A fresh host's `pull` bound
all 399 tracks to library files.

### 3.6 The read-only rule

DESIGN. Commands that only look (`status`, `verify`, dry runs, saved-plan
checks) never open the mutable original journal through SQLite. They copy a
bounded, quiescent database to a private location and read the copy. This keeps
a read-only command from advancing the write-ahead log or taking a lock on a
journal another run depends on. A later recheck of such a copy compares the
original's file identity instead of hashing it again. `verify` also never
writes the fingerprint cache (section 6.2). `pull` reads an earlier journal the
same way.

Post-write recovery opens the run's copy of the journal alone, after the phone
write, so a read-only open of a copy checks the schema and provenance only and
needs no file beside it. CONFIRMED.

## 4. Planning

### 4.1 The pipeline

DESIGN.

```text
select sources -> inspect and validate -> capture the phone's library
  -> match -> classify (Add, Keep, Update, Replace, Remove, Skip, Conflict, Excluded)
  -> schedule sessions -> show the proposal
```

A plan names the device and library binding, the current catalogue evidence,
the selected source paths and their hashes, every intended field change, the
transfer sizes, the identifiers it would reserve, the conflicts, and the
verification it will require. Machine output uses strings for identifiers to
avoid numeric precision loss.

Planning never guesses an allocation, and never silently turns an update it
cannot send into a delete and re-add.

**Skip and Conflict.** DESIGN. A refusal is a Skip when it is
about one source file, leaving that file out leaves its phone track exactly as
it is, and it cannot change how the other sources are grouped, numbered or
sent: a source that cannot be read or fingerprinted or changed while it was
read, a format or MPEG profile the phone does not take, tags the phone cannot
hold (a field given more than once, a missing album, a track number over its
total), a FLAC that cannot be converted, audio another selected file also has,
an MP3 that may be an earlier version of a phone track, and an owned track
whose source changed in a way this run does not send (changed audio, a cleared
genre, a field an edit cannot change). Every other refusal is a Conflict and
holds the whole run before anything is sent: the journal (unfinished work,
drift, no journal yet for insert batches), the phone's state (several tracks
with the same audio, a phone file that does not match its measurement), and
what sources share (albums and artists the planner cannot place, a composer
spelling it cannot reuse, a removed album brought back). The planner decides
every refusal first, then turns the Skips' refusals into Skips, so a skipped
source never takes an identifier, an album, an artist or a place in a batch,
and the others are planned as if it were not selected. `sync` and `push` leave
out the same files, and so do their dry runs, `plan` and `apply`.

A plan whose only refusals are Skips is ready, saved or not. The run ends by
listing each file it left out, with a plain reason and its fix; `--message-format
json` adds the refusal's code and full text, and the run directory's
`complete.json` keeps the list (section 10.7). A run held by a
Conflict lists what must be fixed first and, when the holds are files `push`
could leave out, the `push --exclude …` that sends the rest now. Leaving a file
out never removes or replaces its track: removals count it as present, and
`sync --replace-library` refuses while any file would be left out. Skips alone
do not change the exit code (0).

### 4.2 Source selection

- `sync` reads one library directory, set once with `config library DIR` and
  kept in the host's configuration (`$XDG_CONFIG_HOME/rustunes/config.json`).
  Without it, `sync` stops and names that command. `sync` takes no include or
  exclude options: the mirror is always the directory's files. `push`, `plan`
  and `scan` take a directory or one file and the options below; `prune`
  takes a directory and, of these, only `--recursive`.
- Directory selection is deterministic and reads only the directory's own
  files. `--recursive` (on `sync`, `push`, `pull`, `plan`, `prune` and `scan`)
  adds its subfolders. A track sent from a subfolder is out of reach of a
  selection without `--recursive`, so `sync` keeps it. Regular `.m4a`, `.mp3`,
  `.flac`, `.wav` and `.aif`, `.aiff`, `.aifc` files are selected. An `.m4a`
  that the ALAC reader refuses is read again as AAC.
- Symlinks and hidden child directories are excluded and reported. A root
  named explicitly on the command line is allowed even if its own name is
  hidden.
- `--include` and `--exclude` take exact root-relative files or subtrees, not
  globs. Exclusion wins.
- Identical copies of the same audio are shown. Different tag variants of the
  same audio are all left out, each naming the others, not an arbitrary first
  choice.
- Missing, unreadable, changing or invalid inputs block any prune that
  depends on them. An empty directory never becomes an instruction to erase.
- A filter that narrows the collection is never evidence that its other tracks
  were removed. The plan states exactly which source scope can authorise
  removals.
- Reading the sources is the largest planning cost. Each source is read once
  per process, and its facts are kept across runs in a source cache keyed by
  path, device, inode, size and modification and change times; a cover is kept
  once by its SHA-256 and checked against it. `--full-readback` reads and
  hashes every source again. A plan of the 399-track library from a saved
  snapshot fell from 92.8 s to 1.5 s with the cache warm.

### 4.3 Matching: identity is the encoded audio

DESIGN. Matching a source file to a phone row never relies on title, artist,
duration or file size alone. A match requires the encoded-audio identity, or a
verified readback of the phone's file.

| Format | Identity | Includes | Excludes |
| --- | --- | --- | --- |
| ALAC in M4A | `EncodedAlac` | The codec configuration (the cookie), sample timing, the media bytes the sample table references | Tags, artwork, box layout, padding |
| MP3 (MPEG-1 Layer III) | `EncodedMpeg1Layer3` | Sample rate, channels, frame count, audio extent, the SHA-256 of the actual frames | ID3 tags, cover pictures, trailing tags, padding |
| AAC, HE-AAC, xHE-AAC, FLAC, WAV, AIFF, MPEG-2 and 2.5 Layer III | `EncodedAudio` | Format, rate, channels, depth, packets, coded samples, payload bytes, SHA-256 of the codec configuration and of the coded payload. For AAC, the AudioSpecificConfig rather than the whole sample entry | Tags, pictures, padding |

Notes:

- **Why tags are excluded.** A user retags files constantly. If tags were part
  of identity, every retag would look like new audio and trigger a delete and
  re-add that loses play counts, ratings and playlist memberships. With audio
  identity, a retag is a metadata edit (section 4.7).
- **Why not decoded PCM.** Two encoders can produce different bytes for the
  same decoded audio. The tool matches the bytes that were sent, not a
  decoded equivalent.
- **Why the AAC identity uses the AudioSpecificConfig.** A remux with
  `ffmpeg -c copy` rewrites the bit-rate fields of the sample description, so
  hashing the whole description would make a remuxed file look like changed
  audio. CONFIRMED: a copy retagged this way matched and produced a tag update.
- **Why padding is excluded.** Some AAC encoders leave bytes in `mdat` that no
  sample uses. They are sent with the file but stay outside the identity, so a
  remux that drops them keeps the same identity. ALAC still requires exact
  coverage.
- **A retag is not a changed-audio event.** Different tags with the same
  identity plan an Update. Different identity at the path the track was
  inserted from plans a replacement.
- **Existing rows without an owned binding** are matched by verified audio
  readback, for comparison only. Matching in a plan never grants ownership;
  only an explicit `pull` turns such a match into a binding (section 4.8).
- **A converted FLAC matches its ALAC track.** A FLAC whose audio the journal
  links to an active ALAC track converted from it plans as that track (Keep,
  or an Update of changed tags), with or without `--flac-to-alac` (section
  9.5).
- **Plausible but unproven matches are left out.** An MP3 without matching
  audio whose title, artist or album and duration (within a second) match a
  catalogue track is skipped as "plausible existing audio requires
  comparison", naming the track. It is not planned as a second Add.
- **Name comparison ignores ASCII case.** The phone keeps one spelling per
  group and merges artist and album spellings that differ only in ASCII case,
  so the planner reads such names as no change and sends edits in the phone's
  stored spelling. CONFIRMED.

### 4.4 Dry-run and saved plans

- `plan DIR --snapshot S --journal J` is fully offline. It reserves nothing,
  changes no ownership and never connects.
- `sync --dry-run` does the same against a fresh read-only capture of the
  phone.
- `apply PLAN` recomputes the plan exactly and refuses stale input: changed
  device or library, changed sources, a stale inventory, unfinished work or an
  unsupported action. `apply --check-only` reconstructs the plan offline.
  `apply --dry-run` adds a live read-only preflight.
- A saved plan is compared as exact bytes or its compact encoding. Edited or
  duplicate-key documents are refused. The way to change a plan is to compute
  it again.
- Update rows are executable from a saved plan: the metadata sessions are
  scheduled against the plan's own snapshot before anything is written.
- A dry run is not authority to execute. Every execution re-checks fresh state.

**Change plans: plans that remove tracks first.** DESIGN,
CONFIRMED on the phone. `plan` with `--remove-missing`,
`--add-remove`, `--replace-changed` or `--replace-library` saves a change plan
(`rustunes.change-plan`). It names:

- each track removed, why (source gone, audio changed, a field an edit cannot
  clear, whole library), its plays and skips, and the source that comes back
  in its place;
- the additions (each with its source file's SHA-256), updates and covers of
  the sync that follows the removal;
- the costs: removal sessions of up to 499 tracks (the session size), bytes
  to upload, plays and skips lost;
- every blocker: an unfinished journal operation, an Active coordination with
  the command that continues it, a track or batch the removal model refuses, a
  conflict in the sync, a source a library replacement cannot add.

The removal goes first. The sync after it plans again from a fresh capture, so
new tracks get their identifiers then and the plan names none. `apply` rebuilds
the change plan from its snapshot, sources and journal and requires the same
bytes (`--check-only` stops there, offline), then runs the same steps as the
matching `sync` from fresh captures, each pinned to the plan. The removal must
select exactly the planned tracks, and the sync after it must add exactly the
planned files (path and SHA-256) and send exactly the planned updates, or that
step refuses before it sends anything. Play counts that change on the phone do
not stop it; a different selection does. A replacement's additions can only be
planned once its tracks are gone, so its dry run checks the removal alone and
says so.

On the phone, over Wi-Fi with test tones: an add-and-remove plan applied in one
verified insert batch of two ALAC and two MP3 tones; a plan of one removal, one
addition and one update applied as a removal batch, an insert with its cover
and a metadata session; a mixed ALAC and MP3 replacement killed right after its
removal batch made fresh plans block and name the command that continues it,
and that command finished it; a delete killed after its session applied made
every plan refuse until `resume` settled it. A plan replacing the whole
399-track library listed 399 removals, each paired with its source, and 8.2 GB,
and `apply --check-only` rebuilt it exactly.

Limits: the sync after the removal is pinned by its actions, not by
identifiers, so if the removal changes what it would do, `apply` stops with the
removal done and asks for a fresh plan. One saved plan holds a missing-source
removal or a replacement, not both. A plan without a removal
(`plan DIR`) is applied the same way: rebuilt exactly, then the sync it
describes runs from a fresh capture, pinned to its additions and updates, with
the insert batches and covers `sync` sends.

### 4.5 Push, mirror, replace and pull

DESIGN.

| Mode | What it does | What it never does |
| --- | --- | --- |
| `push [PATH]` | Adds what is missing from a directory or one file (the library directory by default), keeps existing catalogue metadata even when it differs from source tags, and sends covers | Update or remove anything. A source that disappears is not a delete request |
| plain `sync` | Mirrors the library directory: removes tracks whose source left it (below), adds what is missing, sends metadata Updates to owned tracks whose audio is unchanged, finishes covers. Changed audio at an owned path is left out and its track kept | Replace changed audio. Remove a track whose audio is still in the library under another path, or a track sent from outside the library |
| `sync --replace-changed` | Removes owned tracks whose source audio changed, verifies them gone, adds the changed sources, and also takes tracks whose source cleared a genre or composer | Touch unowned tracks. It states the losses in its proposal: new identity, play and skip counts, ratings, playlist memberships |
| `sync --replace-library` | Removes every track from the phone, verifies an empty library, adds the library directory back | Keep old identities or statistics |
| `pull` | Rebuilds the journal from a fresh capture (section 3.5). `--tracks` copies phone tracks no library file holds into the library directory (or `--into DIR`) and binds them there | Send anything to the phone, or overwrite a file |
| `prune DIR` | Removes owned tracks whose source is absent from the exact original full directory | Remove unowned, unrelated or still-present tracks |
| `delete ID...` / `clear` | Removes named tracks or every track, after a proposal and confirmation, including tracks the tool did not insert | Delete cloud purchases or subscriptions |

Both replacement modes run as one coordination (section 5.4) and are
resumable by running the same command again. A track pushed from the library
directory is part of the library, and the mirror removes it once its file
leaves. One pushed from elsewhere stays through every `sync`.

**Removing tracks whose source is gone.** DESIGN, CONFIRMED on the phone.

- A track's source is the absolute path it was inserted from, which the
  journal pins: the source path recorded with its binding, or, for a track
  bound before that record existed, its insert batch's retained input or its
  single insertion's input and plan. The mirror removes the track when that
  path lies in the library directory and no file is there.
- A track whose encoded audio is still in the library under another path (a
  moved or renamed file) is kept, and the sync matches it there. A converted
  track's source is its FLAC, not the temporary ALAC.
- Tracks the tool did not insert, tracks sent from outside the library and
  tracks whose evidence names no path stay. The preview counts the last.
- Sources are read only once one is found gone. Then an empty library removes
  nothing and stops the run before the sync. A file that cannot be read or
  fingerprinted might be a moved track's audio, so the sync removes nothing
  this run, names the files, sends the rest, and exits 3.
- A skipped file still counts as present: it is at the path the journal pins,
  so its track is not removed, and a skip is never a replacement.
  Just before sending, each selected path is checked again; a file put back
  refuses.
- The removal lists the tracks and asks (`--yes` for scripts); declining
  removes and sends nothing and suggests `push`. It runs as `delete`'s verified
  removal batches of `--batch-size` (499 by default), the last of which, if it
  is not full, goes with the sync's first new tracks in one session (section
  5.1). Running `sync` again settles an unfinished batch or session first.

On the phone: with a covered ALAC, an MP3 and a singly inserted test tone
deleted and another covered tone moved to a subdirectory, the preview selected
exactly the three and kept the moved one, which the sync matched at its new
path. The removal batch moved a genre's representative to a covered member
(chapter 05) and settled Verified. With the test album's directory emptied, an
`--include` of it blocked as an empty source. A run that added one tone,
removed one and kept a retagged one's phone tags ran as one removal batch and
one insert batch. A mirror of a scratch library added two tones,
refused its removal without `--yes`, removed one with it, and sent a retag that
`push` had left alone. The real library's dry run named a
pinned source for all 399 tracks: 397 in the library and 2 sent earlier from a
scratch directory, which stayed.

### 4.6 Mixed-format batches

DESIGN. Ownership of an existing binding is checked by one typed comparison:
exact track identity, permanent path, file length, full-file hash and the
whole recorded codec layout. For MPEG it also requires sample rate, channels,
frame count, audio offset and length, and trailing-tag length. Survivors of
either format are therefore checked after any operation.

Between steps the engine also tolerates one known maintenance property that the
phone advances between sessions (an artist-image import revision). A next-step
refresh compares all 45 tables with the previous state and allows only that
property to catch up to the unchanged maximum entity revision. CONFIRMED
on a mixed ALAC and MP3 run.

An insert batch may mix ALAC with any other supported format. CONFIRMED on the
phone: one session added three ALAC tracks, four MPEG-1 MP3s, an
MPEG-2 MP3, and AAC, WAV and AIFF tracks, and verified. Batches of
one ALAC and one MP3 were also killed at chosen points and settled by `resume`
(section 5.5).

### 4.7 Metadata edit planning

DESIGN, CONFIRMED on the phone.

- **Two entry points, one sender.** `metadata set TRACK --title ... --genre ...`
  edits named fields of one track. A plain `sync` drives the same sender from
  changed source tags.
- **What an Update contains.** The track identifier and each field's current and
  desired value, the comment and the sort names included.
  `push` keeps catalogue values and reports differences.
- **Receiver rules decide what a session can carry.** The phone applies groups
  by name and revision. An artist or album-artist edit is safe only for the
  group's stored representative. One representative may leave a group per
  session. A year change and an album join are separate. So the planner
  simulates the whole sequence on the capture, including the verifier's
  representative hand-over, and packs what each session can carry. Renaming
  an artist shared by two tracks takes two sessions. If the sequence could
  not finish, nothing is sent and the plan says how many sessions it needs.
- **Cleared fields.** A year, track number, track count, disc count or
  copyright the source no longer has is cleared (0 for numbers, empty text for
  copyright). A genre or composer cannot be cleared by an edit. The plan gives
  the conflict "metadata update needs replacement" and `--replace-changed`
  takes it up.
- **Refused as conflicts.** Content rating, duration, and any track the tool
  did not insert.
- **MP3 specifics.** An owned MP3 whose frames are unchanged is matched to its
  binding while the phone's file is still the one the tool sent. Its desired
  fields are the insert-batch projection of the new tags. The phone's file
  keeps its old tags, as an ALAC container does. A different front cover is a
  conflict, because an MP3 cover is sent only with its audio.
- **The phone numbers revisions in its own order.** A session touching several
  groups showed that the phone numbers each group once, by class (album
  artist, album, artist, composer, genre), then the items in operation order.
  The verifier follows that.

### 4.8 Ownership, and why unowned rows are never touched

DESIGN.

- A track is owned when the journal holds a verified binding from this tool's
  own insertion, for this device and library, or a binding an explicit `pull`
  made. Filename, source membership, similar tags or a matching hash found
  while planning are not ownership.
- Matching an existing phone track by verified readback lets the planner
  treat it as Kept. It grants no prune right.
- Prune, metadata Update, and changed-audio replacement act only on owned
  tracks. Explicit `delete ID` and `clear` can remove unowned tracks, but only
  after a proposal that counts them and a confirmation, and they never create
  ownership.
- Why so strict. The phone has no field that says who added a track. If the
  tool guessed, one wrong guess would delete a track whose play history exists
  nowhere else. A conservative miss costs a re-add. A wrong delete costs the
  track.
- A pull is the one way to take tracks over: `pull` itself, or the carry that a
  command runs when it meets a superseded journal. It binds a phone track only
  when exactly one library file holds its encoded audio (an MP3 byte for byte).
  That is not a guess: the audio the phone plays is the audio of the file the
  tool will manage. From then on `sync` may update and remove the track like
  one it sent. A track no library file holds stays unbound unless
  `pull --tracks` copies it into the library and binds it to the copy.
- If the journal is lost, `pull` rebuilds it from the phone (section 7.6).
  Without a pull, existing tracks stay untouched: they can be matched and kept,
  but not pruned or updated. A new empty journal does not resolve unfinished
  work.

## 5. Execution

### 5.1 One session per batch

DESIGN. Each insert, removal or metadata batch is one signed metadata session.
Each is planned from its own fresh capture of the phone, so a long run is a
sequence of short, independently verified sessions rather than one long plan
that goes stale.

A full restore ran in removal and insert batches of 100, and a library re-add
in insert batches of 8. Removal batches up to 100 targets are verified on
the phone. CONFIRMED. Insert batches at 100 tracks with covers:
CONFIRMED. Sessions of 499 are verified too (below), so every command that
sends fills each session up to 499 operations by default, removals of `sync`,
`apply` and the replacements included.

DESIGN: a session stages one operation file of at most 500
operations, `update_db_info` included, the most Apple's host writes in one file
(chapter 08, section 5). Apple's host splits a larger sync over
several files of one session; what the phone does with several is untested, so
the tool caps instead. `--batch-size` on `sync`, `push`, `apply` and the removal
commands counts a session's operations besides `update_db_info`, from 1 to 499:
each new track is one `insert_track` (its cover rides inside it) and each removed
track one `delete_track`. Metadata sessions pack at most 499 `update_track`. A
Clear deletes its ordinary playlists in one session, so it refuses, before
removing anything, a phone with more than 499 of them. Each operation file
builder refuses more than 500.

CONFIRMED over Wi-Fi, with covered ALAC test tones: one insert
session of 200 tracks (201 operations) and one of 499 (500 operations), and
one removal session of each size. Each was verified, moved the anchor by one
and, for removals, the phone collected every album and artist it emptied. The
499-track push took 154 s from start to verified, its removal 27.2 s. Two
rules let such sessions run:

- The guard connection of an insert batch checks each member's namespace and
  cover, about 40 AFC operations per covered member, so it may take up to
  32768 operations, enough for 499 covered members beside the largest
  catalogue read. Every other connection takes at most 8192, and a batch over
  the bound is refused before it is prepared.
- A large session may give every prior track new sort-key orders in the same
  relative order (chapter 05, section 1), as the 200-track session did. The
  batch verifier accepts prior orders that move while keeping their relative
  order.

**Removals and additions in one session.** DESIGN, CONFIRMED on the phone
(below). Apple's host sends a sync's removals and additions in one session,
removals first (chapter 08, section 6). A mirroring `sync`, and `apply` of a
plan that removes and adds, send full removal batches first and leave the
removals that do not fill a batch to the first insert batch, whose session
removes them before its insertions within the same 499 operations. With 600
removals and 100 additions that is two sessions: 499 removals, then 101
removals and 100 additions. The phone applies such a session as one (chapter
05, section 5).

- **Journal.** The session is an insert batch and a removal batch paired with
  it. Both hold the one operation, before state and evidence; they are
  Prepared, Started and settled together, the removal half first.
- **Verification.** One observation settles both halves. Every removed track
  must have left the catalogue and its file the phone. The insertions are
  verified as an insert batch's, against the before catalogue with the removed
  tracks taken out: their rows, statistics and track cover registrations go,
  their groups count one member fewer each, and a group they emptied stays
  unless the phone collected it. A session the phone did not apply settles
  both halves NotApplied. Rows left without audio make the insert half
  Partial, and are compensated as in any insert batch, while the removal half
  is Verified.
- **When removals go alone.** The session is verified against the catalogue,
  which holds no playlist, so a removal of a track in an ordinary playlist
  stays in a removal batch of its own. So do the left removals when the sync
  is blocked, and when it sends no insert batch or its plan matches one of
  their tracks: they go first and the sync plans again without them, from a
  fresh capture, since its plan described the phone before they went.
- **Recovery.** `resume` and the next `sync` settle an unfinished session
  through its insert half: a Prepared one is cancelled, a Started one is
  observed and never sent again. The rows a killed session left without audio
  must be its insert half's own targets; the next `sync` settles the session
  first and captures the phone again, while its dry run names the `resume`
  that settles it.
- **On the phone.** Mirroring syncs of a scratch library sent 5 removals and 2
  additions, then 249 and 250 (500 operations, about 75 s), each as one
  verified session moving the anchor by one. A session of 50 and 50 killed
  after 20 uploads was settled by running the sync again: the removal half
  Verified, the insert half Compensated (30 rows without audio removed), and
  the same run sent the 30 again.

### 5.2 Anchors and revisions

DESIGN (protocol facts in chapters 02 and 04).

- Every library has a media anchor. A session announces the anchor it starts
  from, and the phone answers with the anchor it holds. The envelope's
  revision is the anchor plus one.
- The tool reads the live anchor from a fresh capture and never infers it by
  counting its own sessions. The phone also changes the anchor for reasons
  the tool did not cause.
- The signing key comes from the session's own Ready exchange, so a plan
  cannot be signed in advance.
- The anchor tells the tool whether a session landed. Anchor unchanged and
  catalogue unchanged means nothing was applied. Anchor advanced and the
  catalogue matches the model means applied.

### 5.3 The send sequence and its guards

For each batch:

1. Capture a fresh Before: catalogue (SQLite and its log, all 45 tables),
   a complete audio index, and the artwork state. An insert or removal batch
   takes the run's latest complete snapshot as its Before instead when a fresh
   catalogue read shows it unchanged; a snapshot whose catalogue differs is set
   aside and captured again.
2. Run read-only guards before any journal row exists: the namespace and the
   state agree with the plan. A catalogue that drifted costs nothing,
   because no reservation was made. If the first guard fails with a typed size
   or digest mismatch, one refresh is allowed within the original deadline,
   rebuilding the same operation from the same identifiers.
3. Check each target against the removal or insert model on that state. Refuse
   shapes the model does not cover.
4. Journal Prepared (immutable operation, anchor, Before receipts, reserved
   identifiers).
5. Journal Started with the signature, durably, before the first phone write.
   Every file-service connection starts locked (DESIGN): a request
   that would create, write, rename or remove is refused before it is counted
   or sent, and only stat, list, read, close and read-only opens pass. The
   session unlocks a connection only with a Started record that carries the
   signature its own Ready exchange made for the exact metadata bytes; it then
   writes the metadata and the signature. The batch insert's upload connections
   unlock the same way. No code path can write before Started is durable.
6. Send. For inserts: write each cover and file, read back what must be read
   back, answer the phone's requests, finish.
7. Capture a fresh After and verify it against the model (section 6).
8. Settle: Verified (bindings created or retired in the same transaction),
   NotApplied (sent, and observation proves nothing changed) or Partial.

Other rules in the sender:

- Files go up in reverse operation order. The phone gives an album the cover
  of its last member in the operation and registers it before any file
  arrives. If a batch stops, an album whose members partly arrived should
  already have a cover that arrived. CONFIRMED.
- Covers travel in the same session as their audio (asset parts: track and
  artwork), because one artwork session per cover would add hours to a full
  restore. An ALAC track whose album already has live tracks sends its cover
  in a follow-up cover step; other formats send it in the batch (section 9.3).
- An insert batch uploads over as few connections as the file service's
  per-connection allowance of 1 MiB requests fits, and checks the shared
  staging folders once per guard rather than once per member.
- Output of the phone's own housekeeping during a session (a Ping, a request
  for assets it did not need) is answered and recorded, not treated as a
  fault.

### 5.4 Coordinations

DESIGN. CONFIRMED on the phone.

A coordination is a journal parent that lets a multi-step operation be
interrupted and continued. Three kinds exist.

| Kind | Phases |
| --- | --- |
| Clear | Remove every track in verified removal batches. Then delete every ordinary playlist in one more removal batch. Closes when a fresh capture shows no track, no indexed audio and no ordinary playlist |
| Replace library | Phase 0: remove every track (the same removal batches). Phase 1: add the library directory with the batched add-only sync of `push`. Completes when a fresh add-only plan has nothing left to add and no cover is pending |
| Replace track | Phase 0: remove the old owned tracks whose source changed. Phase 1: add the changed sources. Completes when a fresh add-only plan has nothing left |

Rules:

- Each batch, single insert and cover is linked to the coordination as a
  step, in order.
- Every phase starts from a fresh capture.
- **Run the same command again to continue.** It continues only with the
  library directory it began with. The refusal for any other command, and
  `status`, print the exact command that continues it.
- Continuing cancels a single insert that the stopped run prepared but never
  sent. A batch or insert that was started needs settling first, and `resume`
  does that under the coordination's own permit, leaving the coordination
  Active for its own command to finish.
- A replace-library refuses to start while a library source cannot be added,
  so the tool does not empty a phone and then find it cannot refill it.

### 5.5 Kill points and compensation

The processes that matter are the ones where the tool dies at an unlucky time.
The table lists what was observed on the phone and what the tool does.

| Kill point or fault | What the phone did | What the tool does |
| --- | --- | --- |
| Removal batch, before send (Prepared) | Nothing | `resume` or the next run cancels it. Nothing was sent. CONFIRMED |
| Removal batch, killed at the phone's first Progress message | Rolled the session back, but may already have deleted a target's audio file | Observation finds the anchor unmoved and nothing changed: settled **NotApplied**. A later removal accepts a row whose file is gone, but only for a track named by a batch the phone did not apply. The next batch removes the row with no file to wait for. CONFIRMED |
| Removal batch, killed at AssetManifest or SyncFinished | Applied the session | Observation matches the model: settled **Verified** without resending. CONFIRMED |
| Playlist-deleting removal batch, killed at the first Progress | Applied it (unlike a track session killed there) | Settled Verified. The NotApplied path of a playlist batch has not been seen. CONFIRMED |
| Removal batch applied but the verifier's model was too narrow (album status, hero revision, stats revision of an emptied album, playlist evaluated for keeping local) | Applied | Batch stays Started. After the model is corrected, `resume` or the same command settles it without resending. CONFIRMED several times |
| Insert batch killed mid-upload | Created every row up front; some files arrived, some did not | `resume` settles **Partial**: arrived files are bound with their covers verified, files absent leave locationless rows. A **compensating removal batch** removes exactly those rows. The insert batch becomes Compensated. CONFIRMED |
| Insert batch abandoned before any upload | Deleted some of its own rows, kept the rest without audio | Rows the phone deleted are recorded as **removed by the phone** (a fourth target outcome, section 3.3). The remaining locationless rows are compensated. CONFIRMED |
| Insert batch where the host stopped before completing a non-ALAC destination | Row created, no audio | Compensated. Completion takes the MPEG path for `.mp3` destinations. CONFIRMED |
| Phone restarted: the `Airlock` staging folders are gone | Folders absent | Inserts, batches and cover updates accept the staging folders absent and create them after Started, before the first upload. CONFIRMED |
| Catalogue written between the audio index and the plan | Moved the catalogue log | One recapture within the original deadline before anything is sent. A batch that was never sent costs nothing |
| Single insert, killed at the first Progress | Rolled back: anchor and track set unchanged, staging empty | `resume` proves it from a fresh observation: settled **NotApplied**. CONFIRMED |
| Single insert, killed mid-upload (2 of 13 MiB sent) | Applied the metadata: a row with no file | `resume` settles it **Compensated** by a verified removal batch. CONFIRMED |
| Cover, killed at SyncFinished before the run's own receipt and After capture | Applied | `resume` settles it **Verified** from the session transcript, after a play and a skip on the phone, by the later-run rule (section 7.3). CONFIRMED |
| Cover, refused or killed after the AssetManifest, before its upload completed | Applied the metadata: the track and its album took the new cache identifier, no image arrived, both kept showing the old cover | `resume` settles it **Abandoned** (section 7.2). The binding keeps its last verified cover and the next sync sends the cover again; that update moves both to its own identifier and the dangling one leaves the catalogue. CONFIRMED, the settling included: a cover killed at the AssetManifest settled Abandoned, and the next sync's update verified |
| Insert batch of one ALAC and one MP3 with covers, killed after AssetManifest, before any audio | Both rows without audio | **Compensated**. CONFIRMED |
| The same batch shape, killed at the fourth 1 MiB write of the ALAC, after the MP3 and its cover | MP3 complete, ALAC row without audio | The MP3 is bound with its cover read back exactly; the ALAC row is **compensated**. CONFIRMED |
| Metadata batch, killed at the first Progress | Rolled back | **NotApplied**. CONFIRMED |
| Any batch, killed after staging its operation pair, before FinishedSyncingMetadata | Applied nothing and kept the pair in `iTunes_Control/Sync/Media` | `resume` proves the pair its own and never finished, removes it and settles **NotApplied** (section 7.2). CONFIRMED, the removal included, for an insert, removal and metadata batch and a single cover |
| Metadata batch, killed at SyncFinished | Applied | **Verified**. CONFIRMED |
| Insert batch with `--flac-to-alac`, killed two seconds in, its conversion directory then deleted as a reboot would | Applied | `resume` converts the FLAC again, requires the bytes the batch sent, and settles **Verified**. CONFIRMED |
| A rolled-back or never-sent removal batch while the process lives | Rolled back | The run retries once as a new batch from fresh state. A second such result stops the run. Unit tests only. OPEN on the phone |
| Failed observation after a removal | Applied or not | Retried up to three times before the run stops |
| Process dies between batches | Library partway through | Run the same command again: it settles the unfinished batch, then selects from fresh state |

In every one of these kill runs `resume` sent nothing again, and a `sync` met
by the unfinished run refused and named the `resume` that settles it.

The kills mid-upload also left partial files in the phone's `Airlock/Media`
staging folder: two audio files whose sizes end on a 1 MiB request boundary,
and one cover. No journal names them and no catalogue row refers to them.
A research tool listed and removed them (each checked as a
regular file of that listing with its stat unchanged, removed by one request,
absent afterwards), and `verify` still passed. The product does not clean
`Airlock` itself.

### 5.6 The compensating removal batch

DESIGN. CONFIRMED.

A compensating batch removes only rows that have no file location and no
asset. Its operation uses a form that deletes only unfilled rows. Asset
requests for such rows are refused. No file may exist at their planned
paths. It is verified by the same 45-table model as any removal.

The reason is that the phone created these rows before the file arrived, and
an unfilled row can break later commands: the index capture and verifiers
treat a row without audio as damage. Removing them with the ordinary
removal machinery, but with no ordinary ownership claim and no file to wait
for, returns the library to its earlier state.

The same batch kind settles one Started single insertion: `resume` captures
the phone first and compensates when the anchor moved and the insertion's row
has no file location. CONFIRMED.

## 6. Verification

### 6.1 What is compared

Three separate checks, never merged:

| Check | Compares | How |
| --- | --- | --- |
| Catalogue rows | A fresh read of the catalogue (all 45 tables, the saved companion files, the preferences) against an exact model of what the operation must change | The model is a set of rules per operation: a removal retires an item's rows and artwork tokens, leaves a tombstone revision, hands group representatives to a surviving member or zero, closes up playlist positions, and moves the anchor once. Every other cell must be unchanged, apart from recent play counters, the maintenance clock and the state the phone fills in on an entry it made itself ([chapter 4](04-media-library-database.md), section 3.1) |
| Audio bytes | Each file on the phone against the source or the recorded fingerprint | Stat and fingerprint cache for known files; full copy and hash for new or changed ones; full readback on request |
| Artwork renditions | The cover original and the phone's derived rendition | Read the permanent original, decode the rendition with FFmpeg, compare per the verifier. Grayscale JPEG covers leave a 0-byte rendition on the phone; the verifier accepts that when the original is exact. A cover under 144 px on its longer side gets no rendition; the verifier accepts its exact original alone. Renditions whose longer side equals the cache edge are accepted. For a PNG source the expected original is the JPEG the tool made from it (section 9.3) |

Surviving tracks are checked after every operation: their audio is unchanged
and their catalogue rows are unchanged except for effects the phone is known
to add by itself (section 7.3).

### 6.2 Proportional verification

DESIGN. The rule: do not reread the whole library by default.

- The **fingerprint cache** is a dedicated SQLite database, separate from the
  journal. A row is keyed by the phone, path, size, birth time and
  modification time (one row per path), and holds a fingerprint of
  the file. A hit means the tool already proved those bytes on that phone.
- The phone's key is the SHA-256 of its UDID, not the journal's device digest:
  that digest carries a hash domain, and a release that redefined it would make
  every entry miss and copy every file on the phone again (about ten minutes
  over Wi-Fi for 8.2 GB). An entry's composite audio identity is recomputed
  from its stored component hashes on load, so a changed hash definition does
  not invalidate it either. CONFIRMED: a variant build with a redefined device
  digest reused all 798 fingerprints and stored none.
- A **copy** stores the stat it saw before and after. An **upload** stores its
  source's fingerprint, so the capture that follows a batch reads no audio
  back.
- The phone's move of a new file from the staging folder into the music
  folder keeps both times, so the cached entry still matches the permanent
  file. CONFIRMED. The phone also trims a new file's spare
  allocation shortly after the write, so allocation (`st_blocks`) is not part
  of the stable attributes.
- A bad entry is a miss. An unreadable cache database is renamed aside and
  rebuilt. A cache failure never fails a run. CONFIRMED.
- `--full-readback` copies everything, compares it, and refreshes the cache.
  Use it for audits. After a full restore it read back 399 uploaded files and
  every byte matched. CONFIRMED.
- `cache stats` and `cache prune --unseen-days N` are explicit. Pruning is by
  age because one phone's index cannot show that another device's entries are
  unused.
- Covers are checked in groups. The phone's AFC service tolerates a bounded
  number of operations per connection, so a group shares one connection and
  one pair of catalogue reads. The decode runs one FFmpeg process per group of
  up to 64 covers, with `-noautoscale` and frame-hash output, instead of one
  process per cover. All 394 decoded renditions hashed as the per-cover decode
  did. CONFIRMED.
- A **cover proof cache** keeps each cover proof whose readback, decode and
  closing guard passed, keyed by the phone stats of the cover's original and
  of the selected rendition. `verify` reuses a proof while
  both files keep those stats: it stats each before and after and probes the
  renditions, and reads or decodes nothing. Its report says how many covers it
  matched to verified proofs. Insert batches store proofs but never reuse
  one. `--full-readback` reads every cover again.
- **Stats are pipelined**: up to 64 file-information
  requests are in flight on one connection. The phone's file service answers
  them in order, and each reply must carry its request's packet number. Apple's
  own sync stays lock-step. `verify` stats every file of an audio group before
  reading any and again after the last read, and asks for all cover originals,
  then all rendition probes, then all closing stats at once. An index pass that
  copied nothing is not stat again at its end.
- `verify` never writes the fingerprint cache; it does store cover proofs.
  Experimental runs use a scratch copy of the state directory (section 8.5).

### 6.3 Why verification never hashes on the phone

CONFIRMED. The AFC hash request would let the tool identify a phone file
without copying it: the phone returns the file's SHA-1. But the request leaks
inside the phone's file service. A probe on a freshly restarted phone showed
12,000 opens and closes all succeeding, but the 2,551st hash request failing
with a resource error. After that every open and every hash request failed,
while stat and device-info requests still worked. The service is one
long-lived process, so the leak survives connection close and waiting. Five
idle minutes did not clear it. Only a phone restart did.

Consequences:

- A library of more than about 2,500 tracks would wedge the phone on the first
  snapshot.
- The tool does not send the hash request, and its read-only AFC transport
  refuses it. DESIGN: never add it back.
- An upload is checked by the replies to its write, size, close and rename
  and by a stat of the stored size. Uploads, index captures and `verify` all
  use only stat, open, read and close.
- Control run, one connection each: 12,000 stats, 6,000 open-read-close and
  12,000 opens all succeeded and afcd kept opening files.
- Apple's own sync never asks for a hash either.

### 6.4 The connect-checked rule and "dropped by phone"

DESIGN, from phone behaviour.

- The phone sometimes refuses a new AFC connection, or drops it before its
  first reply. The tool opens every new AFC connection through one function
  that sends a device-information request first, reopens if the connection
  was never served, and restarts packet numbering at zero afterwards. Every
  site that opens a connection must use it. Evidence files that record packet
  numbers from zero are unchanged by the restart.
- A typed check, `dropped_by_phone`, recognises the phone refusing or dropping
  a connection, by error type and never by message text. The phone also
  breaks established connections (a broken pipe after 26 operations was seen).
- **Read-only steps retry once** on such an error: the state capture, index
  passes and verification restart on a new connection. **Write steps never
  retry.** A write that failed in this way is settled by observation, not
  resent.

### 6.5 What verification does not prove

A session reply is not verification. An AFC file in place is not
verification. Playback and display are separate observations. After the
tool's own checks pass, the phone plays the tracks; that is recorded
separately (section 12).

## 7. Recovery

### 7.1 `status` and `resume`

- `status` is local. It reads the journal and prints outstanding work and the
  command that continues it: a single attempt, a removal, insert or metadata
  batch, or an Active coordination.
- `resume` reads fresh phone state and settles unfinished work. It never
  resends an uncertain operation and never resumes an old batch implicitly.
  After it settles something, a fresh plan follows.
- `delete`, `prune`, `clear` and the mirror's removal settle a dead run
  themselves: running the same command again settles the unfinished removal
  batch first, then selects from fresh state, so no separate `resume` is
  needed. A dry run changes nothing.
- A `sync` met by any other unfinished work refuses before planning and names
  the command that settles it: `resume --cancel-prepared` for a Prepared single
  attempt, which sent nothing, or the `resume` with the run that holds the
  attempt's or batch's input for a Started one. For a batch, whose journal
  record names its evidence, the snapshot only identifies the phone; the
  refusing run's own capture serves, even with the rows the batch left
  without audio. A single attempt still needs a snapshot in which every row
  holds its audio.

### 7.2 The settling rule

For a batch or attempt left Started, the tool takes a fresh capture and
compares it with an exact model of the write applied to the retained Before
state.

- Matches the model: Verified (bindings created or retired in the same
  transaction).
- Anchor unmoved, catalogue unchanged and staging empty: NotApplied. A
  staging folder holding only the record's own pair, never finished, is
  emptied first (below).
- Rows without audio: Partial, then compensation (section 5.6).
- Neither: the batch stays Started and the tool says so. A model that is
  too narrow for a new phone behaviour is corrected in the code, and then
  `resume` settles the same batch from the same saved state. This happened
  several times; none needed anything resent.

A single Started insertion or cover is observed before anything else
(DESIGN, CONFIRMED). `resume` first captures the phone, allowing the
attempt's own row to lack audio, then decides:

- **NotApplied**: the anchor is still the prepared one, the insertion's row is
  absent (for a cover, its track holds neither the new cache identifier nor
  any trace of it), the track set equals the given snapshot's when that
  snapshot holds the prepared revision, no file is at the planned path and the
  phone's media staging folder is empty, or holds only the attempt's own pair,
  never finished (below). The proof is kept, and its digest is the attempt's
  receipt. Uploads the phone never claimed are named, not removed.
- **Compensated**: the anchor moved and the insertion's row has no file
  location (section 5.6).
- **Abandoned**, a cover only: the anchor moved by exactly one, the track is
  still at its path and holds the new cache identifier with no image for it,
  the track set is unchanged, the staging folder is empty, and the session
  transcript in the run (`--run-dir`) shows FinishedSyncingMetadata sent and
  no FileComplete or FileError, so no image is coming. Nothing is bound, the
  cache identifier stays reserved, and the next sync sends the cover again as
  an ordinary update. A `push` of its source does too, add-only as it is: a
  track this tool inserted that names a cache identifier with no image holds
  a cover the tool still owes it, like one inserted without its cover.
- **Otherwise** the attempt's own route verifies it.

A pair staged and never finished (CONFIRMED on the phone, the removal too).
A host killed after staging `Sync_00000000.plist` and its `.cig`,
but before `FinishedSyncingMetadata`, leaves both files there, and the phone
never takes them. Every send needs that folder empty, so nothing more could be
written. Each outgoing frame is written to the session's trace before it is
sent, so a trace that holds the session's RequestingSync and no
FinishedSyncingMetadata proves the phone was never told to apply the pair.
With the anchor unmoved and nothing else changed, `resume` then reads each
file back and removes it only when it holds exactly the record's operation or
signature; the AFC connection stays locked for every other write. The folder
must then be empty, and the NotApplied proof names the files removed
(`staged_removed`). A batch's trace is in its evidence; a single attempt's is
in its run, so `resume` needs `--run-dir` for it, which the hint `sync` prints
includes.

A run killed between the phone's SyncFinished and its own receipt leaves no
pending-verification record. The session transcript, which must end in the
phone's own SyncFinished, then gives the terminal digest, and recovery's fresh
audio and cover readbacks cover the host-side hashes the receipt would have
held. The verified evidence records the receipt as absent.

### 7.3 Playback and housekeeping between runs

CONFIRMED. DESIGN of the rule.

Music is played between runs. On its own the phone also re-revisions every
item and drops stale statistics revisions some minutes after listening or
after a session. After a metadata session with no music played, all 399 items
took new revisions in no particular order and 336 of 342 statistics revisions
went. A dry run whose source inspection spanned that moment refused at
preflight, and its retry passed.

The tool therefore passes a window to every verifier.

- `SameRun`: the run's own post-write check. Exact.
- `LaterRun`: used by `resume`. It absorbs only effects of listening and
  housekeeping:
  - completed plays and skips: lifetime counts only grow, each recent counter
    is the prior value plus later events (or only later events if the write's
    own session cleared it), play and skip dates advance exactly when their
    events occur, ratings stay exact;
  - Playback History entries appended or dropped, and its revision. Older
    entries may name tracks removed since; a new entry must name a track in
    the library;
  - the history's count of entries per media type
    (`container_item_media_type`), moving by exactly the change in its entries
    whose track is in the library;
  - full-record revisions of live items renumbered above the old maximum, and
    statistics revisions added or dropped once superseded;
  - the maintenance task date advancing.
- Every other table and cell is still compared exactly, so a content change is
  refused. The receipt records the absorbed effects. The absorption grants no
  permission to resend or to change the journal.

Phone runs: a resume of an MPEG insert absorbed 5 plays, 1 skip, 4 history
entries, 444 renumbered revisions and 5 statistics revisions in 35 s.
CONFIRMED.

Removal verifiers accept positive recent counters in a fresh Before, and record
each reset. Stale-snapshot refusals say a fresh snapshot is needed because
playing music changes the library.

Cover recovery absorbs listening the same way when the run left no immediate
After to compare with: the later-run catalogue rule applies.
So does `resume` of an insert batch: its catalogue check
takes the same rule, keeps every other statistic of a listened track exact and
lets its play or skip date move only forward, with new plays or skips. The
batch's own run stays exact. CONFIRMED: a batch left Started
absorbed 2 plays and 1 skip on three tracks, besides 13 recent counters its
own session cleared.

Removal and metadata batches absorb it too, whenever a later run observes one:
`resume`, or a rerun of `delete`, `prune` or `clear` settling an earlier run's
batch. Their full-table models still pin what the session wrote (tombstones,
moved album and playlist revisions, keep-local evaluations, the metadata edit's
revision block), and the `LaterRun` rule takes the rest. A removal's survivors
may carry any new revision above its tombstones, since the session's own
refresh and the phone's renumbering since cannot be told apart, and hero
progress may stop anywhere the state reaches above them. A batch the phone
never applied may find its targets played: the playback columns are set aside,
lifetime counts may only grow, and a target's live revision may be renumbered
upwards. CONFIRMED: a metadata batch and then a removal batch, each left
Started on a test tone, absorbed 2 plays and 1 skip, and 1 play and 1 skip.

Two facts of the Playback History shape the rule. Its entries outlive their
tracks (341 of 457 named tracks had been removed earlier). And the phone counts
the entries whose track is in the library, per media type, in
`container_item_media_type`: 118 of 459 entries, exactly, in one capture.
Plays move the count with the entries. A deletion leaves it unchanged
([chapter 5](05-receiver-operations.md), "What the phone does to one track"),
yet it matched exactly after 295 played tracks had been removed. When
the phone corrects it is OPEN; a later run that finds it corrected refuses.

The phone also finishes its own work of an earlier session in the next one. An
insert batch killed mid-upload left its new album unevaluated
(`keep_local_status` 0). The next session, a metadata edit of another album,
set it to 2, moved the album's revision above the session's own block and
deleted its statistics revision row, as an emptying removal does. The metadata
model accepts that for an album the edit leaves alone and records it.
CONFIRMED. An insert or cover is verified against the catalogue,
which holds neither the status nor revisions, so it accepts such a session as
it is. A removal that keeps the album's members holds it to
the emptied-album rule and records it (`evaluated_albums`). DESIGN for the
removal: the removal batch right after the killed insert batch left the album
unevaluated, so no removal has been seen evaluating one.

### 7.4 Recovery from retained evidence

When a run finishes its phone work but the journal is not updated (a crash
between the phone write and the commit), the retained run directory has the
signed session, the Before and the transfer record. Recovery rebuilds the
original operation from retained evidence and the source, checks the fresh
state, and completes only the local journal.

`resume --run-dir` reconciles a Started attempt from its retained Started
operation without replay. The fresh state must be exactly one revision past
the prepared one, the permanent audio is read back in full and the operation
is rebuilt from the source. A single insertion that creates its album and
artist is then proven against a model of its effect on all 45 tables; any
other (one joining an album already on the phone, or carrying a composer,
compilation or rating) by the same catalogue checks the run itself applies.
Those compare the catalogue alone and absorb no listening, so `resume` refuses
such an insertion after a play or skip on the phone. The completed binding
records the source path the run's retained plan named, as the run would have.

### 7.5 What cannot be recovered

- **Removed audio.** There is no backup. If the source file is gone, so is
  the track.
- **Statistics and playlist membership of a replaced track.** A replacement is
  a new track with a new identifier. Play counts, skip counts, ratings and
  playlist memberships are lost, and the proposal says so. A `delete` followed
  by a re-add also gives a new identifier.
- **Emptied groups.** Albums, artists, composers and genres emptied by a
  removal can stay on the phone. A later re-add reuses them by name.
- **Library-container and Playback History rows, and cached artwork.** The
  phone keeps them after an empty library. Nothing reclaims that storage.
- **Anything the journal lost.** Unfinished work is not discoverable from the
  phone. A new empty journal does not resolve it.
- **A write whose effect no model explains.** The tool leaves the batch
  Started, reports it, and refuses further writes until it is understood.

### 7.6 Lost or replaced journal

If the journal is lost, nothing on the phone is damaged. `pull` rebuilds it
from a fresh capture: each phone track whose audio exactly one library file
holds is bound to that file and managed from then on, and `pull --tracks`
copies the rest into the library directory, checks each copy against the
phone's file byte for byte, binds it, and never overwrites a file. Covers,
play counts and the rest of the phone's rows are kept as they are. A pull on a
journal with unfinished work refuses until `resume`, or the command that
started the work, settles it. CONFIRMED: a fresh host's `pull`
bound all 399 tracks of the reference library, and a dry run then planned
nothing; three pushed test tones were copied back by `pull --tracks`
byte-identical, and a second pull copied nothing.

A copy is the phone's file as it is. A track this tool sent is its source file
byte for byte, with the cover embedded as the source had it. The cover the
Music app shows comes from the phone's separate artwork store, which is not
written into copies, so a track another program put on the phone without an
embedded cover is copied without one. A pulled track that the tool did not
send has no cover record; its cover is kept on the phone but not verified.

## 8. Wi-Fi and USB in the engine

### 8.1 The same engine over both transports

DESIGN, CONFIRMED. The ATC and AFC protocols are unchanged over TCP once
Lockdown and the pairing record provide TLS (chapter 03). The engine selects
the transport at one point (`--wifi`, `--wifi-address IP`), and everything
above it, including the journal, planning, verification and recovery, is the
same. Wi-Fi uses the same pairing record that the USB multiplexer saved when
the phone was paired over USB. The tool does not require netmuxd, avahi or any
system daemon. DESIGN: the project adds crates, never system packages.

### 8.2 Discovery

- In-process mDNS browse (`mdns-sd`). The instance name carries a private
  Wi-Fi address that differs from the pairing record's hardware address, so
  the tool matches the authTag instead: an eight-byte HMAC of the identifier
  under a key derived from the host identifier. It falls back to the MAC for
  older phones.
- It then confirms that a paired session reports the record's device
  identifier before any other connection is made.
- The tool remembers each phone's last address.
- One discovery per command. An engine step that runs inside another with the
  same configuration shares the phone the outer one found and holds, which
  saves about 7 s a step. Over Wi-Fi, `sync` reads its sources while the phone
  is being found and held.

### 8.3 Keeping the phone awake

CONFIRMED.

- A locked phone sleeps within seconds and closes every open service
  connection when it does. The heartbeat service sends a sleep notice about
  two seconds after connecting.
- A unicast datagram to the echo or discard port wakes it (a magic packet to
  UDP port 9 works). Broadcasts and other ports do not. The tool sends a
  magic packet before discovery and before every TCP connection.
- The assertion service and the ATC service close the connection on the first
  message unless the host also holds a heartbeat connection. With a heartbeat
  kept open, a wireless-sync power assertion is accepted.
- The tool holds a 60 s assertion renewed every 20 s. The assertion service
  answers one command per connection, so each renewal is a new connection, and
  the old one closes after its successor holds.
- **One heartbeat per host.** A second process from the same host replaces the
  first one's heartbeat. The tool and any probe must not run at once.
- **A decided sleep cannot be stopped.** About 1.4 s after a wake with no
  assertion, the phone decides to sleep and nothing created afterward stops
  it. After holding the phone the tool watches the heartbeat for 6 s. If a
  sleep notice arrives it lets the phone sleep, wakes it and holds it again,
  up to three times. Six back-to-back snapshots passed this way. The watch
  runs before anything else, not just before the first write: a command
  started right after another can find that the phone has already decided to
  sleep.

### 8.4 Transfer sizes

CONFIRMED.

| | 64 KiB requests with read-back | 1 MiB requests, no read-back |
| --- | --- | --- |
| Upload per track | 7.0 to 7.7 s | 1.74 s |
| One batch of 100 | 768 to 850 s | 276 s |
| Evidence files per batch | about 130,000 | about 8,500 |

Each request is lock-step. Over Wi-Fi, one round trip costs about 10 ms under
load. A steady 1 MiB write takes about 40 ms, about 26 MB/s, which is the link.
A second connection would not help. Reading every upload back would send each
track across the link twice (the left column of the table). Wi-Fi
retransmissions on a healthy connection occasionally stall one request for
several seconds, so each request may wait 30 s for its reply, and `sync` plans
again by itself (up to three times) after a batch the phone undid.

### 8.5 Isolated cache runs

DESIGN. Experiments that must not disturb the real fingerprint cache or journal
set `XDG_STATE_HOME` to a scratch copy of the state directory. The tool uses
that location for everything. Cache ageing, corruption and rebuild checks on
the phone all ran this way. CONFIRMED.

### 8.6 Measured timings

CONFIRMED.

**Effect of the caches and capture reuse.** The table compares runs without
and with the measures of sections 4.2, 5.3, 6.2 and 8.2: the caches, capture
reuse, pipelined stats and one discovery per command. Each check they narrow
follows how Apple Devices handles the same case: capture reuse, the cover
proof cache, the source cache across runs, the run-history recheck once per
plan, and no closing stat of an index pass that copied nothing. Measured over
Wi-Fi with 403 tracks, 8.2 GB and 402 covers:

| Operation | Without them | With them |
| --- | ---: | ---: |
| `plan` of the library from a saved snapshot | 92.8 s | 1.5 s |
| `sync --dry-run` of the library | 142 s | 17.6 s |
| `verify` (403 tracks, 402 covers) | 66.5 s | 17.3 s (53.1 s with no cover proofs kept) |
| Insert batch of 12 test tones (10 ALAC with covers, 2 MP3) | 62.8 s | 28.2 s |
| Delete of those 12 tones | 44.1 s | 21.8 s |
| Delete of the 399 library tracks (4 batches) | | 58.9 s |
| Restore of the library with `--batch-size 100` (4 batches, 398 covers) | | 539.7 s |

After the restore `verify` passed in 16.1 s with every cover proof reused,
and a dry run of the library matched 399 in 18.4 s. A dry run started while
the phone was still waking took 72 s to find it; the next took 8.4 s. Left
alone: the two catalogue guards of each index pass and capture (about 0.45 s
per 7 MB read over Wi-Fi), the 6 s sleep watch, pipelined writes and
concurrent cover groups.

A dry run that found no usable cache entry (section 6.2) read the
399 files once in 9 min 57 s; the next took 18.5 s with 798 fingerprints reused
and none stored.

## 9. The formats pipeline

### 9.1 What the tool checks before it sends anything

DESIGN. `validate FILE` names the profile a source fits and shows what an
insert sends: the projected tags, the comment and the sort names among them,
and the cover with its checksum. A source no profile takes is explained, not
widened: `validate` lists each reason, and `plan` repeats them in the source's
conflict. `inspect FILE` shows codec, duration and timing, every tag in the
container's store and each picture, as text or JSON, without rewriting.
`validate --decode` fully decodes with FFmpeg. A local decode pass proves
neither a transfer nor gapless playback.

Every profile:

- reads container, tags, timing, and artwork directly (not through a tag tool;
  stale free-form tags once conflicted with the real title);
- caps transferred files at 64 MiB;
- takes one cover (`mpeg::cover_index`): the file's only picture whatever its
  type, or among several the one of type 3 (front cover); each MP4 `covr`
  image counts as type 3. A file with several pictures and not exactly one
  front cover goes without a cover, and `validate` and `plan` name the reason
  (`cover_left_out`). A JPEG of any kind is sent as it is; a PNG, GIF, BMP or
  TIFF is sent as a JPEG (section 9.3);
- checks the phone's own advertised limits (its codec list from Lockdown)
  before planning;
- sends the comment and the five sort names, and reports
  tags the catalogue does not take (a described comment such as `iTunNORM`, an
  ID3v1 tag) as kept in the file and not projected, and never blocks on them.

| Profile | Accepts | Refuses |
| --- | --- | --- |
| ALAC | Unprotected ALAC in M4A, mono or stereo, 16, 20, 24 or 32 bits, any rate the phone plays | Protected files, tracks not covering the media data exactly, compilations without a true compilation tag and an explicit non-empty album artist |
| MP3, insert batch | MPEG-1, 2 and 2.5 Layer III, up to 48 kHz and 320 kb/s, mono or stereo, constant or variable rate, optional Info, Xing or VBRI frame, projected tags, one front cover, new groups or groups on the phone. Sent by insert batch only, with Apple's timing and gapless fields | Layers I and II (Apple's sync refuses them too) |
| AAC, HE-AAC, HE-AAC v2, xHE-AAC in M4A | Within the phone's limits (AAC to 96 kHz, HE-AAC to 192 kHz, xHE to 96 kHz mono or stereo). Sample range from the edit list, encoder delay and padding as gapless fields | Protected `.m4p`, other containers |
| FLAC | Mono or stereo, every sample. With `--flac-to-alac`, sent as ALAC instead (section 9.5) | Over the phone's limits |
| WAV, AIFF, AIFF-C (`NONE`, `twos`, `sowt`) | Integer PCM up to 24 bits, mono or stereo | Floating point, over the limits |

Not targeted: Opus, Ogg, WMA, CAF, ADTS, AC-3 and E-AC-3, Audible, protected
files. Apple's own sync has no path for them.

### 9.2 How each format is sent

CONFIRMED on the phone over Wi-Fi: each format below was sent,
verified, played normally on the phone, and removed.

- Each insert names its format: an `audio_format` number, the kind, the
  destination extension and, outside MPEG, the gapless fields.
- ALAC's bit rate in the insert is the PCM rate (rate times bits times
  channels).
- A full release date gives its year.
- An MP3 sends what Apple's sync computes for it (the rule is
  chapter 06, section 13): the frames Apple counts (a Xing or VBRI frame
  count, else the byte estimate, else every frame) for its display time, the
  average bit rate under a Xing or VBRI header or else the first frame's, the
  samples played as `duration`, and the encoder delay, drain, heuristic word
  and last-frame resynch. These come from an `iTunSMPB` comment, an accepted
  LAME tag or an `SMPB` frame (heuristic 1), else from the encoder the tag
  names and a search of the decoded last frames for where the sound stops
  (heuristic `0x02000003`). The decoder is a pure-Rust MP3 decoder. The values
  for Track R, the one MP3 with Apple traffic, reproduce Apple's own insert
  exactly.
- The comment and the sort names of the title, artist, album, album artist and
  composer go with each insert. A missing sort name is the name without a
  leading "The", "An" or "A". A group the track joins keeps its stored sort
  name.
- The phone maps AAC, AIFF, WAV, MPEG and ALAC to known kinds. It does not
  know "FLAC audio file", so a FLAC gets kind 0. The row is otherwise like a
  WAV row, is marked supported, and plays. CONFIRMED.
- Tag Updates of an owned FLAC and AAC were sent after a stream-copy retag.
  Changed-audio replacement of every format ran too: one
  coordination replaced eleven test tracks (ALAC, four kinds of MPEG-1 MP3,
  MPEG-2, AAC, WAV and AIFF) after each one's audio changed, in two minutes.

On the phone over Wi-Fi, one insert batch of eleven tracks
verified, and the phone stored every value sent: for a LAME-encoded MP3 delay
576, drain 2592, heuristic 1 and 352,800 samples (exactly 8 s); for the
FFmpeg-made MP3s and the MPEG-2 file heuristic `0x02000003` with delay 528; the
sort names, a two-line comment and article-stripped titles, artists and
albums. Track R was then sent again and the phone holds what Apple's own sync
sent for it (duration 11,162,592, delay 528, drain 912, resynch 4,047,097,
heuristic `0x02000003`). Played into the following track, it played through
normally. The new projection then planned four Updates of the reference
library (a comment on two tracks, a sort title on two others), which one
verified metadata batch applied.

### 9.3 Cover handling

DESIGN, CONFIRMED.

- A cover rides with its audio in the same session. The insert operation names
  the cover by its cache identifier and checksum, exactly as the reference
  host does, so the phone requests the track with the track and artwork parts
  together. The checksum is the length plus 17 for an ALAC cover, the length
  plus 1 for an MP3 and the other formats, and an MP4 cover's checksum also
  counts its data atom header.
- Cache identifiers are allocated per batch above every saved, registered and
  reserved identifier and reserved in the journal. Both guards check that the
  saved artwork database is unchanged and no cover stage exists.
- Each cover is written and read back before its audio. Settlement requires
  each covered track to register exactly its cache identifier and an uncovered
  one none. An album keeps its registration or takes one of its new members'
  covers. No other registration moves. Each bound cover's permanent original is
  then read back and its rendition decoded.
- An ALAC track whose album already has live tracks gets its cover in a
  follow-up step. Under `push` that step first proves the album's existing
  image is identical, so a member with a different cover is refused. A plain
  sync sends it. If a run stops before that step the next run plans the track
  as Kept with its cover pending.
- The follow-up step sends only ALAC covers, so any other format's cover rides
  in its insert batch, also when its album is already on the phone; the album
  keeps its image or takes the new one. Under `push` the album's image must
  stay as it was: such a cover rides only when the album's permanent original
  on the phone holds exactly its bytes, read back once per album and cover
  before the batch. Otherwise the track goes without its cover and the run
  names it. CONFIRMED: an MP3 and an AAC carrying an existing album's own
  cover bytes sent them in the batch and verified, and a FLAC with another
  cover went without one.
- Mixed albums (one covered member, one not) verified in one session.
  CONFIRMED.

**Cover formats.** DESIGN, CONFIRMED on the phone. Apple's sync sends JPEG and
PNG covers unchanged and converts other image types to PNG (chapter 06,
section 14). On the phone, a JPEG of any kind (baseline, progressive,
greyscale) is kept byte for byte, but a PNG original is replaced by the
phone's own baseline JPEG of the same raster, transparency flattened onto
white. A PNG sent as it is could therefore never be verified as the original.
The tool sends a PNG cover as a baseline JPEG of the same raster it makes
itself: transparency flattened onto white, quality 95, no chroma subsampling.
The same PNG always gives the same JPEG, `validate` reports the cover as
converted from PNG, and the checksum rule applies to the JPEG that is sent. A
batch of ALAC, AAC, FLAC and MP3 tones with RGB, RGBA,
palette-with-transparency and 16-bit PNG covers verified four of four, each
original exactly the JPEG sent. The tool's JPEG and the phone's own differ
only in encoder rounding (38 dB apart, the same picture).

A GIF (its first frame), BMP or TIFF cover is sent the same way: decoded on
the host and sent as the baseline JPEG a PNG would give, as Apple's sync
converts such a cover to PNG and the phone keeps no PNG. A file's only picture
is its cover whatever its type (FFmpeg marks an attached picture "other" by
default), and a file with several pictures and not exactly one front cover
goes without a cover; the file is sent unchanged either way. Under
`--flac-to-alac` FFmpeg copies a FLAC's only JPEG or PNG picture, and any
other cover is written into the ALAC as the JPEG that is sent. A picture that
declares no type (an empty MIME type, or a bare `image/`, as ID3v2.3 and some
FLAC taggers write it) is read from its bytes (a declared type that
disagrees with them is still refused); FFmpeg skips such a FLAC picture, so
it is written into the ALAC as that JPEG too. CONFIRMED on
the phone: a GIF cover of an MP3 and a BMP cover of an ALAC were sent as JPEG
and verified, as was an MP3's only JPEG marked "other", and an MP3 with two
unmarked pictures went without a cover. DESIGN, tests only: a TIFF cover, and
a FLAC picture other than JPEG or PNG under `--flac-to-alac`.

#### The ffmpeg batch-decode decision

A cover check needs the phone's rendition decoded to compare. One FFmpeg
process per cover would cost about 0.2 s each, most of it process start, so
covers are decoded one FFmpeg per group of up to 64, with no auto-scaling and
frame-hash output. DESIGN, CONFIRMED: all 394 hashes equalled the per-cover
result. FFmpeg is run directly, never through a shell. The cost is
the same over USB and Wi-Fi, so it stays in the verification time (about 41 s
of a 65 s Wi-Fi `verify`).

### 9.4 Codec edge cases the design has absorbed

- AAC identity covers the AudioSpecificConfig, not the whole sample entry
  (section 4.3).
- Some encoders leave unused bytes in `mdat`. The AAC reader takes the span the
  sample tables reference. When neither the ALAC nor the AAC reader takes an
  M4A, both reasons are reported.
- Grayscale JPEG covers leave a 0-byte rendition. The verifier accepts that
  when the original is exact.
- A cover smaller than every rendition edge (under 144 px on its longer side)
  gets no rendition at all: the phone makes only the five qualified sizes
  (556, 531, 480, 390 and 144) and never enlarges. The verifier accepts such a
  cover on its exact original alone, and refuses a larger cover that has no
  rendition. Its proof is not cached, so `verify` reads it again each time.
  CONFIRMED on a 64×48 cover made from a GIF: its 3,296-byte original was on
  the phone and every `Caches` size was absent; `resume` then verified the
  batch that sent it.
- A VBRI header is read: its frame count sets the display time, as in
  Apple's sync.
- FFmpeg 8.1's young xHE-AAC decoder cannot decode one encoder's plain xHE-AAC
  output. `validate --decode` fails on such a file although the phone plays it.
  CONFIRMED.

### 9.5 FLAC to ALAC

DESIGN, CONFIRMED on the phone. Phone behaviour that motivates it: a FLAC is
stored under kind 0, while ALAC has a known kind, and its covers, metadata
updates, replacement and removal have more phone verification behind them. The
sound is lossless either way, since each upload is compared with its source and
FLAC decodes to the exact samples.

Checked by hand with FFmpeg 8.1:

- The same covered FLAC converted twice gave byte-identical ALAC.
- The result passed `validate` under the ALAC profile with its cover.
- A 24-bit 96 kHz FLAC converted to 24-bit 96 kHz ALAC and decoded to the same
  samples.
- FFmpeg's tag mapping dropped the track total and disc total, and its M4A
  writer writes no sort atoms. Tags must come from the tool's own FLAC reading.
- FFmpeg's ALAC encoder stops at 24 bits. A 32-bit FLAC lost bits.

The design:

- **Flag and check.** `--flac-to-alac` on `sync`, `push` and `plan` (dry runs
  and change plans included). Right after parsing its arguments, before
  discovery or any other work, the tool requires FFmpeg on the path with the
  ALAC encoder and the FLAC and ALAC decoders, and stops with an install hint
  without them. A plan records the flag, so `apply` and saved-plan rebuilding
  replay it, and each converted addition is shown as FLAC to ALAC.
- **Conversion.** FFmpeg runs directly, never through a shell, with its
  bit-exact flags, and copies only the audio (as ALAC) and the one front cover.
  Every tag (title, artist, album, album artist, genre, composer, copyright,
  date, comment, the five sort names, track and disc with totals, compilation)
  is then written from the FLAC's own projection. The same FFmpeg makes the
  same bytes from the same FLAC.
- **Checks before sending, each refusing that track only.** The FLAC is at most
  24 bits and mono or stereo; both files decode to identical PCM (SHA-256 and
  length of FFmpeg's 32-bit integer output); the ALAC passes the ALAC profile,
  its projected tags equal the FLAC's field by field, and its cover is the
  FLAC's front picture (a PNG stays PNG in the file and is sent as its JPEG);
  it is at most 64 MiB; the FLAC did not change while it was converted.
- **Skips.** A FLAC deeper than 24 bits or with more than two channels is known
  when planning. The plan skips it with its reason and the rest goes on, as for
  any skipped source (section 4.1): the plan stays ready, a change plan lists
  it as skipped, and the run exits 0. A skipped FLAC is never a replacement, so
  a track converted from an earlier version of it stays. `--replace-library`
  refuses it, as any source it cannot add. A conversion that fails while
  syncing drops that FLAC only; the rest of its batch goes on, the run's list
  of skipped files names it with stage `send`, and the run exits 3.
- **Temporary files.** One private directory per run under `TMPDIR`. A
  settled batch's conversions are removed, and the directory at the end of the
  run. A run killed mid-batch leaves them for `resume`, which makes a missing
  one again from its FLAC (same FFmpeg, same FLAC, and the bytes the batch
  sent) and removes them once the batch settles, or cancels a batch the run
  never sent, together with what the killed run had converted ahead. A new
  converting run removes what earlier runs left, except files an unsettled
  batch names and directories of a process still running.
- **Converting ahead** (after Apple's host, whose conversions work ahead of
  its upload). Up to three FFmpeg runs at once, never more than the processors
  but one, take the FLACs in member order: a batch's as it begins, and the
  next batch's while it is sent. As batches of 499 FLACs would need about
  20 GB, often of RAM, a batch holds at most 100 FLACs and as many convert
  ahead of the next (about 4 GB for FLACs of 20 MB; each session's own checks,
  about 12.5 s, then add little per track). A batch also ends before the first
  FLAC whose conversion would not fit, with those before it, in twice their
  size beside the conversions under way. Only a FLAC that cannot fit alone
  stops the run, with nothing sent in its batch. A FLAC of the next batch
  starts only while there is room for twice its size beside those, and waits
  otherwise. A batch waits for all its conversions and then takes them in
  member order, so its refusals come as it begins, in member order, and the
  others keep their order. `TMPDIR` thus holds up to two batches' conversions
  at once. An error or cancel kills FFmpeg and removes the conversions no
  batch took, and FFmpeg dies with a killed run. Conversion writes nothing to
  the phone, so the one outstanding native write (section 3.4) is unchanged.
  CONFIRMED over Wi-Fi with 16 test FLACs of 133 to 177 s each (303 MB): in
  one batch a push took 68.8 s, its conversions and the capture before the
  session 16.5 s, against 73.8 s and 25.1 s without converting ahead; at
  `--batch-size 1` it took 254 s against 260 s: the next FLAC converts during
  the current session, so the wait before each upload is 2.0 s instead of
  3.1 s, but each one-track session still costs about 12.5 s. FFmpeg alone
  converts them in 26.5 s one at a time and 11.1 s three at a time on this
  host. A run killed by SIGKILL while it uploaded a batch left no FFmpeg
  running; `resume` cancelled the batch it had not sent and removed its
  directory.
- **The link.** The journal records, with each converted insert target, the
  FLAC's absolute path, size, file SHA-256, audio identity and the FFmpeg
  version line (section 3.2). It is written only with a Prepared target and
  never changed. A FLAC's audio is linked to at most one active track and is
  never converted again while that track is on the phone.
- **Planning through the link.** A linked FLAC matches its ALAC track with or
  without the flag: Keep, or an Update of the fields its new tags change, sent
  in metadata batches; a different front cover is a conflict, as for an MP3.
  A FLAC already on the phone as FLAC keeps that track. A changed FLAC at the
  path a converted track came from is a replacement. `prune` counts a converted
  track's FLAC as its audio present, and the mirror's removal names the FLAC as
  the track's source.

On the phone over Wi-Fi, with test FLACs: one insert batch sent
three FLACs as ALAC (16-bit 44.1 kHz with a JPEG cover, 24-bit 96 kHz with a
half-transparent PNG cover, 16-bit 48 kHz mono without one), both covers
verified; a dry run without the flag kept all three as the ALACs converted from
them; a new title in one FLAC was sent as a metadata Update of its ALAC; new
audio in another ran as a replacement; a killed batch was resumed (section
5.5); a deleted FLAC's track was removed, named by the FLAC's path; a 32-bit
FLAC was skipped while the 16-bit one beside it was sent; change plans with
the flag applied as planned; and `prune` kept both converted tracks while their
FLACs were present and removed one once its FLAC was deleted. Every test track
was then deleted, and `verify` confirmed the 399 library tracks and 398 covers.
The reference library's own 398 ALAC files are FFmpeg conversions of FLACs
that are played on the phone daily, so the converter's output plays. Not
tested: a 20-bit FLAC, which FFmpeg pads to 24 bits and which the PCM check
sends only if that is exact.

## 10. Output for other programs

DESIGN. `--message-format json` makes every command print JSON records instead
of sentences, for programs that drive the tool, such as a GUI. The flag is
global and goes anywhere on the command line:

```bash
rustunes --message-format json sync --dry-run
rustunes push ~/Downloads/album --message-format json --batch-size 20
```

`--message-format human` is the default and prints sentences for a terminal.

### 10.1 The stream

- **stdout** carries one JSON object per line
  ([NDJSON](https://github.com/ndjson/ndjson-spec)), flushed as each is
  written, and nothing else.
- **stderr** carries nothing of the tool's own unless `-v` asks for the run's
  log there (the README's Logs section). A crash (a Rust panic) may still
  print there.
- **stdin** is read only to answer a `confirm` record (section 10.6).
- The **exit code** is the one in the final `result` record:

| Code | Meaning |
|------|---------|
| 0 | The command did all its work. |
| 1 | It failed; the `result` record carries the error. |
| 2 | clap refused the command line (unknown command, bad value). |
| 3 | It ran but did not finish its work: blocked by conflicts, cancelled at a prompt, verification findings, a FLAC whose conversion failed while it was being sent, removals held because a source could not be read. |

A source `sync` or `push` leaves out when it plans (a Skip, section 4.1: a tag
the phone can't hold, a format it doesn't play, an MP3 profile it doesn't send)
does not change the code: the rest was sent, so the run ends `completed` with
0, and the `sync` report lists each file left out.

Every record starts with the same three fields:

| Field | |
|-------|-|
| `type` | `start`, `phase`, `progress`, `notice`, `confirm`, `report` or `result`. |
| `seq` | 0 for `start`, then one more for each record, with no gaps. |
| `elapsed_ms` | Milliseconds since the command started. |

The first record is always `start` and the last is always `result`, so a
reader can tell a finished command from one that was killed. The other fields
of a record follow in alphabetical order. Readers should ignore fields they do
not know; new ones may be added. A change that breaks readers renames the
schema (`rustunes.messages`).

### 10.2 `start`

```json
{"type":"start","seq":0,"elapsed_ms":0,"command":"push","log":"/home/me/.local/state/rustunes/logs/1718023843630412000-4242.log","options":{"batch_size":"20","dry_run":true,"message_format":"json","path":"/home/me/album"},"pid":4242,"schema":"rustunes.messages","started_unix_ms":1718023843630,"version":"0.1.0"}
```

`command` is the subcommand path (`"sync"`, `"library list"`, `"cache
prune"`). `options` holds every argument given on the command line, keyed by
its name with `-` as `_`: flags are `true`, repeatable options (`--include`,
`--track`, track IDs) are lists of strings, other values strings. Defaults are
not listed. `log` is the path of the run's log, which a bug report can attach;
it is left out when the state directory cannot hold one. A run that fails
keeps the log under the same name in `logs/failed` as well, where later runs
do not rotate it away.

### 10.3 `phase`

```json
{"type":"phase","seq":3,"elapsed_ms":2210,"message":"Reading device catalogue and audio identities for an exact comparison","phase":"capture_device"}
```

The command entered a new step. A phase lasts until the next `phase` record or
the `result`. Each `progress` record names its phase too, and a `phase` record
always comes before the first progress of a phase. Phases repeat: a batched
sync captures the phone's state before each batch, so it moves between
`capture_device`/`index_audio` and `insert` once per batch.

| Phase | What happens |
|-------|--------------|
| `find_device` | Finding and waking the phone over Wi-Fi (`--wifi`, `--wifi-address`; up to 90 s). |
| `capture_device` | Copying the phone's Music catalogue for a complete, read-only snapshot. |
| `index_audio` | Identifying each track's audio on the phone: cached fingerprints are reused; changed or new files are copied. |
| `read_snapshot` | Reading a saved snapshot instead of the phone. |
| `inspect_sources` | Reading and fingerprinting the local source files. |
| `revalidate_sources` | Checking the planned sources again before they are sent. |
| `compare` | Comparing sources with the phone and the journal: the plan. |
| `add` | Adding tracks one native session each (`--batch-size 1`). |
| `insert` | Adding tracks in insert batches: uploads and their verification. |
| `covers` | Sending and verifying covers that did not ride with their tracks. |
| `metadata` | Sending tag updates in metadata sessions. |
| `remove` | Removing tracks in removal batches (`delete`, `prune`, `clear`, `sync`'s removals, replacements). |
| `verify_audio`, `verify_artwork` | `verify` reading back audio and covers. |
| `pull_tracks` | `pull --tracks` copying phone tracks into the library. |
| `reconcile` | `resume --run-dir` reading the phone's state to settle an unfinished single attempt. Settling an unfinished batch goes through `capture_device` and `index_audio` instead. |
| `decode` | `validate --decode` decoding audio with FFmpeg. |
| `receive` | `probe` and `syslog` receiving from the phone. |

### 10.4 `progress`

```json
{"type":"progress","seq":118,"elapsed_ms":61833,"bytes_done":187695104,"bytes_per_second":3466823.5,"bytes_total":412139520,"current":{"batch":1,"batches":1,"bytes_done":10485760,"bytes_total":38207117,"path":"Artist/Album/03 Song.m4a","track_id":"5c3e0f7a9b1d2e44"},"done":4,"eta_ms":64742,"message":"Uploading \"Artist/Album/03 Song.m4a\": 10485760/38207117 bytes","percent":45.5,"phase":"insert","rate_per_second":0.074,"total":10,"unit":"tracks"}
```

| Field | |
|-------|-|
| `phase` | The step (section 10.3). It also names the progress stream. |
| `unit` | What `done` and `total` count: `tracks`, `files`, `covers`, `batches`, `sessions`, `frames`, `seconds`. |
| `done`, `total` | Items finished, of how many. `total` is `null` when not known in advance. |
| `bytes_done`, `bytes_total` | Present when the step moves bytes: uploads, copies from the phone, source reads. |
| `percent` | 0–100, by bytes when the stream counts them, else by items, counting the part of the current item its `current.bytes_done` shows; `null` without a total. |
| `eta_ms` | Estimated milliseconds until this phase completes, from its average rate since the phase began (bytes when counted); `null` until there is a rate (after a second of progress), `0` when complete. Fixed costs at a phase's start (a session's setup before its first upload, the stat of every file before an index pass) count against the rate, so the first estimates run high and then fall; smooth them, or show them only after a few seconds. |
| `rate_per_second`, `bytes_per_second` | The average rates behind the estimate; `null` until known. |
| `current` | The item in hand, where there is one: a source `path` (relative to the source root), a `track_id` (16 hex digits), `title` and `artist`, and its own `bytes_done`/`bytes_total`; or the `batch` of `batches` under way. |
| `message` | The sentence the terminal shows for it, when there is one. |

Records of one phase are at least 250 ms apart: an update sooner after the
last is not written. The first record of each pass, the one that first gives
its byte total and the one that completes it are always written, and a
completed pass is written once. A pass begins again, carrying nothing over
from the last, when a phase's item or byte total changes or its counts go back
(each capture before a batch is a new `index_audio` pass; `pull` inspects the
library and then the `--into` directory as two `inspect_sources` passes). An
update for a step's start counts only the items before it as done: `Adding
3/10` is `done: 2`; the step's own end, the next step, or the next phase
completes it. A pass with no total known in advance ends with a record that
gives one: `probe` ends `receive` with the frames it received, `syslog` with
the seconds it listened.

Progress across insert batches: `insert` counts every new track of the run,
its `bytes_total` is the size of every new track's source (for a FLAC sent as
ALAC, the FLAC's size, so the estimate is approximate), and its estimate
includes the time spent capturing and verifying between batches.

### 10.5 `notice`

```json
{"type":"notice","seq":2,"elapsed_ms":38,"code":"run_directory","level":"info","message":"Run evidence: /home/me/.local/state/rustunes/runs/1718023843630-4242","path":"/home/me/.local/state/rustunes/runs/1718023843630-4242"}
```

A fact worth showing. `code` names it for programs; `level` is `info` or
`warning`; `message` is the sentence the terminal shows; the other fields are
its details.

| Code | Level | Details |
|------|-------|---------|
| `device_detected` | info | `connection` (`usb`, `wifi`), `address` |
| `pairing_record_available` | info | |
| `run_directory` | info | `path`: where this run keeps its evidence |
| `plan_saved` | info | `path` |
| `selection_saved` | info | `path`: a removal's saved selection |
| `catalogue_capture_retained`, `state_capture_retained` | info | `path`; a catalogue captured inside a state capture is not announced on its own, as the `state_capture_retained` names where the whole capture is kept |
| `audio_index_retained` | info | `tracks`, `bytes` |
| `artwork_proofs_reused` | info | `reused`, `total` |
| `journal_carried` | info | `earlier`, `journal`, `carried`, `matched`, `unbound`, `gone`, `unfinished` |
| `updates_scheduled` | info | `tracks`, `sessions` |
| `sync_continuing` | info | `remaining`: actions a fresh sync runs one per session |
| `converting_flacs` | info | `directory` |
| `replacement_phase` | info | `coordination`, `phase` |
| `artwork_started` | info | `track_id` |
| `insertion_verified` | info | |
| `refreshing_before_session` | info | |
| `earlier_batch_settled` | info | `batch`, `state` |
| `dry_run_complete`, `cancelled` | info | |
| `atc_connected` | info | `ping_sent` (`probe`) |
| `pong` | info | `session` (`probe --ping`) |
| `cache_counts` | info | `cache` (`fingerprints`, `cover_proofs`, `source_reads`), `reused`, `stored` |
| `runs_pruned` | info | `removed`, `trimmed`, `bytes`: after the command, earlier run directories no journal needs were removed and finished ones trimmed to what planning reads |
| `sync_blocked` | warning | `holds`: what must be fixed first, each with its source `path` (`null` for the journal), `code` and plain `reason`; `hint`: how to send the rest now, or `null` |
| `removal_blocked` | warning | |
| `removals_held` | warning | `tracks`, `sources`: `sync` could not read or fingerprint these sources, so it removes nothing this run and sends the rest; exit code 3 |
| `nothing_sent` | warning | `sync` removed nothing it needed to remove, so it sent nothing |
| `sync_resuming` | warning | `reason`: an insert batch was cut short and undone; a fresh sync adds the rest |
| `cover_left_out` | warning | `path` |
| `flac_refused` | warning | `path`, `reason`: its conversion failed while it was being sent; the `sync` report lists it with stage `send` |
| `updates_refused` | warning | `reason` |
| `recapturing` | warning | the catalogue changed or a connection dropped; the capture runs once more |
| `recent_counter_resets` | warning | `plays`, `skips`: tracks whose recent counters a cover import may reset |
| `empty_rendition` | warning | `track_id`, `edge` |
| `cache_unavailable` | warning | `cache`, `error`: the run goes on without it |
| `untested_catalogue_version` | warning | `user_version`, `tested`: the phone's music database has a schema version rustunes was not tested with, and every table and column it reads; once per command |
| `runs_kept` | warning | `error`: why earlier run directories were not pruned; the next command tries again |
| `no_library` | warning | `pull` without a library directory |

### 10.6 `confirm`

```json
{"type":"confirm","seq":41,"elapsed_ms":9120,"code":"remove_tracks","default":"no","playlists":0,"prompt":"Remove these 3 tracks from this iPhone?","respond":"one line on stdin: yes or no","selection":"/home/me/.local/state/rustunes/runs/…/selection.json","tracks":3}
```

The command waits for one line on stdin. `yes` or `y` (any case) goes on;
anything else, or the end of stdin, declines, and the command ends with
outcome `cancelled` and exit code 3. The `report` before it shows what is
being confirmed (`removal_selection`, `replacement_proposal`, `change_plan` or
`metadata_edit`). Pass `--yes` to skip the question, or `--dry-run` to only
see the proposal.

| Code | Asked by | Details |
|------|----------|---------|
| `remove_tracks` | `delete`, `prune`, `clear`, `sync` | `tracks`, `playlists`, `selection` |
| `replace_tracks` | `sync --replace-library`, `--replace-changed` | `tracks`, `sources` |
| `apply_change_plan` | `apply` of a plan that removes tracks | `removals`, `additions`, `updates` |
| `send_edit` | `metadata set` | `tracks` |

### 10.7 `report`

```json
{"type":"report","seq":12,"elapsed_ms":4,"kind":"validation","report":{"…":"…"}}
```

A command's result data: `kind` names it, and `report` holds it. Where a
command has a `--json` option, the report is exactly what `--json` prints. A
command may write several reports: `sync` writes `removal`, `plan` and `sync`.

| Kind | Written by | Holds |
|------|------------|-------|
| `plan` | `plan`, `sync`, `push`, `apply` | The plan's counts (`additions`, `matched`, `updates`, `conflicts`, `skipped`), `ready`, `actions` (`add`, `update` with its `changes`, `conflict`, `skip`, `finish_cover` and `send_cover` (these two with the cover's `bytes`); each with its `path`), `blockers`, `artwork_recent_resets`, `preserved_unmatched_device_tracks`, `excluded_sources`. A `conflict` holds the run and carries `code`, a plain `reason` and the planner's `message`; a `skip` is a source left out, with `code`, a plain `reason` (what is wrong and how to fix it) and the refusal in full as `detail`. A plan whose only refusals are skips is `ready`. Tracks it keeps as they are are only counted. |
| `plan_document` | `plan` | The complete saved plan, as `plan --json` prints it. |
| `change_plan` | `plan --remove-missing`, `--add-remove`, `--replace-changed`, `--replace-library`; `apply` of such a plan | The plan, as `plan --json` prints it. |
| `change_apply` | `apply` | `outcome`, `removed`/`removals`, `additions`, `updates`, `message` |
| `apply` | `sync`, `push` and `apply` when they send one track per session or check a plan | `result` (`preflight`, `unchanged`, `completed`), `additions`, `metadata_updates`, `message` |
| `sync` | `sync`, `push`, `apply` | `outcome` (`blocked`, `dry_run`, `completed`), `run`, `plan`, `journal`, `managed_covers_verified`, `existing_unmanaged_covers_unverified`, `metadata_updates_verified`, `covers_left_out`, `skipped`, `skipped_count`, `message`. `skipped` lists each source left out: `path`, `code`, `reason`, `detail` and `stage` (`plan`: the plan left it out; `send`: its FLAC's conversion failed while it was being sent). A dry run lists what it would skip. The run directory's `complete.json` keeps the same list. |
| `removal_selection` | `delete`, `prune`, `clear`, `sync` | The selection, as `delete --json` prints it: the tracks proposed for removal, those kept and why, playlists, blockers. |
| `removal` | `delete`, `prune`, `clear`, `sync` | `removed`, `deleted_playlists`, `evidence`, `message` |
| `replacement_proposal` | `sync --replace-library`, `--replace-changed` | `kind`, `continuing`, `removals`, `sources`, `selection` |
| `replacement` | the same | `outcome`, `coordination`, `removed`, `tracks`, `message` |
| `pull` | `pull` | As `pull --json` prints it. |
| `artwork` | `artwork` | `result` (`unchanged`, `planned`, `verified`), `track_id`, `cache_id`, `bytes`, `message` |
| `metadata_edit` | `metadata set` | `tracks` (`track_id`, `fields`), `operation` |
| `metadata_set` | `metadata set` | `outcome`, `updated`, `message` |
| `metadata_plan` | `metadata plan` | The intent it prints. |
| `verification` | `verify` | As `verify --json` prints it. |
| `recovery` | `resume` | As `resume --json` prints it. |
| `journal_status` | `status` | As `status --json` prints it. |
| `snapshot` | `snapshot` | `directory`, `message` |
| `library_list`, `library_track`, `library_stats` | `library list`, `show`, `stats` | As `library … --json` prints them. |
| `catalog` | `catalog` | `track_count` and `tracks`, each with the CSV's columns; with `--output`, only `output`. |
| `devices` | `device list` | As `device list --json` prints it. |
| `device_info` | `device info` | As `device info --json` prints it. |
| `lockdown_domain` | `device info --domain` | `domain`, `value`: the property list as JSON (data as `{"base64": …}`). |
| `source` | `inspect` | As `inspect --json` prints it. |
| `validation` | `validate` | As `validate --json` prints it. |
| `scan` | `scan` | As `scan --json` prints it. |
| `cache_statistics`, `cache_prune` | `cache stats`, `cache prune` | As `--json` prints them. |
| `config` | `config library` | `file`, `library` (`null` when unset), `changed` |
| `completions` | `completions` | `shell`, `script` |
| `probe` | `probe` | `frames` (`command`, `payload_bytes`), `sync_allowed`, `ping_sent`, `pong`, `pong_session`, `trace_dir`, `message` |
| `syslog` | `syslog` | `lines`, `output`, `message` |

### 10.8 `result`

```json
{"type":"result","seq":57,"elapsed_ms":548210,"exit_code":0,"ok":true,"outcome":"completed"}
{"type":"result","seq":9,"elapsed_ms":15020,"error":{"causes":["timed out discovering the device"],"message":"…"},"exit_code":1,"ok":false,"outcome":"failed"}
```

| Field | |
|-------|-|
| `ok` | `exit_code` is 0. |
| `exit_code` | As in the table of section 10.1. |
| `outcome` | `completed`, `dry_run`, `checked` (a saved plan or evidence checked offline), `nothing_to_do`, `blocked`, `cancelled`, `findings` (`verify`, `scan`), `unsupported` (`validate`), `incomplete` (exit code 3 for another reason), `failed` (exit code 1), `usage_error` (exit code 2). |
| `error` | With `failed` and `usage_error`: `message` and its `causes`, outermost first. |

### 10.9 A session

`push` of two new tracks in one batch, abridged:

```json
{"type":"start","seq":0,"elapsed_ms":0,"command":"push","log":"…/logs/…","options":{"batch_size":"10","path":"/home/me/new","yes":true}, …}
{"type":"phase","seq":1,"elapsed_ms":0,"phase":"find_device","message":"Finding the phone over Wi-Fi"}
{"type":"notice","seq":2,"elapsed_ms":2071,"code":"device_detected","connection":"wifi","address":"192.0.2.5", …}
{"type":"notice","seq":4,"elapsed_ms":2075,"code":"run_directory","path":"…/runs/…", …}
{"type":"phase","seq":5,"elapsed_ms":2075,"phase":"capture_device", …}
{"type":"progress","seq":6,"elapsed_ms":4630,"phase":"index_audio","done":0,"total":399,"bytes_done":0,"bytes_total":16512870000, …}
{"type":"progress","seq":7,"elapsed_ms":4900,"phase":"index_audio","done":399,"total":399,"percent":100.0,"eta_ms":0, …}
{"type":"progress","seq":9,"elapsed_ms":5010,"phase":"inspect_sources","done":0,"total":2,"current":{"path":"a.m4a","bytes_total":31457280}, …}
{"type":"phase","seq":11,"elapsed_ms":5260,"phase":"compare", …}
{"type":"report","seq":13,"elapsed_ms":5300,"kind":"plan","report":{"additions":2,"actions":[{"action":"add","path":"a.m4a", …}, …], …}}
{"type":"progress","seq":20,"elapsed_ms":8100,"phase":"insert","done":0,"total":2,"current":{"batch":1,"batches":1,"tracks":2}, …}
{"type":"progress","seq":31,"elapsed_ms":14200,"phase":"insert","done":1,"total":2,"bytes_done":45000000,"bytes_total":62914560,"eta_ms":2400,"current":{"path":"b.m4a","bytes_done":13542912, …}, …}
{"type":"progress","seq":40,"elapsed_ms":30100,"phase":"insert","done":2,"total":2,"percent":100.0, …}
{"type":"report","seq":44,"elapsed_ms":30150,"kind":"sync","report":{"outcome":"completed","managed_covers_verified":2, …}}
{"type":"result","seq":46,"elapsed_ms":30160,"exit_code":0,"ok":true,"outcome":"completed"}
```

A GUI can drive a progress bar from the `progress` records of the current
phase (`percent`, `eta_ms`), a status line from `phase` and `current`, a log
from every `message`, and its summary from the last `report` and the `result`.

## 11. Limits and open design questions

Known limits (all DESIGN unless noted):

- Group collection: the phone collects emptied albums, artists, composers and
  genres by its own rules, seen in sessions of 16 to 499 targets but not in
  smaller ones or in one of 90 (chapter 05, section 3). The tool cannot force
  it, and `delete_album` and `delete_artist` do nothing. Emptied groups can
  stay.
- Sessions of more than 500 operations, which Apple's host splits over
  several operation files (chapter 08, section 5), have not been observed.
- Removal of members of smart playlists, folders and phone-made playlists is
  refused. `delete` and `prune` leave emptied ordinary playlists; only `clear`
  deletes them.
- Tags the catalogue does not take (a described comment such as `iTunNORM`,
  an ID3v1 tag) stay in the file and are not projected.
- A cleared genre or composer costs a replacement and its losses.
- Fraunhofer's CRC-protected MP3 marker is not read (such a file takes the
  tail search), and the Sound Check energy Apple sends for an MP3 is not
  derived.
- A file with several pictures and not exactly one front cover goes without a
  cover. A TIFF cover, and a FLAC picture other than JPEG or PNG under
  `--flac-to-alac`, are sent as JPEG by design but have not been on the phone
  (section 9.3). CMYK and 12-bit JPEGs would be sent unchanged, as Apple's
  sync sends them, but neither has been on the phone.
- A non-ALAC track already on the phone without its cover (one `push` left
  out) gets it only by being deleted and added again: the cover step sends
  ALAC covers only.
- A FLAC sent as FLAC gets kind 0 on the phone. It plays.
- A track sent from outside the library directory is never removed by the
  mirror.
- Index capture refuses rows without audio unless a journaled batch accounts
  for them, which means a phone that someone else left in that state needs
  that tool's cleanup first.
- An album a killed insert batch left unevaluated is accepted in any later
  session; a removal that keeps its members evaluating it is
  modelled but has not been observed.
- Only one tool process per host at a time: one heartbeat per host (section
  8.3) and one journal writer.
- Each batch pays a fixed cost for its captures and checks, so tiny syncs pay
  it too. An insert batch of twelve short tones takes about 28 s over Wi-Fi
  (section 8.6).

Open questions:

1. Whether one saved plan should hold a missing-source removal and a
   replacement together.
2. What triggers the phone's group collection, and whether a command that
   deletes chosen playlists without a Clear is wanted.
3. The NotApplied path of a playlist-deleting batch, and the in-run retry
   path, which has unit tests only.
4. A like-for-like timing of `clear` over Wi-Fi.
5. Whether listening ever changes the saved companion files that the recovery
   rules require to be equal.
6. What happens if the phone creates its Playback History container for the
   first time between runs. The history rule refuses a changed container set.
7. Whether the keep-awake assertion keeps a locked phone awake on a charger,
   and whether the ATC `PowerAssertion` message works over Wi-Fi.
8. Whether `pull --tracks` should write the cover from the phone's artwork
   store into the copy of a track another program sent without an embedded
   cover.
9. Whether the tool should clean the partial uploads that killed sessions
   leave in the phone's `Airlock` staging folder (section 5.5).

## 12. Evidence

### 12.1 Facts observed on the phone

The phone was running an iOS 27 release throughout.

- **Inserts.** Insert batches with covers, and mixed ALAC and MP3 insert batches. All phone formats sent and played, including HE-AAC and xHE-AAC; an eleven-track batch of every format. Apple's MP3 timing and gapless fields, comments and sort names stored as sent; Track R re-sent with Apple's values and played through into the next track. A mixed ALAC and MP3 run needed the maintenance-property catch-up between steps. FLAC to ALAC: conversions, Updates, replacement, a resumed batch, skips, change plans and prune; the conversion itself checked by hand (not on the phone).
- **Covers.** Non-ALAC covers in batches for albums already on the phone. A mixed covered and uncovered album. An album left unevaluated finished by the next session. Covers of every JPEG kind kept byte for byte; a PNG original replaced by the phone's own JPEG, then PNG covers sent as the tool's JPEG. GIF and BMP covers sent as the tool's JPEG, one of them too small for any rendition.
- **Metadata.** Metadata updates sent and settled, including cleared fields and the replacement of a track whose source cleared a composer.
- **Removal.** Removal batches up to 499. Owned MP3 prune. A played MP3 deleted. Playlist members removed. `clear` with playlist deletion. Removal of tracks whose source is gone, including a genre representative moving to a covered member. `delete_album` and `delete_artist` ignored.
- **Replacement and change plans.** The library replaced end to end: 400 removed in four batches, 398 added in 8-track batches. Replacement of every format, MP3 included. Interrupted replacement coordinations continued. Change plans applied, interrupted and blocked as planned. All revision-order and group-rule discoveries.
- **Interruptions.** `clear` with kills at three kill points; the phone rolled back a kill at the first Progress but had already deleted the file. Single inserts, a cover, mixed ALAC and MP3 insert batches and metadata batches killed at chosen messages and settled by `resume`. Partial batches killed mid-upload and compensated, an MP3 batch among them, and one with an album cover already pointing at an arrived file. Resume with playback absorbed (35 s). 443 items renumbered with no content change.
- **Journal and caches.** Journals of other builds carried forward from the phone; a fresh host's `pull`; `pull --tracks` copies byte-identical; the mirroring `sync` and `push`. Read-only open of a copied journal. SQLite cache checks (reuse, corruption, rebuild); caches keyed by the UDID's SHA-256 reused across a redefined device digest.
- **Transport and speed.** Wi-Fi: unlocked, then locked; keep-awake; wake datagram; heartbeat requirement; Airlock removed by a restart. Partial uploads of killed sessions found and removed from `Airlock`. Transfer speed with 1 MiB requests; one discovery per command, the source and cover proof caches, capture reuse and pipelined stats. Restore of the library over Wi-Fi in 539.7 s. The phone drops about one new AFC connection in nine after hashing sessions. The hash leak that wedges the file service until a restart (section 6.3).
- **Write lock.** Phone writes locked until a Started permit: a one-track sync with its cover, an eleven-track batch, a metadata edit and a twelve-track delete sent the same requests as without the lock.

### 12.2 Design decisions

These were decided and tested, not discovered:

- two crates and where the line falls, with the protocol library independent
  of the application (section 2);
- the send sequence, one outstanding write, no second permit for a Started
  attempt, and file-service writes locked until a Started permit (sections
  2.4, 3.4, 5.3);
- exact-content journal, reservations never freed, ownership only from the
  tool's own insertion or an explicit pull (section 3);
- one journal schema edited in place, and journals of other versions carried
  forward from the phone instead of migrated (sections 3.3, 3.5);
- identity by encoded audio, tags excluded (section 4.3);
- change plans pinned to the steps they run (section 4.4);
- `push`, a mirroring `sync` with a confirmed removal, the replacements and
  `pull` as separate commands with stated losses (section 4.5);
- a conservative refusal list (section 1.3);
- no backup, no source modification (section 1.2);
- stat-keyed fingerprint cache keyed by the phone's UDID, cover proof and
  source caches, proportional verification, no phone hashing (sections 4.2
  and 6);
- read-only commands read a copy of the journal (section 3.6);
- the window rule for playback between runs (section 7.3);
- one FFmpeg per group of covers, and PNG covers sent as the tool's JPEG
  (section 9.3);
- FLAC to ALAC behind a flag, with the PCM compared and a journal link
  (section 9.5);
- one JSON record per line for other programs, from `start` to `result`
  (section 10).

### 12.3 Planned and open

Nothing is PLANNED. OPEN: the questions in section 11.
