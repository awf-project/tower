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
//! Only two host functions are exposed to wasm guests:
//!
//! | Symbol | Module | Description |
//! |--------|--------|-------------|
//! | `host_log` | `tower_host` | Write a log message to host stderr. |
//! | `host_read_file` | `tower_host` | Read a workspace file via `FileSystemPort`. |
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
pub use install::{global_plugins_dir, InstalledPlugin, Scope};
pub use isolation::{IsolatedSandbox, IsolationConfig, IsolationEngine, MAX_CONSECUTIVE_FAILURES};
pub use loader::WasmtimeHost;
pub use runtime::{
    load_plugins_into_registry, production_isolation_config, resolve_plugin_dirs,
    resolve_plugins_dir, DEFAULT_PLUGINS_SUBDIR,
};
