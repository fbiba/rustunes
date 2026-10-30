# music-protocol

Building blocks for talking to the stock Music library of an iPhone from
Linux: the ATC sync protocol, the media checks it needs, and readers and
models of the phone's music database.

The crate has no command line and keeps no state of its own. It encodes
messages and reports what it observes; deciding what to send, recording what
was sent and recovering from interruptions are left to the caller.

## What is inside

- **`protocol`**: ATC framing and the bounded sync session, the request
  signature, the insert, update and delete operations as Apple's host sends
  them, AFC file transfer with checked replies, and the ancillary asset
  manifests.
- **`media`**: container and tag reading for ALAC, AAC, MP3, FLAC, WAV and
  AIFF, stable audio fingerprints that ignore tags and padding, MP3 gapless
  timing, and cover art inspection.
- **`catalogue`**: bounded readers of `MediaLibrary.sqlitedb`, its library
  identity and revision anchor, and models of what each operation must change
  in its tables, used to verify a session against the database read back
  after it.

The findings behind these models, and their evidence, are in the `docs/`
chapters of the repository this crate comes from.

## Status

Version 0.1. The interfaces follow what the phone was observed to do and may
change between releases. Tested against the iOS versions named in the
repository's docs.

## Features

- `offline-test-support`: in-memory AFC peers, synthetic catalogues and MPEG
  frames, for the tests of this crate and of crates built on it. Off by
  default.

## License

MIT. See `LICENSE`.
