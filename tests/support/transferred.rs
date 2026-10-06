//! Test-only mutation of ALAC observations through the public source constructors.
//! Keep the redundant outer digest independent, including intentionally inconsistent
//! values used by refusal tests. Reconstructing with `From` alone would repair it.
use music_protocol::media::fingerprint::{EncodedAlac, TransferredSource};

pub(crate) fn mutate_alac<T>(
    source: &mut TransferredSource,
    mutate: impl FnOnce(&mut EncodedAlac) -> T,
) -> T {
    let outer_sha256 = source.audio.sha256;
    let mut fingerprint = source.require_alac().expect("test requires ALAC");
    let result = mutate(&mut fingerprint.audio);
    *source = fingerprint.into();
    source.audio.sha256 = outer_sha256;
    result
}

#[test]
fn alac_mutation_preserves_independent_outer_hash() {
    use music_protocol::media::fingerprint::SourceFingerprint;
    let mut source: TransferredSource = SourceFingerprint {
        file_bytes: 256,
        full_file_sha256: [1; 32],
        audio: EncodedAlac {
            sha256: [2; 32],
            alac_config_sha256: [3; 32],
            sample_sizes_sha256: [4; 32],
            media_sha256: [5; 32],
            timescale: 44100,
            duration_ticks: 44100,
            media_bytes: 64,
        },
    }
    .into();
    source.audio.sha256 = [9; 32];
    mutate_alac(&mut source, |audio| audio.sha256 = [7; 32]);
    assert_eq!(source.audio.sha256, [9; 32]);
    assert_eq!(source.audio.alac().unwrap().sha256, [7; 32]);
    assert_eq!(source.full_file_sha256, [1; 32]);
    assert_eq!(source.file_bytes, 256);
    assert!(source.validate().is_err());
}
