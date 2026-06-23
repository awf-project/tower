//! Socket wire types and line framing.
//!
//! Every frame is a single `\n`-terminated JSON line. The first line a client
//! sends is the [`Handshake`]; for `role:mcp` the rest of the stream is handed
//! verbatim to `rmcp::serve_server`, so [`read_handshake`] must NOT read past
//! the first newline.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Socket protocol version. Bump on any wire-format change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum handshake/control line length (bytes) before we give up.
const MAX_LINE: usize = 4096;

/// What a connecting client wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientRole {
    /// MCP relay: the remainder of the stream is rmcp JSON-RPC.
    Mcp,
    /// Read-only observer (reserved for the future TUI; not yet served).
    Observer,
    /// One-shot control request (status/shutdown).
    Control,
}

/// First line sent by every client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    pub role: ClientRole,
    pub protocol: u32,
}

impl Handshake {
    #[must_use]
    pub fn new(role: ClientRole) -> Self {
        Self {
            role,
            protocol: PROTOCOL_VERSION,
        }
    }

    /// Serialize to a single `\n`-terminated JSON line.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("handshake serializes");
        s.push('\n');
        s
    }
}

/// Failure modes when reading a handshake.
#[derive(Debug)]
pub enum HandshakeError {
    Io(std::io::Error),
    Malformed(String),
    UnsupportedProtocol(u32),
    TooLong,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "handshake io error: {e}"),
            Self::Malformed(s) => write!(f, "malformed handshake: {s}"),
            Self::UnsupportedProtocol(v) => {
                write!(
                    f,
                    "incompatible daemon protocol v{v} (this binary speaks v{PROTOCOL_VERSION})"
                )
            }
            Self::TooLong => write!(f, "handshake line exceeded {MAX_LINE} bytes"),
        }
    }
}

/// Read exactly one `\n`-terminated line (up to `cap` bytes) without consuming
/// any byte past the newline. Returns `Ok(None)` on clean EOF before any byte.
pub async fn read_line_capped<R: AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break; // EOF without trailing newline — return what we have.
        }
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() >= cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line too long",
            ));
        }
        buf.push(byte[0]);
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write a JSON line (`line` + `\n`) and flush.
pub async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Read + validate the handshake line. Does not read past the first newline.
pub async fn read_handshake<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Handshake, HandshakeError> {
    let line = match read_line_capped(reader, MAX_LINE).await {
        Ok(Some(l)) => l,
        Ok(None) => return Err(HandshakeError::Malformed("empty stream".into())),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(HandshakeError::TooLong);
        }
        Err(e) => return Err(HandshakeError::Io(e)),
    };
    let hs: Handshake =
        serde_json::from_str(&line).map_err(|e| HandshakeError::Malformed(e.to_string()))?;
    if hs.protocol != PROTOCOL_VERSION {
        return Err(HandshakeError::UnsupportedProtocol(hs.protocol));
    }
    Ok(hs)
}

// ── Control protocol ────────────────────────────────────────────────────────

/// A one-shot control request (sent after a `role:control` handshake).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ControlRequest {
    Status,
    Shutdown,
}

/// Daemon snapshot returned to `tower status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub uptime_secs: u64,
    pub mcp_clients: usize,
    pub indexed_files: usize,
    pub extensions: Vec<String>,
}

/// Reply to a [`ControlRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ControlResponse {
    Status(StatusSnapshot),
    Ok,
    Unsupported,
}
