// compile-pass: #[plugin_main] on a valid Plugin impl should compile cleanly.
use plugin_sdk::{plugin_main, ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, Value};

#[plugin_main]
struct GreetPlugin;

impl Plugin for GreetPlugin {
    fn init() -> PluginManifest {
        PluginManifest {
            name: "greet".to_owned(),
            version: "0.1.0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![],
            hooks: vec![],
        }
    }

    fn call_tool(name: &str, _args: Value) -> Result<Value, SdkError> {
        Err(SdkError::ToolNotFound(name.to_owned()))
    }

    fn on_hook(_kind: HookKind, _payload: HookPayload) {}
}

fn main() {
    let manifest = GreetPlugin::init();
    assert_eq!(manifest.name, "greet");
    assert_eq!(manifest.abi, ABI_VERSION);
}
