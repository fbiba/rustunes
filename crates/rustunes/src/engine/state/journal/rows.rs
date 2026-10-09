//! Row-level validation of attempts, bindings, reservations and the attempt
//! sequence, independent of the SQL constraints.
use super::*;

type TransitionEvidence = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<u32>,
);
