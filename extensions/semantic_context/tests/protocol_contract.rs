#[path = "../../../crates/core_engine/tests/support/mod.rs"]
mod support;

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use extension_protocol::{Capability, InitResult};
use serde_json::{Value, json};

const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
const NEGATIVE_WINDOW: Duration = Duration::from_millis(150);
const PUBLIC_TOOLS: [&str; 4] = [
    "semantic_reindex",
    "semantic_search",
    "semantic_related_files",
    "semantic_context_for_task",
];

struct SemanticChild {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Value>,
    next_id: u64,
    workspace: tempfile::TempDir,
    provider: support::EmbeddingFixture,
    files: std::collections::BTreeMap<String, String>,
}

impl SemanticChild {
    fn spawn() -> Self {
        Self::spawn_with_clock(None)
    }

    fn spawn_with_clock(clock: Option<&std::path::Path>) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let provider = support::EmbeddingFixture::start("private-provider-response");
        let mut command = Command::new(env!("CARGO_BIN_EXE_semantic_context_extension"));
        command
            .current_dir(workspace.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(clock) = clock {
            command.env("TOWER_SEMANTIC_TEST_CLOCK", clock);
        }
        let mut child = command.spawn().expect("spawn semantic context sidecar");
        let stdin = child.stdin.take().expect("semantic sidecar stdin");
        let stdout = child.stdout.take().expect("semantic sidecar stdout");
        let (tx, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("read semantic sidecar frame");
                tx.send(serde_json::from_str(&line).expect("semantic frame is JSON"))
                    .ok();
            }
        });
        Self {
            child,
            stdin,
            frames,
            next_id: 1,
            workspace,
            provider,
            files: std::collections::BTreeMap::new(),
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send_frame(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        id
    }

    fn send_frame(&mut self, frame: Value) {
        writeln!(self.stdin, "{frame}").expect("write semantic protocol frame");
        self.stdin.flush().expect("flush semantic protocol frame");
    }

    fn recv(&self) -> Value {
        self.frames
            .recv_timeout(FRAME_TIMEOUT)
            .expect("semantic sidecar must emit a frame before the deadline")
    }

    fn recv_response(&mut self, request_id: u64) -> Value {
        loop {
            let frame = self.recv();
            if frame.get("method").is_some() {
                self.answer_host_call(&frame);
                continue;
            }
            if frame["id"] == request_id {
                return frame;
            }
        }
    }

    fn answer_host_call(&mut self, frame: &Value) {
        let result = match frame["method"].as_str().expect("HostCall method") {
            "workspace/listFiles" => json!(self.files.keys().collect::<Vec<_>>()),
            "workspace/readFile" => {
                let path = frame["params"]["path"].as_str().unwrap();
                match self.files.get(path) {
                    Some(content) => json!({"status":"ok","content":content}),
                    None => json!({"status":"error","kind":"not_found"}),
                }
            }
            "index/get" => Value::Null,
            "log" => Value::Null,
            method => panic!("unexpected semantic HostCall: {method}"),
        };
        self.send_frame(json!({
            "jsonrpc": "2.0",
            "id": frame["id"].clone(),
            "result": result,
        }));
    }

    fn initialize(&mut self, config: Value) -> Value {
        let id = self.send(
            "initialize",
            json!({
                "protocol_version": extension_protocol::PROTOCOL_VERSION,
                "client_info": "semantic-protocol-contract",
                "extension_config": config,
            }),
        );
        self.recv_response(id)
    }

    fn invoke(&mut self, name: &str, params: Value) -> Value {
        let id = self.send("invokeTool", json!({"name": name, "params": params}));
        self.recv_response(id)
    }

    fn deliver(&mut self, event: Value) -> Value {
        let id = self.send("deliverEvent", event);
        self.recv_response(id)
    }
}

impl Drop for SemanticChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn disabled_config() -> Value {
    json!({
        "enabled": false,
        "embedding": {
            "provider": "openai_compatible",
            "base_url": "http://127.0.0.1:11434/v1",
            "model": "nomic-embed-text",
            "timeout_ms": 30000,
            "batch_size": 32,
            "allow_remote": false
        },
        "timeouts": {"search_ms": 30000},
        "chunking": {"target_bytes": 4096, "overlap_bytes": 512, "max_file_bytes": 2097152},
        "store": {"backend": "sqlite", "path": ".tower/semantic_context/index.sqlite3"}
    })
}

/// The lifecycle scenarios intentionally use an enabled configuration.  Keeping
/// this distinct from `disabled_config` prevents a compliant implementation
/// from satisfying an index/readiness assertion by violating disabled mode.
fn enabled_config(child: &SemanticChild) -> Value {
    json!({
        "enabled": true,
        "embedding": {
            "provider": "openai_compatible",
            "base_url": child.provider.endpoint(),
            "model": "fixture-model",
            "timeout_ms": 1000,
            "batch_size": 1,
            "dimensions": 3,
            "allow_remote": false
        },
        "timeouts": {"search_ms": 2000},
        "chunking": {"target_bytes": 64, "overlap_bytes": 0, "max_file_bytes": 4096},
        "store": {"backend": "sqlite", "path": ".tower/semantic_context/index.sqlite3"}
    })
}

fn expected_schemas() -> [(&'static str, Value); 5] {
    [
        (
            "semantic_reindex",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        (
            "semantic_search",
            json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":0},"path_prefixes":{"type":"array","items":{"type":"string"}},"languages":{"type":"array","items":{"type":"string"}},"lexical_rerank":{"type":"boolean"}},"required":["query","limit"],"additionalProperties":false}),
        ),
        (
            "semantic_related_files",
            json!({"type":"object","properties":{"path":{"type":"string"},"limit":{"type":"integer","minimum":0},"path_prefixes":{"type":"array","items":{"type":"string"}},"languages":{"type":"array","items":{"type":"string"}}},"required":["path","limit"],"additionalProperties":false}),
        ),
        (
            "semantic_context_for_task",
            json!({"type":"object","properties":{"task":{"type":"string"},"token_budget":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":0},"path_prefixes":{"type":"array","items":{"type":"string"}},"languages":{"type":"array","items":{"type":"string"}}},"required":["task","token_budget","limit"],"additionalProperties":false}),
        ),
        (
            "_tower_catch_up_fence",
            json!({"type":"object","properties":{"epoch":{"type":"integer","minimum":0},"delivered_events":{"type":"integer","minimum":0}},"required":["epoch","delivered_events"],"additionalProperties":false}),
        ),
    ]
}

fn result_data(frame: &Value) -> &Value {
    assert_eq!(frame["result"]["type"], "ToolResult", "{frame}");
    &frame["result"]["data"]
}

fn assert_invalid_params(frame: &Value) {
    assert_eq!(
        frame["error"],
        json!({"code": -32602, "message": "Invalid params"}),
        "{frame}"
    );
}

#[test]
fn semantic_manifest_declares_exact_tools_events_capabilities_and_schemas() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/extension.toml"))
        .expect("read semantic manifest");
    let parsed: extension_protocol::ExtensionManifest =
        toml::from_str(&manifest).expect("semantic manifest must parse");
    assert_eq!(parsed.tools.len(), 5);
    for (name, schema) in expected_schemas() {
        let declared = parsed
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("manifest omitted {name}"));
        assert_eq!(
            serde_json::from_str::<Value>(&declared.schema_json).unwrap(),
            schema,
            "manifest schema for {name}"
        );
    }

    let mut child = SemanticChild::spawn();
    let initialized = child.initialize(disabled_config());
    assert_eq!(initialized["result"]["type"], "Initialized");
    let init: InitResult = serde_json::from_value(initialized["result"]["data"].clone())
        .expect("decode semantic InitResult");
    assert_eq!(init.tools.len(), 5);
    let names: Vec<_> = init.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "semantic_reindex",
            "semantic_search",
            "semantic_related_files",
            "semantic_context_for_task",
            "_tower_catch_up_fence",
        ]
    );
    assert_eq!(
        names.iter().filter(|name| !name.starts_with('_')).count(),
        4
    );
    assert_eq!(
        names
            .iter()
            .copied()
            .filter(|name| !name.starts_with('_'))
            .collect::<Vec<_>>(),
        PUBLIC_TOOLS,
        "only the four public declarations may be passed to MCP discovery"
    );
    assert_eq!(
        init.events,
        [
            "event/fileIndexed",
            "event/fileChanged",
            "event/fileDeleted"
        ]
    );
    assert_eq!(
        init.capabilities,
        [
            Capability::ReadFile,
            Capability::ListFiles,
            Capability::IndexGet,
            Capability::Log,
        ]
    );
    for (name, schema) in expected_schemas() {
        let declared = init
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("Initialized omitted {name}"));
        assert_eq!(
            serde_json::from_str::<Value>(&declared.schema_json).unwrap(),
            schema,
            "Initialized schema for {name}"
        );
    }
}

#[test]
fn malformed_semantic_tool_params_return_invalid_params() {
    let mut child = SemanticChild::spawn();
    child.initialize(disabled_config());
    let malformed = [
        ("semantic_reindex", json!({"unknown": true})),
        ("semantic_search", json!({"limit": 1})),
        ("semantic_search", json!({"query": 7, "limit": 1})),
        ("semantic_search", json!({"query": "q", "limit": 1001})),
        (
            "semantic_search",
            json!({"query": "q", "limit": 1, "unknown": true}),
        ),
        (
            "semantic_search",
            json!({"query": "q", "limit": 1, "path_prefixes": ["../escape"]}),
        ),
        ("semantic_related_files", json!({"path": "", "limit": 1})),
        (
            "semantic_related_files",
            json!({"path": "../escape", "limit": 1}),
        ),
        (
            "semantic_context_for_task",
            json!({"task": "t", "limit": 1}),
        ),
        (
            "semantic_context_for_task",
            json!({"task": "t", "limit": 1, "token_budget": 1_000_001}),
        ),
    ];
    for (tool, params) in malformed {
        assert_invalid_params(&child.invoke(tool, params));
    }
}

#[test]
fn disabled_semantic_tools_return_provider_not_configured() {
    let mut child = SemanticChild::spawn();
    child.initialize(disabled_config());
    let requests = [
        ("semantic_reindex", json!({})),
        ("semantic_search", json!({"query":"needle","limit":5})),
        (
            "semantic_related_files",
            json!({"path":"src/lib.rs","limit":5}),
        ),
        (
            "semantic_context_for_task",
            json!({"task":"explain","token_budget":64,"limit":5}),
        ),
    ];
    for (tool, params) in requests {
        assert_eq!(
            result_data(&child.invoke(tool, params)),
            &json!({"status":"unsupported","reason":"provider_not_configured"})
        );
    }
    assert!(
        child.frames.recv_timeout(NEGATIVE_WINDOW).is_err(),
        "disabled mode started provider or rebuild work after its terminal response"
    );
}

#[test]
fn semantic_events_replace_and_delete_only_the_target_path() {
    let mut child = SemanticChild::spawn();
    child.initialize(enabled_config(&child));
    assert!(
        child.provider.requests().is_empty(),
        "initialize must not embed"
    );
    child
        .files
        .insert("src/a.rs".into(), "fn alpha_a() {}".into());
    child
        .files
        .insert("src/b.rs".into(), "fn alpha_b() {}".into());
    for (id, path) in [(1, "src/a.rs"), (2, "src/b.rs")] {
        assert_eq!(
            child.deliver(json!({"type":"FileIndexed","file_id":id,"path":path}))["result"]["type"],
            "Ack"
        );
    }
    child
        .files
        .insert("src/a.rs".into(), "fn alpha_replacement() {}".into());
    child.deliver(json!({"type":"FileChanged","file_id":1,"path":"src/a.rs"}));
    assert_eq!(
        result_data(&child.invoke(
            "_tower_catch_up_fence",
            json!({"epoch":1,"delivered_events":3})
        )),
        &json!({"epoch":1,"processed_events":3,"ready":true,"generation":1})
    );
    let result = child.invoke("semantic_search", json!({"query":"alpha","limit":10}));
    let expected = json!({"status":"ok","hits":[
        {"path":"src/a.rs","start_byte":0,"end_byte":25,"score":1.0,"language":"rust","snippet":"fn alpha_replacement() {}","reason":"semantic_similarity"},
        {"path":"src/b.rs","start_byte":0,"end_byte":15,"score":1.0,"language":"rust","snippet":"fn alpha_b() {}","reason":"semantic_similarity"}],"truncated":false});
    assert_eq!(result_data(&result), &expected);
    assert!(
        child
            .workspace
            .path()
            .join(".tower/semantic_context/index.sqlite3")
            .is_file()
    );
    assert_eq!(
        result_data(&child.invoke("semantic_reindex", json!({})))["indexed_files"],
        2
    );
    assert_eq!(
        result_data(&child.invoke("semantic_search", json!({"query":"","limit":0}))),
        &json!({"status":"ok","hits":[],"truncated":false})
    );
    assert_eq!(
        result_data(&child.invoke(
            "semantic_related_files",
            json!({"path":"src/b.rs","limit":0})
        )),
        &json!({"status":"ok","files":[],"truncated":false})
    );
    assert_eq!(
        result_data(&child.invoke(
            "semantic_context_for_task",
            json!({"task":"","token_budget":0,"limit":0})
        )),
        &json!({"status":"ok","chunks":[],"estimated_tokens":0,"truncated":true,"eligible_unfitted":2})
    );
    assert_eq!(
        result_data(&child.invoke("semantic_search", json!({"query":"","limit":5}))),
        &json!({"status":"ok","hits":[],"truncated":false})
    );
    assert_eq!(
        result_data(&child.invoke(
            "semantic_context_for_task",
            json!({"task":"","token_budget":64,"limit":5})
        )),
        &json!({"status":"ok","chunks":[],"estimated_tokens":0,"truncated":true,"eligible_unfitted":2})
    );
    child.files.remove("src/a.rs");
    child.deliver(json!({"type":"FileDeleted","file_id":1,"path":"src/a.rs"}));
    let fence = child.invoke(
        "_tower_catch_up_fence",
        json!({"epoch":1,"delivered_events":4}),
    );
    assert_eq!(result_data(&fence)["ready"], true);
    let result = child.invoke("semantic_search", json!({"query":"alpha","limit":10}));
    assert_eq!(result_data(&result)["hits"].as_array().unwrap().len(), 1);
    assert_eq!(
        result_data(&result)["hits"][0],
        expected["hits"][1],
        "unaffected path unchanged"
    );
}

#[test]
fn catch_up_fence_publishes_reconciled_generation_before_acknowledgement() {
    let mut child = SemanticChild::spawn();
    child.initialize(enabled_config(&child));
    child
        .files
        .insert("stale.rs".into(), "fn alpha_stale() {}".into());
    child.deliver(json!({"type":"FileIndexed","file_id":1,"path":"stale.rs"}));
    let first = child.invoke(
        "_tower_catch_up_fence",
        json!({"epoch":7,"delivered_events":1}),
    );
    assert_eq!(
        result_data(&first),
        &json!({"epoch":7,"processed_events":1,"ready":true,"generation":1})
    );
    // Restart the same store with a complete empty inventory. Persisted paths
    // disappear only when the new epoch publishes its reconciled generation.
    child.initialize(enabled_config(&child));
    let first = child.invoke(
        "_tower_catch_up_fence",
        json!({"epoch":8,"delivered_events":0}),
    );
    assert_eq!(
        result_data(&first),
        &json!({"epoch":8,"processed_events":0,"ready":true,"generation":2})
    );
    let result = child.invoke("semantic_search", json!({"query":"alpha","limit":10}));
    assert!(result_data(&result)["hits"].as_array().unwrap().is_empty());
    let pending = child.invoke(
        "_tower_catch_up_fence",
        json!({"epoch":8,"delivered_events":1}),
    );
    assert_eq!(
        result_data(&pending),
        &json!({"epoch":8,"processed_events":0,"ready":false,"generation":2})
    );
}

#[test]
fn expired_catch_up_fence_preserves_generation_and_recovers_with_fresh_snapshot() {
    let clock_dir = tempfile::tempdir().unwrap();
    let clock = clock_dir.path().join("clock-ms");
    std::fs::write(&clock, "0").unwrap();
    let mut child = SemanticChild::spawn_with_clock(Some(&clock));
    let mut config = enabled_config(&child);
    config["timeouts"]["reindex_ms"] = json!(10);
    child.initialize(config);
    child
        .files
        .insert("src/recovered.rs".into(), "fn recovered() {}".into());
    child.deliver(json!({"type":"FileIndexed","file_id":1,"path":"src/recovered.rs"}));

    std::fs::write(&clock, "11").unwrap();
    let expired = child.invoke(
        "_tower_catch_up_fence",
        json!({"epoch":1,"delivered_events":1}),
    );
    assert_eq!(result_data(&expired)["code"], "operation_timeout");
    assert_eq!(result_data(&expired)["ready"], false);
    assert_eq!(result_data(&expired)["generation"], 0);

    std::fs::write(&clock, "0").unwrap();
    let recovered = child.invoke(
        "_tower_catch_up_fence",
        json!({"epoch":2,"delivered_events":1}),
    );
    assert_eq!(
        result_data(&recovered),
        &json!({"epoch":2,"processed_events":1,"ready":true,"generation":1})
    );
    let search = child.invoke("semantic_search", json!({"query":"recovered","limit":10}));
    assert_eq!(result_data(&search)["status"], "ok");
    assert_eq!(result_data(&search)["hits"][0]["path"], "src/recovered.rs");
}

#[test]
fn semantic_failures_return_stable_sanitized_payloads() {
    let mut child = SemanticChild::spawn();
    let mut config = enabled_config(&child);
    config["embedding"]["timeout_ms"] = json!(1000);
    child.initialize(config);
    child.provider.enqueue([
        support::EmbeddingReply::Unavailable,
        support::EmbeddingReply::Malformed,
        support::EmbeddingReply::DimensionMismatch,
    ]);
    for code in [
        "provider_unavailable",
        "provider_invalid_response",
        "provider_dimension_mismatch",
    ] {
        let result = child.invoke(
            "semantic_search",
            json!({"query":"api-key-secret /private/provider/path","limit":1}),
        );
        assert_eq!(result_data(&result)["status"], "error");
        assert_eq!(result_data(&result)["code"], code);
        assert!(!result.to_string().contains("api-key-secret"));
    }
    let (held, release) = support::EmbeddingFixture::held_reply();
    child.provider.enqueue([held]);
    let completed = child.provider.logs().len();
    let result = child.invoke("semantic_search", json!({"query":"alpha","limit":1}));
    support::EmbeddingFixture::release(&release);
    child.provider.wait_for_logs(completed + 2);
    assert_eq!(
        result_data(&result),
        &json!({"status":"error","code":"provider_timeout","message":"Semantic provider timed out","timeout":{"operation":"embedding","layer":"provider","budget_ms":1000}})
    );
    let result = child.invoke("semantic_search", json!({"query":"alpha","limit":1}));
    assert_eq!(result_data(&result)["status"], "ok", "{result}");
}

#[test]
fn host_frames_are_preserved_while_awaiting_provider_host_calls() {
    let mut child = SemanticChild::spawn();
    let initialize = child.initialize(enabled_config(&child));
    assert_eq!(initialize["result"]["type"], "Initialized");
    child
        .files
        .insert("src/lib.rs".into(), "fn alpha() {}".into());
    let snapshot_id = child.send(
        "deliverEvent",
        json!({"type":"FileIndexed","file_id":1,"path":"src/lib.rs"}),
    );
    let held_host_call = child.recv();
    assert_eq!(held_host_call["method"], "workspace/readFile");
    let live_id = child.send(
        "deliverEvent",
        json!({"type":"FileChanged","file_id":1,"path":"src/lib.rs"}),
    );
    let fence_id = child.send(
        "invokeTool",
        json!({"name":"_tower_catch_up_fence","params":{"epoch":9,"delivered_events":2}}),
    );
    let invoke_id = child.send(
        "invokeTool",
        json!({"name":"semantic_search","params":{"query":"alpha","limit":1}}),
    );
    let shutdown_id = child.send("shutdown", json!({}));
    child.answer_host_call(&held_host_call);
    for (id, kind) in [
        (snapshot_id, "Ack"),
        (live_id, "Ack"),
        (fence_id, "ToolResult"),
        (invoke_id, "ToolResult"),
        (shutdown_id, "Ack"),
    ] {
        let response = child.recv_response(id);
        assert_eq!(
            response["result"]["type"], kind,
            "milestone {id}: {response}"
        );
        if id == fence_id {
            assert_eq!(result_data(&response)["ready"], true);
        }
        if id == invoke_id {
            assert_eq!(result_data(&response)["hits"][0]["path"], "src/lib.rs");
        }
    }
    assert!(child.frames.recv_timeout(NEGATIVE_WINDOW).is_err());
}
