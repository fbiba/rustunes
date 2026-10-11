//! Exact read-only native row ancestry; no filesystem discovery or permit.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::PathBuf;
fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
