//! Test fixture: a plugin that intentionally reports a wrong ABI version.
//!
//! Used by spec 11c AC4 tests to verify that the host rejects plugins
//! with mismatched `manifest.abi`. The exported `__plugin_init` returns a
//! manifest with `abi = 0` (guaranteed to differ from any real ABI_VERSION).
//!
//! # Build
//!
//! ```sh
//! cargo build -p fixture_abi_mismatch --target wasm32-wasip1
//! ```
//!
//! The resulting `.wasm` is loaded by the 11c integration tests.

// This fixture hand-writes the unsafe export surface that `#[plugin_main]`
// normally generates (which carries its own lint allowance), so mirror that.
#![allow(clippy::missing_safety_doc)]

// Decision: instead of using #[plugin_main] (which would embed the real
// ABI_VERSION), we manually implement the four export symbols to hardcode
// abi = 0. This is the only safe way to produce a fixture with a wrong ABI
// from within the plugin_sdk framework (we cannot override ABI_VERSION).
//
// Trade-off: duplicates a small amount of glue code; acceptable for a fixture
// that is test-only and never changes.

use plugin_sdk::__private;
use plugin_sdk::{HookKind, HookPayload, Plugin, PluginManifest, SdkError, Value};

pub struct MismatchPlugin;

impl Plugin for MismatchPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "mismatch".to_owned(),
            version: "0.1.0".to_owned(),
            // Intentionally wrong ABI version — the host must reject this.
            abi: 0,
            tools: vec![],
            hooks: vec![],
        }
    }

    fn call_tool(name: &str, _args: Value) -> Result<Value, SdkError> {
        Err(SdkError::ToolNotFound(name.to_owned()))
    }

    fn on_hook(_kind: HookKind, _payload: HookPayload) {}
}

// Manual export surface — mirrors what #[plugin_main] would generate but with
// the manifest above (abi=0) instead of the real ABI_VERSION.

#[unsafe(no_mangle)]
pub extern "C" fn __plugin_init() -> *mut u8 {
    let manifest = MismatchPlugin::init();
    let encoded = __private::encode_manifest(manifest);
    __private::vec_into_raw(encoded)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __plugin_call_tool(ptr: *mut u8, len: usize) -> *mut u8 {
    let response = {
        let request_bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        __private::dispatch_call_tool::<MismatchPlugin>(request_bytes)
    };
    unsafe { __private::free_alloc_buffer(ptr, len) };
    __private::vec_into_raw(response)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __plugin_on_hook(ptr: *mut u8, len: usize) {
    let hook_bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    __private::dispatch_on_hook::<MismatchPlugin>(hook_bytes);
    unsafe { __private::free_alloc_buffer(ptr, len) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __plugin_free(ptr: *mut u8, len: usize) {
    unsafe { __private::free_buffer(ptr, len) };
}

#[unsafe(no_mangle)]
pub extern "C" fn __plugin_alloc(len: u32) -> *mut u8 {
    __private::alloc_guest_buffer(len as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::ABI_VERSION;

    #[test]
    fn manifest_has_wrong_abi() {
        let manifest = MismatchPlugin::init();
        assert_eq!(manifest.abi, 0);
        assert_ne!(
            manifest.abi, ABI_VERSION,
            "fixture must differ from host ABI"
        );
    }
}
