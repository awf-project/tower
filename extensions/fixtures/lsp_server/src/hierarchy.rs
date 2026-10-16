use crate::{Server, wire};
use serde_json::{Value, json};
use std::io;

fn item(name: &str, uri: &str, line: u32, character: u32, data: &str) -> Value {
    json!({
        "name": name, "kind": 5, "uri": uri,
        "range": {"start": {"line": line, "character": character}, "end": {"line": line, "character": character + 6}},
        "selectionRange": {"start": {"line": line, "character": character + 1}, "end": {"line": line, "character": character + 5}},
        "data": data,
    })
}

fn dispatch_item(name: &str, uri: &str, line: u32, end: u32, selection_end: u32) -> Value {
    json!({"name": name, "kind": 5, "uri": uri,
        "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": end}},
        "selectionRange": {"start": {"line": line, "character": 7}, "end": {"line": line, "character": selection_end}}})
}

impl Server {
    pub(super) fn hierarchy(
        &self,
        method: &str,
        id: &Value,
        params: &Value,
        uri: &str,
    ) -> io::Result<()> {
        if self.dispatch_profile {
            let result = match method {
                "textDocument/prepareTypeHierarchy" => vec![dispatch_item("Child", uri, 0, 12, 12)],
                "typeHierarchy/supertypes" => {
                    if self.mode == "dispatch_backend_error" {
                        return wire::send(
                            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603,
                            "message": "opaque backend data must stay private", "data": {"secret": "opaque backend data must stay private"}}}),
                        );
                    }
                    vec![dispatch_item(
                        "Parent",
                        &uri.replace("main.rs", "parent.rs"),
                        1,
                        13,
                        13,
                    )]
                }
                "typeHierarchy/subtypes" => vec![dispatch_item(
                    "Grandchild",
                    &uri.replace("main.rs", "grandchild.rs"),
                    2,
                    18,
                    17,
                )],
                _ => return wire::reply(id, Value::Null),
            };
            return wire::reply(id, json!(result));
        }
        let result = match method {
            "textDocument/prepareTypeHierarchy" => {
                let source = self
                    .documents
                    .get(uri)
                    .map(String::as_str)
                    .unwrap_or_default();
                self.trace("prepare", json!({"uri": uri, "text": source}))?;
                match self.mode.as_str() {
                    "hierarchy_backend_error" => {
                        return wire::error(id, -32099, "PRIVATE_BACKEND_FAILURE");
                    }
                    "hierarchy_null" => return wire::reply(id, Value::Null),
                    "hierarchy_empty" => vec![],
                    _ => {
                        let name = if source.contains("SNAPSHOT_B") {
                            "SnapshotB"
                        } else if source.contains("SNAPSHOT_A") {
                            "SnapshotA"
                        } else {
                            "Root"
                        };
                        vec![item(name, uri, 1, 2, "OPAQUE_PREPARED_DATA")]
                    }
                }
            }
            "typeHierarchy/supertypes" => {
                self.trace("supertypes", json!({"item": params["item"]}))?;
                vec![item("Base", uri, 11, 12, "OPAQUE_PARENT_DATA")]
            }
            "typeHierarchy/subtypes" => {
                self.trace("subtypes", json!({"item": params["item"]}))?;
                if self.mode == "hierarchy_snapshot" {
                    vec![item(
                        params["item"]["name"].as_str().unwrap_or("Root"),
                        uri,
                        1,
                        2,
                        "OPAQUE_CHILD_DATA",
                    )]
                } else {
                    vec![
                        item("DerivedOne", uri, 3, 4, "OPAQUE_CHILD_ONE"),
                        item(
                            "DerivedTwo",
                            "file:///outside/external.rs",
                            7,
                            8,
                            "OPAQUE_CHILD_TWO",
                        ),
                    ]
                }
            }
            _ => return wire::reply(id, Value::Null),
        };
        wire::reply(id, json!(result))
    }
}
