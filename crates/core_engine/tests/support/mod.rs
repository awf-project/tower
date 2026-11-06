#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use core_engine::adapters::daemon::socket::socket_path;
use core_engine::adapters::daemon::wire::{ClientRole, Handshake};
use serde_json::{Value, json};
use tempfile::TempDir;

pub const SEMANTIC_TOOLS: [&str; 4] = [
    "tower_semantic_reindex",
    "tower_semantic_search",
    "tower_semantic_related_files",
    "tower_semantic_context_for_task",
];

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

pub fn invalid_semantic_startup(config: &str) -> (std::process::ExitStatus, String) {
    let workspace = tempfile::tempdir().expect("invalid semantic workspace");
    fs::create_dir_all(workspace.path().join(".tower")).unwrap();
    fs::write(workspace.path().join(".tower/config.toml"), config).unwrap();
    let output = Command::new(selected_binary())
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace.path())
        .output()
        .expect("run invalid semantic daemon");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn selected_binary() -> PathBuf {
    std::env::var_os("TOWER_PRODUCTION_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target/debug/tower"))
}

pub struct EmbeddingFixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    logs: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<VecDeque<EmbeddingReply>>>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

/// One deterministic response from the owned OpenAI-compatible fixture.
///
/// Tests enqueue these at the provider boundary instead of selecting behaviour
/// through magic search strings. The production adapter therefore receives the
/// same wire failures it would receive from a real provider.
#[derive(Clone, Debug)]
pub enum EmbeddingReply {
    Embeddings,
    Unavailable,
    Malformed,
    MissingEmbedding,
    NonFinite,
    DimensionMismatch,
    Hold(Arc<(Mutex<bool>, Condvar)>),
}

impl EmbeddingFixture {
    pub fn start(response_sentinel: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding fixture");
        listener
            .set_nonblocking(true)
            .expect("make embedding fixture nonblocking");
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        let observed = Arc::clone(&requests);
        let observed_logs = Arc::clone(&logs);
        let configured_replies = Arc::clone(&replies);
        let (stop_tx, stop_rx) = mpsc::channel();
        let sentinel = response_sentinel.to_owned();
        let worker = thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => serve_embedding(
                        stream,
                        &observed,
                        &observed_logs,
                        &configured_replies,
                        &sentinel,
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::yield_now();
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint,
            requests,
            logs,
            replies,
            stop: Some(stop_tx),
            worker: Some(worker),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn endpoint_with_userinfo(&self, userinfo: &str) -> String {
        self.endpoint
            .replacen("http://", &format!("http://{userinfo}@"), 1)
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.requests.lock().unwrap().len() < count {
            assert!(
                Instant::now() < deadline,
                "provider request {count} was not observed"
            );
            thread::yield_now();
        }
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }

    pub fn wait_for_logs(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.logs.lock().unwrap().len() < count {
            assert!(
                Instant::now() < deadline,
                "provider log {count} was not observed"
            );
            thread::yield_now();
        }
    }

    pub fn enqueue(&self, replies: impl IntoIterator<Item = EmbeddingReply>) {
        self.replies.lock().unwrap().extend(replies);
    }

    /// Holds one provider call until `release` is set. The observed request is
    /// recorded before the wait, giving process tests a deterministic boundary.
    pub fn held_reply() -> (EmbeddingReply, Arc<(Mutex<bool>, Condvar)>) {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        (EmbeddingReply::Hold(Arc::clone(&release)), release)
    }

    pub fn release(release: &Arc<(Mutex<bool>, Condvar)>) {
        let (ready, wake) = &**release;
        *ready.lock().unwrap() = true;
        wake.notify_all();
    }
}

impl Drop for EmbeddingFixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_embedding(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<String>>>,
    logs: &Arc<Mutex<Vec<String>>>,
    replies: &Arc<Mutex<VecDeque<EmbeddingReply>>>,
    sentinel: &str,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("bound fixture reads");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end + 4]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + length {
                break;
            }
        }
    }
    let request = String::from_utf8_lossy(&bytes).into_owned();
    requests.lock().unwrap().push(request.clone());
    logs.lock()
        .unwrap()
        .push(format!("provider request bytes={}", request.len()));
    let body = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|position| &bytes[position + 4..])
        .unwrap_or_default();
    let inputs = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body["input"].as_array().cloned())
        .unwrap_or_else(|| vec![Value::String(String::new())]);
    let reply = replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(EmbeddingReply::Embeddings);
    if let EmbeddingReply::Hold(release) = &reply {
        let (ready, wake) = &**release;
        // A failed assertion must not strand fixture teardown on an unreleased
        // hold. This safeguard is separate from the provider's tested deadline.
        let _ = wake
            .wait_timeout_while(ready.lock().unwrap(), Duration::from_secs(5), |ready| {
                !*ready
            })
            .unwrap();
    }
    let _ = match reply {
        EmbeddingReply::Unavailable => write_response(
            &mut stream,
            "503 Service Unavailable",
            "{\"error\":\"unavailable\"}",
        ),
        EmbeddingReply::Malformed => write_response(&mut stream, "200 OK", "not-json"),
        EmbeddingReply::MissingEmbedding => {
            write_response(&mut stream, "200 OK", "{\"data\":[{\"index\":0}]}")
        }
        EmbeddingReply::NonFinite => write_response(
            &mut stream,
            "200 OK",
            "{\"data\":[{\"index\":0,\"embedding\":[1.0,1e999,0.0]}]}",
        ),
        EmbeddingReply::DimensionMismatch => write_response(
            &mut stream,
            "200 OK",
            "{\"data\":[{\"index\":0,\"embedding\":[1.0,0.0]}]}",
        ),
        EmbeddingReply::Embeddings | EmbeddingReply::Hold(_) => {
            let data: Vec<_> = inputs
                .iter()
                .enumerate()
                .map(|(index, input)| json!({"index":index,"embedding":fixture_embedding(input.as_str().unwrap_or_default())}))
                .collect();
            let body = json!({"data":data,"model":"fixture","sentinel":sentinel}).to_string();
            write_response(&mut stream, "200 OK", &body)
        }
    }; // An expired client may close its socket before a held reply is released.
    logs.lock()
        .unwrap()
        .push("provider response complete".to_owned());
}

fn fixture_embedding(input: &str) -> [f64; 3] {
    // These are intentionally input-derived and non-collinear. They give the
    // public ranking assertions an independently controlled expected order.
    if input.contains("alpha") || input.contains("src") {
        [1.0, 0.0, 0.0]
    } else if input.contains("watcher") || input.contains("delete") {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    }
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

pub struct SemanticMcpHarness {
    pub workspace: TempDir,
    child: Child,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    stderr_path: PathBuf,
    clock_path: PathBuf,
}

impl SemanticMcpHarness {
    pub fn start(enabled: bool, provider_endpoint: Option<&str>, reindex_ms: Option<u64>) -> Self {
        Self::start_with_budgets(enabled, provider_endpoint, reindex_ms, 2000, 2000)
    }

    pub fn start_with_budgets(
        enabled: bool,
        provider_endpoint: Option<&str>,
        reindex_ms: Option<u64>,
        search_ms: u64,
        provider_ms: u64,
    ) -> Self {
        Self::start_internal(
            enabled,
            provider_endpoint,
            reindex_ms,
            search_ms,
            provider_ms,
            false,
        )
    }

    pub fn start_with_store_collision(provider_endpoint: &str) -> Self {
        Self::start_internal(true, Some(provider_endpoint), None, 2_000, 2_000, true)
    }

    fn start_internal(
        enabled: bool,
        provider_endpoint: Option<&str>,
        reindex_ms: Option<u64>,
        search_ms: u64,
        provider_ms: u64,
        store_collision: bool,
    ) -> Self {
        let workspace = tempfile::tempdir().expect("semantic workspace");
        fs::create_dir_all(workspace.path().join(".tower/extensions/semantic_context"))
            .expect("semantic extension directory");
        fs::create_dir_all(workspace.path().join(".tower")).unwrap();
        let stderr_path = workspace.path().join(".tower/daemon.stderr.log");
        let clock_path = workspace.path().join(".tower/semantic-test-clock-ms");
        fs::write(&clock_path, "0").unwrap();
        fs::copy(
            workspace_root().join("extensions/semantic_context/extension.toml"),
            workspace
                .path()
                .join(".tower/extensions/semantic_context/extension.toml"),
        )
        .expect("install semantic manifest");
        fs::copy(
            workspace_root().join("target/debug/semantic_context_extension"),
            workspace
                .path()
                .join(".tower/extensions/semantic_context/semantic_context_extension"),
        )
        .expect("install semantic binary");
        fs::write(
            workspace.path().join("src.rs"),
            "pub fn alpha_ranked_symbol() -> usize { 7 }\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("watcher.rs"),
            "pub fn watcher_delete_fixture() {}\n",
        )
        .unwrap();
        fs::write(workspace.path().join("notes.md"), "semantic notes\n").unwrap();
        fs::write(
            workspace.path().join("src_z.rs"),
            "pub fn alpha_peer() {}\n",
        )
        .unwrap();
        let mut config = format!("[semantic_context]\nenabled = {enabled}\n");
        if let Some(endpoint) = provider_endpoint {
            config.push_str(&format!(
                "[semantic_context.embedding]\nbase_url = {endpoint:?}\nmodel = \"fixture-model\"\ndimensions = 3\nbatch_size = 1\ntimeout_ms = {provider_ms}\napi_key_env = \"SEMANTIC_TEST_API_KEY\"\n"
            ));
        }
        config.push_str(&format!(
            "[semantic_context.timeouts]\nsearch_ms = {search_ms}\n"
        ));
        if let Some(cap) = reindex_ms {
            config.push_str(&format!("reindex_ms = {cap}\n"));
        }
        fs::write(workspace.path().join(".tower/config.toml"), config).unwrap();
        if store_collision {
            fs::create_dir_all(
                workspace
                    .path()
                    .join(".tower/semantic_context/index.sqlite3"),
            )
            .unwrap();
        }

        let stderr = fs::File::create(&stderr_path).expect("create daemon stderr capture");
        let child = Command::new(selected_binary())
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(workspace.path())
            .arg("--extensions-dir")
            .arg(workspace.path().join(".tower/extensions"))
            .env("SEMANTIC_TEST_API_KEY", "semantic-api-key-sentinel")
            .env("TOWER_SEMANTIC_TEST_CLOCK", &clock_path)
            .env("TOWER_SEMANTIC_TEST_OBSERVER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn semantic daemon");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match UnixStream::connect(socket_path(workspace.path())) {
                Ok(stream) => break stream,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "daemon socket unavailable: {error}"
                    );
                    thread::yield_now();
                }
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut harness = Self {
            workspace,
            child,
            stream,
            reader,
            next_id: 1,
            stderr_path,
            clock_path,
        };
        let initialized = harness.request(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"semantic-e2e","version":"1"}}
        }));
        assert!(initialized["result"].is_object(), "{initialized}");
        harness
            .stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .unwrap();
        harness.next_id = 2;
        harness
    }

    pub fn write_file(&self, path: &str, content: &str) {
        fs::write(self.workspace.path().join(path), content).unwrap();
    }

    pub fn remove_file(&self, path: &str) {
        fs::remove_file(self.workspace.path().join(path)).unwrap();
    }

    pub fn list_tools(&mut self) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.request(json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":{}}))
    }

    pub fn await_semantic_tools(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let response = self.list_tools();
            let tools = response["result"]["tools"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let names: Vec<_> = tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            if SEMANTIC_TOOLS.iter().all(|name| names.contains(name)) {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "semantic tools not published: {response}\ndaemon stderr: {}",
                self.stderr()
            );
            thread::yield_now();
        }
    }

    pub fn call(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let response = self.request(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }));
        if response["result"]["content"][0]["text"].is_string() {
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .expect("semantic tool payload is JSON")
        } else {
            response
        }
    }

    pub fn call_frame(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.request(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }))
    }

    pub fn malformed_call_frame(&mut self, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.request(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call","params":params
        }))
    }

    pub fn send_call(&mut self, name: &str, arguments: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stream,
            "{}",
            json!({
                "jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":name,"arguments":arguments}
            })
        )
        .unwrap();
        self.stream.flush().unwrap();
        id
    }

    pub fn receive(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "MCP connection closed"
            );
            let response: Value = serde_json::from_str(line.trim()).expect("MCP response JSON");
            if response["id"] == id {
                return response;
            }
        }
    }

    pub fn advance_clock_ms(&self, offset_ms: u64) {
        let staged = self.clock_path.with_extension("tmp");
        fs::write(&staged, offset_ms.to_string()).unwrap();
        fs::rename(staged, &self.clock_path).unwrap();
    }

    pub fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    pub fn await_semantic_observation(&self, marker: &str) {
        let expected = format!("semantic-test-observer {marker}");
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.stderr().contains(&expected) {
            assert!(
                Instant::now() < deadline,
                "semantic observation {expected:?} was not emitted: {}",
                self.stderr()
            );
            thread::yield_now();
        }
    }

    fn request(&mut self, frame: Value) -> Value {
        let id = frame["id"].as_u64().expect("request id");
        writeln!(self.stream, "{frame}").unwrap();
        self.stream.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap_or_else(|error| {
                    panic!(
                        "MCP response read failed for {} (id {id}): {error}; daemon stderr: {}",
                        frame["method"],
                        self.stderr(),
                    )
                }) > 0,
                "MCP connection closed"
            );
            let response: Value = serde_json::from_str(line.trim()).expect("MCP response JSON");
            if response["id"] == id {
                return response;
            }
        }
    }

    pub fn sqlite_path(&self) -> PathBuf {
        self.workspace
            .path()
            .join(".tower/semantic_context/index.sqlite3")
    }
}

impl Drop for SemanticMcpHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
