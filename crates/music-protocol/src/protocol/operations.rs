//! How many operations one staged operation file may hold. Apple's host
//! writes at most 500 operations to one `Sync_%08X.plist`, `update_db_info`
//! included, and splits a larger sync over several files of the same session.
//! How the phone takes several files of one session is not known, so every
//! builder here stages one file and refuses to put more than 500 in it.
use anyhow::{Result, ensure};

/// Operations in one operation file, `update_db_info` included.
pub const MAX_OPERATIONS: usize = 500;

/// Refuse an operation file of `count` operations above [`MAX_OPERATIONS`].
pub fn ensure_within(count: usize) -> Result<()> {
    ensure!(
        count <= MAX_OPERATIONS,
        "an operation file of {count} operations is over the {MAX_OPERATIONS} one file may hold"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_holds_at_most_five_hundred_operations() {
        ensure_within(1).unwrap();
        ensure_within(MAX_OPERATIONS).unwrap();
        let error = ensure_within(MAX_OPERATIONS + 1).unwrap_err().to_string();
        assert!(error.contains("501 operations"), "{error}");
    }
}
