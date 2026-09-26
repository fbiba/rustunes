//! The reusable decoder refuses a nonsource before spawning a local decoder.
use music_protocol::media::{pcm, timing::Timing};

#[test]
fn decoder_rejects_directory_input_before_starting_ffmpeg() {
    let timing = Timing {
        timescale: 44_100,
        duration_ticks: 44_100,
        packets: 1,
    };
    let error = pcm::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")), &timing)
        .err()
        .expect("a directory is never a PCM source");
    assert_eq!(
        error.to_string(),
        "PCM source must be a bounded regular nonsymlink file"
    );
}
