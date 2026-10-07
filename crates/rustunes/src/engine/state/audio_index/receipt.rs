//! Parser for trusted, explicitly hash-pinned complete capture receipts.
use super::*;
use music_protocol::media::fingerprint::EncodedAlac;
use serde::Deserialize;
use serde_json::json;
use std::{fs::Metadata, io::Read};

const MAX_RECEIPT: u64 = 128 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityDto {
    device_identifier_sha256: String,
    library_identity_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDto {
    database_sha256: String,
    database_bytes: u64,
    wal_sha256: Option<String>,
    wal_bytes: Option<u64>,
    persisted_media_anchor: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OtherDto {
    sync_id: String,
    afc_path: String,
    #[serde(deserialize_with = "attributes")]
    attributes: Attributes,
    file_bytes: u64,
    full_file_sha256: String,
    codec: CodecDto,
}
#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(super) enum CodecDto {
    #[serde(rename = "mpeg1-layer3")]
    Mpeg1Layer3 {
        sample_rate: u32,
        channels: u8,
        frame_count: u32,
        audio_offset: u64,
        audio_bytes: u64,
        trailing_tag_bytes: u32,
    },
    #[serde(rename = "encoded-audio")]
    Encoded {
        format: String,
        sample_rate: u32,
        channels: u8,
        bits_per_sample: u8,
        packets: u64,
        coded_samples: u64,
        audio_offset: u64,
        audio_bytes: u64,
        config_sha256: String,
        audio_sha256: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FingerprintDto {
    file_bytes: u64,
    full_file_sha256: String,
    audio: AudioDto,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioDto {
    sha256: String,
    alac_config_sha256: String,
    sample_sizes_sha256: String,
    media_sha256: String,
    timescale: u32,
    duration_ticks: u64,
    media_bytes: u64,
}

fn attributes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Attributes, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Attributes;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded unique AFC attributes")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Attributes, A::Error> {
            let mut out = Attributes::new();
            let mut bytes = 0usize;
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                bytes = bytes.saturating_add(key.len()).saturating_add(value.len());
                if key.is_empty()
                    || key.contains('\0')
                    || value.contains('\0')
                    || bytes > 16384
                    || out.len() >= 64
                    || out.insert(key, value).is_some()
                {
                    return Err(serde::de::Error::custom(
                        "ambiguous/oversized AFC attributes",
                    ));
                }
            }
            Ok(out)
        }
    }
    deserializer.deserialize_map(Visitor)
}

pub(super) fn hash(text: &str) -> Result<[u8; 32]> {
    ensure!(
        text.len() == 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "noncanonical receipt digest"
    );
    let mut result = [0; 32];
    for (at, part) in text.as_bytes().chunks_exact(2).enumerate() {
        result[at] = u8::from_str_radix(std::str::from_utf8(part)?, 16)?;
    }
    Ok(result)
}
fn sync_id(text: &str) -> Result<u64> {
    ensure!(
        text.len() == 16
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)),
        "noncanonical receipt track identity"
    );
    let value = u64::from_str_radix(text, 16)?;
    ensure!(value != 0, "zero receipt track identity");
    Ok(value)
}

fn same(left: &Metadata, right: &Metadata) -> Result<bool> {
    let equal = left.file_type() == right.file_type()
        && left.len() == right.len()
        && left.modified()? == right.modified()?
        && left.created().ok() == right.created().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(equal
            && left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec())
    }
    #[cfg(not(unix))]
    {
        Ok(equal)
    }
}
pub(super) fn read_pinned(path: &Path, expected: [u8; 32]) -> Result<Vec<u8>> {
    ensure!(expected != [0; 32], "receipt SHA pin is required");
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("receipt parent required")?;
    let parent_before = fs::symlink_metadata(parent)?;
    ensure!(
        parent_before.is_dir() && !parent_before.file_type().is_symlink(),
        "receipt parent must be a nonsymlink directory"
    );
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.file_type().is_file() && (1..=MAX_RECEIPT).contains(&before.len()),
        "receipt must be a bounded regular nonsymlink file"
    );
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    ensure!(same(&before, &opened)?, "receipt changed before open");
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == opened.len()
            && same(&opened, &file.metadata()?)?
            && same(&opened, &fs::symlink_metadata(path)?)?
            && same(&parent_before, &fs::symlink_metadata(parent)?)?,
        "receipt or parent changed during read"
    );
    ensure!(
        <[u8; 32]>::from(Sha256::digest(&bytes)) == expected,
        "receipt SHA pin mismatch"
    );
    Ok(bytes)
}

/// Ascending upper-case track identities, as a receipt lists them.
fn record_ids(records: &[String]) -> Result<Vec<u64>> {
    let ids = records
        .iter()
        .map(|id| {
            ensure!(
                id.len() == 16
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)),
                "invalid receipt record identity"
            );
            Ok(u64::from_str_radix(id, 16)?)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "receipt record identities are not ascending"
    );
    Ok(ids)
}

/// The JSON a receipt, cache entry or pending record holds for a codec;
/// [`codec`] reads it back.
pub(crate) fn codec_json(codec: &OtherCodec) -> serde_json::Value {
    match codec {
        OtherCodec::Mpeg1Layer3 {
            sample_rate,
            channels,
            frame_count,
            audio_offset,
            audio_bytes,
            trailing_tag_bytes,
        } => json!({"kind":"mpeg1-layer3","sample_rate":sample_rate,"channels":channels,
            "frame_count":frame_count,"audio_offset":audio_offset,"audio_bytes":audio_bytes,
            "trailing_tag_bytes":trailing_tag_bytes}),
        OtherCodec::Encoded(e) => json!({"kind":"encoded-audio","format":e.format.name(),
            "sample_rate":e.sample_rate,"channels":e.channels,"bits_per_sample":e.bits_per_sample,
            "packets":e.packets,"coded_samples":e.coded_samples,"audio_offset":e.audio_offset,
            "audio_bytes":e.audio_bytes,"config_sha256":music_protocol::media::mpeg::hex(&e.config_sha256),
            "audio_sha256":music_protocol::media::mpeg::hex(&e.audio_sha256)}),
    }
}

pub(super) fn codec(dto: CodecDto, file_bytes: u64) -> Result<OtherCodec> {
    Ok(match dto {
        CodecDto::Encoded {
            format,
            sample_rate,
            channels,
            bits_per_sample,
            packets,
            coded_samples,
            audio_offset,
            audio_bytes,
            config_sha256,
            audio_sha256,
        } => {
            let encoded = music_protocol::media::fingerprint::EncodedAudio {
                format: music_protocol::media::format::Format::from_name(&format)
                    .context("unknown audio format")?,
                sample_rate,
                channels,
                bits_per_sample,
                packets,
                coded_samples,
                audio_offset,
                audio_bytes,
                config_sha256: hash(&config_sha256)?,
                audio_sha256: hash(&audio_sha256)?,
            };
            encoded.validate(file_bytes)?;
            OtherCodec::Encoded(encoded)
        }
        CodecDto::Mpeg1Layer3 {
            sample_rate,
            channels,
            frame_count,
            audio_offset,
            audio_bytes,
            trailing_tag_bytes,
        } => {
            ensure!(
                [32000, 44100, 48000].contains(&sample_rate)
                    && [1, 2].contains(&channels)
                    && (2..=2_000_000).contains(&frame_count)
                    && audio_offset <= 16 * 1024 * 1024 + 20
                    && [0, 128].contains(&trailing_tag_bytes)
                    && audio_bytes >= u64::from(frame_count) * 96
                    && audio_bytes <= u64::from(frame_count) * 1441
                    && audio_offset
                        .checked_add(audio_bytes)
                        .and_then(|n| n.checked_add(u64::from(trailing_tag_bytes)))
                        == Some(file_bytes),
                "invalid MPEG classification evidence"
            );
            OtherCodec::Mpeg1Layer3 {
                sample_rate,
                channels,
                frame_count,
                audio_offset,
                audio_bytes,
                trailing_tag_bytes,
            }
        }
    })
}
