//! Regression fixture: an extension that completes the `initialize` handshake
//! but then **ignores every subsequent request — including `shutdown`** — and
//! never exits voluntarily.
//!
//! Purpose: exercise `SidecarHostAdapter::shutdown()`'s kill-escalation path. A
//! correct host must reap this process via SIGTERM/SIGKILL and return from
//! `shutdown()` within its grace budget, even though the extension never honours
//! the cooperative `shutdown` request. A host that joins its reader thread
//! *before* killing the child deadlocks here (the reader never sees stdout EOF).
//!
//! Safety valve: a watchdog thread force-exits the process after 20s so that a
//! *buggy* host which deadlocks in `shutdown()` fails the regression test by
//! timing out (~20s) rather than wedging the whole test binary forever. A
//! correct host kills us in ~2s, well before the watchdog fires.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use extension_protocol::{InitParams, InitResult, PROTOCOL_VERSION, Response};

const WATCHDOG: Duration = Duration::from_secs(20);

fn main() {
    // Watchdog: never outlive the test even if the host leaks us without killing.
    std::thread::spawn(|| {
        std::thread::sleep(WATCHDOG);
        std::process::exit(0);
    });

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut lines = stdin.lock().lines();

    // Answer exactly the `initialize` handshake so the host's `spawn` succeeds.
    for line in lines.by_ref() {
        let line = match line {
            Ok(l) => l.trim().to_owned(),
            Err(_) => return,
        };
        if line.is_empty() {
            continue;
        }
        let envelope: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if envelope.get("method").and_then(|v| v.as_str()) != Some("initialize") {
            continue; // ignore pre-initialize noise
        }

        let id = envelope.get("id").cloned();
        let params_val = envelope
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let init_params: InitParams = match serde_json::from_value(params_val) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if init_params.protocol_version != PROTOCOL_VERSION {
            continue;
        }

        let result = InitResult {
            tools: vec![],
            events: vec![],
            capabilities: vec![],
        };
        send_response(&mut out, &id, &Response::Initialized(result));
        out.flush().expect("flush Initialized");
        break;
    }

    // From here on: deliberately ignore ALL further input (including `shutdown`)
    // and NEVER respond or exit. We keep draining stdin so the host's writes do
    // not raise SIGPIPE, but we send nothing back and hold stdout open — forcing
    // the host to kill us. The watchdog is the only voluntary exit.
    for _line in lines {
        // discard
    }

    // stdin reached EOF (host dropped it) — still refuse to exit promptly; park
    // so the host's kill escalation is what actually reaps us.
    std::thread::park_timeout(WATCHDOG);
}

fn send_response(out: &mut impl Write, id: &Option<serde_json::Value>, resp: &Response) {
    let result = serde_json::to_value(resp).expect("serialize Response");
    let envelope = match id {
        Some(id_val) => serde_json::json!({"jsonrpc": "2.0", "id": id_val, "result": result}),
        None => serde_json::json!({"jsonrpc": "2.0", "result": result}),
    };
    let s = serde_json::to_string(&envelope).expect("serialize envelope");
    writeln!(out, "{s}").expect("write envelope");
}
