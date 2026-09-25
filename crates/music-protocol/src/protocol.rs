//! Bounded Music protocol mechanisms with caller-owned evidence paths.
//!
//! These interfaces encode messages, enforce session bounds, and report factual
//! observations. They do not establish application ownership or durable execution
//! authority; callers retain those obligations around every mutating operation.

pub mod afc;
pub mod trace;
pub mod transfer;
