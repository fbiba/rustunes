//! Bounded catalogue parsing, independent factual verification and pure operation encoding.
//! Completion, ownership and durable workflow policy belong to the caller.

pub mod anchor;
pub mod artwork_update;
pub mod artwork_verification;
pub mod companion_metadata;
pub mod identity;
pub mod inventory;
pub mod metadata_edit;
pub mod metadata_update;
pub mod metadata_verification;
pub mod mutation;
pub mod presentation;
pub mod sqlite;
pub mod title_verification;
