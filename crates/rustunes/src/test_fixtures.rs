//! The workspace root, for test inputs kept in the repository, and FLACs
//! retagged from them.
use std::path::Path;

pub(crate) fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("application package must be under crates in the workspace")
}

/// `original` with its Vorbis comment replaced by exactly `tags` (and its
/// padding dropped).
pub(crate) fn retagged_flac(original: &[u8], tags: &[(&str, &str)]) -> Vec<u8> {
    assert_eq!(&original[..4], b"fLaC");
    let mut blocks = Vec::new();
    let mut at = 4;
    loop {
        let header = &original[at..at + 4];
        let kind = header[0] & 0x7f;
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if kind != 1 && kind != 4 {
            blocks.push((kind, original[at + 4..at + 4 + length].to_vec()));
        }
        at += 4 + length;
        if header[0] & 0x80 != 0 {
            break;
        }
    }
    let mut comment = Vec::new();
    comment.extend(4u32.to_le_bytes());
    comment.extend(b"test");
    comment.extend((tags.len() as u32).to_le_bytes());
    for (key, value) in tags {
        let field = format!("{key}={value}");
        comment.extend((field.len() as u32).to_le_bytes());
        comment.extend(field.as_bytes());
    }
    blocks.push((4, comment));
    let mut out = b"fLaC".to_vec();
    let count = blocks.len();
    for (i, (kind, body)) in blocks.into_iter().enumerate() {
        out.push(if i + 1 == count { 0x80 | kind } else { kind });
        out.extend(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend(body);
    }
    out.extend(&original[at..]);
    out
}
