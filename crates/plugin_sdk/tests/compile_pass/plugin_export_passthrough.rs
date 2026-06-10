// compile-pass: #[plugin_export] should pass the function through unchanged.
use plugin_sdk::{plugin_export, SdkError, Value};

#[plugin_export]
fn greet(args: Value) -> Result<Value, SdkError> {
    let _ = args;
    Ok(Value::Null)
}

fn main() {
    let result = greet(Value::Null);
    assert!(result.is_ok());
}
