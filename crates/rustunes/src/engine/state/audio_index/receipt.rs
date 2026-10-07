//! Parser for trusted, explicitly hash-pinned complete capture receipts.
use super::*;
use music_protocol::media::fingerprint::EncodedAlac;
use serde::Deserialize;
use serde_json::json;
use std::{fs::Metadata, io::Read};

const MAX_RECEIPT: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    complete: bool,
    identity: IdentityDto,
    catalog: CatalogDto,
    track_count: usize,
    total_bytes: u64,
    encoded_audio_kind: String,
    records: Vec<TrackDto>,
    other_records: Vec<OtherDto>,
    alac_track_count: usize,
    other_track_count: usize,
    /// Rows without audio the capture was told to expect; written only when
    /// there are any.
    #[serde(default)]
    unfilled_records: Vec<String>,
    /// Catalogued rows whose file was missing, when the capture allowed it;
    /// written only when there are any.
    #[serde(default)]
    gone_records: Vec<String>,
    progress_sha256: String,
    completed_unix_time_ns: String,
    fresh_prefs_before_after_components_equal: bool,
    atomic_snapshot_claim: bool,
    // Descriptive metadata is deliberately not an identity or matching input.
    catalog_metadata: serde::de::IgnoredAny,
}
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
struct TrackDto {
    sync_id: String,
    afc_path: String,
    #[serde(deserialize_with = "attributes")]
    attributes: Attributes,
    fingerprint: FingerprintDto,
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

pub(super) fn load(
    path: &Path,
    expected: [u8; 32],
    catalog: &Catalog,
    identity: &IndexIdentity,
    absent: Absent<'_>,
) -> Result<DeviceAudioIndex> {
    let bytes = read_pinned(path, expected)?;
    let (targets, unfilled) = targets_allowing(catalog, &absent.rows(catalog))?;
    let index = parse_allowing(
        &bytes,
        &CatalogBinding::from_catalog(catalog),
        identity,
        &targets,
        &unfilled,
        absent.gone,
    )?;
    index.ensure_matches(catalog, identity)?;
    Ok(index)
}

#[cfg(test)]
pub(super) fn parse(
    bytes: &[u8],
    expected: &CatalogBinding,
    identity: &IndexIdentity,
    targets: &[Target],
) -> Result<DeviceAudioIndex> {
    parse_allowing(bytes, expected, identity, targets, &BTreeSet::new(), false)
}

/// The receipt must list exactly `unfilled` as its rows without audio; rows
/// it lists as gone must be targets, and only when `gone` allows them.
fn parse_allowing(
    bytes: &[u8],
    expected: &CatalogBinding,
    identity: &IndexIdentity,
    targets: &[Target],
    unfilled: &BTreeSet<u64>,
    gone: bool,
) -> Result<DeviceAudioIndex> {
    ensure!(
        bytes.len() as u64 <= MAX_RECEIPT,
        "receipt exceeds input bound"
    );
    let receipt: Receipt =
        serde_json::from_slice(bytes).context("invalid complete-index receipt")?;
    ensure!(
        receipt.schema == "rustunes.device-audio-index"
            && receipt.alac_track_count == receipt.records.len()
            && receipt.other_track_count == receipt.other_records.len()
            && receipt.complete
            && receipt.fresh_prefs_before_after_components_equal
            && !receipt.atomic_snapshot_claim
            && receipt.encoded_audio_kind == "encoded-alac-not-pcm",
        "unsupported or incomplete audio-index receipt"
    );
    let _metadata = receipt.catalog_metadata;
    hash(&receipt.progress_sha256)?;
    ensure!(
        !receipt.completed_unix_time_ns.is_empty()
            && receipt.completed_unix_time_ns.len() <= 39
            && receipt
                .completed_unix_time_ns
                .bytes()
                .all(|b| b.is_ascii_digit())
            && receipt.completed_unix_time_ns.parse::<u128>()?.to_string()
                == receipt.completed_unix_time_ns,
        "invalid receipt completion time"
    );
    let listed = record_ids(&receipt.unfilled_records)?;
    ensure!(
        listed.iter().copied().collect::<BTreeSet<_>>() == *unfilled,
        "receipt rows without audio differ from the catalog's"
    );
    let missing: BTreeSet<u64> = record_ids(&receipt.gone_records)?.into_iter().collect();
    ensure!(
        missing.is_empty()
            || (gone
                && missing
                    .iter()
                    .all(|id| targets.iter().any(|t| t.sync_id == *id))),
        "receipt rows whose file is gone are not allowed here or not in the catalog"
    );
    let present: Vec<Target> = targets
        .iter()
        .filter(|t| !missing.contains(&t.sync_id))
        .cloned()
        .collect();
    let mut index = validate_rows(
        receipt.identity,
        receipt.catalog,
        receipt.track_count,
        receipt.total_bytes,
        receipt.records,
        receipt.other_records,
        expected,
        identity,
        &present,
    )?;
    index.unfilled = unfilled.clone();
    index.gone = missing;
    Ok(index)
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

fn validate_rows(
    observed_identity_dto: IdentityDto,
    catalog_dto: CatalogDto,
    track_count: usize,
    total_bytes: u64,
    records: Vec<TrackDto>,
    other_records: Vec<OtherDto>,
    expected: &CatalogBinding,
    identity: &IndexIdentity,
    targets: &[Target],
) -> Result<DeviceAudioIndex> {
    let observed_identity = IndexIdentity {
        device_identifier_sha256: hash(&observed_identity_dto.device_identifier_sha256)?,
        library_identity_sha256: hash(&observed_identity_dto.library_identity_sha256)?,
    };
    ensure!(
        &observed_identity == identity
            && identity.device_identifier_sha256 != [0; 32]
            && identity.library_identity_sha256 != [0; 32],
        "receipt device/library binding differs"
    );
    let binding = CatalogBinding {
        database_sha256: hash(&catalog_dto.database_sha256)?,
        database_bytes: catalog_dto.database_bytes,
        wal_sha256: catalog_dto.wal_sha256.as_deref().map(hash).transpose()?,
        wal_bytes: catalog_dto.wal_bytes,
        persisted_media_anchor: catalog_dto.persisted_media_anchor,
    };
    ensure!(
        &binding == expected && binding.wal_sha256.is_some() == binding.wal_bytes.is_some(),
        "receipt catalog binding differs"
    );
    ensure!(
        track_count == targets.len()
            && records.len() + other_records.len() == targets.len()
            && targets.len() <= MAX_TRACKS,
        "receipt coverage count differs"
    );
    let expected_tracks: BTreeMap<_, _> = targets
        .iter()
        .map(|target| (target.sync_id, target))
        .collect();
    let mut rows = BTreeMap::new();
    let mut other_rows = BTreeMap::new();
    let mut paths = BTreeMap::new();
    let mut total = 0u64;
    for row in records {
        let id = sync_id(&row.sync_id)?;
        let target = expected_tracks
            .get(&id)
            .context("unexpected receipt track identity")?;
        ensure!(
            safe_path(&row.afc_path)
                && row.afc_path == target.path
                && !row.attributes.contains_key("st_atime"),
            "receipt track path/attributes differ"
        );
        let fingerprint = fingerprint(row.fingerprint)?;
        ensure!(
            fingerprint.file_bytes == target.bytes
                && regular_size(&row.attributes)? == target.bytes,
            "receipt track size differs"
        );
        total = total
            .checked_add(fingerprint.file_bytes)
            .context("receipt total overflow")?;
        ensure!(
            total <= MAX_TOTAL_BYTES
                && paths
                    .insert(row.afc_path.to_ascii_lowercase(), ())
                    .is_none(),
            "receipt total/path ambiguity"
        );
        ensure!(
            rows.insert(
                id,
                TrackAudio {
                    sync_id: id,
                    afc_path: row.afc_path,
                    attributes: row.attributes,
                    fingerprint
                }
            )
            .is_none(),
            "duplicate receipt track identity"
        );
    }
    for row in other_records {
        let id = sync_id(&row.sync_id)?;
        let target = expected_tracks
            .get(&id)
            .context("unexpected other-audio identity")?;
        ensure!(
            safe_path(&row.afc_path)
                && row.afc_path == target.path
                && !row.attributes.contains_key("st_atime"),
            "other-audio receipt path/attributes differ"
        );
        ensure!(
            (1..=MAX_FILE_BYTES).contains(&row.file_bytes)
                && row.file_bytes == target.bytes
                && regular_size(&row.attributes)? == target.bytes,
            "other-audio receipt size differs"
        );
        let codec = codec(row.codec, row.file_bytes)?;
        total = total
            .checked_add(row.file_bytes)
            .context("other-audio total overflow")?;
        ensure!(
            total <= MAX_TOTAL_BYTES
                && !rows.contains_key(&id)
                && paths
                    .insert(row.afc_path.to_ascii_lowercase(), ())
                    .is_none(),
            "other-audio identity/path ambiguity"
        );
        ensure!(
            other_rows
                .insert(
                    id,
                    OtherAudio {
                        sync_id: id,
                        afc_path: row.afc_path,
                        attributes: row.attributes,
                        file_bytes: row.file_bytes,
                        full_file_sha256: hash(&row.full_file_sha256)?,
                        codec
                    }
                )
                .is_none(),
            "duplicate other-audio identity"
        );
    }
    ensure!(
        total == total_bytes && rows.len() + other_rows.len() == targets.len(),
        "receipt total/coverage differs"
    );
    Ok(DeviceAudioIndex {
        identity: observed_identity,
        catalog: binding,
        tracks: rows,
        other_audio: other_rows,
        unfilled: BTreeSet::new(),
        gone: BTreeSet::new(),
    })
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

/// The JSON a receipt, cache entry or planner record holds for an ALAC source
/// fingerprint; [`fingerprint`] reads it back from its DTO.
pub(crate) fn fingerprint_json(fingerprint: &SourceFingerprint) -> serde_json::Value {
    use music_protocol::media::mpeg::hex;
    let audio = &fingerprint.audio;
    json!({"file_bytes":fingerprint.file_bytes,"full_file_sha256":hex(&fingerprint.full_file_sha256),"audio":{
        "sha256":hex(&audio.sha256),"alac_config_sha256":hex(&audio.alac_config_sha256),"sample_sizes_sha256":hex(&audio.sample_sizes_sha256),
        "media_sha256":hex(&audio.media_sha256),"timescale":audio.timescale,"duration_ticks":audio.duration_ticks,"media_bytes":audio.media_bytes}})
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

pub(super) fn fingerprint(dto: FingerprintDto) -> Result<SourceFingerprint> {
    read_fingerprint(dto, true)
}

/// A cached fingerprint, whose composite is computed again from its other
/// fields: an entry stays usable when this build defines the composite
/// differently from the build that stored it.
pub(super) fn cached_fingerprint(dto: FingerprintDto) -> Result<SourceFingerprint> {
    read_fingerprint(dto, false)
}

fn read_fingerprint(dto: FingerprintDto, stored_composite: bool) -> Result<SourceFingerprint> {
    let audio = dto.audio;
    ensure!(
        (1..=MAX_FILE_BYTES).contains(&dto.file_bytes)
            && audio.timescale > 0
            && audio.duration_ticks > 0
            && audio.media_bytes > 0
            && audio.media_bytes <= dto.file_bytes,
        "invalid receipt fingerprint dimensions"
    );
    let mut audio = EncodedAlac {
        sha256: hash(&audio.sha256)?,
        alac_config_sha256: hash(&audio.alac_config_sha256)?,
        sample_sizes_sha256: hash(&audio.sample_sizes_sha256)?,
        media_sha256: hash(&audio.media_sha256)?,
        timescale: audio.timescale,
        duration_ticks: audio.duration_ticks,
        media_bytes: audio.media_bytes,
    };
    if stored_composite {
        ensure!(
            audio.composite_sha256() == audio.sha256,
            "receipt encoded fingerprint composite differs"
        );
    } else {
        audio.sha256 = audio.composite_sha256();
    }
    Ok(SourceFingerprint {
        file_bytes: dto.file_bytes,
        full_file_sha256: hash(&dto.full_file_sha256)?,
        audio,
    })
}
