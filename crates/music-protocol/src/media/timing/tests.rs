use super::*;
use std::io::Cursor;

const ALAC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/silence-alac.m4a"
));
const MDHD: &[[u8; 4]] = &[*b"moov", *b"trak", *b"mdia", *b"mdhd"];
const MVHD: &[[u8; 4]] = &[*b"moov", *b"mvhd"];
const STTS: &[[u8; 4]] = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stts"];
const STSZ: &[[u8; 4]] = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsz"];
const STSD: &[[u8; 4]] = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"];
const ELST: &[[u8; 4]] = &[*b"moov", *b"trak", *b"edts", *b"elst"];

// Test-only mutations retain the real silence container and adjust ancestor
// sizes. They never write source music or invoke a metadata writer.
fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [(body.len() as u32 + 8).to_be_bytes().as_slice(), kind, body].concat()
}

fn locate<'a>(data: &'a [u8], path: &[[u8; 4]]) -> &'a [u8] {
    let mut offset = 0;
    while offset < data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        let found = &data[offset..offset + size];
        if found[4..8] == path[0] {
            return if path.len() == 1 {
                found
            } else {
                locate(&found[8..], &path[1..])
            };
        }
        offset += size;
    }
    panic!("missing fixture box {path:?}");
}

fn replace(data: &[u8], path: &[[u8; 4]], replacement: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut offset = 0;
    let mut replaced = false;
    while offset < data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        let found = &data[offset..offset + size];
        if found[4..8] == path[0] {
            replaced = true;
            if path.len() == 1 {
                result.extend_from_slice(replacement);
            } else {
                result.extend(atom(
                    &path[0],
                    &replace(&found[8..], &path[1..], replacement),
                ));
            }
        } else {
            result.extend_from_slice(found);
        }
        offset += size;
    }
    assert!(replaced);
    result
}

fn patch(data: &[u8], path: &[[u8; 4]], offset: usize, bytes: &[u8]) -> Vec<u8> {
    let mut changed = locate(data, path).to_vec();
    changed[8 + offset..8 + offset + bytes.len()].copy_from_slice(bytes);
    replace(data, path, &changed)
}

fn reject(data: &[u8], message: &str) {
    let error = read(&mut Cursor::new(data)).unwrap_err();
    assert!(
        error.to_string().contains(message),
        "expected {message:?}, got {error:#}"
    );
}

#[test]
fn exact_timing_from_real_v0_and_extended_v1_headers() {
    let expected = Timing {
        timescale: 44100,
        duration_ticks: 2205,
        packets: 1,
    };
    assert_eq!(read(&mut Cursor::new(ALAC)).unwrap(), expected);
    let mut v1 = ALAC.to_vec();
    for path in [MDHD, MVHD] {
        let body = &locate(&v1, path)[8..];
        let duration = u64::from(u32::from_be_bytes(body[16..20].try_into().unwrap()));
        let mut changed = vec![1, 0, 0, 0];
        changed.extend([0; 16]);
        changed.extend_from_slice(&body[12..16]);
        changed.extend(duration.to_be_bytes());
        changed.extend_from_slice(&body[20..]);
        v1 = replace(&v1, path, &atom(path.last().unwrap(), &changed));
    }
    let edit = [
        &[1, 0, 0, 0][..],
        &1_u32.to_be_bytes(),
        &50_u64.to_be_bytes(),
        &0_i64.to_be_bytes(),
        &0x0001_0000_u32.to_be_bytes(),
    ]
    .concat();
    v1 = replace(&v1, ELST, &atom(b"elst", &edit));
    let wide_ticks = (1_u64 << 32) + 2205;
    let wide = patch(&v1, MDHD, 24, &wide_ticks.to_be_bytes());
    let mdhd = locate(&wide, MDHD);
    assert_eq!(
        header(
            &mut Cursor::new(mdhd),
            BoxInfo {
                kind: *b"mdhd",
                data: 8,
                end: mdhd.len() as u64
            },
            true
        )
        .unwrap(),
        (44100, wide_ticks)
    );
    reject(
        &patch(&v1, MDHD, 24, &(u64::MAX - 1).to_be_bytes()),
        "metadata reader bounds",
    );
    let moov = locate(&v1, &[*b"moov"]);
    let extended = [
        &1_u32.to_be_bytes()[..],
        b"moov",
        &(moov.len() as u64 + 8).to_be_bytes(),
        &moov[8..],
    ]
    .concat();
    v1 = replace(&v1, &[*b"moov"], &extended);
    assert_eq!(read(&mut Cursor::new(v1)).unwrap(), expected);
    let no_edit = replace(ALAC, &[*b"moov", *b"trak", *b"edts"], &[]);
    assert_eq!(read(&mut Cursor::new(no_edit)).unwrap(), expected);
}

#[test]
fn rejects_disagreeing_or_unbounded_timing_tables() {
    reject(
        &patch(ALAC, MDHD, 16, &2206_u32.to_be_bytes()),
        "stts duration disagrees",
    );
    reject(
        &patch(ALAC, STSZ, 8, &2_u32.to_be_bytes()),
        "stsz packet count disagrees",
    );
    reject(&patch(ALAC, STTS, 12, &0_u32.to_be_bytes()), "zero stts");
    reject(
        &patch(ALAC, STTS, 4, &(MAX_TIME_ENTRIES + 1).to_be_bytes()),
        "entry count",
    );
    reject(
        &patch(ALAC, STTS, 4, &2_u32.to_be_bytes()),
        "truncated or extra stts",
    );
    reject(
        &patch(ALAC, STSZ, 4, &0_u32.to_be_bytes()),
        "truncated or extra stsz",
    );
    let overflow = [0_u32, 2, u32::MAX, u32::MAX, u32::MAX, u32::MAX]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect::<Vec<_>>();
    reject(
        &replace(ALAC, STTS, &atom(b"stts", &overflow)),
        "duration overflow",
    );
    reject(
        &patch(ALAC, MDHD, 12, &0_u32.to_be_bytes()),
        "unknown or zero",
    );
    reject(
        &patch(ALAC, MDHD, 16, &u32::MAX.to_be_bytes()),
        "unknown or zero",
    );
    reject(
        &patch(ALAC, MDHD, 0, &[2]),
        "unsupported timing header version",
    );
    reject(
        &patch(ALAC, MDHD, 12, &48000_u32.to_be_bytes()),
        "timescale must equal",
    );
}

#[test]
fn refuses_ambiguous_tracks_fragments_and_nontrivial_edits() {
    let path = &[*b"moov", *b"trak"];
    let track = locate(ALAC, path);
    reject(
        &replace(ALAC, path, &[track, track].concat()),
        "multiple \"trak\"",
    );
    reject(
        &[ALAC, atom(b"moof", &[]).as_slice()].concat(),
        "fragmented",
    );
    let moov = locate(ALAC, &[*b"moov"]);
    reject(
        &replace(
            ALAC,
            &[*b"moov"],
            &atom(b"moov", &[&moov[8..], &atom(b"mvex", &[])].concat()),
        ),
        "fragmented",
    );
    reject(
        &patch(ALAC, STSD, 4, &2_u32.to_be_bytes()),
        "one sample description",
    );
    let stbl_path = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl"];
    let stbl = locate(ALAC, stbl_path);
    reject(
        &replace(
            ALAC,
            stbl_path,
            &atom(b"stbl", &[&stbl[8..], &atom(b"ctts", &[0; 8])].concat()),
        ),
        "composition offsets",
    );
    for (offset, bytes) in [(8, 49_u32), (12, 1), (12, u32::MAX), (16, 0x0002_0000)] {
        reject(
            &patch(ALAC, ELST, offset, &bytes.to_be_bytes()),
            "trimmed, delayed or rate-adjusted",
        );
    }
    reject(
        &patch(ALAC, ELST, 4, &2_u32.to_be_bytes()),
        "multiple/empty edits",
    );
    reject(
        &patch(ALAC, MVHD, 16, &51_u32.to_be_bytes()),
        "movie duration mapping",
    );
}

#[test]
fn rejects_truncated_overflowing_and_excessive_box_boundaries() {
    reject(&ALAC[..ALAC.len() - 1], "box size exceeds");
    reject(&[ALAC, &[0, 0, 0]].concat(), "truncated box header");
    let huge = [&1_u32.to_be_bytes()[..], b"free", &u64::MAX.to_be_bytes()].concat();
    reject(&[ALAC, &huge].concat(), "box size exceeds");
    let mut crowded = ALAC.to_vec();
    for _ in 0..MAX_BOXES {
        crowded.extend(atom(b"free", &[]));
    }
    reject(&crowded, "too many boxes");
}
