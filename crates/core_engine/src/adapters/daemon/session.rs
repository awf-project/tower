//! Per-connection session bookkeeping: keep-alive counting (drives the idle
//! timeout) and push fan-out to subscribed MCP peers.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use rmcp::Peer;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::RoleServer;
use tokio::sync::Notify;

use crate::adapters::mcp::lsp_tools::SubscriptionRegistry;

/// One live MCP session (a connected, initialized client).
struct SessionEntry {
    id: u64,
    peer: Peer<RoleServer>,
    sub: Arc<Mutex<SubscriptionRegistry>>,
}

struct Inner {
    sessions: Vec<SessionEntry>,
    next_id: u64,
    initializing: usize,
}

/// Registry of live keep-alive sessions, shared across the accept loop, the
/// idle monitor, and the push fan-out task.
pub struct SessionRegistry {
    inner: Mutex<Inner>,
    /// Pinged on every register/unregister so the idle monitor can re-evaluate.
    pub count_changed: Notify,
}

impl SessionRegistry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                sessions: Vec::new(),
                next_id: 1,
                initializing: 0,
            }),
            count_changed: Notify::new(),
        })
    }

    /// Count an accepted MCP connection while rmcp performs initialization.
    pub fn register_initializing(&self) {
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.initializing += 1;
        }
        self.count_changed.notify_waiters();
    }

    /// Drop an initializing connection that closed or failed before serving.
    pub fn unregister_initializing(&self) {
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.initializing = g.initializing.saturating_sub(1);
        }
        self.count_changed.notify_waiters();
    }

    /// Register a fully initialized session; returns its id.
    pub fn register_initialized(
        &self,
        peer: Peer<RoleServer>,
        sub: Arc<Mutex<SubscriptionRegistry>>,
    ) -> u64 {
        let id = {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.initializing = g.initializing.saturating_sub(1);
            let id = g.next_id;
            g.next_id += 1;
            g.sessions.push(SessionEntry { id, peer, sub });
            id
        };
        self.count_changed.notify_waiters();
        id
    }

    /// Remove a session. Decrements the keep-alive count.
    pub fn unregister(&self, id: u64) {
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.sessions.retain(|s| s.id != id);
        }
        self.count_changed.notify_waiters();
    }

    /// Number of live keep-alive sessions (mcp/observer).
    #[must_use]
    pub fn keep_alive_count(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.sessions.len() + g.initializing
    }

    /// Notify every session subscribed to `uri`. Runs on the push thread's
    /// tokio handle. Peers that error (disconnected) are left for the serve
    /// task to unregister.
    pub fn broadcast(&self, uri: &str, rt: &tokio::runtime::Handle) {
        let targets: Vec<Peer<RoleServer>> = {
            let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.sessions
                .iter()
                .filter(|s| {
                    s.sub
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .is_subscribed(uri)
                })
                .map(|s| s.peer.clone())
                .collect()
        };
        for peer in targets {
            let params = ResourceUpdatedNotificationParam::new(uri.to_owned());
            let _ = rt.block_on(peer.notify_resource_updated(params));
        }
    }
}

/// Pure selection used by tests and `broadcast`: ids whose subscription matches.
#[must_use]
pub fn select_subscribed(subs: &[(u64, Arc<Mutex<SubscriptionRegistry>>)], uri: &str) -> Vec<u64> {
    subs.iter()
        .filter(|(_, s)| {
            s.lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_subscribed(uri)
        })
        .map(|(id, _)| *id)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::adapters::daemon::session::{SessionRegistry, select_subscribed};
    use crate::adapters::mcp::lsp_tools::SubscriptionRegistry;

    fn sub_for(uri: &str) -> Arc<Mutex<SubscriptionRegistry>> {
        let mut r = SubscriptionRegistry::new();
        r.subscribe(uri);
        Arc::new(Mutex::new(r))
    }

    #[test]
    fn select_returns_only_subscribed_sessions() {
        let subs = vec![
            (1u64, sub_for("file:///a.rs")),
            (2u64, sub_for("file:///b.rs")),
            (3u64, Arc::new(Mutex::new(SubscriptionRegistry::new()))), // subscribed to nothing
        ];
        let hit = select_subscribed(&subs, "file:///b.rs");
        assert_eq!(hit, vec![2]);
        let none = select_subscribed(&subs, "file:///z.rs");
        assert!(none.is_empty());
    }

    #[test]
    fn initializing_mcp_connection_counts_as_keep_alive_until_registered_or_closed() {
        let registry = SessionRegistry::new();
        assert_eq!(registry.keep_alive_count(), 0);

        registry.register_initializing();
        assert_eq!(registry.keep_alive_count(), 1);

        registry.unregister_initializing();
        assert_eq!(registry.keep_alive_count(), 0);
    }
}
