#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum DapMessage {
    Response(DapResponse),
    Event(DapEvent),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DapResponse {
    pub request_seq: u64,
    pub command: String,
    pub success: bool,
    pub body: Value,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DapEvent {
    pub event: String,
    pub body: Value,
}

#[derive(Debug)]
pub enum DapError {
    Io(std::io::Error),
    MalformedFrame(String),
    MalformedMessage(String),
    Timeout { command: String },
    AdapterExited,
}

impl From<std::io::Error> for DapError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub trait DapTransport {
    fn send(&mut self, message: &Value) -> Result<(), DapError>;
    fn recv(&mut self, timeout: Duration) -> Result<Option<Value>, DapError>;
}

pub struct DapClient {
    transport: Box<dyn DapTransport + Send>,
    next_seq: u64,
    pending_events: Vec<DapEvent>,
    pending_responses: VecDeque<DapResponse>,
}

impl DapClient {
    pub fn new(transport: Box<dyn DapTransport + Send>) -> Self {
        Self {
            transport,
            next_seq: 1,
            pending_events: Vec::new(),
            pending_responses: VecDeque::new(),
        }
    }

    pub fn request(
        &mut self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<DapResponse, DapError> {
        let request_seq = self.next_seq;
        self.next_seq += 1;

        let request = serde_json::json!({
            "seq": request_seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        });
        self.transport.send(&request)?;

        if let Some(index) = self
            .pending_responses
            .iter()
            .position(|response| response.request_seq == request_seq && response.command == command)
        {
            return self
                .pending_responses
                .remove(index)
                .ok_or_else(|| DapError::MalformedMessage("buffered response disappeared".into()));
        }

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| DapError::Timeout {
                    command: command.to_string(),
                })?;
            let frame = self.transport.recv(remaining)?;
            let Some(frame) = frame else {
                return Err(DapError::AdapterExited);
            };

            match parse_message(frame)? {
                DapMessage::Event(event) => self.pending_events.push(event),
                DapMessage::Response(response)
                    if response.request_seq == request_seq && response.command == command =>
                {
                    return Ok(response);
                }
                DapMessage::Response(response) => self.pending_responses.push_back(response),
            }
        }
    }

    pub fn wait_for_event(
        &mut self,
        event_names: &[&str],
        timeout: Duration,
    ) -> Result<Option<DapEvent>, DapError> {
        if let Some(index) = self
            .pending_events
            .iter()
            .position(|event| event_names.contains(&event.event.as_str()))
        {
            return Ok(Some(self.pending_events.remove(index)));
        }

        let deadline = Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            let frame = match self.transport.recv(remaining) {
                Ok(frame) => frame,
                Err(DapError::Timeout { .. }) => return Ok(None),
                Err(error) => return Err(error),
            };
            let Some(frame) = frame else {
                return Err(DapError::AdapterExited);
            };

            match parse_message(frame)? {
                DapMessage::Event(event) if event_names.contains(&event.event.as_str()) => {
                    return Ok(Some(event));
                }
                DapMessage::Event(event) => self.pending_events.push(event),
                DapMessage::Response(response) => self.pending_responses.push_back(response),
            }
        }
    }

    pub fn drain_events(&mut self) -> Vec<DapEvent> {
        std::mem::take(&mut self.pending_events)
    }
}

pub fn write_frame(writer: &mut impl Write, message: &Value) -> Result<(), DapError> {
    let body = serde_json::to_vec(message)
        .map_err(|error| DapError::MalformedMessage(error.to_string()))?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>, DapError> {
    let mut content_length = None;
    let mut saw_header_bytes = false;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            if saw_header_bytes {
                return Err(DapError::MalformedFrame(
                    "EOF while reading DAP frame headers".to_string(),
                ));
            }
            return Ok(None);
        }
        saw_header_bytes = true;

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let parsed = value.trim().parse::<usize>().map_err(|error| {
                DapError::MalformedFrame(format!("invalid Content-Length: {error}"))
            })?;
            content_length = Some(parsed);
        }
    }

    let len = content_length
        .ok_or_else(|| DapError::MalformedFrame("missing Content-Length header".to_string()))?;
    let mut body = vec![0u8; len];
    std::io::Read::read_exact(reader, &mut body).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            DapError::MalformedFrame(format!("EOF while reading DAP frame body of {len} bytes"))
        } else {
            DapError::Io(error)
        }
    })?;
    let value = serde_json::from_slice(&body)
        .map_err(|error| DapError::MalformedFrame(format!("invalid JSON frame body: {error}")))?;
    Ok(Some(value))
}

fn parse_message(value: Value) -> Result<DapMessage, DapError> {
    let message_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| DapError::MalformedMessage("missing DAP message type".to_string()))?;

    match message_type {
        "event" => {
            let event = value
                .get("event")
                .and_then(Value::as_str)
                .ok_or_else(|| DapError::MalformedMessage("missing DAP event name".to_string()))?;
            let body = value.get("body").cloned().unwrap_or(Value::Null);
            Ok(DapMessage::Event(DapEvent {
                event: event.to_string(),
                body,
            }))
        }
        "response" => {
            let request_seq = value
                .get("request_seq")
                .and_then(Value::as_u64)
                .ok_or_else(|| DapError::MalformedMessage("missing DAP request_seq".to_string()))?;
            let command = value
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    DapError::MalformedMessage("missing DAP response command".to_string())
                })?;
            let success = value
                .get("success")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    DapError::MalformedMessage("missing DAP response success".to_string())
                })?;
            let body = value.get("body").cloned().unwrap_or(Value::Null);
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(DapMessage::Response(DapResponse {
                request_seq,
                command: command.to_string(),
                success,
                body,
                message,
            }))
        }
        other => Err(DapError::MalformedMessage(format!(
            "unsupported DAP message type: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::BufReader;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::{
        DapClient, DapError, DapEvent, DapMessage, DapResponse, DapTransport, read_frame,
        write_frame,
    };

    struct ScriptedTransport {
        sent: Arc<Mutex<Vec<Value>>>,
        received: VecDeque<Result<Option<Value>, DapError>>,
    }

    impl ScriptedTransport {
        fn new(received: Vec<Result<Option<Value>, DapError>>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                received: received.into(),
            }
        }

        fn sent_messages(&self) -> Arc<Mutex<Vec<Value>>> {
            Arc::clone(&self.sent)
        }
    }

    impl DapTransport for ScriptedTransport {
        fn send(&mut self, message: &Value) -> Result<(), DapError> {
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<Option<Value>, DapError> {
            self.received
                .pop_front()
                .unwrap_or(Err(DapError::AdapterExited))
        }
    }

    #[test]
    fn publicly_defines_dap_message_event_response_error_transport_and_client() {
        let event = DapEvent {
            event: "initialized".to_string(),
            body: json!({ "threadId": 7 }),
        };
        let response = DapResponse {
            request_seq: 1,
            command: "initialize".to_string(),
            success: true,
            body: json!({ "supportsConfigurationDoneRequest": true }),
            message: None,
        };

        assert_eq!(DapMessage::Event(event.clone()), DapMessage::Event(event));
        assert_eq!(
            DapMessage::Response(response.clone()),
            DapMessage::Response(response)
        );
        assert!(matches!(
            DapError::MalformedFrame("bad".to_string()),
            DapError::MalformedFrame(_)
        ));
        let _client = DapClient::new(Box::new(ScriptedTransport::new(Vec::new())));
        assert_eq!(
            env!("CARGO_PKG_NAME"),
            "debug_extension",
            "`cargo test -p debug_extension dap::tests process::tests -- --nocapture` must target this crate"
        );
        assert!(
            std::hint::black_box(cfg!(test)),
            "targeted cargo test command must compile and run the debug extension test harness"
        );
    }

    #[test]
    fn write_frame_writes_valid_content_length_dap_frames() {
        let message = json!({ "seq": 1, "type": "request", "command": "initialize" });
        let mut buffer = Vec::new();

        write_frame(&mut buffer, &message).expect("frame should write");

        let body = serde_json::to_vec(&message).expect("test message should serialize");
        let expected = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(buffer.starts_with(expected.as_bytes()));
        assert_eq!(&buffer[expected.len()..], body.as_slice());
    }

    #[test]
    fn read_frame_reads_complete_frames_and_returns_none_on_clean_eof() {
        let body = br#"{"seq":1,"type":"event","event":"initialized","body":{}}"#;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(body);
        let mut reader = BufReader::new(bytes.as_slice());

        let value = read_frame(&mut reader)
            .expect("complete frame should parse")
            .expect("frame should be present");
        assert_eq!(
            value,
            json!({ "seq": 1, "type": "event", "event": "initialized", "body": {} })
        );

        let empty: &[u8] = b"";
        let mut eof_reader = BufReader::new(empty);
        assert!(
            read_frame(&mut eof_reader)
                .expect("clean EOF should not error")
                .is_none()
        );
    }

    #[test]
    fn read_frame_returns_malformed_frame_or_io_with_context_on_invalid_input() {
        let missing_header = b"Content-Type: application/json\r\n\r\n{}";
        let mut reader = BufReader::new(missing_header.as_slice());

        let error = read_frame(&mut reader).expect_err("missing Content-Length should fail");

        match error {
            DapError::MalformedFrame(context) => {
                assert!(
                    !context.is_empty(),
                    "invalid frame errors must preserve context"
                );
            }
            DapError::Io(context) => assert!(
                !context.to_string().is_empty(),
                "invalid frame errors must preserve context"
            ),
            other => panic!("expected MalformedFrame or Io, got {other:?}"),
        }
    }

    #[test]
    fn read_frame_reports_eof_during_header_as_malformed_frame() {
        let partial_header = b"Content-Length: 42\r\n";
        let mut reader = BufReader::new(partial_header.as_slice());

        let error = read_frame(&mut reader).expect_err("partial header EOF should fail");

        assert!(matches!(
            error,
            DapError::MalformedFrame(context)
                if context.contains("EOF") && context.contains("headers")
        ));
    }

    #[test]
    fn read_frame_reports_eof_during_body_as_malformed_frame() {
        let partial_body = b"Content-Length: 42\r\n\r\n{\"seq\":1";
        let mut reader = BufReader::new(partial_body.as_slice());

        let error = read_frame(&mut reader).expect_err("partial body EOF should fail");

        assert!(matches!(
            error,
            DapError::MalformedFrame(context)
                if context.contains("EOF") && context.contains("body")
        ));
    }

    #[test]
    fn request_returns_matching_response_for_request_id_and_buffers_unrelated_events() {
        let transport = ScriptedTransport::new(vec![
            Ok(Some(json!({
                "seq": 11,
                "type": "event",
                "event": "stopped",
                "body": { "reason": "breakpoint" }
            }))),
            Ok(Some(json!({
                "seq": 12,
                "type": "response",
                "request_seq": 2,
                "command": "threads",
                "success": true,
                "body": { "threads": [] }
            }))),
            Ok(Some(json!({
                "seq": 13,
                "type": "response",
                "request_seq": 1,
                "command": "initialize",
                "success": true,
                "body": { "supportsTerminateRequest": true }
            }))),
        ]);
        let sent = transport.sent_messages();
        let mut client = DapClient::new(Box::new(transport));

        let response = client
            .request(
                "initialize",
                json!({ "adapterID": "fixture" }),
                Duration::from_millis(50),
            )
            .expect("matching response should be returned");

        assert_eq!(response.request_seq, 1);
        assert_eq!(response.command, "initialize");
        assert!(response.success);
        assert_eq!(response.body, json!({ "supportsTerminateRequest": true }));
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            &[json!({
                "seq": 1,
                "type": "request",
                "command": "initialize",
                "arguments": { "adapterID": "fixture" }
            })]
        );
        assert_eq!(
            client.drain_events(),
            vec![DapEvent {
                event: "stopped".to_string(),
                body: json!({ "reason": "breakpoint" }),
            }]
        );

        let buffered_response = client
            .request("threads", json!({}), Duration::from_millis(50))
            .expect("previously buffered non-matching response should satisfy later request");

        assert_eq!(buffered_response.request_seq, 2);
        assert_eq!(buffered_response.command, "threads");
        assert!(buffered_response.success);
        assert_eq!(buffered_response.body, json!({ "threads": [] }));
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            &[
                json!({
                    "seq": 1,
                    "type": "request",
                    "command": "initialize",
                    "arguments": { "adapterID": "fixture" }
                }),
                json!({
                    "seq": 2,
                    "type": "request",
                    "command": "threads",
                    "arguments": {}
                })
            ]
        );
    }

    #[test]
    fn drain_events_exposes_events_preserved_during_request_response_correlation() {
        let transport = ScriptedTransport::new(vec![
            Ok(Some(json!({
                "seq": 2,
                "type": "event",
                "event": "output",
                "body": { "category": "console", "output": "ready" }
            }))),
            Ok(Some(json!({
                "seq": 3,
                "type": "response",
                "request_seq": 1,
                "command": "configurationDone",
                "success": true,
                "body": {}
            }))),
        ]);
        let mut client = DapClient::new(Box::new(transport));

        client
            .request("configurationDone", json!({}), Duration::from_millis(50))
            .expect("request should complete after preserving event");

        assert_eq!(
            client.drain_events(),
            vec![DapEvent {
                event: "output".to_string(),
                body: json!({ "category": "console", "output": "ready" }),
            }]
        );
        assert!(client.drain_events().is_empty());
    }

    #[test]
    fn timeout_bounded_waits_return_timeout_without_blocking_beyond_configured_tolerance() {
        let mut client = DapClient::new(Box::new(ScriptedTransport::new(vec![Err(
            DapError::Timeout {
                command: "launch".to_string(),
            },
        )])));
        let timeout = Duration::from_millis(10);
        let started = Instant::now();

        let error = client
            .request("launch", json!({ "program": "fixture" }), timeout)
            .expect_err("request should time out");

        assert!(started.elapsed() <= timeout + Duration::from_millis(100));
        assert!(matches!(
            error,
            DapError::Timeout { command } if command == "launch"
        ));
    }

    #[test]
    fn adapter_crash_or_eof_is_reported_as_adapter_exited_without_panicking() {
        let mut client = DapClient::new(Box::new(ScriptedTransport::new(vec![Ok(None)])));

        let error = client
            .request("threads", json!({}), Duration::from_millis(50))
            .expect_err("EOF should be reported as adapter exit");

        assert!(matches!(error, DapError::AdapterExited));
    }

    #[test]
    fn wait_for_event_observes_response_before_event_ordering_and_buffers_response() {
        let mut client = DapClient::new(Box::new(ScriptedTransport::new(vec![
            Ok(Some(serde_json::json!({
                "seq": 1,
                "type": "response",
                "request_seq": 1,
                "command": "threads",
                "success": true,
                "body": { "threads": [] }
            }))),
            Ok(Some(serde_json::json!({
                "seq": 2,
                "type": "event",
                "event": "stopped",
                "body": { "reason": "breakpoint", "threadId": 7 }
            }))),
        ])));

        let event = client
            .wait_for_event(&["stopped", "terminated"], Duration::from_millis(20))
            .expect("event wait should not fail")
            .expect("stopped event should arrive before the deadline");

        assert_eq!(event.event, "stopped");
        assert_eq!(event.body["threadId"], 7);

        let response = client
            .request("threads", Value::Null, Duration::from_millis(20))
            .expect("unrelated response received while waiting for event must be preserved");
        assert_eq!(response.request_seq, 1);
        assert_eq!(response.command, "threads");
    }
}
