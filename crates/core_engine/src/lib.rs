//! Core engine library root.
//!
//! Hexagonal layout: the domain holds business logic (no I/O), ports define the
//! contracts adapters must satisfy, and adapters wire the domain to the outside
//! world.
//!
//! - `domain`   — spec 01: pure types, no I/O.
//! - `ports`    — spec 02: trait contracts (StoragePort, FileSystemPort, …).
//! - `adapters` — spec 02: in-memory fakes; specs 04/05/11: real adapters.

pub mod adapters;
pub mod domain;
pub mod ports;

/// Test-support helpers: contract-test macros and shared fixtures.
///
/// Always available under `cfg(test)`. Also available when the `testing`
/// feature is enabled so external integration-test crates can import the
/// contract macros without activating `cfg(test)` globally.
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
