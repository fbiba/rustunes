//! Saved-only planning. The returned serialized bytes are exactly the bytes
//! persisted for later reconstruction; a plan never grants write authority.
use anyhow::{Result, ensure};
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;
use std::{fs::File, path::Path, time::Instant};
pub(crate) mod source_reads;
