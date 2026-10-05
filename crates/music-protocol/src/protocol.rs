//! Bounded Music protocol mechanisms with caller-owned evidence paths.
//!
//! These interfaces encode messages, enforce session bounds, and report factual
//! observations. They do not establish application ownership or durable execution
//! authority; callers retain those obligations around every mutating operation.

pub mod afc;
pub mod alac;
pub mod ancillary;
pub mod batch;
pub mod framing;
pub mod mpeg;
pub mod operations;
pub mod signing;
pub mod templates;
pub mod trace;
pub mod transfer;
