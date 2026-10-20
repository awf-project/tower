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
    safe_delete_requests: usize,
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
        if self.mode.starts_with("safe_delete_")
            && !self.mode.starts_with("safe_delete_no_cleanup_")
            && !matches!(
                self.mode.as_str(),
                "safe_delete_no_cleanup_with_references"
                    | "safe_delete_no_cleanup_reference_free"
                    | "safe_delete_reference_lookup_unavailable"
            )
        {
            capabilities["experimental"] = json!({"towerSafeDelete": true});
        }
        if matches!(
            self.mode.as_str(),
            "safe_delete_reference_lookup_unavailable" | "safe_delete_capability_unavailable"
        ) {
            capabilities["referencesProvider"] = json!(false);
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
            } else if method == "textDocument/references" && self.mode.starts_with("safe_delete_") {
                self.safe_delete_references(id, uri)?;
            } else if method == "experimental/tower/safeDelete" {
                self.safe_delete_cleanup(id, uri)?;
            } else {
                self.navigation(method, id, params, uri)?;
            }
        }
        Ok(())
    }

    fn safe_delete_references(&mut self, id: &Value, uri: &str) -> io::Result<()> {
        self.safe_delete_requests += 1;
        let first = self.safe_delete_requests == 1;
        if self.mode == "safe_delete_backend_timeout" {
            std::thread::sleep(Duration::from_secs(30));
            return Ok(());
        }
        if self.mode == "safe_delete_backend_crashed" {
            std::process::exit(71);
        }
        if self.mode == "safe_delete_backend_text_sentinel" {
            return wire::error(id, -32603, "BACKEND_TEXT_SENTINEL_0a42");
        }
        if self.mode == "safe_delete_backend_credential_sentinel" {
            return wire::error(
                id,
                -32603,
                "RAW_SIDECAR_ERROR_SENTINEL CREDENTIAL_SENTINEL_7d91",
            );
        }
        if self.mode == "safe_delete_absolute_path_sentinel" {
            return wire::reply(
                id,
                json!([location(
                    "file:///private/ABSOLUTE_PATH_SENTINEL_5f14.rs",
                    0,
                    3,
                    11
                )]),
            );
        }
        if self.mode == "safe_delete_external_uri_sentinel" {
            return wire::reply(
                id,
                json!([location("file:///external/URI_SENTINEL_39c1.rs", 0, 3, 11)]),
            );
        }
        if self.mode == "safe_delete_workspace_external" {
            return wire::reply(
                id,
                json!([location("file:///outside/workspace.rs", 0, 3, 11)]),
            );
        }
        if self.mode == "safe_delete_symlink_escape" {
            return wire::reply(
                id,
                json!([location(&replace_file_name(uri, "escape.rs"), 0, 3, 11)]),
            );
        }
        if self.mode == "safe_delete_invalid_location"
            || self.mode == "safe_delete_invalid_utf16_position"
        {
            return wire::reply(id, json!([location(uri, 99, 0, 1)]));
        }
        if self.mode == "safe_delete_split_surrogate_position" {
            return wire::reply(id, json!([location(uri, 0, 999, 1000)]));
        }
        if self.mode.ends_with("_once") && first {
            if self.mode == "safe_delete_malformed_once" {
                return wire::reply(id, json!({"not": "locations"}));
            }
            if matches!(
                self.mode.as_str(),
                "safe_delete_unconfigured_once"
                    | "safe_delete_not_renameable_once"
                    | "safe_delete_language_not_configured_once"
            ) {
                return wire::error(id, -32601, "safe-delete fixture capability unavailable");
            }
            return wire::error(id, -32603, "safe-delete fixture failure");
        }
        if matches!(
            self.mode.as_str(),
            "safe_delete_no_cleanup_reference_free" | "safe_delete_reference_free"
        ) {
            return wire::reply(id, json!([]));
        }
        if self.mode == "safe_delete_null_references" {
            return wire::reply(id, Value::Null);
        }
        if matches!(
            self.mode.as_str(),
            "safe_delete_no_cleanup_internal"
                | "safe_delete_no_cleanup_mixed"
                | "safe_delete_no_cleanup_external_same_file"
                | "safe_delete_no_cleanup_crossing"
        ) {
            let text = &self.documents[uri];
            let offset = text.rfind("old_name").expect("fixture has a reference");
            let prefix = &text[..offset];
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
            let column = prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
            let internal = if self.mode == "safe_delete_no_cleanup_crossing" {
                json!({"uri": uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 1, "character": 4}}})
            } else {
                location(uri, line, column, column + 8)
            };
            let mut references = vec![internal];
            if self.mode == "safe_delete_no_cleanup_mixed" {
                references.push(location(&replace_file_name(uri, "consumer.rs"), 0, 0, 8));
            }
            return wire::reply(id, json!(references));
        }
        if self.mode == "safe_delete_no_cleanup_with_references" {
            return wire::reply(
                id,
                json!([location(&replace_file_name(uri, "consumer.rs"), 0, 0, 8)]),
            );
        }
        let reference = if matches!(
            self.mode.as_str(),
            "safe_delete_complete"
                | "safe_delete_complete_triple"
                | "safe_delete_private_replacement"
        ) {
            location(
                &replace_file_name(
                    uri,
                    if self.mode == "safe_delete_complete_triple" {
                        "b.rs"
                    } else {
                        "consumer.rs"
                    },
                ),
                0,
                14,
                22,
            )
        } else if self.mode == "safe_delete_unicode" {
            location(&replace_file_name(uri, "consumer.rs"), 0, 29, 37)
        } else {
            location(uri, 0, 3, 11)
        };
        if self.mode == "safe_delete_complete_triple" {
            return wire::reply(
                id,
                json!([
                    reference,
                    location(&replace_file_name(uri, "c.rs"), 0, 13, 19)
                ]),
            );
        }
        wire::reply(id, json!([reference]))
    }

    fn safe_delete_cleanup(&mut self, id: &Value, uri: &str) -> io::Result<()> {
        if self.mode == "safe_delete_malformed_response" {
            return wire::reply(
                id,
                json!({"coverage": "complete", "edit": [], "extra": true}),
            );
        }
        let coverage = if matches!(
            self.mode.as_str(),
            "safe_delete_incomplete" | "safe_delete_private_replacement"
        ) {
            "incomplete"
        } else {
            "complete"
        };
        let changes = if self.mode == "safe_delete_complete" {
            json!({replace_file_name(uri, "consumer.rs"): [{
                "range": {"start": {"line": 0, "character": 14}, "end": {"line": 0, "character": 22}},
                "newText": ""
            }]})
        } else if self.mode == "safe_delete_complete_triple" {
            json!({
                replace_file_name(uri, "b.rs"): [{
                    "range": {"start": {"line": 0, "character": 14}, "end": {"line": 0, "character": 22}},
                    "newText": ""
                }],
                replace_file_name(uri, "c.rs"): [{
                    "range": {"start": {"line": 0, "character": 13}, "end": {"line": 0, "character": 19}},
                    "newText": ""
                }]
            })
        } else if self.mode == "safe_delete_private_replacement" {
            json!({replace_file_name(uri, "consumer.rs"): [{
                "range": {"start": {"line": 0, "character": 14}, "end": {"line": 0, "character": 22}},
                "newText": "REPLACEMENT_SENTINEL_fa81"
            }]})
        } else if self.mode == "safe_delete_unicode" {
            json!({replace_file_name(uri, "consumer.rs"): [{
                "range": {"start": {"line": 0, "character": 29}, "end": {"line": 0, "character": 37}},
                "newText": ""
            }]})
        } else {
            json!({})
        };
        wire::reply(
            id,
            json!({"coverage": coverage, "edit": {"changes": changes}}),
        )
    }
}

fn location(uri: &str, line: u32, start: u32, end: u32) -> Value {
    json!({
        "uri": uri,
        "range": {
            "start": {"line": line, "character": start},
            "end": {"line": line, "character": end}
        }
    })
}

fn replace_file_name(uri: &str, name: &str) -> String {
    uri.rsplit_once('/')
        .map(|(parent, _)| format!("{parent}/{name}"))
        .unwrap_or_else(|| uri.to_owned())
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
        safe_delete_requests: 0,
    }
    .run()
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fixture_lsp_server: {error}");
        std::process::exit(1);
    }
}
