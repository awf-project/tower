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

#[cfg(test)]
pub(crate) mod timeout_test_support {
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn timeout_fixture_bin() -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("core_engine must be under crates")
            .parent()
            .expect("crates must be under the workspace root")
            .join("target/debug/fixture_hang_forever")
            .to_string_lossy()
            .into_owned()
    }

    pub(crate) fn timeout_manifest(name: &str, activation: &str) -> String {
        format!(
            r#"name = "{name}"
version = "0.1.0"
command = ["{}"]
activation = "{activation}"

[[tools]]
name = "wait"
description = "Wait forever"
schema_json = "{{}}"
"#,
            timeout_fixture_bin().replace('\\', "\\\\")
        )
    }

    pub(crate) fn assert_timeout_after(
        instance: &mut Box<dyn crate::domain::ExtensionInstance>,
        minimum: Duration,
    ) -> Duration {
        let started = Instant::now();
        let result = instance.call_tool(
            "wait",
            serde_json::json!({}),
            &crate::domain::NeverCancelled,
        );
        let elapsed = started.elapsed();
        assert!(matches!(
            result,
            Err(crate::domain::extension_host::ExtensionCallError::Fault(
                extension_protocol::ExtensionFault::Timeout { .. }
            ))
        ));
        assert!(
            elapsed >= minimum,
            "request timed out after {elapsed:?}, before {minimum:?}"
        );
        elapsed
    }
}
