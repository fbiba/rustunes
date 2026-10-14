//! The cover an insert batch sends with an MP3 or another non-ALAC source. An
//! ID3 or FLAC cover's checksum counts the image payload alone, where an MP4
//! cover's (AAC) counts the whole `data` atom header too.
use anyhow::Result;
use music_protocol::media::mpeg::{Source, read_front_cover};
use std::path::Path;

/// The cover an insert batch sends with a source's audio: the picture
/// `mpeg::cover_index` chooses, a JPEG byte for byte and any other image as its
/// JPEG. The native checksum is the picture count plus the payload length, as
/// Apple's own sync sent for the reference MP3 (54,360 + 1); an MP4 cover's
/// also counts its 16-byte `data` atom header, as for ALAC.
pub(crate) fn batch_cover(
    path: &Path,
    source: &Source,
) -> Result<Option<music_protocol::media::artwork::Cover>> {
    let Some(cover) = read_front_cover(path, source)? else {
        return Ok(None);
    };
    music_protocol::media::artwork::sent_cover(
        cover.bytes(),
        if source.format().mp4() {
            music_protocol::media::artwork::MP4_CHECKSUM_HEADER
        } else {
            1
        },
    )
    .map(Some)
}

#[cfg(test)]
mod tests;
