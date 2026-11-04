use extension_protocol::{
    Capability, InitParams, InitResult, PROTOCOL_VERSION, Response, ToolDecl,
};
use extension_sidecar_harness::{
    HostCallIdAllocator, QueuedFrame, frame_from_envelope, read_host_response, send_error,
    send_response, write_envelope,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{self, BufRead, BufReader},
    sync::{Arc, Mutex},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticReindexRequest {}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSearchRequest {
    pub query: String,
    pub limit: u32,
    pub path_prefixes: Option<Vec<String>>,
    pub languages: Option<Vec<String>>,
    pub lexical_rerank: Option<bool>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticRelatedFilesRequest {
    pub path: String,
    pub limit: u32,
    pub path_prefixes: Option<Vec<String>>,
    pub languages: Option<Vec<String>>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticContextForTaskRequest {
    pub task: String,
    pub token_budget: u32,
    pub limit: u32,
    pub path_prefixes: Option<Vec<String>>,
    pub languages: Option<Vec<String>>,
}
#[derive(Debug, Serialize)]
pub struct SemanticReindexResponse {
    pub status: String,
    pub indexed_files: u64,
    pub indexed_chunks: u64,
    pub generation: u64,
}
#[derive(Debug, Serialize)]
pub struct SemanticSearchHit {
    pub path: String,
    pub start_byte: u64,
    pub end_byte: u64,
    pub score: f32,
    pub language: String,
    pub snippet: String,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct SemanticSearchResponse {
    pub status: String,
    pub hits: Vec<SemanticSearchHit>,
    pub truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct SemanticRelatedFile {
    pub path: String,
    pub score: f32,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub struct SemanticRelatedFilesResponse {
    pub status: String,
    pub files: Vec<SemanticRelatedFile>,
    pub truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct SemanticContextForTaskResponse {
    pub status: String,
    pub chunks: Vec<SemanticSearchHit>,
    pub estimated_tokens: u32,
    pub truncated: bool,
    pub eligible_unfitted: u32,
}

const REINDEX_SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
const SEARCH_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":0},"path_prefixes":{"type":"array","items":{"type":"string"}},"languages":{"type":"array","items":{"type":"string"}},"lexical_rerank":{"type":"boolean"}},"required":["query","limit"],"additionalProperties":false}"#;
const RELATED_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"limit":{"type":"integer","minimum":0},"path_prefixes":{"type":"array","items":{"type":"string"}},"languages":{"type":"array","items":{"type":"string"}}},"required":["path","limit"],"additionalProperties":false}"#;
const CONTEXT_SCHEMA: &str = r#"{"type":"object","properties":{"task":{"type":"string"},"token_budget":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":0},"path_prefixes":{"type":"array","items":{"type":"string"}},"languages":{"type":"array","items":{"type":"string"}}},"required":["task","token_budget","limit"],"additionalProperties":false}"#;
const FENCE_SCHEMA: &str = r#"{"type":"object","properties":{"epoch":{"type":"integer","minimum":0},"delivered_events":{"type":"integer","minimum":0}},"required":["epoch","delivered_events"],"additionalProperties":false}"#;

fn declarations() -> Vec<ToolDecl> {
    [
        (
            "semantic_reindex",
            "Rebuild the semantic index from the host-provided workspace snapshot.",
            REINDEX_SCHEMA,
        ),
        (
            "semantic_search",
            "Search indexed semantic context.",
            SEARCH_SCHEMA,
        ),
        (
            "semantic_related_files",
            "Find files related to a workspace path.",
            RELATED_SCHEMA,
        ),
        (
            "semantic_context_for_task",
            "Collect a bounded semantic context for a task.",
            CONTEXT_SCHEMA,
        ),
        (
            "_tower_catch_up_fence",
            "Host-internal catch-up ordering fence.",
            FENCE_SCHEMA,
        ),
    ]
    .into_iter()
    .map(|(n, d, s)| ToolDecl {
        name: n.into(),
        description: d.into(),
        schema_json: s.into(),
    })
    .collect()
}
fn init_result() -> InitResult {
    InitResult {
        tools: declarations(),
        events: vec![
            "event/fileIndexed".into(),
            "event/fileChanged".into(),
            "event/fileDeleted".into(),
        ],
        capabilities: vec![
            Capability::ReadFile,
            Capability::ListFiles,
            Capability::IndexGet,
            Capability::Log,
        ],
    }
}
// The transport owns the input queue so HostCall waits and the main dispatcher
// consume exactly the same ordered stream. Requests encountered during a wait
// are replayed before reading another frame.
struct Input {
    lines: std::io::Lines<BufReader<std::io::Stdin>>,
    ids: HostCallIdAllocator,
    queued: VecDeque<QueuedFrame>,
}
struct Transport {
    input: Mutex<Input>,
    output: Arc<Mutex<std::io::Stdout>>,
}
impl semantic_context_extension::adapters::workspace::WorkspaceHostCallTransport for Transport {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut input = self.input.lock().map_err(|_| "input unavailable")?;
        let id = input.ids.next_id();
        write_envelope(
            &self.output,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .map_err(|_| "host unavailable")?;
        let Input { lines, queued, .. } = &mut *input;
        read_host_response(lines, id, queued).map_err(|_| "host unavailable".to_owned())
    }
}
impl Transport {
    fn next(&self) -> Option<QueuedFrame> {
        let mut input = self.input.lock().ok()?;
        if let Some(frame) = input.queued.pop_front() {
            return Some(frame);
        }
        loop {
            let line = input.lines.next()?.ok()?;
            if let Some(frame) = serde_json::from_str(&line)
                .ok()
                .and_then(frame_from_envelope)
            {
                return Some(frame);
            }
        }
    }
}

pub fn run() -> ! {
    let transport = Arc::new(Transport {
        input: Mutex::new(Input {
            lines: BufReader::new(io::stdin()).lines(),
            ids: HostCallIdAllocator::new(10_000),
            queued: VecDeque::new(),
        }),
        output: Arc::new(Mutex::new(io::stdout())),
    });
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("semantic runtime");
    let mut runtime = None;
    while let Some(frame) = transport.next() {
        let QueuedFrame::Request { id, method, params } = frame else {
            continue;
        };
        match method.as_str() {
            "initialize" => {
                let init = serde_json::from_value::<InitParams>(params);
                let Ok(init) = init else {
                    let _ = send_error(&transport.output, &id, -32602, "Invalid params");
                    continue;
                };
                if init.protocol_version != PROTOCOL_VERSION {
                    let _ = send_error(&transport.output, &id, -32600, "Protocol version mismatch");
                    continue;
                }
                match init.extension_config.ok_or(()).and_then(|config| {
                    executor.block_on(super::runtime::Runtime::new(config, transport.clone()))
                }) {
                    Ok(value) => {
                        runtime = Some(value);
                        let _ = send_response(
                            &transport.output,
                            &id,
                            &Response::Initialized(init_result()),
                        );
                    }
                    Err(()) => {
                        let _ = send_error(
                            &transport.output,
                            &id,
                            -32602,
                            "Invalid semantic configuration",
                        );
                    }
                }
            }
            "deliverEvent" if runtime.is_some() => {
                executor.block_on(runtime.as_mut().expect("initialized").event(params));
                let _ = send_response(&transport.output, &id, &Response::Ack);
            }
            "invokeTool" if runtime.is_some() => {
                let name = params.get("name").and_then(Value::as_str);
                let args = params.get("params").cloned();
                let result = match (name, args) {
                    (Some(name), Some(args)) => executor
                        .block_on(runtime.as_mut().expect("initialized").dispatch(name, args)),
                    _ => Err(()),
                };
                match result {
                    Ok(value) => {
                        let _ = send_response(&transport.output, &id, &Response::ToolResult(value));
                    }
                    Err(()) => {
                        let _ = send_error(&transport.output, &id, -32602, "Invalid params");
                    }
                }
            }
            "shutdown" => {
                let _ = send_response(&transport.output, &id, &Response::Ack);
                break;
            }
            _ => {
                let _ = send_error(&transport.output, &id, -32600, "Invalid Request");
            }
        }
    }
    std::process::exit(0)
}
