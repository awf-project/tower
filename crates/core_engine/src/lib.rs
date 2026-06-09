//! Core engine library root.
//!
//! Hexagonal layout: the domain holds business logic (no I/O), ports define the
//! contracts adapters must satisfy, and adapters wire the domain to the outside
//! world. Modules are placeholders until spec 01 introduces real types.

pub mod adapters;
pub mod domain;
pub mod ports;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_smoke() {
        // Proves the test harness compiles and runs for this crate.
        // Real behavior tests arrive with spec 01.
        assert_eq!(core_smoke(), 4);
    }

    fn core_smoke() -> u32 {
        2 + 2
    }
}
