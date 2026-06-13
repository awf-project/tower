//! Per-plugin mailbox worker (Approach A).
//!
//! Each plugin instance is owned exclusively by one worker thread that drains a
//! bounded `Command` channel. Tool calls are served before hook deliveries
//! (stable priority), so interactive MCP calls stay responsive during file
//! churn. See `docs/superpowers/specs/2026-06-13-internal-plugin-queue-design.md`.
//!
//! The registry refactor (Task 2) is the only crate-internal consumer of
//! `Command`, `PLUGIN_MAILBOX_CAPACITY`, and `run_worker`; until it lands they
//! are exercised solely by this module's tests, hence the module-level
//! `dead_code` allowance.
// TODO(task-2): remove once the registry consumes Command/run_worker/PLUGIN_MAILBOX_CAPACITY
#![allow(dead_code)]

use std::sync::mpsc::{Receiver, Sender};

use plugin_sdk::{HookKind, HookPayload, Value};

use super::{PluginFaultKind, PluginHostError, PluginInstance};

/// Bounded mailbox capacity per plugin (Decision Cap). One large checkout burst
/// fits; each queued item is a path-sized payload so memory is negligible.
pub(crate) const PLUGIN_MAILBOX_CAPACITY: usize = 1024;

/// A unit of work for a plugin worker.
pub(crate) enum Command {
    /// Fire-and-forget lifecycle hook.
    Hook {
        kind: HookKind,
        payload: HookPayload,
    },
    /// MCP tool call; the result is sent back on `reply`.
    CallTool {
        tool: String,
        args: Value,
        reply: Sender<Result<Value, PluginHostError>>,
    },
}

impl Command {
    /// Lower number = served first. Tool calls outrank hooks (Decision C).
    fn priority(&self) -> u8 {
        match self {
            Command::CallTool { .. } => 0,
            Command::Hook { .. } => 1,
        }
    }
}

/// Drive one plugin instance until all senders are dropped.
///
/// Blocks on `recv`; once woken, drains every immediately-available command into
/// a batch, stable-sorts it tool-calls-first, then processes it. A quarantined
/// plugin switches to drain-and-discard: hooks are dropped (so the mailbox never
/// stays full and backpressures the watcher), tool calls reply with the fault.
pub(crate) fn run_worker(instance: &mut dyn PluginInstance, rx: Receiver<Command>, name: &str) {
    let mut quarantined = false;
    loop {
        let first = match rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => break, // all senders dropped, buffer empty → exit
        };
        let mut batch = vec![first];
        while let Ok(cmd) = rx.try_recv() {
            batch.push(cmd);
        }
        batch.sort_by_key(Command::priority); // stable: FIFO preserved within class

        for cmd in batch {
            match cmd {
                Command::CallTool { tool, args, reply } => {
                    let result = if quarantined {
                        Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined))
                    } else {
                        instance.call_tool(&tool, args)
                    };
                    let _ = reply.send(result); // caller may have dropped the receiver
                }
                Command::Hook { kind, payload } => {
                    if quarantined {
                        continue; // drain-and-discard
                    }
                    if let Err(e) = instance.deliver_hook(kind.clone(), payload) {
                        eprintln!("[tower] plugin '{name}' hook delivery error ({kind:?}): {e}");
                        if matches!(
                            e,
                            PluginHostError::PluginFault(PluginFaultKind::Quarantined)
                        ) {
                            quarantined = true;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, sync_channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use plugin_sdk::{ABI_VERSION, HookKind, HookPayload, PluginManifest, Value};

    use super::*;
    use crate::domain::plugin_host::{PluginFaultKind, PluginHostError, PluginInstance};

    /// Records every hook payload it receives, in order. Optionally sleeps to
    /// simulate a slow plugin. Optionally reports itself quarantined.
    struct ProbePlugin {
        manifest: PluginManifest,
        delivered: Arc<Mutex<Vec<String>>>,
        sleep: Duration,
        quarantine_after: Option<usize>,
        seen: Arc<Mutex<usize>>,
    }

    impl ProbePlugin {
        fn new(hooks: Vec<HookKind>, delivered: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                manifest: PluginManifest {
                    name: "probe".to_owned(),
                    version: "0.1.0".to_owned(),
                    abi: ABI_VERSION,
                    tools: vec![],
                    hooks,
                },
                delivered,
                sleep: Duration::ZERO,
                quarantine_after: None,
                seen: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl PluginInstance for ProbePlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }
        fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value, PluginHostError> {
            self.delivered.lock().unwrap().push("<tool>".to_owned());
            Ok(Value::Null)
        }
        fn deliver_hook(
            &mut self,
            _kind: HookKind,
            payload: HookPayload,
        ) -> Result<(), PluginHostError> {
            if self.sleep > Duration::ZERO {
                std::thread::sleep(self.sleep);
            }
            let mut seen = self.seen.lock().unwrap();
            *seen += 1;
            if let Some(limit) = self.quarantine_after
                && *seen > limit
            {
                return Err(PluginHostError::PluginFault(PluginFaultKind::Quarantined));
            }
            let path = match payload {
                HookPayload::FileChanged { path } | HookPayload::FileIndexed { path } => path,
                other => panic!("unexpected hook payload: {other:?}"),
            };
            self.delivered.lock().unwrap().push(path);
            Ok(())
        }
    }

    fn changed(path: &str) -> Command {
        Command::Hook {
            kind: HookKind::FileChanged,
            payload: HookPayload::FileChanged {
                path: path.to_owned(),
            },
        }
    }

    #[test]
    fn hooks_are_delivered_in_fifo_order() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = ProbePlugin::new(vec![HookKind::FileChanged], Arc::clone(&delivered));
        let (tx, rx) = sync_channel::<Command>(PLUGIN_MAILBOX_CAPACITY);

        let handle = std::thread::spawn(move || run_worker(&mut plugin, rx, "probe"));
        for i in 0..50 {
            tx.send(changed(&format!("f{i}.rs"))).unwrap();
        }
        drop(tx); // disconnect → worker drains then exits
        handle.join().unwrap();

        let got = delivered.lock().unwrap().clone();
        let want: Vec<String> = (0..50).map(|i| format!("f{i}.rs")).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn tool_calls_are_served_before_queued_hooks() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = ProbePlugin::new(vec![HookKind::FileChanged], Arc::clone(&delivered));
        plugin.sleep = Duration::from_millis(20);
        let (tx, rx) = sync_channel::<Command>(PLUGIN_MAILBOX_CAPACITY);

        // Fill the whole batch BEFORE the worker runs. The priority guarantee
        // only holds within a single drain; if the worker's blocking `recv`
        // returned a hook before the tool call was queued, that hook would run
        // first and the test would flake. With every command already buffered,
        // the first drain sees the whole batch and the stable sort puts the
        // single tool call first, deterministically.
        for i in 0..5 {
            tx.send(changed(&format!("f{i}.rs"))).unwrap();
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(Command::CallTool {
            tool: "noop".to_owned(),
            args: Value::Null,
            reply: reply_tx,
        })
        .unwrap();
        drop(tx);

        let handle = std::thread::spawn(move || run_worker(&mut plugin, rx, "probe"));

        let reply = reply_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(reply, Ok(Value::Null)));
        handle.join().unwrap();

        // The tool call recorded "<tool>" into the same ordered log the hooks
        // use. It must appear first, with the hooks following in FIFO order.
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got.first().map(String::as_str), Some("<tool>"));
        let want: Vec<String> = std::iter::once("<tool>".to_owned())
            .chain((0..5).map(|i| format!("f{i}.rs")))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn quarantined_plugin_discards_hooks_without_blocking() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = ProbePlugin::new(vec![HookKind::FileChanged], Arc::clone(&delivered));
        plugin.quarantine_after = Some(2);
        let (tx, rx) = sync_channel::<Command>(4);
        let handle = std::thread::spawn(move || run_worker(&mut plugin, rx, "probe"));

        for i in 0..100 {
            tx.send(changed(&format!("f{i}.rs"))).unwrap();
        }
        drop(tx);
        handle.join().unwrap();

        assert_eq!(delivered.lock().unwrap().len(), 2);
    }

    #[test]
    fn dropping_sender_drains_then_exits() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = ProbePlugin::new(vec![HookKind::FileChanged], Arc::clone(&delivered));
        let (tx, rx) = sync_channel::<Command>(PLUGIN_MAILBOX_CAPACITY);
        let handle = std::thread::spawn(move || run_worker(&mut plugin, rx, "probe"));
        tx.send(changed("a.rs")).unwrap();
        tx.send(changed("b.rs")).unwrap();
        drop(tx);
        assert!(handle.join().is_ok());
        assert_eq!(delivered.lock().unwrap().len(), 2);
    }
}
