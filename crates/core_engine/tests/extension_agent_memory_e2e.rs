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

const MEMORY_TOOLS: [&str; 7] = [
    "tower_memory_remember",
    "tower_memory_update",
    "tower_memory_forget",
    "tower_memory_search",
    "tower_memory_timeline",
    "tower_memory_get",
    "tower_memory_context_for_task",
];

#[derive(Clone)]
enum ProviderReply {
    Projection(String),
    Unavailable,
    Hold(Arc<(Mutex<bool>, Condvar)>, String),
}

struct ProviderFixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    replies: Arc<Mutex<VecDeque<ProviderReply>>>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ProviderFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/enrich", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        let observed = Arc::clone(&requests);
        let configured = Arc::clone(&replies);
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => serve_provider(stream, &observed, &configured),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::yield_now()
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint,
            requests,
            replies,
            stop: Some(stop_tx),
            worker: Some(worker),
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    fn enqueue(&self, reply: ProviderReply) {
        self.replies.lock().unwrap().push_back(reply);
    }
    fn held(projection: &str) -> (ProviderReply, Arc<(Mutex<bool>, Condvar)>) {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        (
            ProviderReply::Hold(Arc::clone(&gate), projection.to_owned()),
            gate,
        )
    }
    fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
    fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.requests.lock().unwrap().len() < count {
            assert!(
                Instant::now() < deadline,
                "provider request {count} not observed"
            );
            thread::yield_now();
        }
    }
    fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ProviderFixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_provider(
    mut stream: TcpStream,
    requests: &Arc<Mutex<Vec<Vec<u8>>>>,
    replies: &Arc<Mutex<VecDeque<ProviderReply>>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end + 4]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    requests.lock().unwrap().push(bytes);
    let reply = replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| ProviderReply::Projection("default-projection".to_owned()));
    let (status, projection) = match reply {
        ProviderReply::Projection(value) => ("200 OK", value),
        ProviderReply::Unavailable => ("503 Service Unavailable", String::new()),
        ProviderReply::Hold(gate, value) => {
            let _ = gate.1.wait_timeout_while(
                gate.0.lock().unwrap(),
                Duration::from_secs(5),
                |ready| !*ready,
            );
            ("200 OK", value)
        }
    };
    let body = json!({"projection":projection}).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn tower_binary() -> PathBuf {
    std::env::var_os("TOWER_PRODUCTION_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target/debug/tower"))
}

fn record(title: &str, body: &str) -> Value {
    json!({
        "record_type":"observation",
        "title":title,
        "body":body,
        "provenance":{
            "branch":"feature/public-memory",
            "paths":["src/lib.rs"],
            "symbols":["public_contract"],
            "tags":["e2e"],
            "source_agent":"review-remediation",
            "source_client":"mcp-e2e",
            "commit":"abc123",
            "pull_request":"17",
            "issue":"F017"
        },
        "confidence":0.9
    })
}

fn record_with(kind: &str, title: &str, body: &str, provenance: Value) -> Value {
    let mut value = record(title, body);
    value["record_type"] = json!(kind);
    value["provenance"] = provenance;
    value
}

fn provenance(
    branch: &str,
    path: &str,
    symbol: &str,
    tag: &str,
    agent: &str,
    client: &str,
) -> Value {
    json!({
        "branch":branch, "paths":[path], "symbols":[symbol], "tags":[tag],
        "source_agent":agent, "source_client":client, "commit":"abc123",
        "pull_request":"17", "issue":"F017"
    })
}

fn tool_payload(response: &Value) -> Result<Value, String> {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .ok_or_else(|| format!("missing MCP tool text: {response}"))?;
    serde_json::from_str(text)
        .map_err(|error| format!("expected JSON payload: {error}; {response}"))
}

fn collect_live_agent_memory_bytes(root: &Path) -> Vec<u8> {
    let db = sled::open(root).expect("open real agent-memory sled database");
    let tree = db.open_tree("agent_memory_v1").unwrap();
    let mut bytes = Vec::new();
    for entry in tree.iter() {
        let (key, value) = entry.unwrap();
        bytes.extend_from_slice(&key);
        bytes.extend_from_slice(&value);
    }
    bytes
}

fn seed_real_legacy_migration(root: &Path, raw_title: &str) {
    let db = sled::open(root).expect("open real agent-memory sled database");
    let tree = db.open_tree("agent_memory_v1").unwrap();
    let entries: Vec<_> = tree.iter().map(|entry| entry.unwrap()).collect();
    let schema_key = entries
        .iter()
        .find(|(key, _)| key.ends_with(b"meta/schema"))
        .unwrap()
        .0
        .clone();
    let mut injected = false;
    for (key, value) in &entries {
        if key[0] == 1 && key[33..].starts_with(b"record/") {
            let mut legacy: Value = serde_json::from_slice(value).unwrap();
            legacy["schema_version"] = json!(0);
            if legacy["title"] == "privacy title" {
                legacy["title"] = json!(raw_title);
                injected = true;
            }
            tree.insert(key, serde_json::to_vec(&legacy).unwrap())
                .unwrap();
        } else if key[0] == 1 && key[33..].starts_with(b"projection/") {
            tree.remove(key).unwrap();
        }
    }
    assert!(injected, "migration sentinel was not injected");
    let checkpoint_key = [&schema_key[..33], b"meta/migration"].concat();
    tree.insert(schema_key, br#"{"version":0}"#).unwrap();
    tree.insert(
        checkpoint_key,
        br#"{"source":0,"target":1,"state":"prepared"}"#.as_slice(),
    )
    .unwrap();
    tree.flush().unwrap();
}

struct MemoryMcpHarness {
    workspace: TempDir,
    child: Option<Child>,
    stream: Option<UnixStream>,
    reader: Option<BufReader<UnixStream>>,
    next_id: u64,
    stderr_path: PathBuf,
    control_path: PathBuf,
    helper_started_path: PathBuf,
}

impl MemoryMcpHarness {
    fn start() -> Self {
        Self::start_with_config("")
    }

    fn start_with_config(config: &str) -> Self {
        let mut harness = Self::new_with_config(config);
        harness.restart();
        harness
    }

    /// Allows startup fixtures to exist before the daemon assembles its real
    /// storage adapter; no test-only behavior is injected into production.
    fn start_with_config_and_preparation(config: &str, prepare: impl FnOnce(&Path)) -> Self {
        let mut harness = Self::new_with_config(config);
        prepare(harness.workspace.path());
        harness.restart();
        harness
    }

    fn new_with_config(config: &str) -> Self {
        let workspace = tempfile::tempdir().expect("agent-memory workspace");
        let extension_dir = workspace.path().join(".tower/extensions/agent_memory");
        fs::create_dir_all(&extension_dir).unwrap();
        let trusted_binary = workspace_root().join("target/debug/agent_memory_extension");
        let manifest =
            fs::read_to_string(workspace_root().join("extensions/agent_memory/extension.toml"))
                .unwrap()
                .replacen(
                    "command = [\"agent_memory_extension\"]",
                    &format!("command = [{:?}]", trusted_binary.to_string_lossy()),
                    1,
                );
        fs::write(extension_dir.join("extension.toml"), manifest).unwrap();
        let helper_binary = workspace_root().join("target/debug/test_helper_extension");
        let helper_dir = workspace.path().join(".tower/extensions/test_helper");
        fs::create_dir_all(&helper_dir).unwrap();
        fs::write(
            helper_dir.join("extension.toml"),
            format!(
                "name = \"test_helper\"\nversion = \"0.1.0\"\ncommand = [{:?}]\nactivation = \"lazy\"\n\n[capabilities]\nrequired = []\n\n[[tools]]\nname = \"agent_memory_undeclared\"\ndescription = \"unauthorized storage probe\"\nschema_json = \"{}\"\n",
                helper_binary.to_string_lossy(), "{}"
            ),
        )
        .unwrap();
        let rogue_dir = workspace.path().join(".tower/extensions/rogue_memory");
        fs::create_dir_all(&rogue_dir).unwrap();
        fs::write(
            rogue_dir.join("extension.toml"),
            format!(
                "name = \"rogue_memory\"\nversion = \"0.1.0\"\ncommand = [{:?}]\nactivation = \"lazy\"\n\n[capabilities]\nrequired = [\"agent_memory_storage\"]\n\n[[tools]]\nname = \"probe\"\ndescription = \"self-declared storage probe\"\nschema_json = \"{}\"\n",
                helper_binary.to_string_lossy(), "{}"
            ),
        )
        .unwrap();
        fs::write(workspace.path().join(".tower/config.toml"), config).unwrap();
        let stderr_path = workspace
            .path()
            .join(".tower/agent-memory-daemon.stderr.log");
        let control_path = workspace.path().join(".tower/agent-memory-control");
        let helper_started_path = workspace.path().join(".tower/test-helper-started");
        Self {
            workspace,
            child: None,
            stream: None,
            reader: None,
            next_id: 1,
            stderr_path,
            control_path,
            helper_started_path,
        }
    }

    fn restart(&mut self) {
        self.stop();
        let stderr = fs::File::create(&self.stderr_path).unwrap();
        let mut command = Command::new(tower_binary());
        let log_input = self.workspace.path().join(".tower/log-input");
        if log_input.exists() {
            command.env("TOWER_TEST_AGENT_MEMORY_LOG_INPUT", log_input);
        }
        let child = command
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(self.workspace.path())
            .arg("--extensions-dir")
            .arg(self.workspace.path().join(".tower/extensions"))
            .env("TOWER_TEST_AGENT_MEMORY_CONTROL", &self.control_path)
            .env(
                "TOWER_TEST_AGENT_MEMORY_ACCESS_TRACE",
                self.control_path.with_extension("access"),
            )
            .env("TOWER_TEST_HELPER_STARTED", &self.helper_started_path)
            .env(
                "TOWER_TEST_HOST_LOG_CAPTURE",
                self.control_path.with_extension("logs"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn agent-memory daemon");
        self.child = Some(child);

        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.stderr().contains("extension tool(s)") {
            assert!(
                Instant::now() < deadline,
                "agent-memory extension did not finish loading: {}",
                self.stderr()
            );
            thread::yield_now();
        }
        let mut stream = loop {
            match UnixStream::connect(socket_path(self.workspace.path())) {
                Ok(stream) => break stream,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "daemon socket unavailable: {error}; {}",
                        self.stderr()
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
        self.reader = Some(BufReader::new(stream.try_clone().unwrap()));
        self.stream = Some(stream);
        let initialized = self.request(json!({
            "jsonrpc":"2.0","id":self.next_id,"method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"agent-memory-e2e","version":"1"}}
        }));
        assert!(initialized["result"].is_object(), "{initialized}");
        self.next_id += 1;
        self.stream
            .as_mut()
            .unwrap()
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .unwrap();
        self.await_tools();
    }

    fn stop(&mut self) {
        self.reader = None;
        self.stream = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while socket_path(self.workspace.path()).exists() && Instant::now() < deadline {
            thread::yield_now();
        }
    }

    fn await_tools(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let response = self.raw_call("tools/list", json!({}));
            let names: Vec<_> = response["result"]["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            if MEMORY_TOOLS.iter().all(|name| names.contains(name)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "memory tools not published: {response}; {}",
                self.stderr()
            );
            thread::yield_now();
        }
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.raw_call("tools/call", json!({"name":name,"arguments":arguments}));
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool did not return JSON text: {response}"));
        serde_json::from_str(text).unwrap_or_else(|error| {
            panic!("tool did not return a JSON payload: {error}; {response}")
        })
    }

    fn raw_call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.request(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
    }

    fn send_call(&mut self, name: &str, arguments: Value) {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stream.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
        )
        .unwrap();
        self.stream.as_mut().unwrap().flush().unwrap();
    }

    fn request(&mut self, frame: Value) -> Value {
        let id = frame["id"].clone();
        writeln!(self.stream.as_mut().unwrap(), "{frame}").unwrap();
        self.stream.as_mut().unwrap().flush().unwrap();
        loop {
            let mut line = String::new();
            let read = self
                .reader
                .as_mut()
                .unwrap()
                .read_line(&mut line)
                .unwrap_or_else(|error| panic!("MCP read failed: {error}; {}", self.stderr()));
            assert!(read > 0, "MCP connection closed; {}", self.stderr());
            let response: Value = serde_json::from_str(line.trim()).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn set_control(&self, value: &str) {
        fs::write(&self.control_path, value).unwrap();
        for extension in ["reached", "release"] {
            let _ = fs::remove_file(self.control_path.with_extension(extension));
        }
    }

    fn clear_control(&self) {
        self.set_control("");
    }

    fn await_path(&self, path: &Path, context: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < deadline, "{context}; {}", self.stderr());
            thread::yield_now();
        }
    }

    fn extension_processes(&self) -> Vec<u32> {
        let pid = self.child.as_ref().expect("running daemon").id();
        fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .unwrap_or_default()
            .split_whitespace()
            .map(|value| value.parse().expect("child pid"))
            .collect()
    }

    fn assert_processes_exited(processes: &[u32]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        for process in processes {
            while Path::new(&format!("/proc/{process}")).exists() && Instant::now() < deadline {
                thread::yield_now();
            }
            assert!(
                !Path::new(&format!("/proc/{process}")).exists(),
                "extension process {process} survived daemon shutdown"
            );
        }
    }

    fn remember(&mut self, title: &str, body: &str, key: &str) -> Value {
        self.call(
            "tower_memory_remember",
            json!({"record":record(title, body),"idempotency_key":key}),
        )
    }

    fn search(&mut self, query: &str, page: Value) -> Value {
        self.call(
            "tower_memory_search",
            json!({"query":query,"filters":{},"page":page}),
        )
    }
}

impl Drop for MemoryMcpHarness {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn remembered_records_survive_restart_with_identity_and_provenance() {
    let mut harness = MemoryMcpHarness::start();
    let remembered = harness.call(
        "tower_memory_remember",
        json!({"record":record("durable identity", "survives daemon restart"),"idempotency_key":"restart-key"}),
    );
    assert_eq!(remembered["code"], "ok");
    assert_eq!(remembered["durable"], true);
    let id = remembered["id"].clone();

    harness.restart();
    let fetched = harness.call("tower_memory_get", json!({"ids":[id.clone()]}));
    assert_eq!(fetched["items"][0]["code"], "ok", "{fetched}");
    assert_eq!(fetched["items"][0]["record"]["id"], id);
    assert_eq!(fetched["items"][0]["record"]["title"], "durable identity");
    assert_eq!(
        fetched["items"][0]["record"]["provenance"]["source_client"],
        "mcp-e2e"
    );
}

#[test]
fn public_batches_preserve_order_and_independent_outcomes() {
    let mut harness = MemoryMcpHarness::start();
    let first = harness.call(
        "tower_memory_remember",
        json!({"record":record("first", "one"),"idempotency_key":"batch-1"}),
    );
    let second = harness.call(
        "tower_memory_remember",
        json!({"record":record("second", "two"),"idempotency_key":"batch-2"}),
    );
    let first_id = first["id"].clone();
    let second_id = second["id"].clone();
    let batch = harness.call(
        "tower_memory_update",
        json!({"items":[
            {"id":first_id,"expected_revision":"1","record":record("duplicate-a", "ignored")},
            {"id":"unknown-id","expected_revision":"1","record":record("unknown", "ignored")},
            {"id":second_id,"expected_revision":"1","record":record("updated", "committed")},
            {"id":first["id"],"expected_revision":"1","record":record("duplicate-b", "ignored")}
        ]}),
    );
    let codes: Vec<_> = batch["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["duplicate_id", "not_found", "ok", "duplicate_id"]);
    assert_eq!(batch["items"][2]["durable"], true);

    let forgotten = harness.call(
        "tower_memory_forget",
        json!({"items":[
            {"id":"another-unknown-id","expected_revision":"1"},
            {"id":second["id"].clone(),"expected_revision":"2"}
        ]}),
    );
    assert_eq!(forgotten["items"][0]["code"], "not_found", "{forgotten}");
    assert_eq!(forgotten["items"][1]["code"], "ok", "{forgotten}");
    assert_eq!(forgotten["items"][1]["durable"], true, "{forgotten}");
}

// An independent public MCP session, also used to race clients on one revision.
fn independent_call(
    socket: &Path,
    name: &str,
    arguments: Value,
    gate: Option<&std::sync::Barrier>,
) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut exchange = |frame: Value| {
        writeln!(stream, "{frame}").unwrap();
        if frame.get("id").is_none() {
            return Value::Null;
        }
        let id = frame["id"].clone();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == id {
                break response;
            }
        }
    };
    let init = exchange(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"independent","version":"1"}}}),
    );
    assert!(init["result"].is_object());
    exchange(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    if let Some(gate) = gate {
        gate.wait();
    }
    tool_payload(&exchange(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":name,"arguments":arguments}}))).unwrap()
}

#[test]
fn public_mutations_enforce_revisions_idempotency_and_acknowledgement() {
    let mut harness = MemoryMcpHarness::start();
    let args = json!({"record":record("initial", "body"),"idempotency_key":"mutation-key"});
    let initial = harness.call("tower_memory_remember", args.clone());
    assert_eq!(initial["durable"], true);
    harness.restart();
    let retry = harness.call("tower_memory_remember", args.clone());
    assert_eq!(retry["id"], initial["id"]);
    assert_eq!(retry["durable"], true);
    let conflict = harness.call(
        "tower_memory_remember",
        json!({"record":record("different", "body"),"idempotency_key":"mutation-key"}),
    );
    assert_eq!(conflict["code"], "idempotency_conflict");

    let socket = socket_path(harness.workspace.path());
    let gate = std::sync::Barrier::new(2);
    let update = json!({"items":[{"id":initial["id"],"expected_revision":"1","record":record("winner", "revision two")}]});
    let forget = json!({"items":[{"id":initial["id"],"expected_revision":"1"}]});
    let (updated, forgotten) = thread::scope(|scope| {
        let updater =
            scope.spawn(|| independent_call(&socket, "tower_memory_update", update, Some(&gate)));
        let forgetter =
            scope.spawn(|| independent_call(&socket, "tower_memory_forget", forget, Some(&gate)));
        (updater.join().unwrap(), forgetter.join().unwrap())
    });
    let codes = [&updated["items"][0]["code"], &forgotten["items"][0]["code"]];
    assert_eq!(
        codes.iter().filter(|code| **code == "ok").count(),
        1,
        "{updated} {forgotten}"
    );
    assert!(
        codes
            .iter()
            .any(|code| **code == "revision_conflict" || **code == "not_found")
    );
    if updated["items"][0]["code"] == "ok" {
        let deleted = harness.call(
            "tower_memory_forget",
            json!({"items":[{"id":initial["id"],"expected_revision":"2"}]}),
        );
        assert_eq!(deleted["items"][0]["code"], "ok");
    }
    let retry = harness.call("tower_memory_remember", args);
    assert_eq!(retry["code"], "record_forgotten");
    assert_eq!(retry["durable"], false);

    harness.set_control("unavailable");
    let unavailable = harness.remember("unavailable", "body", "unavailable");
    assert_eq!(unavailable["code"], "storage_unavailable");
    assert_eq!(unavailable["durable"], false);
    harness.set_control("flush_failure");
    let uncertain = harness.remember("uncertain", "body", "uncertain");
    assert_eq!(uncertain["code"], "commit_outcome_unknown");
    assert_eq!(uncertain["durable"], false);
    let uncertain_retry = harness.remember("uncertain", "body", "uncertain");
    assert_eq!(uncertain_retry["code"], "commit_outcome_unknown");
    assert_eq!(uncertain_retry["durable"], false);
    harness.clear_control();
    let reconciled = harness.remember("uncertain", "body", "uncertain");
    assert_eq!(reconciled["code"], "ok");
    assert_eq!(reconciled["durable"], true);
    harness.restart();
    let fetched = harness.call("tower_memory_get", json!({"ids":[reconciled["id"]]}));
    assert_eq!(fetched["items"][0]["code"], "ok");

    let mut volatile = MemoryMcpHarness::start_with_config_and_preparation(
        "[agent_memory.storage]\nstartup_policy='in_memory'\n",
        |workspace| {
            fs::write(workspace.join(".tower/agent_memory"), "not a directory").unwrap();
        },
    );
    let acknowledgement = volatile.remember("volatile", "body", "volatile");
    assert_eq!(acknowledgement["code"], "ok");
    assert_eq!(acknowledgement["persistence_mode"], "in_memory");
    assert_eq!(acknowledgement["durable"], false);
}

fn durable_snapshot(root: &Path) -> std::collections::BTreeMap<Vec<u8>, Vec<u8>> {
    let db = sled::open(root).unwrap();
    db.open_tree("agent_memory_v1")
        .unwrap()
        .iter()
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn assert_complete_remember_recovery(
    prior: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    recovered: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
) {
    if recovered == prior {
        return;
    }
    let added: Vec<_> = recovered
        .iter()
        .filter(|(key, _)| !prior.contains_key(*key))
        .collect();
    assert_eq!(
        added.len(),
        4,
        "record, projection, job, and idempotency must appear together"
    );
    let (record_key, bytes) = added
        .iter()
        .find(|(key, _)| key[33..].starts_with(b"record/"))
        .unwrap();
    let record: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(record["title"], "interrupted");
    assert_eq!(record["body"], "transaction published");
    assert_eq!(record["revision"], "1");
    assert_eq!(record["schema_version"], 1);
    let suffix = &record_key[33 + b"record/".len()..];
    for (prefix, expected) in [
        (
            b"projection/".as_slice(),
            b"interrupted\ntransaction published".as_slice(),
        ),
        (b"job/".as_slice(), b"pending".as_slice()),
    ] {
        let key = [record_key[..33].as_ref(), prefix, suffix].concat();
        assert_eq!(recovered.get(&key).unwrap(), expected);
    }
    let (idem_key, idem_bytes) = added
        .iter()
        .find(|(key, _)| key[33..].starts_with(b"idempotency/"))
        .unwrap();
    assert_eq!(
        &idem_key[33 + b"idempotency/".len()..],
        record["idem_digest"].as_str().unwrap().as_bytes()
    );
    let idem: Value = serde_json::from_slice(idem_bytes).unwrap();
    assert_eq!(idem["id"], record["id"]);
    assert_eq!(idem["deleted"], false);
    assert_eq!(idem["payload_digest"].as_str().unwrap().len(), 64);
    // Every existing canonical key, projection, tombstone, idempotency entry,
    // schema and migration checkpoint must be byte-for-byte unchanged.
    for (key, value) in prior {
        if key[0] == 0 {
            let old = u64::from_be_bytes(value.as_slice().try_into().unwrap());
            let new = u64::from_be_bytes(recovered[key].as_slice().try_into().unwrap());
            assert_eq!(new, old + 1);
        } else {
            assert_eq!(recovered.get(key), Some(value), "changed prior key {key:?}");
        }
    }
}

#[test]
fn transaction_boundary_crashes_recover_previous_or_complete_state() {
    for phase in ["before_transaction", "after_transaction", "before_flush"] {
        let mut harness = MemoryMcpHarness::start();
        let baseline = harness.remember("baseline", "known state", "crash-baseline");
        let removed = harness.remember("forgotten", "deleted state", "crash-forgotten");
        let forgotten = harness.call(
            "tower_memory_forget",
            json!({"items":[{"id":removed["id"],"expected_revision":"1"}]}),
        );
        assert_eq!(forgotten["items"][0]["durable"], true);
        harness.stop();
        let root = harness.workspace.path().join(".tower/agent_memory");
        let prior = durable_snapshot(&root);
        assert!(
            prior
                .keys()
                .any(|key| key[0] == 1 && key[33..].starts_with(b"tombstone/"))
        );
        assert!(prior.keys().any(|key| key.ends_with(b"meta/schema")));
        assert!(!prior.keys().any(|key| key.ends_with(b"meta/migration")));
        harness.restart();
        harness.set_control(phase);
        harness.send_call("tower_memory_remember", json!({"record":record("interrupted", "transaction published"),"idempotency_key":"crash-key"}));
        harness.await_path(&harness.control_path.with_extension("reached"), phase);
        harness.stop();
        harness.clear_control();
        let recovered = durable_snapshot(&root);
        assert_complete_remember_recovery(&prior, &recovered);
        if phase == "before_transaction" {
            assert_eq!(recovered, prior);
        }
        harness.restart();
        let old = harness.call("tower_memory_get", json!({"ids":[baseline["id"]]}));
        assert_eq!(old["items"][0]["code"], "ok");
        let reconciled = harness.remember("interrupted", "transaction published", "crash-key");
        assert_eq!(reconciled["durable"], true, "{phase}: {reconciled}");
        harness.restart();
        let persisted = harness.call("tower_memory_get", json!({"ids":[reconciled["id"]]}));
        assert_eq!(persisted["items"][0]["code"], "ok");
    }
}

#[test]
fn supervised_storage_host_calls_reject_unauthorized_extensions_before_adapter_access() {
    let mut harness = MemoryMcpHarness::start();
    let baseline = harness.remember("authorization baseline", "unchanged", "auth-baseline");
    harness.stop();
    let root = harness.workspace.path().join(".tower/agent_memory");
    let before = durable_snapshot(&root);
    harness.restart();
    let trace = harness.control_path.with_extension("access");
    fs::write(&trace, "").unwrap();
    let undeclared = harness.call(
        "tower_test_helper_agent_memory_undeclared",
        json!({"exit_after_probe":true}),
    );
    assert_eq!(
        undeclared["denials"],
        json!([
            {"code":-32603,"message":"capability 'agent_memory_storage' not declared"},
            {"code":-32603,"message":"capability 'agent_memory_storage' not declared"},
            {"code":-32603,"message":"capability 'agent_memory_storage' not declared"}
        ])
    );
    let pid = undeclared["pid"].as_u64().unwrap() as u32;
    // Observe the exited child through its supervisor so it is reaped before
    // stopping the daemon that owns it.
    let _ = harness.raw_call(
        "tools/call",
        json!({"name":"tower_test_helper_agent_memory_undeclared","arguments":{}}),
    );
    MemoryMcpHarness::assert_processes_exited(&[pid]);
    assert!(
        harness
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(&trace).unwrap(),
        "",
        "get/scan/transact counts must all be zero"
    );
    fs::remove_file(&harness.helper_started_path).unwrap();
    let forbidden = harness.raw_call(
        "tools/call",
        json!({"name":"tower_rogue_memory_probe","arguments":{}}),
    );
    assert!(forbidden["result"]["isError"] == true || forbidden["error"].is_object());
    assert!(
        !harness.helper_started_path.exists(),
        "forbidden manifest spawned a child"
    );
    assert_eq!(fs::read_to_string(&trace).unwrap(), "");
    let socket = socket_path(harness.workspace.path());
    let unrelated = independent_call(&socket, "tower_list_dir", json!({"path":""}), None);
    // Any response proves the independent public connection remains live.
    assert!(!unrelated.is_null());
    harness.stop();
    assert_eq!(
        durable_snapshot(&root),
        before,
        "unauthorized calls changed contents or generation"
    );
    harness.restart();
    fs::write(&trace, "").unwrap();
    let control = harness.remember("authorization control", "owned adapter", "auth-control");
    assert_eq!(control["durable"], true);
    assert_eq!(
        harness.call("tower_memory_get", json!({"ids":[baseline["id"]]}))["items"][0]["code"],
        "ok"
    );
    assert_eq!(harness.search("authorization", json!({}))["code"], "ok");
    let calls = fs::read_to_string(trace).unwrap();
    for operation in ["get", "scan", "transact"] {
        assert!(
            calls.lines().any(|line| line == operation),
            "disconnected {operation} spy: {calls}"
        );
    }
}

#[test]
fn public_retrieval_is_compact_filtered_bounded_and_provider_independent() {
    let mut harness = MemoryMcpHarness::start();
    let first = harness.call(
        "tower_memory_remember",
        json!({"record":record_with("observation", "café alpha", "body must not be returned", provenance("branch-a", "src/a.rs", "alpha", "tag-a", "agent-a", "client-a")), "idempotency_key":"retrieval-a"}),
    );
    let second = harness.call(
        "tower_memory_remember",
        json!({"record":record_with("decision", "café beta", "another private body", provenance("branch-b", "src/b.rs", "beta", "tag-b", "agent-b", "client-b")), "idempotency_key":"retrieval-b"}),
    );
    assert_eq!(first["code"], "ok");
    assert_eq!(second["code"], "ok");

    let search = harness.call(
        "tower_memory_search",
        json!({
            "query":"café",
            "filters":{"branches":["branch-a"],"tags":["tag-a"]},
            "page":{"limit":10,"response_bytes":4096}
        }),
    );
    assert_eq!(search["code"], "ok", "{search}");
    assert_eq!(search["persistence_mode"], "durable");
    let hits = search["hits"].as_array().expect("compact search hits");
    assert_eq!(hits.len(), 1, "{search}");
    assert!(hits.iter().all(|hit| hit.get("body").is_none()), "{search}");
    assert_eq!(hits[0]["id"], first["id"]);
    assert_eq!(hits[0]["title"], "café alpha");
    assert_eq!(
        hits[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec![
            "id",
            "paths",
            "reasons",
            "record_type",
            "revision",
            "score",
            "timestamp",
            "title",
            "truncated_fields"
        ],
        "compact hits must not expose full records: {search}"
    );
    assert!(serde_json::to_vec(&search).unwrap().len() <= 4096);

    // Every public filter field selects its distinct real record. Calling the
    // same tied query twice also makes deterministic ordering observable without
    // relying on generated IDs.
    for filters in [
        json!({"record_types":["decision"]}),
        json!({"paths":["src/b.rs"]}),
        json!({"symbols":["beta"]}),
        json!({"source_agent":"agent-b"}),
        json!({"source_client":"client-b"}),
        json!({"created_from":"1970-01-01T00:00:00Z"}),
        json!({"created_to":"2999-01-01T00:00:00Z"}),
    ] {
        let filtered = harness.call(
            "tower_memory_search",
            json!({"query":"café","filters":filters,"page":{"limit":10,"response_bytes":4096}}),
        );
        assert_eq!(filtered["code"], "ok", "{filtered}");
        assert!(
            filtered["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["id"] == second["id"]),
            "{filtered}"
        );
    }
    let tie_one = harness.search("café", json!({"limit":10,"response_bytes":4096}));
    let tie_two = harness.search("café", json!({"limit":10,"response_bytes":4096}));
    assert_eq!(tie_one["hits"], tie_two["hits"], "search tie order changed");
    assert_eq!(
        tie_one["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["id"].clone())
            .collect::<Vec<_>>(),
        vec![second["id"].clone(), first["id"].clone()],
        "equal-score hits must use newest timestamp, then ascending id"
    );

    let batch = harness.call(
        "tower_memory_get",
        json!({"ids":[second["id"].clone(), "missing", first["id"].clone()]}),
    );
    assert_eq!(batch["items"][0]["record"]["title"], "café beta");
    assert_eq!(batch["items"][1]["code"], "not_found");
    assert_eq!(batch["items"][2]["record"]["title"], "café alpha");

    let context = harness.call("tower_memory_context_for_task", json!({"query":"unmatched task", "filters":{"paths":["src/b.rs"]}, "page":{"limit":10,"response_bytes":4096}}));
    assert_eq!(context["code"], "ok", "{context}");
    assert_eq!(context["hits"][0]["id"], second["id"]);
    assert!(
        context["hits"][0]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == &json!({"kind":"metadata","field":"path"})),
        "{context}"
    );

    let empty = harness.search("absent-token", json!({"limit":10,"response_bytes":4096}));
    assert_eq!(empty["hits"], json!([]), "{empty}");
    let missing_anchor = harness.call(
        "tower_memory_timeline",
        json!({"anchor":{"kind":"record","id":"missing","before":1,"after":1}}),
    );
    assert_eq!(missing_anchor["code"], "not_found", "{missing_anchor}");
    for anchor in [
        json!({"kind":"record","id":first["id"].clone(),"before":0,"after":1}),
        json!({"kind":"query","query":"café"}),
    ] {
        let timeline = harness.call(
            "tower_memory_timeline",
            json!({"anchor":anchor,"page":{"limit":10,"response_bytes":4096}}),
        );
        assert_eq!(timeline["code"], "ok", "{timeline}");
    }
    let timeline = harness.call(
        "tower_memory_timeline",
        json!({
            "anchor":{"kind":"window","from":"1970-01-01T00:00:00Z","to":"2999-01-01T00:00:00Z"},
            "page":{"limit":10,"response_bytes":4096}
        }),
    );
    assert_eq!(timeline["code"], "ok", "{timeline}");
    assert_eq!(
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["id"].clone())
            .collect::<Vec<_>>(),
        vec![first["id"].clone(), second["id"].clone()],
        "timeline events must remain in chronological creation order"
    );
    assert!(
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event.get("body").is_none()),
        "timeline events must remain compact: {timeline}"
    );
    let too_small = harness.search("café", json!({"limit":1,"response_bytes":1}));
    assert_eq!(
        too_small["code"], "response_budget_too_small",
        "{too_small}"
    );
    let malformed = harness.raw_call(
        "tools/call",
        json!({"name":"tower_memory_search","arguments":{"query":"café","page":{"limit":0}}}),
    );
    assert_eq!(malformed["result"]["isError"], true, "{malformed}");
    assert_eq!(malformed["result"]["content"][0]["text"], "Invalid params");

    // Reopen the same durable records under a smaller compact-field limit.
    // The four two-byte code points force truncation between UTF-8 boundaries.
    let unicode = harness.remember("éééé", "utf8 boundary", "utf8-truncation");
    fs::write(
        harness.workspace.path().join(".tower/config.toml"),
        "[agent_memory.limits]\nmax_title_bytes=7\n",
    )
    .unwrap();
    harness.restart();
    let truncated = harness.search("utf8 boundary", json!({"limit":10,"response_bytes":4096}));
    let hit = truncated["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["id"] == unicode["id"])
        .unwrap();
    assert_eq!(hit["title"], "ééé");
    assert_eq!(hit["truncated_fields"], json!(["title"]));
    assert!(serde_json::to_vec(&truncated).unwrap().len() <= 4096);
}

#[test]
fn public_retrieval_pages_are_complete_until_store_generation_changes() {
    let mut harness = MemoryMcpHarness::start();
    for index in 0..4 {
        let result = harness.remember(
            &format!("paged record {index}"),
            "page body",
            &format!("page-{index}"),
        );
        assert_eq!(result["code"], "ok");
    }

    let first = harness.search("paged", json!({"limit":2,"response_bytes":4096}));
    let cursor = first["next_cursor"].clone();
    assert!(cursor.is_string(), "{first}");
    let second = harness.search(
        "paged",
        json!({"limit":2,"response_bytes":4096,"cursor":cursor}),
    );
    let ids: Vec<_> = first["hits"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["hits"].as_array().unwrap())
        .map(|hit| hit["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len());
    assert!(second["next_cursor"].is_null(), "{second}");

    let stale_page = harness.search("paged", json!({"limit":1,"response_bytes":4096}));
    let stale_cursor = stale_page["next_cursor"].clone();
    harness.remember("paged mutation", "changes generation", "page-mutation");
    let expired = harness.search(
        "paged",
        json!({"limit":1,"response_bytes":4096,"cursor":stale_cursor}),
    );
    assert_eq!(expired["code"], "cursor_expired", "{expired}");
}

#[test]
fn public_memory_privacy_filters_real_storage_logs_and_provider_requests() {
    let provider = ProviderFixture::start();
    let sentinels = [
        "REJECT-TITLE-017",
        "REJECT-BODY-017",
        "REJECT-METADATA-017",
        "REJECT-IDEMPOTENCY-017",
        "REJECT-TASKQUERY-017",
        "REDACT-MIGRATION-017",
    ];
    let redact = "REDACT-PROVIDER-017";
    let provider_result = "REDACT-RESULT-017";
    let mut harness = MemoryMcpHarness::start_with_config_and_preparation(
        &format!(
            "[[agent_memory.privacy.rules]]\nid='reject'\npattern='REJECT-[A-Z]+-017'\naction='reject'\n\n[[agent_memory.privacy.rules]]\nid='redact'\npattern='REDACT-[A-Z]+-017'\naction='redact'\nreplacement='[filtered]'\n\n[agent_memory.provider]\nname={:?}\ntimeout_ms=500\n",
            provider.endpoint()
        ),
        |workspace| {
            fs::write(workspace.join(".tower/log-input"), "REDACT-LOG-017").unwrap();
        },
    );
    assert_eq!(
        harness.search("initialize log producer", json!({}))["code"],
        "ok"
    );
    let log_capture = fs::read_to_string(harness.control_path.with_extension("logs")).unwrap();
    assert!(
        log_capture.contains("[filtered]"),
        "real logging producer did not reach capture: {log_capture}"
    );
    assert!(!log_capture.contains("REDACT-LOG-017"));
    let store_input = harness.remember(
        "real store producer",
        "REDACT-STORE-017",
        "real-store-input",
    );
    assert_eq!(store_input["code"], "ok");
    let real_store = harness.call("tower_memory_get", json!({"ids":[store_input["id"]]}));
    assert_eq!(real_store["items"][0]["record"]["body"], "[filtered]");
    // Wait for its optional enrichment before configuring the next reply.
    provider.wait_for_requests(1);
    let rejected = [
        harness.call("tower_memory_remember", json!({"record":record_with("observation", sentinels[0], "safe", provenance("branch", "src/privacy.rs", "privacy", "privacy", "agent", "client")), "idempotency_key":"safe-title"})),
        harness.call("tower_memory_remember", json!({"record":record_with("observation", "safe", sentinels[1], provenance("branch", "src/privacy.rs", "privacy", "privacy", "agent", "client")), "idempotency_key":"safe-body"})),
        harness.call("tower_memory_remember", json!({"record":record_with("observation", "safe", "safe", provenance(sentinels[2], "src/privacy.rs", "privacy", "privacy", "agent", "client")), "idempotency_key":"safe-metadata"})),
        harness.call("tower_memory_remember", json!({"record":record("safe", "safe"), "idempotency_key":sentinels[3]})),
    ];
    for (response, sentinel) in rejected.iter().zip(&sentinels[..4]) {
        assert_eq!(response["code"], "privacy_rejected", "{response}");
        assert!(
            !response.to_string().contains(sentinel),
            "privacy error reflected {sentinel}: {response}"
        );
    }
    provider.enqueue(ProviderReply::Projection(provider_result.to_owned()));
    let stored = harness.remember("privacy title", redact, "privacy-redact-key");
    assert_eq!(stored["code"], "ok", "{stored}");
    provider.wait_for_requests(2);
    thread::sleep(Duration::from_millis(25));
    let provider_bytes = provider.requests().remove(1);
    assert!(
        !provider_bytes
            .windows(redact.len())
            .any(|window| window == redact.as_bytes())
    );
    assert!(String::from_utf8_lossy(&provider_bytes).contains("[filtered]"));
    let fetched = harness.call("tower_memory_get", json!({"ids":[stored["id"].clone()]}));
    assert_eq!(fetched["items"][0]["record"]["body"], "[filtered]");

    let task_query_response = harness.raw_call(
        "tools/call",
        json!({"name":"tower_memory_context_for_task","arguments":{"query":sentinels[4],"filters":{},"page":{"limit":10,"response_bytes":4096}}}),
    );
    assert!(
        tool_payload(&task_query_response).is_ok(),
        "privacy rejection must be a fixed public payload, not a transport error: {task_query_response}"
    );
    let task_query = tool_payload(&task_query_response).unwrap();
    assert_eq!(task_query["code"], "privacy_rejected", "{task_query}");
    assert!(!task_query.to_string().contains(sentinels[4]));

    harness.stop();
    seed_real_legacy_migration(
        &harness.workspace.path().join(".tower/agent_memory"),
        sentinels[5],
    );
    harness.restart();
    let migrated = harness.call("tower_memory_get", json!({"ids":[stored["id"].clone()]}));
    assert_eq!(
        migrated["items"][0]["record"]["title"], "[filtered]",
        "{migrated}"
    );
    let public_observation = format!(
        "{rejected:?}{stored}{fetched}{migrated}{task_query}{}",
        harness.stderr()
    );
    for sentinel in &sentinels[..5] {
        assert!(
            !public_observation.contains(sentinel),
            "public/log observation leaked {sentinel}"
        );
    }
    assert!(!public_observation.contains(redact));
    harness.stop();
    let durable_bytes =
        collect_live_agent_memory_bytes(&harness.workspace.path().join(".tower/agent_memory"));
    let durable_projection = String::from_utf8_lossy(&durable_bytes);
    for sentinel in sentinels {
        assert!(
            !durable_projection.contains(sentinel),
            "durable storage leaked {sentinel}"
        );
    }
    assert!(
        !durable_projection.contains(redact),
        "durable storage leaked redacted input"
    );
    assert!(
        !durable_projection.contains(provider_result),
        "durable storage leaked raw provider result"
    );
    assert!(!durable_projection.contains("REDACT-STORE-017"));
    assert!(!durable_projection.contains("REDACT-LOG-017"));
    harness.restart();
    assert_eq!(
        harness.search("initialize backend producer", json!({}))["code"],
        "ok"
    );
    // Inject the distinct raw diagnostic at the concrete backend read seam;
    // the public error must expose only the typed fixed failure code.
    harness.set_control("unavailable:REJECT-BACKENDERROR-017");
    let backend_error = harness.search("privacy", json!({}));
    assert_eq!(
        backend_error["code"], "storage_unavailable",
        "{backend_error}"
    );
    let observations = format!(
        "{backend_error}{}{}",
        harness.stderr(),
        fs::read_to_string(harness.control_path.with_extension("logs")).unwrap()
    );
    for sentinel in [
        "REJECT-BACKENDERROR-017",
        "REDACT-LOG-017",
        "REDACT-STORE-017",
    ] {
        assert!(
            !observations.contains(sentinel),
            "external producer leaked {sentinel}"
        );
    }
}

#[test]
fn storage_startup_failures_are_isolated_and_report_configured_modes() {
    // Read-only is valid only over a compatible, already initialized durable
    // store. Prepare it through the same daemon/adapter path used in production.
    let mut read_only = MemoryMcpHarness::start();
    assert_eq!(
        read_only.search("initialize", json!({"limit":1,"response_bytes":1024}))["code"],
        "ok"
    );
    fs::write(
        read_only.workspace.path().join(".tower/config.toml"),
        "[agent_memory.storage]\nstartup_policy='read_only'\n",
    )
    .unwrap();
    read_only.restart();
    let read_only_response = read_only.raw_call(
        "tools/call",
        json!({"name":"tower_memory_search","arguments":{"query":"anything","filters":{},"page":{"limit":1,"response_bytes":1024}}}),
    );
    assert!(
        tool_payload(&read_only_response).is_ok(),
        "read-only startup must report a stable tool payload while the daemon remains live: {read_only_response}"
    );
    let read_only_result = tool_payload(&read_only_response).unwrap();
    assert_eq!(
        read_only_result["persistence_mode"], "read_only",
        "{read_only_result}"
    );
    assert_eq!(read_only_result["durable_data_unavailable"], false);
    assert_eq!(
        read_only.remember("blocked", "mutation", "read-only-key")["code"],
        "read_only"
    );
    assert!(read_only.raw_call("tools/list", json!({}))["result"]["tools"].is_array());

    for (name, prepare) in [
        ("unavailable", "file"),
        ("v0", "schema:0"),
        ("future", "schema:2"),
        ("corrupt", "schema:corrupt"),
        ("interrupted", "schema:prepared"),
    ] {
        let mut volatile = MemoryMcpHarness::start_with_config_and_preparation(
            "[agent_memory.storage]\nstartup_policy='in_memory'\n",
            |workspace| {
                let root = workspace.join(".tower/agent_memory");
                match prepare {
                    "file" => fs::write(root, "unavailable").unwrap(),
                    "schema:0" | "schema:2" | "schema:corrupt" => {
                        fs::create_dir_all(&root).unwrap();
                        fs::write(root.join("schema"), prepare.trim_start_matches("schema:"))
                            .unwrap();
                    }
                    "schema:prepared" => {
                        fs::create_dir_all(&root).unwrap();
                        fs::write(root.join("schema"), "0").unwrap();
                        fs::write(root.join("migration-checkpoint"), "prepared").unwrap();
                    }
                    unexpected => panic!("unexpected startup fixture {unexpected}"),
                }
            },
        );
        let result = volatile.remember("volatile", "fallback", &format!("volatile-{name}"));
        assert_eq!(result["code"], "ok", "{name}: {result}");
        assert_eq!(result["persistence_mode"], "in_memory", "{name}: {result}");
        assert_eq!(result["durable_data_unavailable"], true, "{name}: {result}");
        assert_eq!(
            volatile.search("volatile", json!({"limit":1,"response_bytes":1024}))["persistence_mode"],
            "in_memory"
        );
        assert!(
            volatile.raw_call("tools/list", json!({}))["result"]["tools"].is_array(),
            "{name}"
        );
    }

    // A failure after successful durable assembly is reported in the selected
    // mode; it never causes an implicit fallback or a runtime mode change.
    let mut durable = MemoryMcpHarness::start();
    let baseline = durable.remember("runtime mode", "durable", "runtime-mode");
    assert_eq!(baseline["persistence_mode"], "durable");
    durable.set_control("unavailable");
    let failed = durable.search("runtime", json!({"limit":1,"response_bytes":1024}));
    assert_eq!(failed["code"], "storage_unavailable", "{failed}");
    assert_eq!(failed["persistence_mode"], "durable", "{failed}");
    durable.clear_control();
    let recovered = durable.search("runtime", json!({"limit":1,"response_bytes":1024}));
    assert_eq!(recovered["code"], "ok", "{recovered}");
    assert_eq!(recovered["persistence_mode"], "durable", "{recovered}");
}

#[test]
fn provider_delay_and_stale_results_do_not_block_or_restore_baseline_memory() {
    let provider = ProviderFixture::start();
    let (held, release) = ProviderFixture::held("held-provider-projection");
    provider.enqueue(held);
    let mut harness = MemoryMcpHarness::start_with_config(&format!(
        "[agent_memory.provider]\nname={:?}\ntimeout_ms=500\n",
        provider.endpoint()
    ));
    let remembered = harness.remember(
        "baseline provider independent",
        "local body",
        "provider-base",
    );
    provider.wait_for_requests(1);
    let captured = String::from_utf8_lossy(&provider.requests()[0]).into_owned();
    assert!(
        captured.contains("baseline provider independent"),
        "{captured}"
    );
    let baseline = harness.search("baseline", json!({"limit":10,"response_bytes":4096}));
    assert_eq!(baseline["code"], "ok", "{baseline}");
    assert_eq!(baseline["hits"][0]["id"], remembered["id"]);
    let updated = harness.call(
        "tower_memory_update",
        json!({"items":[{"id":remembered["id"].clone(),"expected_revision":"1","record":record("baseline updated", "local body two")}]})
    );
    assert_eq!(updated["items"][0]["code"], "ok", "{updated}");
    ProviderFixture::release(&release);
    thread::sleep(Duration::from_millis(25));
    let after_timeout = harness.search("baseline", json!({"limit":10,"response_bytes":4096}));
    assert_eq!(after_timeout["code"], "ok", "{after_timeout}");
    assert_eq!(after_timeout["hits"][0]["title"], "baseline updated");
    assert_eq!(
        harness.search(
            "held-provider-projection",
            json!({"limit":10,"response_bytes":4096})
        )["hits"],
        json!([]),
        "a response released after update restored a stale projection"
    );

    provider.enqueue(ProviderReply::Unavailable);
    let failure = harness.remember("provider failure baseline", "local", "provider-failure");
    provider.wait_for_requests(2);
    thread::sleep(Duration::from_millis(25));
    assert_eq!(
        harness.call("tower_memory_get", json!({"ids":[failure["id"].clone()]}))["items"][0]["code"],
        "ok"
    );

    let forgotten = harness.call(
        "tower_memory_forget",
        json!({"items":[{"id":remembered["id"].clone(),"expected_revision":"2"}]}),
    );
    assert_eq!(forgotten["items"][0]["code"], "ok", "{forgotten}");
    harness.restart();
    let after_restart = harness.search(
        "baseline updated",
        json!({"limit":10,"response_bytes":4096}),
    );
    assert!(
        after_restart["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["id"] != remembered["id"]),
        "forgotten record was restored: {after_restart}"
    );
    assert!(!after_restart.to_string().contains(provider.endpoint()));

    let (held, _never_release) = ProviderFixture::held("late-provider-projection");
    provider.enqueue(held);
    let timeout = harness.remember("provider timeout baseline", "local", "provider-timeout");
    provider.wait_for_requests(3);
    thread::sleep(Duration::from_millis(550));
    assert_eq!(
        harness.call("tower_memory_get", json!({"ids":[timeout["id"].clone()]}))["items"][0]["code"],
        "ok"
    );
}

#[test]
fn agent_memory_tools_are_discovered_through_the_standard_merge() {
    let mut harness = MemoryMcpHarness::start();
    let listed = harness.raw_call("tools/list", json!({}));
    let names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .filter(|name| name.starts_with("tower_memory_"))
        .collect();
    assert_eq!(names.len(), MEMORY_TOOLS.len(), "{listed}");
    for expected in MEMORY_TOOLS {
        assert_eq!(
            names.iter().filter(|name| **name == expected).count(),
            1,
            "{listed}"
        );
        assert!(
            !names
                .iter()
                .any(|name| name.contains("agent_memory_memory"))
        );
    }

    let remembered = harness.remember("discovery", "route", "discovery-key");
    assert_eq!(remembered["code"], "ok");
    assert_eq!(
        harness.call(
            "tower_memory_get",
            json!({"ids":[remembered["id"].clone()]})
        )["items"][0]["code"],
        "ok"
    );
    assert_eq!(
        harness.search("discovery", json!({"limit":1,"response_bytes":4096}))["code"],
        "ok"
    );
    assert_eq!(
        harness.call(
            "tower_memory_context_for_task",
            json!({"query":"discovery"})
        )["code"],
        "ok"
    );
    assert_eq!(
        harness.call(
            "tower_memory_timeline",
            json!({"anchor":{"kind":"query","query":"discovery"}})
        )["code"],
        "ok"
    );
    assert_eq!(harness.call("tower_memory_update", json!({"items":[{"id":remembered["id"].clone(),"expected_revision":"1","record":record("discovery updated", "route")}]}))["items"][0]["code"], "ok");
    assert_eq!(
        harness.call(
            "tower_memory_forget",
            json!({"items":[{"id":remembered["id"].clone(),"expected_revision":"2"}]})
        )["items"][0]["code"],
        "ok"
    );
}

#[test]
fn agent_memory_host_frames_survive_twenty_parallel_public_sessions() {
    let mut harness = MemoryMcpHarness::start();
    assert!(harness.raw_call("tools/list", json!({}))["result"]["tools"].is_array());
    for repetition in 0..20 {
        harness.set_control("read");
        let socket = socket_path(harness.workspace.path());
        let sessions: Vec<_> = (0..20)
            .map(|session| {
                let socket = socket.clone();
                thread::spawn(move || {
                    let mut stream = UnixStream::connect(socket).expect("parallel MCP session");
                    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    stream.write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes()).unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":format!("parallel-{repetition}-{session}"),"version":"1"}}});
                    writeln!(stream, "{initialize}").unwrap();
                    stream.flush().unwrap();
                    loop {
                        let mut line = String::new();
                        assert!(reader.read_line(&mut line).unwrap() > 0);
                        let response: Value = serde_json::from_str(line.trim()).unwrap();
                        if response["id"] == 1 {
                            assert!(response["result"].is_object(), "{response}");
                            break;
                        }
                    }
                    // Pipeline multiple host requests while the first memory
                    // invocation is deterministically blocked in a HostCall.
                    // Responses may arrive in either order, but no ID may be
                    // lost or answered twice.
                    for frame in [
                        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tower_memory_search","arguments":{"query":"parallel","filters":{},"page":{"limit":1,"response_bytes":1024}}}}),
                        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tower_memory_search","arguments":{"query":"queued","filters":{},"page":{"limit":1,"response_bytes":1024}}}}),
                        json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}),
                    ] {
                        writeln!(stream, "{frame}").unwrap();
                    }
                    stream.flush().unwrap();
                    let mut answered = std::collections::BTreeSet::new();
                    while answered.len() < 3 {
                        let mut line = String::new();
                        assert!(reader.read_line(&mut line).unwrap() > 0);
                        let response: Value = serde_json::from_str(line.trim()).unwrap();
                        let id = response["id"].as_u64().expect("response id");
                        assert!((2..=4).contains(&id), "{response}");
                        assert!(answered.insert(id), "duplicate response id {id}");
                        assert!(response["result"].is_object(), "{response}");
                    }
                })
            })
            .collect();
        harness.await_path(
            &harness.control_path.with_extension("reached"),
            "no real storage HostCall reached the deterministic hold",
        );
        fs::write(harness.control_path.with_extension("release"), "release").unwrap();
        for session in sessions {
            session.join().expect("parallel session must not deadlock");
        }
        harness.clear_control();
        assert!(harness.raw_call("tools/list", json!({}))["result"]["tools"].is_array());
    }
    let processes = harness.extension_processes();
    harness.stop();
    MemoryMcpHarness::assert_processes_exited(&processes);
}

#[test]
fn durable_memory_survives_twenty_extension_restarts() {
    let mut harness = MemoryMcpHarness::start();
    let remembered = harness.remember("twenty restarts", "durable", "restart-twenty");
    assert_eq!(remembered["durable"], true);
    for _ in 0..20 {
        let old_processes = harness.extension_processes();
        harness.restart();
        MemoryMcpHarness::assert_processes_exited(&old_processes);
        let fetched = harness.call(
            "tower_memory_get",
            json!({"ids":[remembered["id"].clone()]}),
        );
        assert_eq!(fetched["items"][0]["code"], "ok", "{fetched}");
        assert_eq!(fetched["items"][0]["record"]["title"], "twenty restarts");
    }
}
