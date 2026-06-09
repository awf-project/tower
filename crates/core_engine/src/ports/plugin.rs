//! `PluginHostPort` — outbound port for plugin lifecycle hooks.
//!
//! # Declaration only (spec 02 scope)
//!
//! This trait is declared here so the domain can accept an `&dyn PluginHostPort`
//! in its constructor and broadcast lifecycle events without coupling to a
//! concrete wasmtime runtime. The wasmtime adapter lands in spec 11.
//!
//! # No-op implementation (AC3 / OP1)
//!
//! [`NoOpPluginHost`] satisfies the trait with empty bodies. The domain wires
//! this when no real plugin host is registered, making the hooks truly optional
//! without any `Option`-wrapping at call sites.

use crate::domain::{FileId, RelativePath};

/// Plugin lifecycle hook contract.
///
/// Each method is called by the domain after a domain event; the host
/// dispatches to registered plugins. Implementations must be infallible from
/// the domain's perspective (errors are logged internally, never propagated).
///
/// # Object safety
///
/// The trait is intentionally object-safe so the domain holds
/// `&dyn PluginHostPort` without monomorphisation.
pub trait PluginHostPort {
    /// Called after a file has been indexed (content hash computed and stored).
    ///
    /// `id` identifies the file in the workspace; `path` is provided for
    /// convenience so plugins do not need a workspace handle.
    fn on_file_indexed(&self, id: FileId, path: &RelativePath);

    /// Called after a file's content has changed (re-indexed after a write).
    fn on_file_changed(&self, id: FileId, path: &RelativePath);
}

/// A no-op [`PluginHostPort`] that discards all hook calls (OP1 / AC3).
///
/// Wire this into domain services when no real plugin runtime is available.
/// Behaviour must be identical to having no host registered (AC3).
///
/// # Example
///
/// ```rust
/// use core_engine::ports::NoOpPluginHost;
///
/// // NoOpPluginHost is zero-sized and trivially constructible.
/// let host = NoOpPluginHost;
/// // Hooks compile and are callable; all calls are silently discarded.
/// let _ = host; // nothing to assert — absence of side-effects is the contract
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpPluginHost;

impl PluginHostPort for NoOpPluginHost {
    #[inline]
    fn on_file_indexed(&self, _id: FileId, _path: &RelativePath) {}

    #[inline]
    fn on_file_changed(&self, _id: FileId, _path: &RelativePath) {}
}
