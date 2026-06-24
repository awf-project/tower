//! Sidecar extension adapter — spec 23.
//!
//! `SidecarHostAdapter` implements [`ExtensionInstance`] over a real child process
//! communicating via JSON-RPC 2.0 over stdio. It:
//!
//! 1. Spawns the extension process from `manifest.command`.
//! 2. Performs the `initialize` handshake (sends `InitParams`, reads `InitResult`,
//!    rejects on protocol-version mismatch — UN1).
//! 3. Forwards `call_tool` as `invokeTool` requests and `deliver_event` as
//!    `deliverEvent` requests.
//! 4. Dispatches **inbound `HostCall`** requests from the extension to the
//!    existing outbound ports (`FileSystemPort`, `AstIndexPort`, `FormatQueuePort`).
//! 5. Enforces path validation (no empty, absolute, or `..`-traversal paths — UN2).
//!
//! # Protocol flow
//!
//! ```text
//! host                                 extension
//!   ──initialize──────────────────────►
//!   ◄──Initialized{tools,events,...}──
//!   ──invokeTool──────────────────────►
//!   │                   ◄──workspace/readFile──
//!   │ →[read via port]
//!   │ ──[result]────────────────────────►
//!   ◄──ToolResult──────────────────────
//!   ──shutdown────────────────────────►
//!   ◄──Ack────────────────────────────
//! ```
//!
//! # Concurrency model
//!
//! A background reader thread owns the child's stdout and routes incoming frames:
//! - If a frame is a response (has `id` matching a pending request): delivers it
//!   via a `(Mutex, Condvar)` pair.
//! - If a frame is a `HostCall` request (the extension is calling back): dispatches
//!   to the appropriate port and writes the response back to child stdin.
//!
//! Writing to child stdin is serialised by a `Mutex<ChildStdin>` shared between
//! the adapter's main methods and the reader thread.
//!
//! Default: **one in-flight request at a time** per extension (serial). The
//! adapter holds a `Mutex<PendingSlot>` so a second `call_tool` blocks until the
//! first completes.
//!
//! # Hexagonal boundary
//!
//! This module is in `adapters/` — it imports `std::process`, `std::io`, and the
//! port traits. It must never be imported by `domain/`.

#![forbid(unsafe_code)]

pub mod discovery;
pub mod host_deps;
pub mod path_validation;
pub mod sidecar;
pub mod supervisor;

#[cfg(test)]
mod supervisor_tests;
#[cfg(test)]
mod tests;

pub use discovery::{
    ExtensionInitConfigMap, ManifestLoadError, Shadow, global_extensions_dir,
    load_extensions_into_existing_registry, load_extensions_into_registry,
    load_extensions_into_shared_registry, resolve_extension_dirs,
};
pub use host_deps::HostDeps;
pub use sidecar::SidecarHostAdapter;
pub use supervisor::ExtensionSupervisor;
