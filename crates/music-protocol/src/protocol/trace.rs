//! Private immutable trace files at caller-supplied paths.
//! No application directory layout or journal policy is imposed.
use anyhow::{Context, Result};
use std::{fs, os::unix::fs::OpenOptionsExt};

/// Create a new mode-0600 trace without replacing or following an existing path.
pub fn private_file(path: &std::path::Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating new private trace file {}", path.display()))
}

/// Write and synchronize a new private trace at the caller-supplied path.
pub fn private_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = private_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// One caller-selected aggregate allowance for all evidence writers in a step.
/// Clones share monotonic consumption. Failed creation, partial writes and
/// failed serialization never return capacity. This bounds logical output bytes
/// and creation attempts, not filesystem allocation or upstream parser memory.
#[derive(Clone)]
pub struct OutputAllowance {
    state: std::sync::Arc<std::sync::Mutex<OutputState>>,
}
struct OutputState {
    byte_limit: u64,
    file_limit: u64,
    bytes: u64,
    files: u64,
    reserved_bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputUsage {
    pub charged_bytes: u64,
    pub charged_files: u64,
    pub reserved_bytes: u64,
}
impl OutputAllowance {
    pub fn new(byte_limit: u64, file_limit: u64) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(OutputState {
                byte_limit,
                file_limit,
                bytes: 0,
                files: 0,
                reserved_bytes: 0,
            })),
        }
    }
    /// Check that both handles share this exact aggregate and its state is not
    /// poisoned. This neither reserves nor consumes output capacity.
    pub fn ensure_same(&self, other: &Self) -> Result<()> {
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.state, &other.state),
            "output allowances belong to different aggregates"
        );
        self.usage()?;
        Ok(())
    }
    pub fn usage(&self) -> std::io::Result<OutputUsage> {
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("output allowance poisoned"))?;
        Ok(OutputUsage {
            charged_bytes: state.bytes,
            charged_files: state.files,
            reserved_bytes: state.reserved_bytes,
        })
    }
    fn charge(&self, bytes: u64, files: u64) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("output allowance poisoned"))?;
        if bytes > state.byte_limit - state.bytes - state.reserved_bytes
            || files > state.file_limit - state.files
        {
            return Err(std::io::Error::other(
                "aggregate evidence output allowance exhausted",
            ));
        }
        state.bytes += bytes;
        state.files += files;
        Ok(())
    }
    /// Hold capacity before consuming input whose partial bytes must survive an
    /// interruption. The caller must stop reading at capacity(). Unused capacity
    /// is released on drop; charged bytes and the reserved file attempt are never
    /// refunded. Other writers cannot spend held capacity.
    pub fn reserve_file(&self, minimum: u64, maximum: u64) -> Result<OutputReservation> {
        anyhow::ensure!(minimum <= maximum, "invalid evidence reservation range");
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("output allowance poisoned"))?;
        let capacity = maximum.min(state.byte_limit - state.bytes - state.reserved_bytes);
        anyhow::ensure!(
            capacity >= minimum && state.files < state.file_limit,
            "aggregate evidence output allowance cannot reserve input retention"
        );
        state.reserved_bytes += capacity;
        state.files += 1;
        Ok(OutputReservation {
            allowance: self.clone(),
            remaining: capacity,
        })
    }
    /// A new private append-only sink. An existing path is never followed or
    /// replaced. Drop retains partial bytes; successful finish synchronizes and
    /// seals the file mode to 0400. No raw descriptor or seek API is exposed.
    pub fn create(&self, path: &std::path::Path) -> Result<OutputTrace> {
        self.create_inner(path, None)
    }
    /// Checks the unchanged deadline/cancellation before creation and every
    /// serialization/write chunk. Retaining already received fragments should
    /// instead use create(), still charged to this same aggregate allowance.
    pub fn create_controlled(
        &self,
        path: &std::path::Path,
        control: &super::transfer::CaptureControl,
    ) -> Result<OutputTrace> {
        self.create_inner(path, Some(control.clone()))
    }
    fn create_inner(
        &self,
        path: &std::path::Path,
        control: Option<super::transfer::CaptureControl>,
    ) -> Result<OutputTrace> {
        if let Some(control) = &control {
            control.check()?;
        }
        self.charge(0, 1)?;
        Ok(OutputTrace {
            file: private_file(path)?,
            allowance: self.clone(),
            control,
            failed: false,
            finished: false,
            remaining: None,
            reservation: None,
        })
    }
    pub fn write(&self, path: &std::path::Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut output = self.create(path)?;
        output.write_all(bytes)?;
        output.finish()
    }
    pub fn write_json(&self, path: &std::path::Path, value: &impl serde::Serialize) -> Result<()> {
        let mut output = self.create(path)?;
        serde_json::to_writer(&mut output, value)?;
        output.finish()
    }
    pub fn write_xml(&self, path: &std::path::Path, value: &plist::Value) -> Result<()> {
        let mut output = self.create(path)?;
        value.to_writer_xml(&mut output)?;
        output.finish()
    }
    pub(crate) fn buffer(
        &self,
        limit: usize,
        control: &super::transfer::CaptureControl,
    ) -> ChargedBuffer {
        ChargedBuffer {
            bytes: Vec::new(),
            allowance: self.clone(),
            limit,
            control: control.clone(),
            failed: false,
        }
    }
}

/// Capacity held in the aggregate, not a new or replenishable allowance.
pub struct OutputReservation {
    allowance: OutputAllowance,
    remaining: u64,
}
impl OutputReservation {
    pub fn capacity(&self) -> u64 {
        self.remaining
    }
    /// Check that a borrowed scratch lease still holds the required capacity
    /// in this exact aggregate. This does not consume, renew or create a lease.
    pub fn ensure_allowance(&self, output: &OutputAllowance, minimum: u64) -> Result<()> {
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.allowance.state, &output.state),
            "output reservation belongs to a different aggregate"
        );
        anyhow::ensure!(
            self.remaining >= minimum,
            "output reservation has insufficient held capacity"
        );
        self.allowance.usage()?;
        Ok(())
    }
    /// Conservatively retain the whole remaining raw-scratch charge when its
    /// files may outlive the owner. Unlike ordinary unused-lease Drop, this
    /// consumes capacity permanently in the existing aggregate and file slot.
    pub fn retain_capacity(mut self) -> Result<()> {
        let remaining = self.remaining;
        self.charge(remaining)?;
        Ok(())
    }
    /// Grow this same held lease after learning an input's length. Admission is
    /// atomic with competing writers. Failed growth leaves its prior capacity
    /// intact, so already-consumed headers remain retainable without reacquiring.
    pub fn grow_to(&mut self, required: u64) -> Result<()> {
        if required <= self.remaining {
            return Ok(());
        }
        let additional = required - self.remaining;
        let mut state = self
            .allowance
            .state
            .lock()
            .map_err(|_| std::io::Error::other("output allowance poisoned"))?;
        anyhow::ensure!(
            additional <= state.byte_limit - state.bytes - state.reserved_bytes,
            "aggregate evidence output allowance cannot grow input retention"
        );
        state.reserved_bytes += additional;
        self.remaining = required;
        Ok(())
    }
    pub fn create(self, path: &std::path::Path) -> Result<OutputTrace> {
        let file = private_file(path)?;
        Ok(OutputTrace {
            file,
            allowance: self.allowance.clone(),
            control: None,
            failed: false,
            finished: false,
            remaining: None,
            reservation: Some(self),
        })
    }
    pub fn write(self, path: &std::path::Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut file = self.create(path)?;
        file.write_all(bytes)?;
        file.finish()
    }
    fn charge(&mut self, bytes: u64) -> std::io::Result<()> {
        if bytes > self.remaining {
            return Err(std::io::Error::other(
                "reserved evidence capacity exhausted",
            ));
        }
        let mut state = self
            .allowance
            .state
            .lock()
            .map_err(|_| std::io::Error::other("output allowance poisoned"))?;
        state.reserved_bytes -= bytes;
        state.bytes += bytes;
        self.remaining -= bytes;
        Ok(())
    }
}
impl Drop for OutputReservation {
    fn drop(&mut self) {
        if let Ok(mut state) = self.allowance.state.lock() {
            state.reserved_bytes -= self.remaining;
        }
    }
}

/// A bounded sink; each full write is charged before touching the file. A
/// failed or partially accepted write poisons this sink and retains its entire
/// charge, so retries cannot turn a short/failed write into a budget refund.
pub struct OutputTrace {
    file: fs::File,
    allowance: OutputAllowance,
    control: Option<super::transfer::CaptureControl>,
    failed: bool,
    finished: bool,
    remaining: Option<u64>,
    reservation: Option<OutputReservation>,
}
impl OutputTrace {
    /// Read-only facts about this exact held sink. No descriptor, seek or
    /// uncharged write access is exposed to callers checking local custody.
    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
    /// An additional per-file cap; the shared aggregate is still charged.
    pub fn limit_bytes(mut self, limit: u64) -> Self {
        self.remaining = Some(self.remaining.map_or(limit, |old| old.min(limit)));
        self
    }
    /// Synchronize a live append-only progress file without sealing it. Any
    /// control or sync failure poisons this sink; consumed charges are retained.
    pub fn sync_all(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.failed,
            "failed evidence output cannot be synchronized"
        );
        let result = (|| {
            if let Some(control) = &self.control {
                control.check()?;
            }
            self.file.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn finish(mut self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(!self.failed, "failed evidence output cannot be completed");
        if let Some(control) = &self.control {
            control.check()?;
        }
        // One sync covers the bytes and the read-only mode set just before.
        self.file
            .set_permissions(fs::Permissions::from_mode(0o400))?;
        self.file.sync_all()?;
        self.finished = true;
        Ok(())
    }
    // Binary ATC encoding needs its length before sending. Its temporary buffer
    // is charged as it grows, then consumed exactly once into its same allowance.
    // This path also retains already charged fragments after cancellation.
    pub(crate) fn write_charged(&mut self, buffer: ChargedBuffer) -> std::io::Result<()> {
        use std::io::Write;
        if self.failed
            || self.reservation.is_some()
            || self.remaining.is_some()
            || !std::sync::Arc::ptr_eq(&self.allowance.state, &buffer.allowance.state)
        {
            self.failed = true;
            return Err(std::io::Error::other("invalid charged evidence buffer"));
        }
        let result = self.file.write_all(&buffer.bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl std::io::Write for OutputTrace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.failed {
            return Err(std::io::Error::other("evidence output already failed"));
        }
        let result = (|| {
            if let Some(control) = &self.control {
                control.check().map_err(std::io::Error::other)?;
            }
            if let Some(remaining) = &mut self.remaining {
                if bytes.len() as u64 > *remaining {
                    return Err(std::io::Error::other(
                        "per-file evidence output allowance exhausted",
                    ));
                }
                *remaining -= bytes.len() as u64;
            }
            if let Some(reservation) = &mut self.reservation {
                reservation.charge(bytes.len() as u64)?;
            } else {
                self.allowance.charge(bytes.len() as u64, 0)?;
            }
            self.file.write_all(bytes)?;
            Ok(bytes.len())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.failed {
            return Err(std::io::Error::other("evidence output already failed"));
        }
        let result = self.file.flush();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
impl Drop for OutputTrace {
    fn drop(&mut self) {
        if !self.finished {
            use std::os::unix::fs::PermissionsExt;
            // Best effort only; callers must not mistake a failed serialization
            // or interrupted write for a durable complete observation.
            let _ = self.file.sync_all();
            let _ = self.file.set_permissions(fs::Permissions::from_mode(0o400));
        }
    }
}

pub(crate) struct ChargedBuffer {
    pub(crate) bytes: Vec<u8>,
    allowance: OutputAllowance,
    limit: usize,
    control: super::transfer::CaptureControl,
    failed: bool,
}
impl std::io::Write for ChargedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.failed {
            return Err(std::io::Error::other("evidence encoding already failed"));
        }
        let result = (|| {
            self.control.check().map_err(std::io::Error::other)?;
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "ATC encoded frame allowance exhausted",
                ));
            }
            self.allowance.charge(bytes.len() as u64, 0)?;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn directory() -> std::path::PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "music-protocol-private-trace-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    mod bounded;
    mod pending_extensions;

    #[test]
    fn output_allowance_identity_accepts_shared_exhausted_budget_without_charge() {
        let output = OutputAllowance::new(32, 1);
        output.charge(8, 0).unwrap();
        let lease = output.reserve_file(24, 24).unwrap();
        let before = output.usage().unwrap();
        assert_eq!(
            before,
            OutputUsage {
                charged_bytes: 8,
                charged_files: 1,
                reserved_bytes: 24,
            }
        );
        let shared = output.clone();
        output.ensure_same(&output).unwrap();
        output.ensure_same(&shared).unwrap();
        shared.ensure_same(&output).unwrap();
        assert_eq!(output.usage().unwrap(), before);
        assert_eq!(shared.usage().unwrap(), before);
        assert_eq!(lease.capacity(), 24);
    }

    #[test]
    fn output_allowance_identity_rejects_equal_independent_budgets_without_charge() {
        let output = OutputAllowance::new(32, 1);
        let other = OutputAllowance::new(32, 1);
        output.charge(8, 0).unwrap();
        other.charge(8, 0).unwrap();
        let lease = output.reserve_file(24, 24).unwrap();
        let other_lease = other.reserve_file(24, 24).unwrap();
        let before = output.usage().unwrap();
        assert_eq!(other.usage().unwrap(), before);
        assert!(output.ensure_same(&other).is_err());
        assert!(other.ensure_same(&output).is_err());
        assert_eq!(output.usage().unwrap(), before);
        assert_eq!(other.usage().unwrap(), before);
        assert_eq!(lease.capacity(), 24);
        assert_eq!(other_lease.capacity(), 24);
    }

    #[test]
    fn output_allowance_identity_rejects_poisoned_shared_budget_without_charge() {
        let output = OutputAllowance::new(32, 1);
        output.charge(8, 0).unwrap();
        let lease = output.reserve_file(24, 24).unwrap();
        let before = output.usage().unwrap();
        let shared = output.clone();
        let panic = std::panic::catch_unwind(|| {
            let _state = output.state.lock().unwrap();
            panic!("poison allowance for identity regression");
        });
        assert!(panic.is_err());
        let error = output.ensure_same(&shared).unwrap_err();
        assert!(error.to_string().contains("output allowance poisoned"));
        assert!(shared.ensure_same(&output).is_err());
        assert!(output.usage().is_err());
        assert_eq!(lease.capacity(), 24);
        let poisoned = output.state.lock().err().expect("poison remains");
        let state = poisoned.into_inner();
        assert_eq!(
            OutputUsage {
                charged_bytes: state.bytes,
                charged_files: state.files,
                reserved_bytes: state.reserved_bytes,
            },
            before
        );
    }

    #[test]
    fn raw_scratch_reservation_binds_same_aggregate_and_retains_consumption() {
        let output = OutputAllowance::new(64, 8);
        let lease = output.reserve_file(16, 16).unwrap();
        let before = output.usage().unwrap();
        lease.ensure_allowance(&output.clone(), 16).unwrap();
        assert!(
            lease
                .ensure_allowance(&OutputAllowance::new(64, 8), 16)
                .is_err()
        );
        assert!(lease.ensure_allowance(&output, 17).is_err());
        assert_eq!(output.usage().unwrap(), before);
        lease.retain_capacity().unwrap();
        assert_eq!(
            output.usage().unwrap(),
            OutputUsage {
                charged_bytes: 16,
                charged_files: 1,
                reserved_bytes: 0,
            }
        );
        assert!(output.reserve_file(49, 49).is_err());
        let ordinary = output.reserve_file(8, 8).unwrap();
        drop(ordinary);
        assert_eq!(
            output.usage().unwrap(),
            OutputUsage {
                charged_bytes: 16,
                charged_files: 2,
                reserved_bytes: 0,
            }
        );
        let partial_path = directory();
        let mut sink = output
            .reserve_file(8, 8)
            .unwrap()
            .create(&partial_path.join("partial"))
            .unwrap();
        std::io::Write::write_all(&mut sink, b"abc").unwrap();
        drop(sink);
        assert_eq!(
            output.usage().unwrap(),
            OutputUsage {
                charged_bytes: 19,
                charged_files: 3,
                reserved_bytes: 0,
            }
        );
        fs::remove_dir_all(partial_path).unwrap();
    }

    #[test]
    fn bounded_sink_metadata_identifies_owned_inode_after_name_replacement() {
        use std::os::unix::fs::MetadataExt;
        let dir = directory();
        let path = dir.join("sink");
        let moved = dir.join("held");
        let allowance = OutputAllowance::new(32, 2);
        let mut sink = allowance.create(&path).unwrap();
        let guard = fs::File::open(&path).unwrap();
        let initial = sink.metadata().unwrap();
        assert_eq!((initial.dev(), initial.ino()), {
            let held = guard.metadata().unwrap();
            (held.dev(), held.ino())
        });
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"foreign").unwrap();
        std::io::Write::write_all(&mut sink, b"owned").unwrap();
        assert_eq!(sink.metadata().unwrap().ino(), initial.ino());
        assert_ne!(fs::symlink_metadata(&path).unwrap().ino(), initial.ino());
        sink.finish().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert_eq!(fs::read(&moved).unwrap(), b"owned");
        assert_eq!(
            guard.metadata().unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(allowance.usage().unwrap().charged_bytes, 5);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn private_traces_preserve_bytes_permissions_and_exclusive_creation() {
        let directory = directory();
        let path = directory.join("receipt.bin");
        private_write(&path, b"immutable receipt\0").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"immutable receipt\0");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let error = private_write(&path, b"replacement").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("creating new private trace file")
        );
        assert_eq!(fs::read(&path).unwrap(), b"immutable receipt\0");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn private_traces_refuse_existing_symlink_without_touching_target() {
        let directory = directory();
        let target = directory.join("target.bin");
        let path = directory.join("receipt.bin");
        fs::write(&target, b"retained target").unwrap();
        symlink(&target, &path).unwrap();
        assert!(private_write(&path, b"replacement").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"retained target");
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
