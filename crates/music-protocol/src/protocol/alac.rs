//! Pure ALAC insert encoding, including the composer sort-label rule.
//! Inputs are observations and proposed opaque IDs. Serializing bytes does not
//! allocate identities, validate ownership, or grant a device execution permit.
use crate::media::metadata::Metadata;
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use std::time::SystemTime;

/// Proposed IDs; collision/ownership validation belongs to the application.
pub struct Ids {
    pub track: u64,
    pub album: u64,
    pub artist: u64,
}

/// Exact catalogue identities and the anchor preceding this proposed operation.
pub struct Catalog {
    pub database_pid: u64,
    pub primary_container_pid: u64,
    pub media_anchor: u32,
}

/// Serialize one operation without reinterpreting metadata or gaining authority.
/// `None` sends the composer as its own sort label. An explicit pair supplies
/// the existing composer label and its independent stored sort label.
/// A presentation sends its sort names and comment; without one every sort
/// name is its name and no comment is sent.
pub fn serialize(
    source: &Metadata,
    ids: &Ids,
    catalog: Catalog,
    imported_at: SystemTime,
    existing_composer: Option<(&str, &str)>,
    presentation: Option<&crate::catalogue::presentation::Presentation<'_>>,
) -> Result<Vec<u8>> {
    ensure!(
        catalog.database_pid != 0 && catalog.primary_container_pid != 0,
        "missing current catalog identity"
    );
    let revision = catalog
        .media_anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    // Apple's sync sent 1411 kb/s for every 44.1 kHz 16-bit stereo ALAC
    // insert: the PCM rate, not the file's. Other classes send theirs.
    let bit_rate =
        (u64::from(source.sample_rate) * u64::from(source.bit_depth) * u64::from(source.channels)
            + 500)
            / 1000;
    crate::media::format::Format::Alac.check_phone_limits(
        source.sample_rate,
        source.channels,
        source.bit_depth,
        u32::try_from(bit_rate)?,
    )?;
    let scalar = source.projected()?;
    source.ensure_compilation_profile()?;
    let (title, artist, album) = (scalar.title, scalar.artist, scalar.album);
    let (album_artist, genre, composer, copyright) = (
        scalar.album_artist,
        scalar.genre,
        scalar.composer,
        scalar.copyright,
    );
    let samples = source.timing.pcm_samples_per_channel();
    let milliseconds =
        samples.checked_mul(1000).context("duration overflow")? / u64::from(source.sample_rate);
    ensure!(milliseconds > 0, "empty audio duration");
    validate_numbering(source.track, source.track_total, "track")?;
    validate_numbering(source.disc, source.disc_total, "disc")?;

    let mut audio = Dictionary::new();
    for (name, value) in [
        ("audio_format", 601),
        ("bit_rate", bit_rate),
        ("duration", samples),
        ("gapless_encoding_delay", 0),
        ("gapless_encoding_drain", 0),
        ("gapless_heuristic_info", 1),
        ("gapless_last_frame_resynch", 0),
        ("analysis_inhibit_flags", 2),
    ] {
        audio.insert(name.into(), value.into());
    }
    audio.insert(
        "sample_rate".into(),
        Value::Real(f64::from(source.sample_rate)),
    );
    // No source-derived normalization analysis exists. Native insertions can
    // omit volume_normalization_energy; do not copy another song's value.
    let mut stats = Dictionary::new();
    for name in [
        "play_count_recent",
        "play_count_user",
        "skip_count_recent",
        "skip_count_user",
    ] {
        stats.insert(name.into(), 0u64.into());
    }
    stats.insert("has_been_played".into(), false.into());

    let mut item = Dictionary::new();
    item.insert("album_pid".into(), (ids.album as i64).into());
    item.insert("artist_pid".into(), (ids.artist as i64).into());
    for (name, text) in [
        ("title", title),
        ("artist", artist),
        ("album", album),
        ("sort_name", presentation.map_or(title, |p| p.sort_title)),
        (
            "sort_artist",
            presentation.map_or(artist, |p| p.sort_artist),
        ),
        ("sort_album", presentation.map_or(album, |p| p.sort_album)),
    ] {
        item.insert(name.into(), text.into());
    }
    if let Some(text) = album_artist {
        item.insert("album_artist".into(), text.into());
        let sort = presentation
            .and_then(|p| p.sort_album_artist)
            .unwrap_or(text);
        item.insert("sort_album_artist".into(), sort.into());
    }
    if let Some(text) = presentation.and_then(|p| p.comment) {
        item.insert("comment".into(), text.into());
    }
    // Apple's host emits a plist boolean only for canonical true. Explicit false
    // and absence retain the qualified ordinary wire shape (field omitted).
    if scalar.compilation == Some(true) {
        item.insert("is_compilation".into(), true.into());
    }
    if let Some(text) = genre {
        item.insert("genre".into(), text.into());
    }
    if let Some(text) = composer {
        let (label, sort) = existing_composer.unwrap_or_else(|| {
            (
                text,
                presentation.and_then(|p| p.sort_composer).unwrap_or(text),
            )
        });
        item.insert("composer".into(), label.into());
        item.insert("sort_composer".into(), sort.into());
    }
    if let Some(text) = copyright {
        item.insert("copyright".into(), text.into());
    }
    item.insert("total_time_ms".into(), milliseconds.into());
    for (name, number) in [
        ("track_number", source.track),
        ("track_count", source.track_total),
        ("disc_number", source.disc),
        ("disc_count", source.disc_total),
        ("year", source.year),
        (
            "content_rating",
            source.content_rating.filter(|n| *n != 0).map(u32::from),
        ),
    ] {
        if let Some(number) = number {
            item.insert(name.into(), number.into());
        }
    }
    // Native dates are optional. No host-library import/modified date has yet
    // been established for this source; keep the envelope timestamp only.
    item.insert("is_song".into(), true.into());
    item.insert("remember_bookmark".into(), false.into());
    // A native album without artwork supplies this exact combination. A source's
    // embedded cover is retained inside the M4A but no artwork asset is staged.
    item.insert("artwork_checksum".into(), 0u64.into());

    let mut location = Dictionary::new();
    location.insert("kind".into(), "Apple Lossless audio file".into());
    let mut operation = Dictionary::new();
    operation.insert("operation".into(), "insert_track".into());
    operation.insert("pid".into(), (ids.track as i64).into());
    operation.insert("avformat_info".into(), Value::Dictionary(audio));
    operation.insert("item_stats".into(), Value::Dictionary(stats));
    operation.insert("item".into(), Value::Dictionary(item));
    operation.insert("location".into(), Value::Dictionary(location));
    // Ordinary native nonempty Media deltas prepend this identity-only update.
    // Do not manufacture a new catalog identity or add a primary playlist write.
    let mut db_info = Dictionary::new();
    db_info.insert(
        "primary_container_pid".into(),
        (catalog.primary_container_pid as i64).into(),
    );
    let mut database = Dictionary::new();
    database.insert("pid".into(), (catalog.database_pid as i64).into());
    database.insert("operation".into(), "update_db_info".into());
    database.insert("db_info".into(), Value::Dictionary(db_info));
    let mut envelope = Dictionary::new();
    envelope.insert("revision".into(), revision.into());
    envelope.insert(
        "operations".into(),
        Value::Array(vec![
            Value::Dictionary(database),
            Value::Dictionary(operation),
        ]),
    );
    envelope.insert("timestamp".into(), Value::Date(imported_at.into()));
    let mut bytes = Vec::new();
    Value::Dictionary(envelope).to_writer_binary(&mut bytes)?;
    ensure!(
        bytes.len() <= 64 * 1024,
        "single operation exceeds size limit"
    );
    Ok(bytes)
}

fn validate_numbering(number: Option<u32>, total: Option<u32>, name: &str) -> Result<()> {
    ensure!(
        number != Some(0) && total != Some(0),
        "invalid zero {name} numbering"
    );
    if let (Some(number), Some(total)) = (number, total) {
        ensure!(number <= total, "{name} number exceeds total");
    }
    Ok(())
}
