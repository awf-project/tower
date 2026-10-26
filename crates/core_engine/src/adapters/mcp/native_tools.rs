//! Native `tower_*` tool handlers — spec 10b.
//!
//! Registers the native workspace tools into the 10a
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
//!   tower_list_dir        {path,recursive}     → SearchUseCase.list_dir
//!   tower_search_text     {pattern}            → SearchUseCase.search_text
//!   tower_read_file       {path}               → FileSystemPort.read
//!   tower_create_file     {path, content}      → FileMutationUseCase.create_file
//!   tower_create_directory{path}               → FileMutationUseCase.create_directory
//!   tower_delete_file     {path}               → FileMutationUseCase.delete_file
//!   tower_global_replace  {target,replacement} → FileMutationUseCase.global_replace
//!   tower_edit_range      {path,start,end,rep} → FileMutationUseCase.edit_range
//!   tower_reindex         {}                   → full VFS + text-index rebuild from fs
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

use std::num::NonZeroUsize;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, RwLock};

use extension_protocol::messages as wire;
use serde_json::{Value, json};

use crate::adapters::fs::WorkspacePathExclusion;
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::adapters::watcher::RenameCoordinator;
use crate::domain::index::{FileSearch, InvertedIndex};
use crate::domain::mutation::{FileMutationService, compute_content_version};
use crate::domain::refactor::{
    PathRenameError, PathRenamePlan, PathRenameRequest, PathRenameResult, PathRenameSemanticPolicy,
    PathRenameStatus, SafeDeleteService, SafeDeleteSnapshot,
};
use crate::domain::token::tokenize;
use crate::domain::virtual_file::FileMetadata;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, FileId, RelativePath};
use crate::ports::inbound::{
    DirectoryEntryKind, FileMutationUseCase, ListDirRequest, SafeDeleteUseCase, SearchUseCase,
    WorkspaceEditSpan,
};
use crate::ports::{
    ExtensionHostPort, FileSystemPort, NoOpExtensionHost, PathExclusionDecision,
    PathExclusionError, PathExclusionPort, StoragePort,
};

/// Canonical ordered native MCP tool surface.
pub const EXPECTED_NATIVE_TOOL_NAMES: [&str; 12] = [
    "tower_find_file",
    "tower_list_dir",
    "tower_search_text",
    "tower_read_file",
    "tower_create_file",
    "tower_create_directory",
    "tower_delete_file",
    "tower_global_replace",
    "tower_edit_range",
    "tower_reindex",
    "tower_rename_path",
    "tower_safe_delete_symbol",
];

// ── EngineState ───────────────────────────────────────────────────────────────

/// All mutable engine state shared between the MCP tool handlers and (in
/// production) the filesystem watcher.
///
/// # Sharing model
///
/// `workspace` and `index` are held as `Arc<RwLock<_>>` so the filesystem
/// watcher (spec 06) can be given clones of the same `Arc`s without needing
/// to take the outer `Arc<RwLock<EngineState>>` lock. The outer lock protects
/// `storage` and `fs`, which are not shared with the watcher.
///
/// # Locking discipline (matches spec 06 watcher lock order)
///
/// The watcher acquires workspace then index (in that order) — always. MCP
/// mutation handlers acquire the outer `EngineState` write-lock first, then
/// lock workspace and index (in the same workspace→index order). Because the
/// watcher never touches the outer `EngineState` lock, there is no lock-order
/// cycle. Short critical sections only — no blocking I/O while holding any lock.
pub struct EngineState {
    pub(crate) publication: Option<Arc<crate::adapters::daemon::CommittedEventPublisher>>,
    pub workspace: Arc<RwLock<ProjectWorkspace>>,
    pub index: Arc<RwLock<InvertedIndex>>,
    pub storage: Box<dyn StoragePort + Send + Sync>,
    pub fs: Box<dyn FileSystemPort + Send + Sync>,
    /// Extension lifecycle hook receiver. Defaults to a no-op until the real
    /// extension host is injected via [`EngineState::set_plugin_host`] (the host
    /// is built after `EngineState` in `main.rs`). Wired so MCP-driven
    /// mutations broadcast `on_file_changed` to extensions (e.g. the AST extension).
    extension_host: Arc<dyn ExtensionHostPort + Send + Sync>,
}

impl EngineState {
    /// Create a new `EngineState` wrapping the given components.
    ///
    /// `workspace` and `index` are wrapped in `Arc<RwLock<_>>` internally.
    /// Call [`EngineState::workspace_arc`] / [`EngineState::index_arc`] to
    /// obtain clones for the filesystem watcher.
    pub fn new(
        workspace: ProjectWorkspace,
        index: InvertedIndex,
        storage: Box<dyn StoragePort + Send + Sync>,
        fs: Box<dyn FileSystemPort + Send + Sync>,
    ) -> Self {
        Self {
            workspace: Arc::new(RwLock::new(workspace)),
            index: Arc::new(RwLock::new(index)),
            storage,
            fs,
            extension_host: Arc::new(NoOpExtensionHost),
            publication: None,
        }
    }

    /// Inject the real extension host after construction.
    ///
    /// `EngineState` is built before the extension registry in `main.rs`, so the
    /// host cannot be passed to [`EngineState::new`]; it is set here once the
    /// registry exists. Until then the no-op host is used.
    pub fn set_plugin_host(&mut self, host: Arc<dyn ExtensionHostPort + Send + Sync>) {
        self.extension_host = host;
    }

    pub(crate) fn extension_host(&self) -> Arc<dyn ExtensionHostPort + Send + Sync> {
        Arc::clone(&self.extension_host)
    }

    /// Return a clone of the workspace `Arc` for sharing with the watcher.
    ///
    /// The watcher and MCP handlers operate on the **same** `ProjectWorkspace`
    /// instance through separate `Arc` references.
    #[must_use]
    pub fn workspace_arc(&self) -> Arc<RwLock<ProjectWorkspace>> {
        Arc::clone(&self.workspace)
    }

    /// Return a clone of the index `Arc` for sharing with the watcher.
    ///
    /// The watcher and MCP handlers operate on the **same** `InvertedIndex`
    /// instance through separate `Arc` references.
    #[must_use]
    pub fn index_arc(&self) -> Arc<RwLock<InvertedIndex>> {
        Arc::clone(&self.index)
    }
}

// ── NativeToolRegistry ────────────────────────────────────────────────────────

/// [`ToolRegistry`] implementation for the native `tower_*` tools (spec 10b).
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
/// assert!(tools.iter().any(|tool| tool.name == "tower_read_file"));
/// ```
pub struct NativeToolRegistry {
    state: Arc<RwLock<EngineState>>,
    rename_coordinator: Arc<RenameCoordinator>,
}

impl NativeToolRegistry {
    #[must_use]
    pub fn descriptions() -> Vec<ToolDesc> {
        let tools = vec![
            tool_find_file_desc(),
            tool_list_dir_desc(),
            tool_search_text_desc(),
            tool_read_file_desc(),
            tool_create_file_desc(),
            tool_create_directory_desc(),
            tool_delete_file_desc(),
            tool_global_replace_desc(),
            tool_edit_range_desc(),
            tool_reindex_desc(),
            tool_rename_path_desc(),
            tool_safe_delete_symbol_desc(),
        ];
        debug_assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            EXPECTED_NATIVE_TOOL_NAMES
        );
        tools
    }

    /// Construct a registry wrapping the given shared engine state.
    #[must_use]
    pub fn new(state: Arc<RwLock<EngineState>>) -> Self {
        Self::with_rename_coordinator(state, Arc::new(RenameCoordinator::default()))
    }

    /// Construct a registry sharing the daemon watcher's rename coordinator.
    #[must_use]
    pub fn with_rename_coordinator(
        state: Arc<RwLock<EngineState>>,
        rename_coordinator: Arc<RenameCoordinator>,
    ) -> Self {
        Self {
            state,
            rename_coordinator,
        }
    }
}

impl ToolRegistry for NativeToolRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        Self::descriptions()
    }

    fn call(
        &mut self,
        name: &str,
        args: Value,
        cancellation: &dyn crate::domain::CancellationSignal,
    ) -> Result<Value, ToolError> {
        match name {
            "tower_find_file" => call_find_file(&self.state, args),
            "tower_list_dir" => call_list_dir(&self.state, args),
            "tower_search_text" => call_search_text(&self.state, args),
            "tower_read_file" => call_read_file(&self.state, args),
            "tower_create_file" => call_create_file(&self.state, args),
            "tower_create_directory" => call_create_directory(&self.state, args),
            "tower_delete_file" => call_delete_file(&self.state, args),
            "tower_global_replace" => call_global_replace(&self.state, args),
            "tower_edit_range" => call_edit_range(&self.state, args),
            "tower_reindex" => call_reindex(&self.state),
            "tower_rename_path" => {
                call_rename_path(&self.state, &self.rename_coordinator, args, cancellation)
            }
            "tower_safe_delete_symbol" => call_safe_delete_symbol(&self.state, args, cancellation),
            other => Err(ToolError::NotFound(other.to_owned())),
        }
    }
}

pub(crate) fn validate_native_call(name: &str, args: &Value) -> Result<(), ToolError> {
    match name {
        "tower_find_file" => {
            require_str(args, "query")?;
        }
        "tower_search_text" => {
            require_str(args, "pattern")?;
        }
        "tower_read_file" | "tower_list_dir" | "tower_create_directory" | "tower_delete_file" => {
            require_str(args, "path")?;
        }
        "tower_create_file" => {
            require_str(args, "path")?;
            require_str(args, "content")?;
            optional_version(args, "expected_version")?;
        }
        "tower_edit_range" => {
            require_str(args, "path")?;
            require_usize(args, "start_byte")?;
            require_usize(args, "end_byte")?;
            require_str(args, "replacement")?;
            optional_version(args, "expected_version")?;
        }
        "tower_global_replace" => {
            require_str(args, "target")?;
            require_str(args, "replacement")?;
            optional_version_map(args, "expected_versions")?;
        }
        "tower_rename_path" => {
            serde_json::from_value::<wire::PathRenameRequest>(args.clone()).map_err(|_| {
                ToolError::InvalidArgs("invalid tower_rename_path arguments".into())
            })?;
        }
        "tower_safe_delete_symbol" => {
            serde_json::from_value::<wire::SafeDeleteRequest>(args.clone()).map_err(|_| {
                ToolError::InvalidArgs("invalid tower_safe_delete_symbol arguments".into())
            })?;
        }
        "tower_reindex" => {}
        _ => return Err(ToolError::NotFound(name.to_owned())),
    }
    Ok(())
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

fn tool_list_dir_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_list_dir".to_owned(),
        description:
            "List indexed files and synthesized directories under a workspace-relative path."
                .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory path to list."
                },
                "recursive": {
                    "type": "boolean",
                    "description": "When true, include descendants rather than only direct children."
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional positive recursion depth limit; valid only when recursive is true."
                }
            },
            "required": ["path"]
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
        description: "Read the raw UTF-8 content of a file at the given workspace-relative path. \
            Pass with_version: true to receive a `version` field (hex SHA-256) alongside `content` \
            for use as an expected_version in a subsequent mutating call (spec 18 optimistic CAS)."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path to the file to read."
                },
                "with_version": {
                    "type": "boolean",
                    "description": "When true, include a `version` field (hex SHA-256) in the \
                        response for use as an optimistic concurrency token."
                }
            },
            "required": ["path"]
        }),
    }
}

fn tool_create_file_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_create_file".to_owned(),
        description: "Create or overwrite a file at the given path with the provided content. \
            Supply expected_version (hex SHA-256 from a prior tower_read_file with with_version:true) \
            to enable optimistic compare-and-swap: the write is rejected with a precondition error \
            if the file changed since you read it."
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
                },
                "expected_version": {
                    "type": "string",
                    "description": "Optional hex SHA-256 of the expected current file content. \
                        When supplied the write is conditional: rejected with PreconditionFailed \
                        if the file has changed."
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
        description: "Replace every occurrence of a target string with a replacement string across \
            all indexed files. Supply expected_versions (a JSON object mapping workspace-relative \
            paths to hex SHA-256 hashes) to apply per-file optimistic CAS guards: files whose \
            content has changed appear in the errors array while non-conflicting files still commit."
            .to_owned(),
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
                },
                "expected_versions": {
                    "type": "object",
                    "description": "Optional map of path → hex SHA-256. When present, each listed \
                        file is checked under the write-lock before being written; a mismatch \
                        records a conflict in TxReport.errors without blocking other files.",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["target", "replacement"]
        }),
    }
}

fn tool_edit_range_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_edit_range".to_owned(),
        description: "Replace the byte range [start_byte, end_byte) of an existing file with the \
            replacement string, splicing it into the current content and committing atomically. \
            Both start_byte and end_byte must fall on UTF-8 character boundaries. \
            end_byte must be ≤ the file length. start_byte must be ≤ end_byte. \
            An empty replacement deletes the span; start_byte == end_byte inserts without removing. \
            Supply expected_version (hex SHA-256 from a prior tower_read_file with with_version:true) \
            to enable optimistic compare-and-swap."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to edit."
                },
                "start_byte": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Inclusive start of the byte range to replace."
                },
                "end_byte": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Exclusive end of the byte range to replace."
                },
                "replacement": {
                    "type": "string",
                    "description": "UTF-8 text that replaces bytes [start_byte, end_byte)."
                },
                "expected_version": {
                    "type": "string",
                    "description": "Optional hex SHA-256 of the expected current file content. \
                        When supplied the edit is conditional: rejected with PreconditionFailed \
                        if the file has changed since the version was read."
                }
            },
            "required": ["path", "start_byte", "end_byte", "replacement"]
        }),
    }
}

fn tool_reindex_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_reindex".to_owned(),
        description: "Force a full re-scan of the workspace: rebuild the file index and text-search index from the current filesystem state (reconciles created and deleted files). Use after external changes or if the index drifted.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

fn tool_rename_path_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_rename_path".to_owned(),
        description:
            "Rename a workspace-relative file or directory, optionally planning semantic edits."
                .to_owned(),
        input_schema: serde_json::from_str(wire::PATH_RENAME_REQUEST_SCHEMA_JSON)
            .expect("canonical path-rename request schema is valid JSON"),
    }
}

fn tool_safe_delete_symbol_desc() -> ToolDesc {
    ToolDesc {
        name: "tower_safe_delete_symbol".to_owned(),
        description: "Safely delete a symbol after planning declaration and reference cleanup."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path containing the declaration."
                },
                "symbol_name": {
                    "type": "string",
                    "description": "Name of the declaration to delete."
                },
                "kind": {
                    "type": "string",
                    "description": "Optional declaration kind used to disambiguate the symbol."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "When true, validate and return the plan without applying edits."
                }
            },
            "required": ["path", "symbol_name"]
        }),
    }
}

// ── Handler implementations ───────────────────────────────────────────────────

fn call_find_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let query = require_str(&args, "query")?;

    let guard = state.read().map_err(lock_poisoned)?;
    let ws_arc = Arc::clone(&guard.workspace);
    let idx_arc = Arc::clone(&guard.index);
    drop(guard); // release outer read-lock before acquiring inner read-locks
    let ws = ws_arc.read().map_err(lock_poisoned)?;
    let idx = idx_arc.read().map_err(lock_poisoned)?;
    let search = FileSearch::new(&idx, &ws);
    let paths = search.find_file(query).map_err(domain_err_to_tool_error)?;

    let paths_json: Vec<Value> = paths
        .iter()
        .map(|p| Value::String(p.as_str().to_owned()))
        .collect();
    Ok(json!({ "paths": paths_json }))
}

fn call_list_dir(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = RelativePath::new(require_str(&args, "path")?);
    let recursive = optional_bool(&args, "recursive")?.unwrap_or(false);
    let max_depth = optional_non_zero_usize(&args, "max_depth")?;

    if max_depth.is_some() && !recursive {
        return Err(ToolError::InvalidArgs(
            "field 'max_depth' is valid only when 'recursive' is true".to_owned(),
        ));
    }

    let result = {
        let guard = state.read().map_err(lock_poisoned)?;
        let ws_arc = Arc::clone(&guard.workspace);
        let idx_arc = Arc::clone(&guard.index);
        drop(guard);
        let ws = ws_arc.read().map_err(lock_poisoned)?;
        let idx = idx_arc.read().map_err(lock_poisoned)?;
        let search = FileSearch::new(&idx, &ws);
        search
            .list_dir(ListDirRequest {
                path,
                recursive,
                max_depth,
            })
            .map_err(domain_err_to_tool_error)?
    };

    let entries: Vec<Value> = result
        .entries
        .iter()
        .map(|entry| {
            let kind = match entry.kind {
                DirectoryEntryKind::File => "file",
                DirectoryEntryKind::Dir => "dir",
            };
            json!({
                "path": entry.path.as_str(),
                "name": entry.name,
                "kind": kind
            })
        })
        .collect();

    Ok(json!({ "entries": entries }))
}

fn call_search_text(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let pattern = require_str(&args, "pattern")?;

    // We need both workspace/index and fs under the outer read-lock to ensure
    // fs is not replaced while we're iterating. Clone Arcs then release guard.
    let guard = state.read().map_err(lock_poisoned)?;
    let ws_arc = Arc::clone(&guard.workspace);
    let idx_arc = Arc::clone(&guard.index);
    // Keep the outer guard alive here to borrow fs safely for the search.
    // FileSearch::with_fs requires `FileSystemPort + Sync`. The EngineState
    // fs field is `Box<dyn FileSystemPort + Send + Sync>`, which satisfies the
    // `Sync` bound by coercion to `&dyn FileSystemPort + Sync`.
    let ws = ws_arc.read().map_err(lock_poisoned)?;
    let idx = idx_arc.read().map_err(lock_poisoned)?;
    let search = FileSearch::new(&idx, &ws).with_fs(guard.fs.as_ref());
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
    // opt-in: absent or false → legacy response shape (AC4/U1)
    let with_version = args
        .get("with_version")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Only `fs` is needed here; take the outer read-lock for fs access.
    let guard = state.read().map_err(lock_poisoned)?;
    let bytes = guard.fs.read(&rel).map_err(port_err_to_tool_error)?;
    let content = String::from_utf8_lossy(&bytes).into_owned();

    if with_version {
        // Compute SHA-256 over the raw bytes (OP1).
        // The hash is computed here in the adapter, using the same bytes
        // already read through the port — no extra I/O, no domain import of
        // std::fs (the bytes came through FileSystemPort::read).
        let version = compute_content_version(&bytes);
        Ok(json!({ "content": content, "version": version }))
    } else {
        // Legacy shape: exactly `{content}` — no version key (U1/AC4).
        Ok(json!({ "content": content }))
    }
}

/// Parse an optional `expected_version` CAS guard (spec 18).
///
/// Absent (or JSON `null`) → `Ok(None)`: the write is unconditional (U2). A
/// present **string** → `Ok(Some)`. A present non-string is a *malformed* guard:
/// we reject it with [`ToolError::InvalidArgs`] (fail-closed) rather than
/// silently dropping it to an unconditional write, which would defeat the very
/// safety property the guard exists to provide.
fn optional_version(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(ToolError::InvalidArgs(format!(
            "`{key}` must be a string (hex SHA-256 version); got {other}"
        ))),
    }
}

/// Parse the optional per-file `expected_versions` map for global-replace CAS.
///
/// Absent (or `null`) → empty map (unconditional). Present must be a JSON object
/// whose values are all strings; any other shape — a non-object, or a non-string
/// value — is a malformed guard and is rejected with [`ToolError::InvalidArgs`]
/// (fail-closed), never silently ignored.
fn optional_version_map(
    args: &Value,
    key: &str,
) -> Result<std::collections::HashMap<RelativePath, String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(std::collections::HashMap::new()),
        Some(Value::Object(obj)) => {
            let mut map = std::collections::HashMap::with_capacity(obj.len());
            for (k, v) in obj {
                let s = v.as_str().ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "`{key}[\"{k}\"]` must be a string (hex SHA-256 version); got {v}"
                    ))
                })?;
                map.insert(RelativePath::new(k), s.to_owned());
            }
            Ok(map)
        }
        Some(other) => Err(ToolError::InvalidArgs(format!(
            "`{key}` must be an object mapping path → version string; got {other}"
        ))),
    }
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

pub(crate) fn with_mutation_service<T>(
    state: &Arc<RwLock<EngineState>>,
    f: impl FnOnce(&mut FileMutationService<'_>) -> Result<T, DomainError>,
) -> Result<T, ToolError> {
    use crate::adapters::daemon::publication::BufferedEvents;
    let publication = state.read().map_err(lock_poisoned)?.publication.clone();
    let commit = publication
        .as_ref()
        .map(|publisher| publisher.commit_lock());
    if let Some(publisher) = &publication {
        publisher
            .check_capacity()
            .map_err(|_| ToolError::ExecutionFailed("commit publication unavailable".to_owned()))?;
    }
    let events = BufferedEvents::default();
    let (result, host) = {
        let mut guard = state.write().map_err(lock_poisoned)?;
        let ws_arc = Arc::clone(&guard.workspace);
        let idx_arc = Arc::clone(&guard.index);
        let mut ws = ws_arc.write().map_err(lock_poisoned)?;
        let mut idx = idx_arc.write().map_err(lock_poisoned)?;
        let engine = &mut *guard;
        let host = Arc::clone(&engine.extension_host);
        let mut svc = FileMutationService::new(
            engine.fs.as_mut(),
            &mut ws,
            &mut idx,
            engine.storage.as_mut(),
            &events,
        );
        let result = f(&mut svc).map_err(domain_err_to_tool_error);
        if let Some(publisher) = &publication {
            publisher
                .commit_events(
                    events
                        .take()
                        .into_iter()
                        .map(|event| match event {
                            crate::adapters::daemon::CommittedFileEvent::Changed {
                                file_id,
                                path,
                            } if ws.get(file_id).is_err() => {
                                crate::adapters::daemon::CommittedFileEvent::Deleted { path }
                            }
                            event => event,
                        })
                        .collect(),
                )
                .map_err(|_| {
                    ToolError::ExecutionFailed("commit publication unavailable".to_owned())
                })?;
        }
        (result, host)
    };
    drop(commit);
    // Compatibility callers without the daemon publisher still receive the
    // same notifications, but never under workspace/index/state locks.
    for event in events.take() {
        use crate::adapters::daemon::CommittedFileEvent;
        match event {
            CommittedFileEvent::Indexed { file_id, path } => host.on_file_indexed(file_id, &path),
            CommittedFileEvent::Changed { file_id, path } => host.on_file_changed(file_id, &path),
            CommittedFileEvent::Deleted { path } => host.on_file_deleted(&path),
        }
    }
    result
}

fn call_create_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let content = require_str(&args, "content")?;
    // Optional CAS guard: absent → unconditional write (U2); malformed → reject.
    let expected_version = optional_version(&args, "expected_version")?;

    let rel = RelativePath::new(path);
    let bytes = content.as_bytes().to_vec();

    with_mutation_service(state, |svc| {
        svc.create_file_cas(rel, bytes, expected_version)
    })?;

    Ok(json!({ "created": true }))
}

fn call_create_directory(
    state: &Arc<RwLock<EngineState>>,
    args: Value,
) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let rel = RelativePath::new(path);

    with_mutation_service(state, |svc| svc.create_directory(rel))?;

    Ok(json!({ "created": true }))
}

fn call_delete_file(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let rel = RelativePath::new(path);

    with_mutation_service(state, |svc| svc.delete_file(&rel))?;

    Ok(json!({ "deleted": true }))
}

fn call_global_replace(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let target = require_str(&args, "target")?;
    let replacement = require_str(&args, "replacement")?;
    // Optional per-file CAS guards: absent → unconditional write (U2); a
    // malformed map (non-object, or non-string value) is rejected, not ignored.
    let expected_versions = optional_version_map(&args, "expected_versions")?;

    let report = with_mutation_service(state, |svc| {
        svc.global_replace_cas(target, replacement, expected_versions)
    })?;

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

fn call_edit_range(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let path = require_str(&args, "path")?;
    let start_byte = require_usize(&args, "start_byte")?;
    let end_byte = require_usize(&args, "end_byte")?;
    let replacement = require_str(&args, "replacement")?;
    // Optional CAS guard: absent → unconditional write (U2); malformed → reject.
    let expected_version = optional_version(&args, "expected_version")?;

    let rel = RelativePath::new(path);

    let report = with_mutation_service(state, |svc| {
        svc.edit_range_cas(&rel, start_byte, end_byte, replacement, expected_version)
    })?;

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

/// Reconcile a staged inventory while retaining live generational identities.
/// Reject a concurrent indexed commit rather than overwrite it with a stale scan.
fn call_reindex(state: &Arc<RwLock<EngineState>>) -> Result<Value, ToolError> {
    use crate::adapters::daemon::publication::snapshot_events;
    use crate::ports::storage::ReconciliationDelta;
    let publication = state.read().map_err(lock_poisoned)?.publication.clone();
    let baseline = publication
        .as_ref()
        .map(|publisher| publisher.capture_cut())
        .transpose()
        .map_err(|_| ToolError::ExecutionFailed("commit publication unavailable".to_owned()))?;
    // Filesystem enumeration stays outside the publication/authoritative locks.
    let entries: Vec<(RelativePath, FileMetadata)> = {
        let guard = state.read().map_err(lock_poisoned)?;
        match guard.fs.workspace_root() {
            Some(root) => crate::adapters::fs::scan::collect_workspace_files(&root),
            None => guard
                .fs
                .scan()
                .into_iter()
                .filter(|path| !crate::domain::mutation::is_tmp_artifact(path))
                .map(|path| {
                    let bytes = guard.fs.read(&path).unwrap_or_default();
                    let metadata = FileMetadata {
                        size: bytes.len() as u64,
                        modified: crate::domain::virtual_file::Timestamp(0),
                        content_hash: Some(crate::adapters::fs::scan::content_hash_of(&bytes)),
                    };
                    (path, metadata)
                })
                .collect(),
        }
    };
    let _commit = publication
        .as_ref()
        .map(|publisher| publisher.commit_lock());
    if let Some(publisher) = &publication {
        publisher
            .check_capacity()
            .map_err(|_| ToolError::ExecutionFailed("commit publication unavailable".to_owned()))?;
        if Some(
            publisher.capture_cut().map_err(|_| {
                ToolError::ExecutionFailed("commit publication unavailable".to_owned())
            })?,
        ) != baseline
        {
            return Err(ToolError::ExecutionFailed(
                "workspace changed during reindex; retry the operation".to_owned(),
            ));
        }
    }
    let mut guard = state.write().map_err(lock_poisoned)?;
    let workspace = guard.workspace_arc();
    let index = guard.index_arc();
    let mut workspace = workspace.write().map_err(lock_poisoned)?;
    let mut index = index.write().map_err(lock_poisoned)?;
    let before = workspace.snapshot();
    let mut staged =
        ProjectWorkspace::from_snapshot(before.clone()).map_err(domain_err_to_tool_error)?;
    let paths: std::collections::HashSet<_> =
        entries.iter().map(|(path, _)| path.clone()).collect();
    let mut removals = Vec::new();
    for file in &before.files {
        if !paths.contains(&file.path) {
            staged.remove(file.id).map_err(domain_err_to_tool_error)?;
            removals.push(file.id);
        }
    }
    for (path, metadata) in entries {
        if let Some(id) = staged.get_by_path(&path) {
            staged
                .update(id, metadata)
                .map_err(domain_err_to_tool_error)?;
        } else {
            staged
                .insert(path, metadata)
                .map_err(domain_err_to_tool_error)?;
        }
    }
    let after = staged.snapshot();
    let additions = after
        .files
        .iter()
        .filter(|file| !before.files.contains(file))
        .cloned()
        .collect();
    guard
        .storage
        .apply_reconciliation_delta(&ReconciliationDelta {
            additions,
            removals,
        })
        .map_err(|_| ToolError::ExecutionFailed("reindex commit failed".to_owned()))?;
    let mut staged_index = InvertedIndex::new();
    for file in &after.files {
        staged_index.insert(file.id, &tokenize(file.path.as_str()));
    }
    *workspace = staged;
    *index = staged_index;
    if let Some(publisher) = &publication {
        publisher
            .commit_events(snapshot_events(&before, &after))
            .map_err(|_| ToolError::ExecutionFailed("commit publication unavailable".to_owned()))?;
    }
    Ok(json!({"files_indexed": after.files.len()}))
}

fn call_rename_path(
    state: &Arc<RwLock<EngineState>>,
    rename_coordinator: &RenameCoordinator,
    args: Value,
    cancellation: &dyn crate::domain::CancellationSignal,
) -> Result<Value, ToolError> {
    let request: wire::PathRenameRequest = serde_json::from_value(args)
        .map_err(|_| ToolError::InvalidArgs("invalid tower_rename_path arguments".to_owned()))?;
    let policy = domain_rename_policy(request.semantic_policy);
    let domain_request = PathRenameRequest {
        source: RelativePath::new(request.source),
        destination: RelativePath::new(request.destination),
        dry_run: request.dry_run,
        semantic_policy: policy,
        expected_versions: request
            .expected_versions
            .into_iter()
            .map(|(path, version)| (RelativePath::new(path), version.to_ascii_lowercase()))
            .collect(),
    };
    let exclusions: Box<dyn PathExclusionPort> = {
        let guard = state.read().map_err(lock_poisoned)?;
        match guard.fs.workspace_root() {
            Some(root) => Box::new(WorkspacePathExclusion::new(root).map_err(|_| {
                ToolError::ExecutionFailed("failed to load workspace exclusion policy".to_owned())
            })?),
            None => Box::new(StructuralPathExclusion),
        }
    };

    let extension_host = state
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .extension_host();
    let native_plan = match with_path_rename_state(state, |locked| {
        locked
            .service(extension_host.as_ref())
            .plan_path_rename(domain_request, exclusions.as_ref())
    })? {
        Ok(plan) => plan,
        Err(error) => return serialize_rename_failure(None, policy, error),
    };

    let semantic = if policy == PathRenameSemanticPolicy::None {
        None
    } else {
        match request_semantic_rename(extension_host.as_ref(), &native_plan, cancellation) {
            Ok(edits) => Some(Ok(edits)),
            Err(SemanticRenameRequestError::Domain(error)) => Some(Err(error)),
            Err(SemanticRenameRequestError::Cancelled) => return Err(ToolError::Cancelled),
        }
    };

    let deferred_host = Arc::new(DeferredRenameHost::default());
    let applied = with_path_rename_state(state, |locked| {
        let (ready, rename_guard, apply_result) = {
            let mut service = locked.service(deferred_host.as_ref());
            let ready = match semantic {
                Some(semantic) => {
                    match service.complete_path_rename_plan(native_plan.clone(), semantic) {
                        Ok(plan) => plan,
                        Err(error) => {
                            return Err(RenameApplyError::Domain(Box::new(native_plan), error));
                        }
                    }
                }
                None => native_plan,
            };
            let rename_guard = if ready.dry_run() {
                None
            } else {
                Some(
                    rename_coordinator
                        .register_engine_owned(ready.moves())
                        .map_err(|_| RenameApplyError::Coordination)?,
                )
            };
            let apply_result = service.apply_path_rename(ready.clone(), exclusions.as_ref());
            (ready, rename_guard, apply_result)
        };
        match apply_result {
            Ok(result) => {
                let committed_files = if matches!(
                    result.status,
                    PathRenameStatus::Applied | PathRenameStatus::Partial
                ) {
                    ready
                        .moves()
                        .iter()
                        .filter_map(|path_move| {
                            locked
                                .workspace
                                .get_by_path(&path_move.destination)
                                .and_then(|id| locked.workspace.get(id).ok())
                                .cloned()
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                Ok((result, rename_guard, committed_files))
            }
            Err(error) => Err(RenameApplyError::Domain(Box::new(ready), error)),
        }
    });
    let (result, mut rename_guard, committed_files) = match applied? {
        Ok(applied) => applied,
        Err(RenameApplyError::Domain(plan, error)) => {
            return serialize_rename_failure(Some(&plan), policy, error);
        }
        Err(RenameApplyError::Coordination) => {
            return Err(ToolError::ExecutionFailed(
                "failed to coordinate path rename".to_owned(),
            ));
        }
    };

    if let Some(rename_guard) = rename_guard.as_mut() {
        rename_guard.commit(&committed_files);
    }
    let mut notifications = deferred_host.drain_changed();
    for file in &committed_files {
        if !notifications
            .iter()
            .any(|(id, path)| *id == file.id && *path == file.path)
        {
            notifications.push((file.id, file.path.clone()));
        }
    }
    if state.read().map_err(lock_poisoned)?.publication.is_none() {
        for (id, path) in notifications {
            extension_host.on_file_changed(id, &path);
        }
    }
    drop(rename_guard);
    serialize_rename_result(&result)
}

fn call_safe_delete_symbol(
    state: &Arc<RwLock<EngineState>>,
    args: Value,
    cancellation: &dyn crate::domain::CancellationSignal,
) -> Result<Value, ToolError> {
    let request: wire::SafeDeleteRequest = serde_json::from_value(args).map_err(|_| {
        ToolError::InvalidArgs("invalid tower_safe_delete_symbol arguments".to_owned())
    })?;
    let extension_host = state
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .extension_host();

    let declaration_request = wire::SafeDeleteDeclarationRequest {
        path: request.path.clone(),
        symbol_name: request.symbol_name.clone(),
        kind: request.kind.clone(),
    };
    let declaration = match invoke_safe_delete_declaration_planner(
        extension_host.as_ref(),
        &declaration_request,
        cancellation,
    )? {
        Ok(declaration) => declaration,
        Err(blocking_reason) => return serialize_safe_delete_block(blocking_reason),
    };

    let declaration_plan = match &declaration {
        wire::SafeDeleteDeclarationOutcome::Planned { plan } => plan.clone(),
        _ => {
            return run_safe_delete(
                state,
                extension_host.as_ref(),
                request,
                declaration,
                wire::SafeDeleteSemanticOutcome::CapabilityUnavailable,
                &[],
            );
        }
    };
    let declaration_path = declaration_plan.path.clone();
    let initial = run_safe_delete(
        state,
        extension_host.as_ref(),
        request.clone(),
        declaration.clone(),
        wire::SafeDeleteSemanticOutcome::CapabilityUnavailable,
        &[declaration_path],
    )?;
    if initial["blocking_reason"] != "capability_unavailable" {
        return Ok(initial);
    }

    let semantic_request = wire::SafeDeleteSemanticRequest {
        declaration: declaration_plan,
    };
    let semantic = invoke_safe_delete_semantic_planner(
        extension_host.as_ref(),
        &semantic_request,
        cancellation,
    )?;
    let snapshot_paths = safe_delete_snapshot_paths(&declaration, &semantic);
    run_safe_delete(
        state,
        extension_host.as_ref(),
        request,
        declaration,
        semantic,
        &snapshot_paths,
    )
}

fn invoke_safe_delete_declaration_planner(
    host: &dyn ExtensionHostPort,
    request: &wire::SafeDeleteDeclarationRequest,
    cancellation: &dyn crate::domain::CancellationSignal,
) -> Result<Result<wire::SafeDeleteDeclarationOutcome, wire::SafeDeleteBlockingReason>, ToolError> {
    let params = serde_json::to_value(request)
        .map_err(|_| ToolError::ExecutionFailed("safe-delete planning failed".to_owned()))?;
    let value = match host.invoke_extension(
        &crate::domain::extension_host::ExtensionId::new("ast"),
        "plan_safe_delete_declaration",
        params,
        cancellation,
    ) {
        Ok(value) => value,
        Err(crate::domain::extension_host::InvokeError::ToolNotFound(_)) => {
            return Ok(Err(wire::SafeDeleteBlockingReason::CapabilityUnavailable));
        }
        Err(crate::domain::extension_host::InvokeError::Cancelled) => {
            return Err(ToolError::Cancelled);
        }
        Err(
            crate::domain::extension_host::InvokeError::Fault(_)
            | crate::domain::extension_host::InvokeError::Application(_),
        ) => {
            return Ok(Err(wire::SafeDeleteBlockingReason::BackendError));
        }
    };
    Ok(serde_json::from_value(value).map_err(|_| wire::SafeDeleteBlockingReason::BackendError))
}

fn serialize_safe_delete_block(
    blocking_reason: wire::SafeDeleteBlockingReason,
) -> Result<Value, ToolError> {
    let blocking_reason = serde_json::to_value(blocking_reason)
        .map_err(|_| ToolError::ExecutionFailed("safe-delete planning failed".to_owned()))?;
    Ok(json!({
        "status": "blocked",
        "blocking_reason": blocking_reason,
        "references": [],
        "proposed_edits": [],
        "affected_files": []
    }))
}

fn invoke_safe_delete_semantic_planner(
    host: &dyn ExtensionHostPort,
    request: &wire::SafeDeleteSemanticRequest,
    cancellation: &dyn crate::domain::CancellationSignal,
) -> Result<wire::SafeDeleteSemanticOutcome, ToolError> {
    let params = serde_json::to_value(request)
        .map_err(|_| ToolError::ExecutionFailed("safe-delete planning failed".to_owned()))?;
    let value = match host.invoke_extension(
        &crate::domain::extension_host::ExtensionId::new("lsp"),
        "plan_safe_delete",
        params,
        cancellation,
    ) {
        Ok(value) => value,
        Err(crate::domain::extension_host::InvokeError::Fault(fault)) => {
            return Ok(match fault {
                extension_protocol::ExtensionFault::Timeout { .. } => {
                    wire::SafeDeleteSemanticOutcome::BackendTimeout
                }
                extension_protocol::ExtensionFault::Crashed { .. } => {
                    wire::SafeDeleteSemanticOutcome::BackendCrashed
                }
                extension_protocol::ExtensionFault::ProtocolError { .. }
                | extension_protocol::ExtensionFault::Quarantined => {
                    wire::SafeDeleteSemanticOutcome::MalformedResponse
                }
            });
        }
        Err(crate::domain::extension_host::InvokeError::Application(_)) => {
            return Ok(wire::SafeDeleteSemanticOutcome::MalformedResponse);
        }
        Err(crate::domain::extension_host::InvokeError::Cancelled) => {
            return Err(ToolError::Cancelled);
        }
        Err(crate::domain::extension_host::InvokeError::ToolNotFound(_)) => {
            return Ok(wire::SafeDeleteSemanticOutcome::CapabilityUnavailable);
        }
    };
    Ok(serde_json::from_value(value).unwrap_or(wire::SafeDeleteSemanticOutcome::MalformedResponse))
}

fn safe_delete_snapshot_paths(
    declaration: &wire::SafeDeleteDeclarationOutcome,
    semantic: &wire::SafeDeleteSemanticOutcome,
) -> Vec<String> {
    let mut paths = Vec::new();
    if let wire::SafeDeleteDeclarationOutcome::Planned { plan } = declaration {
        paths.push(plan.path.clone());
    }
    if let wire::SafeDeleteSemanticOutcome::Complete { plan }
    | wire::SafeDeleteSemanticOutcome::Incomplete { plan } = semantic
    {
        for path in plan
            .references
            .iter()
            .map(|reference| &reference.path)
            .chain(plan.cleanup_edits.iter().map(|edit| &edit.path))
        {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
    }
    paths
}

fn run_safe_delete(
    state: &Arc<RwLock<EngineState>>,
    extension_host: &dyn ExtensionHostPort,
    request: wire::SafeDeleteRequest,
    declaration: wire::SafeDeleteDeclarationOutcome,
    semantic: wire::SafeDeleteSemanticOutcome,
    snapshot_paths: &[String],
) -> Result<Value, ToolError> {
    let deferred_host = DeferredRenameHost::default();
    let mut report = with_path_rename_state(state, |locked| {
        let snapshots = snapshot_paths
            .iter()
            .map(|path| {
                let path = RelativePath::new(path.clone());
                locked
                    .fs
                    .read(&path)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|content| SafeDeleteSnapshot { path, content })
            })
            .collect::<Option<Vec<_>>>();
        let snapshots = match snapshots {
            Some(snapshots) => snapshots,
            None => {
                return Ok(wire::SafeDeleteReport {
                    status: wire::SafeDeleteStatus::Blocked,
                    blocking_reason: Some(wire::SafeDeleteBlockingReason::ReferencesUnresolved),
                    declaration: match declaration {
                        wire::SafeDeleteDeclarationOutcome::Planned { plan } => Some(plan),
                        _ => None,
                    },
                    references: Vec::new(),
                    proposed_edits: Vec::new(),
                    affected_files: Vec::new(),
                    mutation_result: None,
                });
            }
        };
        let mut mutation = locked.service(&deferred_host);
        SafeDeleteService::new(&mut mutation).safe_delete(request, declaration, semantic, snapshots)
    })?
    .map_err(domain_err_to_tool_error)?;

    if report.status == wire::SafeDeleteStatus::Blocked {
        for edit in &mut report.proposed_edits {
            edit.replacement.clear();
        }
    }

    if state.read().map_err(lock_poisoned)?.publication.is_none() {
        for (id, path) in deferred_host.drain_changed() {
            extension_host.on_file_changed(id, &path);
        }
    }
    serde_json::to_value(report).map_err(|_| {
        ToolError::ExecutionFailed("failed to serialize safe-delete result".to_owned())
    })
}

enum RenameApplyError {
    Domain(Box<PathRenamePlan>, PathRenameError),
    Coordination,
}

struct LockedRenameState<'a> {
    fs: &'a mut dyn FileSystemPort,
    workspace: &'a mut ProjectWorkspace,
    index: &'a mut InvertedIndex,
    storage: &'a mut dyn StoragePort,
}

impl LockedRenameState<'_> {
    fn service<'a>(&'a mut self, host: &'a dyn ExtensionHostPort) -> FileMutationService<'a> {
        FileMutationService::new(self.fs, self.workspace, self.index, self.storage, host)
    }
}

// The closure owns the complete state -> workspace -> index critical section.
// Sidecar I/O and extension callbacks must run after this function returns.
fn with_path_rename_state<T>(
    state: &Arc<RwLock<EngineState>>,
    f: impl FnOnce(&mut LockedRenameState<'_>) -> T,
) -> Result<T, ToolError> {
    let publication = state
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .publication
        .clone();
    let _commit = publication
        .as_ref()
        .map(|publisher| publisher.commit_lock());
    if let Some(publisher) = &publication {
        publisher
            .check_capacity()
            .map_err(|_| ToolError::ExecutionFailed("commit publication unavailable".to_owned()))?;
    }
    let mut guard = state.write().unwrap_or_else(|poison| poison.into_inner());
    let workspace = Arc::clone(&guard.workspace);
    let index = Arc::clone(&guard.index);
    let mut workspace = workspace
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut index = index.write().unwrap_or_else(|poison| poison.into_inner());
    let EngineState { fs, storage, .. } = &mut *guard;
    let before = publication.as_ref().map(|_| workspace.snapshot());
    let result = f(&mut LockedRenameState {
        fs: fs.as_mut(),
        workspace: &mut workspace,
        index: &mut index,
        storage: storage.as_mut(),
    });
    if let (Some(publisher), Some(before)) = (&publication, before) {
        publisher
            .commit_events(crate::adapters::daemon::publication::snapshot_events(
                &before,
                &workspace.snapshot(),
            ))
            .map_err(|_| ToolError::ExecutionFailed("commit publication unavailable".to_owned()))?;
    }
    Ok(result)
}

#[derive(Default)]
struct DeferredRenameHost {
    changed: Mutex<Vec<(FileId, RelativePath)>>,
}

impl DeferredRenameHost {
    fn drain_changed(&self) -> Vec<(FileId, RelativePath)> {
        self.changed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain(..)
            .collect()
    }
}

impl ExtensionHostPort for DeferredRenameHost {
    fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

    fn on_file_changed(&self, id: FileId, path: &RelativePath) {
        self.changed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push((id, path.clone()));
    }

    fn on_file_deleted(&self, _path: &RelativePath) {}

    fn declared_tools(&self) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
        Vec::new()
    }

    fn invoke(
        &self,
        tool_name: &str,
        _params: Value,
        _cancellation: &dyn crate::domain::CancellationSignal,
    ) -> Result<Value, crate::domain::InvokeError> {
        Err(crate::domain::InvokeError::ToolNotFound(
            tool_name.to_owned(),
        ))
    }
}

struct StructuralPathExclusion;

impl PathExclusionPort for StructuralPathExclusion {
    fn classify(&self, path: &RelativePath) -> Result<PathExclusionDecision, PathExclusionError> {
        if path.as_str().is_empty()
            || Path::new(path.as_str()).is_absolute()
            || Path::new(path.as_str()).components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            Err(PathExclusionError::OutsideWorkspace)
        } else {
            Ok(PathExclusionDecision::Allowed)
        }
    }
}

fn domain_rename_policy(policy: wire::PathRenameSemanticPolicy) -> PathRenameSemanticPolicy {
    match policy {
        wire::PathRenameSemanticPolicy::None => PathRenameSemanticPolicy::None,
        wire::PathRenameSemanticPolicy::BestEffort => PathRenameSemanticPolicy::BestEffort,
        wire::PathRenameSemanticPolicy::Required => PathRenameSemanticPolicy::Required,
    }
}

fn wire_rename_policy(policy: PathRenameSemanticPolicy) -> wire::PathRenameSemanticPolicy {
    match policy {
        PathRenameSemanticPolicy::None => wire::PathRenameSemanticPolicy::None,
        PathRenameSemanticPolicy::BestEffort => wire::PathRenameSemanticPolicy::BestEffort,
        PathRenameSemanticPolicy::Required => wire::PathRenameSemanticPolicy::Required,
    }
}

fn wire_rename_plan(plan: &PathRenamePlan) -> wire::PathRenamePlan {
    wire::PathRenamePlan {
        moves: plan
            .moves()
            .iter()
            .map(|path_move| wire::PathMove {
                source: path_move.source.as_str().to_owned(),
                destination: path_move.destination.as_str().to_owned(),
                kind: match path_move.kind {
                    DirectoryEntryKind::File => wire::PathMoveKind::File,
                    DirectoryEntryKind::Dir => wire::PathMoveKind::Directory,
                },
            })
            .collect(),
        edits: plan
            .edits()
            .iter()
            .map(|edit| wire::WorkspaceEditSpan {
                path: edit.path.as_str().to_owned(),
                start_byte: edit.start_byte,
                end_byte: edit.end_byte,
                replacement: edit.replacement.clone(),
                base_hash: edit.base_hash.clone(),
            })
            .collect(),
        semantic_policy: wire_rename_policy(plan.semantic_policy()),
    }
}

enum SemanticRenameRequestError {
    Domain(PathRenameError),
    Cancelled,
}

fn request_semantic_rename(
    host: &dyn ExtensionHostPort,
    native_plan: &PathRenamePlan,
    cancellation: &dyn crate::domain::CancellationSignal,
) -> Result<Vec<WorkspaceEditSpan>, SemanticRenameRequestError> {
    let request = wire::PathRenamePlanningRequest {
        moves: wire_rename_plan(native_plan).moves,
        semantic_policy: wire_rename_policy(native_plan.semantic_policy()),
    };
    let params = serde_json::to_value(&request)
        .map_err(|_| SemanticRenameRequestError::Domain(PathRenameError::SemanticBackend))?;
    let value = host
        .invoke("plan_path_rename", params, cancellation)
        .map_err(|error| match error {
            crate::domain::extension_host::InvokeError::ToolNotFound(_) => {
                SemanticRenameRequestError::Domain(PathRenameError::SemanticUnsupported)
            }
            crate::domain::extension_host::InvokeError::Cancelled => {
                SemanticRenameRequestError::Cancelled
            }
            _ => SemanticRenameRequestError::Domain(PathRenameError::SemanticBackend),
        })?;
    let response: wire::PathRenamePlanningResult = serde_json::from_value(value)
        .map_err(|_| SemanticRenameRequestError::Domain(PathRenameError::SemanticBackend))?;
    if let Some(error) = response.error {
        return Err(SemanticRenameRequestError::Domain(
            domain_rename_error_code(error.code),
        ));
    }
    let plan = response.plan.ok_or(SemanticRenameRequestError::Domain(
        PathRenameError::SemanticBackend,
    ))?;
    let expected = wire_rename_plan(native_plan);
    if plan.moves != expected.moves || plan.semantic_policy != expected.semantic_policy {
        return Err(SemanticRenameRequestError::Domain(
            PathRenameError::SemanticBackend,
        ));
    }
    plan.edits
        .into_iter()
        .map(|edit| {
            Ok(WorkspaceEditSpan {
                path: RelativePath::new(edit.path),
                start_byte: edit.start_byte,
                end_byte: edit.end_byte,
                replacement: edit.replacement,
                base_hash: edit.base_hash,
            })
        })
        .collect()
}

fn domain_rename_error_code(code: wire::PathRenameErrorCode) -> PathRenameError {
    match code {
        wire::PathRenameErrorCode::SemanticUnsupported => PathRenameError::SemanticUnsupported,
        wire::PathRenameErrorCode::WorkspaceEscape => PathRenameError::WorkspaceEscape {
            path: RelativePath::new("semantic-target"),
        },
        _ => PathRenameError::SemanticBackend,
    }
}

fn serialize_rename_failure(
    plan: Option<&PathRenamePlan>,
    policy: PathRenameSemanticPolicy,
    error: PathRenameError,
) -> Result<Value, ToolError> {
    let status = if error == PathRenameError::SemanticUnsupported {
        wire::PathRenameStatus::Unsupported
    } else {
        wire::PathRenameStatus::Rejected
    };
    let plan = plan.map_or_else(
        || wire::PathRenamePlan {
            moves: Vec::new(),
            edits: Vec::new(),
            semantic_policy: wire_rename_policy(policy),
        },
        wire_rename_plan,
    );
    serde_json::to_value(wire::PathRenameResult {
        status,
        plan,
        edit_result: None,
        errors: vec![wire_rename_error(&error)],
    })
    .map_err(|_| ToolError::ExecutionFailed("failed to serialize path rename result".to_owned()))
}

fn wire_rename_error(error: &PathRenameError) -> wire::PathRenameError {
    let (code, path) = match error {
        PathRenameError::SourceNotFound { path } => {
            (wire::PathRenameErrorCode::SourceNotFound, Some(path))
        }
        PathRenameError::DestinationExists { path } => {
            (wire::PathRenameErrorCode::DestinationExists, Some(path))
        }
        PathRenameError::IdenticalPaths { path } => {
            (wire::PathRenameErrorCode::IdenticalPaths, Some(path))
        }
        PathRenameError::DestinationInsideSource { destination, .. } => (
            wire::PathRenameErrorCode::DestinationInsideSource,
            Some(destination),
        ),
        PathRenameError::DestinationExcluded { path, .. } => {
            (wire::PathRenameErrorCode::DestinationExcluded, Some(path))
        }
        PathRenameError::WorkspaceEscape { path } => {
            (wire::PathRenameErrorCode::WorkspaceEscape, Some(path))
        }
        PathRenameError::DuplicateDestination { path } => {
            (wire::PathRenameErrorCode::DuplicateDestination, Some(path))
        }
        PathRenameError::InvalidExpectedVersion { path } => (
            wire::PathRenameErrorCode::InvalidExpectedVersion,
            Some(path),
        ),
        PathRenameError::SemanticUnsupported => {
            (wire::PathRenameErrorCode::SemanticUnsupported, None)
        }
        PathRenameError::SemanticBackend => (wire::PathRenameErrorCode::SemanticBackend, None),
        PathRenameError::Filesystem => (wire::PathRenameErrorCode::FilesystemError, None),
        PathRenameError::Persistence => (wire::PathRenameErrorCode::PersistenceError, None),
        PathRenameError::VersionConflict { path, .. } => {
            (wire::PathRenameErrorCode::CasConflict, Some(path))
        }
    };
    wire::PathRenameError {
        code,
        path: path.map(|path| path.as_str().to_owned()),
        message: String::new(),
    }
}

fn serialize_rename_result(result: &PathRenameResult) -> Result<Value, ToolError> {
    let status = match result.status {
        PathRenameStatus::Preview => wire::PathRenameStatus::Preview,
        PathRenameStatus::Applied => wire::PathRenameStatus::Applied,
        PathRenameStatus::Partial => wire::PathRenameStatus::Partial,
        PathRenameStatus::Unsupported => wire::PathRenameStatus::Unsupported,
        PathRenameStatus::Rejected => wire::PathRenameStatus::Rejected,
    };
    serde_json::to_value(wire::PathRenameResult {
        status,
        plan: wire_rename_plan(&result.plan),
        edit_result: result.edit_result.as_ref().map(|edit_result| {
            wire::WorkspaceApplyEditsResult {
                files_changed: edit_result.files_changed,
                per_file: edit_result
                    .per_file
                    .iter()
                    .map(|file| wire::PerFileEditResult {
                        path: file.path.as_str().to_owned(),
                        applied: file.applied,
                        edits_applied: file.edits_applied,
                        edits_skipped: file.edits_skipped,
                        new_version: file.new_version.clone(),
                        preview: file.preview.clone(),
                        error: None,
                    })
                    .collect(),
            }
        }),
        errors: result.errors.iter().map(wire_rename_error).collect(),
    })
    .map_err(|_| ToolError::ExecutionFailed("failed to serialize path rename result".to_owned()))
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

/// Extract a required non-negative integer field from a JSON args object as `usize`.
///
/// Returns `ToolError::InvalidArgs` when:
/// - the field is absent,
/// - the value is not a JSON integer,
/// - the value is negative, or
/// - the value exceeds `usize::MAX` on the current platform.
fn require_usize(args: &Value, field: &str) -> Result<usize, ToolError> {
    let v = args
        .get(field)
        .ok_or_else(|| ToolError::InvalidArgs(format!("required field '{field}' is missing")))?;
    // `as_u64` rejects both non-numbers and negative integers.
    let n = v.as_u64().ok_or_else(|| {
        ToolError::InvalidArgs(format!(
            "required field '{field}' must be a non-negative integer"
        ))
    })?;
    usize::try_from(n).map_err(|_| {
        ToolError::InvalidArgs(format!(
            "required field '{field}' value {n} exceeds platform usize::MAX"
        ))
    })
}

fn optional_bool(args: &Value, field: &str) -> Result<Option<bool>, ToolError> {
    match args.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| ToolError::InvalidArgs(format!("field '{field}' must be a boolean"))),
    }
}

fn optional_non_zero_usize(args: &Value, field: &str) -> Result<Option<NonZeroUsize>, ToolError> {
    match args.get(field) {
        None => Ok(None),
        Some(value) => {
            let n = value.as_u64().ok_or_else(|| {
                ToolError::InvalidArgs(format!("field '{field}' must be a positive integer"))
            })?;
            let n = usize::try_from(n).map_err(|_| {
                ToolError::InvalidArgs(format!(
                    "field '{field}' value {n} exceeds platform usize::MAX"
                ))
            })?;
            NonZeroUsize::new(n).map(Some).ok_or_else(|| {
                ToolError::InvalidArgs(format!("field '{field}' must be greater than zero"))
            })
        }
    }
}

/// Map a [`DomainError`] to a [`ToolError`] with a stable, inspectable code.
///
/// # Stable code assignment
///
/// | Variant              | ToolError variant       | JSON-RPC code |
/// |----------------------|-------------------------|---------------|
/// | `NotFound`           | `ResourceNotFound`      | -32002        |
/// | `InvalidRange`       | `InvalidArgs`           | -32602        |
/// | others               | `ExecutionFailed`       | -32603        |
///
/// `DomainError::NotFound` maps to [`ToolError::ResourceNotFound`] which the
/// transport maps to `-32002`. Clients branch on this stable code to detect
/// "resource not found" without parsing the error string (spec 10b AC5).
///
/// `DomainError::InvalidRange` is a bad-input error (the caller provided an
/// out-of-bounds or non-char-boundary range), so it maps to `InvalidArgs`
/// (`-32602`) rather than `ExecutionFailed` (`-32603`).
fn domain_err_to_tool_error(err: DomainError) -> ToolError {
    match err {
        DomainError::NotFound => ToolError::ResourceNotFound(err.to_string()),
        DomainError::NotADirectory(path) => {
            ToolError::InvalidArgs(format!("not a directory: {}", path.as_str()))
        }
        DomainError::InvalidRange(_) => ToolError::InvalidArgs(err.to_string()),
        DomainError::VersionConflict { .. } => ToolError::PreconditionFailed(err.to_string()),
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

    use serde_json::{Value, json};

    use super::{EXPECTED_NATIVE_TOOL_NAMES, EngineState, NativeToolRegistry};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::mcp::types::ToolError;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::RelativePath;
    use crate::domain::index::InvertedIndex;
    use crate::domain::token::tokenize;
    use crate::domain::virtual_file::FileMetadata;
    use crate::domain::workspace::ProjectWorkspace;
    use crate::ports::FileSystemPort;

    struct CancelledExtensionHost;

    impl crate::ports::ExtensionHostPort for CancelledExtensionHost {
        fn on_file_indexed(&self, _id: crate::domain::FileId, _path: &RelativePath) {}
        fn on_file_changed(&self, _id: crate::domain::FileId, _path: &RelativePath) {}
        fn on_file_deleted(&self, _path: &RelativePath) {}
        fn declared_tools(
            &self,
        ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
            Vec::new()
        }
        fn invoke(
            &self,
            _tool_name: &str,
            _params: Value,
            _cancellation: &dyn crate::domain::CancellationSignal,
        ) -> Result<Value, crate::domain::InvokeError> {
            Err(crate::domain::InvokeError::Cancelled)
        }

        fn invoke_extension(
            &self,
            _extension_id: &crate::domain::ExtensionId,
            _tool_name: &str,
            _params: Value,
            _cancellation: &dyn crate::domain::CancellationSignal,
        ) -> Result<Value, crate::domain::InvokeError> {
            Err(crate::domain::InvokeError::Cancelled)
        }
    }

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

    fn state_with_client_file_and_storage(
        storage: Box<dyn StoragePort + Send + Sync>,
    ) -> Arc<RwLock<EngineState>> {
        let mut workspace = ProjectWorkspace::new();
        let mut index = InvertedIndex::new();
        let mut fs = InMemoryFs::new();

        let path = RelativePath::new("src/client.rs");
        let id = workspace
            .insert(path.clone(), FileMetadata::default())
            .unwrap();
        let file = workspace.get(id).unwrap().clone();
        index.insert(id, &tokenize("src/client.rs"));
        fs.write(path, b"fn client() {}".to_vec()).unwrap();

        let state = Arc::new(RwLock::new(EngineState::new(
            workspace,
            index,
            storage,
            Box::new(fs),
        )));
        state.write().unwrap().storage.put(file).unwrap();
        state
    }

    fn make_registry(state: Arc<RwLock<EngineState>>) -> NativeToolRegistry {
        NativeToolRegistry::new(state)
    }

    #[test]
    fn cancelled_semantic_rename_does_not_mutate_the_workspace() {
        let state = state_with_file("src/old.rs", "fn old() {}\n");
        state
            .write()
            .unwrap()
            .set_plugin_host(Arc::new(CancelledExtensionHost));
        let mut registry = make_registry(Arc::clone(&state));

        let error = registry
            .call(
                "tower_rename_path",
                json!({
                    "source": "src/old.rs",
                    "destination": "src/new.rs",
                    "semantic_policy": "best_effort",
                    "expected_versions": {}
                }),
                &crate::domain::NeverCancelled,
            )
            .expect_err("cancellation must abort the composite rename");

        assert!(matches!(error, ToolError::Cancelled));
        let guard = state.read().unwrap();
        assert!(guard.fs.read(&RelativePath::new("src/old.rs")).is_ok());
        assert!(guard.fs.read(&RelativePath::new("src/new.rs")).is_err());
    }

    #[test]
    fn cancelled_safe_delete_does_not_mutate_the_workspace() {
        let original = "fn target() {}\n";
        let state = state_with_file("src/lib.rs", original);
        state
            .write()
            .unwrap()
            .set_plugin_host(Arc::new(CancelledExtensionHost));
        let mut registry = make_registry(Arc::clone(&state));

        let error = registry
            .call(
                "tower_safe_delete_symbol",
                json!({
                    "path": "src/lib.rs",
                    "symbol_name": "target",
                    "kind": "function",
                    "dry_run": false
                }),
                &crate::domain::NeverCancelled,
            )
            .expect_err("cancellation must abort the composite safe-delete");

        assert!(matches!(error, ToolError::Cancelled));
        assert_eq!(
            state
                .read()
                .unwrap()
                .fs
                .read(&RelativePath::new("src/lib.rs"))
                .unwrap(),
            original.as_bytes()
        );
    }

    fn state_with_list_dir_files() -> Arc<RwLock<EngineState>> {
        let state = empty_state();
        let mut reg = make_registry(Arc::clone(&state));
        for (path, content) in [
            ("README.md", "# Project"),
            ("src/a.rs", "fn a() {}"),
            ("src/nested/b.rs", "fn b() {}"),
        ] {
            reg.call(
                "tower_create_file",
                json!({ "path": path, "content": content }),
                &crate::domain::NeverCancelled,
            )
            .expect("setup create_file must succeed");
        }
        state
    }

    fn list_dir_entries(value: &Value) -> Vec<(String, String, String)> {
        value["entries"]
            .as_array()
            .expect("list_dir response must contain entries array")
            .iter()
            .map(|entry| {
                (
                    entry["path"].as_str().unwrap().to_owned(),
                    entry["name"].as_str().unwrap().to_owned(),
                    entry["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn assert_invalid_args(err: ToolError) {
        assert!(
            matches!(err, ToolError::InvalidArgs(_)),
            "expected ToolError::InvalidArgs; got {err:?}"
        );
    }

    #[derive(Default)]
    struct FailBatchStorage {
        inner: InMemoryStorage,
    }

    impl StoragePort for FailBatchStorage {
        fn get(
            &self,
            id: crate::domain::FileId,
        ) -> Result<crate::domain::VirtualFile, crate::ports::PortError> {
            self.inner.get(id)
        }

        fn put(&mut self, file: crate::domain::VirtualFile) -> Result<(), crate::ports::PortError> {
            self.inner.put(file)
        }

        fn put_batch(
            &mut self,
            _files: &[crate::domain::VirtualFile],
        ) -> Result<(), crate::ports::PortError> {
            Err(crate::ports::PortError::WriteFailed(
                "injected put_batch failure".to_owned(),
            ))
        }

        fn delete(&mut self, id: crate::domain::FileId) -> Result<(), crate::ports::PortError> {
            self.inner.delete(id)
        }

        fn put_blob(
            &mut self,
            hash: crate::domain::ContentHash,
            bytes: Vec<u8>,
        ) -> Result<(), crate::ports::PortError> {
            self.inner.put_blob(hash, bytes)
        }

        fn get_blob(
            &self,
            hash: &crate::domain::ContentHash,
        ) -> Result<Vec<u8>, crate::ports::PortError> {
            self.inner.get_blob(hash)
        }

        fn mark_scan_complete(&mut self) -> Result<(), crate::ports::PortError> {
            self.inner.mark_scan_complete()
        }

        fn is_scan_complete(&self) -> Result<bool, crate::ports::PortError> {
            self.inner.is_scan_complete()
        }
    }

    #[test]
    fn t009_registry_names_match_expected_native_tool_names() {
        let reg = make_registry(empty_state());
        let tools = reg.list();
        let actual: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();

        assert_eq!(
            actual, EXPECTED_NATIVE_TOOL_NAMES,
            "NativeToolRegistry::list must stay aligned with the shared expected-name helper"
        );
    }

    #[test]
    fn t009_expected_native_tool_names_include_list_dir() {
        let expected = EXPECTED_NATIVE_TOOL_NAMES;

        assert!(
            expected.contains(&"tower_list_dir"),
            "expected native tool names must include tower_list_dir; got {expected:?}"
        );
    }

    #[test]
    fn t009_expected_native_tool_names_cover_registry_names() {
        let reg = make_registry(empty_state());
        let tools = reg.list();
        let actual: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();

        for expected_name in EXPECTED_NATIVE_TOOL_NAMES {
            assert!(
                actual.contains(&expected_name),
                "native registry unit assertions must include {expected_name}; got {actual:?}"
            );
        }
    }

    #[test]
    fn t009_native_tools_has_no_stale_nine_tool_expectations() {
        let source = include_str!("native_tools.rs");
        let stale_numeric = concat!("9", " native tools");
        let stale_word = concat!("nine", " native tools");
        let stale_doc = concat!("9", " `tower_*` tools");

        assert!(
            !source.contains(stale_numeric)
                && !source.contains(stale_word)
                && !source.contains(stale_doc),
            "native tool tests/comments must not retain stale 9-tool expectations"
        );
    }

    #[test]
    fn t009_native_tools_has_no_fragile_tool_count_comments() {
        let source = include_str!("native_tools.rs");
        let stale_implements = concat!("Implements all ", "9", " workspace tools");
        let fragile_count = concat!("10", " native `tower_*` tools");
        let fragile_doc_assert = concat!("assert_eq!(tools.len(), ", "10", ");");

        assert!(
            !source.contains(stale_implements)
                && !source.contains(fragile_count)
                && !source.contains(fragile_doc_assert),
            "native_tools.rs comments should avoid duplicating a native tool count that can drift"
        );
    }

    // ── T007: tower_list_dir native tool wiring ───────────────────────────────

    #[test]
    fn list_dir_native_tool_registry_list_includes_tower_list_dir_tool_descriptor() {
        let reg = make_registry(empty_state());
        let tools = reg.list();

        assert!(
            tools.iter().any(|tool| tool.name == "tower_list_dir"),
            "NativeToolRegistry::list must include tower_list_dir; got {:?}",
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn list_dir_native_tool_registry_call_dispatches_tower_list_dir_to_new_handler() {
        let mut reg = make_registry(state_with_list_dir_files());

        let value = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src" }),
                &crate::domain::NeverCancelled,
            )
            .expect("tower_list_dir call must dispatch to the handler and succeed");

        assert_eq!(
            list_dir_entries(&value),
            vec![
                ("src/a.rs".to_owned(), "a.rs".to_owned(), "file".to_owned()),
                (
                    "src/nested".to_owned(),
                    "nested".to_owned(),
                    "dir".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn list_dir_schema_describes_object_with_path_recursive_and_max_depth_properties() {
        let reg = make_registry(empty_state());
        let tool = reg
            .list()
            .into_iter()
            .find(|tool| tool.name == "tower_list_dir")
            .expect("tower_list_dir descriptor must be registered");

        assert_eq!(tool.input_schema["type"], "object");
        let properties = tool.input_schema["properties"]
            .as_object()
            .expect("tower_list_dir schema must declare object properties");
        assert!(properties.contains_key("path"));
        assert!(properties.contains_key("recursive"));
        assert!(properties.contains_key("max_depth"));
    }

    #[test]
    fn list_dir_handler_defaults_omitted_recursive_to_false() {
        let mut reg = make_registry(state_with_list_dir_files());

        let value = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src" }),
                &crate::domain::NeverCancelled,
            )
            .expect("omitted recursive must default to false");

        assert_eq!(
            list_dir_entries(&value),
            vec![
                ("src/a.rs".to_owned(), "a.rs".to_owned(), "file".to_owned()),
                (
                    "src/nested".to_owned(),
                    "nested".to_owned(),
                    "dir".to_owned()
                ),
            ],
            "default non-recursive listing must not include src/nested/b.rs"
        );
    }

    #[test]
    fn list_dir_handler_rejects_max_depth_when_recursive_is_omitted_or_false_with_tool_error_invalid_args()
     {
        let mut reg = make_registry(state_with_list_dir_files());

        let omitted = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src", "max_depth": 1 }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert_invalid_args(omitted);

        let false_recursive = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src", "recursive": false, "max_depth": 1 }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert_invalid_args(false_recursive);
    }

    #[test]
    fn list_dir_handler_rejects_max_depth_zero_with_tool_error_invalid_args() {
        let mut reg = make_registry(state_with_list_dir_files());

        let err = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src", "recursive": true, "max_depth": 0 }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert_invalid_args(err);
    }

    #[test]
    fn list_dir_handler_rejects_non_string_path_non_boolean_recursive_and_non_integer_max_depth_with_tool_error_invalid_args()
     {
        let mut reg = make_registry(state_with_list_dir_files());

        for args in [
            json!({ "path": 42 }),
            json!({ "path": "src", "recursive": "yes" }),
            json!({ "path": "src", "recursive": true, "max_depth": "1" }),
        ] {
            let err = reg
                .call("tower_list_dir", args, &crate::domain::NeverCancelled)
                .unwrap_err();
            assert_invalid_args(err);
        }
    }

    #[test]
    fn list_dir_call_with_file_path_maps_not_a_directory_to_tool_error_invalid_args_preserving_path()
     {
        let mut reg = make_registry(state_with_list_dir_files());
        let err = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src/a.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        match err {
            ToolError::InvalidArgs(message) => assert!(
                message.contains("src/a.rs"),
                "NotADirectory mapping must preserve requested path; got {message:?}"
            ),
            other => panic!("expected ToolError::InvalidArgs; got {other:?}"),
        }
    }

    #[test]
    fn list_dir_successful_response_is_json_object_containing_entries_with_path_name_and_kind_file_or_dir()
     {
        let mut reg = make_registry(state_with_list_dir_files());

        let value = reg
            .call(
                "tower_list_dir",
                json!({ "path": "src", "recursive": true }),
                &crate::domain::NeverCancelled,
            )
            .expect("recursive tower_list_dir must succeed");

        assert!(
            value.as_object().is_some(),
            "response must be a JSON object"
        );
        assert_eq!(
            list_dir_entries(&value),
            vec![
                ("src/a.rs".to_owned(), "a.rs".to_owned(), "file".to_owned()),
                (
                    "src/nested".to_owned(),
                    "nested".to_owned(),
                    "dir".to_owned()
                ),
                (
                    "src/nested/b.rs".to_owned(),
                    "b.rs".to_owned(),
                    "file".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn list_dir_native_registry_changes_remain_source_compatible_with_main_registry_usage() {
        fn accepts_registry_trait(registry: &mut dyn ToolRegistry) -> Result<Value, ToolError> {
            let tools = registry.list();
            assert!(tools.iter().any(|tool| tool.name == "tower_list_dir"));
            registry.call(
                "tower_list_dir",
                json!({ "path": "src" }),
                &crate::domain::NeverCancelled,
            )
        }

        let mut reg = make_registry(state_with_list_dir_files());
        let value = accepts_registry_trait(&mut reg)
            .expect("NativeToolRegistry must remain usable through ToolRegistry");

        assert!(value.get("entries").is_some());
    }

    // ── Extension-host notification on MCP mutations ──────────────────────────

    use std::sync::Mutex;

    use crate::domain::FileId;
    use crate::ports::{ExtensionHostPort, StoragePort};

    /// Records every `on_file_changed` / `on_file_indexed` call so tests can
    /// assert the domain broadcast actually reached the host.
    #[derive(Default)]
    struct RecordingExtensionHost {
        changed: Mutex<Vec<String>>,
        indexed: Mutex<Vec<String>>,
    }

    impl ExtensionHostPort for RecordingExtensionHost {
        fn on_file_indexed(&self, _id: FileId, path: &RelativePath) {
            self.indexed.lock().unwrap().push(path.as_str().to_owned());
        }
        fn on_file_changed(&self, _id: FileId, path: &RelativePath) {
            self.changed.lock().unwrap().push(path.as_str().to_owned());
        }
        fn on_file_deleted(&self, _path: &RelativePath) {}
        fn declared_tools(
            &self,
        ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
            Vec::new()
        }
        fn invoke(
            &self,
            tool_name: &str,
            _params: serde_json::Value,
            _: &dyn crate::domain::CancellationSignal,
        ) -> Result<serde_json::Value, crate::domain::InvokeError> {
            Err(crate::domain::InvokeError::ToolNotFound(
                tool_name.to_owned(),
            ))
        }
    }

    /// MCP create/delete must notify the real extension host (regression: handlers
    /// previously hard-coded `&NoOpExtensionHost`, so the AST extension never learned
    /// of MCP-driven mutations and its cross-file index went stale).
    #[test]
    fn mcp_mutations_notify_extension_host() {
        let state = empty_state();
        let host = Arc::new(RecordingExtensionHost::default());
        state
            .write()
            .unwrap()
            .set_plugin_host(Arc::clone(&host) as Arc<dyn ExtensionHostPort + Send + Sync>);

        let mut reg = make_registry(Arc::clone(&state));

        reg.call(
            "tower_create_file",
            json!({ "path": "src/lib.rs", "content": "fn lib() {}" }),
            &crate::domain::NeverCancelled,
        )
        .expect("create_file should succeed");

        reg.call(
            "tower_delete_file",
            json!({ "path": "src/lib.rs" }),
            &crate::domain::NeverCancelled,
        )
        .expect("delete_file should succeed");

        let changed = host.changed.lock().unwrap();
        assert!(
            changed.iter().any(|p| p == "src/lib.rs"),
            "create_file must broadcast on_file_changed; got {changed:?}"
        );
        assert!(
            changed.iter().filter(|p| *p == "src/lib.rs").count() >= 2,
            "delete_file must also broadcast on_file_changed; got {changed:?}"
        );
    }

    // ── AC1: tools/list shows native tools with schemas ───────────────────────

    #[test]
    fn ac1_tools_list_returns_ten_tower_tools() {
        let reg = make_registry(empty_state());
        let tools = reg.list();
        assert_eq!(
            tools.len(),
            EXPECTED_NATIVE_TOOL_NAMES.len(),
            "unexpected native tool count; got {}",
            tools.len()
        );
    }

    #[test]
    fn ac1_all_ten_tool_names_present() {
        let reg = make_registry(empty_state());
        let tool_list = reg.list();
        let names: Vec<&str> = tool_list.iter().map(|t| t.name.as_str()).collect();
        for name in EXPECTED_NATIVE_TOOL_NAMES {
            assert!(
                names.contains(&name),
                "missing tool '{name}'; got {names:?}"
            );
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
        let result = reg.call(
            "tower_find_file",
            json!({ "query": "client" }),
            &crate::domain::NeverCancelled,
        );
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
            .call(
                "tower_find_file",
                json!({ "query": "zzznomatch" }),
                &crate::domain::NeverCancelled,
            )
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
            &crate::domain::NeverCancelled,
        )
        .expect("create_file must succeed");

        // Find.
        let val = reg
            .call(
                "tower_find_file",
                json!({ "query": "widget" }),
                &crate::domain::NeverCancelled,
            )
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
            &crate::domain::NeverCancelled,
        )
        .unwrap();

        let val = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/readme.md" }),
                &crate::domain::NeverCancelled,
            )
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
        let err = reg
            .call("tower_find_file", json!({}), &crate::domain::NeverCancelled)
            .unwrap_err();
        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "missing 'query' must return InvalidArgs"
        );
    }

    #[test]
    fn ac4_missing_path_for_create_file_returns_invalid_args() {
        let mut reg = make_registry(empty_state());
        let err = reg
            .call(
                "tower_create_file",
                json!({ "content": "hi" }),
                &crate::domain::NeverCancelled,
            )
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
            .call(
                "tower_create_file",
                json!({ "path": "a.rs" }),
                &crate::domain::NeverCancelled,
            )
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
        let _ = reg.call(
            "tower_create_file",
            json!({ "path": "ghost.rs" }),
            &crate::domain::NeverCancelled,
        );

        // State must be unchanged: the file must NOT exist.
        let val = reg
            .call(
                "tower_find_file",
                json!({ "query": "ghost" }),
                &crate::domain::NeverCancelled,
            )
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
            .call(
                "tower_delete_file",
                json!({ "path": "nonexistent.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        // Must be ResourceNotFound (maps to -32002 on the wire).
        assert!(
            matches!(
                err,
                crate::adapters::mcp::types::ToolError::ResourceNotFound(_)
            ),
            "delete-missing must return ResourceNotFound for stable -32002 client detection; got: {err:?}"
        );
    }

    // ── tower_search_text round-trip ──────────────────────────────────────────

    #[test]
    fn search_text_finds_content_in_indexed_file() {
        let mut reg = make_registry(state_with_client_file());
        let val = reg
            .call(
                "tower_search_text",
                json!({ "pattern": "client" }),
                &crate::domain::NeverCancelled,
            )
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

        reg.call(
            "tower_delete_file",
            json!({ "path": "src/client.rs" }),
            &crate::domain::NeverCancelled,
        )
        .expect("delete of existing file must succeed");

        // File must no longer appear in find.
        let val = reg
            .call(
                "tower_find_file",
                json!({ "query": "client" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        let paths = val["paths"].as_array().unwrap();
        assert!(paths.is_empty(), "deleted file must not be findable");
    }

    // ── tower_create_directory ────────────────────────────────────────────────

    #[test]
    fn create_directory_succeeds_on_empty_state() {
        let mut reg = make_registry(empty_state());
        let val = reg
            .call(
                "tower_create_directory",
                json!({ "path": "a/b/c" }),
                &crate::domain::NeverCancelled,
            )
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
                &crate::domain::NeverCancelled,
            )
            .unwrap();

        assert!(
            val["files_changed"].as_u64().unwrap() >= 1,
            "global_replace must report at least one file changed"
        );

        // Content must have been updated.
        let read_val = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        let content = read_val["content"].as_str().unwrap();
        assert!(
            content.contains("server"),
            "content must contain replacement; got: {content}"
        );
    }

    // ── tower_reindex: no-op on empty fs ─────────────────────────────────────

    #[test]
    fn reindex_on_empty_state_returns_zero_files_indexed() {
        let mut reg = make_registry(empty_state());
        let val = reg
            .call("tower_reindex", json!({}), &crate::domain::NeverCancelled)
            .unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(99),
            0,
            "reindex on empty state must report 0 files_indexed"
        );
    }

    // ── tower_reindex: reconciles additions ──────────────────────────────────

    #[test]
    fn reindex_picks_up_file_added_to_fs_port() {
        let state = empty_state();

        // Inject a file directly into the fs port (simulating an external add).
        {
            let mut guard = state.write().unwrap();
            guard
                .fs
                .write(RelativePath::new("src/new.rs"), b"pub fn new() {}".to_vec())
                .unwrap();
        }

        let mut reg = make_registry(Arc::clone(&state));
        let val = reg
            .call("tower_reindex", json!({}), &crate::domain::NeverCancelled)
            .unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(0),
            1,
            "reindex must report 1 file after adding to fs"
        );

        // The file must now be findable via the index.
        let find = reg
            .call(
                "tower_find_file",
                json!({ "query": "new" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        let paths = find["paths"].as_array().unwrap();
        let path_strings: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
        assert!(
            path_strings.contains(&"src/new.rs"),
            "reindexed file must be findable; got {path_strings:?}"
        );
    }

    // ── tower_reindex: reconciles deletions ──────────────────────────────────

    #[test]
    fn reindex_removes_file_deleted_from_fs_port() {
        // Start with a file in the workspace + fs.
        let state = state_with_client_file();

        // Remove the file directly from the fs port (simulating an external delete).
        {
            let mut guard = state.write().unwrap();
            guard
                .fs
                .delete(&RelativePath::new("src/client.rs"))
                .unwrap();
        }

        let mut reg = make_registry(Arc::clone(&state));
        let val = reg
            .call("tower_reindex", json!({}), &crate::domain::NeverCancelled)
            .unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(99),
            0,
            "reindex must report 0 files after deleting the only file from fs"
        );

        // The deleted file must no longer be findable.
        let find = reg
            .call(
                "tower_find_file",
                json!({ "query": "client" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        let paths = find["paths"].as_array().unwrap();
        assert!(
            paths.is_empty(),
            "file removed from fs must not be findable after reindex"
        );
    }

    #[test]
    fn reindex_put_batch_failure_leaves_existing_persisted_record_recoverable() {
        let state = state_with_client_file_and_storage(Box::new(FailBatchStorage::default()));
        let client_id = {
            let guard = state.read().unwrap();
            let ws = guard.workspace.read().unwrap();
            ws.get_by_path(&RelativePath::new("src/client.rs"))
                .expect("setup file must be tracked")
        };

        {
            let mut guard = state.write().unwrap();
            guard
                .fs
                .delete(&RelativePath::new("src/client.rs"))
                .unwrap();
        }

        let mut reg = make_registry(Arc::clone(&state));
        let err = reg
            .call("tower_reindex", json!({}), &crate::domain::NeverCancelled)
            .unwrap_err();
        assert!(
            matches!(err, ToolError::ExecutionFailed(_)),
            "reindex must surface storage failure; got {err:?}"
        );

        let guard = state.read().unwrap();
        let persisted = guard.storage.get(client_id);
        assert!(
            persisted.is_ok(),
            "failed reindex must not delete old persisted record before fresh batch succeeds"
        );
    }

    // ── tower_reindex: populates real size + content hash ────────────────────

    #[test]
    fn reindex_populates_real_size_and_content_hash() {
        let state = empty_state();
        let content = b"pub fn alpha() -> u32 { 7 }".to_vec();
        let expected_size = content.len() as u64;
        {
            let mut guard = state.write().unwrap();
            guard
                .fs
                .write(RelativePath::new("src/alpha.rs"), content)
                .unwrap();
        }

        let mut reg = make_registry(Arc::clone(&state));
        reg.call("tower_reindex", json!({}), &crate::domain::NeverCancelled)
            .unwrap();

        let guard = state.read().unwrap();
        let ws = guard.workspace.read().unwrap();
        let id = ws
            .get_by_path(&RelativePath::new("src/alpha.rs"))
            .expect("reindexed file must be in the workspace");
        let vfile = ws.get(id).expect("slot must be live");
        assert_eq!(vfile.size, expected_size, "reindex must record real size");
        assert!(
            vfile.content_hash.is_some(),
            "reindex must record a content hash, not None"
        );
    }

    // ── tower_reindex: real-disk walk finds files created outside the engine ──

    #[test]
    fn reindex_walks_real_disk_for_externally_created_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Write a file DIRECTLY to disk — never through the fs port. RealFs::scan()
        // would NOT report it; only a true recursive walk does.
        let content = b"pub struct External {}";
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/external.rs"), content).unwrap();

        let state = Arc::new(RwLock::new(EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(crate::adapters::fs::RealFs::new(tmp.path())),
        )));

        let mut reg = make_registry(Arc::clone(&state));
        let val = reg
            .call("tower_reindex", json!({}), &crate::domain::NeverCancelled)
            .unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(0),
            1,
            "real-disk reindex must find the externally-created file"
        );

        let find = reg
            .call(
                "tower_find_file",
                json!({ "query": "external" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        let paths: Vec<&str> = find["paths"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            paths.contains(&"src/external.rs"),
            "externally-created file must be findable after reindex; got {paths:?}"
        );

        let guard = state.read().unwrap();
        let ws = guard.workspace.read().unwrap();
        let id = ws
            .get_by_path(&RelativePath::new("src/external.rs"))
            .expect("file must be in workspace after reindex");
        let vfile = ws.get(id).expect("slot must be live");
        assert_eq!(vfile.size, content.len() as u64, "real size recorded");
        assert!(vfile.content_hash.is_some(), "content hash recorded");
    }

    // ── tower_edit_range ──────────────────────────────────────────────────────

    /// Helper: create a file through the MCP `tower_create_file` tool, returning
    /// a registry ready for further calls.
    fn state_with_file(path: &str, content: &str) -> Arc<RwLock<EngineState>> {
        let state = empty_state();
        let mut reg = make_registry(Arc::clone(&state));
        reg.call(
            "tower_create_file",
            json!({ "path": path, "content": content }),
            &crate::domain::NeverCancelled,
        )
        .expect("setup create_file must succeed");
        state
    }

    /// AC1 (success): splice mid-file, report shape correct, content updated,
    /// surrounding bytes untouched.
    #[test]
    fn edit_range_success_splice_and_report() {
        // "fn main() {}" — replace bytes 3..7 ("main") with "run".
        let state = state_with_file("src/main.rs", "fn main() {}");
        let mut reg = make_registry(Arc::clone(&state));

        let val = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/main.rs",
                    "start_byte": 3,
                    "end_byte": 7,
                    "replacement": "run"
                }),
                &crate::domain::NeverCancelled,
            )
            .expect("edit_range must succeed");

        assert_eq!(val["files_changed"].as_u64().unwrap(), 1);
        assert_eq!(val["replacements"].as_u64().unwrap(), 1);
        assert!(val["errors"].as_array().unwrap().is_empty());

        // Verify the actual content via tower_read_file.
        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/main.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            read["content"].as_str().unwrap(),
            "fn run() {}",
            "spliced content must be correct; surrounding bytes untouched"
        );
    }

    /// AC2 (bad range): end_byte > file length → InvalidArgs, file unchanged.
    #[test]
    fn edit_range_end_beyond_len_returns_invalid_args() {
        // "abc" is 3 bytes; end_byte=10 is out of range.
        let state = state_with_file("src/bounds.rs", "abc");
        let mut reg = make_registry(Arc::clone(&state));

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/bounds.rs",
                    "start_byte": 0,
                    "end_byte": 10,
                    "replacement": "x"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "end > len must be InvalidArgs; got {err:?}"
        );

        // File must be unchanged.
        let read = make_registry(Arc::clone(&state))
            .call(
                "tower_read_file",
                json!({ "path": "src/bounds.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "abc");
    }

    /// AC3 (missing file): path not in VFS → ResourceNotFound.
    #[test]
    fn edit_range_missing_file_returns_resource_not_found() {
        let mut reg = make_registry(empty_state());

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "ghost.rs",
                    "start_byte": 0,
                    "end_byte": 0,
                    "replacement": "x"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(
                err,
                crate::adapters::mcp::types::ToolError::ResourceNotFound(_)
            ),
            "missing file must be ResourceNotFound; got {err:?}"
        );
    }

    /// AC4 (bad args): missing start_byte → InvalidArgs.
    #[test]
    fn edit_range_missing_start_byte_returns_invalid_args() {
        let state = state_with_file("src/a.rs", "hello");
        let mut reg = make_registry(state);

        let err = reg
            .call(
                "tower_edit_range",
                json!({ "path": "src/a.rs", "end_byte": 2, "replacement": "x" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "missing start_byte must be InvalidArgs; got {err:?}"
        );
    }

    /// AC4 (bad args): negative start_byte (JSON negative integer) → InvalidArgs.
    #[test]
    fn edit_range_negative_start_byte_returns_invalid_args() {
        let state = state_with_file("src/b.rs", "hello");
        let mut reg = make_registry(state);

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/b.rs",
                    "start_byte": -1,
                    "end_byte": 2,
                    "replacement": "x"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "negative start_byte must be InvalidArgs; got {err:?}"
        );
    }

    /// AC4 (bad args): non-integer start_byte (JSON string) → InvalidArgs.
    #[test]
    fn edit_range_non_integer_start_byte_returns_invalid_args() {
        let state = state_with_file("src/c.rs", "hello");
        let mut reg = make_registry(state);

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/c.rs",
                    "start_byte": "not-a-number",
                    "end_byte": 2,
                    "replacement": "x"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "string start_byte must be InvalidArgs; got {err:?}"
        );
    }

    /// AC5 (extension notification): on_file_changed is broadcast after a successful edit.
    #[test]
    fn edit_range_notifies_extension_host_on_success() {
        use std::sync::Mutex;

        use crate::domain::FileId;
        use crate::ports::ExtensionHostPort;

        #[derive(Default)]
        struct RecordingHost {
            changed: Mutex<Vec<String>>,
        }
        impl ExtensionHostPort for RecordingHost {
            fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}
            fn on_file_changed(&self, _id: FileId, path: &RelativePath) {
                self.changed.lock().unwrap().push(path.as_str().to_owned());
            }
            fn on_file_deleted(&self, _path: &RelativePath) {}
            fn declared_tools(
                &self,
            ) -> Vec<(crate::domain::ExtensionId, extension_protocol::ToolDecl)> {
                Vec::new()
            }
            fn invoke(
                &self,
                tool_name: &str,
                _params: serde_json::Value,
                _: &dyn crate::domain::CancellationSignal,
            ) -> Result<serde_json::Value, crate::domain::InvokeError> {
                Err(crate::domain::InvokeError::ToolNotFound(
                    tool_name.to_owned(),
                ))
            }
        }

        let state = state_with_file("src/notify.rs", "fn old() {}");
        let host = Arc::new(RecordingHost::default());
        state
            .write()
            .unwrap()
            .set_plugin_host(Arc::clone(&host) as Arc<dyn ExtensionHostPort + Send + Sync>);

        let mut reg = make_registry(Arc::clone(&state));
        reg.call(
            "tower_edit_range",
            json!({
                "path": "src/notify.rs",
                "start_byte": 3,
                "end_byte": 6,
                "replacement": "new"
            }),
            &crate::domain::NeverCancelled,
        )
        .expect("edit_range must succeed");

        let changed = host.changed.lock().unwrap();
        assert!(
            changed.iter().any(|p| p == "src/notify.rs"),
            "edit_range must broadcast on_file_changed; got {changed:?}"
        );
    }

    // ── TG3: start > end at the MCP layer → InvalidArgs, file unchanged ─────

    /// TG3: `start_byte > end_byte` must return `InvalidArgs` and leave the
    /// file unchanged. This exercises the MCP argument-to-domain error path for
    /// the `start > end` validation inside `FileMutationService::edit_range`.
    #[test]
    fn edit_range_start_greater_than_end_returns_invalid_args_and_file_unchanged() {
        let state = state_with_file("src/order_mcp.rs", "abcdef");
        let mut reg = make_registry(Arc::clone(&state));

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/order_mcp.rs",
                    "start_byte": 5,
                    "end_byte": 2,
                    "replacement": "x"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "start > end must return InvalidArgs at the MCP layer; got {err:?}"
        );

        // File must be unchanged.
        let read = make_registry(Arc::clone(&state))
            .call(
                "tower_read_file",
                json!({ "path": "src/order_mcp.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            read["content"].as_str().unwrap(),
            "abcdef",
            "file must be unchanged after rejected start > end edit"
        );
    }

    // ── TG4: UTF-8 boundary split at the MCP layer → InvalidArgs, file unchanged

    /// TG4: a `start_byte`/`end_byte` that lands inside a multi-byte UTF-8
    /// character must return `InvalidArgs` and leave the file unchanged.
    ///
    /// `"café"` in UTF-8: 'c'=0x63, 'a'=0x61, 'f'=0x66, 'é'=0xC3 0xA9 — total 5 bytes.
    /// Byte 3 is the start of 'é' (a char boundary); byte 4 (0xA9) is the second
    /// byte of 'é' and is NOT a char boundary. Requesting `start_byte=3, end_byte=4`
    /// splits 'é' in half.
    #[test]
    fn edit_range_utf8_boundary_split_returns_invalid_args_and_file_unchanged() {
        let content = "café"; // 5 bytes: c(1) a(1) f(1) é(2) = 5
        let state = state_with_file("src/utf8_mcp.rs", content);
        let mut reg = make_registry(Arc::clone(&state));

        // Sanity: confirm the byte layout is what we expect.
        assert_eq!(content.len(), 5, "café must be 5 bytes");
        assert!(
            !content.is_char_boundary(4),
            "byte 4 must not be a char boundary in 'café'"
        );

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/utf8_mcp.rs",
                    "start_byte": 3,
                    "end_byte": 4,
                    "replacement": "e"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(err, crate::adapters::mcp::types::ToolError::InvalidArgs(_)),
            "split inside multi-byte char must return InvalidArgs; got {err:?}"
        );

        // File must be unchanged.
        let read = make_registry(Arc::clone(&state))
            .call(
                "tower_read_file",
                json!({ "path": "src/utf8_mcp.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            read["content"].as_str().unwrap(),
            content,
            "file must be unchanged after rejected UTF-8 boundary split"
        );
    }

    // ── unknown tool ──────────────────────────────────────────────────────────

    #[test]
    fn unknown_tool_name_returns_not_found() {
        let mut reg = make_registry(empty_state());
        let err = reg
            .call(
                "tower_unknown_tool",
                json!({}),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            crate::adapters::mcp::types::ToolError::NotFound(_)
        ));
    }

    // ── Spec 18: CAS (compare-and-swap) MCP integration ──────────────────────

    /// AC4: `tower_read_file` without `with_version` returns exactly `{content}` —
    /// no `version` key present (backward-compatible, U1).
    #[test]
    fn read_file_without_with_version_has_no_version_key() {
        let state = state_with_client_file();
        let mut reg = make_registry(state);

        let val = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();

        assert!(val.get("content").is_some(), "must have content");
        assert!(
            val.get("version").is_none(),
            "must NOT have version key when with_version is absent; got {val}"
        );
    }

    /// OP1: `tower_read_file` with `with_version: true` includes `version` as a
    /// 64-char lowercase hex string alongside `content`.
    #[test]
    fn read_file_with_version_true_returns_hex_sha256() {
        let state = state_with_client_file();
        let mut reg = make_registry(state);

        let val = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();

        assert!(val.get("content").is_some(), "must have content");
        let version = val["version"].as_str().expect("must have version string");
        assert_eq!(version.len(), 64, "version must be 64 hex chars");
        assert!(
            version.chars().all(|c| c.is_ascii_hexdigit()),
            "version must be lowercase hex; got {version}"
        );
    }

    /// The version returned by `with_version: true` is stable across identical
    /// reads (same bytes → same hash).
    #[test]
    fn read_file_with_version_is_stable() {
        let state = state_with_client_file();
        let mut reg = make_registry(Arc::clone(&state));

        let v1 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        let v2 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        assert_eq!(v1, v2, "hash must be stable across identical reads");
    }

    /// AC1: read v0, `tower_create_file` with `expected_version=v0` on unchanged
    /// file → commits.
    #[test]
    fn cas_create_file_matching_version_commits() {
        let state = state_with_client_file();
        let mut reg = make_registry(Arc::clone(&state));

        // Read current version.
        let v0 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        // Write with matching version — must succeed.
        let result = reg.call(
            "tower_create_file",
            json!({
                "path": "src/client.rs",
                "content": "fn client_v2() {}",
                "expected_version": v0
            }),
            &crate::domain::NeverCancelled,
        );
        assert!(
            result.is_ok(),
            "CAS create_file with correct version must commit; got {result:?}"
        );

        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "fn client_v2() {}");
    }

    /// AC2: A reads v0; B writes (now v1); A tries `expected_version=v0` →
    /// PreconditionFailed, file stays at B's content.
    #[test]
    fn cas_create_file_stale_version_returns_precondition_failed() {
        let state = state_with_client_file();
        let mut reg = make_registry(Arc::clone(&state));

        // A reads v0.
        let v0 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        // B writes — changes the file.
        reg.call(
            "tower_create_file",
            json!({ "path": "src/client.rs", "content": "fn modified_by_b() {}" }),
            &crate::domain::NeverCancelled,
        )
        .unwrap();

        // A tries to write with stale v0.
        let err = reg
            .call(
                "tower_create_file",
                json!({
                    "path": "src/client.rs",
                    "content": "fn a_write() {}",
                    "expected_version": v0
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(
                err,
                crate::adapters::mcp::types::ToolError::PreconditionFailed(_)
            ),
            "stale CAS must return PreconditionFailed; got {err:?}"
        );

        // File must still hold B's content.
        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            read["content"].as_str().unwrap(),
            "fn modified_by_b() {}",
            "file must retain B's content after A's rejected CAS"
        );
    }

    /// AC3: `tower_create_file` without `expected_version` overwrites
    /// unconditionally — legacy path intact.
    #[test]
    fn cas_create_file_no_version_is_unconditional() {
        let state = state_with_client_file();
        let mut reg = make_registry(Arc::clone(&state));

        reg.call(
            "tower_create_file",
            json!({ "path": "src/client.rs", "content": "fn unconditional() {}" }),
            &crate::domain::NeverCancelled,
        )
        .expect("no expected_version → must always succeed");

        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/client.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "fn unconditional() {}");
    }

    /// AC1 variant for `tower_edit_range`: matching version → commits.
    #[test]
    fn cas_edit_range_matching_version_commits() {
        let state = state_with_file("src/edit_cas.rs", "fn main() {}");
        let mut reg = make_registry(Arc::clone(&state));

        let v0 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/edit_cas.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        let result = reg.call(
            "tower_edit_range",
            json!({
                "path": "src/edit_cas.rs",
                "start_byte": 3,
                "end_byte": 7,
                "replacement": "run",
                "expected_version": v0
            }),
            &crate::domain::NeverCancelled,
        );
        assert!(
            result.is_ok(),
            "matching CAS edit_range must commit; got {result:?}"
        );

        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/edit_cas.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "fn run() {}");
    }

    /// AC2 variant for `tower_edit_range`: stale version → PreconditionFailed, no write.
    #[test]
    fn cas_edit_range_stale_version_returns_precondition_failed() {
        let state = state_with_file("src/edit_stale.rs", "fn original() {}");
        let mut reg = make_registry(Arc::clone(&state));

        // Read v0 before B changes it.
        let v0 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/edit_stale.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        // B changes the file.
        reg.call(
            "tower_create_file",
            json!({ "path": "src/edit_stale.rs", "content": "fn modified() {}" }),
            &crate::domain::NeverCancelled,
        )
        .unwrap();

        // A tries edit_range with stale v0.
        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/edit_stale.rs",
                    "start_byte": 0,
                    "end_byte": 0,
                    "replacement": "// stale ",
                    "expected_version": v0
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();

        assert!(
            matches!(
                err,
                crate::adapters::mcp::types::ToolError::PreconditionFailed(_)
            ),
            "stale edit_range CAS must return PreconditionFailed; got {err:?}"
        );

        // File must hold the content B wrote.
        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/edit_stale.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "fn modified() {}");
    }

    /// AC5: two concurrent CAS writers with the same expected_version — exactly
    /// one commits, the other gets PreconditionFailed.
    ///
    /// Serialised sequentially here (the write-lock already serialises real
    /// concurrent calls in production; this proves the under-lock recompute path).
    #[test]
    fn cas_exactly_one_winner_two_writers_same_version() {
        let state = state_with_file("src/race.rs", "shared content");
        let mut reg = make_registry(Arc::clone(&state));

        let v0 = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/race.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        // Writer A commits.
        let a_result = reg.call(
            "tower_create_file",
            json!({
                "path": "src/race.rs",
                "content": "writer_a",
                "expected_version": v0.clone()
            }),
            &crate::domain::NeverCancelled,
        );
        assert!(a_result.is_ok(), "writer A must win; got {a_result:?}");

        // Writer B (same stale v0) must lose.
        let b_err = reg
            .call(
                "tower_create_file",
                json!({
                    "path": "src/race.rs",
                    "content": "writer_b",
                    "expected_version": v0
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert!(
            matches!(
                b_err,
                crate::adapters::mcp::types::ToolError::PreconditionFailed(_)
            ),
            "writer B must get PreconditionFailed; got {b_err:?}"
        );

        // Final content must be A's.
        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/race.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            read["content"].as_str().unwrap(),
            "writer_a",
            "A's write must be the winner"
        );
    }

    /// AC6: `tower_global_replace` with `expected_versions` where one file
    /// drifted → that file is in errors; non-conflicting file still commits.
    #[test]
    fn cas_global_replace_per_file_conflict_in_errors() {
        let state = empty_state();
        let mut reg = make_registry(Arc::clone(&state));

        // Create two files.
        reg.call(
            "tower_create_file",
            json!({ "path": "src/a.rs", "content": "let x = foo;" }),
            &crate::domain::NeverCancelled,
        )
        .unwrap();
        reg.call(
            "tower_create_file",
            json!({ "path": "src/b.rs", "content": "let y = foo;" }),
            &crate::domain::NeverCancelled,
        )
        .unwrap();

        // Read v0 for file b.
        let v0_b = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/b.rs", "with_version": true }),
                &crate::domain::NeverCancelled,
            )
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();

        // Externally modify b so v0_b is stale.
        reg.call(
            "tower_create_file",
            json!({ "path": "src/b.rs", "content": "let y = BAR;" }),
            &crate::domain::NeverCancelled,
        )
        .unwrap();

        // Run global_replace with stale guard on b.
        let val = reg
            .call(
                "tower_global_replace",
                json!({
                    "target": "foo",
                    "replacement": "bar",
                    "expected_versions": { "src/b.rs": v0_b }
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();

        // File a (no guard) must have been replaced.
        assert_eq!(
            val["files_changed"].as_u64().unwrap(),
            1,
            "only a must be changed; got {val}"
        );

        let a_read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/a.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(a_read["content"].as_str().unwrap(), "let x = bar;");

        // File b must appear in errors.
        let errors = val["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "b must be in errors; got {val}");
        assert_eq!(errors[0]["path"].as_str().unwrap(), "src/b.rs");

        // File b's content must be unchanged (external write preserved).
        let b_read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/b.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(b_read["content"].as_str().unwrap(), "let y = BAR;");
    }

    /// A malformed `expected_version` (non-string) must be rejected with
    /// InvalidArgs — a safety guard that fails open would silently downgrade to
    /// an unconditional write and clobber a concurrent writer.
    #[test]
    fn cas_create_file_non_string_version_is_rejected() {
        let state = state_with_file("src/x.rs", "v0");
        let mut reg = make_registry(Arc::clone(&state));

        let err = reg
            .call(
                "tower_create_file",
                json!({ "path": "src/x.rs", "content": "v1", "expected_version": 123 }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(_)),
            "non-string expected_version must be InvalidArgs; got {err:?}"
        );
        // The file must be untouched — the malformed call wrote nothing.
        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/x.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "v0");
    }

    /// Same fail-closed rule for `tower_edit_range`.
    #[test]
    fn cas_edit_range_non_string_version_is_rejected() {
        let state = state_with_file("src/y.rs", "abcdef");
        let mut reg = make_registry(Arc::clone(&state));

        let err = reg
            .call(
                "tower_edit_range",
                json!({
                    "path": "src/y.rs",
                    "start_byte": 0,
                    "end_byte": 1,
                    "replacement": "Z",
                    "expected_version": true
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(_)),
            "non-string expected_version must be InvalidArgs; got {err:?}"
        );
    }

    /// `expected_versions` with a non-string value is a malformed guard map and
    /// must be rejected, not silently degraded to an unconditional replace.
    #[test]
    fn cas_global_replace_non_string_version_value_is_rejected() {
        let state = state_with_file("src/z.rs", "foo");
        let mut reg = make_registry(Arc::clone(&state));

        let err = reg
            .call(
                "tower_global_replace",
                json!({
                    "target": "foo",
                    "replacement": "bar",
                    "expected_versions": { "src/z.rs": 42 }
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(_)),
            "non-string version value must be InvalidArgs; got {err:?}"
        );
        // Nothing was replaced.
        let read = reg
            .call(
                "tower_read_file",
                json!({ "path": "src/z.rs" }),
                &crate::domain::NeverCancelled,
            )
            .unwrap();
        assert_eq!(read["content"].as_str().unwrap(), "foo");
    }

    /// `expected_versions` that is not even an object is rejected.
    #[test]
    fn cas_global_replace_non_object_versions_is_rejected() {
        let state = state_with_file("src/w.rs", "foo");
        let mut reg = make_registry(Arc::clone(&state));

        let err = reg
            .call(
                "tower_global_replace",
                json!({
                    "target": "foo",
                    "replacement": "bar",
                    "expected_versions": "not-an-object"
                }),
                &crate::domain::NeverCancelled,
            )
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(_)),
            "non-object expected_versions must be InvalidArgs; got {err:?}"
        );
    }
}
