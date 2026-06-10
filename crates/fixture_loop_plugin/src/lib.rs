//! Test fixture: a plugin whose `loop_tool` runs an infinite loop.
//!
//! Used by spec 11d AC2 tests to verify that a wasm guest that loops forever
//! is interrupted by fuel exhaustion and returned as `PluginFault::Trapped` —
//! without hanging the host.
//!
//! # Deterministic interruption strategy
//!
//! The host configures a small fuel budget (e.g. 100_000 instructions) before
//! each call. The infinite loop exhausts fuel deterministically in microseconds,
//! triggering `wasmtime::Trap` with a fuel-exhaustion error. This makes the
//! test fully deterministic — no wall-clock timers, no sleep.
//!
//! # Build
//!
//! ```sh
//! cargo build -p fixture_loop_plugin --target wasm32-wasip1
//! ```

use plugin_sdk::{
    plugin_main, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
    ABI_VERSION,
};

/// A plugin whose only tool runs an infinite loop.
#[plugin_main]
pub struct LoopPlugin;

impl Plugin for LoopPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "loop_plugin".to_owned(),
            version: "0.1.0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![ToolDesc {
                name: "loop_tool".to_owned(),
                description: "Loops forever — used to test host fuel/epoch interruption."
                    .to_owned(),
                schema_json: r#"{"type":"object","properties":{}}"#.to_owned(),
            }],
            hooks: vec![],
        }
    }

    fn call_tool(name: &str, _args: Value) -> Result<Value, SdkError> {
        match name {
            "loop_tool" => {
                // Infinite loop: the wasm fuel budget will interrupt this.
                // The compiler cannot optimize this away because the result
                // of the volatile read is used.
                let mut counter: u64 = 0;
                loop {
                    counter = counter.wrapping_add(1);
                    // Prevent the optimizer from eliding the loop body.
                    // Safety: this is a wasm-only fixture with no real I/O.
                    if counter == u64::MAX {
                        break;
                    }
                }
                // Unreachable in practice — fuel interrupts before this.
                Ok(Value::Text("done".to_owned()))
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
        let m = LoopPlugin::init();
        assert_eq!(m.name, "loop_plugin");
        assert_eq!(m.abi, ABI_VERSION);
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.tools[0].name, "loop_tool");
    }
}
