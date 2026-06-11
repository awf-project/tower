//! Test fixture: a plugin whose `panic_tool` calls `panic!`.
//!
//! Used by spec 11d AC1 tests to verify that a wasm guest panic (which becomes
//! a wasm `unreachable` trap) is caught by the host's fault isolation layer and
//! returned as `PluginFault::Trapped` — without killing the host process.
//!
//! # What the guest panic does
//!
//! In Rust compiled to `wasm32-wasip1`, a `panic!` call eventually executes
//! the wasm `unreachable` instruction, which raises a [`wasmtime::Trap`].
//! The host catches that trap via `TypedFunc::call` returning `Err(...)`,
//! which the isolation wrapper maps to `PluginFault::Trapped`.
//!
//! # Build
//!
//! ```sh
//! cargo build -p fixture_panic_plugin --target wasm32-wasip1
//! ```

use plugin_sdk::{
    ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
    plugin_main,
};

/// A plugin whose only tool unconditionally panics.
#[plugin_main]
pub struct PanicPlugin;

impl Plugin for PanicPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "panic_plugin".to_owned(),
            version: "0.1.0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![ToolDesc {
                name: "panic_tool".to_owned(),
                description: "Always panics — used to test host fault isolation.".to_owned(),
                schema_json: r#"{"type":"object","properties":{}}"#.to_owned(),
            }],
            hooks: vec![],
        }
    }

    fn call_tool(name: &str, _args: Value) -> Result<Value, SdkError> {
        match name {
            "panic_tool" => {
                panic!("deliberate panic from fixture_panic_plugin");
            }
            other => Err(SdkError::ToolNotFound(other.to_owned())),
        }
    }

    fn on_hook(_kind: HookKind, _payload: HookPayload) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_correct() {
        let m = PanicPlugin::init();
        assert_eq!(m.name, "panic_plugin");
        assert_eq!(m.abi, ABI_VERSION);
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.tools[0].name, "panic_tool");
    }
}
