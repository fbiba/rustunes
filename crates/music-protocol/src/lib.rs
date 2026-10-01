//! Reusable bounded Music protocol, media and catalogue capabilities.
//! Observations and encoded operations convey no application execution authority.
pub mod catalogue;
pub mod media;
pub mod protocol;

#[cfg(any(test, feature = "offline-test-support"))]
pub mod test_support;
