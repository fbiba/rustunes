# Local metadata parser fixtures

These tiny files contain generated silence and tones, not user music. Tests
read them; tag-editing tests operate on in-memory copies. Building needs no
FFmpeg; the tests that make covers or convert FLAC to ALAC run the `ffmpeg` on
`PATH`.

Generated with FFmpeg's `anullsrc` at 44,100 Hz, stereo, for 0.05 seconds:

```bash
ffmpeg -v error -n -f lavfi -i 'anullsrc=r=44100:cl=stereo' -t 0.05 -c:a alac -sample_fmt s16p -fflags +bitexact -flags:a +bitexact -map_metadata -1 -metadata title='Standard title' -metadata artist='First artist' -metadata album='Example album' -metadata track='6/10' -metadata disc='1' -metadata genre='Rap/Hip Hop' tests/fixtures/silence-alac.m4a
ffmpeg -v error -n -f lavfi -i 'anullsrc=r=44100:cl=stereo' -t 0.05 -c:a aac -fflags +bitexact -flags:a +bitexact -map_metadata -1 tests/fixtures/silence-aac.m4a
```

The ALAC fixture contains 2,205 16-bit samples per channel. The AAC fixture
tests refusal of an ordinary non-ALAC audio container, irrespective of its `.m4a`
extension. Neither fixture demonstrates device compatibility or synchronization.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `silence-alac.m4a` | 936 | `e8e1bdb74828640934c26a54e5d439353d53ddbba8dd92781b940413ba1d3d71` |
| `silence-aac.m4a` | 794 | `03aebf3d17c740e8677e55376e099a84fcc1dd53a2aa58bc431b6a4b22f1fcac` |

## Format fixtures

Tiny 440 Hz tones: one per format the container readers handle (every format
but ALAC and MPEG-1 Layer III), and one of each of those two. Every one is
tagged title `Format title`, artist `Format artist`, album `Format album`,
track 3/9, date 2024 and genre `Test`. The ALAC tone and the VBR MP3, which is
MPEG-1, are the cases that the ALAC and MPEG-1 readers handle.

```bash
md=(-metadata title='Format title' -metadata artist='Format artist' -metadata album='Format album' -metadata track='3/9' -metadata date='2024' -metadata genre='Test')
src=(-f lavfi -i 'sine=frequency=440:sample_rate=44100' -t 0.1 -ac 2)
bx=(-fflags +bitexact -flags:a +bitexact -map_metadata -1)
ffmpeg -v error -n "${src[@]}" -c:a aac -b:a 128k "${bx[@]}" "${md[@]}" tone-aac.m4a
ffmpeg -v error -n "${src[@]}" -c:a flac "${bx[@]}" "${md[@]}" tone-flac.flac
ffmpeg -v error -n "${src[@]}" -c:a pcm_s16le "${bx[@]}" "${md[@]}" tone-wav.wav
ffmpeg -v error -n "${src[@]}" -c:a pcm_s24be -write_id3v2 1 "${bx[@]}" "${md[@]}" tone-aiff.aif
ffmpeg -v error -n -f lavfi -i 'sine=frequency=440:sample_rate=22050' -t 0.2 -ac 1 -c:a libmp3lame -b:a 32k "${bx[@]}" "${md[@]}" tone-mpeg2.mp3
ffmpeg -v error -n "${src[@]}" -c:a alac -sample_fmt s16p "${bx[@]}" "${md[@]}" tone-alac.m4a
ffmpeg -v error -n "${src[@]}" -t 0.3 -c:a libmp3lame -q:a 4 "${bx[@]}" "${md[@]}" tone-mp3-vbr.mp3
```

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `tone-aac.m4a` | 2800 | `4f4c1b43a5fd9e3f28cfde7dc9fcbc79220f9e167b21407e790e228186bd8397` |
| `tone-flac.flac` | 9602 | `fa65cdba8cc40a558257324c7682cd8e5fba67967dcd37eab42867fc92ac4c33` |
| `tone-wav.wav` | 17802 | `737998e0e9c38fc370377e77c28805593450e3f7b0f2e694f6612cfb0b0b4931` |
| `tone-aiff.aif` | 26682 | `ec3790b12c7289ede02b4e9cb5cc495ab32922d91c750204941ce6ed936bccdf` |
| `tone-mpeg2.mp3` | 1367 | `cd34c1d1b80e43e176651c88854a761368523c0e9020c67f1d72594e5fad317d` |
| `tone-alac.m4a` | 2691 | `1987b1362840940921b3f3e8d822219d7308180c96f1d35e65ba9fbb99ea0eda` |
| `tone-mp3-vbr.mp3` | 2457 | `48c75f29f519eaf02c9cf8377e81a4120c9faa28168d361021afc7d632601801` |

## Fixed-request signature vectors

`grappa-fixed-vectors.plist` contains four deterministic synthetic request,
reply, operation, decoded-plaintext, derived-key and sidecar vectors. It contains
no captured phone or library data. The vectors were verified against the
standalone public calculation and an independent standard AES/HMAC reconstruction;
see [chapter 02, section 11](../../docs/02-atc-sync-protocol.md#11-signing).
They validate that narrow calculation, not current-device acceptance or the
different native Apple host-request mode.

The binary plist is 1,199 bytes, SHA-256
`737a226ebed8700efed366b01372ada2d5b99d062b3e9da7cd6ba99f67306e4e`.

`crates/music-protocol/tests/fixtures` keeps its own copies of these files, so
that the library builds and tests without this directory.
