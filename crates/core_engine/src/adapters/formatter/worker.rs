//! Format worker — runs external tool and writes result atomically (spec 13a).
//!
//! # Filter contract (default)
//!
//! The tool reads the file bytes from stdin and writes formatted output to
//! stdout. Example: `rustfmt --edition 2021`.
//!
//! # Inplace contract
//!
//! The tool writes the formatted file in place on a temp copy provided by the
//! host. The `{path}` placeholder in `argv` is substituted with the temp copy
//! path. The host reads the temp copy back and performs the atomic write.
//! Example: `php-cs-fixer fix {path}`.
//!
//! # Atomic write pattern (shadow → fsync → rename)
//!
//! The worker replicates the domain shadow-file pattern directly with `std::fs`
//! because the domain `FileMutationService` requires mutable borrows to
//! `ProjectWorkspace` and `InvertedIndex` — structures the background worker
//! cannot safely hold. The watcher observes the atomic rename and updates the
//! VFS/index via its normal `EventProcessor` path.
//!
//! # CAS (optimistic concurrency)
//!
//! Before writing the formatted output the worker re-reads the file and hashes
//! it. If the hash differs from the pre-read hash `h0`, a concurrent writer has
//! modified the file and the formatted output would overwrite newer content. In
//! that case the write is aborted and the newer content is preserved.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::adapters::config::{FormatterConfig, FormatterMode};
use crate::adapters::fs::durable_write;
use extension_protocol::TimeoutDetail;

// ── FormatJob ─────────────────────────────────────────────────────────────────

/// Describes a single format job (not currently persisted, used for clarity).
#[derive(Clone, Debug)]
pub struct FormatJob {
    pub path: String,
    pub timeout: Option<Duration>,
}

enum FormatterCommandError {
    Timeout(TimeoutDetail),
    Spawn(io::Error),
    Io(io::Error),
    Exit,
    InvalidTimeout,
}

impl fmt::Debug for FormatterCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(detail) => formatter.debug_tuple("Timeout").field(detail).finish(),
            Self::Spawn(error) => formatter.debug_tuple("Spawn").field(&error.kind()).finish(),
            Self::Io(error) => formatter.debug_tuple("Io").field(&error.kind()).finish(),
            Self::Exit => formatter.write_str("Exit"),
            Self::InvalidTimeout => formatter.write_str("InvalidTimeout"),
        }
    }
}

// ── FormatWorker ──────────────────────────────────────────────────────────────

/// Stateless worker that executes one format job.
///
/// Constructed per-job; holds only shared Arcs so it is `Send + Sync`.
///
/// # Path convention
///
/// The `run` method receives a **workspace-relative** path (the same string
/// enqueued by `host_request_format`). It resolves it to an absolute path
/// using `workspace_root` for all filesystem operations. The echo-suppression
/// set key is the original relative path, consistent with the path the watcher
/// broadcasts and `PluginHostRegistry::fan_out` receives.
pub struct FormatWorker {
    /// Echo-suppression set: workspace-relative path → () (presence only).
    pub echo_set: Arc<Mutex<HashMap<String, ()>>>,
    /// Formatter configuration.
    pub config: Arc<FormatterConfig>,
    /// Absolute path to the workspace root for resolving relative paths.
    pub workspace_root: Arc<PathBuf>,
}

impl FormatWorker {
    /// Execute a format job for `relative_path`. Returns `true` on success (tool
    /// ran and output written, or output was identical — both are "non-failure"),
    /// `false` on tool error, missing tool, or CAS abort.
    ///
    /// `relative_path` is a workspace-relative path (no leading `/`, no `..`
    /// components) as produced by the watcher and enqueued by
    /// `host_request_format_impl`. It is resolved to an absolute path using
    /// `self.workspace_root` before any filesystem operation.
    ///
    /// The echo-suppression set key is kept as `relative_path` so it matches
    /// what `PluginHostRegistry::fan_out` receives from the watcher.
    ///
    /// "Success" in terms of the failure counter means "the tool was runnable and
    /// exited 0". A CAS abort is not a failure — it means a concurrent writer won,
    /// which is the correct outcome.
    pub fn run(&self, job: &FormatJob) -> bool {
        let relative_path = job.path.as_str();
        // Resolve workspace-relative path to absolute for filesystem operations.
        // The workspace_root is always absolute; joining a relative path produces
        // an absolute path without traversal risk (the capability already rejects
        // `..` and leading `/`).
        let abs_path = self.workspace_root.join(relative_path);
        let abs_path_str = match abs_path.to_str() {
            Some(s) => s.to_owned(),
            None => {
                eprintln!("[tower/fmt] non-UTF-8 absolute path for {relative_path}");
                return false;
            }
        };

        // Determine extension to look up the right tool.
        let ext = Path::new(relative_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        // Find the tool for this extension.
        let tool = match self.config.tool_for_extension(ext) {
            Some(t) => t,
            None => return true, // No tool configured for this extension — not a failure.
        };

        match tool.mode {
            FormatterMode::Filter => {
                self.run_filter_job(relative_path, &abs_path_str, &tool.argv, job.timeout)
            }
            FormatterMode::Inplace => {
                self.run_inplace_job(relative_path, &abs_path_str, &tool.argv, job.timeout)
            }
        }
    }

    /// Run a filter-mode job: pipe file bytes to stdin, capture stdout.
    ///
    /// `relative_path` is the workspace-relative path (used as the echo-set key).
    /// `abs_path` is the resolved absolute path (used for all `std::fs` operations).
    ///
    /// Returns `true` if the job completed without error (including the case where
    /// the output equals the input — already formatted).
    fn run_filter_job(
        &self,
        relative_path: &str,
        abs_path: &str,
        argv: &[String],
        timeout: Option<Duration>,
    ) -> bool {
        if argv.is_empty() {
            eprintln!("[tower/fmt] empty argv for filter job: {relative_path}");
            return false;
        }

        // Step 1: read file and compute pre-read hash.
        let bytes = match fs::read(abs_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tower/fmt] cannot read {abs_path}: {e}");
                return false;
            }
        };
        let h0 = hash_bytes(&bytes);

        // Step 2: run the formatter with stdin=bytes, capture stdout.
        // `{path}` (if present, e.g. prettier's --stdin-filepath) is substituted
        // with the absolute path so the tool can infer parser/config; the source
        // still flows via stdin. cwd = the file's directory for config discovery.
        let cwd = Path::new(abs_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let substituted = substitute_path(argv, abs_path);
        let out = match run_filter_command(&substituted, &bytes, cwd, timeout) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[tower/fmt] formatter command failed: {e:?}");
                return false;
            }
        };

        self.commit_formatter_output(relative_path, abs_path, &bytes, h0, &out)
    }

    /// Run an inplace-mode job: copy file to a temp path, run the tool, read back.
    ///
    /// `relative_path` is the workspace-relative path (used as the echo-set key).
    /// `abs_path` is the resolved absolute path (used for all `std::fs` operations).
    ///
    /// Returns `true` if the job completed without error.
    fn run_inplace_job(
        &self,
        relative_path: &str,
        abs_path: &str,
        argv: &[String],
        timeout: Option<Duration>,
    ) -> bool {
        if argv.is_empty() {
            eprintln!("[tower/fmt] empty argv for inplace job: {relative_path}");
            return false;
        }

        // Step 1: read source file.
        let bytes = match fs::read(abs_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tower/fmt] cannot read {abs_path}: {e}");
                return false;
            }
        };
        let h0 = hash_bytes(&bytes);

        // Step 2: create temp copy in the file's directory.
        let file_path = Path::new(abs_path);
        let stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let ext_str = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let dir = file_path.parent().unwrap_or(Path::new("."));

        let tmp_name = if ext_str.is_empty() {
            format!("{stem}.fmt.tmp")
        } else {
            format!("{stem}.fmt.tmp.{ext_str}")
        };
        let tmp_path = dir.join(&tmp_name);
        let tmp_path_str = match tmp_path.to_str() {
            Some(s) => s.to_owned(),
            None => {
                eprintln!("[tower/fmt] non-UTF-8 tmp path for {relative_path}");
                return false;
            }
        };

        // Write source bytes to tmp copy.
        if let Err(e) = fs::write(&tmp_path, &bytes) {
            eprintln!("[tower/fmt] cannot write tmp copy {tmp_path_str}: {e}");
            return false;
        }

        // Step 3: build substituted argv, run command.
        let substituted = substitute_path(argv, &tmp_path_str);

        let cwd = dir;
        let ok = match run_inplace_command(&substituted, cwd, timeout) {
            Ok(success) => success,
            Err(e) => {
                eprintln!("[tower/fmt] inplace formatter command failed: {e:?}");
                let _ = fs::remove_file(&tmp_path);
                return false;
            }
        };

        if !ok {
            let _ = fs::remove_file(&tmp_path);
            return false;
        }

        // Step 4: read tmp file back.
        let out = match fs::read(&tmp_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tower/fmt] cannot read tmp result {tmp_path_str}: {e}");
                let _ = fs::remove_file(&tmp_path);
                return false;
            }
        };

        // Clean up tmp copy.
        let _ = fs::remove_file(&tmp_path);

        self.commit_formatter_output(relative_path, abs_path, &bytes, h0, &out)
    }

    fn commit_formatter_output(
        &self,
        relative_path: &str,
        abs_path: &str,
        original: &[u8],
        original_hash: u64,
        output: &[u8],
    ) -> bool {
        if output == original {
            return true;
        }

        let current_bytes = match fs::read(abs_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tower/fmt] CAS re-read failed for {relative_path}: {e}");
                return false;
            }
        };
        if hash_bytes(&current_bytes) != original_hash {
            eprintln!("[tower/fmt] CAS abort for {relative_path}: concurrent write detected");
            return true;
        }

        let dest = Path::new(abs_path);
        let shadow_path = PathBuf::from(format!("{abs_path}.tmp_write"));
        if let Err(e) = durable_write(dest, &shadow_path, output) {
            eprintln!("[tower/fmt] atomic write failed for {relative_path}: {e}");
            return false;
        }

        {
            let mut es = self.echo_set.lock().unwrap_or_else(|p| p.into_inner());
            es.insert(relative_path.to_owned(), ());
        }

        true
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Compute a fast content hash using the standard library's `DefaultHasher`.
///
/// We use `DefaultHasher` (fast, no extra dependency) for CAS comparison only.
/// This is not a cryptographic hash; collision resistance is not required here
/// because we are comparing two reads of the same path within a short window.
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Substitute the `{path}` placeholder in each argv element with `value`.
///
/// Shared by both modes: `inplace` substitutes the temp copy path, `filter`
/// substitutes the file path (e.g. prettier's `--stdin-filepath {path}`).
/// Tools whose argv contains no `{path}` are unaffected (no-op replace).
fn substitute_path(argv: &[String], value: &str) -> Vec<String> {
    argv.iter()
        .map(|arg| arg.replace("{path}", value))
        .collect()
}

/// Run a filter-mode command: write `input` to stdin, capture stdout.
///
/// `cwd` is the file's directory so the tool can discover its own config
/// (`.rustfmt.toml`, `.prettierrc`). Returns `Ok(stdout_bytes)` on exit code 0,
/// `Err(message)` otherwise.
fn run_filter_command(
    argv: &[String],
    input: &[u8],
    cwd: &Path,
    timeout: Option<Duration>,
) -> Result<Vec<u8>, FormatterCommandError> {
    let wait_budget = WaitBudget::new(timeout)?;
    let mut command = formatter_command(argv, cwd)?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let mut child = command.spawn().map_err(FormatterCommandError::Spawn)?;
    let stdin = child.stdin.take().ok_or_else(pipe_error)?;
    let writer = Some(spawn_writer(stdin, input.to_vec()));
    let output = wait_with_timeout(child, wait_budget, writer)?;
    if !output.status.success() {
        return Err(FormatterCommandError::Exit);
    }
    Ok(output.stdout)
}

/// Run an inplace-mode command: the tool writes to the temp copy in place.
///
/// Returns `Ok(true)` on exit 0, `Ok(false)` on non-zero exit (stderr logged),
/// `Err(message)` on spawn failure.
fn run_inplace_command(
    argv: &[String],
    cwd: &Path,
    timeout: Option<Duration>,
) -> Result<bool, FormatterCommandError> {
    let wait_budget = WaitBudget::new(timeout)?;
    let mut command = formatter_command(argv, cwd)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let child = command.spawn().map_err(FormatterCommandError::Spawn)?;
    let output = wait_with_timeout(child, wait_budget, None)?;
    Ok(output.status.success())
}

#[derive(Clone, Copy)]
struct WaitBudget {
    deadline: Option<Instant>,
    budget_ms: Option<u64>,
}

impl WaitBudget {
    fn new(timeout: Option<Duration>) -> Result<Self, FormatterCommandError> {
        let Some(timeout) = timeout else {
            return Ok(Self {
                deadline: None,
                budget_ms: None,
            });
        };
        if timeout.is_zero() {
            return Err(FormatterCommandError::InvalidTimeout);
        }
        let budget_ms = u64::try_from(timeout.as_millis())
            .map_err(|_| FormatterCommandError::InvalidTimeout)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(FormatterCommandError::InvalidTimeout)?;
        Ok(Self {
            deadline: Some(deadline),
            budget_ms: Some(budget_ms),
        })
    }
}

fn formatter_command(argv: &[String], cwd: &Path) -> Result<Command, FormatterCommandError> {
    let executable = argv.first().ok_or(FormatterCommandError::InvalidTimeout)?;
    let mut command = Command::new(executable);
    command.args(&argv[1..]).current_dir(cwd);
    Ok(command)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

/// Collect output from a child process, killing its process tree if its optional
/// operation deadline expires. `WaitBudget` is validated before the spawn.
fn wait_with_timeout(
    mut child: Child,
    budget: WaitBudget,
    writer: Option<PipeWorker<()>>,
) -> Result<Output, FormatterCommandError> {
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);
    wait_with_pipe_workers(child, budget, writer, stdout, stderr)
}

fn wait_with_pipe_workers(
    mut child: Child,
    budget: WaitBudget,
    mut writer: Option<PipeWorker<()>>,
    mut stdout: Option<PipeWorker<Vec<u8>>>,
    mut stderr: Option<PipeWorker<Vec<u8>>>,
) -> Result<Output, FormatterCommandError> {
    let mut status = None;
    let mut writer_done = writer.is_none();
    let mut stdout_bytes = stdout.is_none().then(Vec::new);
    let mut stderr_bytes = stderr.is_none().then(Vec::new);

    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(next_status) => status = next_status,
                Err(error) => {
                    return cleanup_failed_wait(
                        &mut child,
                        writer,
                        stdout,
                        stderr,
                        FormatterCommandError::Io(error),
                    );
                }
            }
        }
        if let Err(error) = poll_pipe(&mut writer, &mut writer_done) {
            return cleanup_failed_wait(&mut child, writer, stdout, stderr, error);
        }
        if let Err(error) = poll_pipe(&mut stdout, &mut stdout_bytes) {
            return cleanup_failed_wait(&mut child, writer, stdout, stderr, error);
        }
        if let Err(error) = poll_pipe(&mut stderr, &mut stderr_bytes) {
            return cleanup_failed_wait(&mut child, writer, stdout, stderr, error);
        }

        let now = Instant::now();
        if budget.deadline.is_some_and(|deadline| now > deadline) {
            return cleanup_failed_wait(
                &mut child,
                writer,
                stdout,
                stderr,
                FormatterCommandError::Timeout(TimeoutDetail {
                    operation: "format".to_owned(),
                    layer: "subprocess".to_owned(),
                    budget_ms: budget
                        .budget_ms
                        .expect("an expired formatter deadline has a budget"),
                }),
            );
        }
        if let Some(status) = status
            && writer_done
            && stdout_bytes.is_some()
            && stderr_bytes.is_some()
        {
            return Ok(Output {
                status,
                stdout: stdout_bytes.unwrap_or_default(),
                stderr: stderr_bytes.unwrap_or_default(),
            });
        }
        thread::sleep(
            budget
                .deadline
                .map_or(Duration::from_millis(10), |deadline| {
                    Duration::from_millis(10).min(deadline.saturating_duration_since(now))
                }),
        );
    }
}

fn cleanup_failed_wait(
    child: &mut Child,
    writer: Option<PipeWorker<()>>,
    stdout: Option<PipeWorker<Vec<u8>>>,
    stderr: Option<PipeWorker<Vec<u8>>>,
    original_error: FormatterCommandError,
) -> Result<Output, FormatterCommandError> {
    terminate_process_tree(child);
    let _ = child.wait();
    let _ = join_pipe(writer);
    let _ = join_pipe(stdout);
    let _ = join_pipe(stderr);
    Err(original_error)
}

struct PipeWorker<T> {
    receiver: mpsc::Receiver<io::Result<T>>,
    handle: thread::JoinHandle<()>,
}

fn spawn_reader<R>(mut reader: R) -> PipeWorker<Vec<u8>>
where
    R: Read + Send + 'static,
{
    spawn_pipe_worker(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn spawn_writer(mut writer: impl IoWrite + Send + 'static, input: Vec<u8>) -> PipeWorker<()> {
    spawn_pipe_worker(move || match writer.write_all(&input) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    })
}

fn spawn_pipe_worker<T: Send + 'static>(
    operation: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> PipeWorker<T> {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let _ = sender.send(operation());
    });
    PipeWorker { receiver, handle }
}

trait PipeOutput<T> {
    fn store(&mut self, value: T);
}

impl<T> PipeOutput<T> for Option<T> {
    fn store(&mut self, value: T) {
        *self = Some(value);
    }
}

impl PipeOutput<()> for bool {
    fn store(&mut self, (): ()) {
        *self = true;
    }
}

fn poll_pipe<T>(
    worker: &mut Option<PipeWorker<T>>,
    output: &mut impl PipeOutput<T>,
) -> Result<(), FormatterCommandError> {
    let Some(active) = worker.as_ref() else {
        return Ok(());
    };
    match active.receiver.try_recv() {
        Ok(result) => {
            let active = worker.take().expect("active pipe worker");
            active.handle.join().map_err(|_| pipe_error())?;
            output.store(result.map_err(FormatterCommandError::Io)?);
            Ok(())
        }
        Err(mpsc::TryRecvError::Empty) => Ok(()),
        Err(mpsc::TryRecvError::Disconnected) => {
            let active = worker.take().expect("active pipe worker");
            active.handle.join().map_err(|_| pipe_error())?;
            Err(pipe_error())
        }
    }
}

fn join_pipe<T>(worker: Option<PipeWorker<T>>) -> Result<(), FormatterCommandError> {
    if let Some(worker) = worker {
        worker.handle.join().map_err(|_| pipe_error())?;
    }
    Ok(())
}

fn pipe_error() -> FormatterCommandError {
    FormatterCommandError::Io(io::Error::other("formatter pipe worker failed"))
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_child(child);
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::super::format_queue::{EnqueueResult, FormatQueue, FormatQueuePort};
    use super::*;
    use crate::adapters::config::{FormatterConfig, FormatterMode, ToolConfig};

    fn worker_with_tools(tools: BTreeMap<String, ToolConfig>) -> FormatWorker {
        FormatWorker {
            echo_set: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(FormatterConfig { tools }),
            workspace_root: Arc::new(PathBuf::new()),
        }
    }

    fn noop_echo_set() -> Arc<Mutex<HashMap<String, ()>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[cfg(unix)]
    fn executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn assert_recorded_process_exits(pid_path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::yield_now();
        }
        let pid = fs::read_to_string(pid_path).unwrap_or_else(|error| {
            panic!("read descendant pid from {}: {error}", pid_path.display())
        });
        let pid = pid.trim();
        while Instant::now() < deadline {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return;
            }
            thread::yield_now();
        }
        panic!("timed-out formatter descendant {pid} remained alive");
    }

    #[cfg(unix)]
    fn assert_timed_out_formatter_reaps_process_tree(mode: FormatterMode) {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let target = dir.path().join("source.txt");
        fs::write(&target, b"unchanged\n").unwrap();
        let script = dir.path().join("formatter.sh");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" > \"$1\"; while :; do sleep 1; done' -- \"$1\" &\nwhile [ ! -s \"$1\" ]; do :; done\nwait\n",
        );
        let argv = match mode {
            FormatterMode::Filter => vec![
                script.to_string_lossy().into_owned(),
                pid_path.to_string_lossy().into_owned(),
            ],
            FormatterMode::Inplace => vec![
                script.to_string_lossy().into_owned(),
                pid_path.to_string_lossy().into_owned(),
                "{path}".to_owned(),
            ],
        };

        let error = match mode {
            FormatterMode::Filter => run_filter_command(
                &argv,
                b"unchanged\n",
                dir.path(),
                Some(Duration::from_millis(100)),
            )
            .expect_err("filter formatter must expire"),
            FormatterMode::Inplace => run_inplace_command(
                &substitute_path(&argv, target.to_str().unwrap()),
                dir.path(),
                Some(Duration::from_millis(100)),
            )
            .expect_err("inplace formatter must expire"),
        };
        assert!(matches!(error, FormatterCommandError::Timeout(_)));
        assert_recorded_process_exits(&pid_path);
        assert_eq!(fs::read(&target).unwrap(), b"unchanged\n");
    }

    #[cfg(unix)]
    #[test]
    fn formatter_without_configured_timeout_completes_after_release() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join("started");
        let release = dir.path().join("release");
        let source = dir.path().join("source.txt");
        fs::write(&source, b"unformatted\n").unwrap();
        let script = dir.path().join("formatter.sh");
        executable(
            &script,
            "#!/bin/sh\nprintf started > \"$1\"\nwhile [ ! -e \"$2\" ]; do :; done\ncat >/dev/null\nprintf 'formatted\\n'\n",
        );
        let mut tools = BTreeMap::new();
        tools.insert(
            "fixture".to_owned(),
            ToolConfig {
                mode: FormatterMode::Filter,
                argv: vec![
                    script.to_string_lossy().into_owned(),
                    marker.to_string_lossy().into_owned(),
                    release.to_string_lossy().into_owned(),
                ],
                extensions: vec!["txt".to_owned()],
                timeout_secs: None,
            },
        );
        let worker = FormatWorker {
            echo_set: noop_echo_set(),
            config: Arc::new(FormatterConfig { tools }),
            workspace_root: Arc::new(dir.path().to_path_buf()),
        };
        let (result_tx, result_rx) = mpsc::channel();
        thread::spawn(move || {
            result_tx
                .send(worker.run(&FormatJob {
                    path: "source.txt".to_owned(),
                    timeout: None,
                }))
                .unwrap();
        });

        let marker_deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < marker_deadline {
            thread::yield_now();
        }
        assert!(marker.exists(), "formatter did not reach its release gate");
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        fs::write(&release, b"release").unwrap();
        assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"formatted\n");
    }

    #[cfg(unix)]
    #[test]
    fn configured_formatter_timeout_reports_operation_layer_and_budget() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let pid_path = dir.path().join("configured-timeout-descendant.pid");
        let script = dir.path().join("configured-timeout-formatter.sh");
        fs::write(&source, b"source\n").unwrap();
        executable(
            &script,
            "#!/bin/sh\nsh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" > \"$1\"; while :; do sleep 1; done' -- \"$1\" &\nwhile [ ! -s \"$1\" ]; do :; done\nwait\n",
        );

        // Start with the persisted tool configuration rather than supplying a
        // duration to the subprocess helper. The queue creates the real worker
        // and that worker must carry this exact budget into its command wait.
        let tool = ToolConfig {
            mode: FormatterMode::Filter,
            argv: vec![
                script.to_string_lossy().into_owned(),
                pid_path.to_string_lossy().into_owned(),
            ],
            extensions: vec!["txt".to_owned()],
            timeout_secs: Some(1),
        };
        let timeout = Duration::from_secs(tool.timeout_secs.expect("configured timeout"));
        let mut tools = BTreeMap::new();
        tools.insert("fixture".to_owned(), tool.clone());
        let queue = FormatQueue::new(FormatterConfig { tools }, dir.path().to_path_buf());
        assert!(
            matches!(
                queue.enqueue("source.txt".to_owned()),
                EnqueueResult::Accepted
            ),
            "the real queue must accept the configured formatter job"
        );
        assert_recorded_process_exits(&pid_path);
        assert_eq!(fs::read(&source).unwrap(), b"source\n");

        // The asynchronous queue reports only success/failure. Assert the
        // worker command contract independently with the same ToolConfig-derived
        // duration, including the original (not elapsed) budget.
        let error = run_filter_command(&tool.argv, b"source\n", dir.path(), Some(timeout))
            .expect_err("configured formatter command must expire");

        match error {
            FormatterCommandError::Timeout(detail) => {
                assert_eq!(detail.operation, "format");
                assert_eq!(detail.layer, "subprocess");
                assert_eq!(detail.budget_ms, 1_000);
            }
            other => panic!("expected structured timeout detail, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_filter_formatter_reaps_its_process_tree() {
        assert_timed_out_formatter_reaps_process_tree(FormatterMode::Filter);
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_inplace_formatter_reaps_its_process_tree() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let source = dir.path().join("source.txt");
        let temp_copy = dir.path().join("source.fmt.tmp.txt");
        let script = dir.path().join("partial-inplace-formatter.sh");
        fs::write(&source, b"original source bytes\n").unwrap();
        executable(
            &script,
            "#!/bin/sh\nprintf 'partial formatter bytes\\n' > \"$2\"\nsh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" > \"$1\"; while :; do sleep 1; done' -- \"$1\" &\nwhile [ ! -s \"$1\" ]; do :; done\nwait\n",
        );

        let mut tools = BTreeMap::new();
        tools.insert(
            "fixture".to_owned(),
            ToolConfig {
                mode: FormatterMode::Inplace,
                argv: vec![
                    script.to_string_lossy().into_owned(),
                    pid_path.to_string_lossy().into_owned(),
                    "{path}".to_owned(),
                ],
                extensions: vec!["txt".to_owned()],
                timeout_secs: Some(1),
            },
        );
        let worker = FormatWorker {
            echo_set: noop_echo_set(),
            config: Arc::new(FormatterConfig { tools }),
            workspace_root: Arc::new(dir.path().to_path_buf()),
        };
        let (result_tx, result_rx) = mpsc::channel();
        thread::spawn(move || {
            result_tx
                .send(worker.run(&FormatJob {
                    path: "source.txt".to_owned(),
                    timeout: Some(Duration::from_secs(1)),
                }))
                .unwrap();
        });

        assert!(
            !result_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("timed-out inplace worker must complete"),
            "timeout must prevent publishing formatter output"
        );
        assert_recorded_process_exits(&pid_path);
        assert_eq!(
            fs::read(&source).unwrap(),
            b"original source bytes\n",
            "partial bytes from the formatter temp copy must never be committed"
        );
        assert!(
            !temp_copy.exists(),
            "timed-out inplace formatter temp copy must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unrepresentable_formatter_deadline_is_rejected_before_spawn() {
        let dir = tempdir().unwrap();
        let sentinel = dir.path().join("spawned");
        let command = format!("printf spawned > '{}'", sentinel.display());
        let error = run_filter_command(
            &["sh".to_owned(), "-c".to_owned(), command],
            b"source",
            dir.path(),
            Some(Duration::MAX),
        )
        .expect_err("unrepresentable deadline must be rejected");

        assert!(!matches!(error, FormatterCommandError::Timeout(_)));
        assert!(!sentinel.exists(), "invalid deadline spawned the formatter");
    }

    #[cfg(unix)]
    #[test]
    fn formatter_command_failures_keep_spawn_and_exit_evidence_distinguishable() {
        let missing = run_filter_command(
            &["tower-missing-formatter-binary".to_owned()],
            b"source",
            Path::new("."),
            None,
        )
        .expect_err("missing formatter must fail to spawn");
        let exited = run_filter_command(
            &["sh".to_owned(), "-c".to_owned(), "exit 7".to_owned()],
            b"source",
            Path::new("."),
            None,
        )
        .expect_err("non-zero formatter must retain exit evidence");

        assert!(matches!(missing, FormatterCommandError::Spawn(_)));
        assert!(matches!(exited, FormatterCommandError::Exit));
    }

    #[cfg(unix)]
    #[test]
    fn formatter_pipe_io_failure_is_sanitized_and_reaps_process_tree() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("formatter.sh");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'printf \"%s\\n\" \"$$\" > \"$1\"; while :; do sleep 1; done' -- \"$1\" &\nwhile [ ! -s \"$1\" ]; do :; done\nwait\n",
        );

        let mut command = Command::new(&script);
        command.arg(&pid_path).stdin(Stdio::null());
        configure_process_group(&mut command);
        let child = command.spawn().unwrap();
        while !pid_path.exists() {
            thread::yield_now();
        }

        let io_sentinel = "formatter-io-secret-source-bytes";
        let stdout = Some(spawn_pipe_worker(move || {
            Err::<Vec<u8>, _>(io::Error::other(io_sentinel))
        }));
        let io_failure =
            wait_with_pipe_workers(child, WaitBudget::new(None).unwrap(), None, stdout, None)
                .expect_err("pipe I/O failure must fail the formatter wait");

        match &io_failure {
            FormatterCommandError::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
            }
            other => panic!("expected deterministic I/O classification, got {other:?}"),
        }
        assert!(
            !format!("{io_failure:?}").contains(io_sentinel),
            "sanitized formatter failure reporting must not expose I/O contents"
        );
        assert_recorded_process_exits(&pid_path);
    }

    #[test]
    fn substitute_path_replaces_placeholder_in_every_arg() {
        let argv = vec![
            "prettier".to_string(),
            "--stdin-filepath".to_string(),
            "{path}".to_string(),
        ];
        assert_eq!(
            substitute_path(&argv, "/ws/src/main.ts"),
            vec!["prettier", "--stdin-filepath", "/ws/src/main.ts"]
        );
    }

    #[test]
    fn substitute_path_is_noop_without_placeholder() {
        let argv = vec![
            "rustfmt".to_string(),
            "--edition".to_string(),
            "2021".to_string(),
        ];
        assert_eq!(substitute_path(&argv, "/ws/x.rs"), argv);
    }

    // ── RED-2 → GREEN-2: filter-mode job formats a file atomically ─────────────

    /// GREEN-2 / AC1: filter-mode job: given a file with content "hello\n",
    /// a tool that outputs "formatted\n", the file is atomically rewritten
    /// and the .tmp_write artifact is gone.
    #[test]
    fn filter_job_rewrites_file_atomically() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, b"hello\n").unwrap();

        let path_str = file_path.to_str().unwrap().to_owned();

        // Use a tool that just outputs "formatted\n" (echo -n on stdin → stdout).
        // We use `cat` style: on Linux `rev` reverses lines, but for predictability
        // use a Python one-liner if available, otherwise skip.
        // Instead, we test the worker logic directly with a mock argv that uses
        // `sh -c 'echo formatted'` to produce a different output.
        let mut tools = BTreeMap::new();
        tools.insert(
            "txt".to_owned(),
            ToolConfig {
                mode: FormatterMode::Filter,
                argv: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "printf 'formatted\\n'".to_owned(),
                ],
                extensions: vec!["txt".to_owned()],
                timeout_secs: None,
            },
        );
        let worker = worker_with_tools(tools);
        let result = worker.run_filter_job(
            &path_str,
            &path_str,
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "printf 'formatted\\n'".to_owned(),
            ],
            None,
        );

        assert!(result, "filter job must return true on success");
        let content = fs::read(&file_path).unwrap();
        assert_eq!(
            content, b"formatted\n",
            "file must contain formatted output"
        );

        // .tmp_write artifact must be gone.
        let tmp = format!("{path_str}.tmp_write");
        assert!(
            !std::path::Path::new(&tmp).exists(),
            ".tmp_write must be cleaned up"
        );
    }

    /// GREEN-2: filter-mode job does not write when output equals input.
    #[test]
    fn filter_job_noop_when_output_equals_input() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("same.txt");
        fs::write(&file_path, b"already formatted\n").unwrap();
        let path_str = file_path.to_str().unwrap().to_owned();

        // Tool that outputs the input unchanged (cat).
        let result = worker_with_tools(BTreeMap::new()).run_filter_job(
            &path_str,
            &path_str,
            &["sh".to_owned(), "-c".to_owned(), "cat".to_owned()],
            None,
        );

        assert!(result, "no-op job returns true");
        // File content must not have changed.
        let content = fs::read(&file_path).unwrap();
        assert_eq!(content, b"already formatted\n");
    }

    // ── RED-3 → GREEN-3: inplace-mode job ────────────────────────────────────

    /// GREEN-3 / AC2: inplace-mode job: tool writes to temp copy, result
    /// is atomically written to original; original is never passed to the tool.
    #[test]
    fn inplace_job_writes_via_tmp_copy() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.php");
        fs::write(&file_path, b"<?php echo 1; ?>").unwrap();
        let path_str = file_path.to_str().unwrap().to_owned();

        // Use `sh -c 'echo "<?php echo 1; ?> // formatted" > "$1"' -- {path}`
        // The {path} placeholder gets substituted with the tmp copy.
        let argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            r#"printf '<?php echo 1; ?>\n// formatted' > "$1""#.to_owned(),
            "--".to_owned(),
            "{path}".to_owned(),
        ];

        let worker = FormatWorker {
            echo_set: noop_echo_set(),
            config: Arc::new(FormatterConfig::default()),
            workspace_root: Arc::new(PathBuf::new()),
        };
        let result = worker.run_inplace_job(&path_str, &path_str, &argv, None);

        assert!(result, "inplace job must return true on success");
        let content = fs::read(&file_path).unwrap();
        assert!(
            content.starts_with(b"<?php echo 1; ?>"),
            "formatted content must be written: {content:?}"
        );

        // Temp copy must be cleaned up.
        let tmp = dir.path().join("test.fmt.tmp.php");
        assert!(!tmp.exists(), ".fmt.tmp must be cleaned up");
    }

    // ── RED-4 → GREEN-4: CAS abort on concurrent change ──────────────────────

    /// GREEN-4 / AC4: If the file is modified between pre-read and write,
    /// the CAS check aborts and the newer content wins.
    ///
    /// We test `run_filter_job` by intercepting at the CAS re-read step.
    /// Since we cannot inject into the method, we verify indirectly:
    /// write different content during the format, then assert the formatted
    /// output was NOT applied (the newer content survives).
    ///
    /// This test uses a script that sleeps briefly so we can write a new
    /// version of the file between the tool invocation and the CAS check.
    /// Due to the sequential nature of the worker, we instead directly test
    /// the hash logic.
    #[test]
    fn cas_aborts_when_file_changed_concurrently() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("concurrent.txt");
        fs::write(&file_path, b"original\n").unwrap();
        let path_str = file_path.to_str().unwrap().to_owned();

        // The script: output formatted text then modify the file on disk
        // (simulating a concurrent writer), before the worker re-reads it.
        // We use a tool that writes "newer content\n" to the file (simulating
        // external write) and then outputs "formatted\n" on stdout.
        // Because the file now has "newer content\n" but h0 was "original\n",
        // the CAS check will abort.
        let script = format!(
            "printf 'newer content\\n' > '{}'; printf 'formatted\\n'",
            path_str.replace('\'', "\\'")
        );
        let argv = vec!["sh".to_owned(), "-c".to_owned(), script];

        let worker = FormatWorker {
            echo_set: noop_echo_set(),
            config: Arc::new(FormatterConfig::default()),
            workspace_root: Arc::new(PathBuf::new()),
        };
        let result = worker.run_filter_job(&path_str, &path_str, &argv, None);

        // Result is true (CAS abort is not a tool failure).
        assert!(result, "CAS abort returns true (not a tool failure)");

        // The newer content must survive (formatted output was NOT applied).
        let final_content = fs::read(&file_path).unwrap();
        assert_eq!(
            final_content, b"newer content\n",
            "newer content must survive CAS abort: got {final_content:?}"
        );
    }

    // ── RED-7 → GREEN-7: exit != 0 leaves file untouched ─────────────────────

    /// GREEN-7 / AC6: If the tool exits with non-zero status, the file is
    /// unchanged and the job returns false (incrementing the failure counter).
    #[test]
    fn filter_job_returns_false_on_tool_failure() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("broken.rs");
        fs::write(&file_path, b"original content\n").unwrap();
        let path_str = file_path.to_str().unwrap().to_owned();

        // Tool that always fails with exit 1.
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "exit 1".to_owned()];

        let worker = FormatWorker {
            echo_set: noop_echo_set(),
            config: Arc::new(FormatterConfig::default()),
            workspace_root: Arc::new(PathBuf::new()),
        };
        let result = worker.run_filter_job(&path_str, &path_str, &argv, None);

        assert!(!result, "failed tool must return false");
        let content = fs::read(&file_path).unwrap();
        assert_eq!(
            content, b"original content\n",
            "file must be unchanged after tool failure"
        );
    }

    // ── RED-8 → GREEN-8: missing tool degrades gracefully ────────────────────

    /// GREEN-8 / AC7: A missing tool binary returns false without crashing.
    #[test]
    fn filter_job_returns_false_for_missing_tool() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("file.rs");
        fs::write(&file_path, b"content\n").unwrap();
        let path_str = file_path.to_str().unwrap().to_owned();

        let argv = vec!["nonexistent_binary_xyzzy_13a".to_owned()];

        let worker = FormatWorker {
            echo_set: noop_echo_set(),
            config: Arc::new(FormatterConfig::default()),
            workspace_root: Arc::new(PathBuf::new()),
        };
        let result = worker.run_filter_job(&path_str, &path_str, &argv, None);

        assert!(!result, "missing tool must return false");
        let content = fs::read(&file_path).unwrap();
        assert_eq!(content, b"content\n", "file unchanged after missing tool");
    }
}
