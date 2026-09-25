//! Private immutable trace files at caller-supplied paths.
//! No application directory layout or journal policy is imposed.
use anyhow::{Context, Result};
use std::{fs, os::unix::fs::OpenOptionsExt};

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
}
