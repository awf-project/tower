//! Core engine library root.
//!
//! Hexagonal layout: the domain holds business logic (no I/O), ports define the
//! contracts adapters must satisfy, and adapters wire the domain to the outside
//! world. `domain` is implemented (spec 01); `ports` and `adapters` are empty
//! until spec 02 onward.

pub mod adapters;
pub mod domain;
pub mod ports;
