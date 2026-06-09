//! Plugin SDK — macros and host-capability bindings for WASM guest plugins.
//!
//! Empty until spec 11a defines the `#[plugin_main]` / `#[plugin_export]`
//! surface.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_smoke() {
        // Proves the test harness compiles and runs for this crate.
        assert_eq!(sdk_smoke(), 4);
    }

    fn sdk_smoke() -> u32 {
        2 + 2
    }
}
