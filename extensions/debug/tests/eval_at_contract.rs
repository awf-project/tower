#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde_json::{Value, json};

#[path = "../src/types.rs"]
mod types;

#[allow(dead_code)]
#[path = "../src/protocol.rs"]
mod protocol;

#[allow(dead_code)]
#[path = "../src/traces.rs"]
mod traces;

#[allow(dead_code)]
#[path = "../src/session.rs"]
mod session;

#[path = "../src/eval_at.rs"]
mod eval_at;

use eval_at::{
    CaptureOptions, CapturedVariable, EvalAtExpressionResult, EvalAtFinished, EvalAtHit,
    EvalAtHitMode, EvalAtRequest, EvalAtResult,
};

#[test]
fn eval_at_request_defaults_and_result_serialization() {
    let request: EvalAtRequest = serde_json::from_value(json!({
        "lang": "rust",
        "program": "target/debug/probe_fixture"
    }))
    .unwrap();

    assert_eq!(request.lang, "rust");
    assert_eq!(request.program, "target/debug/probe_fixture");
    assert!(request.args.is_empty());
    assert!(request.env.is_empty());
    assert!(request.expressions.is_empty());
    assert_eq!(request.capture, CaptureOptions::default());
    assert_eq!(request.on_hit, EvalAtHitMode::First);
    assert_eq!(request.max_hits, 1);
    assert_eq!(request.max_depth, 2);
    assert_eq!(request.max_children, 50);

    let mut evaluated = BTreeMap::new();
    evaluated.insert(
        "answer".to_owned(),
        EvalAtExpressionResult::Value {
            value: "42".to_owned(),
            r#type: Some("i32".to_owned()),
        },
    );
    let result = EvalAtResult {
        hit: true,
        hits: vec![EvalAtHit {
            thread_id: Some(1),
            frame: None,
            stack: Vec::new(),
            locals: Vec::new(),
            args: Vec::new(),
            evaluated,
        }],
        output: Vec::new(),
        finished: EvalAtFinished::Stopped,
        exit_code: None,
        condition_unsupported: None,
    };

    let result_json = serde_json::to_value(result).unwrap();
    assert_eq!(result_json["hit"], json!(true));
    assert_eq!(result_json["finished"], json!("stopped"));
    assert_eq!(
        result_json["hits"][0]["evaluated"]["answer"],
        json!({
            "value": "42",
            "type": "i32"
        })
    );
    assert_object_has_no_key_recursively(&result_json, "session_id");
}

#[test]
fn eval_at_captured_variable_serialization_and_truncation_semantics_remain_compatible_after_origin_capture_helper_extraction()
 {
    let captured = CapturedVariable {
        name: "root".to_owned(),
        value: "{...}".to_owned(),
        r#type: Some("Fixture".to_owned()),
        children: vec![CapturedVariable {
            name: "child".to_owned(),
            value: "1".to_owned(),
            r#type: Some("i32".to_owned()),
            children: Vec::new(),
            truncated: false,
        }],
        truncated: true,
    };

    let value = serde_json::to_value(&captured).unwrap();

    assert_eq!(
        value,
        json!({
            "name": "root",
            "value": "{...}",
            "type": "Fixture",
            "children": [{
                "name": "child",
                "value": "1",
                "type": "i32",
                "children": [],
                "truncated": false
            }],
            "truncated": true
        })
    );
    assert_eq!(
        serde_json::from_value::<CapturedVariable>(value).unwrap(),
        captured
    );
}

fn assert_object_has_no_key_recursively(value: &Value, forbidden_key: &str) {
    match value {
        Value::Object(object) => {
            assert!(!object.contains_key(forbidden_key));
            for nested in object.values() {
                assert_object_has_no_key_recursively(nested, forbidden_key);
            }
        }
        Value::Array(values) => {
            for nested in values {
                assert_object_has_no_key_recursively(nested, forbidden_key);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}
