//! Saved Current evidence, reloaded read-only by insert recovery and MP3
//! verification. No connector, device discovery, transport, Started transition
//! or replay is available here.
use crate::engine::state::retained_files as io;

mod current;
pub use current::SavedCurrent;
pub(crate) use current::load_current as load_saved_current;
pub(crate) use current::load_current_with_control as load_saved_current_with_control;

#[cfg(test)]
mod tests;
