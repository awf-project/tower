// compile-fail: #[plugin_export] must not accept arguments.
use plugin_sdk::plugin_export;

#[plugin_export(tool_name = "greet")]
fn greet() {}

fn main() {}
