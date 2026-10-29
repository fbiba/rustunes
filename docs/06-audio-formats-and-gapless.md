# Audio formats, capability decisions and MP3 gapless metadata

This chapter records which audio formats the iPhone Music app accepts through the sync path, how Apple's Windows host describes each one in the track record it sends, which formats it refuses or transcodes, and what the phone stored when each format was actually sent. It also gives the complete rule by which Apple Devices computes the MP3 gapless fields (encoder delay, drain, duration, last-frame resynch and the heuristic word), with addresses in both analysed builds, an acceptance test against captured traffic, and what the phone stored and played when MP3s were sent from Linux by that rule. A final part covers cover art: how ID3 and MP4 pictures were read back, a study of decoding phone-side JPEG renditions without an external ffmpeg process, which cover image types Apple Devices sends, and what the phone keeps of a JPEG or PNG original.

## Contents

1. Scope and provenance
2. Evidence labels and conventions
3. How the track record is produced
4. Summary table
5. `audio_format` codes
6. `location.kind` strings
7. Containers and on-device file extensions
8. `avformat_info` per codec
9. Transcoding, refusal and capability decisions
10. The phone's own capability list
11. Media kinds
12. What the phone did with each format
13. MP3 gapless metadata (how Apple Devices computes it)
14. Cover art
15. Things that contradict or refine naive assumptions
16. Open questions
17. Evidence

## 1. Scope and provenance

Phone: iPhone17,2, iOS 27.0.1, build 24A446.

Host binaries analysed (static analysis only; no Apple binary was executed for these findings): Apple Devices 1.1540.23042.0, containing AMPDevicesAgent.exe 1.6.4.90 (called AMP below), AirTrafficHost.dll and MobileDevice.dll; and Apple Devices 1.1540.24088.0, containing AMPDevicesAgent.exe 1.6.4.94 (AMP2). The binaries are not redistributed; only addresses and findings are given. Unless stated, addresses are virtual addresses `0x1400xxxxx` in AMP (image base `0x140000000`). Every function and constant quoted is byte-identical in AMP2 at a shifted address; the MP3 gapless functions were compared instruction by instruction (section 13.15 gives both addresses).

Runtime evidence used:

- Captured Apple Devices traffic: one `insert_track` for an MP3 file (called "Track R" below: an ffmpeg-made 128 kb/s CBR file, 4,105,105 bytes) and over a thousand `insert_track` messages for ALAC files.
- Phone database captures and the phone's own lockdown capability list.
- Test files sent from Linux to the phone over USB and Wi-Fi (generated tones, 8 s each, plus one 1.4 to 2.6 s MP3 per timing experiment), then read back and played. They include MP3s carrying the gapless fields computed by the rule of section 13 (section 12.7) and test covers of every JPEG kind and PNG covers (section 14.4).
- A few Linux tools (`ffmpeg`, `ffprobe`, `lame`) on Track R and on synthetic sine files, used only to test the reading of the code in section 13.

[Chapter 02](02-atc-sync-protocol.md) describes the message that carries these fields. [Chapter 03](03-transport-usb-and-wifi.md) covers how bytes reach the phone. [Chapter 04](04-media-library-database.md) covers the database columns they land in, and [chapter 07](07-sync-engine-design.md) the client-side policy built on them.

## 2. Evidence labels and conventions

| Label | Meaning |
| --- | --- |
| CONFIRMED | Seen in code, constants or data flow in the host binaries, in captured traffic, or on the phone. |
| INFERRED | From names, strings or analogy only. |
| OPEN | Not answered by the available evidence. |

A later or explicitly corrected statement overrides an earlier one. Corrections to earlier statements about these formats are listed in section 13.14.

Terms: "coded samples" is `frames * samples-per-frame` (1152 for MPEG-1 Layer III, 576 for MPEG-2 and 2.5 Layer III). "Valid samples" is the audible length after encoder delay and padding. All durations in `avformat_info` are sample counts, not milliseconds.

## 3. How the track record is produced

CONFIRMED.

- AMP is the full iTunes library and sync engine. The `insert_track` builder (`0x140461900`) reads a `TrackInstance` / `TrackInfo` taken from the Apple Music library (`AMPLibraryClient`, strings `amplc>`). Every `avformat_info` number, the kind string and the extension are copied from that record.
- For MP3, AMP's own MPEG handler fills the gapless group (section 13). An earlier reading that "the host does not analyse the file" was wrong for MP3. For ALAC and AAC the values come from the library and from the MP4 importer's parse of the `iTunSMPB` atom (section 8.2).
- A helper `0x14069950c(dict, key, value, alwaysSet)` omits a number from the plist when it is 0 and `alwaysSet` is false (`0x140699593`: `test rdi,rdi / jne / test r9b,r9b / je skip`). That is why keys vary by codec: ALAC records have no `channels` (library value 0) and MP3 records have no `analysis_inhibit_flags` (0).
- All code constants and mappings quoted in this chapter are byte-identical in AMP2 (`code->fourcc` at `0x3b24c4` in AMP2, `fourcc->code` at `0x3ae59a`, extension switch at `0x49474c`).

## 4. Summary table

`audio_format` is the number in `avformat_info.audio_format` and in the phone's `item_playback.audio_format`. "Device codec" is the name AMP looks for in the phone's `AudioCodecs` capability dictionary (section 10) before it will send the file. If it is missing the track is refused as `UnsupportedAudioFormat` (status 13) unless the "convert higher bit rate songs" preference is on.

| Source codec / container | `audio_format` | `location.kind` | `location.extension` / device ext | Notes | Evidence |
| --- | --- | --- | --- | --- | --- |
| ALAC in M4A | 601 | "Apple Lossless audio file" (phone kind id 26) | `"M4A "` / `.m4a` | `bit_rate` 1411 nominal for 44.1 kHz 16-bit stereo; `gapless_heuristic_info` 1 and delay/drain/resynch 0; `analysis_inhibit_flags` 2; no `channels` | CONFIRMED (over a thousand real inserts); other depths and rates CONFIRMED on the phone (section 12.3) |
| AAC-LC in M4A (unprotected) | 502 (mp4a subtype 2, or 0) | "AAC audio file" (phone kind id 12) | `"M4A "` / `.m4a` | `channels` if the library has it; gapless group only if heuristic bit 0 | code CONFIRMED; sent and played on the phone |
| HE-AAC (SBR), HE-AAC v2 (PS) | 508, 509 (mp4a subtype 8, 9) | "AAC audio file" (kind 12) | `.m4a` | device codec `haac` is derived from the `AAC` entry with the maximum sample rate doubled; other mp4a subtypes 501, 503 to 507, 510 have no device codec and are refused | CONFIRMED; sent and played |
| xHE-AAC (USAC) in M4A | 1001 (`usac`) | "AAC audio file" (kind 12) | `.m4a` | device codec `usac` must exist | CONFIRMED; sent and played |
| Protected AAC (`.m4p`) | 502 | library | `"M4P "` / `.m4p` | never transcoded; needs `supportsEncryption` | code CONFIRMED; not sent |
| Audiobook (`.m4b`) | 502 | library | `"M4B "` / `.m4b` | item gets `is_audio_book`; audiobook limits (`MaxAudiobookLimitExceeded`) | code CONFIRMED; not sent |
| Ringtone (`.m4r`) | 502 | library | `"M4R "` / `.m4r` | duration over 40,000 ms refused (`RingtoneDurationTooLong`, `0x140495bc5`); never transcoded | code CONFIRMED; not sent |
| MP3, MPEG-1 Layer III | 301 | "MPEG audio file" (kind 42) | `"MP3 "` / `.mp3` | `channels` present; gapless group (section 13); no `analysis_inhibit_flags`; VBR needs `supportsVBR` | CONFIRMED (traffic and phone) |
| MP3, MPEG-2 and MPEG-2.5 Layer III | 302, 303 | "MPEG audio file" (kind 42) | `.mp3` | accepted exactly like 301 | CONFIRMED (code); 302 sent and played |
| MPEG Layer I and II (`.mp1`, `.mp2`) | 101 to 103, 201 to 203 | n/a | file type `MPG2` maps to `.mp3` | refused: the audio check accepts only 301 to 303 (`0x140495d26`, falls to status 13) | CONFIRMED (code) |
| WAV (integer PCM) | 701 (`lpcm`) | "WAV audio file" (kind 45) | `"WAV "` / `.wav` | device codec `wave`; converted when the preference is on and bit rate exceeds the threshold | CONFIRMED; sent and played |
| AIFF, AIFF-C (`NONE`, `sowt`) | 701 | "AIFF audio file" (kind 44) | `"AIF "` / `.aif` | device codec `aiff`; with the conversion preference on, always converted to AAC (`0x140495a33`) | CONFIRMED; sent and played |
| FLAC | 602 | "FLAC audio file" is unknown to the phone: stored kind id 0 | `"FLAC"` / `.flac` | device codec `flac` must exist | CONFIRMED; sent and played |
| Audible `.aa`, `.aax` | 401 to 404, 406 | library | `.aa`, `.aax` | device codecs `aa2`, `aa3`, `aa4`, `aax` from the device's `Audible` dictionary | code CONFIRMED; not sent |
| AC-3, E-AC-3 | 901, 902 | n/a | none | exist only in the code to FourCC table (video tracks); no audio file type reaches them | CONFIRMED (code) |
| Opus, Ogg, WMA, CAF, ADTS `.aac`, MIDI | none | n/a | WMA, CAF, MIDI have extension rows; the accept switch has no case | refused as "unsupported file type" (status 11); Opus and ADTS have no mapping at all | CONFIRMED (code) |

Phone kind ids come from `item_extra.location_kind_id` (section 6).

## 5. `audio_format` codes

CONFIRMED. The number scheme is present in four places in AMP, all consistent.

- `0x1403af144` maps library codec type and subtype to the number (default 1):

  | Library codec | Number |
  | --- | --- |
  | `'alac'` (`0x616c6163`) | `0x259` = 601 |
  | `'audb'` (`0x61756462`) | `0x191` = 401 if subtype 0, else `0x190 + subtype` |
  | `'flac'` | `0x25a` = 602 |
  | `'lpcm'` (`0x6c70636d`) | `0x2bd` = 701 |
  | `'mp4a'` (`0x6d703461`) | `0x1f6` = 502 if subtype 0, else `0x1f4 + subtype` |
  | `'mpeg'` (`0x6d706567`) | subtype 1: `0x65` = 101; subtype 2: `0xc9` = 201; otherwise `0x12d` = 301 |
  | `'usac'` | `0x3e9` = 1001 |
  | anything else | 1 |

- `0x1403af24c` is the inverse (number to codec and subtype) and agrees: 101 to 103 mpeg/1, 201 to 203 mpeg/2, 301 to 303 mpeg/3, 401 to 406 audb, 501 to 510 mp4a, 601 alac, 701 lpcm, 602 flac, 1001 usac.
- The MPEG handler (`0x140439004`, `itTrackInfoPriv->audioFormat`, assertion string at `0x14099f150`) sets the number from the frame header: MPEG-1 gives x01, MPEG-2 x02, MPEG-2.5 x03; Layer III gives 3xx, Layer II 2xx, Layer I 1xx. So 301 is MPEG-1 Layer III (44.1, 48, 32 kHz), 302 is MPEG-2 Layer III (22.05, 24, 16 kHz), 303 is MPEG-2.5 Layer III (11.025, 12, 8 kHz).
- `0x1403b30c4` maps the number to the **device** codec FourCC used for the capability check:

  | Number | Device codec |
  | --- | --- |
  | 301, 302, 303 | `'mp3 '` |
  | 402, 403, 404, 406 | `'aa2 '`, `'aa3 '`, `'aa4 '`, `'aax '` |
  | 502 | `'aac '` |
  | 508, 509 | `'haac'` |
  | 601 | `'alac'` |
  | 602 | `'flac'` |
  | 701 | `'pcm '` |
  | 801, 802, 803, 804 | `'samr'`, `'ima4'`, `'ulaw'`, `'alaw'` (voice memos) |
  | 901, 902 | `'ac3 '`, `'eac3'` |
  | 1001 | `'usac'` |
  | everything else (101/201 families, 401, 501, 503 to 507, 510) | 0, which fails the check |

AAC is 502 for every common AAC-LC file, not 501. FLAC and PCM are separate numbers (602, 701). Real Apple traffic contained only 301 and 601; the other numbers were confirmed later on the phone (section 12).

## 6. `location.kind` strings

- CONFIRMED. The kind string is copied from the Apple Music library, not computed in AMP. `0x140395a14(trackInfo, buf)` looks up `kind_id` (`[trackInfo+0xf4]`) in the library's kind map, and the builder (`0x14046221f`, `0x140462476`) stores it under `location.kind`. No "... audio file" string exists in AMP, AirTrafficHost.dll or MobileDevice.dll (checked as ASCII and UTF-16).
- CONFIRMED. The phone stores only an integer, `item_extra.location_kind_id`; the string is not kept. Known mappings:

  | String sent | Phone `location_kind_id` | How known |
  | --- | --- | --- |
  | "Apple Lossless audio file" | 26 | Apple traffic and phone |
  | "MPEG audio file" | 42 | Apple traffic and phone (MPEG-1 CBR, MPEG-1 VBR, MPEG-2) |
  | "AAC audio file" | 12 | test inserts (AAC-LC, HE-AAC, HE-AAC v2, xHE-AAC) |
  | "AIFF audio file" | 44 | test insert |
  | "WAV audio file" | 45 | test insert |
  | "FLAC audio file" | 0 (the phone does not know the string) | test insert |

  Kind 0 does not stop the track from being supported (`is_supported_type` was 1) or from playing.
- Not tried: Protected AAC, Purchased AAC, Audible. Their strings are INFERRED from iTunes naming and their phone ids are unknown.
- Apple's own `location` dictionary is larger than a kind alone: `location_type` `"FILE"`, `extension` (OSType text such as `"M4A "`, `"MP3 "`), `date_created`, `file_size` and the full permanent path, for example `iTunes_Control/Music/F02/KWZT.m4a` and `.../F40/HQVN.mp3` (so the host, not the phone, picks `Fxx/XXXX.ext`). The builder code is at `0x140462421` to `0x140462518`. A location with only `kind` and the destination path was enough for every format sent.

## 7. Containers and on-device file extensions

CONFIRMED.

- The device extension is chosen by `GetIPodTrackFileExtension` (assertion text at `0x1409a2bd0`, function `0x140495214`) from an OSType "file type" derived from the **source file's extension** through a table at `0x1408de2e0` (12-byte rows: extension, native type, canonical type):

  | Source extensions | File type |
  | --- | --- |
  | `m4a m4p m4b m4r` | `M4A `, `M4P `, `M4B `, `M4R ` |
  | `mp3 mpg mpga mp2` | `MPG3` / `MPG2` |
  | `m4v movp mov mp4` | video types |
  | `aif aiff aifc cdda cda` | `AIFF` |
  | `wav wave` | `WAVE` |
  | `aa aax`, `sd2`, `wma`, `nvf`, `3gp 3gpp 3g2 3gp2`, `mid`, `flac`, `caf` | own rows |

  `.aac`, `.adts`, `.opus`, `.ogg` are not in the table.
- File type to device extension (switch at `0x140495224`): `M4A `, `M4B `, `M4P `, `M4R ` to `m4a`, `m4b`, `m4p`, `m4r`; `MPG2` and `MPG3` to `mp3`; `WAVE` to `wav`; `AIFF` to `aif`; `flac` to `flac`; `AUDB` to `aa`; `AUDX` to `aax`; `MP4 ` to `mp4`; `M4V ` to `m4v`; `MooV` to `mov`; `3gp`, `3g2`; `epub`; `PDF ` to `pdf`; `WMA ` to `wma`; `iboo` to `ibooks`; `ite `; `itlp`. An unknown type has no extension and the track is not synced. An `.m4a` source flagged as a ringtone becomes `M4R ` (`0x1404951af`).
- ALAC, AAC, HE-AAC and xHE-AAC all share `.m4a`. `.m4b` and `.m4p` are chosen from the source extension, not from the codec.
- Destination names accepted by the phone: `.m4a`, `.mp3` (CONFIRMED from Apple traffic); `.aif`, `.wav`, `.flac` (CONFIRMED on the phone). An earlier experiment found the phone rejects `.mp3` at a host path that expects `.m4a` ("unsupported permanent filename"), so the extension must match the codec. `.m4b` and `.m4p` as destinations are untested.

## 8. `avformat_info` per codec

### 8.1 Builder layout

The builder (`0x1404638d0` to `0x140463996`, `0x140462041` to `0x1404620c8`, `0x140463c6d`) emits these keys. Zero values are omitted except inside the gapless group.

| Key | Source field in TrackInstance | Meaning | Emitted when |
| --- | --- | --- | --- |
| `audio_format` | WORD from `0x14036afac` (library DB value, or `0x1403af144` for shared tracks) | section 5 number | non-zero |
| `bit_rate` | WORD `[+0x54]` | kb/s as stored by the library. ALAC: PCM rate, `rate * bits * channels / 1000` (1411 for 44.1 kHz, 16-bit, stereo). MP3: header or Xing/VBRI average (section 13.12) | non-zero |
| `sample_rate` | float `[+0x50]`, sent as a real | Hz | non-zero |
| `channels` | WORD `[+0x56]` | channel count | non-zero (present for MP3, absent for ALAC) |
| `duration` | QWORD `[+0xd0]` | **samples** at `sample_rate`, after delay and drain | non-zero |
| `gapless_heuristic_info` | DWORD `[+0xe0]` | flag word; the group is emitted only if bit 0 is set (`test byte [r13+0xe0],1` at `0x140463c6d`) | gapless group |
| `gapless_encoding_delay`, `gapless_encoding_drain` | DWORD `[+0xe4]`, `[+0xe8]` | samples to skip at start and end | with the group, always, even when 0 |
| `gapless_last_frame_resynch` | QWORD `[+0xd8]` | byte offset, see section 13.5 | with the group |
| `analysis_inhibit_flags` | BYTE `[+0x47]` | 2 for the library's ALAC files | non-zero |
| `audio_fingerprint` | QWORD `[+0xc8]` | not seen in real traffic | non-zero |
| `volume_normalization_energy` | WORD `[+0x5a]` | Sound Check energy | non-zero |
| `is_transcoded` | bit 7 of byte `[+0x49]` | set if the file is a conversion Apple made | always |
| `item.total_time_ms` | DWORD `[+0x64]` | milliseconds | non-zero |

The plist has no `is_vbr`, `codec_subtype` or `data_kind` key. The phone table `item_playback` has `codec_type`, `codec_subtype`, `data_kind` and `format`, but every row from Apple Devices and from later test inserts leaves them 0 or NULL. The VBR flag (bit 4 of `[+0x49]`, folded into flag bit `0x2` by `0x1403a2930`) is used only for the accept check and is not sent. CONFIRMED.

### 8.2 MP4 family (ALAC, AAC, HE-AAC, xHE-AAC)

- The MP4 importer parses the freeform `iTunSMPB` atom (`0x1403c5dc5`, parser `0x1403c3024`). It requires exactly 116 characters (12 space-separated hex fields: one unused, delay, padding, a 16-hex-digit original sample count, and so on); a zero sample count rejects it. Delay and padding are stored at `[+0x54]` and `[+0x68]` of the importer's private record. `iTunNORM` (`0x1403c5cfe`) feeds the Sound Check energy. No `elst`, `sgpd` or roll handling was found by name.
- Without `iTunSMPB`, the real ALAC rows are delay 0, drain 0, heuristic 1, resynch 0.
- The phone stored heuristic 1 and delay 1024 for an AAC-LC file (section 12.2), which is the standard AAC encoder delay.

### 8.3 MPEG family

See section 13 for the complete rule. In short: the MP3 record carries `channels`, a gapless group derived by AMP's own decoder, and a total time that counts the first (Info) frame. The values for Track R, the one captured MP3:

```
audio_format 301, bit_rate 128, channels 2, sample_rate 44100.0
duration 11162592            (9691*1152 - 528 - 912)
item.total_time_ms 253152    (11,164,032 coded samples * 1000 / 44100)
gapless_heuristic_info 33554435 (0x02000003)
gapless_encoding_delay 528, gapless_encoding_drain 912
gapless_last_frame_resynch 4047097, volume_normalization_energy 1792
```

## 9. Transcoding, refusal and capability decisions

Decision function: `0x140495790` (file type, track instance, device context; status in `r15d`). Status names come from the table behind `0x1408dcc60`: NoProblem 0, RequiresTranscoding 1, UnsupportedFileType 11, UnsupportedAudioFormat 13, UnsupportedSampleRate 15, UnsupportedBitRate 16, VariableBitRateNotSupported 17, UnsupportedNumChannels 49, and others. CONFIRMED.

1. **Early refusals** (`0x140495937` to `0x14049596f`): subscription (Apple Music) tracks (`IsSubscriptionMusicTrack` 55, podcast 66), family-sharing tracks (54), ringtones over 40,000 ms (42, `0x140495bce`). Cloud and remote statuses (46, 47) exist; where they are raised was not traced.
2. **File type switch** (`r12d` = OSType): `M4A `, `M4B `, `M4P `, `M4R ` go to the audio check with the codec from the `audio_format` number. `AUDB` and `AUDX` go to the Audible check (`aa2`, `aa3`, `aa4`, `aax`, anything else is 13). `MPG2` and `MPG3` accept only `audio_format` 301 to 303 with codec `mp3 `. `AIFF` uses codec `aiff`; `WAVE` uses `wave`; `flac` uses `flac`. `M4V `, `MP4 `, `MooV`, 3gp go to the video path. Everything else (`WMA `, `caff`, unknown) gives status 11.
3. **Capability check** `0x1403b35f4(deviceClass, deviceCodec, codecList, isProtected, bitRate, sampleRate, channels, flags)` against the device's codec list (36-byte entries):
   - no entry for the codec and no wildcard `'****'` entry: status 13;
   - protected file but `supportsEncryption` false: 11; VBR file but `supportsVBR` false: 17;
   - sample rate (if non-zero) below `MinimumSampleRate` or above `MaximumSampleRate`: 15;
   - bit rate below `MinimumBitRate` or above `MaximumBitRate` (protected) or `MaximumUnencryptedBitRate` (unprotected): 16;
   - channels: 1 needs `Mono`, 2 needs `Stereo`, otherwise `Multichannel`: 49;
   - an extra rule only for device class `'iPod'` (not iPhone or iPad) and codecs `aac ` and `alac`: the sample rate must be one of 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000 Hz (`0x1403b3789` to `0x1403b37f4`), else 15;
   - `MaximumBitDepth` is parsed into the entry but **not used** by either audio check function (`0x1403b35f4`, `0x1403b4438`). High-resolution limits (24-bit, above 48 kHz) are therefore expressed only through the sample-rate and bit-rate limits of the device entry. INFERRED from the absence of a reader.
4. **Entry defaults** (`0x1403b2e6c`): VBR, mono, stereo and multichannel all true; minimum sample rate 0; maximum sample rate 48,000 Hz; bit-rate limits unlimited. If the device dictionary has no `MaximumSampleRate`, devices of class iPad, iPhone or iPod (`0x1404ca5bc`) get unlimited (`FLT_MAX`, `0x14051af93`). `haac` is cloned from `aac ` with the maximum sample rate times 2 (`0x1403b3560`, constant `2.0f`). Audible gets `aa2`, `aa3`, `aa4` from the keys `Type2`, `Type3`, `Type4` and `aax` from `AAC` inside an `Audible` dictionary (`0x14051ae30`). CONFIRMED.
5. **Where the device list comes from.** The consumers are CONFIRMED: the dictionary keys `AudioCodecs`, `VideoCodecs`, `VoiceMemoFormats` (`0x14051c9bc`, `0x14051cace`, `0x14051cd6e`). Per-codec keys (table at `0x140916430`): `MinimumSampleRate`, `MaximumSampleRate`, `MinimumBitRate` / `MinimumDataRate`, `MaximumBitRate` / `MaximumDataRate`, `MaximumUnencryptedBitRate` / `...DataRate`, `MaximumBitDepth`, `Mono`, `Stereo`, `Multichannel`, `VariableBitRate`, `LC/VariableBitRate`. Codec names accepted by `0x1403b31ac`: `MP3`, `AAC`, `WAV`, `AIFF`, `AppleLossless`, `AC3`, `EnhancedAC3`, `FLAC`, `USAC` (plus `...AudioCodecInfo` aliases) and `Audible`. An unknown name is a logged assertion, so Opus, WMA and similar codecs cannot be advertised. The device must also advertise AAC and the three Audible types, or the whole device record fails validation (`0x14051eb7e`, `0x14051eb3e`). The code that fetches the dictionary from the phone was not found, but the phone itself exposes it (section 10).
6. **Convert higher bit rate songs.** Preferences `transcodeSongs` (bool) and `transcodeBitRate` (128, 192 or 256 kb/s; strings `Transcode 128/192/256`; threshold WORD at context `+0xe016`). With the option on (`0x140495915`, `[ctx+0x392]` and the per-sync flag) and the track not protected:
   - M4A, M4B, M4P (including ALAC and AAC), MP3 and FLAC: if the capability check fails **or** the bit rate exceeds the threshold, the result is RequiresTranscoding (1), except ringtones;
   - WAV: bit rate over the threshold gives 1, else the normal `wave` check;
   - AIFF: always 1 (`0x140495a33`).

   The result is an AAC encode at the chosen bit rate (`AAC Encoder`, `ACMP4AACHighEfficiencyEncoder`, `perf> transcode of track ... to %u kbps`) written as M4A (AAC, 502). With the option off, every failure is a skipped track with the status as the warning. CONFIRMED (control flow). The default of the preference was not decoded.

## 10. The phone's own capability list

CONFIRMED (read from the phone over Wi-Fi). The dictionary that AMP checks against is the lockdown domain `com.apple.mobile.iTunes`, key `AudioCodecs`. The rest of that domain holds FairPlay identifiers. The audio part for the iPhone17,2 on iOS 27.0.1:

| Name | Keys the phone sets |
| --- | --- |
| `MP3` | MaximumDataRate 320, MaximumSampleRate 48000, Mono, Stereo |
| `AAC` | MaximumSampleRate 96000, LC {VariableBitRate, PerceptualNoiseSubsitution}, AppleDRM |
| `USAC` | Codec `usac`, MaximumSampleRate 96000, Mono, Stereo, Multichannel false |
| `AppleLossless` | MaximumBitDepth 32, MaximumBitDepthUntruncated 16, Mono, Stereo, Multichannel false, AppleDRM |
| `FLAC` | Codec `flac`, Mono, Stereo, Multichannel false |
| `WAV` | MaximumBitDepth 24, Mono, Stereo, Multichannel false |
| `AIFF` | MaximumBitDepth 24, Mono, Stereo, Multichannel false |
| `AC3`, `EnhancedAC3` | MaximumSampleRate 48000, AppleDRM (Apple Devices maps these only for video) |
| `AMR` | Codec `samr`, 8000 Hz mono 16-bit (voice memos) |
| `Audible` | AAC, Type2, Type3, Type4 (Type1 false) |

Read with the rules of section 9 (iPhone class, so a missing `MaximumSampleRate` means no limit):

- MP3: MPEG-1, MPEG-2 and MPEG-2.5 Layer III, at most 320 kb/s and 48 kHz, mono or stereo, VBR allowed (there is no `VariableBitRate: false`).
- AAC-LC (502): up to 96 kHz, VBR allowed. HE-AAC (508, 509): up to 192 kHz (the AAC entry doubled). xHE-AAC (1001): up to 96 kHz, mono or stereo.
- ALAC (601): any sample rate, mono or stereo. FLAC (602): any sample rate, mono or stereo.
- WAV and AIFF (701): mono or stereo. Their `MaximumBitDepth` 24 is advertised but Apple Devices never reads it (section 9).
- Multichannel is refused for USAC, ALAC, FLAC, WAV and AIFF. AAC and MP3 leave it unset (default true for AAC; MP3 multichannel does not exist).

## 11. Media kinds

CONFIRMED.

- Item flags come from the TrackInfo media-kind bit mask (`0x140462b1b` to `0x140462d56`): `is_song` is bit 0 or bit 8 (mask `0x101`); movie bit 1; podcast bit 2; audiobook bit 3; music video bit 5; TV show bit 6; home video bit 10; ringtone bit 14; rental bit 15; digital booklet bit 16; voice memo bit 20; iTunes U bit 21; book bits 22 and 23; tone (alert) bit 24. `is_subscription` comes from byte `[+0x4a]` bit 2. The real insert sends only `is_song: true`.
- `GetTrackInfoDBItemKind` (`0x1403b24a8`) maps the mask to legacy DB item kinds: song 5, music video 6, movie 7, home video 8, podcast and iTunes U 13, audiobook 15, TV show 11, ringtone and tone 22, book 23, booklet 17; others are "unmappable". The phone's own `item_extra.media_kind` is 1 for songs, and the `item.media_type` / `media_kind` plist keys are not sent for songs.
- Audiobooks are `.m4b` (file type `M4B `) or Audible `.aa` / `.aax`; they use the same AAC `audio_format` (502) and extension rules. Not examined further.

## 12. What the phone did with each format

All results below come from files sent from Linux in insert batches over Wi-Fi unless stated otherwise. Test files were 8 s generated tones. After sending, the files were read back from the phone and each track was played.

### 12.1 Six formats in two batches

One batch added, without covers (60 s): AAC-LC 256 kb/s `.m4a`, AIFF 16-bit 44.1 kHz, FLAC 16-bit 44.1 kHz, MPEG-2 Layer III at 22.05 kHz, MPEG-1 VBR (Xing) and WAV 16-bit 44.1 kHz. Another batch added AAC, AIFF, FLAC and VBR MP3 with a front cover each; all four covers verified. A full read-back then matched 405 of 405 files.

What the phone recorded:

| File | `location_kind_id` | `audio_format` | gapless heuristic / delay | `is_supported_type` |
| --- | --- | --- | --- | --- |
| AAC `.m4a` | 12 | 502 | 1 / 1024 | 1 |
| AIFF `.aif` | 44 | 701 | 1 / 0 | 1 |
| FLAC `.flac` | **0** | 602 | 1 / 0 | 1 |
| MPEG-2 `.mp3` | 42 | 302 | 0 / 0 | 1 |
| MPEG-1 VBR `.mp3` | 42 | 301 | 0 / 0 | 1 |
| WAV `.wav` | 45 | 701 | 1 / 0 | 1 |

The phone maps "AAC audio file", "AIFF audio file" and "WAV audio file" to kinds. It does not know "FLAC audio file" (kind 0). Every other column of the FLAC row matches the WAV row. Kind 0 does not stop playback. For the two MP3 rows the sender included no gapless group, and the phone stored heuristic 0 and delay 0. Section 12.7 has what the phone stored once the group was sent.

### 12.2 AAC-LC, HE-AAC, HE-AAC v2 and xHE-AAC

- AAC-LC: `audio_format` 502, kind 12, heuristic 1 with delay 1024 stored (above).
- HE-AAC (with a cover), HE-AAC v2, xHE-AAC (exhale layout, with a cover) and xHE-AAC with eSBR were sent in one batch. The phone stored kind 12 and `audio_format` 508, 509, 1001, 1001; all were supported and the covers verified. Each played normally. This confirms that the phone accepts xHE-AAC (USAC) even though no real Apple traffic contained it.
- The test encoders were fdk-aac v2.0.3 (`libfdk_aac -profile:a aac_he`) for HE-AAC and v2, and exhale v1.2.2 (presets 0 to 9, or a to g with eSBR) for xHE-AAC, both producing `.m4a`. exhale writes `moov` before `mdat` and leaves two `mdat` bytes that no sample uses; a remux with `ffmpeg -c copy` drops those bytes.
- Covers: AAC covers use the MP4 `covr` checksum rule (payload + 17, section 14.1).

### 12.3 ALAC beyond 44.1 kHz and 16-bit

CONFIRMED. The phone takes ALAC at 16, 20, 24 and 32 bits, mono or stereo, at any sample rate tried. A further insert batch added 96 kHz 24-bit stereo and 48 kHz 16-bit mono. The insert's `bit_rate` is the PCM rate: 4608 for 96000 * 24 * 2 and 768 for 48000 * 16 * 1, the same rule as Apple's 1411 for 44.1 kHz, 16-bit, stereo. Both tracks got kind 26, were supported, and had durations of 768,000 and 384,000 samples (8 s at 96 kHz and 48 kHz). They played.

### 12.4 Playback

All 13 test tracks of the first four batches (six formats, four covered tracks, two high-resolution ALAC, one retagged AAC) played normally on the phone, including both FLAC files. The four covered tracks showed their cover. The four HE-AAC and xHE-AAC tracks of the later batch also played normally.

### 12.5 Retagging non-ALAC files

A FLAC test track retagged with `ffmpeg -c copy` was detected as a tag-only change and sent as an update (title, year); the metadata change was verified on the phone. An AAC file first looked like changed audio because an identity over the whole MP4 sample description entry included the `esds` bit rates, which a remux rewrites. Hashing only the AudioSpecificConfig (as the ALAC identity hashes its cookie) keeps the identity stable under remuxing. A fresh AAC track retagged the same way then updated normally.

### 12.6 MP3 experiments without a gapless group (what was sent, what the phone stored)

These runs sent MP3 files from Linux with different fields and read the result back.

| Run | Source | What was sent | What the phone stored |
| --- | --- | --- | --- |
| First generated tone (20,645 bytes, 49 frames) | MPEG-1 Layer III, CBR 128 kb/s, 44.1 kHz, joint stereo; ID3v2.3 title, artist, album; no artwork, no Xing | `update_db_info` plus one `insert_track` with `audio_format` 301, `bit_rate` 128, `sample_rate` 44100.0 (real), `channels` 2; `location` with only `kind: MPEG audio file`; artwork checksum 0; **no** duration, no gapless group, no energy | file stored byte-identical at the permanent `.mp3` path; native duration **0** and `total_time_ms` **0.0** |
| Second generated tone (41,121 bytes, 98 frames, 112,896 coded samples) | same shape; ID3v2.3, TSSE `Lavf62.12.103`, no Xing, offset 161, 40,960 frame bytes | as above plus integer `avformat_info.duration` 112896 and integer `item.total_time_ms` 2560; no gapless group, no energy | `item_playback.duration` INTEGER 112896 and `item_extra.total_time_ms` REAL 2560.0; file identical |
| Tone with 59 frames (24,853 bytes, 67,968 coded samples) | Unicode title, no artwork, no Xing | coded duration | total time 1541 ms (floor of 1541.2); catalogue duration as sent |
| Tone with rich tags (23,083 bytes, 54 frames, 62,208 coded samples) | title, artist, album artist, album, genre, composer, copyright, year, track 1 of 2, disc 1 of 1 | coded duration 62,208 and 1,410 ms | all fields mapped to the right catalogue columns; five new groups (album, album artist, item artist, genre, composer) created; album year correct |

Observations:

- CONFIRMED. A positive length is needed. With both duration scalars omitted the phone stored zero. With the physical coded sample count and its floor in milliseconds, the phone stored them as sent.
- CONFIRMED. The phone accepted the record without a gapless group for headerless CBR files and for VBR and MPEG-2 files (heuristic 0, delay 0 stored). Playback of MPEG-2 and VBR files was normal.
- These runs established transfer and catalogue correctness. They did not test audible gapless timing. Whether the omitted group causes an audible gap between tracks was never compared; MP3s sent by Apple's rule carry the group (section 12.7).

### 12.7 MP3 timing sent by Apple's rule

In these runs the Linux sender computed every MP3's timing and gapless fields by the rule of section 13: `item.total_time_ms` from the frames Apple counts, `bit_rate` as Apple reports it, `duration` as the samples played, and the delay, drain, resynch and heuristic word from an `iTunSMPB` comment, an accepted LAME tag or an `SMPB` frame, else from the encoder text and a decode of the last frames. The decoder was a pure-Rust Layer III decoder, not Apple's. All CONFIRMED on the phone over Wi-Fi.

One insert batch added eleven tracks in one session: three ALAC, four MPEG-1 Layer III files (CBR from ffmpeg, one with a `VBRI` header, one LAME-encoded, and one with a comment and every sort tag), one MPEG-2 Layer III file, and AAC, WAV and AIFF. The phone stored every value sent:

| File | What the phone stored |
| --- | --- |
| LAME-encoded MP3 (accepted LAME tag) | heuristic 1, delay 576, drain 2592, `duration` 352,800 samples (exactly 8 s at 44.1 kHz) |
| ffmpeg-made MP3s and the MPEG-2 file (no accepted tag) | heuristic `0x02000003`, delay 528, drain and resynch as computed |

A further batch added an MP3 whose ID3 encoder text names LAME but which has no LAME tag. It took delay 576, by the encoder rule of section 13.8.

Track R was then deleted (its row still held the earlier insert's heuristic 0 and no trim) and sent again from Linux. The phone now holds what Apple's own sync sent for it: `duration` 11,162,592, delay 528, drain 912, resynch 4,047,097, heuristic `0x02000003` (section 13.2). Played on the phone into the following track, it played through normally. Every test track also played normally.

This settles the practical question: a sender that reproduces Apple's values gets them stored as sent, and the phone plays the result. What the phone does with each value (in particular the heuristic bits and the resynch offset) is still not known from the phone side (section 16).

## 13. MP3 gapless metadata (how Apple Devices computes it)

Source: static analysis of AMP 1.6.4.90 (package 1.1540.23042.0), cross-checked against AMP2 (1.6.4.94, package 1.1540.24088.0), where every function below is instruction-for-instruction identical. Linux `ffmpeg`, `ffprobe` and `lame` were used on Track R and on synthetic sine files to test the reading of the code.

The analysis used an annotated disassembly of every function quoted here, an old-to-new address map with an equality check, and an executable model of the whole rule. None of this material is redistributed. The model reproduces every row of the acceptance test in section 13.2.

### 13.1 Answers: what an implementation has to reproduce

**Where the values come from.** CONFIRMED. The MPEG handler in AMP fills the gapless group for MP3. It writes a "TrackInfo" struct (`+0x54` flags, `+0x58` delay, `+0x5c` drain, `+0x60` duration in samples, `+0x68` resynch). The library merge at `0x140362b58` to `0x140362c76` copies it to the TrackInstance (`+0xe0`, `+0xe4`, `+0xe8`, `+0xd0`, `+0xd8`) that the insert builder reads. The numbers for Track R are reproduced exactly (section 13.2).

**1. Drain.** `drain = total - valid - delay`, where `total = frames * 1152`, `delay` is the encoder rule (answer 3) and `valid` is the duration (answer 2). Which `valid` depends on the source (answer 6).

- *Analysis path* (no accepted tag, for example Track R and every ffmpeg or Lavf file): `valid` comes from a search in the last decoded PCM (`0x14043cbc0`). If the tail has no "hit" (it is quiet or silent, the usual case) the result is the first candidate `c = total - delay - 530`. If the "CD flag" is on (sample rate 44100, or `iTunes_CDDB_*` COMM frames, or encoder text starting `MusicMatch` or `Amazon.com`) then `valid = floor(c / 588) * 588`, else `valid = c`. So drain is `530 + (c mod 588)` at 44.1 kHz and exactly `530` otherwise. For Track R: `c = 11,164,032 - 528 - 530 = 11,162,974`, `valid = 18984 * 588 = 11,162,592`, `drain = 912`. A loud, abruptly cut tail can make Apple pick a smaller multiple of 588 (up to 3 steps earlier); that needs a real decode (section 13.4).
- *LAME tag accepted*: drain is the tag padding, plus 1152 whenever `frames * 1152 - padding - delay` is not a multiple of 588 (almost always); section 13.4.4.
- *ID3 `iTunSMPB`*: drain is field 3 of the text (hex).

**2. Duration.** `avformat_info.duration` is a sample count equal to the same `valid` (Track R 11,162,592). `item.total_time_ms = floor(frames * 1152 * 1000 / sample_rate)`, independent of delay and drain (Track R 253,152). `frames` counts the Info frame for a CBR `Info` file (the count is `round(bytes / average_frame_size)`, where `bytes` covers the whole audio including the Info frame). It does NOT count the Xing or VBRI frame when a `Xing` or `VBRI` header gives the frame count. If the byte estimate is not consistent (VBR without header), every frame is counted.

**3. Delay.** Read from the ID3v2 tag text, not from the first frame: the last of `TENC` / `TEN` and `TSSE` / `TSS` text frames, or else the description (or text) of the first `COMM` frame, as a string of at most 255 bytes. Rule (first match wins): contains the app's localized name or `Apple`: 528; starts `MusicMatch`: 726 (plus CD flag); starts `Fraunhofer`: stored VBRI/B4 delay if non-zero, else 726; starts `Amazon.com`: 576 (plus CD flag); starts `LAME`: 576; anything else, or no ID3 tag: 528. Track R: `TSSE = "Lavf58.76.100"` gives 528. The delay is only used on the analysis path; a tag that is accepted brings its own delay (answer 6). Layout detail for Track R: the LAME-tag text `Lavf` sits at frame offset 156, and the tag's 576 / 0 are ignored because the string is not `LAME`, `iTunes` or `Apple`.

**4. Resynch.** Byte offset, measured from the first MPEG frame (the Info or Xing frame is offset 0, that is, right after the ID3v2 tag), of the frame that is 7 frames before the last frame (the 8th from the end). Track R: frame 9683 of 0 to 9690 starts at 4,047,097. If fewer than 8 frames are decoded it is 0. With `iTunSMPB` it is instead the 6th field of that text (usually 0).

**5. Heuristic word** (`TrackInfo+0x54`, sent as `gapless_heuristic_info`):

| Source of the gapless values | Word | Other effects |
| --- | --- | --- |
| Decoder analysis (no accepted tag): Track R, plain CBR or VBR, Lavf | `0x02000003` (bit 0 + bit 1 + byte 3 = 2) | delay by encoder rule, drain and duration from the tail search, resynch from the ring |
| Accepted LAME tag, `SMPB` frame marker, B4 ancillary marker, or `iTunSMPB` with field 5 bit 1 clear | `0x00000001` | tag values kept; resynch still added from the ring (if the tail pass runs) |
| `iTunSMPB` with field 5 bit 1 set | `field5 \| 1` first, then the analysis path overwrites everything and the word becomes `(field5 & 0xffffff) \| 3 \| 0x02000000` | |
| No gapless info at all (bit 0 clear) | group omitted (heuristic 0) | |

Bit 0 means "gapless info valid" (the builder emits the group only if bit 0 is set, `0x140463c6d`). Bit 1 and the byte value 2 are written only by the decoder-analysis path. INFERRED meaning: "resynch is real, derived by analysis". The one reader in this binary, the merge at `0x140362c2c`, keeps the library's resynch for MP3 when bit 1 is clear and the new resynch is 0. The iOS meaning of the bits is not in this binary.

**6. When omitted, and tags.** The group is omitted only when no source produced gapless info (TrackInfo bit 27 clear). The analysis path always produces it for a decodable MP3, so in practice it is always sent. Delay and drain are taken from tags, in this priority: (1) ID3 COMM `iTunSMPB` (text of 115 or 116 chars, hex fields: delay, padding, 64-bit sample count, flags, resynch); (2) Xing/Info LAME tag, only when the encoder string is exactly `LAME...` and delay, padding and sample count are all non-zero; (3) an Apple `SMPB` frame marker; (4) a CRC-8 protected `0xB4` marker in the first decoded frame (Fraunhofer / iTunes style). Details and conditions are in section 13.10. Track R matches none of them (`Lavf`, padding 0).

**7. VBRI, Xing, Info.** An earlier claim "no Xing/VBRI/Info signature compare" was wrong. They are compared byte by byte (`0x14041cf86` `cmp al,0x58` ... `0x14041cf9c` `cmp [rbx+3],0x67`), which is why searches for the dword immediates found nothing. All three are parsed in `0x14041cf50`; none is skipped. `Xing` and `VBRI` count as a "VBR header" (frame count taken from the header, average bit rate computed, the next frame used for the format). `Info` is parsed (bytes field, LAME tag) but returns "not a VBR header": its frame field is ignored and the frame stays in the byte-based frame count. VBRI's delay field is read (section 13.8).

**8. Bit rate and Sound Check.** `bit_rate`: with a `Xing` or `VBRI` header it is the average `floor((end - start + 1) * 8 / duration_ms)` in kb/s; otherwise the bit rate in the header of the first frame (a VBR file without Xing or VBRI reports the first frame's rate, not an average). Track R: 128. `volume_normalization_energy` is read only from an ID3 COMM `iTunNORM` (formula in section 13.12). Track R has none, yet Apple sent 1792, so that value does not come from this binary (INFERRED: the Apple Music library analysis). A sender does not need it: 42 captured ALAC inserts omit it and the phone accepted them.

### 13.2 Acceptance test: Track R

Inputs read from the file: ID3v2.4 tag of 54,664 bytes (TIT2, TPE1, TALB, empty COMM, `TSSE = Lavf58.76.100`, APIC); 9,691 MPEG-1 Layer III frames, 128 kb/s, 44.1 kHz, joint stereo; frame 0 is an `Info` frame (flags `0x0f`, frames field 9690, bytes 4,050,441, LAME-tag text `Lavf`); file size 4,105,105; no ID3v1. Reference traffic: the captured `insert_track` for this file.

| Field | Rule | Computed | Captured | |
| --- | --- | --- | --- | --- |
| frames | `round(4,050,442 / (128 * 144000 / 44100))`; remainder +0.0011 (within 0.05) | 9691 | n/a | |
| total samples | `9691 * 1152` | 11,164,032 | n/a | |
| `item.total_time_ms` | `11,164,032 * 1000 / 44100`, floor | 253,152 | 253152 | exact |
| `gapless_encoding_delay` | `TSSE` is `Lavf...`, so default | 528 | 528 | exact |
| CD flag | sample rate 44100 | on | n/a | |
| valid / `duration` | `floor((11,164,032 - 528 - 530) / 588) * 588` = 18984 * 588 | 11,162,592 | 11162592 | exact |
| `gapless_encoding_drain` | `11,164,032 - 11,162,592 - 528` | 912 | 912 | exact |
| `gapless_last_frame_resynch` | start of frame 9683 minus 54,664 | 4,047,097 | 4047097 | exact |
| `gapless_heuristic_info` | analysis path | `0x02000003` | 33554435 | exact |
| `bit_rate`, `channels`, `sample_rate` | frame header | 128, 2, 44100 | same | exact |
| `volume_normalization_energy` | not derivable | n/a | 1792 | NOT reproduced |

For contrast, the tag's own declaration would give a different length. The `Info` frame declares 9,690 audio frames and the `Lavf` text declares delay 576 and padding 0, so the declared valid length is 9,690 * 1152 - 576 = 11,162,304 samples. Apple's wire value is 11,162,592, the decoder-analysis result, and the physical decode in ffmpeg gives yet another count. A sender that copies the tag's declaration does not reproduce Apple's numbers.

The tail of Track R is silent: in the ffmpeg float decode every sample in the last 54,583 sample frames is below 3e-4, so every scanned window is below the T2 threshold and the thresholds never trigger; the first-candidate fallback is used. A transcription of the tail search (`0x14043cbc0`) on that tail returns 11,162,592 with the CD flag on and 11,162,974 with it off. Only the first matches the capture, which supports the 588 rule. This is the only MP3 with Apple traffic. The LAME-tag, `iTunSMPB`, `SMPB`, B4, VBRI, Xing and mono paths are code-confirmed but not traffic-confirmed.

### 13.3 How the pieces fit

```
file --ID3v2 callbacks--> encoder text, CDDB flag, iTunSMPB/iTunNORM          0x140436bc8, 0x14043686c, 0x1403be164
     --analyzer 0x140437344--> frame count F, duration ms, bit rate, tag-based gapless   (TrackInfo +0x54..+0x68)
     --OPEN 0x14043ce9c--> delay rule, CD flag, tail decode pass --> 0x14043cbc0 valid, ring --> resynch
                          writes +0x58 +0x5c +0x60 +0x68 +0x54 unless a tag already did (flags & 3 == 1)
     --merge 0x140362b58--> TrackInstance +0xe4 +0xe8 +0xd0 +0xd8 +0xe0   (+0x64 total_time from ITTrackInfo+0x1014)
     --insert builder 0x140461900 (0x140463c6d)--> avformat_info group, only if bit 0 of +0xe0
```

- AMP contains a complete Layer III decoder (`0x14041c700` frame decode, `0x14041ca18` table init, bit reservoir limit `0x700`). The message handler `0x14043e6xx` dispatches fourCC messages; message `0x65736d70` ('esmp') calls OPEN; another calls `0x14043ca60`, then `0x140439004`, then the analyzer.
- `0x140439004(kind, stream, ITTrackInfo, TrackInfo, ...)` calls the analyzer with the library TrackInfo and then maps MPEG version and layer to `audio_format` (`0x12d` = 301 for MPEG-1 L3, `0x12e` and `0x12f` for MPEG-2 and 2.5, `0x65` to `0xc9` for L1 and L2). It also stores bit rate in `[ITTrackInfo+0x1028]`, sample rate in `[+0x1454]`, channels, and duration ms in `[ITTrackInfo+0x1014]` (the source of `item.total_time_ms`).
- INFERRED: the order in which the library calls GetInfo and OPEN (OPEN reads the TrackInfo `+0x58` and `+0x54` that GetInfo left, `0x14043d3bd`, `0x14043da38`). Not traced up to the library caller.

### 13.4 Drain: the code

#### 13.4.1 Where it is written (OPEN tail, `0x14043d97a`)

```
0x14043d9f2  lea r9d,[rdi+0x211]          ; start offset = delay + 529 (decoder delay)
0x14043da23  call 0x14043cbc0             ; tail search: [rbp-0x78] = valid samples
0x14043da38  mov r8d,[rcx+0x54]; and al,r14b(=3); cmp al,1 ; if tag-provided gapless: keep, add resynch only
0x14043da86  bts [rcx],27                 ; gapless valid
0x14043da8f  mov [rax+0x58],edi           ; delay  (encoder rule)
0x14043da9b  mov [rax+0x5c],esi           ; esi = total - valid - delay   (esi = [rbp-0x60] total samples)
0x14043daa5  mov [rax+0x60],rcx           ; duration = valid
0x14043dab4  mov [rax+0x68],rcx           ; resynch = ring slot 0   ([rbp+0x17a0])
0x14043dabe  or  [rax+0x54],ecx (1, then 2); mov byte [rax+0x57],2     ; => 0x02000003
```

`total` (`[rbp-0x60]`) is `frames * [decoder+0x30]` (samples per frame, 1152): `imul eax,[rcx+0x30]` at `0x14043d31d`, where `frames` is the analyzer's frame count fetched from a cache list (`0x140437300`), see section 13.6.

#### 13.4.2 How OPEN gets the PCM

At `0x14043d559` to `0x14043d62f`, OPEN computes the frame duration in ms (`ceil(1152 * 1000 / rate)` = 27) and a tail window of `20 frames * 27 ms * 40 bytes/ms = 21,600` bytes (40 bytes/ms is 320 kb/s). It seeks to `audio_end - window`, resynchronises (`0x140436170`, up to `0x10000` bytes) and decodes frame by frame to the end (`0x14043ad08`, one frame per call), appending samples to a FIFO of 4 frames (`[rbp-0x78] = 4 * 1152 * channels` floats). If the window was too small or no resync point was recorded, it first decodes the whole file in "count only" mode and retries with 100 frames (`0x14043d7ce` to `0x14043d8b6`). The last `n = min(4608, decoded)` sample frames go to the tail search.

#### 13.4.3 The tail search `0x14043cbc0` (valid = result)

Arguments: float PCM, `n`, channels, `startOff = delay + 529`, `total`, `spf`, CD flag, `&valid`. `r = total - startOff` (`0x14043cbed`). Pseudocode (stereo):

```
T1 = 0.5, T2 = 0.0003                     (floats at 0x140a0df5c, 0x140a0dea8)
loop passes:
  prev = last sample frame (+1e-38); k = 1
  for i = n-1 down to 0: k += 1
     c = r - k + 1                          ; candidate valid length
     if CD flag and c % 588 != 0: continue  ; magic 0xdee95c4d, exact divide by 588
     if no candidate yet: fallback = c      ; ebx
     for each channel: rel = (prev - x[i]) / prev ; hit if |prev| > T2 and |rel| > T1
     if hit: first hit remembers (c, |rel|); a second hit returns the first c if its |rel| is
             larger, else the second c
     prev = x[i] + 1e-38
     stereo: stop when k >= 2*1152 ; mono: scan all n
  after the scan: if one hit -> return it
  T1 -= 0.1 ; T2 -= 2e-5 ; stop (return fallback) when T1 <= 0 or T2 <= 0   (five passes)
```

The return value `-0x28` means nothing hit; OPEN treats it as success (`0x14043da2a`: `cmp eax,0xffffffd8 ; cmovne ebx,eax` leaves `ebx = 0`).

Meaning: scanning backwards from the end, the first place where a sample above about -70 dB follows a sample that differs from it by more than 50 percent (relaxed down to about 10 percent) is taken as the end of the audible signal. With a quiet or silent tail nothing hits and `valid` is the first candidate. With a loud tail the result is one of at most four multiples of 588 (44.1 kHz) within 2304 frames of the end. The PCM scale is not known (INFERRED +-1.0 from T2 = 3e-4). Apple's decoder is not bit identical to a Linux decoder, so borderline cases may differ by one 588 step. Using the no-hit rule for every file is a good approximation (an error of at most 3 * 588 samples = 40 ms on abruptly ending tracks). Both builds contain identical constants.

#### 13.4.4 Other sources of the drain

- **LAME tag** (`0x14041d245` to `0x14041d308`): `delay = b0<<4 | b1>>4` and `padding = (b1&15)<<8 | b2` from the 3 bytes at LAME-tag text + 21. Both are zeroed if `delay >= 2000`, `padding >= 4000`, or the frame field is 0. `valid = frames * 1152 - padding - delay`. If `valid % 588 != 0` (magic `0xbdd2b899406f74af`, exact divide) then `padding += 1152` and `valid` is recomputed. This removes the Info frame that the byte-based count includes; for `Xing` files it removes a real frame, a quirk (section 13.7). Accepted at `0x140437c49` only when the encoder string starts `LAME`, delay, padding and valid are all non-zero, and nothing was found earlier.
- **iTunSMPB**: drain is field 3.
- **`SMPB` frame**: drain = `frames * 1152 - samples - delay` (`0x140438316`, done whenever bit 0 is set and drain is 0).
- **B4 marker**: same derivation.

### 13.5 Resynch

OPEN keeps a ring of 8 qwords (`[rbp+0x17a0..0x17d8]`). After each successful frame decode (`0x14043d73e`) it shifts the ring down (`0x14043d780`; the helper `0x1406c3724` takes src, dst, n) and stores `position - start` at `+0x17d8` (`0x14043d7ae`), where `position` (`[rbp+0x48]`) is the file position before that decode and `start` the first frame. After the last frame the oldest slot `+0x17a0` holds the value stored 7 decodes earlier, which is the start of the frame 7 before the last one. With M frames in the tail window: `resynch = start_of_frame[M-8]`; it is 0 if `M < 8`; at the first decode the stored value is the window start, not a frame boundary (matters only for tiny files). It is written at `0x14043dab4` (analysis path) or `0x14043da65` (only when the tag path left it 0). Track R: frame 9683 begins 4,047,097 bytes after the Info frame. For `iTunSMPB` it is field 6 (`0x1403be573`: `[rbx+0x68] = [rbp-0x7c]`, the zeroed parse-struct field at `+0x24`).

### 13.6 Frame count and durations

#### 13.6.1 Frame count F (analyzer `0x140437344`)

1. `0x14041d99c(frameinfo, bytes, &rem)`: `x = bytes / (kbps * 144000 / rate)`, `F0 = int(x + 0.5)`, `rem = F0 - x` (floats at `0x140a0e820`: 144000.0 for MPEG-1, 72000.0 for MPEG-2 and 2.5). `bytes = end - start + 1` (`0x140437886`), with `start` the first frame (after the ID3v2 tag; `0x14043648c` finds two consecutive consistent frames and rewinds to the first) and `end` the end of audio (ID3v1 and similar are excluded by the ID3 parser, INFERRED, plus a retry that subtracts 128-byte steps up to `0x400` at `0x1404378e9`). Track R: `x = 9690.9989`, `F0 = 9691`.
2. If `0x14041cf50` returned true (`Xing` or `VBRI`) and its frame field is non-zero and differs from `F0`, then `F = header frames` (`0x140437ae8` to `0x140438054`).
3. Else if `|rem| <= 0.05` and `F0 != 0`: `F = F0` (`0x140438060`).
4. Else every frame is counted by walking the file (`0x140438430` to `0x14043850f`, `0x1404370f4` per frame).

For `Info` files `F0` counts the Info frame because the Info frame is part of `bytes`. For `Xing` files the header frame count (which by convention excludes the Xing frame) is used.

#### 13.6.2 Durations

- `item.total_time_ms = F * spf * 1000 / rate` (integer divide, `0x14043832d` to `0x140438339`, stored via `0x1404386b7` to `ITTrackInfo+0x1014`, merged to `TrackInstance+0x64`, builder `item.total_time_ms`). CONFIRMED by the merge at `0x1403625fd` to `0x14036260e`.
- `avformat_info.duration = TrackInfo+0x60 = valid` (a sample count; `[TrackInstance+0xd0]`). The Info or Xing frame is counted in `total` exactly as in `F`.

### 13.7 A quirk to be aware of (INFERRED intent, CONFIRMED code)

The "multiple of 588" correction in the LAME branch runs for every sample rate and every length. A normal LAME VBR file (Xing frame counts already exclude the Xing frame) whose valid length is not a CD multiple gets `drain + 1152` and a duration one frame shorter. This path has no real-traffic confirmation (the only captured MP3 has no LAME tag). Synthetic files made by `lame` with 10 s of 44.1 kHz sine are multiples of 588 and are not affected (see section 13.13).

### 13.8 Delay: the encoder rule (OPEN, `0x14043d34f` to `0x14043d42f`)

Text source: `0x140305fdc` walks the ID3v2 frames and calls the callback `0x140436bc8` with a Pascal string buffer `[rbp+0x17e0]` (length byte, then up to 255 bytes; `0x140304dd8` decodes Latin-1, UTF-16 with or without BOM, and UTF-8):

- Frame ids `TENC`, `TEN`, `TEN ` and `TSSE`, `TSS`, `TSS `: the text is stored, always replacing the buffer (a later one wins; `0x140436d68` to `0x140436d7e`).
- `COM`, `COM `, `COMM`: first the description is tested against the three CDDB descriptions `iTunes_CDDB_1`, `iTunes_CDDB_IDs`, `iTunes_CDDB_TrackNumber` (Pascal strings at `0x14099ef08`, `0x14099f080`, `0x14099f0a0`); a match sets the CD flag (`0x140436c62` to `0x140436ca2`). Then, only if the buffer is still empty, the description is stored, or if the description is empty the comment text (`0x140305968`, `0x140305bb8`); a text starting `MusicMatch` or `Amazon.com` sets the CD flag and length 10 (`0x140436d05` to `0x140436d4b`).
- `TLEN`: decimal ms parsed to `[rbp+0x38]`; not used afterwards.

Comparison on `buffer+1` (`0x14043d348` to `0x14043d42b`); order matters:

| Test | Delay | CD flag |
| --- | --- | --- |
| `strstr(text, localized app name)` (`SharedUtils::GetLocalizedAppName`, `0x1406e5108`) | 528 | no |
| `strstr(text, "Apple")` | 528 | no |
| `strncmp(text, "MusicMatch", 10) == 0` | 726 (`0x2d6`) | yes |
| `strncmp(text, "Fraunhofer", 10) == 0` | `TrackInfo+0x58` if non-zero, else 726 | no |
| `strncmp(text, "Amazon.com", 10) == 0` | 576 (`0x240`) | yes |
| `strncmp(text, "LAME", 4) == 0` | 576 | no |
| anything else, empty, no ID3 tag | 528 (`0x210`) | no |

(`neg eax; sbb ecx,ecx; and ecx,-0x30; add ecx,0x240` at `0x14043d415` to `0x14043d421` is the LAME-or-other choice.) After the table: if the CD flag is still 0, `flag = (sample rate == 44100)` (`0x14043d32d`). `startOff = delay + 529`.

The "stored value" for Fraunhofer is `TrackInfo+0x58` as left by the analyzer: from a `VBRI` header the delay field minus 1152 (576 for MPEG-2) minus 529 when larger than that sum (`0x14041d401` to `0x14041d433`), copied at `0x140437c9d` only if the string is `Fraunhofer`. Because OPEN decides on the ID3 text, a VBRI file without a `Fraunhofer` ID3 text gets 528 (INFERRED consequence; could not be tested).

Separately, the analyzer reads a **second** encoder string from the Xing or Info frame itself: the 20 bytes at the "LAME tag" position, which is frame offset `4 + side_info (32 stereo, 17 mono, +2 if CRC) + 8 + 4 (frames) + 4 (bytes) + 100 (TOC) + 4 (quality)` = 156 for Track R. It is kept only when it starts `LAME`, `iTunes` or `Apple` (also `LAME` at +4), otherwise cleared (`0x14041d230`). It is used only for the tag-acceptance test of section 13.4.4, not for the delay.

### 13.9 Heuristic word: setter sites

Written fields of `TrackInfo+0x54` (dword), all CONFIRMED:

| Site | Effect |
| --- | --- |
| `0x140437c66`, `0x140437dcd`, `0x140437fe6` (LAME tag, `SMPB` frame, B4 marker) | `(old & ~2) \| 1` |
| `0x1403be57d` to `0x1403be58a` (iTunSMPB) | `field5 & 2 ? field5 \| 1 : (old & ~2) \| 1` |
| `0x14043da4a` to `0x14043da52` (OPEN, tag already gave `flags & 3 == 1`) | `\|= 1; &= ~2` |
| `0x14043dabe` to `0x14043dacc` (OPEN analysis) | `\|= 1; \|= 2; byte[+0x57] = 2` |

Conditions: a LAME file with a valid tag gets `1`; an iTunes-style file with `iTunSMPB` gets `1` (normally); files without accepted tags (including every `Lavf` / ffmpeg file, CBR or VBR, with or without Info or Xing) get `0x02000003`. Whether the encoder is known matters only through the tag test. CBR versus VBR matters only through the frame count and bit rate. No other bits are produced by AMP. The meaning of bit 25 (byte 3 = 2) beyond "set by analysis only" is UNKNOWN (iOS side).

### 13.10 Sources of gapless info and their conditions (analyzer `0x140437344`)

Order of evaluation and gating (CONFIRMED):

1. **ID3 COMM `iTunSMPB`** (callback `0x14043686c` to `0x1403be164`, SMPB at `0x1403be526`). Needs a TrackInfo (the library call has one; OPEN's call does not). The text must be 115 or 116 characters (`0x1403bdb2a`: `lea eax,[rbx-0x73]; cmp eax,1`), with fields of the form ` 00000000 00000840 000001A4 0000000000A5D5CC 00000000 00000000 ...`: field 2 hex = delay (`+0x58`), field 3 = padding (`+0x5c`), field 4 (16 hex digits) = samples (`+0x60`), field 5 = flags word (bit 1 kept), field 6 = resynch (`+0x68`). Sets bit 27 / bit 0. Descriptor test `0x140305a8c` (`iTun` + `SMPB` or `NORM` + NUL). The frame count is then `(delay + padding + samples) / 1152` (`0x14043776a` to `0x140437794`) and later checks skip the other sources.
2. **Xing/Info LAME tag** (section 13.4.4). Accepted when the encoder string is `LAME`, no gapless info exists yet, and delay, padding and samples are all non-zero. The `Info` frame's frame count is the byte estimate.
3. **`SMPB` frame** (`0x140437024`): the bytes at `frame_body[side_info]` are `'S','M','P','B'` (where `Xing` would sit), then 4 unknown bytes, a big-endian 32-bit delay and a big-endian 64-bit sample count. Checked on the second frame when the first one is Xing, VBRI, LAME or flag-31, else on the first frame (`0x140437d9e`).
4. **B4 marker** (`0x14041dad0`, called from the decoder at `0x14041c7de` only for the first 3 decoded frames, body longer than 9 bytes): 9 bytes starting `0xB4` plus a CRC-8 (table at `0x14041dad0`, init 0xFF) over them; bytes 1 to 2 = `delay + 528` (accepted if `>= 528` and `<= 528 + 0x1df0`), bytes 3 to 6 = sample count (must be at most 1.2 times `F * 1152`). Then delay = field - 528, drain derived, encoder string set to `Fraunhofer` (`0x140437f47` to `0x140437ffc`). Provenance of the marker: INFERRED Fraunhofer / iTunes.
5. **Sanity** (`0x140438073` to `0x1404380c0`): if the final frame count equals the CBR estimate and the tag-derived frame count differs from it by more than a factor 1.2 (`|a-b|/min(a,b) > 1.2`, constant `0x140a0e140`), gapless is dropped (`btr [rdi],27`).
6. Otherwise OPEN's analysis fills it. If OPEN is not run or the tail search fails, the group is omitted for that track (bit 0 stays clear). Not seen in any capture.

Not found: any use of `elst`-like data for MP3, `iTunPGAP` (read at `0x1403be592` as a gapless-playback flag into ITTrackInfo, unrelated to the group), or the LAME `ReplayGain` fields.

### 13.11 Xing, Info and VBRI parsing

`0x14041cf50(body, S)`:

- Position: `body + [frameinfo+0x38]` (side-info size; `0x14041653c` computes 32 or 17 (+2 CRC) for MPEG-1 and 17 or 9 for MPEG-2), then, if nothing is found, again at fixed offset `0x20` (for `VBRI`, `0x14041d54f`).
- `Xing`: flags BE32 at +4. Bit 0 frames (stored to `S+0x10` only for `Xing`), bit 1 bytes (`S+0x14` for both), bit 2 TOC (skipped, 100 bytes; a callback can read it), bit 3 quality dword (a check at `0x14041d077` treats it as absent if the first 3 TOC bytes are 0), bit 8 an Apple variant (`S+0x31 = 1`, TOC skipped). Then the LAME-tag text (20 bytes) as in section 13.8.
- `Info`: identical parse with `r15b = 0`, so the frames field is ignored and the function returns false.
- `VBRI`: fields at body + 6: delay (BE16), quality, bytes (BE32), frames (BE32), TOC geometry. Sets `S.frames`, `S.bytes`, encoder text `Fraunhofer`, delay as in section 13.8; returns true.
- Returned true (`Xing` or `VBRI`) means: the analyzer takes the frame count from the header, sets `p13 |= 1` ("VBR header present" to the caller), computes the bit rate from the average, and reads the following frame and uses it for the format (`0x140437b6e` to `0x140437bef`) and for the `SMPB` and B4 checks.
- `0x14041da7c` (used by the frame finder and a seek routine) tests only `Xing` and `VBRI`, not `Info`.

None of the three frames is "skipped as non-audio" by the frame counter; the header's frame-count convention decides whether it is in `F` (section 13.6).

### 13.12 Bit rate and Sound Check

- `ITTrackInfo+0x1028` is the frame info bit rate `[frameinfo+0x10]` (`0x140439263`). The analyzer overwrites `[frameinfo+0x10]` at `0x140438360` with `(end - start + 1) * 8 / duration_ms` when a `Xing` or `VBRI` header is present (and `S+0x31` is 0), where `duration_ms = F * 1152 * 1000 / rate`. For `S+0x31 = 1` (the variant) the second frame's rate is used (`0x140437bd4`).
- `iTunNORM` (COMM descriptor, text of 89 or 90 chars = 10 hex words, `0x1403bd92c`) is converted by `0x1403be0b0`: `g = max(w0, w1)`, `p = max(w6, w7)`; if `sqrt(1000/g) * p >= 29200` then `g = 1000 * (p/29200)^2`; if `g <= 0` then `g = 1000`; clamp to `[1, 65534]` (constants `0x140a0e544` = 1000, `0x140a0e560` = 29200, `0x140a0e574` = 65534, `0x140a0e578` = 65535). The result is `TrackInfo+0x18` (WORD), flag bit 1, merged to `TrackInstance+0x5a` (`0x140362b84`). The `sqrt` is INFERRED from the call target `0x14080532b`.
- Nothing in the MPEG handler produces an energy without an `iTunNORM` tag. Track R has none (its COMM frame is 5 bytes, empty). The captured 1792 is therefore from outside this binary's file analysis. A sender does not need it (INFERRED from 42 energy-less ALAC inserts in the same capture that the phone accepted).

### 13.13 Predictions for other file kinds (UNVERIFIED)

These come from the executable model, not from Apple traffic. They have not been checked against any Apple sync of such a file, and the phone has not been shown to expect them.

Synthetic 10 s, 44.1 kHz stereo sine made with `lame 3.100`:

| File | Model result |
| --- | --- |
| `lame -b 128` (CBR, Info + LAME tag 576/792) | heuristic 1, delay 576, drain 1944, duration 441,000, total_time 10,057 ms |
| `lame -V2` (VBR, Xing + LAME tag 576/792) | heuristic 1, delay 576, drain 792, duration 441,000, total_time 10,031 ms, bit rate 51 |
| `lame -t -b 128` (no tag) | heuristic `0x02000003`, delay 528, drain 840, duration 441,000 |

These use the "multiples of 588" coincidence of 10 s at 44.1 kHz; a real LAME file generally gets the +1152 correction (section 13.7).

### 13.14 Corrections to the earlier format analysis

An earlier static pass over the format paths (the source of sections 3 to 11) made the statements below. The MP3 gapless study corrects them. The corrected versions are what sections 3, 8 and 13 give.

1. "AMP does not parse Xing / Info / VBRI headers": wrong. They are compared byte by byte.
2. "Encoder ID text in the first frame": the delay rule reads the ID3v2 text frames. The Xing or Info frame contributes a second, LAME-only string used for tag acceptance.
3. "Drain could not be derived": it is derived (section 13.4). "Resynch looked like frame number last-8": it is the frame 7 before the last (the 8th from the end), from a ring of 8 positions; reproduced.
4. "Heuristic bits not recovered": the setters are recovered (section 13.9). The meaning of the bits is still iOS-side.
5. "The delay is not read from the LAME tag": it is, when the tag says `LAME` and the padding is non-zero. Track R's tag says `Lavf`.
6. "The Info frame IS counted as audio in total_time": true only for `Info` (byte estimate), not for `Xing` and `VBRI`.
7. "`iTunSMPB` handling only for MP4": the ID3 COMM `iTunSMPB` is parsed for MP3 too (115 or 116 chars; MP4 accepts 116).
8. "AMP does not analyse the file for MP3": it does, with its own decoder.

### 13.15 Evidence index (both builds)

Old = AMP 1.6.4.90 (package 1.1540.23042.0); new = AMP2 1.6.4.94 (package 1.1540.24088.0). Every function below was located in the new build by its first eight instructions and then compared instruction by instruction with addresses, displacements and branch targets removed. All matched with identical sequences and instruction counts, so an offset inside a function shifts by the same amount as its start. The tail-search float constants (0.5, 0.0003, -0.1, -2e-5, 1e-38, 0.0) have the same values in both builds at the addresses shown at the end of the table.

| What | Old VA | New VA |
| --- | --- | --- |
| Xing / Info / VBRI / LAME-tag parser | `0x14041cf50` (signatures `0x14041cf86`, `0x14041cfa6`, `0x14041d32d`; LAME `0x14041d174`; delay and padding `0x14041d266` to `0x14041d308`) | `0x14041d074` |
| CBR frame estimate | `0x14041d99c` | `0x14041dac0` |
| Xing/VBRI presence | `0x14041da7c` | `0x14041dba0` |
| B4 CRC marker | `0x14041dad0` (call `0x14041c7de`) | `0x14041dbf4` |
| `SMPB` frame marker | `0x140437024` | `0x140437148` |
| Analyzer (frames, bit rate, gapless sources) | `0x140437344` | `0x140437468` |
| Gapless set sites in the analyzer | `0x140437c49`, `0x140437dac`, `0x140437f47`, `0x1404380c0` (btr) | old + `0x124` |
| ID3 callbacks | `0x140436bc8` (OPEN), `0x14043686c` (analyzer) | `0x140436cec`, `0x140436990` |
| Find first frame / sync | `0x14043648c` / `0x140436170` | `0x1404365b0` / `0x140436294` |
| Frame walker (resync and count) | `0x1404370f4` | `0x140437218` |
| ID3 callback 3 (clean-file writer) | `0x140436df0` | `0x140436f14` |
| GetInfo wrapper / message | `0x140439004` / `0x14043ca60` | `0x140439128` / `0x14043cb84` |
| Tail search | `0x14043cbc0` | `0x14043cce4` |
| OPEN (delay rule `0x14043d34f` to `0x14043d42f`, tail pass `0x14043d559` to `0x14043d7ce`) | `0x14043ce9c` | `0x14043cfc0` |
| OPEN tail writes | `0x14043d97a` to `0x14043dad2` | `0x14043da9e` |
| Frame-count cache | `0x140437300` | `0x140437424` |
| iTunSMPB text parser | `0x1403bdaf4` | `0x1403bdaf0` |
| iTunNORM text / energy | `0x1403bd92c` / `0x1403be0b0` | `0x1403bd928` / `0x1403be0ac` |
| ID3 COMM importer | `0x1403be164` (SMPB `0x1403be526`, NORM `0x1403be4e3`) | `0x1403be160` |
| COMM descriptor tests | `0x140305a8c` (iTun), `0x140305b18` (CDDB) | `0x140305a48`, `0x140305ad4` |
| ID3 text decode | `0x140304dd8` | `0x140304d94` |
| Merge TrackInfo to TrackInstance | `0x140362b58` to `0x140362c76` | `0x140362b50` |
| Builder gapless group / SQL | `0x140463c6d` / string at file offset `0x9d52b0` | same function layout |
| Layer III decoder entry / frame decode | `0x14041c8f0` / `0x14041c700` | `0x14041ca14` / `0x14041c824` |
| Frame header / info decode | `0x140416838` / `0x14041653c` | `0x14041695c` / `0x140416660` |
| Constants: T1 0.5, T2 3e-4, steps -0.1 and -2e-5, 1e-38 | `0x140a0df5c`, `0x140a0dea8`, `0x140a0e5dc`, `0x140a0e5d8`, `0x140a0de90` | `0x140a0ef5c`, `0x140a0eea8`, `0x140a0f5dc`, `0x140a0f5d8`, `0x140a0ee90` |

Shifts by region, old to new: `0x14041xxxx` to `0x14043xxxx` +0x124; `0x1403bdxxx` to `0x1403bfxxx` -0x4 (the capability code at `0x1403af144` to `0x1403b46dc` is unshifted); `0x1403053xx` -0x44; merge `0x140362b58` -0x8. Spot checks of single instructions, old to new: flags test `0x14043da38` to `0x14043db5c`; `mov BYTE [rax+0x57],2` `0x14043dacc` to `0x14043dbf0`; mod-588 magic `0x14043cc84` to `0x14043cda8`; delay + 0x211 `0x14043d9f2` to `0x14043db16`; LAME magic `0xbdd2b899406f74af` `0x14041d2b5` to `0x14041d3d9`; `cmp eax,0x534d5042` ('SMPB') `0x1403be526` to `0x1403be522`; `bt rax,0x1b` `0x140362bb0` to `0x140362ba8`. The addresses of the format-path functions of sections 3 to 11 are in section 17.2.

### 13.16 Dead ends and unresolved items

- The origin of `volume_normalization_energy = 1792` for Track R. The MPEG handler, the merge, constants such as `0x700` and the energy parser were searched; nothing computes an energy from audio.
- Real Apple traffic exists for one MP3 only (section 13.2).
- Apple's decoder output scale and bit exactness (matters only for loud, abruptly ending tails).
- Whether the library calls the OPEN pass for every import or only for tracks played or transcoded. The captured numbers show it ran for Track R. INFERRED: it always runs, since the group is expected for every MP3.
- `0x140309768` (ID3v2 header parser): the end-of-audio field `[rbp+0x8528]` (ID3v1 and APE exclusion) was not read.
- The iOS meaning of the heuristic bits, and what the phone does with the resynch value.
- MPEG-2 and 2.5 (302, 303) use the same code with `spf = 576`; not exercised.
- Fraunhofer B4 marker provenance, the Apple `SMPB` frame writer, and the Xing flag-8 variant: structure only.

### 13.17 Notes for implementers

The rule for files without an accepted tag (the analysis path that Track R and every ffmpeg-made file take):

```
F      = xing_or_vbri_frames if header present and != estimate
         else round(audio_bytes/(kbps*144000/rate)) if |remainder| <= 0.05 else count_all_frames
total  = F * 1152 ; delay = encoder_rule(ID3 TENC/TSSE/COMM text)
cd     = rate == 44100 or ID3 COMM desc in {iTunes_CDDB_1, iTunes_CDDB_IDs, iTunes_CDDB_TrackNumber}
         or text starts MusicMatch/Amazon.com
c      = total - delay - 530 ; valid = cd ? c - c % 588 : c         (no-hit rule)
drain  = total - valid - delay ; duration = valid
resynch= offset of frame (last-7) from the first frame (0 if fewer than 8 frames)
heuristic = 0x02000003 ; total_time_ms = total * 1000 / rate ; bit_rate = Xing/VBRI ? avg : header
```

With tags, apply the priority list of section 13.10 first (`iTunSMPB`, then an accepted LAME tag with the 588 correction) and use heuristic `1`.

Advice the evidence supports:

- Compute `duration` and `item.total_time_ms` from a complete frame count. Both must be positive; the phone stored zero when they were omitted (section 12.6).
- Send `channels` for MP3 and omit it for ALAC.
- For MP3 files with a Xing or VBRI header, remember that the frame count in the header excludes that frame, whereas an `Info` file's byte-based count includes it.
- Keep the tail-search approximation (no-hit rule) unless a loud, abruptly cut tail matters; it costs at most 3 * 588 samples (about 40 ms).
- Do not copy another file's delay, drain or resynch; derive them per file.
- The host's native source transfer is limited to 64 MiB per file. A larger MP3 can be analysed but is not sent through the native path.
- Client-side policy for MP3 (what to refuse, how to project tags, what to verify after a transfer) is in [chapter 07](07-sync-engine-design.md).

## 14. Cover art

### 14.1 Checksum rule and readback

- CONFIRMED. For an MP4 `covr` cover the record's artwork checksum is the JPEG payload size + 17 (a 16-byte `data` header plus one picture). For an ID3 `APIC` cover it is the JPEG payload size + 1 (one picture). The ID3 rule began as a one-sample hypothesis (Track R's single 720x719 APIC JPEG is 54,360 bytes; Apple's operation carried 54,361). It was confirmed when AIFF, FLAC and VBR MP3 test tracks with front covers were sent with payload + 1 and the phone's covers verified. AAC covers (payload + 17) verified the same way.
- The source JPEG may be rectangular, up to 4096 pixels on a side and 32 MiB compressed. The phone's permanent renditions are a separate matter: the derivative sizes seen are square (556 x 556, 480 x 480) or nearly so (555 x 556, implied by a 925,740-byte raster). A decode of the source image does not prove that a rendition is pixel-equal to it or which image the Music UI shows.
- The artwork protocol steps (own-artwork demand, staged JPEG under `Airlock/Media/Artwork/<AssetID>`, FileComplete, terminal anchor) are codec-neutral. What differs per codec is the checksum rule above and which picture is the front cover.
- The phone does not produce renditions for grayscale covers; it leaves them as 0-byte files. All 394 renditions examined were baseline, 8-bit, three-component, 4:2:0 JPEGs.
- A cover read back from the phone is verified in three steps: byte count and SHA-256 against the expectation, a structural JPEG check, then a full entropy decode.
- Multiple pictures per file and linked pictures need separate handling and were not tested on the phone. Progressive, greyscale and PNG covers were (sections 14.3 and 14.4).

### 14.2 Study: decoding phone renditions without an ffmpeg process per cover

Question: the verification that a phone rendition is a complete, clean JPEG used one `ffmpeg` process per cover (about 0.2 s each). Can it be done faster, and without ffmpeg?

**What the existing check guarantees.** Before decoding, the file is read in full (at most 32 MiB), its length and SHA-256 must equal the expectation, and a structural check must pass. The structural check accepts only: starts with SOI; one `0xC0` baseline SOF with 8-bit precision and exactly 3 components; exactly one SOS scan; an EOI that is the last two bytes of the file; well-formed marker segment lengths. It does not look at Huffman or quantisation tables or entropy data. The decode then runs with every bitstream irregularity fatal:

```
ffmpeg -nostdin -hide_banner -v error -xerror -max_error_rate 0 -err_detect explode
       -protocol_whitelist file,pipe -threads 1 -c:v mjpeg -f jpeg_pipe -i /proc/self/fd/0
       -map 0:v:0 -an -sn -dn -map_metadata -1 -frames:v 1 -threads 1
       -pix_fmt rgb24 -c:v rawvideo -f rawvideo pipe:1
```

The raster must be exactly `width * height * 3` bytes and stderr must be empty. Semantically this means "the file entropy-decodes completely, cleanly, to exactly the declared geometry". It is not a pixel-integrity check (a bit flip that stays inside valid Huffman codes decodes fine); pixel integrity comes from the byte SHA-256. The RGB24 conversion is done by ffmpeg's swscale with default flags, so the recorded raster hash depends on swscale as well as on the JPEG decoder.

**Is the RGB24 hash compared with anything?** No. It is recorded in the verification report and kept in the cover proof cache, but no check compares it, so a decoder swap changes the recorded hash without failing anything.

**Candidates** (versions as benchmarked):

| | zune-jpeg 0.5.15 | jpeg-decoder 0.3.2 | image 0.25.10 | turbojpeg 1.5.1 (libjpeg-turbo 3.1.0) |
| --- | --- | --- | --- | --- |
| Implementation | Rust (unsafe only in SIMD) | Rust (unsafe in SIMD) | Rust (uses zune-jpeg) | C behind FFI |
| Strict mode | optional, catches only some irregularities | none | hard-coded lenient | stops on warning |
| Progressive, grayscale | yes | yes (grayscale returns L8) | yes | yes |
| CMYK to RGB | decodes (colours may be inverted) | returns CMYK32 | decodes | unsupported conversion |
| Maintenance | active | last release 2025-06 | active | active, vendored C lags upstream |

**Samples and method.** The samples were 394 real phone renditions read back over AFC: 332 at 556 x 556, 61 at 480 x 480 and one at 556 x 555 (the rendition of Track R's 720 x 719 cover). All are baseline, 8-bit, three-component, 4:2:0 JPEGs. Four further covers had 0-byte renditions (the grayscale case of section 14.1) and nothing to decode. Eight source covers were added for capability checks: two progressive 4:2:0, one baseline 4:4:4, one baseline 4:2:0 at 720 x 719 and four progressive grayscale.

The machine was a 22-thread x86 laptop CPU under a load average of about 3, so run-to-run noise is about 30 percent (zune-jpeg strict measured 3.29, 3.66 and 2.79 ms mean in three runs). ffmpeg was 8.1.3 with 317 shared libraries. Timing covers the decode call only, with the data already in memory and the output allocation included. SHA-256 of the output (about 1 ms for a 0.93 MB raster) is excluded. Each in-process figure is the best of 7 repetitions per image. ffmpeg ran once per image, one process per cover, with the exact argument list above and stderr required to be empty. Re-running ffmpeg 8.1.3 on all 394 renditions reproduced the recorded raster hashes 394 of 394.

**Per-cover decode time** (394 real renditions, milliseconds):

| Decoder | Mean | Median | p95 | Min | Max |
| --- | --- | --- | --- | --- | --- |
| ffmpeg, one process per cover | 211.8 | 192.2 | 338.4 | 124.2 | 423.6 |
| zune-jpeg 0.5.15, strict, x86 SIMD | 3.29 | 3.01 | 5.74 | 0.53 | 12.28 |
| zune-jpeg, lenient | 4.04 | 3.51 | 8.13 | 0.52 | 14.43 |
| zune-jpeg, no SIMD (safe-code build) | 4.96 | 4.16 | 10.73 | 0.98 | 15.40 |
| jpeg-decoder 0.3.2 (multi-threaded) | 2.58 | 2.51 | 3.75 | 0.94 | 8.05 |
| image 0.25.10 | 4.19 | 3.90 | 7.98 | 0.74 | 13.13 |
| libjpeg-turbo 3.1.0 (turbojpeg crate, no SIMD) | 4.03 | 3.91 | 7.19 | 1.35 | 14.18 |
| libjpeg-turbo, stop on warning | 4.98 | 4.45 | 9.50 | 1.12 | 12.16 |
| zune-jpeg strict, one CPU | 6.12 | 5.80 | 10.38 | 1.67 | 13.98 |
| jpeg-decoder, one CPU | 7.79 | 7.69 | 11.35 | 2.28 | 14.49 |
| libjpeg-turbo stop on warning, one CPU | 6.91 | 6.79 | 10.24 | 2.74 | 14.49 |

Single first-touch calls without repetition took 4.6 ms (zune-jpeg strict), 3.1 ms (image), 3.6 ms (libjpeg-turbo stop on warning) and 2.5 ms (jpeg-decoder), so warm-up does not change the result.

**RGB24 equality with ffmpeg** (394 renditions):

| Decoder | Byte-identical files | Differing bytes | Max per-channel difference | Mean absolute difference |
| --- | --- | --- | --- | --- |
| zune-jpeg (strict and lenient), image | 0 of 394 | 184,438,825 of 350,988,396 (52.55 percent) | 87 | 0.9875 |
| jpeg-decoder | 0 of 394 | 186,919,585 (53.26 percent) | 87 | 0.9877 |
| libjpeg-turbo (plain and stop on warning) | 0 of 394 | 186,744,748 (53.21 percent) | 88 | 0.9802 |

No decoder pair agrees on any file either (zune-jpeg against libjpeg-turbo, against jpeg-decoder, and libjpeg-turbo against jpeg-decoder: 0 of 394). The `image` crate equals zune-jpeg strict exactly. The zune-jpeg SIMD and scalar builds are byte-identical on all 394 (x86 only; NEON was not checked). Flat solid-colour 4:4:4 test images (24 x 17, 16 x 12, 556 x 556) are byte-identical across ffmpeg and every candidate. The Y, Cb and Cr planes before colour conversion differ from ffmpeg's by at most 1 for libjpeg-turbo on 40 samples, with ffmpeg's default or `-idct int` IDCT. 4:4:4 images differ by at most 3 and 4:2:0 images by up to 88. The large differences are therefore INFERRED to come from chroma upsampling and YUV to RGB conversion. A crate upgrade can change the bytes too (zune-jpeg 0.5.7 added SIMD upsampling; 0.5.6 fixed last-pixel sampling).

- `ffmpeg -version` alone costs 130 to 230 ms, so the per-cover cost is process start-up (317 shared libraries), not decoding. 394 covers cost 83.5 s with one process each and 1.3 to 2.7 s in-process.
- A 4096 x 4096 worst-case source decodes in 45 ms (zune-jpeg), 55 ms (jpeg-decoder), 88 ms (libjpeg-turbo) and 359 ms (ffmpeg).
- **No in-process decoder reproduces ffmpeg's RGB24 bytes** on the real renditions (table above). Any decoder swap, even a crate upgrade, changes the recorded raster hash.
- **Strictness on corrupt input.** Of 30 mutated files derived from one rendition, ffmpeg rejected 10 that pass the structural check. libjpeg-turbo rejected 9 of those 10 (it missed a zeroed quantisation entry, which a quantisation-table check could catch). jpeg-decoder rejected 6, zune-jpeg strict 2, `image` 1. The pure-Rust decoders silently pad truncated entropy data followed by an EOI marker. Bit flips that stay within valid codes pass everywhere, including ffmpeg.

The 30 files were one 556 x 556 rendition (21,242 bytes) and 29 deterministic mutations of it. A case is OK for ffmpeg only if it exits 0, prints nothing on stderr and writes exactly width * height * 3 bytes. The "check" column is the structural check described above. A failure there stops the process before any decoder runs, so only the `pass` rows matter. `edge!` means the longer side is not 556.

```text
case                                check   ffmpeg       zune-strict  zune-lenient jpeg-decoder image        turbo        turbo-strict
00-good                             pass    OK           OK           OK           OK           OK           OK           OK
01-empty                            fail    ERR          ERR          ERR          ERR          ERR          ERR          ERR
02-soi-only                         fail    ERR          ERR          ERR          ERR          ERR          ERR          ERR
03-not-jpeg                         fail    ERR          ERR          ERR          ERR          ERR          ERR          ERR
04-header-only-no-scan              fail    ERR          ERR          OK           ERR          OK           ERR          ERR
05-trunc-50pct-no-eoi               fail    ERR          ERR          OK           ERR          OK           ERR          ERR
06-trunc-50pct-plus-eoi             pass    ERR          OK           OK           ERR          OK           ERR          ERR
07-trunc-90pct-plus-eoi             pass    ERR          OK           OK           ERR          OK           ERR          ERR
08-trunc-last-100-bytes-plus-eoi    pass    ERR          OK           OK           OK           OK           ERR          ERR
09-eoi-missing                      fail    OK           OK           OK           ERR          OK           ERR          ERR
10-eoi-missing-last-byte-cut        fail    OK           OK           OK           ERR          OK           ERR          ERR
11-trailing-garbage-after-eoi       fail    OK           OK           OK           OK           OK           OK           OK
12-trailing-second-image            fail    OK           OK           OK           OK           OK           OK           OK
13-trailing-single-zero             fail    OK           OK           OK           OK           OK           OK           OK
14-bitflip-0                        pass    OK           OK           OK           OK           OK           OK           OK
14-bitflip-1                        pass    ERR          OK           OK           OK           OK           ERR          ERR
14-bitflip-2                        pass    OK           OK           OK           OK           OK           OK           OK
15-zeroed-64-bytes-mid              pass    ERR          OK           OK           OK           OK           ERR          ERR
16-garbage-16-bytes-mid             pass    ERR          OK           OK           OK           OK           ERR          ERR
17-all-ones-huffman-mid             pass    ERR          ERR          OK           ERR          OK           ERR          ERR
18-sof-height-plus-8                edge!   ERR          OK           OK           ERR          OK           ERR          ERR
19-sof-height-minus-8               pass    OK           OK           OK           OK           OK           OK           OK
20-sof-width-minus-8                pass    OK           OK           OK           OK           OK           OK           OK
21-dht-removed-one                  pass    OK           ERR          ERR          ERR          ERR          OK           OK
22-all-dht-removed                  pass    OK           ERR          ERR          ERR          ERR          OK           OK
23-dqt-zeroed-entry                 pass    ERR          OK           OK           ERR          OK           OK           OK
24-sos-bad-component-id             pass    ERR          ERR          ERR          ERR          ERR          ERR          ERR
25-eoi-injected-mid                 fail    ERR          OK           OK           ERR          OK           ERR          ERR
26-rst-marker-injected              pass    ERR          OK           OK           ERR          OK           ERR          ERR
27-leading-junk                     fail    OK           ERR          ERR          ERR          ERR          ERR          ERR
```

Reading the `pass` rows:

- ffmpeg rejects 10 check-passing variants: truncations 06 to 08, bit flip 14-1, zeroed or garbage runs 15 and 16, all-ones Huffman data 17, a zeroed quantisation entry 23, a bad scan component id 24 and an injected restart marker 26.
- libjpeg-turbo rejects 9 of those 10 (all but 23). jpeg-decoder rejects 6 (it accepts 08, 14-1, 15 and 16). zune-jpeg strict rejects 2 (17 and 24). zune-jpeg lenient and `image` reject 1 (24). The pure-Rust decoders pad truncated entropy data silently, even in strict mode.
- libjpeg-turbo behaves the same with and without stop-on-warning: the high-level call already treats any warning as an error.
- Cases 19 and 20 (frame height or width reduced by 8, entropy data unchanged) are accepted by every decoder, because the expected byte count follows the declared geometry. Only the byte SHA-256 against the expected file catches them.
- ffmpeg and libjpeg-turbo accept cases 21 and 22 (Huffman tables removed) by falling back to the standard tables. The Rust decoders reject them, which is stricter.
- A fix for the single libjpeg-turbo miss (23) is to parse DQT in the structural check and refuse a zero entry (about 15 lines).
- **Capability if the structural check were relaxed.** The check rejects 6 of the 8 source covers ("unsupported JPEG frame") and both CMYK test files, and accepts the baseline 4:4:4 and the baseline 4:2:0 720 x 719 covers. Decoder results on all eight, compared with ffmpeg RGB24 (maximum per-channel difference in brackets):

  | Cover | zune-jpeg | jpeg-decoder | libjpeg-turbo |
  | --- | --- | --- | --- |
  | Progressive 4:2:0, 800 x 800 and 500 x 500 | OK (42, 26) | OK (42, 25) | OK (42, 26) |
  | Baseline 4:4:4, 1000 x 1000 | OK (3) | OK (3) | OK (3) |
  | Baseline 4:2:0, 720 x 719 | OK (41) | OK (41) | OK (41) |
  | Progressive grayscale (4 files) | OK (1) | returns L8, caller must expand | OK (1) |
  | CMYK and YCCK (Adobe marker) | no error, colours questionable | returns CMYK32 | error: unsupported conversion |

  ffmpeg converts CMYK and YCCK to RGB24 without complaint. The phone made baseline renditions even from progressive colour originals.
- **One ffmpeg process for many covers works and keeps every hash.** Feeding all 394 renditions to one process (44.6 MB on stdin, `-f image2pipe`, `-noautoscale`, `-f framehash -hash sha256`) took 1.7 to 1.9 s (4.3 to 4.8 ms per cover), and all 394 per-frame SHA-256 values equalled the recorded hashes. `-noautoscale` is mandatory: without it ffmpeg silently rescales every frame to the first frame's size. A bad image fails the whole process, so a safe design re-runs the affected images one at a time to get the exact verdict. ffmpeg emitted no output until end of input, so the batch has to be assembled first. The spelling `-autoscale 0` is rejected by ffmpeg 8.1.3.

**Recommendation.** First, batch the existing ffmpeg decode (about 40 times faster, no change to hashes, schema or strictness). If ffmpeg must be dropped for covers, use libjpeg-turbo with a new, versioned decoder identifier and decoder-aware hash comparisons (it matched ffmpeg's rejections best and is the most fuzzed), accepting a new C build prerequisite and C code in the process. Do not use `image` (lenient only), zune-jpeg alone (accepts truncated data) or jpeg-decoder (unmaintained, weaker). A pure-Rust variant is possible only with an added strict entropy check; it was not prototyped. Progressive, grayscale and CMYK support are not needed while the structural check admits only baseline three-component JPEGs.

A measurement of the batched approach in a real run: one `ffmpeg` per group of up to 64 renditions, with `-noautoscale` and framehash output, reproduced the hashes of all 394 renditions (chapter 03, section 10.2).

The sources may also hold progressive and greyscale JPEGs (section 14.3). The structural check above still applies to the phone's renditions, which were baseline three-component JPEGs in every case seen, also for progressive originals. A source cover is checked by its own structure (JPEG markers or PNG chunks) and, once on the phone, by its exact bytes.

### 14.3 Which cover images Apple Devices sends

Static analysis of AMP and AMP2, plus the captured traffic and a phone database capture. No Apple binary was executed.

- CONFIRMED (code). The cover bytes reach the phone unchanged. The AirTraffic asset handler (`0x140458040`) fetches the picture as a `CFData` (`0x1404577b0`, from the file at `0x140513b98` or from the library item at `0x140514038`) and the write callback (`0x1404587d8`) stores those bytes as the staging file. No image function is called between the fetch and the write. `AirTrafficHost.dll` only transports the file.
- CONFIRMED (code). When AMP builds its `Artwork` object from a file (`0x1402df988`, `0x1402dfba8`), ImageIO must open the data (`CGImageSourceCreateWithData`, at least one image) or the picture is dropped. A JPEG (type 13) or PNG (type 14) is kept as it is. Any other type (BMP, GIF, TIFF, JPEG 2000 and so on) is re-encoded **to PNG** (`0x140709834`, `public.png`). The MP4 reader takes the type from the `data` atom's flags byte (13, 14, 27 for BMP); the ID3 reader maps `image/jpeg`, `image/jpg`, `image/png` and `image/bmp` and the v2.2 codes `JPG`, `PNG`, `BMP` the same way. AMP has no FLAC picture parser.
- CONFIRMED (code). `artwork_checksum` is the same for PNG as for JPEG: the number of pictures plus the summed size of the pictures' containers (`0x1402df860`, written by the builder at `0x14046295f`). For MP4 the size is the 32-bit size field of each `covr` `data` atom, header included, so one picture gives payload + 16 + 1; for ID3 it is the APIC payload, so one picture gives payload + 1. Neither the atom type nor the MIME type is read for it. At most 128 MP4 pictures are counted.
- CONFIRMED (code). No size, dimension or byte limit exists on the send path, and AMP parses no JPEG markers: no frame-kind, component or precision check exists. The phone's `AlbumArt` and `ImageSpecifications` capability dictionaries feed the old thumbnail route (`0x14051c83c`), not the AirTraffic upload.
- CONFIRMED (traffic). The reference library's M4A covers were 154 baseline, 239 progressive and 4 progressive greyscale JPEGs, all 8-bit. Of the 371 Apple-inserted tracks that map to files, 130 baseline, 237 progressive and 4 progressive greyscale covers were sent with an `artwork_cache_id` and a checksum equal to the file rule, and each has its artwork token in the phone database. Apple's sync sends progressive and greyscale JPEGs as they are. No PNG cover exists in the reference library, so no Apple traffic shows one.
- INFERRED. CMYK and 12-bit JPEGs would also be sent unchanged; none was in the traffic.

New-build equivalents (old to new): asset handler `0x140458040` to `0x140458164`; fetch `0x1404577b0` to `0x1404578d4`; library route `0x140514038` to `0x140514160`; file route `0x140513b98` to `0x140513cc0`; write callback `0x1404587d8` to `0x1404588fc`; `ArtworkCreate` `0x1402df988` to `0x1402df930` and `0x1402dfba8` to `0x1402dfb50` (same type test and PNG branch); checksum `0x1402df860` to `0x1402df808`; MP4 `covr` sizes `0x1407174a4` to `0x1407175fc`; ID3 APIC count `0x1403bdef0` to `0x1403bdeec`.

### 14.4 What the phone keeps of a JPEG or PNG original

CONFIRMED on the phone over Wi-Fi. ALAC, AAC, FLAC and MP3 test tones were sent with PNG covers, an MP3 with a progressive JPEG and a FLAC with a progressive greyscale JPEG, each as its bytes with the checksum above. The phone's originals and renditions were then copied off the phone and compared.

- **JPEG of any kind.** The progressive and the greyscale JPEG were kept byte for byte as the original. The greyscale one got no renditions, as in the earlier grayscale cases (section 14.1).
- **PNG.** The phone did not keep it. Each PNG original was replaced by the phone's own baseline JPEG of the same raster, with any transparency flattened onto white. The track's audio was unaffected, but a host that expects the original to equal the bytes it sent can never verify a PNG cover.
- A host-made JPEG of the same raster (baseline, quality 95, no chroma subsampling, transparency flattened onto white) sent instead was kept byte for byte, for RGB, RGBA, palette-with-transparency and 16-bit PNG sources alike. It differs from the phone's own conversion only in encoder rounding (38 dB between the two, the same picture).

So a sender should send a JPEG. Apple's own sync would send a PNG as it is and leave the conversion to the phone (INFERRED from section 14.3: the bytes it would stage are the bytes sent here).

## 15. Things that contradict or refine naive assumptions

1. MP3 gapless fields are not copied from a LAME tag in the common case. Apple derives them from the ID3 encoder text (Lavf and unknown encoders give delay 528, LAME gives 576), a tail decode and a frame count. A file tagged by `lame` with a valid tag is the exception: its tag values are used (section 13.10).
2. `duration` is the valid sample count after delay and drain, while `item.total_time_ms` is the coded extent in milliseconds, including the Info frame for `Info` files.
3. A positive `duration` and `total_time_ms` must be sent for new MP3 files; the phone stored zero otherwise.
4. `channels` is sent for MP3 and omitted for ALAC; `analysis_inhibit_flags` is sent for ALAC (2) and omitted for MP3.
5. `bit_rate` is the library's value in kb/s. ALAC is the PCM rate (1411 for 44.1 kHz, 16-bit, stereo; 4608 for 96 kHz, 24-bit, stereo). MP3 VBR without Xing or VBRI reports the first frame's rate, not an average.
6. Layer I and II MPEG and MPEG-2 and 2.5 Layer III have their own `audio_format` values (1xx, 2xx, 302, 303). Do not map every MPEG file to 301. Layer I and II are refused.
7. AAC is 502, not 501. HE-AAC is 508 and 509. xHE-AAC is 1001. FLAC is 602; WAV and AIFF share 701.
8. The kind string is the library's choice, not the host's. For a format never sent by Apple (FLAC), the phone does not know the string and stores kind 0, and still plays the file.
9. `MaximumBitDepth` in the capability list is never read by Apple Devices. A 24-bit WAV or AIFF is judged by sample rate and bit rate only.
10. `location` in Apple's traffic is richer (type, extension, size, date, path); a record with only `kind` and a destination path was sufficient for ALAC, MP3, AAC, HE-AAC, xHE-AAC, FLAC, WAV and AIFF.
11. The destination extension must match the codec; an `.mp3` at an `.m4a`-only path was refused.
12. A VBR flag is not sent; it is used locally in the accept check (`supportsVBR`).
13. The gapless heuristic word is `0x02000003` for analysed files and `1` for tagged files. The sender-side omission of the group (heuristic 0) was accepted by the phone for MP3, and the computed group is stored as sent (section 12.7).
14. Apple Devices sends a PNG cover unchanged and converts other image types to PNG, but the phone keeps no PNG original: it substitutes its own JPEG. Progressive and greyscale JPEGs are kept byte for byte (sections 14.3 and 14.4).

## 16. Open questions

| # | Question | Status |
| --- | --- | --- |
| 1 | Is gapless playback between MP3 tracks audibly correct when the group is omitted (heuristic 0), and when sent per section 13? | Sent per section 13: Track R played through into the next track normally (section 12.7). The omission case was never compared. |
| 2 | Meaning of `gapless_heuristic_info` bits (`0x02000003` versus `1`) and of bit 25, and what the phone does with the resynch value. | OPEN (iOS side) |
| 3 | Real Apple traffic for MP3 with a LAME tag, `iTunSMPB`, Xing, VBRI, mono, MPEG-2 or an SMPB or B4 marker. | OPEN. Only the analysis path is traffic-confirmed. The phone stored the values computed for a LAME tag, a VBRI header and MPEG-2 from Linux (section 12.7), which shows acceptance, not Apple's own values. |
| 4 | Where `volume_normalization_energy` (1792 for Track R) comes from. | OPEN. Not needed by the phone. |
| 5 | Exact behaviour of the tail search on loud, abruptly cut tracks (Apple's decoder is not bit exact). | OPEN; the no-hit rule is within 3 * 588 samples. |
| 6 | Whether the library always runs the gapless analysis pass for every MP3 import. | OPEN (INFERRED: yes) |
| 7 | `location.kind` strings and ids for Protected AAC, Purchased AAC, Audible. | OPEN. |
| 8 | Destination extensions `.m4b` and `.m4p`, and whether the phone accepts `.aifc`, `.caf`, `.wma`, ADTS `.aac`. | OPEN; by static analysis the last four have no path. |
| 9 | Whether the phone enforces limits itself (a 640 kb/s or 8 kHz file) beyond what Apple Devices checks before sending. | OPEN |
| 10 | Whether a VBR MP3's `bit_rate` should be the average or the first frame's rate when no Xing or VBRI exists. | OPEN. Apple reports the first frame's rate. |
| 11 | What the phone does with a `location.file_size` that differs from the real size. | OPEN |
| 12 | Whether a received `.flac` kind 0 has any user-visible effect (for example in "kind" display). | OPEN; it plays. |
| 13 | A pure-Rust JPEG path with a strict entropy check as a replacement for ffmpeg in cover checks. | OPEN; not prototyped. |
| 14 | What the phone keeps of a CMYK or 12-bit JPEG original. | OPEN; neither has been sent. |
| 15 | Which picture Apple's sync uses when none is marked as the front cover (ffmpeg writes type 0, "other", by default). | OPEN; not researched. rustunes sends a file's only picture whatever its type, and no cover from a file with several pictures and not exactly one front cover. |

## 17. Evidence

### 17.1 Kinds of evidence

All evidence in this chapter comes from one phone (iPhone17,2, iOS 27.0.1, build 24A446) and one Windows host with Apple Devices installed.

- **Captured Apple traffic.** Windows traces of Apple Devices 1.1540.23042.0. The reset reference session staged 399 `insert_track` operations for ALAC files, one `insert_playlist` and one `update_db_info`. The delta reference session added one ALAC track and 400 `update_track` operations. One captured `insert_track` is the only Apple-made MP3 record (Track R). The ALAC records are the reference for the 601 rows and for the keys that vary by codec.
- **Static analysis of the host agent.** AMPDevicesAgent.exe 1.6.4.90 (Apple Devices 1.1540.23042.0, 12,409,712 bytes, SHA-256 `ad957f32107b7177969b3cb11b91e3922d27e622b53f9240b65cb457522111f9`) and 1.6.4.94 (Apple Devices 1.1540.24088.0, SHA-256 `f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9`). Code was read, never executed, for the track-record builder, the decision function, the capability check, the MPEG, MP4 and ID3 readers and the layer III decoder. An executable model of the MP3 gapless rule reproduces every field of the Track R record.
- **Phone database captures.** Read-only copies of the database and WAL (all 45 tables compared before and after each run, [chapter 04](04-media-library-database.md) section 2). The format runs (section 12.1 to 12.5) were also read back file by file: 405 of 405 audio files matched, and the covers of the covered tracks verified. The MP3 insert runs of section 12.6 were captured too.
- **The phone's capability list.** The lockdown dictionary `AudioCodecs`, read from the phone over Wi-Fi (section 10).
- **Linux tool checks.** `ffmpeg`, `ffprobe` and `lame` 3.100 on Track R and on synthetic 10 s 44.1 kHz stereo sine files (section 13.13 and 13.2). fdk-aac 2.0.3 and exhale 1.2.2 made the HE-AAC and xHE-AAC test files (section 12.2). ffmpeg 8.1.3, zune-jpeg, jpeg-decoder, image and libjpeg-turbo decoded 394 phone renditions (section 14.2).
- **Format runs.** Six formats in one batch (AAC-LC, AIFF, FLAC, MPEG-2 Layer III, MPEG-1 VBR, WAV), covered variants, HE-AAC, HE-AAC v2, xHE-AAC, ALAC at 96 kHz 24-bit and 48 kHz mono, retagging of FLAC and AAC, and playback of every test track.
- **MP3 timing runs.** An eleven-track batch with LAME-encoded, ffmpeg-made, `VBRI` and MPEG-2 files sent with computed gapless fields; Track R sent again with Apple's values; the stored rows read back; playback of every test track and of Track R into the next track (section 12.7).
- **Cover runs.** Static analysis of the cover path in both builds (section 14.3), a scan of the reference library's covers by frame kind, and two insert batches of test tones with PNG, progressive and greyscale covers whose originals and renditions were copied back off the phone (section 14.4).

### 17.2 Address index for the format path (both builds)

Old is AMPDevicesAgent 1.6.4.90, new is 1.6.4.94. Virtual addresses in the `0x1400xxxxx` image. The gapless functions are in section 13.15. The functions below were matched between the builds by instruction-sequence comparison, and all matched identically. Addresses inside a function shift with its start.

| What | Old | New |
| --- | --- | --- |
| Library codec and subtype to `audio_format`; inverse | `0x1403af144`; `0x1403af24c` | unchanged |
| `audio_format` to device codec FourCC | `0x1403b30c4` (in `0x1403b301c`) | unchanged |
| Capability name to codec | `0x1403b31ac` | unchanged |
| Capability check (first; second audio check) | `0x1403b35f4`; `0x1403b4438` | unchanged |
| HE-AAC clone from AAC | `0x1403b3560` | unchanged |
| Entry defaults | `0x1403b2e6c` | unchanged |
| Media kind to legacy DB item kind | `0x1403b24a8` | unchanged |
| Kind string lookup | `0x140395a14` | `0x140395a08` |
| `audio_format` source word | `0x14036afac` | `0x14036afa4` |
| VBR flag fold | `0x1403a2930` | unchanged |
| MPEG frame header to 101 to 303 | `0x140439004` | `0x140439128` |
| Encoder-ID delay rule | `0x14043d34f` to `0x14043d42f` | `0x14043d473` to `0x14043d553` |
| MP4 `iTunSMPB` parser; atom site | `0x1403c3024`; `0x1403c5dc5` | `0x1403c3020`; `0x1403c5dc1` |
| MP4 `iTunNORM` atom site | `0x1403c5cfe` | `0x1403c5cfa` |
| Accept, transcode or refuse decision | `0x140495790` | `0x1404958b8` |
| Early refusals | `0x140495937` to `0x14049596f` | `0x140495a5f` to `0x140495a97` |
| Convert-songs preference test | `0x140495915` | `0x140495a3d` |
| AIFF always-convert branch | `0x140495a33` | `0x140495b5b` |
| Ringtone length refusal | `0x140495bc5` | `0x140495ced` |
| MPEG layer accept check | `0x140495d26` | `0x140495e4e` |
| Device extension from file type | `0x140495214`; switch `0x140495224` | `0x14049533c`; `0x14049534c` |
| Ringtone flag on `.m4a` | `0x1404951af` | `0x1404952d7` |
| Extension to file type | `0x140494ebc`, `0x1403c1b80`; table `0x1408de2e0` (data) | `0x140494fe4`, `0x1403c1b7c`; table not located |
| Operation builder | `0x140461900` | `0x140461a24` |
| `audio_format` and duration emitters | `0x1404638d0` to `0x140463996` | `0x1404639f4` |
| Gapless group emit | `0x140463c6d` | `0x140463d91` |
| `location` emitters | `0x1404621c0` to `0x140462518` (kind `0x14046221f`, `0x140462476`; extension and date `0x140462421`) | `0x1404622e4` to `0x14046263c` |
| Media-kind flags | `0x140462b1b` to `0x140462d56` | `0x140462c3f` to `0x140462e7a` |
| Omit-zero numeric setter (`0x140699593` test) | `0x14069950c` | `0x140699660` |
| `AudioCodecs` per-codec parse; key table (data) | `0x14051ae30`; `0x140916430` | `0x14051af58`; not located |
| `AudioCodecs`, `VideoCodecs`, `VoiceMemoFormats` readers | `0x14051c9bc`, `0x14051cace`, `0x14051cd6e` | `0x14051cae4`, `0x14051cbf6`, `0x14051ce96` |
| Device-record validation (AAC and Audible required) | `0x14051eb7e`, `0x14051eb3e` | `0x14051eca6`, `0x14051ec66` |

Unchanged means the function sits at the same address in both builds. A region shift of +0x128 applies to `0x14049xxxx` and `0x14051xxxx`, +0x124 to `0x14046xxxx`, and -0x4 to `0x1403cxxxx`.

Real traffic used as the reference: the ALAC `insert_track` records of the reset reference session and the MP3 record captured in the same series. The phone rows quoted in section 12 come from captures of the format runs.
