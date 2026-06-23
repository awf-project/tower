//! The daemon: owns the engine, accepts socket connections, serves one rmcp
//! session per `mcp` connection over shared state, answers `control` requests,
//! and self-terminates after the configured idle timeout.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use rmcp::serve_server;
use tokio::net::UnixStream;
use tokio::sync::Notify;

use crate::adapters::cli::GlobalOpts;
use crate::adapters::config::TowerConfig;
use crate::adapters::daemon::engine::build_engine;
use crate::adapters::daemon::session::SessionRegistry;
use crate::adapters::daemon::socket::{bind_listener, socket_path};
use crate::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, StatusSnapshot, read_handshake, read_line_capped,
    write_line,
};
use crate::adapters::mcp::diagnostics::DiagnosticsReader;
use crate::adapters::mcp::extension_merged_registry::ExtensionMergedRegistry;
use crate::adapters::mcp::lsp_tools::SubscriptionRegistry;
use crate::adapters::mcp::native_tools::EngineState;
use crate::adapters::mcp::rmcp_server::TowerMcpHandler;
use crate::domain::extension_host::ExtensionRegistry;

/// Shared context for every accepted connection.
struct DaemonCtx {
    started: Instant,
    registry: Arc<SessionRegistry>,
    state: Arc<RwLock<EngineState>>,
    ext_registry: Arc<RwLock<ExtensionRegistry>>,
    diag_reader: Arc<dyn DiagnosticsReader>,
    shutdown: Arc<Notify>,
}

impl DaemonCtx {
    fn snapshot(&self) -> StatusSnapshot {
        let indexed_files = self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .workspace_arc()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .all_file_ids()
            .len();
        let extensions = self
            .ext_registry
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .declared_tools()
            .into_iter()
            .map(|(_, decl)| decl.name)
            .collect();
        StatusSnapshot {
            uptime_secs: self.started.elapsed().as_secs(),
            mcp_clients: self.registry.keep_alive_count(),
            indexed_files,
            extensions,
        }
    }
}

/// Build a fresh per-connection MCP handler over the shared engine state.
fn build_handler(ctx: &DaemonCtx) -> (TowerMcpHandler, Arc<Mutex<SubscriptionRegistry>>) {
    let sub = Arc::new(Mutex::new(SubscriptionRegistry::new()));
    let registry =
        ExtensionMergedRegistry::new(Arc::clone(&ctx.state), Arc::clone(&ctx.ext_registry));
    let handler = TowerMcpHandler::new(
        registry,
        Arc::clone(&ctx.diag_reader),
        Arc::clone(&sub),
        Vec::new(),
    );
    (handler, sub)
}

/// Serve one accepted connection to completion.
async fn serve_connection(stream: UnixStream, ctx: Arc<DaemonCtx>) {
    let (mut read_half, mut write_half) = stream.into_split();
    let hs = match read_handshake(&mut read_half).await {
        Ok(hs) => hs,
        Err(_) => return, // bad handshake: drop the connection silently.
    };
    match hs.role {
        ClientRole::Mcp => {
            let (handler, sub) = build_handler(&ctx);
            let running = match serve_server(handler, (read_half, write_half)).await {
                Ok(r) => r,
                Err(_) => return,
            };
            let id = ctx.registry.register(running.peer().clone(), sub);
            let _ = running.waiting().await;
            ctx.registry.unregister(id);
        }
        ClientRole::Observer => {
            let body = serde_json::to_string(&ControlResponse::Unsupported).unwrap();
            let _ = write_line(&mut write_half, &body).await;
        }
        ClientRole::Control => {
            let line = match read_line_capped(&mut read_half, 4096).await {
                Ok(Some(l)) => l,
                _ => return,
            };
            let req: ControlRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => return,
            };
            match req {
                ControlRequest::Status => {
                    let body =
                        serde_json::to_string(&ControlResponse::Status(ctx.snapshot())).unwrap();
                    let _ = write_line(&mut write_half, &body).await;
                }
                ControlRequest::Shutdown => {
                    let body = serde_json::to_string(&ControlResponse::Ok).unwrap();
                    let _ = write_line(&mut write_half, &body).await;
                    ctx.shutdown.notify_waiters();
                }
            }
        }
    }
}

/// Idle monitor: when keep-alive count is 0 for `idle`, trigger shutdown.
async fn idle_monitor(
    registry: Arc<SessionRegistry>,
    shutdown: Arc<Notify>,
    idle: std::time::Duration,
) {
    loop {
        if registry.keep_alive_count() == 0 {
            tokio::select! {
                () = tokio::time::sleep(idle) => {
                    if registry.keep_alive_count() == 0 {
                        shutdown.notify_waiters();
                        return;
                    }
                }
                () = registry.count_changed.notified() => {}
            }
        } else {
            registry.count_changed.notified().await;
        }
    }
}

/// Run the daemon to completion (blocks the calling thread on its own runtime).
pub fn run_daemon(opts: &GlobalOpts, config: TowerConfig, detach: bool) -> Result<(), String> {
    if detach {
        // Best-effort: detach from the controlling terminal/session so the
        // daemon survives the spawning client. Foreground `tower daemon`
        // (no --detach) stays attached for systemd/debugging.
        let _ = rustix::process::setsid();
    }

    let workspace_root = crate::adapters::cli::resolve_workspace_root(opts);
    let idle = config.daemon.idle_timeout();
    let handle = build_engine(opts, config)?;

    let sock = socket_path(&workspace_root);

    // Build the runtime first so that `UnixListener::bind` (a tokio type that
    // requires a reactor) runs inside a runtime context.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("daemon-runtime")
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    // Bind the listener inside the runtime context.
    let listener = rt.block_on(async { bind_listener(&sock) }).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            "another daemon already owns this workspace socket".to_string()
        } else {
            format!("failed to bind daemon socket {}: {e}", sock.display())
        }
    })?;

    let registry = SessionRegistry::new();
    let shutdown = Arc::new(Notify::new());
    let ctx = Arc::new(DaemonCtx {
        started: Instant::now(),
        registry: Arc::clone(&registry),
        state: Arc::clone(&handle.state),
        ext_registry: Arc::clone(&handle.ext_registry),
        diag_reader: Arc::clone(&handle.diag_reader),
        shutdown: Arc::clone(&shutdown),
    });

    // Push fan-out: drain the single push_rx, broadcast to subscribed sessions.
    {
        let registry = Arc::clone(&registry);
        let rt_handle = rt.handle().clone();
        let push_rx = handle.push_rx;
        std::thread::Builder::new()
            .name("daemon-push-fanout".to_owned())
            .spawn(move || {
                while let Ok(event) = push_rx.recv() {
                    registry.broadcast(&event.uri, &rt_handle);
                }
            })
            .map_err(|e| format!("failed to spawn push fan-out thread: {e}"))?;
    }

    let result = rt.block_on(async {
        tokio::spawn(idle_monitor(
            Arc::clone(&registry),
            Arc::clone(&shutdown),
            idle,
        ));
        eprintln!("tower: daemon listening on {}", sock.display());
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let ctx = Arc::clone(&ctx);
                            tokio::spawn(serve_connection(stream, ctx));
                        }
                        Err(e) => return Err(format!("accept error: {e}")),
                    }
                }
                () = shutdown.notified() => {
                    eprintln!("tower: daemon shutting down");
                    return Ok(());
                }
            }
        }
    });

    // Best-effort socket cleanup on the way out.
    let _ = std::fs::remove_file(&sock);
    result
}
