//! `LspToolRegistry` — the `tower_lsp_diagnostics` MCP tool (spec: code-intel Plan 1).
//!
//! A thin [`ToolRegistry`] over a [`CodeIntelligencePort`]. It reads the target
//! file's current bytes from the shared [`EngineState`]'s `FileSystemPort`, runs
//! `check`, and serialises diagnostics. `Unsupported` is reported as a normal
//! result (`"supported": false`) so agents can branch without parsing errors.

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

use crate::adapters::mcp::diagnostics_json::{
    DiagnosticJson, diagnostics_response_json, unsupported_diagnostics_json,
};
use crate::adapters::mcp::lsp_support::{read_text, require_str};
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::domain::RelativePath;
use crate::ports::{CodeIntelError, CodeIntelligencePort};

// ── SubscriptionRegistry ──────────────────────────────────────────────────────

/// Thread-safe set of MCP resource URIs the client has subscribed to.
///
/// Shared between the push-forwarding task (reads `is_subscribed` on each push
/// event) and the `resources/subscribe`/`resources/unsubscribe` handler methods
/// (writes). Held only for `HashSet` operations (microseconds); never held
/// while doing any session I/O.
pub struct SubscriptionRegistry {
    subscribed: std::collections::HashSet<String>,
}

impl SubscriptionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            subscribed: std::collections::HashSet::new(),
        }
    }

    pub fn subscribe(&mut self, uri: &str) {
        self.subscribed.insert(uri.to_owned());
    }

    pub fn unsubscribe(&mut self, uri: &str) {
        self.subscribed.remove(uri);
    }

    #[must_use]
    pub fn is_subscribed(&self, uri: &str) -> bool {
        self.subscribed.contains(uri)
    }

    pub fn clear(&mut self) {
        self.subscribed.clear();
    }
}

impl Default for SubscriptionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── LspToolRegistry ───────────────────────────────────────────────────────────

/// Tool registry exposing `tower_lsp_diagnostics`.
pub struct LspToolRegistry {
    state: Arc<RwLock<EngineState>>,
    code_intel: Arc<dyn CodeIntelligencePort>,
}

impl LspToolRegistry {
    #[must_use]
    pub fn new(state: Arc<RwLock<EngineState>>, code_intel: Arc<dyn CodeIntelligencePort>) -> Self {
        Self { state, code_intel }
    }
}

fn tool_lsp_diagnostics_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_lsp_diagnostics".to_owned(),
        description: "Run the configured language server over a file's current content and return \
                      compiler/linter diagnostics (errors and warnings). Use after editing a file \
                      to verify the change did not break it."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to analyze."
                }
            },
            "required": ["path"]
        }),
    }
}

impl ToolRegistry for LspToolRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        vec![tool_lsp_diagnostics_desc()]
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        if name != "tower_lsp_diagnostics" {
            return Err(ToolError::NotFound(name.to_owned()));
        }
        let path = require_str(&args, "path")?;
        let rel = RelativePath::new(path);

        // Read current content via the shared FileSystemPort.
        let text = read_text(&self.state, &rel)?;

        match self.code_intel.check(&rel, &text) {
            Ok(diags) => {
                let diagnostics: Vec<DiagnosticJson<'_>> = diags
                    .iter()
                    .map(|diagnostic| DiagnosticJson {
                        path: None,
                        diagnostic,
                    })
                    .collect();
                Ok(diagnostics_response_json(true, &diagnostics))
            }
            Err(CodeIntelError::Unsupported) => Ok(unsupported_diagnostics_json()),
            Err(CodeIntelError::Backend(msg)) => Err(ToolError::ExecutionFailed(msg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use serde_json::json;

    use super::{LspToolRegistry, SubscriptionRegistry};
    use crate::adapters::mcp::native_tools::EngineState;
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryCodeIntel, InMemoryFs, InMemoryStorage};
    use crate::domain::RelativePath;
    use crate::domain::code_intel::{Diagnostic, Position, Range, Severity};
    use crate::domain::index::InvertedIndex;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::ports::{CodeIntelError, CodeIntelligencePort, FileSystemPort};

    struct InformationCodeIntel;

    impl CodeIntelligencePort for InformationCodeIntel {
        fn check(
            &self,
            _path: &RelativePath,
            _text: &str,
        ) -> Result<Vec<Diagnostic>, CodeIntelError> {
            Ok(vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 2,
                    },
                    end: Position {
                        line: 1,
                        character: 6,
                    },
                },
                severity: Severity::Information,
                message: "informational diagnostic".to_owned(),
                source: Some("test-lsp".to_owned()),
                code: Some("I0001".to_owned()),
            }])
        }
    }

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

    fn registry(state: Arc<RwLock<EngineState>>) -> LspToolRegistry {
        LspToolRegistry::new(state, Arc::new(InMemoryCodeIntel::new()))
    }

    #[test]
    fn lists_one_tool() {
        let reg = registry(state_with_file("src/a.rs", b"fn main() {}"));
        let tool_list = reg.list();
        let names: Vec<&str> = tool_list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["tower_lsp_diagnostics"]);
    }

    #[test]
    fn clean_file_reports_supported_no_diagnostics() {
        let mut reg = registry(state_with_file("src/a.rs", b"fn main() {}"));
        let val = reg
            .call("tower_lsp_diagnostics", json!({ "path": "src/a.rs" }))
            .unwrap();
        assert_eq!(val["supported"], true);
        assert!(val["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn broken_file_reports_error_diagnostic() {
        let mut reg = registry(state_with_file("src/a.rs", b"fn main() { //!ERR\n}"));
        let val = reg
            .call("tower_lsp_diagnostics", json!({ "path": "src/a.rs" }))
            .unwrap();
        let diags = val["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["severity"], "error");
        assert!(diags[0].get("path").is_none());
    }

    #[test]
    fn diagnostics_tool_serializes_information_severity_as_info() {
        let state = state_with_file("src/a.rs", b"fn main() {}");
        let mut reg = LspToolRegistry::new(state, Arc::new(InformationCodeIntel));

        let val = reg
            .call("tower_lsp_diagnostics", json!({ "path": "src/a.rs" }))
            .unwrap();
        let diags = val["diagnostics"].as_array().unwrap();

        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["severity"], "info");
        assert_ne!(diags[0]["severity"], "information");
        assert!(diags[0].get("path").is_none());
    }

    #[test]
    fn unsupported_extension_reports_supported_false() {
        let mut reg = registry(state_with_file("notes.txt", b"//!ERR"));
        let val = reg
            .call("tower_lsp_diagnostics", json!({ "path": "notes.txt" }))
            .unwrap();
        assert_eq!(val["supported"], false);
        assert!(val["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn missing_path_arg_is_invalid_args() {
        let mut reg = registry(state_with_file("src/a.rs", b"fn main() {}"));
        let err = reg.call("tower_lsp_diagnostics", json!({})).unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn missing_file_is_resource_not_found() {
        let mut reg = registry(state_with_file("src/a.rs", b"fn main() {}"));
        let err = reg
            .call("tower_lsp_diagnostics", json!({ "path": "src/ghost.rs" }))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::ResourceNotFound(_)
        ));
    }

    #[test]
    fn lsp_tools_uses_shared_diagnostics_json_serializer() {
        let source = include_str!("lsp_tools.rs");

        assert!(
            source.contains(concat!("diagnostics_response_json", "(true, &diagnostics)")),
            "tower_lsp_diagnostics must serialize through diagnostics_response_json"
        );
        assert!(
            source.contains(concat!("unsupported_diagnostics_json", "()")),
            "unsupported diagnostics must serialize through unsupported_diagnostics_json"
        );
        assert!(
            !source.contains(concat!("fn ", "diagnostic_to_json")),
            "lsp_tools.rs must not retain an LSP-only diagnostic serializer"
        );
    }

    #[test]
    fn subscription_registry_subscribe_unsubscribe() {
        let mut reg = SubscriptionRegistry::new();
        reg.subscribe("file:///a.rs");
        assert!(reg.is_subscribed("file:///a.rs"));
        assert!(!reg.is_subscribed("file:///b.rs"));
        reg.unsubscribe("file:///a.rs");
        assert!(!reg.is_subscribed("file:///a.rs"));
    }

    #[test]
    fn subscription_registry_clear() {
        let mut reg = SubscriptionRegistry::new();
        reg.subscribe("file:///a.rs");
        reg.subscribe("file:///b.rs");
        reg.clear();
        assert!(!reg.is_subscribed("file:///a.rs"));
        assert!(!reg.is_subscribed("file:///b.rs"));
    }
}
