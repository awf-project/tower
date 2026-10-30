//! The daemon: owns the engine, accepts socket connections, serves one rmcp
//! session per `mcp` connection over shared state, answers `control` requests,
//! and self-terminates after the configured idle timeout.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::serve_server;
use tokio::net::UnixStream;
use tokio::sync::Notify;

use crate::adapters::cli::GlobalOpts;
use crate::adapters::config::TowerConfig;
use crate::adapters::daemon::DaemonLifecycle;
use crate::adapters::daemon::client::ExternalDaemonClient;
use crate::adapters::daemon::engine::{
    EarlyDaemonContext, build_early_context, start_workspace_preparation,
};
use crate::adapters::daemon::session::SessionRegistry;
use crate::adapters::daemon::socket::{WorkspaceDaemonOwner, bind_listener, socket_path};
use crate::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, StatusSnapshot, read_handshake, read_line_capped,
    write_line,
};
use crate::adapters::mcp::ProjectQueryRegistry;
use crate::adapters::mcp::chain_registry::ChainRegistry;
use crate::adapters::mcp::diagnostics::NoOpDiagnosticsReader;
use crate::adapters::mcp::extension_merged_registry::ReadinessAwareRegistry;
use crate::adapters::mcp::lsp_tools::SubscriptionRegistry;
use crate::adapters::mcp::rmcp_server::TowerMcpHandler;

/// A sticky broadcast: shutdown cannot be lost between select iterations or
/// before a newly accepted connection starts waiting.
#[derive(Default)]
struct ShutdownSignal {
    requested: std::sync::atomic::AtomicBool,
    changed: Notify,
}
impl ShutdownSignal {
    fn request(&self) {
        self.requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }
    async fn wait(&self) {
        let changed = self.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !self.requested.load(std::sync::atomic::Ordering::Acquire) {
            changed.await;
        }
    }
}

/// Shared context for every accepted connection.
struct DaemonCtx {
    generation: u64,
    started: Instant,
    registry: Arc<SessionRegistry>,
    early: Arc<EarlyDaemonContext>,
    shutdown: Arc<ShutdownSignal>,
}

impl DaemonCtx {
    fn snapshot(&self) -> StatusSnapshot {
        let prepared = self.early.wait_prepared(Some(Duration::ZERO)).ok();
        let indexed_files = prepared
            .as_ref()
            .map(|engine| {
                engine
                    .state()
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .workspace_arc()
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .all_file_ids()
                    .len()
            })
            .unwrap_or(0);
        let extensions = prepared
            .as_ref()
            .map(|engine| {
                engine
                    .extension_registry()
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .declared_tools()
                    .into_iter()
                    .map(|(_, decl)| decl.name)
                    .collect()
            })
            .unwrap_or_default();
        let readiness = self.early.readiness().snapshot();
        StatusSnapshot {
            uptime_secs: self.started.elapsed().as_secs(),
            mcp_clients: self.registry.keep_alive_count(),
            indexed_files,
            extensions,
            instrumentation: self.early.instrumentation_snapshot(),
            generation: self.generation,
            lifecycle: readiness.lifecycle,
            workspace: readiness.workspace,
            ast: readiness.ast,
            lsp: readiness.lsp,
        }
    }
}

/// Build a fresh per-connection MCP handler over the shared engine state.
fn build_handler(ctx: &DaemonCtx) -> (TowerMcpHandler, Arc<Mutex<SubscriptionRegistry>>) {
    let sub = Arc::new(Mutex::new(SubscriptionRegistry::new()));
    let registry = ChainRegistry::new(vec![
        Box::new(ProjectQueryRegistry::new(
            ctx.early.workspace_root().to_path_buf(),
            ctx.early.queryable_projects().clone(),
            ctx.early.external_availability_timeout(),
            Arc::new(ExternalDaemonClient::new()),
        )),
        Box::new(ReadinessAwareRegistry::new(Arc::clone(&ctx.early))),
    ]);
    let handler = TowerMcpHandler::new(
        registry,
        Arc::new(NoOpDiagnosticsReader),
        Arc::clone(&sub),
        Vec::new(),
    );
    (handler, sub)
}

async fn connection_shutdown(ctx: &DaemonCtx) {
    ctx.shutdown.wait().await;
}

/// Serve one accepted connection to completion.
async fn serve_connection(stream: UnixStream, ctx: Arc<DaemonCtx>) {
    let (mut read_half, mut write_half) = stream.into_split();
    let handshake = tokio::select! {
        result = read_handshake(&mut read_half) => result,
        () = connection_shutdown(&ctx) => return,
    };
    let hs = match handshake {
        Ok(hs) => hs,
        Err(_) => return, // bad handshake: drop the connection silently.
    };
    match hs.role {
        ClientRole::Mcp => {
            let (handler, sub) = build_handler(&ctx);
            ctx.registry.register_initializing();
            let initialized = tokio::select! {
                result = serve_server(handler, (read_half, write_half)) => result,
                () = connection_shutdown(&ctx) => { ctx.registry.unregister_initializing(); return; },
            };
            let running = match initialized {
                Ok(r) => r,
                Err(_) => {
                    ctx.registry.unregister_initializing();
                    return;
                }
            };
            let id = ctx
                .registry
                .register_initialized(running.peer().clone(), sub);
            let cancellation = running.cancellation_token();
            let waiting = running.waiting();
            tokio::pin!(waiting);
            tokio::select! {
                _ = &mut waiting => {},
                () = connection_shutdown(&ctx) => {
                    cancellation.cancel();
                    let _ = waiting.await;
                }
            }
            ctx.registry.unregister(id);
        }
        ClientRole::Observer => {
            let body = serde_json::to_string(&ControlResponse::Unsupported).unwrap();
            let _ = write_line(&mut write_half, &body).await;
        }
        ClientRole::Control => {
            let request = tokio::select! {
                result = read_line_capped(&mut read_half, 4096) => result,
                () = connection_shutdown(&ctx) => return,
            };
            let line = match request {
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
                    ctx.shutdown.request();
                }
            }
        }
    }
}

/// Idle monitor: when keep-alive count is 0 for `idle`, trigger shutdown.
async fn idle_monitor(
    registry: Arc<SessionRegistry>,
    shutdown: Arc<ShutdownSignal>,
    idle: std::time::Duration,
) {
    loop {
        if registry.keep_alive_count() == 0 {
            tokio::select! {
                () = tokio::time::sleep(idle) => {
                    if registry.keep_alive_count() == 0 {
                        shutdown.request();
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
    let owner =
        WorkspaceDaemonOwner::acquire(&workspace_root).map_err(|error| error.to_string())?;
    pre_bind_test_milestone()?;
    let idle = config.daemon.idle_timeout();
    const FAILURE_CLEANUP_GRACE: Duration = Duration::from_secs(10);
    let failure_grace = config
        .daemon
        .connection_timeout()
        .unwrap_or(FAILURE_CLEANUP_GRACE);

    let sock = socket_path(&workspace_root);

    // Build the runtime first so that `UnixListener::bind` (a tokio type that
    // requires a reactor) runs inside a runtime context.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("daemon-runtime")
        .build()
        .map_err(|e| format!("failed to build tokio runtime: {e}"))?;

    // Bind the listener inside the runtime context.
    let listener = rt
        .block_on(async { bind_listener(&owner) })
        .map_err(|error| error.to_string())?;

    // Publish the transport while lifetime ownership is held, before opening
    // Sled or initializing the rest of the engine. This closes the startup
    // race without allowing a losing daemon to touch shared engine resources.
    let early = match build_early_context(opts, config, &owner) {
        Ok(context) => Arc::new(context),
        Err(error) => {
            drop(listener);
            let _ = std::fs::remove_file(&sock);
            return Err(error.to_string());
        }
    };
    let preparation = match start_workspace_preparation(Arc::clone(&early)) {
        Ok(handle) => handle,
        Err(error) => {
            drop(listener);
            let _ = std::fs::remove_file(&sock);
            return Err(error.to_string());
        }
    };
    #[cfg(feature = "startup-test-hooks")]
    if std::env::var("TOWER_TEST_FAIL_RECONCILIATION").as_deref() == Ok("before_commit")
        && std::env::var_os("TOWER_TEST_WORKSPACE")
            .and_then(|path| std::path::PathBuf::from(path).canonicalize().ok())
            == workspace_root.canonicalize().ok()
    {
        let outcome = preparation.join();
        early.release_prepared();
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        drop(owner);
        return match outcome {
            Ok(_) => Err(format!(
                "injected apply_reconciliation_delta failure at {}",
                workspace_root.display()
            )),
            Err(_) => Err(format!(
                "workspace reconciliation failed during apply_reconciliation_delta at {}",
                workspace_root.display()
            )),
        };
    }

    let registry = SessionRegistry::new();
    let shutdown = Arc::new(ShutdownSignal::default());
    let ctx = Arc::new(DaemonCtx {
        generation: owner.generation(),
        started: Instant::now(),
        registry: Arc::clone(&registry),
        early: Arc::clone(&early),
        shutdown: Arc::clone(&shutdown),
    });

    // The server owns this worker and its stop signal; no detached fan-out
    // can retain prepared resources beyond workspace ownership release.
    let fanout_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fanout = {
        let registry = Arc::clone(&registry);
        let rt_handle = rt.handle().clone();
        let early = Arc::clone(&early);
        let cancelled = Arc::clone(&fanout_cancelled);
        std::thread::Builder::new()
            .name("daemon-push-fanout".to_owned())
            .spawn(move || {
                let Ok(prepared) = early.wait_prepared(early.preparation_timeout()) else {
                    return;
                };
                while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    if let Some(push_rx) = prepared.take_push_receiver() {
                        while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            match push_rx.recv_timeout(Duration::from_millis(25)) {
                                Ok(event) => registry.broadcast(&event.uri, &rt_handle),
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                        return;
                    }
                    std::thread::park_timeout(Duration::from_millis(25));
                }
            })
            .map_err(|_| "failed to spawn push fan-out thread".to_owned())?
    };

    let result = rt.block_on(async {
        let idle_task = tokio::spawn(idle_monitor(
            Arc::clone(&registry),
            Arc::clone(&shutdown),
            idle,
        ));
        eprintln!("tower: daemon listening on {}", sock.display());
        let failure_shutdown = async {
            early.readiness().wait_for_workspace_failure().await;
            // Keep status available briefly, even with a connected client, then
            // run the same joined teardown as an explicit shutdown.
            tokio::time::sleep(failure_grace).await;
        };
        tokio::pin!(failure_shutdown);
        let preparation_expiry = async {
            let Some(timeout) = early.preparation_timeout() else {
                std::future::pending::<()>().await;
                return;
            };
            tokio::time::sleep(timeout).await;
            // The daemon is usable once the workspace engine is published;
            // extension-family catch-up may legitimately continue afterward
            // under its own supervision budget.
            if early.prepared_if_available().is_some() {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(preparation_expiry);
        let mut connections = tokio::task::JoinSet::new();
        let result = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let ctx = Arc::clone(&ctx);
                            connections.spawn(serve_connection(stream, ctx));
                        }
                        Err(e) => break Err(format!("accept error: {e}")),
                    }
                }
                () = &mut failure_shutdown => {
                    break Err("workspace preparation failed".to_owned());
                }
                () = &mut preparation_expiry => {
                    break Err("workspace preparation timed out".to_owned());
                }
                () = shutdown.wait() => {
                    eprintln!("tower: daemon shutting down");
                    break Ok(());
                }
                _ = connections.join_next(), if !connections.is_empty() => {},
            }
        };
        let _ = early
            .readiness()
            .transition_daemon(DaemonLifecycle::Stopping);
        preparation.cancel();
        shutdown.request();
        idle_task.abort();
        let _ = idle_task.await;
        while connections.join_next().await.is_some() {}
        result
    });

    let _ = early
        .readiness()
        .transition_daemon(DaemonLifecycle::Stopping);
    preparation.cancel();
    let _ = preparation.join();
    fanout_cancelled.store(true, std::sync::atomic::Ordering::Release);
    let _ = fanout.join();
    // Drop the runtime before engine/owner teardown: any synchronous MCP
    // dispatch still finishing on a runtime worker must release its permit.
    drop(rt);
    debug_assert!(early.readiness().wait_for_dispatch_drain(Duration::ZERO));
    early.release_prepared();
    drop(ctx);
    drop(listener);
    // Best-effort socket cleanup on the way out.
    let _ = std::fs::remove_file(&sock);
    drop(owner);
    #[cfg(feature = "startup-test-hooks")]
    if let Some(trace) = std::env::var_os("TOWER_TEST_CLEANUP_TRACE") {
        let cleanup = [
            "watcher",
            "sidecars",
            "push_fanout",
            "sled",
            "socket",
            "ownership",
        ];
        let _ = std::fs::write(trace, serde_json::to_vec(&cleanup).unwrap_or_default());
    }
    result
}

#[cfg(feature = "startup-test-hooks")]
fn pre_bind_test_milestone() -> Result<(), String> {
    use std::io::{BufRead as _, Write as _};
    use std::time::Duration;

    if std::env::var("TOWER_TEST_PRE_BIND_HANDSHAKE").as_deref() != Ok("stdio") {
        return Ok(());
    }
    println!("ownership-acquired");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("failed to flush pre-bind milestone: {error}"))?;
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pre-bind-test-milestone".to_owned())
        .spawn(move || {
            let mut command = String::new();
            let result = std::io::stdin()
                .lock()
                .read_line(&mut command)
                .map(|_| command);
            let _ = command_tx.send(result);
        })
        .map_err(|error| format!("failed to start pre-bind continuation reader: {error}"))?;
    let command = command_rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "timed out waiting for pre-bind continuation".to_owned())?
        .map_err(|error| format!("failed to read pre-bind continuation: {error}"))?;
    if command == "continue\n" {
        Ok(())
    } else {
        Err("invalid pre-bind continuation".to_owned())
    }
}

#[cfg(not(feature = "startup-test-hooks"))]
fn pre_bind_test_milestone() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod shutdown_signal_tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_before_wait_and_between_accept_iterations_is_not_lost() {
        let signal = Arc::new(ShutdownSignal::default());
        signal.request();
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), signal.wait())
                .await
                .unwrap();
        }
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let signal = Arc::clone(&signal);
            tasks.spawn(async move {
                signal.wait().await;
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    }
}
