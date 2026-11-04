//! Cancellation-safe newline framing for the daemon's MCP connections.
//!
//! rmcp 1.7 selects between receive and outgoing notifications. Its default
//! AsyncRwTransport clears the partial line at the next receive, losing bytes
//! when an outgoing notification interrupts read_until. Keep both the buffer
//! and the SDK decoder on the transport; read_buf is cancellation-safe.

use rmcp::RoleServer;
use rmcp::model::ErrorData;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::{AsyncRwTransport, JsonRpcMessageCodec, JsonRpcMessageCodecError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio_util::bytes::{Buf, BytesMut};
use tokio_util::codec::Decoder;

pub(crate) struct McpTransport<R: AsyncRead, W: AsyncWrite> {
    read: R,
    buffer: BytesMut,
    decoder: JsonRpcMessageCodec<RxJsonRpcMessage<RoleServer>>,
    writer: AsyncRwTransport<RoleServer, tokio::io::Empty, W>,
    eof: bool,
}

impl<R, W> McpTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    pub(crate) fn new(read: R, write: W) -> Self {
        Self {
            read,
            buffer: BytesMut::new(),
            decoder: JsonRpcMessageCodec::default(),
            writer: AsyncRwTransport::new_server(tokio::io::empty(), write),
            eof: false,
        }
    }
}

impl<R, W> Transport<RoleServer> for McpTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.writer.send(message)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            let blank_line_bytes = if self.buffer.starts_with(b"\r\n") {
                2
            } else if self.buffer.starts_with(b"\n") {
                1
            } else {
                0
            };
            if blank_line_bytes != 0 {
                self.buffer.advance(blank_line_bytes);
                self.decoder = JsonRpcMessageCodec::default();
                continue;
            }
            let buffered = self.buffer.len();
            let decoded = if self.eof {
                self.decoder.decode_eof(&mut self.buffer)
            } else {
                self.decoder.decode(&mut self.buffer)
            };
            match decoded {
                Ok(Some(message)) => return Some(message),
                // The SDK can consume an ignored compatibility notification
                // while leaving another complete frame in the buffer.
                Ok(None) if self.buffer.len() < buffered && !self.buffer.is_empty() => continue,
                Ok(None) if self.eof => return None,
                Ok(None) => {}
                Err(JsonRpcMessageCodecError::Serde(_)) => {
                    let error = TxJsonRpcMessage::<RoleServer>::error(
                        ErrorData::parse_error("Parse error", None),
                        None,
                    );
                    self.writer.send(error).await.ok()?;
                    continue;
                }
                Err(_) => return None,
            }
            match self.read.read_buf(&mut self.buffer).await {
                Ok(0) => self.eof = true,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.writer.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{future::poll_fn, task::Poll};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn fragmented_request_survives_receive_cancellation_and_notification() {
        let (server, mut client) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let mut transport = McpTransport::new(read, write);
        let request = json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}});
        let line = format!("{request}\n");
        let split = line.len() / 2;
        client.write_all(&line.as_bytes()[..split]).await.unwrap();
        {
            let mut receive = std::pin::pin!(transport.receive());
            poll_fn(|cx| {
                assert!(receive.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        assert!(
            !transport.buffer.is_empty(),
            "the first fragment was consumed"
        );
        transport
            .send(
                serde_json::from_value(json!({
                    "jsonrpc":"2.0","method":"notifications/tools/list_changed"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        client.write_all(&line.as_bytes()[split..]).await.unwrap();
        let received = transport
            .receive()
            .await
            .expect("fragmented request must remain intact");
        assert_eq!(serde_json::to_value(received).unwrap(), request);
    }

    #[tokio::test]
    async fn malformed_frame_reports_parse_error_and_preserves_next_request() {
        let (server, client) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let mut transport = McpTransport::new(read, write);
        let (read, mut write) = tokio::io::split(client);
        write
            .write_all(b"invalid json\n{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        let request = transport.receive().await.unwrap();
        assert_eq!(serde_json::to_value(request).unwrap()["id"], 8);
        let mut response = String::new();
        BufReader::new(read).read_line(&mut response).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], -32700);
        assert!(response["id"].is_null());
    }

    #[tokio::test]
    async fn blank_lines_crlf_and_final_frame_without_newline_remain_supported() {
        let (server, mut client) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let mut transport = McpTransport::new(read, write);
        client.write_all(b"\n\r\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\r\n{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"ping\"}").await.unwrap();
        client.shutdown().await.unwrap();
        for id in [9, 10] {
            let request = transport.receive().await.unwrap();
            assert_eq!(serde_json::to_value(request).unwrap()["id"], id);
        }
        assert!(transport.receive().await.is_none());
    }
}
