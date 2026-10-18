//! Scripted LSP peer. It deliberately supports faults as well as valid replies.
#![forbid(unsafe_code)]

mod hierarchy;
mod navigation;
mod wire;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

struct Server {
    mode: String,
    dispatch_profile: bool,
    log: Option<PathBuf>,
    documents: HashMap<String, String>,
}

impl Server {
    fn trace(&self, event: &str, mut fields: Value) -> io::Result<()> {
        if let Some(log) = &self.log
            && !self.dispatch_profile
        {
            fields["event"] = json!(event);
            append_line(
                PathBuf::from(format!("{}.trace", log.display())),
                &fields.to_string(),
            )?;
        }
        Ok(())
    }

    fn capabilities(&self) -> Value {
        if self.mode == "dispatch_unavailable" {
            return json!({});
        }
        let mut capabilities = json!({
            "textDocumentSync": 1,
            "renameProvider": if self.mode == "no_prepare" || self.dispatch_profile { json!(true) } else { json!({"prepareProvider": true}) },
        });
        if self.mode != "provider_absent" {
            for provider in [
                "definitionProvider",
                "referencesProvider",
                "hoverProvider",
                "implementationProvider",
                "typeHierarchyProvider",
            ] {
                capabilities[provider] = json!(
                    self.mode != "provider_false"
                        && (provider != "typeHierarchyProvider"
                            || self.mode != "hierarchy_unavailable")
                );
            }
        }
        if self.mode.starts_with("path_rename_") && self.mode != "path_rename_unsupported" {
            capabilities["workspace"] = json!({"fileOperations": {"willRename": true}});
        }
        capabilities
    }

    fn synchronize(&mut self, method: &str, params: &Value, uri: &str) -> io::Result<()> {
        match method {
            "textDocument/didOpen" => {
                self.documents.insert(
                    uri.to_owned(),
                    params["textDocument"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
                self.trace("didOpen", json!({"uri": uri, "text": self.documents[uri]}))?;
                if self.dispatch_profile || self.mode == "diagnostics_ready" {
                    wire::send(
                        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": []}}),
                    )?;
                    wire::send(
                        &json!({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"quiescent": true}}),
                    )?;
                } else if self.mode == "server_crashed" {
                    std::process::exit(71);
                } else if self.mode == "server_malformed_response" {
                    io::stdout().write_all(b"Content-Length: nope\r\n\r\n")?;
                    io::stdout().flush()?;
                    std::thread::sleep(Duration::from_secs(30));
                }
            }
            "textDocument/didChange" => {
                if let Some(change) = params["contentChanges"]
                    .as_array()
                    .and_then(|changes| changes.last())
                {
                    self.documents.insert(
                        uri.to_owned(),
                        change["text"].as_str().unwrap_or_default().to_owned(),
                    );
                }
                self.trace("didChange", json!({"uri": uri, "text": self.documents.get(uri).map(String::as_str).unwrap_or_default()}))?;
            }
            "textDocument/didClose" => {
                self.trace("didClose", json!({"uri": uri}))?;
                self.documents.remove(uri);
            }
            _ => {}
        }
        Ok(())
    }

    fn run(&mut self) -> io::Result<()> {
        let mut input = io::stdin().lock();
        while let Some(message) = wire::read(&mut input)? {
            let method = message["method"].as_str().unwrap_or_default();
            if let Some(log) = &self.log {
                let line = if self.dispatch_profile || self.mode.starts_with("path_rename_") {
                    message.to_string()
                } else {
                    method.to_owned()
                };
                append_line(log, &line)?;
            }
            let params = &message["params"];
            let uri = params["textDocument"]["uri"]
                .as_str()
                .or_else(|| params["item"]["uri"].as_str())
                .unwrap_or("file:///workspace/src/lib.rs");
            if method == "exit" {
                break;
            }
            if method.starts_with("textDocument/did") {
                self.synchronize(method, params, uri)?;
                continue;
            }
            let Some(id) = message.get("id") else {
                continue;
            };
            if method == "initialize" {
                wire::reply(id, json!({"capabilities": self.capabilities()}))?;
                if self.mode == "server_transport_error" {
                    disconnect_stdin()?;
                    std::thread::sleep(Duration::from_secs(300));
                    return Ok(());
                }
            } else if method == "textDocument/prepareTypeHierarchy"
                || method.starts_with("typeHierarchy/")
            {
                self.hierarchy(method, id, params, uri)?;
            } else if method == "workspace/willRenameFiles" {
                if matches!(
                    self.mode.as_str(),
                    "path_rename_malformed" | "path_rename_timeout" | "path_rename_crash"
                ) && let Some(log) = &self.log
                {
                    append_line(log, &format!("privacy_fixture_exercised:{}", self.mode))?;
                }
                match self.mode.as_str() {
                    "path_rename_null" => wire::reply(id, Value::Null)?,
                    "path_rename_edit" => {
                        let target = params["files"][0]["oldUri"].clone();
                        wire::reply(
                            id,
                            json!({"changes": {
                                target.as_str().expect("rename URI"): [{
                                    "range": {
                                        "start": {"line": 0, "character": 3},
                                        "end": {"line": 0, "character": 11}
                                    },
                                    "newText": "new_name"
                                }]
                            }}),
                        )?
                    }
                    "path_rename_malformed" => wire::reply(
                        id,
                        json!({
                            "changes": concat!(
                                "PRIVATE_RAW_BACKEND_SENTINEL ",
                                "/absolute/PRIVATE_ABSOLUTE_PATH_SENTINEL ",
                                "file:///PRIVATE_EXTERNAL_URI_SENTINEL ",
                                "PRIVATE_REPLACEMENT_CONTENT_SENTINEL"
                            )
                        }),
                    )?,
                    "path_rename_timeout" => std::thread::sleep(Duration::from_secs(30)),
                    "path_rename_crash" => std::process::exit(71),
                    "path_rename_semantic" => self.path_rename_semantic(id, params)?,
                    _ => wire::reply(id, json!({"changes": {}}))?,
                }
            } else {
                self.navigation(method, id, params, uri)?;
            }
        }
        Ok(())
    }
}

// Emit a complete record in one write so concurrent test readers never observe
// the fragments produced by formatting a JSON Value directly into a File.
fn append_line(path: impl AsRef<std::path::Path>, line: &str) -> io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(format!("{line}\n").as_bytes())
}

// Closing the pipe's read end while keeping the process alive distinguishes
// transport failure from process exit in the host's error classifier.
#[cfg(unix)]
fn disconnect_stdin() -> io::Result<()> {
    rustix::stdio::dup2_stdin(std::fs::File::open("/dev/null")?)?;
    Ok(())
}

#[cfg(not(unix))]
fn disconnect_stdin() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "pipe disconnection fixture requires Unix",
    ))
}

fn run() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected fixture mode"))?;
    let log = args.next().filter(|value| value != "-").map(PathBuf::from);
    if let Some(pids) = args.next().filter(|value| value != "-") {
        append_line(pids, &std::process::id().to_string())?;
    }
    let dispatch_profile = mode.starts_with("dispatch_");
    Server {
        mode,
        dispatch_profile,
        log,
        documents: HashMap::new(),
    }
    .run()
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fixture_lsp_server: {error}");
        std::process::exit(1);
    }
}
