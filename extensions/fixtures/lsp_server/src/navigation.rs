use crate::{Server, wire};
use serde_json::{Value, json};
use std::io;
use std::time::Duration;

fn range() -> Value {
    json!({"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 11}})
}

impl Server {
    pub(super) fn navigation(
        &self,
        method: &str,
        id: &Value,
        params: &Value,
        uri: &str,
    ) -> io::Result<()> {
        match method {
            "textDocument/definition"
            | "textDocument/references"
            | "textDocument/implementation" => {
                match self.mode.as_str() {
                    "backend_error" => {
                        return wire::error(id, -32099, "implementation backend exploded");
                    }
                    "server_timeout" => {
                        std::thread::sleep(Duration::from_secs(30));
                        return Ok(());
                    }
                    "method_not_found" => return wire::error(id, -32601, "method not found"),
                    "empty" => return wire::reply(id, json!([])),
                    _ => {}
                }
                let (uri, range) = if self.dispatch_profile {
                    (
                        uri.replace("main.rs", "answer.rs"),
                        json!({"start": {"line": 7, "character": 2}, "end": {"line": 7, "character": 8}}),
                    )
                } else {
                    (uri.to_owned(), range())
                };
                wire::reply(id, json!([{"uri": uri, "range": range}]))
            }
            "textDocument/hover" => {
                if self.mode == "method_not_found" {
                    return wire::error(id, -32601, "method not found");
                }
                wire::reply(
                    id,
                    if self.mode == "empty" || self.dispatch_profile {
                        Value::Null
                    } else {
                        json!({"contents": {"kind": "plaintext", "value": "fn old_name()"}})
                    },
                )
            }
            "textDocument/prepareRename" => self.prepare_rename(id),
            "textDocument/rename" => {
                let mut edit = json!({"range": range(), "newText": params["newName"].as_str().unwrap_or("new_name")});
                if self.mode == "invalid_range" {
                    edit["range"]["start"]["character"] = json!(999);
                    edit["range"]["end"]["character"] = json!(1000);
                }
                let result = match self.mode.as_str() {
                    "unsupported_edit" => {
                        json!({"documentChanges": [{"kind": "create", "uri": uri}]})
                    }
                    "versioned_document_changes" => {
                        json!({"documentChanges": [{"textDocument": {"uri": uri, "version": 7}, "edits": [edit]}]})
                    }
                    _ => json!({"changes": {uri: [edit]}}),
                };
                wire::reply(id, result)
            }
            _ => wire::reply(id, Value::Null),
        }
    }

    fn prepare_rename(&self, id: &Value) -> io::Result<()> {
        match self.mode.as_str() {
            "server_crashed" => std::process::exit(71),
            "server_timeout" => {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            }
            "server_malformed_response" => {
                use std::io::Write;
                io::stdout().write_all(b"Content-Length: 1\r\n\r\n{")?;
                io::stdout().flush()?;
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            }
            "backend_error" => wire::error(id, -32603, "SENTINEL_PRIVATE_BACKEND_DETAIL"),
            "no_prepare" => wire::error(id, -32603, "prepareRename should not be called"),
            "prepare_method_not_found" => wire::error(id, -32601, "method not found"),
            "reject" => wire::error(id, -32602, "not valid rename target"),
            _ => wire::reply(id, json!({"range": range(), "placeholder": "old_name"})),
        }
    }
}
