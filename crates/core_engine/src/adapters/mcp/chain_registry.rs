//! `ChainRegistry` — compose N `ToolRegistry`s into one surface.
//!
//! `list` concatenates; `call` tries each registry in order, returning the
//! first non-`NotFound` result. Used to add `tower_lsp_diagnostics` alongside the
//! native + plugin `MergedRegistry` without modifying it.

#![forbid(unsafe_code)]

use serde_json::Value;

use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};

/// A `ToolRegistry` that delegates to an ordered list of inner registries.
///
/// `list` returns the concatenation of every inner registry's `list`.
/// `call` tries each registry in order; the first non-`NotFound` result
/// (success or a non-`NotFound` error) is returned. If all registries
/// return `NotFound`, a final `NotFound` is returned.
pub struct ChainRegistry {
    inner: Vec<Box<dyn ToolRegistry + Send>>,
}

impl ChainRegistry {
    /// Build a chain from the given ordered list of registries.
    #[must_use]
    pub fn new(inner: Vec<Box<dyn ToolRegistry + Send>>) -> Self {
        Self { inner }
    }
}

impl ToolRegistry for ChainRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        self.inner.iter().flat_map(|r| r.list()).collect()
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        for reg in &mut self.inner {
            match reg.call(name, args.clone()) {
                Err(ToolError::NotFound(_)) => continue,
                other => return other,
            }
        }
        Err(ToolError::NotFound(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use serde_json::json;

    use super::ChainRegistry;
    use crate::adapters::mcp::lsp_tools::LspToolRegistry;
    use crate::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryCodeIntel, InMemoryFs, InMemoryStorage};
    use crate::domain::index::InvertedIndex;
    use crate::domain::workspace::ProjectWorkspace;

    #[test]
    fn chain_lists_native_plus_lsp_tools() {
        let state = Arc::new(RwLock::new(EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(InMemoryFs::new()),
        )));
        let chain = ChainRegistry::new(vec![
            Box::new(NativeToolRegistry::new(Arc::clone(&state))),
            Box::new(LspToolRegistry::new(
                Arc::clone(&state),
                Arc::new(InMemoryCodeIntel::new()),
            )),
        ]);
        let names: Vec<String> = chain.list().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "tower_find_file"));
        assert!(names.iter().any(|n| n == "tower_lsp_diagnostics"));
    }

    #[test]
    fn chain_routes_unknown_to_not_found() {
        let state = Arc::new(RwLock::new(EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(InMemoryFs::new()),
        )));
        let mut chain = ChainRegistry::new(vec![Box::new(LspToolRegistry::new(
            state,
            Arc::new(InMemoryCodeIntel::new()),
        ))]);
        let err = chain.call("nope", json!({})).unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::NotFound(_)
        ));
    }
}
