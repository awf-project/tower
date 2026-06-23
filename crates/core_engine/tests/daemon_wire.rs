//! Wire-protocol unit tests (Task 3).
#![forbid(unsafe_code)]

use core_engine::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, HandshakeError, PROTOCOL_VERSION,
    StatusSnapshot, read_handshake,
};

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
