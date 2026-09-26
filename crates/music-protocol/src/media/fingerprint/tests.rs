use super::*;
use std::io::Cursor;

const SOURCE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/silence-alac.m4a"
));
const STBL: &[[u8; 4]] = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl"];

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [(body.len() as u32 + 8).to_be_bytes().as_slice(), kind, body].concat()
}

fn locate<'a>(data: &'a [u8], path: &[[u8; 4]]) -> &'a [u8] {
    let mut at = 0;
    while at < data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let found = &data[at..at + size];
        if found[4..8] == path[0] {
            return if path.len() == 1 {
                found
            } else {
                locate(&found[8..], &path[1..])
            };
        }
        at += size;
    }
    panic!("missing fixture atom");
}

fn replace(data: &[u8], path: &[[u8; 4]], replacement: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut at = 0;
    let mut replaced = false;
    while at < data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let found = &data[at..at + size];
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
        at += size;
    }
    assert!(replaced);
    result
}

fn table_path(kind: [u8; 4]) -> Vec<[u8; 4]> {
    [STBL, &[kind]].concat()
}

fn patch(data: &[u8], path: &[[u8; 4]], at: usize, bytes: &[u8]) -> Vec<u8> {
    let mut target = locate(data, path).to_vec();
    target[8 + at..8 + at + bytes.len()].copy_from_slice(bytes);
    replace(data, path, &target)
}

fn fingerprint(data: &[u8]) -> Result<SourceFingerprint> {
    inspect(&mut Cursor::new(data), data.len() as u64)
}

fn reject(data: &[u8], needle: &str) {
    let error = fingerprint(data).err().expect("expected refusal");
    assert!(error.to_string().contains(needle), "{error:#}");
}

#[test]
fn exact_components_and_metadata_independent_relocation() {
    let original = fingerprint(SOURCE).unwrap();
    assert_eq!(
        original.full_file_sha256,
        <[u8; 32]>::from(Sha256::digest(SOURCE))
    );
    assert_eq!(original.audio.timescale, 44100);
    assert_eq!(original.audio.duration_ticks, 2205);
    // This pins the composite's domain, byte order and component order.
    let golden: String = original
        .audio
        .sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        golden,
        "b298a681626015d0c8c4d82db3c9bc76bb66b3815c350c54f5fbe7b29cce5801"
    );
    let media = locate(SOURCE, &[*b"mdat"]);
    assert_eq!(
        original.audio.media_sha256,
        <[u8; 32]>::from(Sha256::digest(&media[8..]))
    );
    let padding = atom(b"free", b"a changed metadata/container layout");
    let stco = table_path(*b"stco");
    let old = u32::from_be_bytes(locate(SOURCE, &stco)[16..20].try_into().unwrap());
    let moved = patch(
        SOURCE,
        &stco,
        8,
        &(old + padding.len() as u32).to_be_bytes(),
    );
    let moved = [padding.as_slice(), moved.as_slice()].concat();
    let changed = fingerprint(&moved).unwrap();
    assert!(changed.audio == original.audio);
    assert_ne!(changed.full_file_sha256, original.full_file_sha256);
    let mut body = media[8..].to_vec();
    body[0] ^= 1;
    let changed_audio = fingerprint(&replace(SOURCE, &[*b"mdat"], &atom(b"mdat", &body))).unwrap();
    assert_ne!(changed_audio.audio.sha256, original.audio.sha256);
}

#[test]
fn wide_chunk_offsets_are_equivalent_but_sample_size_representations_are_explicit() {
    let stco = table_path(*b"stco");
    let offset = u32::from_be_bytes(locate(SOURCE, &stco)[16..20].try_into().unwrap());
    let wide = [
        0_u32.to_be_bytes().as_slice(),
        &1_u32.to_be_bytes(),
        &u64::from(offset).to_be_bytes(),
    ]
    .concat();
    let original = fingerprint(SOURCE).unwrap();
    let changed = fingerprint(&replace(SOURCE, &stco, &atom(b"co64", &wide))).unwrap();
    assert!(original.audio == changed.audio);
    let stsz = table_path(*b"stsz");
    let fixed = u32::from_be_bytes(locate(SOURCE, &stsz)[12..16].try_into().unwrap());
    assert!(fixed > 0);
    let variable = [0_u32, 0, 1, fixed]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect::<Vec<_>>();
    let variable = fingerprint(&replace(SOURCE, &stsz, &atom(b"stsz", &variable))).unwrap();
    assert_eq!(variable.audio.media_sha256, original.audio.media_sha256);
    assert_ne!(variable.audio.sha256, original.audio.sha256);
}

#[test]
fn rejects_external_or_ambiguous_data() {
    let dref = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"dinf", *b"dref"];
    reject(&patch(SOURCE, dref, 16, &[0; 4]), "external data reference");
    let extra = [SOURCE, &atom(b"mdat", b"unreferenced")].concat();
    reject(&extra, "ambiguous \"mdat\"");
    let stco = table_path(*b"stco");
    let double = [locate(SOURCE, &stco), locate(SOURCE, &stco)].concat();
    reject(
        &replace(SOURCE, &stco, &double),
        "ambiguous or missing chunk offsets",
    );
    let trak = &[*b"moov", *b"trak"];
    reject(
        &replace(
            SOURCE,
            trak,
            &[locate(SOURCE, trak), locate(SOURCE, trak)].concat(),
        ),
        "multiple \"trak\"",
    );
}

#[test]
fn rejects_gaps_overlap_tails_wrong_description_and_incomplete_samples() {
    let stco = table_path(*b"stco");
    let old = u32::from_be_bytes(locate(SOURCE, &stco)[16..20].try_into().unwrap());
    for (offset, needle) in [(old - 1, "outside the media"), (old + 1, "beyond media")] {
        reject(&patch(SOURCE, &stco, 8, &offset.to_be_bytes()), needle);
    }
    let media = locate(SOURCE, &[*b"mdat"]);
    reject(
        &replace(
            SOURCE,
            &[*b"mdat"],
            &atom(b"mdat", &[&media[8..], &[0]].concat()),
        ),
        "unreferenced media",
    );
    let stsc = table_path(*b"stsc");
    reject(
        &patch(SOURCE, &stsc, 16, &2_u32.to_be_bytes()),
        "description or count",
    );
    reject(
        &patch(SOURCE, &stsc, 12, &2_u32.to_be_bytes()),
        "excess samples",
    );
    reject(
        &patch(SOURCE, &stsc, 8, &0_u32.to_be_bytes()),
        "run ordering",
    );
    reject(
        &patch(SOURCE, &stsc, 4, &(MAX_CHUNK_RUNS + 1).to_be_bytes()),
        "count exceeds",
    );
}

#[test]
fn refuses_nonregular_sources_before_open_and_changed_metadata() {
    let root = std::env::temp_dir().join(format!(
        "fingerprint-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    assert!(read(&root).is_err());
    let path = root.join("source.m4a");
    fs::write(&path, SOURCE).unwrap();
    let before = fs::metadata(&path).unwrap();
    assert!(read(&path).is_ok());
    #[cfg(unix)]
    {
        let link = root.join("link.m4a");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link).is_err());
        fs::remove_file(link).unwrap();
    }
    fs::write(&path, b"changed").unwrap();
    assert!(!same_metadata(&before, &fs::metadata(&path).unwrap()).unwrap());
    fs::remove_file(path).unwrap();
    fs::remove_dir(root).unwrap();
}
