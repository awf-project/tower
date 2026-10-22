//! Wire-protocol unit tests (Task 3).
#![forbid(unsafe_code)]

use core_engine::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, HandshakeError, PROTOCOL_VERSION,
    StatusSnapshot, read_handshake,
};
use core_engine::adapters::daemon::{CapabilityDependency, StartupFailureClass};
use core_engine::adapters::daemon::{CapabilityReadiness, DaemonLifecycle, StartupPhase};
use core_engine::adapters::mcp::types::{
    CapabilityInitializationFailed, CapabilityStopping, CapabilityUnavailable,
    InitializationPending, ToolError, capability_initialization_failed_error,
    capability_stopping_error, capability_unavailable_error, initialization_pending_error,
};
use serde_json::{Value, json};

fn assert_application_error(error: ToolError, expected: Value) {
    match error {
        ToolError::Application {
            code,
            message,
            data,
        } => assert_eq!(
            json!({"code": code, "message": message, "data": data}),
            expected
        ),
        other => panic!("expected application error, got {other:?}"),
    }
}

#[tokio::test]
async fn handshake_round_trips_over_a_reader() {
    let line = Handshake::new(ClientRole::Mcp).to_line();
    let mut bytes = line.into_bytes();
    bytes.extend_from_slice(b"left over rmcp bytes"); // must NOT be consumed
    let mut cursor = std::io::Cursor::new(bytes.clone());
    let hs = read_handshake(&mut cursor).await.expect("handshake");
    assert_eq!(hs.role, ClientRole::Mcp);
    assert_eq!(hs.protocol, PROTOCOL_VERSION);
    // Cursor stopped right after the newline; the rmcp bytes remain.
    let pos = cursor.position() as usize;
    assert_eq!(&bytes[pos..], b"left over rmcp bytes");
}

#[tokio::test]
async fn handshake_rejects_incompatible_protocol() {
    let mut bytes = br#"{"role":"mcp","protocol":999}"#.to_vec();
    bytes.push(b'\n');
    let mut cursor = std::io::Cursor::new(bytes);
    let err = read_handshake(&mut cursor).await.unwrap_err();
    assert!(matches!(err, HandshakeError::UnsupportedProtocol(999)));
}

#[tokio::test]
async fn handshake_rejects_unknown_role() {
    let mut bytes = br#"{"role":"ftp","protocol":1}"#.to_vec();
    bytes.push(b'\n');
    let mut cursor = std::io::Cursor::new(bytes);
    let err = read_handshake(&mut cursor).await.unwrap_err();
    assert!(matches!(err, HandshakeError::Malformed(_)));
}

#[test]
fn control_request_round_trips() {
    let json = serde_json::to_string(&ControlRequest::Status).unwrap();
    assert_eq!(json, r#"{"op":"status"}"#);
    let back: ControlRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, ControlRequest::Status));
}

#[test]
fn control_response_status_round_trips() {
    let snap = StatusSnapshot {
        uptime_secs: 12,
        mcp_clients: 2,
        indexed_files: 7,
        extensions: vec!["tower_ast_get_outline".into()],
        instrumentation: Default::default(),
        generation: 0,
        lifecycle: DaemonLifecycle::Starting {
            phase: StartupPhase::OwnershipAcquired,
        },
        workspace: CapabilityReadiness::Preparing {
            phase: StartupPhase::OwnershipAcquired,
        },
        ast: CapabilityReadiness::Preparing {
            phase: StartupPhase::OwnershipAcquired,
        },
        lsp: CapabilityReadiness::Preparing {
            phase: StartupPhase::OwnershipAcquired,
        },
    };
    let resp = ControlResponse::Status(snap);
    let json = serde_json::to_string(&resp).unwrap();
    let back: ControlResponse = serde_json::from_str(&json).unwrap();
    match back {
        ControlResponse::Status(s) => {
            assert_eq!(s.mcp_clients, 2);
            assert_eq!(s.indexed_files, 7);
        }
        _ => panic!("expected status"),
    }
}

#[test]
fn status_snapshot_round_trips_exact_readiness_wire_shape() {
    let snapshot = StatusSnapshot {
        uptime_secs: 12,
        mcp_clients: 2,
        indexed_files: 7,
        extensions: vec!["ast".to_owned()],
        instrumentation: Default::default(),
        generation: 41,
        lifecycle: DaemonLifecycle::Degraded {
            failure_class: StartupFailureClass::ExtensionTimeout,
        },
        workspace: CapabilityReadiness::Ready,
        ast: CapabilityReadiness::Preparing {
            phase: StartupPhase::CapabilitiesCatchingUp,
        },
        lsp: CapabilityReadiness::Failed {
            failure_class: StartupFailureClass::ExtensionProtocol,
        },
    };
    let expected = json!({
        "uptime_secs": 12,
        "mcp_clients": 2,
        "indexed_files": 7,
        "extensions": ["ast"],
        "instrumentation": {"phases": [], "extension_replays": []},
        "generation": 41,
        "lifecycle": {"state": "degraded", "failure_class": "extension_timeout"},
        "workspace": {"state": "ready"},
        "ast": {"state": "preparing", "phase": "capabilities_catching_up"},
        "lsp": {"state": "failed", "failure_class": "extension_protocol"},
    });

    assert_eq!(serde_json::to_value(&snapshot).unwrap(), expected);
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<StatusSnapshot>(expected.clone()).unwrap())
            .unwrap(),
        expected
    );
}

#[test]
fn initialization_pending_error_has_exact_application_payload() {
    let dependencies = [
        (CapabilityDependency::Workspace, "workspace"),
        (CapabilityDependency::Ast, "ast"),
        (CapabilityDependency::Lsp, "lsp"),
    ];
    let phases = [
        (StartupPhase::OwnershipAcquired, "ownership_acquired"),
        (StartupPhase::TransportBound, "transport_bound"),
        (StartupPhase::StorageOpening, "storage_opening"),
        (StartupPhase::WorkspaceReconciling, "workspace_reconciling"),
        (StartupPhase::WatcherCatchingUp, "watcher_catching_up"),
        (
            StartupPhase::ExtensionsDiscovering,
            "extensions_discovering",
        ),
        (
            StartupPhase::CapabilitiesCatchingUp,
            "capabilities_catching_up",
        ),
    ];

    for (dependency, dependency_name) in dependencies {
        for (phase, phase_name) in phases {
            for wait_expired in [false, true] {
                assert_application_error(
                    initialization_pending_error(InitializationPending {
                        dependency,
                        phase,
                        wait_expired,
                    }),
                    json!({
                        "code": -32004,
                        "message": "capability initialization is still in progress",
                        "data": {
                            "dependency": dependency_name,
                            "phase": phase_name,
                            "wait_expired": wait_expired,
                        }
                    }),
                );
            }
        }
    }
}

#[test]
fn capability_initialization_failed_error_has_exact_application_payload() {
    let classes = [
        (StartupFailureClass::Ownership, "ownership"),
        (StartupFailureClass::Storage, "storage"),
        (StartupFailureClass::Reconciliation, "reconciliation"),
        (StartupFailureClass::Watcher, "watcher"),
        (StartupFailureClass::ExtensionTimeout, "extension_timeout"),
        (StartupFailureClass::ExtensionCrash, "extension_crash"),
        (StartupFailureClass::ExtensionProtocol, "extension_protocol"),
        (StartupFailureClass::Cancelled, "cancelled"),
    ];

    for (dependency, dependency_name) in [
        (CapabilityDependency::Workspace, "workspace"),
        (CapabilityDependency::Ast, "ast"),
        (CapabilityDependency::Lsp, "lsp"),
    ] {
        for (failure_class, failure_name) in classes {
            assert_application_error(
                capability_initialization_failed_error(CapabilityInitializationFailed {
                    dependency,
                    failure_class,
                }),
                json!({
                    "code": -32005,
                    "message": "capability initialization failed",
                    "data": {
                        "dependency": dependency_name,
                        "failure_class": failure_name,
                    },
                }),
            );
        }
    }
}

#[test]
fn capability_stopping_error_has_exact_application_payload() {
    for (dependency, dependency_name) in [
        (CapabilityDependency::Workspace, "workspace"),
        (CapabilityDependency::Ast, "ast"),
        (CapabilityDependency::Lsp, "lsp"),
    ] {
        assert_application_error(
            capability_stopping_error(CapabilityStopping { dependency }),
            json!({
                "code": -32006,
                "message": "daemon is stopping",
                "data": {"dependency": dependency_name},
            }),
        );
    }
}

#[test]
fn capability_unavailable_error_has_exact_application_payload() {
    assert_eq!(
        serde_json::to_value(CapabilityReadiness::Unavailable).unwrap(),
        json!({"state": "unavailable"})
    );
    for (dependency, dependency_name) in [
        (CapabilityDependency::Ast, "ast"),
        (CapabilityDependency::Lsp, "lsp"),
    ] {
        assert_application_error(
            capability_unavailable_error(CapabilityUnavailable { dependency }),
            json!({
                "code": -32007,
                "message": "capability is not configured",
                "data": {"dependency": dependency_name},
            }),
        );
    }
}

#[test]
fn old_status_shape_decodes_to_conservative_preparing_defaults() {
    let old = json!({
        "uptime_secs": 5,
        "mcp_clients": 1,
        "indexed_files": 0,
        "extensions": [],
    });
    let snapshot: StatusSnapshot = serde_json::from_value(old).unwrap();

    assert_eq!(snapshot.generation, 0);
    assert_eq!(
        snapshot.lifecycle,
        DaemonLifecycle::Starting {
            phase: StartupPhase::OwnershipAcquired,
        }
    );
    let preparing = CapabilityReadiness::Preparing {
        phase: StartupPhase::OwnershipAcquired,
    };
    assert_eq!(snapshot.workspace, preparing.clone());
    assert_eq!(snapshot.ast, preparing.clone());
    assert_eq!(snapshot.lsp, preparing);
}
