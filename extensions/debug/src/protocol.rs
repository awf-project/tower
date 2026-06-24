#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

use extension_protocol::ToolDecl;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugInitializeConfig {
    pub languages: BTreeMap<String, DebugAdapterConfig>,
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
        self.languages.is_empty()
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
        Ok(())
    }
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
    if config.is_none_or(DebugInitializeConfig::is_empty) {
        return Vec::new();
    }

    debug_tool_specs()
        .iter()
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

const DEBUG_TOOL_SPECS: [DebugToolSpec; 12] = [
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
    DebugToolSpec::new("terminate", "Terminate a debug session.", SESSION_ID_SCHEMA),
    DebugToolSpec::new(
        "disconnect",
        "Disconnect from a debug session.",
        SESSION_ID_SCHEMA,
    ),
    DebugToolSpec::new("sessions", "List debug sessions.", SESSIONS_SCHEMA),
];

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
const SESSIONS_SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
