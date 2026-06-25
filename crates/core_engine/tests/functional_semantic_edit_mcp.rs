// Feature: F007

#![forbid(unsafe_code)]
#![allow(clippy::pedantic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::config::TowerConfig;
use core_engine::adapters::daemon::engine::{EngineHandle, build_engine};
use core_engine::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use core_engine::adapters::mcp::registry::ToolRegistry;
use serde_json::{Value, json};

struct FunctionalEngine {
    workspace: tempfile::TempDir,
    registry: ExtensionMergedRegistry,
    _handle: EngineHandle,
}

fn workspace_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn target_debug_bin(name: &str) -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn install_extension(workspace: &Path, name: &str, bin_name: &str) {
    let extension_dir = workspace.join(".tower/extensions").join(name);
    fs::create_dir_all(&extension_dir).expect("create extension directory");

    let shipped = fs::read_to_string(
        workspace_root()
            .join("extensions")
            .join(name)
            .join("extension.toml"),
    )
    .expect("read shipped extension manifest");
    let bin = target_debug_bin(bin_name).replace('\\', "\\\\");
    let manifest = shipped
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("command") {
                format!("command = [\"{bin}\"]")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    fs::write(extension_dir.join("extension.toml"), manifest).expect("write test manifest");
}

fn write_file(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create file parent");
    }
    fs::write(path, content).expect("write workspace file");
}

fn build_functional_engine(
    files: &[(&str, &str)],
    extensions: &[(&str, &str)],
    config: TowerConfig,
) -> FunctionalEngine {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    fs::create_dir_all(workspace.path().join(".tower")).expect("create .tower");
    for (path, content) in files {
        write_file(workspace.path(), path, content);
    }
    for (name, bin_name) in extensions {
        install_extension(workspace.path(), name, bin_name);
    }

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(workspace.path().join(".tower/extensions")),
    };
    let handle = build_engine(&opts, config).expect("build functional engine");
    let registry =
        ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry.clone());

    FunctionalEngine {
        workspace,
        registry,
        _handle: handle,
    }
}

fn ast_engine(files: &[(&str, &str)]) -> FunctionalEngine {
    let mut engine =
        build_functional_engine(files, &[("ast", "ast_extension")], TowerConfig::default());
    engine
        .registry
        .call("tower_ast_reindex", json!({}))
        .expect("seed AST index through public reindex tool");
    engine
}

fn lsp_engine(mode: &str) -> FunctionalEngine {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    fs::create_dir_all(workspace.path().join(".tower")).expect("create .tower");
    write_file(workspace.path(), "src/lib.rs", "fn old_name() {}\n");
    install_extension(workspace.path(), "lsp", "lsp_extension");
    let script = write_fake_lsp_server(workspace.path(), mode);
    let config_src = lsp_config_source(&script, mode);
    fs::write(workspace.path().join(".tower/config.toml"), &config_src)
        .expect("write LSP workspace config");
    let config = toml::from_str(&config_src).expect("parse LSP test config");

    let opts = GlobalOpts {
        workspace_dir: Some(workspace.path().to_path_buf()),
        extensions_dir: Some(workspace.path().join(".tower/extensions")),
    };
    let handle = build_engine(&opts, config).expect("build LSP functional engine");
    let registry =
        ExtensionMergedRegistry::new(Arc::clone(&handle.state), handle.ext_registry.clone());

    FunctionalEngine {
        workspace,
        registry,
        _handle: handle,
    }
}

fn lsp_config_source(script: &Path, mode: &str) -> String {
    let script = script.to_string_lossy().replace('\\', "\\\\");
    let mode = mode.replace('\\', "\\\\");
    format!(
        r#"
[extensions]
request_timeout_secs = 10

[lsp.rust]
command = "python3"
args = ["{script}", "{mode}"]
extensions = ["rs"]
"#
    )
}

fn write_fake_lsp_server(workspace: &Path, mode: &str) -> PathBuf {
    let script = workspace.join(format!("fake_lsp_{mode}.py"));
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

MODE = sys.argv[1]

def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])).decode("utf-8"))

def send_message(payload):
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    request_id = message.get("id")
    if request_id is None:
        continue
    method = message.get("method")
    params = message.get("params") or {}
    text_document = params.get("textDocument") or {}
    uri = text_document.get("uri") or "file:///workspace/src/lib.rs"

    if method == "initialize":
        send_message({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "implementationProvider": True,
                    "renameProvider": {"prepareProvider": True}
                }
            }
        })
    elif method == "textDocument/implementation":
        send_message({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": [{
                "uri": uri,
                "range": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 11}
                }
            }]
        })
    elif method == "textDocument/prepareRename":
        if MODE == "reject":
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32602, "message": "not valid rename target"}
            })
        else:
            send_message({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 11}
                    },
                    "placeholder": "old_name"
                }
            })
    elif method == "textDocument/rename":
        send_message({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "changes": {
                    uri: [{
                        "range": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 11}
                        },
                        "newText": params.get("newName", "new_name")
                    }]
                }
            }
        })
    else:
        send_message({"jsonrpc": "2.0", "id": request_id, "result": None})
"#,
    )
    .expect("write fake LSP server");
    script
}

fn read_workspace_file(engine: &FunctionalEngine, path: &str) -> String {
    fs::read_to_string(engine.workspace.path().join(path)).expect("read workspace file")
}

fn assert_success(value: &Value) {
    assert!(
        value.get("error").is_none(),
        "expected successful tool result, got {value}"
    );
}

#[test]
fn ast_delete_symbol_mutates_workspace_file_through_public_mcp_tool() {
    let mut engine = ast_engine(&[(
        "src/lib.rs",
        "pub fn remove_me() {}\n\npub fn keep_me() -> u8 {\n    1\n}\n",
    )]);

    let result = engine
        .registry
        .call(
            "tower_ast_delete_symbol",
            json!({
                "path": "src/lib.rs",
                "symbol_name": "remove_me",
                "kind": "function"
            }),
        )
        .expect("delete symbol through public MCP tool");

    assert_success(&result);
    assert_eq!(result["applied"], true, "delete payload: {result}");
    let edited = read_workspace_file(&engine, "src/lib.rs");
    assert!(!edited.contains("remove_me"), "edited file: {edited}");
    assert!(edited.contains("pub fn keep_me() -> u8"));
}

#[test]
fn ast_replace_symbol_body_dry_run_returns_preview_without_writing_file() {
    let mut engine = ast_engine(&[("src/lib.rs", "pub fn answer() -> u8 {\n    1\n}\n")]);

    let result = engine
        .registry
        .call(
            "tower_ast_replace_symbol_body",
            json!({
                "path": "src/lib.rs",
                "symbol_name": "answer",
                "kind": "function",
                "replacement": "\n    2\n",
                "dry_run": true
            }),
        )
        .expect("dry-run replace through public MCP tool");

    assert_success(&result);
    assert_eq!(result["applied"], false, "dry-run payload: {result}");
    assert!(
        result["preview"].as_str().unwrap_or_default().contains("2"),
        "dry-run payload: {result}"
    );
    assert_eq!(
        read_workspace_file(&engine, "src/lib.rs"),
        "pub fn answer() -> u8 {\n    1\n}\n"
    );
}

#[test]
fn ast_insert_after_symbol_reports_ambiguous_symbol_without_writing_file() {
    let initial = "mod a { pub fn duplicate() {} }\nmod b { pub fn duplicate() {} }\n";
    let mut engine = ast_engine(&[("src/lib.rs", initial)]);

    let result = engine
        .registry
        .call(
            "tower_ast_insert_after_symbol",
            json!({
                "path": "src/lib.rs",
                "symbol_name": "duplicate",
                "kind": "function",
                "replacement": "\npub fn added() {}\n"
            }),
        )
        .expect("ambiguous symbol error is returned as a structured result");

    assert_eq!(result["code"], "ambiguous_symbol", "payload: {result}");
    assert_eq!(read_workspace_file(&engine, "src/lib.rs"), initial);
}

#[test]
fn lsp_implementations_returns_locations_through_configured_language_server() {
    let mut engine = lsp_engine("ok");

    let result = engine
        .registry
        .call(
            "tower_lsp_implementations",
            json!({
                "path": "src/lib.rs",
                "line": 0,
                "character": 3
            }),
        )
        .expect("implementation lookup through public MCP tool");

    assert_success(&result);
    assert_eq!(
        result["supported"], true,
        "implementation payload: {result}"
    );
    assert_eq!(result["locations"][0]["path"], "src/lib.rs");
    assert_eq!(result["locations"][0]["line"], 0);
    assert_eq!(result["locations"][0]["character"], 3);
}

#[test]
fn lsp_rename_dry_run_returns_preview_without_writing_file() {
    let mut engine = lsp_engine("ok");

    let result = engine
        .registry
        .call(
            "tower_lsp_rename",
            json!({
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name",
                "dry_run": true
            }),
        )
        .expect("rename dry-run through public MCP tool");

    assert_success(&result);
    assert!(
        result["preview"]
            .as_str()
            .unwrap_or_default()
            .contains("fn new_name()"),
        "rename payload: {result}"
    );
    assert_eq!(
        read_workspace_file(&engine, "src/lib.rs"),
        "fn old_name() {}\n"
    );
}

#[test]
fn lsp_rename_reports_not_renameable_without_writing_file() {
    let mut engine = lsp_engine("reject");

    let result = engine
        .registry
        .call(
            "tower_lsp_rename",
            json!({
                "path": "src/lib.rs",
                "line": 0,
                "character": 3,
                "new_name": "new_name"
            }),
        )
        .expect("not-renameable result through public MCP tool");

    assert_eq!(result["code"], "not_renameable", "payload: {result}");
    assert_eq!(
        read_workspace_file(&engine, "src/lib.rs"),
        "fn old_name() {}\n"
    );
}
