#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use extension_protocol::{HostCall, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("host error: {0}")]
    HostError(Value),
    #[error("malformed host response")]
    MalformedResponse,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum QueuedFrame {
    Request {
        id: Option<Value>,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCallIdAllocator {
    next: u64,
}

impl HostCallIdAllocator {
    pub fn new(start: u64) -> Self {
        Self { next: start }
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
}

pub fn send_response(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<Value>,
    response: &Response,
) -> Result<(), HarnessError> {
    let result = serde_json::to_value(response)?;
    let envelope = if let Some(id) = id {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "result": result,
        })
    };
    write_envelope(out, envelope)
}

pub fn send_error(
    out: &Arc<Mutex<impl Write>>,
    id: &Option<Value>,
    code: i32,
    message: &str,
) -> Result<(), HarnessError> {
    let envelope = if let Some(id) = id {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": message },
        })
    };
    write_envelope(out, envelope)
}

pub fn write_envelope(out: &Arc<Mutex<impl Write>>, envelope: Value) -> Result<(), HarnessError> {
    let line = serde_json::to_string(&envelope)?;
    let mut out = out
        .lock()
        .map_err(|_| std::io::Error::other("output lock poisoned"))?;
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

pub fn frame_from_envelope(envelope: Value) -> Option<QueuedFrame> {
    let method = envelope.get("method").and_then(Value::as_str)?;
    let params = envelope.get("params").cloned().unwrap_or(Value::Null);
    let id = envelope.get("id").cloned();

    if id.is_some() {
        Some(QueuedFrame::Request {
            id,
            method: method.to_owned(),
            params,
        })
    } else {
        Some(QueuedFrame::Notification {
            method: method.to_owned(),
            params,
        })
    }
}

pub fn host_call<R>(
    out: &Arc<Mutex<impl Write>>,
    lines: &mut R,
    ids: &mut HostCallIdAllocator,
    method: &str,
    call: &HostCall,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, HarnessError>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let id = ids.next_id();
    write_envelope(
        out,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(call)?,
        }),
    )?;
    read_host_response(lines, id, queued)
}

pub fn read_host_response<R>(
    lines: &mut R,
    expected_id: u64,
    queued: &mut VecDeque<QueuedFrame>,
) -> Result<Value, HarnessError>
where
    R: Iterator<Item = Result<String, std::io::Error>>,
{
    let expected = serde_json::json!(expected_id);

    loop {
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(error)) => return Err(HarnessError::Io(error)),
            None => return Err(HarnessError::MalformedResponse),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let envelope: Value = serde_json::from_str(line)?;
        if envelope.get("id") == Some(&expected) {
            if let Some(result) = envelope.get("result") {
                return Ok(result.clone());
            }
            if let Some(error) = envelope.get("error") {
                return Err(HarnessError::HostError(error.clone()));
            }
            return Err(HarnessError::MalformedResponse);
        }

        if let Some(frame) = frame_from_envelope(envelope) {
            queued.push_back(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    use extension_protocol::{HostCall, Response};
    use serde_json::{Value, json};

    use super::{
        HarnessError, HostCallIdAllocator, QueuedFrame, frame_from_envelope, host_call,
        read_host_response, send_error, send_response, write_envelope,
    };

    type SendResponseFn<W> =
        fn(&Arc<Mutex<W>>, &Option<Value>, &Response) -> Result<(), HarnessError>;
    type SendErrorFn<W> = fn(&Arc<Mutex<W>>, &Option<Value>, i32, &str) -> Result<(), HarnessError>;
    type WriteEnvelopeFn<W> = fn(&Arc<Mutex<W>>, Value) -> Result<(), HarnessError>;
    type HostCallFn<W, R> = fn(
        &Arc<Mutex<W>>,
        &mut R,
        &mut HostCallIdAllocator,
        &str,
        &HostCall,
        &mut VecDeque<QueuedFrame>,
    ) -> Result<Value, HarnessError>;

    #[test]
    fn workspace_cargo_toml_includes_crates_extension_sidecar_harness() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_manifest =
            std::fs::read_to_string(manifest_dir.join("../../Cargo.toml")).unwrap();

        assert!(workspace_manifest.contains("\"crates/extension_sidecar_harness\""));
    }

    #[test]
    fn targeted_cargo_test_command_selects_this_package_without_extra_features() {
        assert_eq!(
            env!("CARGO_PKG_NAME"),
            "extension_sidecar_harness",
            "`cargo test -p extension_sidecar_harness -- --nocapture` must target this crate"
        );
        assert!(
            std::hint::black_box(cfg!(test)),
            "targeted cargo test command must compile and run this crate's test harness"
        );
    }

    #[test]
    fn lib_rs_publicly_re_exports_jsonrpc_contract_items() {
        fn assert_send_response_fn<W: io::Write>(_f: SendResponseFn<W>) {}
        fn assert_send_error_fn<W: io::Write>(_f: SendErrorFn<W>) {}
        fn assert_write_envelope_fn<W: io::Write>(_f: WriteEnvelopeFn<W>) {}
        fn assert_host_call_fn<W, R>(_f: HostCallFn<W, R>)
        where
            W: io::Write,
            R: Iterator<Item = Result<String, io::Error>>,
        {
        }

        let _: Option<crate::QueuedFrame> = None;
        let mut ids = crate::HostCallIdAllocator::new(7);
        assert_eq!(ids.next_id(), 7);

        assert_send_response_fn::<Vec<u8>>(crate::send_response);
        assert_send_error_fn::<Vec<u8>>(crate::send_error);
        assert_write_envelope_fn::<Vec<u8>>(crate::write_envelope);
        let _: fn(Value) -> Option<crate::QueuedFrame> = crate::frame_from_envelope;
        assert_host_call_fn::<Vec<u8>, std::vec::IntoIter<Result<String, io::Error>>>(
            crate::host_call::<std::vec::IntoIter<Result<String, io::Error>>>,
        );
        let _ = crate::read_host_response::<std::vec::IntoIter<Result<String, io::Error>>>;
    }

    #[test]
    fn queued_frame_is_public_enum_with_request_and_notification_variants() {
        let request = QueuedFrame::Request {
            id: Some(json!(12)),
            method: "invokeTool".to_owned(),
            params: json!({"name": "check"}),
        };
        let notification = QueuedFrame::Notification {
            method: "event/fileIndexed".to_owned(),
            params: json!({"path": "src/lib.rs"}),
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "type": "Request",
                "id": 12,
                "method": "invokeTool",
                "params": { "name": "check" }
            })
        );
        assert_eq!(
            serde_json::to_value(notification).unwrap(),
            json!({
                "type": "Notification",
                "method": "event/fileIndexed",
                "params": { "path": "src/lib.rs" }
            })
        );
    }

    #[test]
    fn host_call_id_allocator_new_and_next_id_provide_deterministic_ids() {
        let mut ids = HostCallIdAllocator::new(41);

        assert_eq!(ids.next_id(), 41);
        assert_eq!(ids.next_id(), 42);
        assert_eq!(ids.next_id(), 43);
    }

    #[test]
    fn send_response_send_error_and_write_envelope_write_newline_delimited_jsonrpc_frames() {
        let out = Arc::new(Mutex::new(Vec::new()));

        write_envelope(&out, json!({"jsonrpc": "2.0", "id": 1, "result": true})).unwrap();
        send_response(&out, &Some(json!("req-1")), &Response::Ack).unwrap();
        send_error(&out, &Some(json!(2)), -32601, "missing method").unwrap();

        let output = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "result": true})
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap(),
            json!({"jsonrpc": "2.0", "id": "req-1", "result": {"type": "Ack"}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[2]).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": { "code": -32601, "message": "missing method" }
            })
        );
    }

    #[test]
    fn frame_from_envelope_returns_none_for_responses_and_preserves_requests_and_notifications() {
        assert_eq!(
            frame_from_envelope(json!({"jsonrpc": "2.0", "id": 99, "result": true})),
            None
        );
        assert_eq!(
            frame_from_envelope(json!({"jsonrpc": "2.0", "id": 99, "error": {"code": 1}})),
            None
        );
        assert_eq!(
            frame_from_envelope(json!({
                "jsonrpc": "2.0",
                "id": "call-1",
                "method": "invokeTool",
                "params": {"name": "lint"}
            })),
            Some(QueuedFrame::Request {
                id: Some(json!("call-1")),
                method: "invokeTool".to_owned(),
                params: json!({"name": "lint"}),
            })
        );
        assert_eq!(
            frame_from_envelope(json!({
                "jsonrpc": "2.0",
                "method": "event/fileChanged",
                "params": {"path": "src/main.rs"}
            })),
            Some(QueuedFrame::Notification {
                method: "event/fileChanged".to_owned(),
                params: json!({"path": "src/main.rs"}),
            })
        );
    }

    #[test]
    fn host_call_sends_request_and_delegates_waiting_to_read_host_response() {
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut ids = HostCallIdAllocator::new(42);
        let mut queued = VecDeque::new();
        let mut lines = vec![Ok(
            json!({"jsonrpc": "2.0", "id": 42, "result": {"ok": true}}).to_string(),
        )]
        .into_iter();

        let result = host_call(
            &out,
            &mut lines,
            &mut ids,
            "workspace/hostCall",
            &HostCall::Log {
                level: "info".to_owned(),
                msg: "ready".to_owned(),
            },
            &mut queued,
        )
        .unwrap();

        assert_eq!(result, json!({"ok": true}));
        assert_eq!(ids.next_id(), 43);
        assert!(queued.is_empty());

        let output = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(output.trim()).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "workspace/hostCall",
                "params": { "type": "Log", "level": "info", "msg": "ready" }
            })
        );
    }

    #[test]
    fn read_host_response_returns_matching_response_queues_host_requests_and_discards_stale_responses()
     {
        let mut queued = VecDeque::from([QueuedFrame::Notification {
            method: "event/alreadyQueued".to_owned(),
            params: json!({"seq": 0}),
        }]);
        let mut lines = vec![
            Ok(json!({"jsonrpc": "2.0", "id": 1, "result": "stale"}).to_string()),
            Ok(json!({
                "jsonrpc": "2.0",
                "id": "host-request",
                "method": "invokeTool",
                "params": {"name": "check"}
            })
            .to_string()),
            Ok(json!({
                "jsonrpc": "2.0",
                "method": "event/fileIndexed",
                "params": {"path": "src/lib.rs"}
            })
            .to_string()),
            Ok(json!({"jsonrpc": "2.0", "id": 7, "result": {"contents": "ok"}}).to_string()),
        ]
        .into_iter();

        let result = read_host_response(&mut lines, 7, &mut queued).unwrap();

        assert_eq!(result, json!({"contents": "ok"}));
        assert_eq!(
            queued,
            VecDeque::from([
                QueuedFrame::Notification {
                    method: "event/alreadyQueued".to_owned(),
                    params: json!({"seq": 0}),
                },
                QueuedFrame::Request {
                    id: Some(json!("host-request")),
                    method: "invokeTool".to_owned(),
                    params: json!({"name": "check"}),
                },
                QueuedFrame::Notification {
                    method: "event/fileIndexed".to_owned(),
                    params: json!({"path": "src/lib.rs"}),
                },
            ])
        );

        let replayed = queued.pop_front().unwrap();
        assert_eq!(
            replayed,
            QueuedFrame::Notification {
                method: "event/alreadyQueued".to_owned(),
                params: json!({"seq": 0}),
            },
            "replaying one queued host request must not disturb later queued frames"
        );

        let mut continued_lines = vec![
            Ok(json!({"jsonrpc": "2.0", "id": 2, "result": "stale-after-replay"}).to_string()),
            Ok(json!({
                "jsonrpc": "2.0",
                "id": "continued-host-request",
                "method": "invokeTool",
                "params": {"name": "format"}
            })
            .to_string()),
            Ok(json!({"jsonrpc": "2.0", "id": 6, "result": "mismatched"}).to_string()),
            Ok(json!({"jsonrpc": "2.0", "id": 8, "result": {"contents": "continued"}}).to_string()),
        ]
        .into_iter();

        let continued = read_host_response(&mut continued_lines, 8, &mut queued).unwrap();

        assert_eq!(continued, json!({"contents": "continued"}));
        assert_eq!(
            queued,
            VecDeque::from([
                QueuedFrame::Request {
                    id: Some(json!("host-request")),
                    method: "invokeTool".to_owned(),
                    params: json!({"name": "check"}),
                },
                QueuedFrame::Notification {
                    method: "event/fileIndexed".to_owned(),
                    params: json!({"path": "src/lib.rs"}),
                },
                QueuedFrame::Request {
                    id: Some(json!("continued-host-request")),
                    method: "invokeTool".to_owned(),
                    params: json!({"name": "format"}),
                },
            ]),
            "continued HostCall waiting must preserve unreplayed queued host requests"
        );
    }

    #[test]
    fn public_harness_error_variants_cover_io_parse_host_error_and_malformed_response() {
        let io_error: HarnessError = io::Error::new(io::ErrorKind::BrokenPipe, "closed").into();
        assert!(matches!(io_error, HarnessError::Io(_)));

        let parse_error: HarnessError = serde_json::from_str::<Value>("{").unwrap_err().into();
        assert!(matches!(parse_error, HarnessError::Parse(_)));

        assert!(matches!(
            HarnessError::HostError(json!({"code": -32000})),
            HarnessError::HostError(_)
        ));
        assert!(matches!(
            HarnessError::MalformedResponse,
            HarnessError::MalformedResponse
        ));
    }

    #[test]
    fn read_host_response_maps_main_error_paths_to_harness_error_variants() {
        let mut queued = VecDeque::new();
        let mut host_error_lines = vec![Ok(
            json!({"jsonrpc": "2.0", "id": 5, "error": {"code": -32000, "message": "nope"}})
                .to_string(),
        )]
        .into_iter();
        let host_error = read_host_response(&mut host_error_lines, 5, &mut queued).unwrap_err();
        assert!(matches!(host_error, HarnessError::HostError(_)));

        let mut malformed_lines =
            vec![Ok(json!({"jsonrpc": "2.0", "id": 6}).to_string())].into_iter();
        let malformed = read_host_response(&mut malformed_lines, 6, &mut queued).unwrap_err();
        assert!(matches!(malformed, HarnessError::MalformedResponse));

        let mut parse_lines = vec![Ok("{".to_owned())].into_iter();
        let parse = read_host_response(&mut parse_lines, 7, &mut queued).unwrap_err();
        assert!(matches!(parse, HarnessError::Parse(_)));
    }
}
