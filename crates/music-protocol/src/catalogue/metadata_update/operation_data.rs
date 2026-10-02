//! Validation of retained operation DATA, without catalogue or receiver authority.
use super::{SourceMetadataPatch, normalize_source_metadata_item, operation_value};
use crate::catalogue::sqlite::Track;
use anyhow::{Context, Result, ensure};
use plist::Value;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

/// Check retained bytes against the serializer's accepted request and canonical
/// envelope. `before` and its sort are descriptive data: this does not rebind a
/// catalogue, admit shared lookup effects, restore a live binding, or authorize
/// execution/completion. The caller must validate the enclosing DTO and custody.
/// A genuine producer always emits this bounded binary representation; requiring
/// it also rejects duplicate keys, alternate encodings and trailing bytes.
pub fn validate_source_metadata_operation_data(
    before: &Track,
    before_sort: &str,
    patch: &SourceMetadataPatch,
    database_pid: u64,
    primary_container_pid: u64,
    base_anchor: u32,
    operation: &[u8],
) -> Result<()> {
    ensure!(
        (40..=65_536).contains(&operation.len()) && operation.starts_with(b"bplist00"),
        "source metadata operation is not bounded binary data"
    );
    validate_binary_layout(operation)?;
    let accepted = normalize_source_metadata_item(before, before_sort, patch)?;
    let value = Value::from_reader(OperationReader::new(operation))?;
    let timestamp = value
        .as_dictionary()
        .and_then(|root| root.get("timestamp"))
        .and_then(Value::as_date)
        .context("source metadata operation timestamp is not a date")?;
    let expected = operation_value(
        database_pid,
        primary_container_pid,
        base_anchor,
        before.sync_id,
        accepted.item,
        timestamp,
    )?;
    ensure!(
        value == expected,
        "source metadata operation differs from the accepted request or envelope"
    );
    let mut canonical = Vec::new();
    expected.to_writer_binary(&mut canonical)?;
    ensure!(
        canonical == operation,
        "source metadata operation is not canonical serializer output"
    );
    Ok(())
}

// Reject implausible trailer allocation requests before invoking the plist
// reader. This shape has fewer than 64 objects; 128 leaves bounded headroom.
fn validate_binary_layout(bytes: &[u8]) -> Result<()> {
    let trailer_start = bytes.len() - 32;
    let trailer = &bytes[trailer_start..];
    let offset_width = usize::from(trailer[6]);
    let reference_width = usize::from(trailer[7]);
    let integer = |part: &[u8]| u64::from_be_bytes(part.try_into().expect("fixed trailer slice"));
    let count = integer(&trailer[8..16]);
    let root = integer(&trailer[16..24]);
    let table = integer(&trailer[24..32]);
    ensure!(
        matches!(offset_width, 1 | 2 | 4 | 8)
            && matches!(reference_width, 1 | 2 | 4 | 8)
            && (1..=128).contains(&count)
            && root < count
            && table >= 8
            && table.checked_add(count * offset_width as u64) == Some(trailer_start as u64),
        "source metadata binary layout has invalid trailer bounds"
    );
    let table = usize::try_from(table)?;
    for offset in bytes[table..trailer_start].chunks_exact(offset_width) {
        let position = offset
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
        ensure!(
            position >= 8 && position < table as u64,
            "source metadata binary layout has an out-of-bounds object offset"
        );
    }
    Ok(())
}

// Binary plist references may revisit the same objects. Input length alone does
// not bound their expanded traversal. The supported envelope needs fewer than
// 64 seeks and 64 KiB of reads, even with every accepted text at its byte limit.
struct OperationReader<'a> {
    cursor: Cursor<&'a [u8]>,
    bytes_left: usize,
    seeks_left: u16,
}
impl<'a> OperationReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
            bytes_left: 262_144,
            seeks_left: 128,
        }
    }
}
impl Read for OperationReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.bytes_left == 0 && !buffer.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source metadata plist read budget",
            ));
        }
        let count = buffer.len().min(self.bytes_left);
        let read = self.cursor.read(&mut buffer[..count])?;
        self.bytes_left -= read;
        Ok(read)
    }
}
impl Seek for OperationReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seeks_left = self.seeks_left.checked_sub(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "source metadata plist traversal budget",
            )
        })?;
        self.cursor.seek(position)
    }
}

#[cfg(test)]
mod tests;
