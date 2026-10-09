//! The catalogue's bounded raw-SQL table reader and capture, which the insert,
//! metadata and removal checks share. A result verifies copied SQL only.
pub(crate) use music_protocol::catalogue::sqlite::tables as reader;
pub use reader::{Capture, SqlHashes};

#[cfg(test)]
pub(crate) mod tests;
