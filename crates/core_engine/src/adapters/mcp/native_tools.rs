//! Native `tower_*` tool handlers — spec 10b.
//!
//! Implements all 7 workspace tools and registers them into the 10a
//! [`ToolRegistry`]. Each handler follows the same thin pattern:
//!
//! ```text
//! validate args  →  call inbound port  →  map Result to MCP value/error
//! ```
//!
//! # Wireframe
//!
//! ```text
//! register_native_tools(registry, state):
//!   tower_find_file       {query}              → SearchUseCase.find_file
//!   tower_search_text     {pattern}            → SearchUseCase.search_text
//!   tower_read_file       {path}               → FileSystemPort.read
//!   tower_create_file     {path, content}      → FileMutationUseCase.create_file
//!   tower_create_directory{path}               → FileMutationUseCase.create_directory
//!   tower_delete_file     {path}               → FileMutationUseCase.delete_file
//!   tower_global_replace  {target,replacement} → FileMutationUseCase.global_replace
//! ```
//!
//! # Design decisions
//!
//! ## tower_read_file approach
//!
//! There is no `SearchUseCase` or `FileMutationUseCase` method for reading raw
//! bytes. Decision: call `FileSystemPort::read` directly in the handler.
//! Why: `FileSystemPort::read` is the canonical outbound port for content reads;
//! adding a wrapping use-case method for a pure-passthrough would be over-
//! engineering (YAGNI). The presentation adapter is allowed to call an outbound
//! port directly when there is no domain logic involved.
//! Trade-off: the handler knows about the outbound port type — acceptable because
//! it remains inside the `adapters` layer.
//!
//! ## Arg validation approach
//!
//! Manual field-presence + type checks. No jsonschema dep.
//! Why: the arg objects are trivial (2–3 fields, all strings). A full jsonschema
//! validator (~1 MB of deps) would cost far more than it saves. The JSON schema
//! strings are published in `ToolDesc::input_schema` for client-side validation
//! (spec U1), while server-side we check only what we need.
//!
//! ## Error code scheme
//!
//! | Domain/port error      | `ToolError` variant     | JSON-RPC code | Meaning                     |
//! |------------------------|-------------------------|---------------|-----------------------------|
//! | `DomainError::NotFound`| `ResourceNotFound`      | `-32002`      | Resource not found (stable) |
//! | `PortError::NotFound`  | `ResourceNotFound`      | `-32002`      | Resource not found (stable) |
//! | `DomainError::IoError` | `ExecutionFailed`       | `-32603`      | Internal error (I/O)        |
//! | others                 | `ExecutionFailed`       | `-32603`      | Internal error (catch-all)  |
//!
//! `-32002` is in the server-defined range (`-32000..=-32099`, JSON-RPC 2.0
//! §5.1) and chosen to be stable: clients can branch on it to show a "not found"
//! message without parsing the error string.
//!
//! ## Shared handler helpers
//!
//! `require_str` — extract a required string field from an args object,
//! returning `ToolError::InvalidArgs` when absent or wrong type.
//! `domain_err_to_tool_error` — uniform `DomainError → ToolError` mapping.
//!
//! # Safety
//!
//! No unsafe code. `#[forbid(unsafe_code)]` is active on the entire domain;
//! this module adds no unsafe blocks either.

#![forbid(unsafe_code)]

use std::sync::{Arc, RwLock};

use serde_json::{json, Value};

use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::domain::index::{FileSearch, InvertedIndex};
use crate::domain::mutation::FileMutationService;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::{FileMutationUseCase, SearchUseCase};
use crate::ports::{FileSystemPort, NoOpPluginHost, StoragePort};

// ── EngineState ───────────────────────────────────────────────────────────────

/// All mutable engine state shared between the MCP tool handlers and (in
/// production) the filesystem watcher.
///
/// # Locking discipline (matches spec 06 watcher lock order)
///
/// Callers acquire the `RwLock` write-lock for any mutation and read-lock for
/// read-only queries. Short critical sections only — no blocking I/O while
/// holding the lock. The watcher (spec 06) follows the same convention so there
/// is no possibility of lock-order inversion (there is only one lock here).
pub struct EngineState {
    pub workspace: ProjectWorkspace,
    pub index: InvertedIndex,
    pub storage: Box<dyn StoragePort + Send + Sync>,
    pub fs: Box<dyn FileSystemPort + Send + Sync>,
}

impl EngineState {
    /// Create a new `EngineState` with the given components.
    pub fn new(
        workspace: ProjectWorkspace,
        index: InvertedIndex,
        storage: Box<dyn StoragePort + Send + Sync>,
        fs: Box<dyn FileSystemPort + Send + Sync>,
    ) -> Self {
        Self {
            workspace,
            index,
            storage,
            fs,
        }
    }
}

// ── NativeToolRegistry ────────────────────────────────────────────────────────

/// [`ToolRegistry`] implementation for the 7 native `tower_*` tools (spec 10b).
///
/// Holds a reference-counted, lock-protected [`EngineState`] so that both this
/// registry and the filesystem watcher can share the workspace/index/storage/fs
/// without copying.
///
/// # Example
///
/// ```rust
/// use std::sync::{Arc, RwLock};
/// use core_engine::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
/// use core_engine::adapters::mcp::ToolRegistry;
/// use core_engine::adapters::{InMemoryFs, InMemoryStorage};
/// use core_engine::domain::index::InvertedIndex;
/// use core_engine::domain::workspace::ProjectWorkspace;
///
/// let state = EngineState::new(
///     ProjectWorkspace::new(),
///     InvertedIndex::new(),
///     Box::new(InMemoryStorage::new()),
///     Box::new(InMemoryFs::new()),
/// );
/// let shared = Arc::new(RwLock::new(state));
/// let mut registry = NativeToolRegistry::new(shared);
///
/// let tools = registry.list();
/// assert_eq!(tools.len(), 7);
/// ```
pub struct NativeToolRegistry {
    state: Arc<RwLock<EngineState>>,
}

impl NativeToolRegistry {
    /// Construct a registry wrapping the given shared engine state.
    #[must_use]
    pub fn new(state: Arc<RwLock<EngineState>>) -> Self {
        Self { state }
    }
}

impl ToolRegistry for NativeToolRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        vec![
            tool_find_file_desc(),
            tool_search_text_desc(),
            tool_read_file_desc(),
            tool_create_file_desc(),
            tool_create_directory_desc(),
            tool_delete_file_desc(),
            tool_global_replace_desc(),
        ]
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
        match name {
            "tower_find_file" => call_find_file(&self.state, args),
            "tower_search_text" => call_search_text(&self.state, args),
            "tower_read_file" => call_read_file(&self.state, args),
            "tower_create_file" => call_create_file(&self.state, args),
            "tower_create_directory" => call_create_directory(&self.state, args),
            "tower_delete_file" => call_delete_file(&self.state, args),
            "tower_global_replace" => call_global_replace(&self.state, args),
            other => Err(ToolError::NotFound(other.to_owned())),
        }
    }
}

// ── Tool descriptions ─────────────────────────────────────────────────────────

fn tool_find_file_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_find_file".to_owned(),
        description: "Find files in the workspace whose path matches the query string.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Substring or fuzzy query to match against file paths."
                }
            },
            "required": ["query"]
        }),
    }
}

fn tool_search_text_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_search_text".to_owned(),
        description: "Search all indexed file contents for lines matching the pattern.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Text pattern to search for across all indexed files."
                }
            },
            "required": ["pattern"]
        }),
    }
}

fn tool_read_file_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_read_file".to_owned(),
        description: "Read the raw UTF-8 content of a file at the given workspace-relative path."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path to the file to read."
                }
            },
            "required": ["path"]
        }),
    }
}

fn tool_create_file_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_create_file".to_owned(),
        description: "Create or overwrite a file at the given path with the provided content."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path for the new or existing file."
                },
                "content": {
                    "type": "string",
                    "description": "UTF-8 content to write to the file."
                }
            },
            "required": ["path", "content"]
        }),
    }
}

fn tool_create_directory_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_create_directory".to_owned(),
        description: "Create a directory at the given workspace-relative path (recursive mkdir)."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the directory to create."
                }
            },
            "required": ["path"]
        }),
    }
}

fn tool_delete_file_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_delete_file".to_owned(),
        description: "Delete a file from the workspace at the given path.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to delete."
                }
            },
            "required": ["path"]
        }),
    }
}

fn tool_global_replace_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_global_replace".to_owned(),
        description: "Replace every occurrence of a target string with a replacement string across all indexed files.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "The string to search for and replace."
                },
                "replacement": {
                    "type": "string",
                    "description": "The string to substitute in place of every occurrence of target."
                }
            },
            "required": ["target", "replacement"]
        }),
    }
}

// ── Handler implementations ───────────────────────────────────────────────────

fn call_find_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let query = require_str(&args, "query")?;

    let guard = state.read().map_err(lock_poisoned)?;
    let search = FileSearch::new(&guard.index, &guard.workspace);
    let paths = search.find_file(query).map_err(domain_err_to_tool_error)?;

    let paths_json: Vec<Value> = paths
        .iter()
        .map(|p| Value::String(p.as_str().to_owned()))
        .collect();
    Ok(json!({ "paths": paths_json }))
}

fn call_search_text(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let pattern = require_str(&args, "pattern")?;

    let guard = state.read().map_err(lock_poisoned)?;
    // FileSearch::with_fs requires `FileSystemPort + Sync`. The EngineState
    // fs field is `Box<dyn FileSystemPort + Send + Sync>`, which satisfies the
    // `Sync` bound by coercion to `&dyn FileSystemPort + Sync`.
    let search = FileSearch::new(&guard.index, &guard.workspace).with_fs(guard.fs.as_ref());
    let matches = search
        .search_text(pattern)
        .map_err(domain_err_to_tool_error)?;

    let matches_json: Vec<Value> = matches
        .iter()
        .map(|m| {
            json!({
                "path": m.path.as_str(),
                "line_number": m.line_number,
                "line_content": m.line_content
            })
        })
        .collect();
    Ok(json!({ "matches": matches_json }))
}

fn call_read_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let rel = RelativePath::new(path);

    let guard = state.read().map_err(lock_poisoned)?;
    let bytes = guard.fs.read(&rel).map_err(port_err_to_tool_error)?;
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Ok(json!({ "content": content }))
}

/// Map a [`PortError`] to a [`ToolError`], preserving the stable not-found code.
///
/// [`PortError::NotFound`] maps to [`ToolError::ResourceNotFound`] (→ `-32002`)
/// to match the behaviour of [`domain_err_to_tool_error`]. Other errors map to
/// [`ToolError::ExecutionFailed`] (→ `-32603`).
fn port_err_to_tool_error(err: crate::ports::PortError) -> ToolError {
    match err {
        crate::ports::PortError::NotFound => {
            ToolError::ResourceNotFound("path or entity not found in workspace".to_owned())
        }
        other => ToolError::ExecutionFailed(other.to_string()),
    }
}

fn call_create_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let content = require_str(&args, "content")?;

    let rel = RelativePath::new(path);
    let bytes = content.as_bytes().to_vec();

    let mut guard = state.write().map_err(lock_poisoned)?;
    let EngineState {
        workspace,
        index,
        storage,
        fs,
    } = &mut *guard;
    let mut svc = FileMutationService::new(
        fs.as_mut(),
        workspace,
        index,
        storage.as_mut(),
        &NoOpPluginHost,
    );
    svc.create_file(rel, bytes)
        .map_err(domain_err_to_tool_error)?;

    Ok(json!({ "created": true }))
}

fn call_create_directory(
    state: &Arc<RwLock<EngineState>>,
    args: Value,
) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let rel = RelativePath::new(path);

    let mut guard = state.write().map_err(lock_poisoned)?;
    let EngineState {
        workspace,
        index,
        storage,
        fs,
    } = &mut *guard;
    let mut svc = FileMutationService::new(
        fs.as_mut(),
        workspace,
        index,
        storage.as_mut(),
        &NoOpPluginHost,
    );
    svc.create_directory(rel)
        .map_err(domain_err_to_tool_error)?;

    Ok(json!({ "created": true }))
}

fn call_delete_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let rel = RelativePath::new(path);

    let mut guard = state.write().map_err(lock_poisoned)?;
    let EngineState {
        workspace,
        index,
        storage,
        fs,
    } = &mut *guard;
    let mut svc = FileMutationService::new(
        fs.as_mut(),
        workspace,
        index,
        storage.as_mut(),
        &NoOpPluginHost,
    );
    svc.delete_file(&rel).map_err(domain_err_to_tool_error)?;

    Ok(json!({ "deleted": true }))
}

fn call_global_replace(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let target = require_str(&args, "target")?;
    let replacement = require_str(&args, "replacement")?;

    let mut guard = state.write().map_err(lock_poisoned)?;
    let EngineState {
        workspace,
        index,
        storage,
        fs,
    } = &mut *guard;
    let mut svc = FileMutationService::new(
        fs.as_mut(),
        workspace,
        index,
        storage.as_mut(),
        &NoOpPluginHost,
    );
    let report = svc
        .global_replace(target, replacement)
        .map_err(domain_err_to_tool_error)?;

    let errors_json: Vec<Value> = report
        .errors
        .iter()
        .map(|e| json!({ "path": e.path.as_str(), "reason": e.reason }))
        .collect();

    Ok(json!({
        "files_changed": report.files_changed,
        "replacements": report.replacements,
        "errors": errors_json
    }))
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Extract a required string field from a JSON args object.
///
/// Returns `ToolError::InvalidArgs` when the field is absent or not a string.
/// The returned `&str` borrows from `args` for the duration of the call.
fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        ToolError::InvalidArgs(format!(
            "required field '{field}' is missing or not a string"
        ))
    })
}

/// Map a [`DomainError`] to a [`ToolError`] with a stable, inspectable code.
///
/// # Stable code assignment
///
/// | Variant              | ToolError variant       | JSON-RPC code |
/// |----------------------|-------------------------|---------------|
/// | `NotFound`           | `ResourceNotFound`      | -32002        |
/// | others               | `ExecutionFailed`       | -32603        |
///
/// `DomainError::NotFound` maps to [`ToolError::ResourceNotFound`] which the
/// transport maps to `-32002`. Clients branch on this stable code to detect
/// "resource not found" without parsing the error string (spec 10b AC5).
fn domain_err_to_tool_error(err: DomainError) -> ToolError {
    match err {
        DomainError::NotFound => ToolError::ResourceNotFound(err.to_string()),
        other => ToolError::ExecutionFailed(other.to_string()),
    }
}

/// Convert a lock-poisoning event into a [`ToolError`].
///
/// A poisoned lock means a previous writer panicked while holding it. The
/// server should not crash — surfacing it as an execution failure lets the
/// transport return a proper JSON-RPC error to the client.
fn lock_poisoned<G>(_: std::sync::PoisonError<G>) -> ToolError {
    ToolError::ExecutionFailed("engine state lock is poisoned".to_owned())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use serde_json::{json, Value};

    use super::{EngineState, NativeToolRegistry};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::index::InvertedIndex;
    use crate::domain::token::tokenize;
    use crate::domain::virtual_file::FileMetadata;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::domain::RelativePath;
    use crate::ports::FileSystemPort;

    // ── Fixture helpers ───────────────────────────────────────────────────────

    fn empty_state() -> Arc<RwLock<EngineState>> {
        Arc::new(RwLock::new(EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(InMemoryFs::new()),
        )))
    }

    /// State with one file indexed: `src/client.rs` containing `"fn client() {}"`.
    fn state_with_client_file() -> Arc<RwLock<EngineState>> {
        let mut workspace = ProjectWorkspace::new();
        let mut index = InvertedIndex::new();
        let mut fs = InMemoryFs::new();
        let storage = InMemoryStorage::new();

        let path = RelativePath::new("src/client.rs");
        let id = workspace
            .insert(path.clone(), FileMetadata::default())
            .unwrap();
        index.insert(id, &tokenize("src/client.rs"));
        fs.write(path, b"fn client() {}".to_vec()).unwrap();

        Arc::new(RwLock::new(EngineState::new(
            workspace,
            index,
            Box::new(storage),
            Box::new(fs),
        )))
    }

    fn make_registry(state: Arc<RwLock<EngineState>>) -> NativeToolRegistry {
        NativeToolRegistry::new(state)
    }

    // ── AC1: tools/list shows 7 tools with schemas ────────────────────────────

    #[test]
    fn ac1_tools_list_returns_seven_tower_tools() {
        let reg = make_registry(empty_state());
        let tools = reg.list();
        assert_eq!(
            tools.len(),
            7,
            "expected 7 native tools; got {}",
            tools.len()
        );
    }

    #[test]
    fn ac1_all_seven_tool_names_present() {
        let reg = make_registry(empty_state());
        let tool_list = reg.list();
        let names: Vec<&str> = tool_list.iter().map(|t| t.name.as_str()).collect();
        let expected = [
            "tower_find_file",
            "tower_search_text",
            "tower_read_file",
            "tower_create_file",
            "tower_create_directory",
            "tower_delete_file",
            "tower_global_replace",
        ];
        for name in &expected {
            assert!(names.contains(name), "missing tool '{name}'; got {names:?}");
        }
    }

    #[test]
    fn ac1_each_tool_has_a_non_empty_input_schema() {
        let reg = make_registry(empty_state());
        for tool in reg.list() {
            assert!(
                tool.input_schema.is_object(),
                "tool '{}' must have an object inputSchema",
                tool.name
            );
            // Schema must declare at least a "type" or "properties" key.
            let schema = &tool.input_schema;
            assert!(
                schema.get("type").is_some() || schema.get("properties").is_some(),
                "tool '{}' schema missing 'type' or 'properties': {schema}",
                tool.name
            );
        }
    }

    // ── AC2: tower_find_file round-trip ───────────────────────────────────────

    #[test]
    fn ac2_find_file_returns_matching_paths() {
        let mut reg = make_registry(state_with_client_file());
        let result = reg.call("tower_find_file", json!({ "query": "client" }));
        let val = result.expect("find_file must succeed");
        let paths = val["paths"]
            .as_array()
            .expect("result must have 'paths' array");
        let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
        assert!(
            path_strings.contains(&"src/client.rs"),
            "expected src/client.rs in paths; got {path_strings:?}"
        );
    }

    #[test]
    fn ac2_find_file_returns_empty_for_no_match() {
        let mut reg = make_registry(state_with_client_file());
        let val = reg
            .call("tower_find_file", json!({ "query": "zzznomatch" }))
            .unwrap();
        let paths = val["paths"].as_array().unwrap();
        assert!(paths.is_empty(), "no-match query must return empty paths");
    }

    // ── AC3: tower_create_file then tower_find_file finds it ─────────────────

    #[test]
    fn ac3_create_file_then_find_file_locates_new_file() {
        let state = empty_state();
        let mut reg = make_registry(Arc::clone(&state));

        // Create.
        reg.call(
            "tower_create_file",
            json!({ "path": "src/widget.rs", "content": "pub struct Widget;" }),
        )
        .expect("create_file must succeed");

        // Find.
        let val = reg
            .call("tower_find_file", json!({ "query": "widget" }))
            .unwrap();
        let paths = val["paths"].as_array().unwrap();
        let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
        assert!(
            path_strings.contains(&"src/widget.rs"),
            "newly created file must be findable; got {path_strings:?}"
        );
    }

    #[test]
    fn ac3_create_file_content_is_readable_via_read_file() {
        let state = empty_state();
        let mut reg = make_registry(Arc::clone(&state));

        reg.call(
            "tower_create_file",
            json!({ "path": "src/readme.md", "content": "# Hello" }),
        )
        .unwrap();

        let val = reg
            .call("tower_read_file", json!({ "path": "src/readme.md" }))
            .unwrap();
        assert_eq!(
            val["content"].as_str().unwrap(),
            "# Hello",
            "read_file must return the content written by create_file"
        );
    }

    // ── AC4: malformed args → invalid-params, no state change ─────────────────

    #[test]
    fn ac4_missing_query_returns_invalid_args() {
        let mut reg = make_registry(empty_state());
        let err = reg.call("tower_find_file", json!({})).unwrap_err();
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "missing 'query' must return InvalidArgs"
        );
    }

    #[test]
    fn ac4_missing_path_for_create_file_returns_invalid_args() {
        let mut reg = make_registry(empty_state());
        let err = reg
            .call("tower_create_file", json!({ "content": "hi" }))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn ac4_missing_content_for_create_file_returns_invalid_args() {
        let mut reg = make_registry(empty_state());
        let err = reg
            .call("tower_create_file", json!({ "path": "a.rs" }))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn ac4_invalid_args_cause_no_state_change() {
        let state = empty_state();
        let mut reg = make_registry(Arc::clone(&state));

        // Attempt create with missing content — must fail.
        let _ = reg.call("tower_create_file", json!({ "path": "ghost.rs" }));

        // State must be unchanged: the file must NOT exist.
        let val = reg
            .call("tower_find_file", json!({ "query": "ghost" }))
            .unwrap();
        let paths = val["paths"].as_array().unwrap();
        assert!(
            paths.is_empty(),
            "failed create must not leave ghost file in workspace"
        );
    }

    // ── AC5: tower_delete_file on missing file → stable-code error ───────────

    #[test]
    fn ac5_delete_missing_file_returns_not_found_error() {
        let mut reg = make_registry(empty_state());
        let err = reg
            .call("tower_delete_file", json!({ "path": "nonexistent.rs" }))
            .unwrap_err();

        // Must be ResourceNotFound (maps to -32002 on the wire).
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::ResourceNotFound(_)),
            "delete-missing must return ResourceNotFound for stable -32002 client detection; got: {err:?}"
        );
    }

    // ── tower_search_text round-trip ──────────────────────────────────────────

    #[test]
    fn search_text_finds_content_in_indexed_file() {
        let mut reg = make_registry(state_with_client_file());
        let val = reg
            .call("tower_search_text", json!({ "pattern": "client" }))
            .unwrap();
        let matches = val["matches"]
            .as_array()
            .expect("must have 'matches' array");
        assert!(
            !matches.is_empty(),
            "search_text must find 'client' in src/client.rs"
        );
        let first = &matches[0];
        assert_eq!(first["path"], "src/client.rs");
        assert!(first["line_number"].as_u64().unwrap() >= 1);
    }

    // ── tower_delete_file success + workspace cleanup ─────────────────────────

    #[test]
    fn delete_file_removes_from_workspace() {
        let state = state_with_client_file();
        let mut reg = make_registry(Arc::clone(&state));

        reg.call("tower_delete_file", json!({ "path": "src/client.rs" }))
            .expect("delete of existing file must succeed");

        // File must no longer appear in find.
        let val = reg
            .call("tower_find_file", json!({ "query": "client" }))
            .unwrap();
        let paths = val["paths"].as_array().unwrap();
        assert!(paths.is_empty(), "deleted file must not be findable");
    }

    // ── tower_create_directory ────────────────────────────────────────────────

    #[test]
    fn create_directory_succeeds_on_empty_state() {
        let mut reg = make_registry(empty_state());
        let val = reg
            .call("tower_create_directory", json!({ "path": "a/b/c" }))
            .unwrap();
        assert_eq!(val["created"], true);
    }

    // ── tower_global_replace ──────────────────────────────────────────────────

    #[test]
    fn global_replace_rewrites_content() {
        let state = state_with_client_file();
        let mut reg = make_registry(Arc::clone(&state));

        let val = reg
            .call(
                "tower_global_replace",
                json!({ "target": "client", "replacement": "server" }),
            )
            .unwrap();

        assert!(
            val["files_changed"].as_u64().unwrap() >= 1,
            "global_replace must report at least one file changed"
        );

        // Content must have been updated.
        let read_val = reg
            .call("tower_read_file", json!({ "path": "src/client.rs" }))
            .unwrap();
        let content = read_val["content"].as_str().unwrap();
        assert!(
            content.contains("server"),
            "content must contain replacement; got: {content}"
        );
    }

    // ── unknown tool ──────────────────────────────────────────────────────────

    #[test]
    fn unknown_tool_name_returns_not_found() {
        let mut reg = make_registry(empty_state());
        let err = reg.call("tower_unknown_tool", json!({})).unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::NotFound(_)
        ));
    }
}
