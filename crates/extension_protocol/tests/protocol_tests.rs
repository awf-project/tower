//! Integration tests encoding the Definition of Done for spec 21.
//!
//! TDD sequence (spec 21):
//! 1. RED:  round-trip serde for Request/Response/Event
//! 2. GREEN
//! 3. RED:  ExtensionManifest TOML parse including activation
//! 4. GREEN
//! 5. RED:  malformed input → ProtocolError, no panic
//! 6. GREEN
//!
//! All tests here are integration-level — they import the public API only.

use extension_protocol::{
    Activation, Capability, Event, ExtensionFault, ExtensionManifest, HostCall, InitParams,
    InitResult, PROTOCOL_VERSION, ProtocolError, Request, Response, ToolDecl,
};

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn shipped_debug_manifest() -> ExtensionManifest {
    let path = workspace_root().join("extensions/debug/extension.toml");
    let contents = std::fs::read_to_string(&path).expect("read shipped debug extension manifest");
    toml::from_str(&contents).expect("shipped debug extension manifest must parse")
}

fn toml_string_array<'a>(value: &'a toml::Value, key: &str) -> Vec<&'a str> {
    value
        .get("workspace")
        .and_then(|workspace| workspace.get(key))
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("workspace.{key} must be an array"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("workspace.{key} entries must be strings"))
        })
        .collect()
}

// ── AC1: Round-trip serde for all protocol message types ─────────────────────

#[test]
fn request_initialize_round_trips_json() {
    let req = Request::Initialize(InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "tower-host/0.1.0".to_owned(),
        extension_config: Some(serde_json::json!({
            "languages": {
                "rust": {
                    "extensions": ["rs"],
                    "command": "lldb-dap",
                    "args": ["--quiet"],
                    "adapter_type": "lldb",
                    "launch": {},
                    "default_timeout_secs": 15,
                    "idle_ttl_secs": 300
                }
            }
        })),
    });
    let json = serde_json::to_string(&req).expect("serialize");
    let decoded: Request = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Request::Initialize(p) => {
            assert_eq!(p.protocol_version, PROTOCOL_VERSION);
            assert_eq!(p.client_info, "tower-host/0.1.0");
            assert_eq!(
                p.extension_config
                    .as_ref()
                    .and_then(|config| config.pointer("/languages/rust/command"))
                    .and_then(serde_json::Value::as_str),
                Some("lldb-dap")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn init_params_has_optional_per_extension_config_payload_field_with_stable_serde_naming() {
    let params = InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "tower-host/0.1.0".to_owned(),
        extension_config: None,
    };

    let value = serde_json::to_value(params).expect("serialize InitParams");

    assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(value["client_info"], "tower-host/0.1.0");
    assert!(
        value.get("extension_config").is_none(),
        "extension_config must keep optional initialize params backward compatible"
    );
}

#[test]
fn init_params_optional_extension_config_round_trips() {
    let params = InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "tower-host/0.1.0".to_owned(),
        extension_config: Some(serde_json::json!({
            "languages": {
                "rust": {
                    "extensions": ["rs"],
                    "command": "lldb-dap",
                    "args": ["--quiet"],
                    "adapter_type": "lldb",
                    "default_timeout_secs": 15,
                    "idle_ttl_secs": 300
                }
            }
        })),
    };

    let json = serde_json::to_string(&params).expect("serialize InitParams");
    let value: serde_json::Value = serde_json::from_str(&json).expect("deserialize JSON value");
    let decoded: InitParams =
        serde_json::from_value(value.clone()).expect("deserialize InitParams");

    assert!(value.get("extension_config").is_some());
    assert_eq!(
        decoded
            .extension_config
            .as_ref()
            .and_then(|config| config.pointer("/languages/rust/command"))
            .and_then(serde_json::Value::as_str),
        Some("lldb-dap")
    );
}

#[test]
fn initialize_params_without_the_optional_config_field_still_deserialize_for_existing_extensions() {
    let json = serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "client_info": "tower-host/0.1.0"
    });

    let decoded: InitParams = serde_json::from_value(json).expect("deserialize InitParams");

    assert_eq!(decoded.extension_config, None);
}

#[test]
fn config_payload_type_supports_serialized_debug_config_data_without_core_engine_dependency() {
    let debug_config = serde_json::json!({
        "languages": {
            "rust": {
                "extensions": ["rs"],
                "command": "lldb-dap",
                "args": ["--quiet"],
                "adapter_type": "lldb",
                "launch": {
                    "request": "launch",
                    "program": "target/debug/tower"
                },
                "default_timeout_secs": 15,
                "idle_ttl_secs": 300
            }
        }
    });

    let params = InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "tower-host/0.1.0".to_owned(),
        extension_config: Some(debug_config),
    };

    let decoded: InitParams =
        serde_json::from_value(serde_json::to_value(params).expect("serialize InitParams"))
            .expect("deserialize InitParams");

    assert_eq!(
        decoded
            .extension_config
            .as_ref()
            .and_then(|config| config.pointer("/languages/rust/adapter_type"))
            .and_then(serde_json::Value::as_str),
        Some("lldb")
    );
    assert_eq!(
        decoded
            .extension_config
            .as_ref()
            .and_then(|config| config.pointer("/languages/rust/launch/program"))
            .and_then(serde_json::Value::as_str),
        Some("target/debug/tower")
    );
}

#[test]
fn request_invoke_tool_round_trips_json() {
    let req = Request::InvokeTool {
        name: "ast_outline".to_owned(),
        params: serde_json::json!({"path": "/src/main.rs"}),
    };
    let json = serde_json::to_string(&req).expect("serialize");
    let decoded: Request = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Request::InvokeTool { name, params } => {
            assert_eq!(name, "ast_outline");
            assert_eq!(params["path"], "/src/main.rs");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn request_deliver_event_round_trips_json() {
    let req = Request::DeliverEvent(Event::FileIndexed {
        file_id: 42,
        path: "/src/main.rs".to_owned(),
    });
    let json = serde_json::to_string(&req).expect("serialize");
    let decoded: Request = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Request::DeliverEvent(Event::FileIndexed { file_id, path }) => {
            assert_eq!(file_id, 42);
            assert_eq!(path, "/src/main.rs");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn request_shutdown_round_trips_json() {
    let req = Request::Shutdown;
    let json = serde_json::to_string(&req).expect("serialize");
    let decoded: Request = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, Request::Shutdown));
}

#[test]
fn response_initialized_round_trips_json() {
    let resp = Response::Initialized(InitResult {
        tools: vec![ToolDecl {
            name: "ast_outline".to_owned(),
            description: "Get AST outline".to_owned(),
            schema_json: "{}".to_owned(),
        }],
        events: vec!["event/fileIndexed".to_owned()],
        capabilities: vec![Capability::ReadFile],
    });
    let json = serde_json::to_string(&resp).expect("serialize");
    let decoded: Response = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Response::Initialized(r) => {
            assert_eq!(r.tools.len(), 1);
            assert_eq!(r.tools[0].name, "ast_outline");
            assert_eq!(r.events.len(), 1);
            assert_eq!(r.capabilities.len(), 1);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn response_tool_result_round_trips_json() {
    let resp = Response::ToolResult(serde_json::json!({"outline": []}));
    let json = serde_json::to_string(&resp).expect("serialize");
    let decoded: Response = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Response::ToolResult(v) => {
            assert!(v["outline"].is_array());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn response_ack_round_trips_json() {
    let resp = Response::Ack;
    let json = serde_json::to_string(&resp).expect("serialize");
    let decoded: Response = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, Response::Ack));
}

#[test]
fn response_error_round_trips_json() {
    let resp = Response::Error(ProtocolError {
        code: -32600,
        message: "Invalid Request".to_owned(),
        data: None,
    });
    let json = serde_json::to_string(&resp).expect("serialize");
    let decoded: Response = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Response::Error(e) => {
            assert_eq!(e.code, -32600);
            assert_eq!(e.message, "Invalid Request");
            assert!(e.data.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn event_file_indexed_round_trips_json() {
    let event = Event::FileIndexed {
        file_id: 1,
        path: "/lib.rs".to_owned(),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: Event = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Event::FileIndexed { file_id, path } => {
            assert_eq!(file_id, 1);
            assert_eq!(path, "/lib.rs");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn event_file_changed_round_trips_json() {
    let event = Event::FileChanged {
        file_id: 7,
        path: "/main.rs".to_owned(),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: Event = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Event::FileChanged { file_id, path } => {
            assert_eq!(file_id, 7);
            assert_eq!(path, "/main.rs");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn host_call_read_file_round_trips_json() {
    let call = HostCall::ReadFile {
        path: "/src/lib.rs".to_owned(),
    };
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        HostCall::ReadFile { path } => assert_eq!(path, "/src/lib.rs"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn host_call_list_files_round_trips_json() {
    let call = HostCall::ListFiles;
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, HostCall::ListFiles));
}

#[test]
fn host_call_index_get_round_trips_json() {
    let call = HostCall::IndexGet {
        key: "ast/main.rs".to_owned(),
    };
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        HostCall::IndexGet { key } => assert_eq!(key, "ast/main.rs"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn host_call_index_put_round_trips_json() {
    let call = HostCall::IndexPut {
        key: "ast/main.rs".to_owned(),
        bytes: vec![1, 2, 3, 4],
    };
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        HostCall::IndexPut { key, bytes } => {
            assert_eq!(key, "ast/main.rs");
            assert_eq!(bytes, vec![1u8, 2, 3, 4]);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn host_call_request_format_round_trips_json() {
    let call = HostCall::RequestFormat {
        path: "/src/main.rs".to_owned(),
    };
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        HostCall::RequestFormat { path } => assert_eq!(path, "/src/main.rs"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn host_call_log_round_trips_json() {
    let call = HostCall::Log {
        level: "info".to_owned(),
        msg: "hello from extension".to_owned(),
    };
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        HostCall::Log { level, msg } => {
            assert_eq!(level, "info");
            assert_eq!(msg, "hello from extension");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn existing_host_call_and_manifest_serialization_contracts_stay_unchanged() {
    let read_file = serde_json::to_value(HostCall::ReadFile {
        path: "/src/lib.rs".to_owned(),
    })
    .expect("serialize readFile host call");
    assert_eq!(
        read_file,
        serde_json::json!({
            "type": "ReadFile",
            "path": "/src/lib.rs"
        })
    );

    let request_format = serde_json::to_value(HostCall::RequestFormat {
        path: "/src/main.rs".to_owned(),
    })
    .expect("serialize requestFormat host call");
    assert_eq!(
        request_format,
        serde_json::json!({
            "type": "RequestFormat",
            "path": "/src/main.rs"
        })
    );

    let log = serde_json::to_value(HostCall::Log {
        level: "info".to_owned(),
        msg: "hello from extension".to_owned(),
    })
    .expect("serialize log host call");
    assert_eq!(
        log,
        serde_json::json!({
            "type": "Log",
            "level": "info",
            "msg": "hello from extension"
        })
    );

    let toml = r#"
        name = "fmt"
        version = "0.1.0"
        command = ["fmt_extension"]
        activation = "lazy"

        [capabilities]
        required = ["read_file", "list_files", "request_format", "log"]
    "#;
    let manifest: ExtensionManifest = toml::from_str(toml).expect("parse existing capabilities");
    assert_eq!(
        manifest.capabilities.required,
        vec!["read_file", "list_files", "request_format", "log"]
    );
    let manifest_value = toml::Value::try_from(&manifest).expect("serialize existing manifest");
    let serialized_required = manifest_value
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("required"))
        .and_then(toml::Value::as_array)
        .expect("serialized manifest capabilities.required must stay present")
        .iter()
        .map(|capability| {
            capability
                .as_str()
                .expect("serialized manifest capability names must be strings")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        serialized_required,
        vec!["read_file", "list_files", "request_format", "log"]
    );

    let capabilities = [
        (Capability::ReadFile, "\"read_file\""),
        (Capability::ListFiles, "\"list_files\""),
        (Capability::RequestFormat, "\"request_format\""),
        (Capability::Log, "\"log\""),
    ];
    for (capability, expected_json) in capabilities {
        let json = serde_json::to_string(&capability).expect("serialize existing capability");
        assert_eq!(json, expected_json);
        let decoded: Capability =
            serde_json::from_str(expected_json).expect("deserialize existing capability");
        assert_eq!(decoded, capability);
    }
}

// ── AC2: ExtensionManifest TOML parse ────────────────────────────────────────

#[test]
fn extension_manifest_parses_eager_activation() {
    let toml = r#"
        name = "ast"
        version = "0.1.0"
        command = ["./ast-extension"]
        activation = "eager"

        [[tools]]
        name = "ast_outline"
        description = "Get AST outline"
        schema_json = "{}"

        [events]
        subscribe = ["event/fileIndexed"]

        [capabilities]
        required = ["read_file"]
    "#;
    let manifest: ExtensionManifest = toml::from_str(toml).expect("parse");
    assert_eq!(manifest.name, "ast");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.command, vec!["./ast-extension"]);
    assert!(matches!(manifest.activation, Activation::Eager));
    assert_eq!(manifest.tools.len(), 1);
    assert_eq!(manifest.tools[0].name, "ast_outline");
}

#[test]
fn extension_manifest_parses_lazy_activation() {
    let toml = r#"
        name = "lsp"
        version = "0.2.0"
        command = ["rust-analyzer"]
        activation = "lazy"
    "#;
    let manifest: ExtensionManifest = toml::from_str(toml).expect("parse");
    assert_eq!(manifest.name, "lsp");
    assert!(matches!(manifest.activation, Activation::Lazy));
}

#[test]
fn extension_manifest_tools_populate_correctly() {
    let toml = r#"
        name = "hello"
        version = "0.1.0"
        command = ["./hello"]
        activation = "eager"

        [[tools]]
        name = "greet"
        description = "Greet the user"
        schema_json = '{"type":"object"}'
    "#;
    let manifest: ExtensionManifest = toml::from_str(toml).expect("parse");
    assert_eq!(manifest.tools.len(), 1);
    assert_eq!(manifest.tools[0].name, "greet");
    assert_eq!(manifest.tools[0].description, "Greet the user");
    assert_eq!(manifest.tools[0].schema_json, r#"{"type":"object"}"#);
}

#[test]
fn extension_manifest_empty_tools_is_valid() {
    let toml = r#"
        name = "minimal"
        version = "0.0.1"
        command = ["./minimal"]
        activation = "lazy"
    "#;
    let manifest: ExtensionManifest = toml::from_str(toml).expect("parse");
    assert!(manifest.tools.is_empty());
}

#[test]
fn workspace_cargo_toml_includes_extensions_debug_as_a_workspace_default_member_consistently_with_existing_extension_crates()
 {
    let path = workspace_root().join("Cargo.toml");
    let contents = std::fs::read_to_string(&path).expect("read workspace Cargo.toml");
    let cargo: toml::Value = toml::from_str(&contents).expect("workspace Cargo.toml must parse");
    let members = toml_string_array(&cargo, "members");
    let default_members = toml_string_array(&cargo, "default-members");

    assert!(
        members.contains(&"extensions/debug"),
        "workspace members must include extensions/debug; got {members:?}"
    );
    assert!(
        default_members.contains(&"extensions/debug"),
        "workspace default-members must include extensions/debug so cargo build --workspace --bins builds debug_extension; got {default_members:?}"
    );
}

#[test]
fn extensions_debug_cargo_toml_defines_a_binary_crate_named_debug_extension_without_dap_runtime_dependencies()
 {
    let path = workspace_root().join("extensions/debug/Cargo.toml");
    let contents = std::fs::read_to_string(&path).expect("read extensions/debug/Cargo.toml");
    let cargo: toml::Value = toml::from_str(&contents).expect("debug Cargo.toml must parse");

    assert_eq!(cargo["package"]["name"].as_str(), Some("debug_extension"));
    assert_eq!(cargo["bin"][0]["name"].as_str(), Some("debug_extension"));
    assert_eq!(cargo["bin"][0]["path"].as_str(), Some("src/main.rs"));

    let dependencies = cargo["dependencies"]
        .as_table()
        .expect("debug dependencies must be a TOML table");
    assert!(
        dependencies.contains_key("extension_protocol"),
        "debug scaffold must depend on extension_protocol for the sidecar wire contract; got {:?}",
        dependencies.keys().collect::<Vec<_>>()
    );

    let runtime_dependencies = [
        "dap",
        "debug-adapter-protocol",
        "lsp-types",
        "notify",
        "rmcp",
        "sled",
        "tokio",
        "tower-lsp",
    ];
    let unexpected = dependencies
        .keys()
        .filter(|dependency| runtime_dependencies.contains(&dependency.as_str()))
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "debug scaffold must not add DAP/session/runtime dependencies before tool behavior is implemented; got {unexpected:?}"
    );
}

#[test]
fn extensions_debug_extension_toml_declares_lazy_activation_and_no_event_subscriptions() {
    let manifest = shipped_debug_manifest();

    assert!(matches!(manifest.activation, Activation::Lazy));
    assert!(
        manifest.events.subscribe.is_empty(),
        "debug extension must not subscribe to workspace file events"
    );
}

#[test]
fn debug_manifest_declares_exactly_the_required_local_tool_names() {
    let manifest = shipped_debug_manifest();
    let tool_names = manifest
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        tool_names,
        vec![
            "launch",
            "set_breakpoints",
            "continue",
            "step",
            "pause",
            "threads",
            "stack",
            "variables",
            "evaluate",
            "terminate",
            "disconnect",
            "sessions",
        ],
    );
}

#[test]
fn debug_manifest_does_not_declare_workspace_write_or_request_apply_edits_capabilities() {
    let manifest = shipped_debug_manifest();
    let forbidden = [
        "request_apply_edits",
        "workspace_write",
        "write_file",
        "create_file",
        "edit_range",
        "global_replace",
        "delete_file",
    ];

    assert!(
        forbidden.iter().all(|capability| !manifest
            .capabilities
            .required
            .iter()
            .any(|required| required == capability)),
        "debug manifest must not request workspace mutation capabilities; got {:?}",
        manifest.capabilities.required
    );
}

#[test]
fn extension_manifest_parses_debug_tools_and_preserves_all_debug_tool_names() {
    let manifest = shipped_debug_manifest();
    let tool_names = manifest
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(manifest.name, "debug");
    assert_eq!(manifest.command, vec!["debug_extension"]);
    assert_eq!(tool_names.len(), 12);
    assert!(tool_names.contains(&"launch"));
    assert!(tool_names.contains(&"set_breakpoints"));
    assert!(tool_names.contains(&"sessions"));
}

// ── AC3: Malformed input → ProtocolError, no panic ───────────────────────────

#[test]
fn malformed_request_returns_error_without_unwind() {
    let bad = r#"{"type":"UnknownMethod","data":{}}"#;
    let result: Result<Request, _> = serde_json::from_str(bad);
    assert!(result.is_err(), "malformed message must fail, not panic");
}

#[test]
fn malformed_response_returns_error_without_unwind() {
    let bad = r#"{"not": "a valid response"}"#;
    let result: Result<Response, _> = serde_json::from_str(bad);
    assert!(result.is_err(), "malformed response must fail, not panic");
}

#[test]
fn malformed_event_returns_error_without_unwind() {
    let bad = r#"{"type":"SomethingElse"}"#;
    let result: Result<Event, _> = serde_json::from_str(bad);
    assert!(result.is_err(), "malformed event must fail, not panic");
}

#[test]
fn incomplete_json_returns_error_without_unwind() {
    let bad = r#"{"type":"Initialize","#;
    let result: Result<Request, _> = serde_json::from_str(bad);
    assert!(result.is_err(), "incomplete JSON must fail, not panic");
}

#[test]
fn protocol_error_has_display() {
    let err = ProtocolError {
        code: -32700,
        message: "Parse error".to_owned(),
        data: None,
    };
    let display = err.to_string();
    assert!(display.contains("Parse error") || display.contains("-32700"));
}

// ── AC4: PROTOCOL_VERSION constant is meaningful ──────────────────────────────

// Compile-time assertion: PROTOCOL_VERSION must be a positive constant.
// Expressed as a const assertion so it is checked at compile time, not
// as a runtime `assert!` that clippy flags as "this assertion has a constant value".
const _: () = assert!(PROTOCOL_VERSION > 0, "PROTOCOL_VERSION must be positive");

#[test]
fn protocol_version_is_nonzero() {
    // The compile-time assertion above is authoritative. This test verifies
    // the constant survives a JSON round-trip (i.e. it is correctly serialised
    // and deserialised as part of InitParams).
    let params = InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "test".to_owned(),
        extension_config: None,
    };
    let json = serde_json::to_string(&params).expect("serialize InitParams");
    let decoded: InitParams = serde_json::from_str(&json).expect("deserialize InitParams");
    // `decoded.protocol_version` is a runtime value (result of deserialisation).
    assert_ne!(
        decoded.protocol_version, 0,
        "PROTOCOL_VERSION must survive JSON round-trip as a positive value"
    );
}

#[test]
fn protocol_version_carried_in_initialize() {
    let req = Request::Initialize(InitParams {
        protocol_version: PROTOCOL_VERSION,
        client_info: "test".to_owned(),
        extension_config: None,
    });
    let json = serde_json::to_string(&req).expect("serialize");
    assert!(
        json.contains(&PROTOCOL_VERSION.to_string()),
        "PROTOCOL_VERSION must appear in the initialize message"
    );
}

// ── ExtensionFault round-trips ────────────────────────────────────────────────

#[test]
fn extension_fault_timeout_round_trips_json() {
    let fault = ExtensionFault::Timeout;
    let json = serde_json::to_string(&fault).expect("serialize");
    let decoded: ExtensionFault = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, ExtensionFault::Timeout));
}

#[test]
fn extension_fault_crashed_round_trips_json() {
    let fault = ExtensionFault::Crashed { code: Some(1) };
    let json = serde_json::to_string(&fault).expect("serialize");
    let decoded: ExtensionFault = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        ExtensionFault::Crashed { code } => assert_eq!(code, Some(1)),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn extension_fault_protocol_error_round_trips_json() {
    let fault = ExtensionFault::ProtocolError {
        message: "bad framing".to_owned(),
    };
    let json = serde_json::to_string(&fault).expect("serialize");
    let decoded: ExtensionFault = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        ExtensionFault::ProtocolError { message } => assert_eq!(message, "bad framing"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn extension_fault_quarantined_round_trips_json() {
    let fault = ExtensionFault::Quarantined;
    let json = serde_json::to_string(&fault).expect("serialize");
    let decoded: ExtensionFault = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(decoded, ExtensionFault::Quarantined));
}

// ── U4: No process/fs/transport code in the crate (structural) ───────────────
// This is enforced at compile-time by the crate's Cargo.toml having no
// std::process / std::fs / tokio / async-std dependency. No runtime test needed.

// ── Capability enum round-trips ───────────────────────────────────────────────

#[test]
fn all_capability_variants_round_trip_json() {
    let caps = [
        Capability::ReadFile,
        Capability::ListFiles,
        Capability::IndexGet,
        Capability::IndexPut,
        Capability::RequestFormat,
        Capability::Log,
        Capability::Notify, // spec 27
    ];
    for cap in &caps {
        let json = serde_json::to_string(cap).expect("serialize");
        let decoded: Capability = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cap, &decoded);
    }
}

// ── Spec 27: FileDeleted event and NotifyResourceUpdated HostCall ─────────────

/// Spec 27 TDD step 1 RED/GREEN: `Event::FileDeleted` round-trips JSON.
#[test]
fn event_file_deleted_round_trips_json() {
    let event = Event::FileDeleted {
        path: "src/lib.rs".to_owned(),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let decoded: Event = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Event::FileDeleted { path } => assert_eq!(path, "src/lib.rs"),
        _ => panic!("expected FileDeleted, got something else"),
    }
}

/// Spec 27: `Event::FileDeleted` delivered in a `Request::DeliverEvent` envelope.
#[test]
fn request_deliver_file_deleted_round_trips_json() {
    let req = Request::DeliverEvent(Event::FileDeleted {
        path: "src/main.rs".to_owned(),
    });
    let json = serde_json::to_string(&req).expect("serialize");
    let decoded: Request = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        Request::DeliverEvent(Event::FileDeleted { path }) => {
            assert_eq!(path, "src/main.rs");
        }
        _ => panic!("expected DeliverEvent(FileDeleted), got: {json}"),
    }
}

/// Spec 27 TDD step 1 RED/GREEN: `HostCall::NotifyResourceUpdated` round-trips JSON.
#[test]
fn host_call_notify_resource_updated_round_trips_json() {
    let call = HostCall::NotifyResourceUpdated {
        uri: "lsp://rust/diagnostics".to_owned(),
    };
    let json = serde_json::to_string(&call).expect("serialize");
    let decoded: HostCall = serde_json::from_str(&json).expect("deserialize");
    match decoded {
        HostCall::NotifyResourceUpdated { uri } => {
            assert_eq!(uri, "lsp://rust/diagnostics");
        }
        _ => panic!("expected NotifyResourceUpdated, got: {json}"),
    }
}

/// Spec 27: `Capability::Notify` serializes to `"notify"`.
#[test]
fn capability_notify_serializes_correctly() {
    let cap = Capability::Notify;
    let json = serde_json::to_string(&cap).expect("serialize");
    assert_eq!(json, r#""notify""#);
    let decoded: Capability = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, Capability::Notify);
}

// ── ProtocolError with data field ─────────────────────────────────────────────

#[test]
fn protocol_error_with_data_round_trips_json() {
    let err = ProtocolError {
        code: -32602,
        message: "Invalid params".to_owned(),
        data: Some(serde_json::json!({"field": "name", "issue": "missing"})),
    };
    let json = serde_json::to_string(&err).expect("serialize");
    let decoded: ProtocolError = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded.code, -32602);
    assert!(decoded.data.is_some());
}
