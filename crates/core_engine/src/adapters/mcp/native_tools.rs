//! Native `tower_*` tool handlers — spec 10b.
//!
//! Implements all 8 workspace tools and registers them into the 10a
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

use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::domain::index::{FileSearch, InvertedIndex};
use crate::domain::mutation::FileMutationService;
use crate::domain::token::tokenize;
use crate::domain::virtual_file::FileMetadata;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::{FileMutationUseCase, SearchUseCase};
use crate::ports::{FileSystemPort, NoOpPluginHost, StoragePort};

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
    pub workspace: Arc<RwLock<ProjectWorkspace>>,
    pub index: Arc<RwLock<InvertedIndex>>,
    pub storage: Box<dyn StoragePort + Send + Sync>,
    pub fs: Box<dyn FileSystemPort + Send + Sync>,
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
        }
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

/// [`ToolRegistry`] implementation for the 8 native `tower_*` tools (spec 10b).
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
/// assert_eq!(tools.len(), 8);
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
            tool_reindex_desc(),
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
            "tower_reindex" => call_reindex(&self.state),
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

    // Only `fs` is needed here; take the outer read-lock for fs access.
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

    // Outer write-lock gives exclusive access to storage + fs.
    // Destructure to split the mutable borrow across fields — the borrow
    // checker cannot split it through a DerefMut guard, but it CAN through a
    // raw &mut reference to the struct.
    let mut guard = state.write().map_err(lock_poisoned)?;
    let ws_arc = Arc::clone(&guard.workspace);
    let idx_arc = Arc::clone(&guard.index);
    let mut ws = ws_arc.write().map_err(lock_poisoned)?;
    let mut idx = idx_arc.write().map_err(lock_poisoned)?;
    // Split the &mut EngineState into independent field borrows.
    let engine = &mut *guard;
    let mut svc = FileMutationService::new(
        engine.fs.as_mut(),
        &mut ws,
        &mut idx,
        engine.storage.as_mut(),
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
    let ws_arc = Arc::clone(&guard.workspace);
    let idx_arc = Arc::clone(&guard.index);
    let mut ws = ws_arc.write().map_err(lock_poisoned)?;
    let mut idx = idx_arc.write().map_err(lock_poisoned)?;
    let engine = &mut *guard;
    let mut svc = FileMutationService::new(
        engine.fs.as_mut(),
        &mut ws,
        &mut idx,
        engine.storage.as_mut(),
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
    let ws_arc = Arc::clone(&guard.workspace);
    let idx_arc = Arc::clone(&guard.index);
    let mut ws = ws_arc.write().map_err(lock_poisoned)?;
    let mut idx = idx_arc.write().map_err(lock_poisoned)?;
    let engine = &mut *guard;
    let mut svc = FileMutationService::new(
        engine.fs.as_mut(),
        &mut ws,
        &mut idx,
        engine.storage.as_mut(),
        &NoOpPluginHost,
    );
    svc.delete_file(&rel).map_err(domain_err_to_tool_error)?;

    Ok(json!({ "deleted": true }))
}

fn call_global_replace(state: &Arc<RwLock<EngineState>>, args: Value) -> Result<Value, ToolError> {
    let target = require_str(&args, "target")?;
    let replacement = require_str(&args, "replacement")?;

    let mut guard = state.write().map_err(lock_poisoned)?;
    let ws_arc = Arc::clone(&guard.workspace);
    let idx_arc = Arc::clone(&guard.index);
    let mut ws = ws_arc.write().map_err(lock_poisoned)?;
    let mut idx = idx_arc.write().map_err(lock_poisoned)?;
    let engine = &mut *guard;
    let mut svc = FileMutationService::new(
        engine.fs.as_mut(),
        &mut ws,
        &mut idx,
        engine.storage.as_mut(),
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

/// Force a full rebuild of the VFS + text-search index from the current filesystem.
///
/// # Algorithm
///
/// 1. Walk the filesystem via `FileSystemPort::scan()` (no locks held during the walk).
/// 2. Build a fresh `ProjectWorkspace` and `InvertedIndex` from the scan results.
/// 3. Persist the rebuilt state: write fresh records via `StoragePort::put_batch`,
///    delete records whose paths are absent from the scan, then call
///    `StoragePort::mark_scan_complete()`.
/// 4. Acquire workspace→index write locks in that order and swap in the rebuilt
///    structures.
///
/// # Concurrency trade-off
///
/// Decision: perform the filesystem walk and in-memory rebuild WITHOUT holding the
/// live workspace/index write-locks. The locks are only acquired for the final swap
/// in step 4.
/// Why: the fs walk may be long (thousands of files); holding write-locks for its
/// duration would starve the watcher and all MCP readers.
/// Trade-off: a watcher event applied to the live workspace during the rebuild
/// window is superseded by the swap, but the next watcher event will re-apply the
/// change. For a deliberate forced reindex (external drift correction) this is
/// acceptable — the caller is explicitly requesting a full reset.
///
/// # FileId generations
///
/// A fresh rebuild reassigns `FileId`s from a new empty `ProjectWorkspace`. Stale
/// external `FileId`s from the previous workspace will fail `validated_index` checks
/// (generation mismatch) — the generational invariant is preserved.
fn call_reindex(state: &Arc<RwLock<EngineState>>) -> Result<Value, ToolError> {
    // Phase 1: enumerate workspace files WITH real metadata (size, mtime,
    // content hash) WITHOUT holding the workspace/index write-locks.
    //
    // Decision: for a real filesystem, perform a TRUE recursive walk via
    // `collect_workspace_files` rather than `fs.scan()`.
    // Why: `RealFs::scan()` only reports paths written through the adapter this
    // session — using it for a project reindex would shrink the index to the
    // session's own writes and miss files created/edited outside the engine.
    // Trade-off: the walk reads each file's bytes to compute the content hash.
    // In-memory adapters (tests) have no real root → fall back to `scan()` +
    // read, still computing real size and hash.
    let entries: Vec<(RelativePath, FileMetadata)> = {
        let guard = state.read().map_err(lock_poisoned)?;
        match guard.fs.workspace_root() {
            Some(root) => crate::adapters::fs::scan::collect_workspace_files(&root),
            None => guard
                .fs
                .scan()
                .into_iter()
                .filter(|p| !crate::domain::mutation::is_tmp_artifact(p))
                .map(|p| {
                    let bytes = guard.fs.read(&p).unwrap_or_default();
                    let metadata = FileMetadata {
                        size: bytes.len() as u64,
                        modified: crate::domain::virtual_file::Timestamp(0),
                        content_hash: Some(crate::adapters::fs::scan::content_hash_of(&bytes)),
                    };
                    (p, metadata)
                })
                .collect(),
        }
    };

    // Phase 2: build fresh in-memory structures from the enumerated entries.
    let mut new_workspace = ProjectWorkspace::new();
    let mut new_index = InvertedIndex::new();
    let mut new_files: Vec<crate::domain::VirtualFile> = Vec::with_capacity(entries.len());

    for (rel_path, metadata) in &entries {
        match new_workspace.insert(rel_path.clone(), *metadata) {
            Ok(file_id) => {
                let virtual_file = new_workspace
                    .get(file_id)
                    .expect("just inserted — slot must be live")
                    .clone();
                let tokens = tokenize(rel_path.as_str());
                new_index.insert(file_id, &tokens);
                new_files.push(virtual_file);
            }
            Err(_) => {
                // DuplicatePath or other domain error — skip the duplicate.
                // A legitimate filesystem scan cannot produce duplicate paths,
                // so this branch is defensive only.
                continue;
            }
        }
    }

    let files_indexed = new_files.len();

    // Phase 3: persist the rebuilt state.
    //
    // Acquire the outer write-lock for exclusive storage access.
    // We do NOT hold the workspace/index locks during storage I/O — storage
    // writes can be slow and the watcher must not be starved.
    //
    // Deletion reconciliation: the previous storage records that are no longer
    // in the fresh scan must be removed.  We enumerate what is currently stored
    // via the live workspace snapshot (held under the outer write-lock, which
    // prevents concurrent mutations), then delete any `FileId` not present in the
    // fresh workspace.  The fresh records are written via `put_batch`, and the
    // scan-complete marker is re-set so the restart guard sees a consistent index.
    {
        let mut guard = state.write().map_err(lock_poisoned)?;

        // Collect stale FileIds from the live workspace before the swap.
        let stale_ids: Vec<crate::domain::FileId> = {
            let ws = guard.workspace.read().map_err(lock_poisoned)?;
            ws.snapshot().files.into_iter().map(|f| f.id).collect()
        };

        // Delete every stale record.  Errors (e.g. already absent) are benign.
        for id in stale_ids {
            let _ = guard.storage.delete(id);
        }

        // Write all fresh records in a single atomic batch.
        guard
            .storage
            .put_batch(&new_files)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        // Re-set the scan-complete marker so restart guard sees a complete index.
        guard
            .storage
            .mark_scan_complete()
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
    }

    // Phase 4: swap the rebuilt in-memory structures into the live state.
    // Acquire workspace→index in that order (matching the watcher lock discipline).
    {
        let guard = state.read().map_err(lock_poisoned)?;
        let mut ws = guard.workspace.write().map_err(lock_poisoned)?;
        let mut idx = guard.index.write().map_err(lock_poisoned)?;
        *ws = new_workspace;
        *idx = new_index;
    }

    Ok(json!({ "files_indexed": files_indexed }))
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

    use serde_json::{Value, json};

    use super::{EngineState, NativeToolRegistry};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::RelativePath;
    use crate::domain::index::InvertedIndex;
    use crate::domain::token::tokenize;
    use crate::domain::virtual_file::FileMetadata;
    use crate::domain::workspace::ProjectWorkspace;
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

    // ── AC1: tools/list shows 8 tools with schemas ────────────────────────────

    #[test]
    fn ac1_tools_list_returns_eight_tower_tools() {
        let reg = make_registry(empty_state());
        let tools = reg.list();
        assert_eq!(
            tools.len(),
            8,
            "expected 8 native tools; got {}",
            tools.len()
        );
    }

    #[test]
    fn ac1_all_eight_tool_names_present() {
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
            "tower_reindex",
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

    // ── tower_reindex: no-op on empty fs ─────────────────────────────────────

    #[test]
    fn reindex_on_empty_state_returns_zero_files_indexed() {
        let mut reg = make_registry(empty_state());
        let val = reg.call("tower_reindex", json!({})).unwrap();
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
        let val = reg.call("tower_reindex", json!({})).unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(0),
            1,
            "reindex must report 1 file after adding to fs"
        );

        // The file must now be findable via the index.
        let find = reg
            .call("tower_find_file", json!({ "query": "new" }))
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
        let val = reg.call("tower_reindex", json!({})).unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(99),
            0,
            "reindex must report 0 files after deleting the only file from fs"
        );

        // The deleted file must no longer be findable.
        let find = reg
            .call("tower_find_file", json!({ "query": "client" }))
            .unwrap();
        let paths = find["paths"].as_array().unwrap();
        assert!(
            paths.is_empty(),
            "file removed from fs must not be findable after reindex"
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
        reg.call("tower_reindex", json!({})).unwrap();

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
        let val = reg.call("tower_reindex", json!({})).unwrap();
        assert_eq!(
            val["files_indexed"].as_u64().unwrap_or(0),
            1,
            "real-disk reindex must find the externally-created file"
        );

        let find = reg
            .call("tower_find_file", json!({ "query": "external" }))
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
