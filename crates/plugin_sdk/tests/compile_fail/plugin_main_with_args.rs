// compile-fail: #[plugin_main] must not accept arguments.
use plugin_sdk::plugin_main;

#[plugin_main(some_arg)]
struct BadPlugin;

fn main() {}
