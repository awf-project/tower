#![cfg(unix)]

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use core_engine::adapters::daemon::socket::socket_path;
use core_engine::adapters::daemon::wire::{ClientRole, Handshake, read_line_capped, write_line};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;

const INVALID_CWD: &str = "working directory must be an existing directory within the workspace";

fn fixture_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("tower-shell-fixture-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("fixture.rs");
        let binary = root.join("fixture");
        fs::write(
            &source,
            r#"
use std::{
    env, fs,
    io::{self, Write},
    process::{Command, exit},
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};
fn main() {
    let mut a = env::args().skip(1);
    let mode = a.next().unwrap();
    match mode.as_str() {
        "argv" => {
            println!("{}", a.collect::<Vec<_>>().join("|"));
            eprintln!("argv-stderr");
        }
        "cwd" => {
            let cwd = env::current_dir().unwrap();
            fs::write(a.next().unwrap(), cwd.to_string_lossy().as_bytes()).unwrap();
            println!("{}", cwd.display())
        }
        "output" => {
            let o: usize = a.next().unwrap().parse().unwrap();
            let e: usize = a.next().unwrap().parse().unwrap();
            let gate = Arc::new(Barrier::new(2));
            let stdout_gate = Arc::clone(&gate);
            let stdout = thread::spawn(move || {
                stdout_gate.wait();
                let mut out = io::stdout().lock();
                out.write_all(&vec![b'O'; o]).unwrap();
                out.flush().unwrap();
            });
            gate.wait();
            let mut err = io::stderr().lock();
            err.write_all(&vec![b'E'; e]).unwrap();
            err.flush().unwrap();
            stdout.join().unwrap();
        }
        "exit" => {
            print!("exit-stdout");
            eprint!("exit-stderr");
            exit(17);
        }
        "mark" => fs::write(a.next().unwrap(), b"spawned").unwrap(),
        "privacy" => {
            let m = a.next().unwrap();
            fs::write(m, b"ran").unwrap();
            print!("{}", a.next().unwrap());
        }
        "sleep" => {
            let pm = a.next().unwrap();
            let cm = a.next().unwrap();
            fs::write(pm, format!("{}", std::process::id())).unwrap();
            let _child = Command::new(env::current_exe().unwrap())
                .args(["child", &cm])
                .spawn()
                .unwrap();
            thread::sleep(Duration::from_secs(60));
        }
        "child" => {
            fs::write(a.next().unwrap(), format!("{}", std::process::id())).unwrap();
            thread::sleep(Duration::from_secs(60));
        }
        _ => {
            let _ = io::stderr().write_all(b"bad mode");
            exit(2);
        }
    }
}
"#,
        )
        .unwrap();
        let status = Command::new("rustc")
            .args(["--edition=2024", "-o"])
            .arg(&binary)
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());
        binary
    })
}

struct Daemon {
    workspace: TempDir,
    child: Child,
    stderr: Option<thread::JoinHandle<String>>,
}
impl Daemon {
    async fn start(shell: Option<&str>) -> (Self, Session) {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join(".tower")).unwrap();
        fs::write(
            workspace.path().join(".tower/config.toml"),
            format!("[daemon]\nidle_timeout_secs = 300\n{}", shell.unwrap_or("")),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tower"))
            .args(["daemon", "--workspace-dir"])
            .arg(workspace.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", workspace.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stderr.take().unwrap();
        let stderr = thread::spawn(move || {
            let mut s = String::new();
            pipe.read_to_string(&mut s).unwrap();
            s
        });
        let session = Session::connect(workspace.path()).await;
        (
            Self {
                workspace,
                child,
                stderr: Some(stderr),
            },
            session,
        )
    }
    fn path(&self) -> &Path {
        self.workspace.path()
    }
    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = Command::new(env!("CARGO_BIN_EXE_tower"))
                .args(["shutdown", "--workspace-dir"])
                .arg(self.path())
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", self.path())
                .status();
            let _ = self.child.wait();
        }
    }
    fn captured_stderr(mut self) -> String {
        self.stop();
        self.stderr.take().unwrap().join().unwrap()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Session {
    stream: UnixStream,
    id: u64,
}
impl Session {
    async fn connect(workspace: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match UnixStream::connect(socket_path(workspace)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(e) => panic!("connect daemon: {e}"),
            }
        };
        stream
            .write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
            .await
            .unwrap();
        write_line(&mut stream,r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"shell-tests","version":"1"}}}"#).await.unwrap();
        let r = read_line_capped(&mut stream, 1 << 20)
            .await
            .unwrap()
            .unwrap();
        assert!(serde_json::from_str::<Value>(&r).unwrap()["result"].is_object());
        write_line(
            &mut stream,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await
        .unwrap();
        Self { stream, id: 2 }
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.id;
        self.id += 1;
        write_line(
            &mut self.stream,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
        )
        .await
        .unwrap();
        let line = tokio::time::timeout(
            Duration::from_secs(8),
            read_line_capped(&mut self.stream, 1 << 20),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id);
        v
    }
    async fn list(&mut self) -> Value {
        self.request("tools/list", json!({})).await
    }
    async fn call(&mut self, args: Value) -> Value {
        self.request(
            "tools/call",
            json!({"name":"tower_run_command","arguments":args}),
        )
        .await
    }
}
fn shell(enabled: bool, argv: &[&str], timeout: u64, stdout: u64, stderr: u64) -> String {
    let argv = argv
        .iter()
        .map(|v| format!("{v:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[shell]\nenabled = {enabled}\ntimeout_secs = {timeout}\nstdout_max_bytes = {stdout}\nstderr_max_bytes = {stderr}\n[[shell.commands]]\nname = \"fixture\"\nargv = [{argv}]\n"
    )
}
fn payload(r: &Value) -> Value {
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn tool_count(v: &Value) -> usize {
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["name"] == "tower_run_command")
        .count()
}
fn invalid(v: &Value, m: &str) {
    assert_eq!(v["error"]["code"], -32602);
    assert_eq!(v["error"]["message"], m)
}
fn marker(p: &Path, n: &str) -> PathBuf {
    p.join(".tower").join(n)
}
fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}
fn string_occurrences(value: &Value, needle: &str) -> usize {
    match value {
        Value::String(text) => text.match_indices(needle).count(),
        Value::Array(values) => values
            .iter()
            .map(|value| string_occurrences(value, needle))
            .sum(),
        Value::Object(values) => values
            .values()
            .map(|value| string_occurrences(value, needle))
            .sum(),
        _ => 0,
    }
}
fn assert_output_only_in_stdout_response(response: &Value, sentinel: &str) {
    let content = response["result"]["content"]
        .as_array()
        .expect("successful tool response must contain content");
    assert_eq!(
        content.len(),
        1,
        "only the documented result content is expected"
    );
    let encoded = content[0]["text"]
        .as_str()
        .expect("result content must be text");
    let mut decoded: Value = serde_json::from_str(encoded).expect("result content must be JSON");
    assert_eq!(decoded["stdout"]["text"], sentinel);

    let mut without_documented_field = response.clone();
    without_documented_field["result"]["content"][0]["text"] = Value::String(String::new());
    assert_eq!(
        string_occurrences(&without_documented_field, sentinel),
        0,
        "output sentinel must not occur outside the documented result field"
    );
    decoded["stdout"]["text"] = Value::String(String::new());
    assert_eq!(
        string_occurrences(&decoded, sentinel),
        0,
        "decoded output sentinel must occur only in stdout.text"
    );
}

pub(crate) async fn disabled_shell_tool_is_absent_from_discovery() {
    let (_a, mut s) = Daemon::start(None).await;
    assert_eq!(tool_count(&s.list().await), 0);
    let c = shell(
        false,
        &[fixture_binary().to_str().unwrap(), "argv"],
        2,
        64,
        64,
    );
    let (_d, mut s) = Daemon::start(Some(&c)).await;
    assert_eq!(tool_count(&s.list().await), 0)
}
pub(crate) async fn enabled_shell_tool_executes_allowed_command() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "argv"],
        2,
        128,
        128,
    );
    let (_d, mut s) = Daemon::start(Some(&c)).await;
    assert_eq!(tool_count(&s.list().await), 1);
    let p = payload(
        &s.call(json!({"command":"fixture","args":["literal one","$NO_EXPANSION"]}))
            .await,
    );
    assert_eq!(
        p,
        json!({"status":{"kind":"exited","code":0},"stdout":{"kind":"text","text":"literal one|$NO_EXPANSION\n","byte_count":26,"truncated":false},"stderr":{"kind":"text","text":"argv-stderr\n","byte_count":12,"truncated":false},"timed_out":false})
    )
}
pub(crate) async fn shell_command_uses_default_and_contained_working_directories() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "cwd"],
        2,
        512,
        64,
    );
    let (d, mut s) = Daemon::start(Some(&c)).await;
    fs::create_dir(d.path().join("nested")).unwrap();
    let root_marker = marker(d.path(), "cwd-root");
    let nested_marker = marker(d.path(), "cwd-nested");
    let r = payload(
        &s.call(json!({"command":"fixture","args":[root_marker.clone()]}))
            .await,
    )["stdout"]["text"]
        .as_str()
        .unwrap()
        .trim()
        .to_owned();
    let n = payload(
        &s.call(json!({"command":"fixture","args":[nested_marker.clone()],"cwd":"nested"}))
            .await,
    )["stdout"]["text"]
        .as_str()
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(Path::new(&r), d.path().canonicalize().unwrap());
    assert_eq!(
        Path::new(&n),
        d.path().join("nested").canonicalize().unwrap()
    );
    assert_ne!(r, n);
    assert_eq!(fs::read_to_string(root_marker).unwrap(), r);
    assert_eq!(fs::read_to_string(nested_marker).unwrap(), n)
}
pub(crate) async fn escaping_working_directory_is_rejected_before_spawn() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "mark"],
        2,
        64,
        64,
    );
    let (d, mut s) = Daemon::start(Some(&c)).await;
    fs::write(d.path().join("file"), b"x").unwrap();
    let out = tempfile::tempdir().unwrap();
    symlink(out.path(), d.path().join("link")).unwrap();
    for (i, cwd) in [
        out.path().to_string_lossy().to_string(),
        "../escape".into(),
        "missing".into(),
        "file".into(),
        "link".into(),
    ]
    .into_iter()
    .enumerate()
    {
        let m = marker(d.path(), &format!("cwd-{i}"));
        invalid(
            &s.call(json!({"command":"fixture","args":[m],"cwd":cwd}))
                .await,
            INVALID_CWD,
        );
        assert!(!m.exists())
    }
}
pub(crate) async fn long_running_shell_command_is_terminated_after_timeout() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "sleep"],
        1,
        64,
        64,
    );
    let (d, mut s) = Daemon::start(Some(&c)).await;
    let pm = marker(d.path(), "parent");
    let cm = marker(d.path(), "child");
    let start = Instant::now();
    let p = payload(
        &s.call(json!({"command":"fixture","args":[pm.clone(),cm.clone()]}))
            .await,
    );
    assert!(start.elapsed() <= Duration::from_secs(3));
    assert_eq!(p["timed_out"], true);
    let parent_pid = fs::read_to_string(pm).unwrap();
    let child_pid = fs::read_to_string(cm).unwrap();
    assert_ne!(parent_pid.trim(), child_pid.trim());
    assert!(!alive(parent_pid.trim()));
    assert!(!alive(child_pid.trim()))
}
pub(crate) async fn shell_command_output_is_independently_bounded() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "output"],
        2,
        17,
        11,
    );
    let (_d, mut s) = Daemon::start(Some(&c)).await;
    let p = payload(
        &s.call(json!({"command":"fixture","args":["80","70"]}))
            .await,
    );
    assert_eq!(p["stdout"]["text"], "O".repeat(17));
    assert_eq!(p["stdout"]["byte_count"], 17);
    assert_eq!(p["stderr"]["text"], "E".repeat(11));
    assert_eq!(p["stderr"]["byte_count"], 11);
    assert_eq!(p["stdout"]["truncated"], true);
    assert_eq!(p["stderr"]["truncated"], true)
}
pub(crate) async fn shell_failures_and_logs_do_not_disclose_private_inputs() {
    let a = "PRIVATE_ARG_SENTINEL";
    let o = "PRIVATE_OUTPUT_SENTINEL";
    let x = "PRIVATE_EXECUTABLE_SENTINEL";
    let os = "No such file or directory";
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "privacy"],
        2,
        256,
        64,
    );
    let (d, mut s) = Daemon::start(Some(&c)).await;
    let m = marker(d.path(), "privacy-ran");
    let r = s
        .call(json!({"command":"fixture","args":[m.clone(),o,a]}))
        .await;
    assert!(m.exists());
    assert_output_only_in_stdout_response(&r, o);
    assert!(!r.to_string().contains(a));
    drop(s);
    let logs = d.captured_stderr();
    for q in [a, o, x, os] {
        assert!(!logs.contains(q))
    }
    let bad = shell(true, &[&format!("/missing/{x}")], 2, 64, 64);
    let (d, mut s) = Daemon::start(Some(&bad)).await;
    let r = s.call(json!({"command":"fixture","args":[a]})).await;
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(
        r["result"]["content"],
        json!([{"type":"text","text":"execution failed: command process could not be started"}])
    );
    for q in [a, x, os] {
        assert!(!r.to_string().contains(q))
    }
    drop(s);
    let logs = d.captured_stderr();
    assert!(!logs.contains(x));
    assert!(!logs.contains(os))
}
pub(crate) async fn unsuccessful_shell_command_preserves_exit_status_and_output() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "exit"],
        2,
        64,
        64,
    );
    let (_d, mut s) = Daemon::start(Some(&c)).await;
    let p = payload(&s.call(json!({"command":"fixture"})).await);
    assert_eq!(p["status"], json!({"kind":"exited","code":17}));
    assert_eq!(p["stdout"]["text"], "exit-stdout");
    assert_eq!(p["stderr"]["text"], "exit-stderr");
    assert_eq!(p["timed_out"], false)
}
pub(crate) async fn malformed_shell_arguments_are_rejected_before_spawn() {
    let c = shell(
        true,
        &[fixture_binary().to_str().unwrap(), "mark"],
        2,
        64,
        64,
    );
    let (d, mut s) = Daemon::start(Some(&c)).await;
    let m = marker(d.path(), "malformed");
    for a in [
        json!({"args":[m.clone()]}),
        json!({"command":"fixture","args":"bad"}),
        json!({"command":"fixture","args":[m.clone()],"unknown":true}),
    ] {
        invalid(&s.call(a).await, "invalid tower_run_command arguments")
    }
    assert!(!m.exists())
}
