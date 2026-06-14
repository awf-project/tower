//! Wasmtime plugin adapter (specs 11c, 11d).
//!
//! Implements the wasmtime-backed [`crate::domain::PluginInstance`] trait,
//! the [`WasmtimeHost::load`] entry point (11c), and the fault-isolation layer
//! [`IsolatedSandbox`] with fuel/epoch bounds and restart/quarantine (11d).
//!
//! # Hexagonal boundary
//!
//! This module is the **only** place in `core_engine` that imports `wasmtime`.
//! The domain layer (`domain/plugin_host`) remains wasmtime-free; it stores
//! `Box<dyn PluginInstance>` trait objects via `PluginHostRegistry::register`.
//!
//! # Security boundary
//!
//! The capability linker here is the security perimeter for the plugin system.
//! Only six host functions are exposed to wasm guests (stage 4a):
//!
//! | Symbol | Module | Description |
//! |--------|--------|-------------|
//! | `host_log` | `tower_host` | Write a log message to host stderr. |
//! | `host_read_file` | `tower_host` | Read a workspace file via `FileSystemPort`. |
//! | `ast_store_put` | `tower_host` | Persist opaque bytes via `AstIndexPort` (opaque key only). |
//! | `ast_store_get` | `tower_host` | Retrieve opaque bytes via `AstIndexPort` (opaque key only). |
//! | `host_list_files` | `tower_host` | Enumerate workspace-relative file paths (read-only snapshot). |
//! | `host_request_format` | `tower_host` | Enqueue a format job; returns 0=Accepted, 1=Dropped (spec 13a). |
//!
//! Any other import from `tower_host` causes a link error at instantiation.
//! WASI (`wasi_snapshot_preview1::*`) is linked but with a zero-capability
//! `WasiCtx` (no preopened dirs, no network) — see [`loader`] for details.

pub mod error;
pub mod install;
pub mod isolation;
pub mod loader;
pub mod runtime;

pub use error::PluginLoadError;
pub use install::{InstalledPlugin, Scope, global_plugins_dir};
pub use isolation::{IsolatedSandbox, IsolationConfig, IsolationEngine, MAX_CONSECUTIVE_FAILURES};
pub use loader::{HostDeps, WasmtimeHost};
pub use runtime::{
    DEFAULT_PLUGINS_SUBDIR, load_plugins_into_registry, production_isolation_config,
    resolve_plugin_dirs, resolve_plugins_dir,
};
