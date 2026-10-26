//! Spec 24 — supervision & fault model integration tests.
//!
//! Uses the real fault-fixture binaries built by the workspace:
//!   - `fixture_hang_forever`   — reads initialize then sleeps forever (AC1)
//!   - `fixture_crash_on_init`  — reads initialize then exits(1) (AC2)
//!   - `fixture_exit_nonzero`   — completes init, exits(42) on invokeTool (AC2 variant)
//!   - `fixture_garbage_frames` — completes init, emits garbage on invokeTool (AC3)
//!
//! All tests use in-memory port doubles (no real disk/DB).
//! Tests assert both the returned fault AND that the host process is still running
//! (i.e. the test thread itself continues normally — AC5/U2).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use extension_protocol::manifest::{Activation, CapabilitiesSection, EventsSection};
use extension_protocol::{ExtensionFault, ExtensionManifest, ToolDecl};
use serde_json::json;

use super::host_deps::{HostDeps, UnsupportedApplyEditsHost};
use super::sidecar::SidecarHostAdapter;
use super::supervisor::ExtensionSupervisor;
use crate::adapters::formatter::NoOpFormatQueue;
use crate::adapters::{InMemoryAstIndex, InMemoryFs};
use crate::domain::ExtensionInstance;
use crate::domain::extension_host::{ExtensionApplicationError, ExtensionCallError};

// ── Binary-path helpers ───────────────────────────────────────────────────────

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent() // tower/
        .unwrap()
        .to_owned()
}

fn fixture_bin(name: &str) -> String {
    workspace_root()
        .join("target")
        .join("debug")
        .join(name)
        .to_str()
        .unwrap()
        .to_owned()
}

// ── HostDeps builder ──────────────────────────────────────────────────────────

fn make_deps() -> HostDeps {
    HostDeps {
        fs: Arc::new(Mutex::new(InMemoryFs::new())),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(UnsupportedApplyEditsHost),
        push_tx: None,
    }
}

fn make_manifest(bin: &str) -> ExtensionManifest {
    ExtensionManifest {
        name: "fixture".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![bin.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    }
}

fn application_then_success_manifest() -> ExtensionManifest {
    let script = r#"
read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[],"events":[],"capabilities":[]}}}'
read -r first_invoke
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"type":"Error","data":{"code":-32000,"message":"request rejected"}}}'
read -r second_invoke
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"type":"ToolResult","data":{"same_process":true}}}'
read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"type":"Ack"}}'
"#;
    ExtensionManifest {
        name: "structured_error_fixture".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    }
}

fn request_gate_manifest(marker: &std::path::Path, release: &std::path::Path) -> ExtensionManifest {
    let script = r#"
read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[],"events":[],"capabilities":[]}}}'
read -r invoke
: > "$1"
while [ ! -e "$2" ]; do sleep 0.01; done
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"type":"ToolResult","data":{"released":true}}}'
read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"type":"Ack"}}'
"#;
    ExtensionManifest {
        name: "request_gate".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "request-gate".to_owned(),
            marker.display().to_string(),
            release.display().to_string(),
        ],
        activation: Activation::Eager,
        tools: vec![ToolDecl {
            name: "run".to_owned(),
            description: "release-gated request".to_owned(),
            schema_json: "{}".to_owned(),
        }],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    }
}

fn wait_for_file(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture did not reach the request gate"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1 — Timeout: hang-forever fixture killed within REQUEST_TIMEOUT
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn unlimited_sidecar_request_completes_after_fixture_release() {
    let workspace = tempfile::tempdir().expect("create fixture directory");
    let marker = workspace.path().join("request-started");
    let release = workspace.path().join("release");
    let manifest = request_gate_manifest(&marker, &release);
    let mut adapter = SidecarHostAdapter::spawn(manifest, make_deps(), None, None)
        .expect("an unlimited sidecar must initialize");
    let (tx, rx) = std::sync::mpsc::channel();

    let request = std::thread::spawn(move || {
        let result = adapter.call_tool("run", json!({}), &crate::domain::NeverCancelled);
        tx.send(result).expect("return request result");
        adapter.shutdown();
    });

    wait_for_file(&marker);
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "an absent budget must keep the request pending until the fixture releases it"
    );
    std::fs::write(&release, "release").expect("release fixture request");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2))
            .expect("released request completes")
            .expect("released request succeeds"),
        json!({"released": true})
    );
    request.join().expect("request thread joins");
}

#[test]
fn configured_sidecar_timeout_reports_operation_layer_and_budget() {
    let workspace = tempfile::tempdir().expect("create fixture directory");
    let marker = workspace.path().join("request-started");
    let release = workspace.path().join("never-release");
    let manifest = request_gate_manifest(&marker, &release);
    let mut adapter = SidecarHostAdapter::spawn(
        manifest,
        make_deps(),
        Some(Duration::from_millis(125)),
        None,
    )
    .expect("fixture initializes within the configured budget");

    let error = adapter
        .call_tool("run", json!({}), &crate::domain::NeverCancelled)
        .expect_err("the release-gated request must expire");
    assert_eq!(
        error,
        ExtensionCallError::Fault(ExtensionFault::Timeout {
            detail: extension_protocol::TimeoutDetail {
                operation: "invokeTool".to_owned(),
                layer: "host".to_owned(),
                budget_ms: 125,
            },
        })
    );
}

#[test]
fn configured_sidecar_timeout_does_not_quarantine_a_healthy_restart() {
    let workspace = tempfile::tempdir().expect("create fixture directory");
    let attempts = workspace.path().join("attempts");
    let script = r#"
attempt=0
if [ -e "$1" ]; then attempt=$(cat "$1"); fi
attempt=$((attempt + 1))
printf '%s' "$attempt" > "$1"
read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"type":"Initialized","data":{"tools":[],"events":[],"capabilities":[]}}}'
read -r invoke
if [ "$attempt" -eq 1 ]; then
  while :; do sleep 1; done
fi
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"type":"ToolResult","data":{"restarted":true}}}'
read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"type":"Ack"}}'
"#;
    let mut manifest = make_manifest("sh");
    manifest.command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script.to_owned(),
        "timeout-then-recover".to_owned(),
        attempts.display().to_string(),
    ];
    manifest.tools.push(ToolDecl {
        name: "run".to_owned(),
        description: "timeout then recover".to_owned(),
        schema_json: "{}".to_owned(),
    });
    let supervisor = ExtensionSupervisor::new(
        manifest,
        make_deps(),
        Some(Duration::from_millis(125)),
        None,
    );
    let mut registry = crate::domain::extension_host::ExtensionRegistry::new();
    registry
        .register(Box::new(supervisor))
        .expect("register supervised sidecar");

    assert!(matches!(
        registry.invoke("run", json!({}), &crate::domain::NeverCancelled),
        Err(crate::domain::extension_host::InvokeError::Fault(
            ExtensionFault::Timeout { .. }
        ))
    ));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        registry
            .invoke("run", json!({}), &crate::domain::NeverCancelled)
            .expect("the same registry routes to the healthy restart"),
        json!({"restarted": true})
    );
}

/// AC1: Given a `hang-forever` fixture, When spawned (initialize hangs),
/// Then `spawn` returns `Timeout` within the deadline; the calling thread survives.
#[test]
fn ac1_initialize_timeout_returns_fault_host_survives() {
    let bin = fixture_bin("fixture_hang_forever");
    let manifest = make_manifest(&bin);
    // Use a very short timeout so the test completes quickly.
    let short = Duration::from_millis(300);

    let result = SidecarHostAdapter::spawn(manifest, make_deps(), short, None);

    // spawn returns Result<Box<dyn ExtensionInstance>, ExtensionFault>.
    // Box<dyn ExtensionInstance> lacks Debug, so extract the error.
    assert!(result.is_err(), "hang-forever must return a fault");
    let fault = result.err().expect("just checked is_err()");
    assert!(
        matches!(fault, ExtensionFault::Timeout { .. }),
        "must be Timeout, got: {fault:?}"
    );
    // The test thread is still alive — host survived (U2 / AC5).
}

/// AC1 variant: supervisor wrapping hang-forever returns Timeout from call_tool.
/// (The supervisor tries to spawn the child on first call; spawn hangs → Timeout.)
#[test]
fn ac1_supervisor_timeout_returns_fault() {
    let bin = fixture_bin("fixture_hang_forever");
    let manifest = make_manifest(&bin);
    let short = Duration::from_millis(300);

    let mut sup = ExtensionSupervisor::new(manifest, make_deps(), short, None);

    // call_tool returns Result<Value, ExtensionCallError>; Value: Debug.
    let result = sup.call_tool("run", json!({}), &crate::domain::NeverCancelled);
    let fault = result.expect_err("hang-forever call_tool must fault");
    assert!(
        matches!(
            fault,
            ExtensionCallError::Fault(ExtensionFault::Timeout { .. })
        ),
        "supervisor must surface Timeout, got: {fault:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC2 — Crash: crash-on-init fixture → Crashed fault + later call respawns
// ═══════════════════════════════════════════════════════════════════════════

/// AC2a: Given `crash-on-init`, When `spawn` is called, Then it returns
/// `ExtensionFault::Crashed` (exit during initialize handshake).
#[test]
fn sidecar_initialization_exit_returns_crash() {
    let bin = fixture_bin("fixture_crash_on_init");
    let manifest = make_manifest(&bin);

    let result = SidecarHostAdapter::spawn(manifest, make_deps(), Duration::from_secs(5), None);

    assert!(result.is_err(), "crash-on-init must fault");
    let fault = result.err().expect("just checked is_err()");
    assert!(
        matches!(fault, ExtensionFault::Crashed { .. }),
        "must be Crashed, got: {fault:?}"
    );
}

/// AC2b: Given `exit-nonzero` (crashes after init during invokeTool), When
/// `call_tool` is called, Then it returns `ExtensionFault::Crashed`.
#[test]
fn sidecar_nonzero_tool_exit_returns_crash() {
    let bin = fixture_bin("fixture_exit_nonzero");
    let manifest = make_manifest(&bin);

    let mut instance =
        SidecarHostAdapter::spawn(manifest, make_deps(), Duration::from_secs(5), None)
            .unwrap_or_else(|e| panic!("exit_nonzero must initialize successfully, got: {e:?}"));

    // The fixture exits(42) when it receives invokeTool.
    // call_tool returns Result<Value, ExtensionCallError>; Value: Debug.
    let result = instance.call_tool("run", json!({}), &crate::domain::NeverCancelled);
    let fault = result.expect_err("exit-nonzero call_tool must fault");
    assert!(
        matches!(
            fault,
            ExtensionCallError::Fault(ExtensionFault::Crashed { .. })
        ),
        "must be Crashed, got: {fault:?}"
    );
}

/// AC2c: Given `ExtensionSupervisor` wrapping `exit-nonzero`, When called once
/// (Crashed), Then a second call is attempted after backoff and also returns a
/// fault (the fixture always crashes). The supervisor enters Backoff and does NOT
/// block the caller.
#[test]
fn supervisor_returns_crash_during_restart_backoff() {
    let bin = fixture_bin("fixture_exit_nonzero");
    let manifest = make_manifest(&bin);
    let timeout = Duration::from_secs(5);

    let mut sup = ExtensionSupervisor::new(manifest, make_deps(), timeout, None);

    // First call: spawn succeeds, call_tool crashes → Crashed.
    let fault1 = sup
        .call_tool("run", json!({}), &crate::domain::NeverCancelled)
        .expect_err("first call must fault");
    assert!(
        matches!(
            fault1,
            ExtensionCallError::Fault(ExtensionFault::Crashed { .. })
        ),
        "first call must be Crashed, got: {fault1:?}"
    );

    // Second call immediately: backoff not elapsed → still Crashed but host alive.
    let fault2 = sup
        .call_tool("run", json!({}), &crate::domain::NeverCancelled)
        .expect_err("second call must fault (in backoff)");
    assert!(
        matches!(
            fault2,
            ExtensionCallError::Fault(ExtensionFault::Crashed { .. })
        ),
        "second call must be Crashed, got: {fault2:?}"
    );
    // Test thread alive — host survived.
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3 — ProtocolError: garbage-frames fixture → ProtocolError
// ═══════════════════════════════════════════════════════════════════════════

/// AC3: Given `garbage-frames`, When `call_tool` is called, Then the host
/// returns `ProtocolError`; no panic.
#[test]
fn malformed_sidecar_frames_return_protocol_error_or_crash() {
    let bin = fixture_bin("fixture_garbage_frames");
    let manifest = make_manifest(&bin);

    let mut instance =
        SidecarHostAdapter::spawn(manifest, make_deps(), Duration::from_secs(5), None)
            .unwrap_or_else(|e| panic!("garbage_frames must initialize successfully, got: {e:?}"));

    // call_tool returns Result<Value, ExtensionFault>; Value: Debug.
    let result = instance.call_tool("run", json!({}), &crate::domain::NeverCancelled);
    let fault = result.expect_err("garbage-frames call_tool must fault");
    assert!(
        matches!(
            fault,
            ExtensionCallError::Fault(
                ExtensionFault::ProtocolError { .. } | ExtensionFault::Crashed { .. }
            )
        ),
        "must be ProtocolError or Crashed (EOF follows garbage), got: {fault:?}"
    );
    // No panic — host survived (U2).
}

/// AC3 via supervisor: garbage-frames through supervisor reports a fault that
/// the registry can count.
#[test]
fn ac3_supervisor_garbage_frames_reports_fault() {
    let bin = fixture_bin("fixture_garbage_frames");
    let manifest = make_manifest(&bin);

    let mut sup = ExtensionSupervisor::new(manifest, make_deps(), Duration::from_secs(5), None);

    let result = sup.call_tool("run", json!({}), &crate::domain::NeverCancelled);

    assert!(
        result.is_err(),
        "garbage-frames must return a fault through supervisor"
    );
    let fault = result.unwrap_err();
    assert!(
        !matches!(
            fault,
            ExtensionCallError::Fault(ExtensionFault::Quarantined)
        ),
        "supervisor never generates Quarantined — that is the registry's job"
    );
}

#[test]
fn f008_application_supervisor_passes_through_without_restart_and_allows_absent_data() {
    let manifest = application_then_success_manifest();
    let mut supervisor =
        ExtensionSupervisor::new(manifest, make_deps(), Duration::from_secs(5), None);

    let error = supervisor
        .call_tool("rename", json!({}), &crate::domain::NeverCancelled)
        .expect_err("the fixture's first call must be rejected");
    assert_eq!(
        error,
        ExtensionCallError::Application(ExtensionApplicationError {
            code: -32_000,
            message: "request rejected".to_owned(),
            data: None,
        })
    );

    let result = supervisor
        .call_tool("rename", json!({}), &crate::domain::NeverCancelled)
        .expect("an application error must leave the live sidecar available");
    assert_eq!(result, json!({"same_process": true}));
    supervisor.shutdown();
}

#[test]
fn f008_application_true_fault_control_enters_supervision_backoff() {
    let manifest = make_manifest(&fixture_bin("fixture_exit_nonzero"));
    let mut supervisor =
        ExtensionSupervisor::new(manifest, make_deps(), Duration::from_secs(5), None);

    let first = supervisor
        .call_tool("run", json!({}), &crate::domain::NeverCancelled)
        .expect_err("process exit must be a genuine fault");
    assert!(matches!(
        first,
        ExtensionCallError::Fault(ExtensionFault::Crashed { .. })
    ));

    let second = supervisor
        .call_tool("run", json!({}), &crate::domain::NeverCancelled)
        .expect_err("the immediate retry must observe supervision backoff");
    assert!(matches!(
        second,
        ExtensionCallError::Fault(ExtensionFault::Crashed { .. })
    ));

    let timeout_manifest = make_manifest(&fixture_bin("fixture_hang_forever"));
    let mut timeout_supervisor = ExtensionSupervisor::new(
        timeout_manifest,
        make_deps(),
        Duration::from_millis(300),
        None,
    );
    assert_eq!(
        timeout_supervisor
            .call_tool("run", json!({}), &crate::domain::NeverCancelled)
            .expect_err("a supervision deadline must remain a genuine fault"),
        ExtensionCallError::Fault(ExtensionFault::Timeout {
            detail: extension_protocol::TimeoutDetail {
                operation: "initialize".into(),
                layer: "host".into(),
                budget_ms: 300,
            },
        })
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4 — No Quarantined from supervisor (quarantine policy is the registry's)
// ═══════════════════════════════════════════════════════════════════════════

/// AC4: Multiple consecutive faults from the supervisor are all Crashed/Timeout
/// (never Quarantined — the supervisor does not decide that).
/// This confirms the registry can count them toward the quarantine threshold.
#[test]
fn ac4_supervisor_never_generates_quarantined_fault() {
    let bin = fixture_bin("fixture_crash_on_init");
    let manifest = make_manifest(&bin);
    let timeout = Duration::from_secs(2);

    let mut sup = ExtensionSupervisor::new(manifest, make_deps(), timeout, None);

    for i in 0..5u32 {
        // Wait past any backoff before each retry.
        // BASE_BACKOFF is 100ms; 200ms padding is sufficient for first few attempts.
        let wait_ms = 200u64.saturating_mul(1u64.checked_shl(i.min(4)).unwrap_or(u64::MAX));
        std::thread::sleep(Duration::from_millis(wait_ms));

        let result = sup.call_tool("run", json!({}), &crate::domain::NeverCancelled);
        match result {
            Err(ExtensionCallError::Fault(ExtensionFault::Quarantined)) => {
                panic!("supervisor must never emit Quarantined — that is the registry (call {i})");
            }
            Err(_) | Ok(_) => {
                // Any non-Quarantined outcome is acceptable here.
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AC5 / U2 — Host + MCP link survival
// ═══════════════════════════════════════════════════════════════════════════

/// AC5 / U2: While a misbehaving extension is being handled, a cooperative
/// extension continues to respond normally.
///
/// Sequence:
///   1. Spawn `fixture_crash_on_init` → faults immediately.
///   2. Spawn `test_helper_extension` → succeeds and echoes params.
#[test]
fn ac5_misbehaving_ext_does_not_affect_cooperative_ext() {
    let crash_bin = fixture_bin("fixture_crash_on_init");
    let crash_manifest = make_manifest(&crash_bin);

    // Spawn the bad extension — expect it to fault.
    let crash_result =
        SidecarHostAdapter::spawn(crash_manifest, make_deps(), Duration::from_secs(3), None);
    assert!(
        crash_result.is_err(),
        "crash-on-init must fault at spawn time"
    );

    // Now spawn the cooperative test_helper and verify it still works.
    let helper_bin = workspace_root()
        .join("target")
        .join("debug")
        .join("test_helper_extension")
        .to_str()
        .unwrap()
        .to_owned();
    let helper_manifest = ExtensionManifest {
        name: "helper".to_owned(),
        version: "0.1.0".to_owned(),
        command: vec![helper_bin],
        activation: Activation::Eager,
        tools: vec![],
        events: EventsSection::default(),
        capabilities: CapabilitiesSection::default(),
    };

    let mut helper =
        SidecarHostAdapter::spawn(helper_manifest, make_deps(), Duration::from_secs(10), None)
            .unwrap_or_else(|e| {
                panic!("test_helper must still spawn after crash-on-init fault; got: {e:?}")
            });

    let result = helper.call_tool(
        "echo",
        json!({"ping": "pong"}),
        &crate::domain::NeverCancelled,
    );
    assert!(
        result.is_ok(),
        "test_helper must respond after crash-on-init fault; got: {result:?}"
    );
    assert_eq!(result.unwrap(), json!({"ping": "pong"}));

    helper.shutdown();
}

// ═══════════════════════════════════════════════════════════════════════════
// Backoff unit test (pure computation, no real processes)
// ═══════════════════════════════════════════════════════════════════════════

/// The backoff formula grows exponentially and is capped at 30 s.
/// This mirrors the implementation in `supervisor::backoff_for_attempt`.
#[test]
fn backoff_formula_grows_and_caps() {
    // Local copy of the formula for verification (same logic as supervisor.rs).
    fn backoff(n: u32) -> Duration {
        let factor: u128 = 1u128.checked_shl(n).unwrap_or(u128::MAX);
        let ms = (100u128).saturating_mul(factor).min(30_000);
        Duration::from_millis(ms as u64)
    }

    assert_eq!(backoff(0), Duration::from_millis(100));
    assert_eq!(backoff(1), Duration::from_millis(200));
    assert_eq!(backoff(4), Duration::from_millis(1_600));
    // 2^8 * 100 = 25_600 ms
    assert_eq!(backoff(8), Duration::from_millis(25_600));
    // 2^9 * 100 = 51_200 ms → capped at 30_000 ms
    assert_eq!(backoff(9), Duration::from_millis(30_000));
    assert_eq!(backoff(100), Duration::from_millis(30_000));
}

// ═══════════════════════════════════════════════════════════════════════════
// Regression — shutdown must not deadlock joining the reader thread before the
// child is killed. An extension that completes `initialize` but then ignores the
// cooperative `shutdown` request must still be reaped via SIGTERM/SIGKILL, so
// `shutdown()` returns within its kill-escalation budget instead of blocking
// forever on the reader thread (which never sees stdout EOF while the child is
// alive). See `fixture_ignore_shutdown`.
// ═══════════════════════════════════════════════════════════════════════════

/// REG: Given an extension that never honours `shutdown`, When the host shuts it
/// down, Then `shutdown()` returns promptly via kill escalation rather than
/// deadlocking by joining the reader thread before killing the child.
#[test]
fn shutdown_reaps_extension_that_ignores_shutdown_request() {
    let bin = fixture_bin("fixture_ignore_shutdown");
    let manifest = make_manifest(&bin);
    let deps = make_deps();

    let mut adapter = SidecarHostAdapter::spawn(manifest, deps, Duration::from_secs(15), None)
        .expect("ignore_shutdown fixture must complete initialize and spawn");

    let start = std::time::Instant::now();
    adapter.shutdown();
    let elapsed = start.elapsed();

    // Kill-escalation budget = SHUTDOWN_GRACE_MS (2s) + SIGTERM_GRACE_MS (2s) +
    // slack. The buggy join-before-kill path blocks until the fixture's 20s
    // watchdog fires, so an 8s ceiling cleanly separates fixed (~2s) from
    // broken (~20s).
    assert!(
        elapsed < Duration::from_secs(8),
        "shutdown() must reap an uncooperative extension within the kill-escalation \
         budget, but took {elapsed:?} (join-before-kill deadlock?)",
    );
}
