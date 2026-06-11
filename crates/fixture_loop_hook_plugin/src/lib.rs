//! Test fixture: a plugin whose `on_hook` handler runs an infinite loop.
//!
//! Used by spec 11d deliver_hook fault isolation tests to verify that a wasm
//! guest hook handler that loops forever is interrupted by fuel exhaustion and
//! returned as `PluginFault` — without hanging the host thread.
//!
//! The `call_tool` side is a no-op so the fixture can also be used to verify
//! that a hook fault does not affect subsequent tool calls on a fresh sandbox.
//!
//! # Build
//!
//! ```sh
//! cargo build -p fixture_loop_hook_plugin --target wasm32-wasip1
//! ```

use plugin_sdk::{
    ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
    plugin_main,
};

/// A plugin whose hook handler runs an infinite loop.
#[plugin_main]
pub struct LoopHookPlugin;

impl Plugin for LoopHookPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "loop_hook_plugin".to_owned(),
            version: "0.1.0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![ToolDesc {
                name: "noop_tool".to_owned(),
                description:
                    "No-op tool — used to verify sandbox is still callable after a hook fault."
                        .to_owned(),
                schema_json: r#"{"type":"object","properties":{}}"#.to_owned(),
            }],
            hooks: vec![],
        }
    }

    fn call_tool(name: &str, _args: Value) -> Result<Value, SdkError> {
        match name {
            "noop_tool" => Ok(Value::Text("ok".to_owned())),
            other => Err(SdkError::ToolNotFound(other.to_owned())),
        }
    }

    fn on_hook(_kind: HookKind, _payload: HookPayload) {
        // Infinite loop: the wasm fuel budget will interrupt this.
        let mut counter: u64 = 0;
        loop {
            counter = counter.wrapping_add(1);
            if counter == u64::MAX {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_correct() {
        let m = LoopHookPlugin::init();
        assert_eq!(m.name, "loop_hook_plugin");
        assert_eq!(m.abi, ABI_VERSION);
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.tools[0].name, "noop_tool");
    }
}
