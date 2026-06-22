//! Diagnostics port and server-initiated push types for the MCP adapter.
//!
//! These types support the `resources/read` and `notifications/resources/updated`
//! paths of the rmcp [`ServerHandler`](super::rmcp_server::TowerMcpHandler).
//! They carry no wire-format concerns — rmcp 1.7 owns all JSON-RPC framing.
//!
//! - [`DiagnosticsReader`] — read-only port for `resources/read`.
//! - [`NoOpDiagnosticsReader`] — no-op implementation for the sidecar model.
//! - [`PushEvent`] — a server-initiated diagnostics push, bridged via `mpsc`
//!   and forwarded to the peer by `rmcp_server::spawn_push_task`.

// ── DiagnosticsReader trait ───────────────────────────────────────────────────

/// One-method read-only trait threaded into the MCP handler for
/// `resources/read`.
///
/// In the sidecar-extension model diagnostics are pushed via the extension's
/// `notify/resourceUpdated` host-call rather than polled from a language-server
/// pool. The handler still accepts a `dyn DiagnosticsReader` so the
/// `resources/read` path returns an authoritative (possibly empty) answer.
pub trait DiagnosticsReader: Send + Sync {
    /// Return the last published diagnostics for the given LSP URI, or `[]`
    /// when no live session exists for that URI's extension.
    fn diagnostics_for(&self, uri: &str) -> Vec<crate::domain::code_intel::Diagnostic>;
}

/// A no-op [`DiagnosticsReader`] that always returns an empty diagnostics list.
///
/// Used in the sidecar-extension model where diagnostics are pushed via the
/// extension's `notify/resourceUpdated` host-call rather than polled through a
/// session pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpDiagnosticsReader;

impl DiagnosticsReader for NoOpDiagnosticsReader {
    fn diagnostics_for(&self, _uri: &str) -> Vec<crate::domain::code_intel::Diagnostic> {
        Vec::new()
    }
}

// ── Push event type ───────────────────────────────────────────────────────────

/// A server-initiated diagnostics push event, bridged from the LSP/extension
/// layer via `mpsc`. Carries only the URI and generation — no MCP-specific
/// types, so this struct lives cleanly without importing rmcp internals.
///
/// The push-forwarding task in `rmcp_server` converts this into an rmcp
/// `ResourceUpdatedNotificationParam` and sends it via `Peer::notify_resource_updated`.
pub struct PushEvent {
    pub uri: String,
    /// Diagnostics generation, mirroring the generation-tagging used throughout
    /// the LSP adapter (`record_diagnostics` bumps it on every publish).
    ///
    /// Reserved: the MCP `resources/updated` notification carries no generation,
    /// so `spawn_push_task` does not currently consult this field. The
    /// `notify/resourceUpdated` host-call also does not thread it through, so the
    /// sidecar path sets `0` ("unknown"). Kept so a future de-duplication /
    /// staleness gate on the push channel can use it without a wire change.
    pub generation: u64,
}
