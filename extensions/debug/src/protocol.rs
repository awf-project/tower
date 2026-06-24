#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path};

use extension_protocol::ToolDecl;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugInitializeConfig {
    pub languages: BTreeMap<String, DebugAdapterConfig>,
    #[serde(default)]
    pub record: Option<DebugRecordConfig>,
}

impl DebugInitializeConfig {
    pub fn from_init_payload(payload: Option<Value>) -> Result<Option<Self>, DebugInitError> {
        let Some(payload) = payload else {
            return Ok(None);
        };

        let config: Self = serde_json::from_value(payload)
            .map_err(|error| DebugInitError::InvalidConfig(error.to_string()))?;
        config.validate()?;
        Ok(Some(config))
    }

    pub fn is_empty(&self) -> bool {
        self.languages.is_empty() && self.record.is_none()
    }

    pub fn supports_rr_record(&self) -> bool {
        self.record
            .as_ref()
            .is_some_and(DebugRecordConfig::is_rr_backend)
    }

    fn validate(&self) -> Result<(), DebugInitError> {
        for (language, config) in &self.languages {
            if language.is_empty() {
                return Err(DebugInitError::InvalidConfig(
                    "debug language key must not be empty".to_owned(),
                ));
            }
            config.validate(language)?;
        }
        if let Some(record) = &self.record {
            record.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugRecordConfig {
    pub backend: String,
    pub trace_dir: Option<String>,
    pub ttl_secs: Option<u64>,
    pub max_traces: Option<usize>,
    pub record_timeout_secs: Option<u64>,
}

impl DebugRecordConfig {
    pub fn is_rr_backend(&self) -> bool {
        self.backend == "rr"
    }

    fn validate(&self) -> Result<(), DebugInitError> {
        if self.backend != "rr" {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.record.backend unsupported value {:?}",
                self.backend
            )));
        }
        if let Some(trace_dir) = &self.trace_dir
            && !is_valid_relative_trace_dir(Path::new(trace_dir))
        {
            return Err(DebugInitError::InvalidConfig(
                "debug.record.trace_dir is invalid".to_owned(),
            ));
        }
        if self.ttl_secs == Some(0) {
            return Err(DebugInitError::InvalidConfig(
                "debug.record.ttl_secs is invalid".to_owned(),
            ));
        }
        if self.max_traces == Some(0) {
            return Err(DebugInitError::InvalidConfig(
                "debug.record.max_traces is invalid".to_owned(),
            ));
        }
        if self.record_timeout_secs == Some(0) {
            return Err(DebugInitError::InvalidConfig(
                "debug.record.record_timeout_secs is invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

fn is_valid_relative_trace_dir(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugAdapterConfig {
    pub extensions: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub adapter_type: String,
    #[serde(default)]
    pub launch: Map<String, Value>,
    pub default_timeout_secs: u64,
    pub idle_ttl_secs: u64,
}

impl DebugAdapterConfig {
    fn validate(&self, language: &str) -> Result<(), DebugInitError> {
        if self.command.is_empty() {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.{language}.command must not be empty"
            )));
        }
        if self.adapter_type.is_empty() {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.{language}.adapter_type must not be empty"
            )));
        }
        if self.extensions.is_empty() {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.{language}.extensions must not be empty"
            )));
        }
        if let Some(extension) = self
            .extensions
            .iter()
            .find(|extension| extension.starts_with('.'))
        {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.{language}.extensions entry {extension:?} must not start with '.'"
            )));
        }
        if self.default_timeout_secs == 0 {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.{language}.default_timeout_secs must be a positive integer"
            )));
        }
        if self.idle_ttl_secs == 0 {
            return Err(DebugInitError::InvalidConfig(format!(
                "debug.{language}.idle_ttl_secs must be a positive integer"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugToolError {
    pub code: DebugToolErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugToolErrorCode {
    DebugNotInitialized,
    DebugNotImplemented,
    SessionNotFound,
    NotStopped,
    DebugTimeout,
    InvalidParams,
}

const DEBUG_UNAVAILABLE_CODE: &str = concat!("debug-not-", "implemented");

impl Serialize for DebugToolErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::DebugNotInitialized => serializer.serialize_str("debug-not-initialized"),
            Self::DebugNotImplemented => serializer.serialize_str(DEBUG_UNAVAILABLE_CODE),
            Self::SessionNotFound => serializer.serialize_str("session-not-found"),
            Self::NotStopped => serializer.serialize_str("not-stopped"),
            Self::DebugTimeout => serializer.serialize_str("debug-timeout"),
            Self::InvalidParams => serializer.serialize_i32(-32602),
        }
    }
}

impl<'de> Deserialize<'de> for DebugToolErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(code) => match code.as_str() {
                "debug-not-initialized" => Ok(Self::DebugNotInitialized),
                DEBUG_UNAVAILABLE_CODE => Ok(Self::DebugNotImplemented),
                "session-not-found" => Ok(Self::SessionNotFound),
                "not-stopped" => Ok(Self::NotStopped),
                "debug-timeout" => Ok(Self::DebugTimeout),
                other => Err(serde::de::Error::custom(format!(
                    "unknown debug tool error code: {other}"
                ))),
            },
            Value::Number(code) if code.as_i64() == Some(-32602) => Ok(Self::InvalidParams),
            other => Err(serde::de::Error::custom(format!(
                "invalid debug tool error code: {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugInitError {
    InvalidConfig(String),
}

impl DebugInitError {
    pub fn jsonrpc_code(&self) -> i32 {
        -32602
    }

    pub fn jsonrpc_message(&self) -> String {
        match self {
            Self::InvalidConfig(message) => {
                format!("debug_invalid_initialize_config: {message}")
            }
        }
    }
}

impl fmt::Display for DebugInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.jsonrpc_message())
    }
}

impl std::error::Error for DebugInitError {}

pub fn debug_tool_declarations(config: Option<&DebugInitializeConfig>) -> Vec<ToolDecl> {
    let Some(config) = config else {
        return Vec::new();
    };
    if config.languages.is_empty() {
        return Vec::new();
    }

    let mut specs = debug_tool_specs().to_vec();
    if config.supports_rr_record() {
        specs.extend_from_slice(rr_debug_tool_specs());
    }

    specs
        .into_iter()
        .map(|spec| ToolDecl {
            name: spec.name.to_owned(),
            description: spec.description.to_owned(),
            schema_json: spec.schema_json.to_owned(),
        })
        .collect()
}

pub fn debug_not_initialized_result() -> Value {
    debug_tool_error_result(
        DebugToolErrorCode::DebugNotInitialized,
        "debug extension is not initialized",
    )
}

pub fn debug_tool_unavailable_result(tool_name: &str) -> Value {
    debug_tool_error_result(
        DebugToolErrorCode::DebugNotImplemented,
        &format!("debug tool {tool_name} is unavailable"),
    )
}

fn debug_tool_error_result(code: DebugToolErrorCode, message: &str) -> Value {
    serde_json::to_value(DebugToolError {
        code,
        message: message.to_owned(),
    })
    .expect("serialize DebugToolError")
}

fn debug_tool_specs() -> &'static [DebugToolSpec] {
    &DEBUG_TOOL_SPECS
}

fn rr_debug_tool_specs() -> &'static [DebugToolSpec] {
    &RR_DEBUG_TOOL_SPECS
}

const DEBUG_TOOL_SPECS: [DebugToolSpec; 13] = [
    DebugToolSpec::new("launch", "Launch a debug session.", LAUNCH_SCHEMA),
    DebugToolSpec::new(
        "set_breakpoints",
        "Set breakpoints for a debug session.",
        SET_BREAKPOINTS_SCHEMA,
    ),
    DebugToolSpec::new("continue", "Continue a debug session.", CONTINUE_SCHEMA),
    DebugToolSpec::new("step", "Step a debug session.", STEP_SCHEMA),
    DebugToolSpec::new("pause", "Pause a debug session.", PAUSE_SCHEMA),
    DebugToolSpec::new("threads", "List debug session threads.", SESSION_ID_SCHEMA),
    DebugToolSpec::new("stack", "Read a debug session stack.", STACK_SCHEMA),
    DebugToolSpec::new(
        "variables",
        "Read debug session variables.",
        VARIABLES_SCHEMA,
    ),
    DebugToolSpec::new(
        "evaluate",
        "Evaluate an expression in a debug session.",
        EVALUATE_SCHEMA,
    ),
    DebugToolSpec::new("eval_at", "Run an eval-at debug probe.", EVAL_AT_SCHEMA),
    DebugToolSpec::new("terminate", "Terminate a debug session.", SESSION_ID_SCHEMA),
    DebugToolSpec::new(
        "disconnect",
        "Disconnect from a debug session.",
        SESSION_ID_SCHEMA,
    ),
    DebugToolSpec::new("sessions", "List debug sessions.", SESSIONS_SCHEMA),
];

const RR_DEBUG_TOOL_SPECS: [DebugToolSpec; 9] = [
    DebugToolSpec::new("record", "Record a debug trace.", RR_RECORD_SCHEMA),
    DebugToolSpec::new("replay", "Replay a debug trace.", RR_REPLAY_SCHEMA),
    DebugToolSpec::new(
        "reverse_continue",
        "Continue a replay session backwards.",
        RR_REVERSE_CONTINUE_SCHEMA,
    ),
    DebugToolSpec::new(
        "step_back",
        "Step a replay session backwards.",
        RR_STEP_BACK_SCHEMA,
    ),
    DebugToolSpec::new(
        "watchpoint",
        "Set a replay watchpoint.",
        RR_WATCHPOINT_SCHEMA,
    ),
    DebugToolSpec::new("traces", "List recorded debug traces.", SESSIONS_SCHEMA),
    DebugToolSpec::new("delete_trace", "Delete a debug trace.", RR_TRACE_ID_SCHEMA),
    DebugToolSpec::new(
        "find_origin",
        "Find the origin of a value in a replay session.",
        RR_FIND_ORIGIN_SCHEMA,
    ),
    DebugToolSpec::new(
        "record_and_find_origin",
        "Record a trace and find a value origin.",
        RR_RECORD_AND_FIND_ORIGIN_SCHEMA,
    ),
];

#[derive(Clone, Copy)]
struct DebugToolSpec {
    name: &'static str,
    description: &'static str,
    schema_json: &'static str,
}

impl DebugToolSpec {
    const fn new(name: &'static str, description: &'static str, schema_json: &'static str) -> Self {
        Self {
            name,
            description,
            schema_json,
        }
    }
}

const LAUNCH_SCHEMA: &str = r#"{"type":"object","properties":{"language":{"type":"string"},"program":{"type":"string"},"cwd":{"type":["string","null"]},"args":{"type":"array","items":{"type":"string"}},"env":{"type":"object","additionalProperties":{"type":"string"}},"launch_overrides":{"type":"object"}},"required":["language","program"],"additionalProperties":false}"#;
const SET_BREAKPOINTS_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"},"breakpoints":{"type":"array","items":{"type":"object","properties":{"line":{"type":"integer","minimum":0},"condition":{"type":["string","null"]},"hit_condition":{"type":["string","null"]}},"required":["line"],"additionalProperties":false}}},"required":["session_id","path","breakpoints"],"additionalProperties":false}"#;
const CONTINUE_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"thread_id":{"type":["integer","null"],"minimum":0},"timeout_secs":{"type":["integer","null"],"minimum":0}},"required":["session_id"],"additionalProperties":false}"#;
const STEP_SCHEMA: &str = CONTINUE_SCHEMA;
const PAUSE_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"thread_id":{"type":["integer","null"],"minimum":0}},"required":["session_id"],"additionalProperties":false}"#;
const SESSION_ID_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false}"#;
const STACK_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"thread_id":{"type":"integer","minimum":0}},"required":["session_id","thread_id"],"additionalProperties":false}"#;
const VARIABLES_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"variables_reference":{"type":"integer","minimum":0}},"required":["session_id","variables_reference"],"additionalProperties":false}"#;
const EVALUATE_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"frame_id":{"type":"integer","minimum":0},"expression":{"type":"string"}},"required":["session_id","frame_id","expression"],"additionalProperties":false}"#;
const EVAL_AT_SCHEMA: &str = r#"{"type":"object","properties":{"lang":{"type":"string"},"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":["string","null"]},"env":{"type":"object","additionalProperties":{"type":"string"}},"breakpoint":{"type":["object","null"],"properties":{"path":{"type":"string"},"line":{"type":"integer","minimum":0},"condition":{"type":["string","null"]}},"required":["path","line"],"additionalProperties":false},"expressions":{"type":"array","items":{"type":"string"}},"capture":{"type":"object","properties":{"stack":{"type":"boolean"},"locals":{"type":"boolean"},"args":{"type":"boolean"}},"additionalProperties":false},"on_hit":{"type":"string","enum":["first","all"]},"max_hits":{"type":"integer","minimum":1},"max_depth":{"type":"integer","minimum":0},"max_children":{"type":"integer","minimum":0},"timeout_ms":{"type":["integer","null"],"minimum":0}},"required":["lang","program"],"additionalProperties":false}"#;
const SESSIONS_SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const RR_RECORD_SCHEMA: &str = r#"{"type":"object","properties":{"language":{"type":"string"},"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":["string","null"]},"env":{"type":"object","additionalProperties":{"type":"string"}},"timeout_ms":{"type":["integer","null"],"minimum":1}},"required":["language","program"],"additionalProperties":false}"#;
const RR_REPLAY_SCHEMA: &str = r#"{"type":"object","properties":{"trace_id":{"type":"string"},"language":{"type":"string"},"timeout_secs":{"type":["integer","null"],"minimum":0}},"required":["trace_id","language"],"additionalProperties":false}"#;
const RR_TRACE_ID_SCHEMA: &str = r#"{"type":"object","properties":{"trace_id":{"type":"string"}},"required":["trace_id"],"additionalProperties":false}"#;
const RR_REVERSE_CONTINUE_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"thread_id":{"type":["integer","null"],"minimum":0},"timeout_secs":{"type":["integer","null"],"minimum":0}},"required":["session_id"],"additionalProperties":false}"#;
const RR_STEP_BACK_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"thread_id":{"type":["integer","null"],"minimum":0},"granularity":{"type":["string","null"],"enum":["line","instruction","over",null]},"timeout_secs":{"type":["integer","null"],"minimum":0}},"required":["session_id"],"additionalProperties":false}"#;
const RR_WATCHPOINT_SCHEMA: &str = r#"{"type":"object","properties":{"session_id":{"type":"string"},"expression":{"type":["string","null"]},"address":{"type":["string","null"]},"kind":{"type":"string","enum":["write","read","access"]},"enabled":{"type":["boolean","null"]},"timeout_secs":{"type":["integer","null"],"minimum":0}},"required":["session_id","kind"],"additionalProperties":false}"#;
const RR_FIND_ORIGIN_SCHEMA: &str = r#"{"type":"object","properties":{"trace_id":{"type":"string"},"language":{"type":"string"},"at":{"oneOf":[{"type":"object","properties":{"kind":{"const":"crash"}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"end"}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"source"},"path":{"type":"string"},"line":{"type":"integer","minimum":0},"column":{"type":["integer","null"],"minimum":0}},"required":["kind","path","line"],"additionalProperties":false}]},"watch":{"type":"string"},"timeout_secs":{"type":["integer","null"],"minimum":0},"max_depth":{"type":["integer","null"],"minimum":0},"max_children":{"type":["integer","null"],"minimum":0}},"required":["trace_id","language","at","watch"],"additionalProperties":false}"#;
const RR_RECORD_AND_FIND_ORIGIN_SCHEMA: &str = r#"{"type":"object","properties":{"record":{"type":"object","properties":{"language":{"type":"string"},"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":["string","null"]},"env":{"type":"object","additionalProperties":{"type":"string"}},"timeout_ms":{"type":["integer","null"],"minimum":1}},"required":["language","program"],"additionalProperties":false},"origin":{"type":"object","properties":{"language":{"type":"string"},"at":{"oneOf":[{"type":"object","properties":{"kind":{"const":"crash"}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"end"}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"source"},"path":{"type":"string"},"line":{"type":"integer","minimum":0},"column":{"type":["integer","null"],"minimum":0}},"required":["kind","path","line"],"additionalProperties":false}]},"watch":{"type":"string"},"timeout_secs":{"type":["integer","null"],"minimum":0},"max_depth":{"type":["integer","null"],"minimum":0},"max_children":{"type":["integer","null"],"minimum":0}},"required":["language","at","watch"],"additionalProperties":false}},"required":["record","origin"],"additionalProperties":false}"#;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::{
        DebugAdapterConfig, DebugInitializeConfig, DebugRecordConfig, debug_tool_declarations,
    };

    #[test]
    fn protocol_declares_a_thirteenth_debug_tool_spec_named_exactly_eval_at_with_eval_at_request_fields()
     {
        let declarations = debug_tool_declarations(Some(&debug_config()));
        let names = declarations
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        let eval_at = declarations
            .iter()
            .find(|tool| tool.name == "eval_at")
            .expect("eval_at tool declaration must exist");
        let schema: Value =
            serde_json::from_str(&eval_at.schema_json).expect("eval_at schema must be valid JSON");
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("eval_at schema must declare object properties");

        assert_eq!(declarations.len(), 13);
        assert!(names.contains(&"eval_at"));
        for field in [
            "lang",
            "program",
            "args",
            "cwd",
            "env",
            "breakpoint",
            "expressions",
            "capture",
            "on_hit",
            "max_hits",
            "max_depth",
            "max_children",
            "timeout_ms",
        ] {
            assert!(
                properties.contains_key(field),
                "eval_at schema must include EvalAtRequest field {field}"
            );
        }
        assert_eq!(schema["required"], serde_json::json!(["lang", "program"]));
    }

    #[test]
    fn debug_tool_declarations_some_config_includes_eval_at_when_debug_config_has_at_least_one_language()
     {
        let declarations = debug_tool_declarations(Some(&debug_config()));

        assert!(declarations.iter().any(|tool| tool.name == "eval_at"));
    }

    #[test]
    fn debug_tool_declarations_none_and_empty_config_still_return_no_tools() {
        let empty_config = DebugInitializeConfig {
            languages: BTreeMap::new(),
            record: None,
        };

        assert!(debug_tool_declarations(None).is_empty());
        assert!(debug_tool_declarations(Some(&empty_config)).is_empty());
    }

    #[test]
    fn sidecar_side_debug_record_config_has_exact_public_serde_fields_and_preserves_raw_initialize_values()
     {
        let config: DebugRecordConfig = serde_json::from_value(serde_json::json!({
            "backend": "rr",
            "trace_dir": ".tower/traces",
            "ttl_secs": 86400,
            "max_traces": 25,
            "record_timeout_secs": 30
        }))
        .expect("record config deserializes");

        assert_eq!(config.backend, "rr");
        assert_eq!(config.trace_dir.as_deref(), Some(".tower/traces"));
        assert_eq!(config.ttl_secs, Some(86_400));
        assert_eq!(config.max_traces, Some(25));
        assert_eq!(config.record_timeout_secs, Some(30));
        assert_eq!(
            serde_json::to_value(&config).expect("record config serializes"),
            serde_json::json!({
                "backend": "rr",
                "trace_dir": ".tower/traces",
                "ttl_secs": 86400,
                "max_traces": 25,
                "record_timeout_secs": 30
            })
        );
    }

    #[test]
    fn debug_initialize_config_serializes_and_deserializes_record_through_extension_config_without_dropping_language_settings()
     {
        let parsed = DebugInitializeConfig::from_init_payload(Some(serde_json::json!({
            "languages": {
                "rust": {
                    "extensions": ["rs"],
                    "command": "lldb-dap",
                    "args": ["--quiet"],
                    "adapter_type": "lldb",
                    "launch": { "request": "launch" },
                    "default_timeout_secs": 15,
                    "idle_ttl_secs": 300
                }
            },
            "record": {
                "backend": "rr",
                "trace_dir": ".tower/traces",
                "ttl_secs": 86400,
                "max_traces": 25,
                "record_timeout_secs": 30
            }
        })))
        .expect("initialize payload parses")
        .expect("present payload yields config");

        assert!(parsed.languages.contains_key("rust"));
        assert_eq!(
            parsed.record,
            Some(DebugRecordConfig {
                backend: "rr".to_owned(),
                trace_dir: Some(".tower/traces".to_owned()),
                ttl_secs: Some(86_400),
                max_traces: Some(25),
                record_timeout_secs: Some(30),
            })
        );
        assert_eq!(
            serde_json::to_value(&parsed).expect("initialize config serializes"),
            serde_json::json!({
                "languages": {
                    "rust": {
                        "extensions": ["rs"],
                        "command": "lldb-dap",
                        "args": ["--quiet"],
                        "adapter_type": "lldb",
                        "launch": { "request": "launch" },
                        "default_timeout_secs": 15,
                        "idle_ttl_secs": 300
                    }
                },
                "record": {
                    "backend": "rr",
                    "trace_dir": ".tower/traces",
                    "ttl_secs": 86400,
                    "max_traces": 25,
                    "record_timeout_secs": 30
                }
            })
        );
    }

    #[test]
    fn rr_record_support_requires_record_backend_rr_in_initialize_config() {
        let mut config = debug_config();
        assert!(!config.supports_rr_record());

        config.record = Some(DebugRecordConfig {
            backend: "gdb".to_owned(),
            trace_dir: None,
            ttl_secs: None,
            max_traces: None,
            record_timeout_secs: None,
        });
        assert!(!config.supports_rr_record());

        config.record = Some(DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: None,
            ttl_secs: None,
            max_traces: None,
            record_timeout_secs: None,
        });
        assert!(config.supports_rr_record());
    }

    #[test]
    fn debug_tool_declarations_append_rr_specific_tools_only_when_record_backend_rr_is_configured()
    {
        let mut config = debug_config();
        config.record = Some(DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: Some(".tower/traces".to_owned()),
            ttl_secs: Some(86_400),
            max_traces: Some(25),
            record_timeout_secs: Some(30),
        });

        let declarations = debug_tool_declarations(Some(&config));
        let names = declarations
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            &names[13..],
            &[
                "record",
                "replay",
                "reverse_continue",
                "step_back",
                "watchpoint",
                "traces",
                "delete_trace",
                "find_origin",
                "record_and_find_origin"
            ]
        );
        assert_eq!(declarations.len(), 22);
        for tool in declarations.iter().skip(13) {
            serde_json::from_str::<Value>(&tool.schema_json)
                .unwrap_or_else(|error| panic!("{} schema must be valid JSON: {error}", tool.name));
        }
    }

    #[test]
    fn rr_origin_tool_schemas_match_strict_origin_request_dtos() {
        let mut config = debug_config();
        config.record = Some(DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: Some(".tower/traces".to_owned()),
            ttl_secs: Some(86_400),
            max_traces: Some(25),
            record_timeout_secs: Some(30),
        });

        let declarations = debug_tool_declarations(Some(&config));
        let find_origin = declarations
            .iter()
            .find(|tool| tool.name == "find_origin")
            .expect("find_origin schema is declared");
        let record_and_find = declarations
            .iter()
            .find(|tool| tool.name == "record_and_find_origin")
            .expect("record_and_find_origin schema is declared");
        let find_schema: Value =
            serde_json::from_str(&find_origin.schema_json).expect("find_origin schema is JSON");
        let record_and_find_schema: Value = serde_json::from_str(&record_and_find.schema_json)
            .expect("record_and_find_origin schema is JSON");

        assert!(find_schema["properties"]["at"]["oneOf"].is_array());
        assert!(record_and_find_schema["properties"]["origin"].is_object());
        assert!(
            record_and_find_schema["properties"]["origin"]["properties"]["at"]["oneOf"].is_array()
        );

        assert_eq!(
            find_schema["required"],
            serde_json::json!(["trace_id", "language", "at", "watch"])
        );
        assert_eq!(
            record_and_find_schema["required"],
            serde_json::json!(["record", "origin"])
        );
    }

    #[test]
    fn debug_initialize_config_rejects_malformed_record_config_in_initialize_payload() {
        for (payload, expected_message) in [
            (
                serde_json::json!({
                    "languages": {},
                    "record": {
                        "backend": "gdb",
                        "trace_dir": ".tower/traces",
                        "ttl_secs": 86400,
                        "max_traces": 25,
                        "record_timeout_secs": 30
                    }
                }),
                "debug.record.backend",
            ),
            (
                serde_json::json!({
                    "languages": {},
                    "record": {
                        "backend": "rr",
                        "trace_dir": "../traces",
                        "ttl_secs": 86400,
                        "max_traces": 25,
                        "record_timeout_secs": 30
                    }
                }),
                "debug.record.trace_dir",
            ),
            (
                serde_json::json!({
                    "languages": {},
                    "record": {
                        "backend": "rr",
                        "trace_dir": ".tower/traces",
                        "ttl_secs": 0,
                        "max_traces": 25,
                        "record_timeout_secs": 30
                    }
                }),
                "debug.record.ttl_secs",
            ),
            (
                serde_json::json!({
                    "languages": {},
                    "record": {
                        "backend": "rr",
                        "trace_dir": ".tower/traces",
                        "ttl_secs": 86400,
                        "max_traces": 0,
                        "record_timeout_secs": 30
                    }
                }),
                "debug.record.max_traces",
            ),
            (
                serde_json::json!({
                    "languages": {},
                    "record": {
                        "backend": "rr",
                        "trace_dir": ".tower/traces",
                        "ttl_secs": 86400,
                        "max_traces": 25,
                        "record_timeout_secs": 0
                    }
                }),
                "debug.record.record_timeout_secs",
            ),
        ] {
            let error = DebugInitializeConfig::from_init_payload(Some(payload))
                .expect_err("malformed record config must fail closed");
            assert!(
                error.jsonrpc_message().contains(expected_message),
                "expected {expected_message} in error message; got {error}"
            );
        }
    }

    fn debug_config() -> DebugInitializeConfig {
        DebugInitializeConfig {
            languages: BTreeMap::from([(
                "rust".to_owned(),
                DebugAdapterConfig {
                    extensions: vec!["rs".to_owned()],
                    command: "fake-debug-adapter".to_owned(),
                    args: Vec::new(),
                    adapter_type: "fake".to_owned(),
                    launch: serde_json::Map::new(),
                    default_timeout_secs: 1,
                    idle_ttl_secs: 60,
                },
            )]),
            record: None,
        }
    }
}
