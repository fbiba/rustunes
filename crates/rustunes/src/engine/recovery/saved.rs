//! The saved Current reader: every saved or retained Current directory (a
//! capture's fresh state, a run's Before, a step's After, or a copy of
//! one) is loaded read-only here, for recovery. No connector, device
//! discovery, transport, Started transition or replay is available.
//!
//! A load reads the state receipt with its codec
//! (`engine::state::current::receipt`), pinned by a `SavedCurrent`'s hash
//! or, for evidence nothing earlier pins, unpinned. Each file the receipt
//! binds is then read and compared with its hash, and a file it records
//! none of (a CDB or ArtworkDB the phone did not have, or a log the
//! catalogue did not have) must not lie beside it. The directory must be a
//! canonical mode 0700 one, and every file in it private. What a caller
//! asks of the evidence beyond that (the Before of a retained attempt
//! being the prepared revision, say) is its own policy.
use crate::engine::state::retained_files as io;

mod current;
pub use current::SavedCurrent;
pub(crate) use current::load_current as load_saved_current;
pub(crate) use current::load_current_with_control as load_saved_current_with_control;
pub(crate) use current::load_unpinned as load_unpinned_saved_current;
pub(crate) use current::load_unpinned_with_control as load_unpinned_saved_current_with_control;

#[cfg(test)]
mod tests;
