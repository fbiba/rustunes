//! Observed, fully decoded artwork renditions. Pure path/geometry rules;
//! these helpers grant no read, mutation, absence, or completion authority.
use anyhow::{Context, Result, ensure};

pub const ARTWORK_ROOT: &str = "iTunes_Control/iTunes/Artwork";
/// Read 556 first, the largest rendition and the one most square covers get,
/// then the other entropy-qualified renditions.
/// Observed directory names alone do not qualify a raster size.
pub const QUALIFIED_EDGES: [u16; 5] = [556, 531, 480, 390, 144];

pub fn is_qualified_edge(edge: u16) -> bool {
    QUALIFIED_EDGES.contains(&edge)
}

pub fn validate_relative_path(relative: &str) -> Result<()> {
    ensure!(
        relative.len() == 41
            && relative.as_bytes()[2] == b'/'
            && relative
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 2 || b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "artwork suffix must be canonical 2hex/38hex"
    );
    Ok(())
}

pub fn derivative_path(relative: &str, edge: u16) -> Result<String> {
    ensure!(
        is_qualified_edge(edge),
        "unqualified artwork rendition edge"
    );
    validate_relative_path(relative)?;
    Ok(format!("{ARTWORK_ROOT}/Caches/{edge}x{edge}/{relative}"))
}

pub fn parse_cache_path(path: &str) -> Result<(u16, &str)> {
    let tail = path
        .strip_prefix("iTunes_Control/iTunes/Artwork/Caches/")
        .context("unsupported artwork cache root")?;
    let (geometry, relative) = tail
        .split_once('/')
        .context("artwork cache geometry absent")?;
    validate_relative_path(relative)?;
    let edge = QUALIFIED_EDGES
        .into_iter()
        .find(|edge| geometry == format!("{edge}x{edge}"))
        .context("unqualified artwork cache geometry")?;
    Ok((edge, relative))
}

/// A rendition's longer side is its cache path's edge; a cover that is not
/// square keeps its aspect (720x719 gives 556x555).
pub fn rgb24_bytes(width: u16, height: u16) -> Result<u64> {
    ensure!(
        is_qualified_edge(width.max(height)) && width.min(height) > 0,
        "unqualified artwork raster geometry"
    );
    Ok(u64::from(width) * u64::from(height) * 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SUFFIX: &str = "a0/6716a80c98398cc0cb0a245f8e347fc804a84f";
    #[test]
    fn exact_qualified_geometry_and_canonical_paths_only() {
        for edge in QUALIFIED_EDGES {
            let path = derivative_path(SUFFIX, edge).unwrap();
            assert_eq!(parse_cache_path(&path).unwrap(), (edge, SUFFIX));
            assert_eq!(rgb24_bytes(edge, edge).unwrap(), u64::from(edge).pow(2) * 3);
            assert_eq!(
                rgb24_bytes(edge, edge - 1).unwrap(),
                u64::from(edge) * u64::from(edge - 1) * 3
            );
            assert!(rgb24_bytes(edge + 1, edge).is_err());
            assert!(rgb24_bytes(edge, 0).is_err());
        }
        for edge in [0, 150, 180, 354, 555, u16::MAX] {
            assert!(derivative_path(SUFFIX, edge).is_err());
            assert!(rgb24_bytes(edge, edge).is_err());
        }
        for bad in [
            "../6716a80c98398cc0cb0a245f8e347fc804a84f",
            "A0/6716a80c98398cc0cb0a245f8e347fc804a84f",
            "a0/short",
        ] {
            assert!(derivative_path(bad, 531).is_err());
        }
        for geometry in ["0531x0531", "531x480", "354x354"] {
            assert!(
                parse_cache_path(&format!("{ARTWORK_ROOT}/Caches/{geometry}/{SUFFIX}")).is_err()
            );
        }
    }
}
