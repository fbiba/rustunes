//! The read-only check that an MP3 source is new to the phone, shared by
//! planning and insert batches.
use super::*;
use crate::{
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::state::audio_index::{DeviceAudioIndex, OtherCodec},
};

fn known_distinct_owned(
    index: &DeviceAudioIndex,
    track: u64,
    bindings: &[Binding],
    source: &music_protocol::media::fingerprint::TransferredSource,
) -> bool {
    bindings.iter().any(|binding| {
        binding.content.track_pid == track
            && binding.content.source.audio.mpeg().is_some()
            && index.matches_transferred_source(
                track,
                &binding.content.path,
                &binding.content.source,
            )
            && binding.content.source.audio.sha256 != source.audio.sha256
    })
}

/// Shared read-only duplicate/identity check for planning and fresh preparation.
/// The device index records no MPEG frame digest, so a layout-only unowned
/// row cannot establish distinct encoded audio.
pub(crate) fn ensure_new_source(
    source: &PreparedSource,
    index: &DeviceAudioIndex,
    bindings: &[Binding],
) -> Result<()> {
    ensure!(
        !bindings
            .iter()
            .any(|b| b.content.source.audio.sha256 == source.identity().audio.sha256),
        "MPEG encoded audio is already owned; preserve the existing binding"
    );
    ensure!(
        !index
            .tracks()
            .values()
            .any(|r| r.fingerprint.full_file_sha256 == source.identity().full_file_sha256),
        "MPEG bytes already occur in the complete index"
    );
    let mpeg = source.identity().audio.mpeg();
    for row in index.other_audio().values() {
        ensure!(
            row.full_file_sha256 != source.identity().full_file_sha256,
            "MPEG source already exists on the phone; the existing track is kept"
        );
        // Every other format's index identity covers its coded payload.
        let (
            OtherCodec::Mpeg1Layer3 {
                sample_rate,
                channels,
                frame_count,
                audio_bytes,
                ..
            },
            Some(mpeg),
        ) = (&row.codec, mpeg)
        else {
            if let OtherCodec::Encoded(encoded) = &row.codec {
                ensure!(
                    encoded.sha256() != source.identity().audio.sha256,
                    "the source's audio already exists on the phone; the existing track is kept"
                );
            }
            continue;
        };
        let (sample_rate, channels, frame_count, audio_bytes) =
            (*sample_rate, *channels, *frame_count, *audio_bytes);
        // The index has no frame digest. Similar layout never proves equal
        // audio, but prevents automatic duplication until exact readback.
        let same_layout = (sample_rate, channels, frame_count, audio_bytes)
            == (
                mpeg.sample_rate,
                mpeg.channels,
                mpeg.frame_count,
                mpeg.audio_bytes,
            );
        let known_distinct = known_distinct_owned(index, row.sync_id, bindings, source.identity());
        ensure!(
            !same_layout || known_distinct,
            "unowned or drifted MPEG frame layout needs exact audio readback before allocating a second track"
        );
    }
    Ok(())
}
