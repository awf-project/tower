//! `NavToolRegistry` — the `tower_lsp_definition` / `tower_lsp_references` / `tower_lsp_hover`
//! MCP tools (spec 14b).
//!
//! Each tool reads the target file's current bytes from the shared
//! [`EngineState`], asks the [`NavigationPort`] for the answer at a
//! `{line, character}` (UTF-16) position, and serialises the result.
//!
//! # No-session fallback (O1/AC9)
//!
//! The registry holds an `Option<Arc<dyn NavigationPort>>`. When no language
//! server is configured (`None`) or the file's language is `Unsupported`, the
//! tool returns `{"supported": false, …}` rather than erroring — the agent can
//! then fall back to the structural `tower_ast_*` tools, which are exposed
//! independently. (Richer in-tool tree-sitter fallback — converting the UTF-16
//! position to a byte offset via `PositionMap` and querying the AST plugin
//! directly — is deferred; it needs a host-side route into the WASM AST tools.)

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

use crate::adapters::mcp::lsp_support::{read_text, require_str, require_u32};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::domain::RelativePath;
use crate::domain::code_intel::{Hover, Location, Position};
use crate::ports::{CodeIntelError, NavigationPort};

/// Tool registry exposing the three navigation tools.
pub struct NavToolRegistry {
    state: Arc<RwLock<EngineState>>,
    /// The navigation backend, or `None` when no language server is configured.
    nav: Option<Arc<dyn NavigationPort>>,
}

impl NavToolRegistry {
    #[must_use]
    pub fn new(state: Arc<RwLock<EngineState>>, nav: Option<Arc<dyn NavigationPort>>) -> Self {
        Self { state, nav }
    }
}

fn location_to_json(loc: &Location) -> Value {
    json!({
        "path": loc.path.as_str(),
        "line": loc.range.start.line,
        "character": loc.range.start.character,
        "endLine": loc.range.end.line,
        "endCharacter": loc.range.end.character,
    })
}

fn hover_to_json(hover: &Hover) -> Value {
    let mut obj = json!({ "contents": hover.contents });
    if let Some(range) = hover.range {
        obj["line"] = json!(range.start.line);
        obj["character"] = json!(range.start.character);
        obj["endLine"] = json!(range.end.line);
        obj["endCharacter"] = json!(range.end.character);
    }
    obj
}

fn position_tool_desc(name: &str, description: &str) -> ToolDesc {
    ToolDesc {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path of the file." },
                "line": { "type": "integer", "description": "Zero-based line number." },
                "character": {
                    "type": "integer",
                    "description": "Zero-based UTF-16 code-unit column."
                }
            },
            "required": ["path", "line", "character"]
        }),
    }
}

fn tool_descs() -> Vec<ToolDesc> {
    vec![
        position_tool_desc(
            "tower_lsp_definition",
            "Resolve the definition site(s) of the symbol at a position, using the configured \
             language server. Returns workspace-relative locations.",
        ),
        position_tool_desc(
            "tower_lsp_references",
            "Find all reference sites of the symbol at a position, using the configured language \
             server.",
        ),
        position_tool_desc(
            "tower_lsp_hover",
            "Get hover information (type/doc) for the symbol at a position, using the configured \
             language server.",
        ),
    ]
}

impl ToolRegistry for NavToolRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        tool_descs()
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        // The result key under which each tool nests its data.
        let result_key = match name {
            "tower_lsp_definition" => "locations",
            "tower_lsp_references" => "references",
            "tower_lsp_hover" => "hover",
            other => return Err(ToolError::NotFound(other.to_owned())),
        };

        // Validate arguments regardless of backend presence.
        let path = require_str(&args, "path")?;
        let line = require_u32(&args, "line")?;
        let character = require_u32(&args, "character")?;
        let rel = RelativePath::new(path);
        let position = Position { line, character };

        // No backend → supported:false (AC9), with the right empty shape.
        let Some(nav) = self.nav.clone() else {
            return Ok(unsupported_result(result_key));
        };

        let text = read_text(&self.state, &rel)?;

        match name {
            "tower_lsp_hover" => match nav.hover(&rel, &text, position) {
                Ok(hover) => Ok(json!({
                    "supported": true,
                    "hover": hover.as_ref().map(hover_to_json),
                })),
                Err(CodeIntelError::Unsupported) => Ok(unsupported_result("hover")),
                Err(CodeIntelError::Backend(msg)) => Err(ToolError::ExecutionFailed(msg)),
            },
            _ => {
                let result = if name == "tower_lsp_definition" {
                    nav.definition(&rel, &text, position)
                } else {
                    nav.references(&rel, &text, position)
                };
                match result {
                    Ok(locs) => {
                        let arr: Vec<Value> = locs.iter().map(location_to_json).collect();
                        Ok(json!({ "supported": true, result_key: arr }))
                    }
                    Err(CodeIntelError::Unsupported) => Ok(unsupported_result(result_key)),
                    Err(CodeIntelError::Backend(msg)) => Err(ToolError::ExecutionFailed(msg)),
                }
            }
        }
    }
}

/// The `supported:false` result for a tool, with its data key set empty.
fn unsupported_result(result_key: &str) -> Value {
    if result_key == "hover" {
        json!({ "supported": false, "hover": Value::Null })
    } else {
        json!({ "supported": false, result_key: [] })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use serde_json::json;

    use super::NavToolRegistry;
    use crate::adapters::mcp::native_tools::EngineState;
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::mcp::types::ToolError;
    use crate::adapters::{InMemoryCodeIntel, InMemoryFs, InMemoryStorage};
    use crate::domain::RelativePath;
    use crate::domain::index::InvertedIndex;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::ports::{FileSystemPort, NavigationPort};

    fn state_with_file(path: &str, content: &[u8]) -> Arc<RwLock<EngineState>> {
        let mut fs = InMemoryFs::new();
        fs.write(RelativePath::new(path), content.to_vec()).unwrap();
        Arc::new(RwLock::new(EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(fs),
        )))
    }

    fn with_fake_nav(state: Arc<RwLock<EngineState>>) -> NavToolRegistry {
        let nav: Arc<dyn NavigationPort> = Arc::new(InMemoryCodeIntel::new());
        NavToolRegistry::new(state, Some(nav))
    }

    #[test]
    fn lists_three_tools() {
        let reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let names: Vec<String> = reg.list().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "tower_lsp_definition",
                "tower_lsp_references",
                "tower_lsp_hover"
            ]
        );
    }

    #[test]
    fn definition_returns_supported_locations() {
        let mut reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let val = reg
            .call(
                "tower_lsp_definition",
                json!({ "path": "src/a.rs", "line": 0, "character": 3 }),
            )
            .unwrap();
        assert_eq!(val["supported"], true);
        let locs = val["locations"].as_array().unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0]["path"], "src/a.rs");
        assert_eq!(locs[0]["line"], 0);
        assert_eq!(locs[0]["character"], 3);
    }

    #[test]
    fn references_returns_supported_list() {
        let mut reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let val = reg
            .call(
                "tower_lsp_references",
                json!({ "path": "src/a.rs", "line": 1, "character": 0 }),
            )
            .unwrap();
        assert_eq!(val["supported"], true);
        assert_eq!(val["references"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn hover_returns_contents() {
        let mut reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let val = reg
            .call(
                "tower_lsp_hover",
                json!({ "path": "src/a.rs", "line": 0, "character": 0 }),
            )
            .unwrap();
        assert_eq!(val["supported"], true);
        assert!(!val["hover"]["contents"].as_str().unwrap().is_empty());
    }

    #[test]
    fn unsupported_extension_reports_supported_false() {
        let mut reg = with_fake_nav(state_with_file("notes.txt", b"hi"));
        let val = reg
            .call(
                "tower_lsp_definition",
                json!({ "path": "notes.txt", "line": 0, "character": 0 }),
            )
            .unwrap();
        assert_eq!(val["supported"], false);
        assert!(val["locations"].as_array().unwrap().is_empty());
    }

    /// AC9: with no navigation backend, the tool reports `supported:false`
    /// (the agent falls back to `tower_ast_*`), never an error.
    #[test]
    fn no_backend_reports_supported_false() {
        let state = state_with_file("src/a.rs", b"fn main() {}");
        let mut reg = NavToolRegistry::new(state, None);
        let val = reg
            .call(
                "tower_lsp_definition",
                json!({ "path": "src/a.rs", "line": 0, "character": 0 }),
            )
            .unwrap();
        assert_eq!(val["supported"], false);
    }

    #[test]
    fn missing_path_is_invalid_args() {
        let mut reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let err = reg
            .call("tower_lsp_definition", json!({ "line": 0, "character": 0 }))
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn missing_line_is_invalid_args() {
        let mut reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let err = reg
            .call(
                "tower_lsp_definition",
                json!({ "path": "src/a.rs", "character": 0 }),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn unknown_tool_is_not_found() {
        let mut reg = with_fake_nav(state_with_file("src/a.rs", b"fn main() {}"));
        let err = reg.call("lsp_nope", json!({})).unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }
}
