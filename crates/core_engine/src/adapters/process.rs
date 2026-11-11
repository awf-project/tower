use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

use crate::domain::CancellationSignal;
use serde::Serialize;

#[cfg(test)]
static PROCESS_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn process_fixture_lock() -> MutexGuard<'static, ()> {
    PROCESS_FIXTURE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessRequest {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) deadline: Option<Instant>,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) stdout_limit: Option<usize>,
    pub(crate) stderr_limit: Option<usize>,
    pub(crate) cleanup_budget: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BoundedOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TerminationStatus {
    Exited { code: i32 },
    Signalled,
    OtherFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessOutcome {
    pub(crate) status: TerminationStatus,
    pub(crate) stdout: BoundedOutput,
    pub(crate) stderr: BoundedOutput,
    pub(crate) timed_out: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProcessError {
    SpawnFailed,
    StdinWriteFailed,
    PipeReadFailed { stream: OutputStream },
    WaitFailed,
    CleanupFailed,
    Cancelled,
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::SpawnFailed => "process spawn failed",
            Self::StdinWriteFailed => "process input write failed",
            Self::PipeReadFailed { .. } => "process output read failed",
            Self::WaitFailed => "process wait failed",
            Self::CleanupFailed => "process cleanup failed",
            Self::Cancelled => "process execution cancelled",
        };

        formatter.write_str(message)
    }
}

impl fmt::Debug for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl std::error::Error for ProcessError {}

pub(crate) fn run_supervised_process(
    request: ProcessRequest,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessOutcome, ProcessError> {
    run_supervised_process_with_factories(
        request,
        cancellation,
        &SystemSpawner,
        &SystemReaders,
        &SystemWaiter,
    )
}

trait Spawner {
    fn spawn(&self, command: &mut Command) -> std::io::Result<Child>;
}

struct SystemSpawner;

impl Spawner for SystemSpawner {
    fn spawn(&self, command: &mut Command) -> std::io::Result<Child> {
        command.spawn()
    }
}

impl<F> Spawner for F
where
    F: Fn(&mut Command) -> std::io::Result<Child>,
{
    fn spawn(&self, command: &mut Command) -> std::io::Result<Child> {
        self(command)
    }
}

trait ReaderFactory {
    fn stdout(
        &self,
        reader: ChildStdout,
        limit: Option<usize>,
    ) -> Result<PipeWorker<BoundedOutput>, ProcessError>;
    fn stderr(
        &self,
        reader: ChildStderr,
        limit: Option<usize>,
    ) -> Result<PipeWorker<BoundedOutput>, ProcessError>;
}

struct SystemReaders;

impl ReaderFactory for SystemReaders {
    fn stdout(
        &self,
        reader: ChildStdout,
        limit: Option<usize>,
    ) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
        spawn_reader(reader, limit, OutputStream::Stdout)
    }

    fn stderr(
        &self,
        reader: ChildStderr,
        limit: Option<usize>,
    ) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
        spawn_reader(reader, limit, OutputStream::Stderr)
    }
}

trait Waiter {
    fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>>;
}

struct SystemWaiter;

impl Waiter for SystemWaiter {
    fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        child.try_wait()
    }
}

impl<F> Waiter for F
where
    F: Fn(&mut Child) -> std::io::Result<Option<ExitStatus>>,
{
    fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        self(child)
    }
}

struct OwnedProcess {
    child: Option<Child>,
    cleanup_budget: Duration,
}

impl OwnedProcess {
    fn new(child: Child, cleanup_budget: Duration) -> Self {
        Self {
            child: Some(child),
            cleanup_budget,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("owned child")
    }

    fn release(mut self) -> Child {
        self.child.take().expect("owned child")
    }

    fn cleanup_workers(
        &mut self,
        writer: Option<PipeWorker<()>>,
        stdout: Option<PipeWorker<BoundedOutput>>,
        stderr: Option<PipeWorker<BoundedOutput>>,
        waiter: &dyn Waiter,
    ) -> Result<(), ProcessError> {
        let cleanup_budget = self.cleanup_budget;
        terminate_and_collect(
            self.child_mut(),
            cleanup_budget,
            writer,
            stdout,
            stderr,
            waiter,
        )?;
        self.child.take();
        Ok(())
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ =
                terminate_and_collect(child, self.cleanup_budget, None, None, None, &SystemWaiter);
        }
    }
}

#[cfg(test)]
pub(crate) fn run_supervised_process_with_stdout_failure(
    request: ProcessRequest,
    cancellation: &dyn CancellationSignal,
    failure_gate: PathBuf,
    failure_message: String,
) -> Result<ProcessOutcome, ProcessError> {
    struct FailingReaders {
        gate: PathBuf,
        message: String,
    }

    struct FailingRead {
        reader: ChildStdout,
        gate: PathBuf,
        message: String,
    }

    impl Read for FailingRead {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            let _ = &self.reader;
            while !self.gate.exists() {
                thread::yield_now();
            }
            Err(std::io::Error::other(self.message.clone()))
        }
    }

    impl ReaderFactory for FailingReaders {
        fn stdout(
            &self,
            reader: ChildStdout,
            _limit: Option<usize>,
        ) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
            spawn_reader(
                FailingRead {
                    reader,
                    gate: self.gate.clone(),
                    message: self.message.clone(),
                },
                None,
                OutputStream::Stdout,
            )
        }

        fn stderr(
            &self,
            reader: ChildStderr,
            limit: Option<usize>,
        ) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
            spawn_reader(reader, limit, OutputStream::Stderr)
        }
    }

    run_supervised_process_with_factories(
        request,
        cancellation,
        &SystemSpawner,
        &FailingReaders {
            gate: failure_gate,
            message: failure_message,
        },
        &SystemWaiter,
    )
}

#[cfg(test)]
fn run_supervised_process_with_spawner(
    request: ProcessRequest,
    cancellation: &dyn CancellationSignal,
    spawner: &dyn Spawner,
) -> Result<ProcessOutcome, ProcessError> {
    run_supervised_process_with_factories(
        request,
        cancellation,
        spawner,
        &SystemReaders,
        &SystemWaiter,
    )
}

#[cfg(test)]
fn run_supervised_process_with_waiter(
    request: ProcessRequest,
    cancellation: &dyn CancellationSignal,
    waiter: &dyn Waiter,
) -> Result<ProcessOutcome, ProcessError> {
    run_supervised_process_with_factories(
        request,
        cancellation,
        &SystemSpawner,
        &SystemReaders,
        waiter,
    )
}

fn run_supervised_process_with_factories(
    request: ProcessRequest,
    cancellation: &dyn CancellationSignal,
    spawner: &dyn Spawner,
    readers: &dyn ReaderFactory,
    waiter: &dyn Waiter,
) -> Result<ProcessOutcome, ProcessError> {
    assert!(request.stdout_limit.is_none_or(|limit| limit > 0));
    assert!(request.stderr_limit.is_none_or(|limit| limit > 0));

    let executable = request.argv.first().ok_or(ProcessError::SpawnFailed)?;
    let mut command = Command::new(executable);
    command
        .args(&request.argv[1..])
        .current_dir(&request.cwd)
        .stdin(if request.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);

    let child = spawner
        .spawn(&mut command)
        .map_err(|_| ProcessError::SpawnFailed)?;
    let mut owned = OwnedProcess::new(child, request.cleanup_budget);
    let writer = match request.stdin {
        Some(input) => {
            let stdin = owned
                .child_mut()
                .stdin
                .take()
                .ok_or(ProcessError::StdinWriteFailed)?;
            Some(spawn_worker(ProcessError::StdinWriteFailed, move || {
                let mut stdin = stdin;
                stdin
                    .write_all(&input)
                    .map_err(|_| ProcessError::StdinWriteFailed)
            })?)
        }
        None => None,
    };
    let mut writer = writer;
    let mut stdout = match owned.child_mut().stdout.take() {
        Some(pipe) => match readers.stdout(pipe, request.stdout_limit) {
            Ok(worker) => Some(worker),
            Err(error) => {
                owned.cleanup_workers(writer.take(), None, None, waiter)?;
                return Err(error);
            }
        },
        None => None,
    };
    let stderr = match owned.child_mut().stderr.take() {
        Some(pipe) => match readers.stderr(pipe, request.stderr_limit) {
            Ok(worker) => Some(worker),
            Err(error) => {
                owned.cleanup_workers(writer.take(), stdout.take(), None, waiter)?;
                return Err(error);
            }
        },
        None => None,
    };
    let child = owned.release();

    supervise(
        child,
        request.deadline,
        request.cleanup_budget,
        cancellation,
        ProcessWorkers {
            writer,
            stdout,
            stderr,
        },
        waiter,
    )
}

struct ProcessWorkers {
    writer: Option<PipeWorker<()>>,
    stdout: Option<PipeWorker<BoundedOutput>>,
    stderr: Option<PipeWorker<BoundedOutput>>,
}

fn supervise(
    mut child: Child,
    deadline: Option<Instant>,
    cleanup_budget: Duration,
    cancellation: &dyn CancellationSignal,
    mut workers: ProcessWorkers,
    waiter: &dyn Waiter,
) -> Result<ProcessOutcome, ProcessError> {
    let mut status = None;
    let mut writer_done = workers.writer.is_none();
    let mut stdout_output = workers.stdout.is_none().then(empty_output);
    let mut stderr_output = workers.stderr.is_none().then(empty_output);

    loop {
        let mut failure = poll_worker(&mut workers.writer, &mut writer_done).err();
        failure = failure.or_else(|| poll_worker(&mut workers.stdout, &mut stdout_output).err());
        failure = failure.or_else(|| poll_worker(&mut workers.stderr, &mut stderr_output).err());
        if status.is_none() {
            match waiter.try_wait(&mut child) {
                Ok(observed) => status = observed,
                Err(_) => {
                    failure.get_or_insert(ProcessError::WaitFailed);
                }
            }
        }

        if cancellation.is_cancelled() {
            terminate_and_collect(
                &mut child,
                cleanup_budget,
                workers.writer,
                workers.stdout,
                workers.stderr,
                waiter,
            )?;
            return Err(ProcessError::Cancelled);
        }
        if let Some(error) = failure {
            terminate_and_collect(
                &mut child,
                cleanup_budget,
                workers.writer,
                workers.stdout,
                workers.stderr,
                waiter,
            )?;
            return Err(error);
        }
        if let Some(exit_status) = status {
            if writer_done && stdout_output.is_some() && stderr_output.is_some() {
                terminate_process_tree(&mut child);
                await_process_tree_exit(&child, cleanup_budget)?;
                return Ok(ProcessOutcome {
                    status: termination_status(exit_status),
                    stdout: stdout_output.unwrap_or_else(empty_output),
                    stderr: stderr_output.unwrap_or_else(empty_output),
                    timed_out: false,
                });
            }

            let collected = terminate_and_collect(
                &mut child,
                cleanup_budget,
                workers.writer,
                workers.stdout,
                workers.stderr,
                waiter,
            )?;
            return Ok(ProcessOutcome {
                status: termination_status(exit_status),
                // Completed readers have already been removed from workers.
                // Preserve their output when cleanup collects the remaining pipes.
                stdout: stdout_output.unwrap_or(collected.stdout),
                stderr: stderr_output.unwrap_or(collected.stderr),
                timed_out: false,
            });
        }

        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            let collected = terminate_and_collect(
                &mut child,
                cleanup_budget,
                workers.writer,
                workers.stdout,
                workers.stderr,
                waiter,
            )?;
            return Ok(ProcessOutcome {
                status: collected.status,
                // Completed readers have already been removed from workers.
                // Preserve their output when cleanup collects the remaining pipes.
                stdout: stdout_output.unwrap_or(collected.stdout),
                stderr: stderr_output.unwrap_or(collected.stderr),
                timed_out: true,
            });
        }

        thread::sleep(deadline.map_or(Duration::from_millis(5), |deadline| {
            Duration::from_millis(5).min(deadline.saturating_duration_since(now))
        }));
    }
}

struct CollectedProcess {
    status: TerminationStatus,
    stdout: BoundedOutput,
    stderr: BoundedOutput,
}

fn terminate_and_collect(
    child: &mut Child,
    cleanup_budget: Duration,
    mut writer: Option<PipeWorker<()>>,
    mut stdout: Option<PipeWorker<BoundedOutput>>,
    mut stderr: Option<PipeWorker<BoundedOutput>>,
    waiter: &dyn Waiter,
) -> Result<CollectedProcess, ProcessError> {
    terminate_process_tree(child);
    let now = Instant::now();
    let (deadline, invalid_budget) = match now.checked_add(cleanup_budget) {
        Some(deadline) => (deadline, false),
        None => (now + Duration::from_secs(2), true),
    };
    let mut status = None;
    let mut writer_done = writer.is_none();
    let mut stdout_output = stdout.is_none().then(empty_output);
    let mut stderr_output = stderr.is_none().then(empty_output);

    loop {
        let _ = poll_worker(&mut writer, &mut writer_done);
        let _ = poll_worker(&mut stdout, &mut stdout_output);
        let _ = poll_worker(&mut stderr, &mut stderr_output);
        if status.is_none() {
            status = waiter
                .try_wait(child)
                .map_err(|_| ProcessError::CleanupFailed)?;
        }
        if let Some(exit_status) = status
            && writer_done
            && stdout_output.is_some()
            && stderr_output.is_some()
            && !process_tree_is_alive(child)
        {
            let collected = CollectedProcess {
                status: termination_status(exit_status),
                stdout: stdout_output.unwrap_or_else(empty_output),
                stderr: stderr_output.unwrap_or_else(empty_output),
            };
            return if invalid_budget {
                Err(ProcessError::CleanupFailed)
            } else {
                Ok(collected)
            };
        }
        if Instant::now() >= deadline {
            return Err(ProcessError::CleanupFailed);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn await_process_tree_exit(child: &Child, cleanup_budget: Duration) -> Result<(), ProcessError> {
    let deadline = Instant::now()
        .checked_add(cleanup_budget)
        .ok_or(ProcessError::CleanupFailed)?;
    while process_tree_is_alive(child) {
        if Instant::now() >= deadline {
            return Err(ProcessError::CleanupFailed);
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

#[cfg(unix)]
fn process_tree_is_alive(child: &Child) -> bool {
    let pid = rustix::process::Pid::from_child(child);
    rustix::process::test_kill_process_group(pid).is_ok()
}

#[cfg(not(unix))]
fn process_tree_is_alive(_child: &Child) -> bool {
    false
}

fn empty_output() -> BoundedOutput {
    BoundedOutput {
        bytes: Vec::new(),
        truncated: false,
    }
}

fn spawn_reader(
    mut reader: impl Read + Send + 'static,
    limit: Option<usize>,
    stream: OutputStream,
) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
    spawn_worker(ProcessError::PipeReadFailed { stream }, move || {
        let mut retained = Vec::new();
        let mut truncated = false;
        let mut chunk = [0_u8; 8192];
        loop {
            let count = reader
                .read(&mut chunk)
                .map_err(|_| ProcessError::PipeReadFailed { stream })?;
            if count == 0 {
                break;
            }
            let remaining = limit.map_or(count, |limit| limit.saturating_sub(retained.len()));
            retained.extend_from_slice(&chunk[..count.min(remaining)]);
            truncated |= remaining < count;
        }
        Ok(BoundedOutput {
            bytes: retained,
            truncated,
        })
    })
}

struct PipeWorker<T> {
    receiver: mpsc::Receiver<Result<T, ProcessError>>,
    handle: thread::JoinHandle<()>,
}

fn spawn_worker<T: Send + 'static>(
    spawn_error: ProcessError,
    operation: impl FnOnce() -> Result<T, ProcessError> + Send + 'static,
) -> Result<PipeWorker<T>, ProcessError> {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::Builder::new()
        .spawn(move || {
            let _ = sender.send(operation());
        })
        .map_err(|_| spawn_error)?;
    Ok(PipeWorker { receiver, handle })
}

trait StoreWorkerOutput<T> {
    fn store(&mut self, value: T);
}

impl<T> StoreWorkerOutput<T> for Option<T> {
    fn store(&mut self, value: T) {
        *self = Some(value);
    }
}

impl StoreWorkerOutput<()> for bool {
    fn store(&mut self, (): ()) {
        *self = true;
    }
}

fn poll_worker<T>(
    worker: &mut Option<PipeWorker<T>>,
    output: &mut impl StoreWorkerOutput<T>,
) -> Result<(), ProcessError> {
    let Some(active) = worker.as_ref() else {
        return Ok(());
    };
    match active.receiver.try_recv() {
        Ok(result) => {
            let active = worker.take().expect("active worker");
            active
                .handle
                .join()
                .map_err(|_| ProcessError::CleanupFailed)?;
            output.store(result?);
            Ok(())
        }
        Err(mpsc::TryRecvError::Empty) => Ok(()),
        Err(mpsc::TryRecvError::Disconnected) => {
            let active = worker.take().expect("active worker");
            active
                .handle
                .join()
                .map_err(|_| ProcessError::CleanupFailed)?;
            Err(ProcessError::CleanupFailed)
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_child(child);
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
}

fn termination_status(status: ExitStatus) -> TerminationStatus {
    if let Some(code) = status.code() {
        return TerminationStatus::Exited { code };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if status.signal().is_some() {
            return TerminationStatus::Signalled;
        }
    }
    TerminationStatus::OtherFailure
}

/// Unix fixture utilities shared by host adapter unit tests.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    const CLEANUP_BUDGET: Duration = Duration::from_secs(2);

    pub(crate) fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub(crate) fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(path.exists(), "process did not publish {}", path.display());
    }

    pub(crate) fn process_is_alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run kill -0 to check process liveness")
            .success()
    }

    pub(crate) fn assert_recorded_process_exits(pid_path: &Path) {
        wait_for_file(pid_path);
        let pid = fs::read_to_string(pid_path).unwrap();
        let pid = pid.trim();
        let deadline = Instant::now() + CLEANUP_BUDGET;
        while process_is_alive(pid) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            !process_is_alive(pid),
            "supervised process {pid} remained alive"
        );
    }

    pub(crate) fn assert_recorded_process_is_dead(pid_path: &Path) {
        let pid = fs::read_to_string(pid_path).unwrap();
        let pid = pid.trim();
        assert!(
            !process_is_alive(pid),
            "supervised process {pid} remained alive after supervision completed"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use tempfile::tempdir;

    use super::test_support::{
        assert_recorded_process_exits, assert_recorded_process_is_dead, executable, wait_for_file,
    };
    use super::*;
    use crate::domain::NeverCancelled;

    const CLEANUP_BUDGET: Duration = Duration::from_secs(2);

    #[derive(Default)]
    struct ToggleCancellation(AtomicBool);

    impl ToggleCancellation {
        fn cancel(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    impl CancellationSignal for ToggleCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    fn request(argv: Vec<String>, cwd: PathBuf) -> ProcessRequest {
        ProcessRequest {
            argv,
            cwd,
            deadline: None,
            stdin: None,
            stdout_limit: None,
            stderr_limit: None,
            cleanup_budget: CLEANUP_BUDGET,
        }
    }

    #[test]
    fn process_spawn_failure_is_sanitized_without_starting_child() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let executable_secret = dir.path().join("private-executable");
        let cwd_secret = dir.path().join("private-working-directory");
        fs::create_dir(&cwd_secret).unwrap();
        let spawn_calls = AtomicUsize::new(0);
        let spawner = |command: &mut Command| {
            spawn_calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(command.get_program(), executable_secret.as_os_str());
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                vec![std::ffi::OsStr::new("private-argument")]
            );
            assert_eq!(command.get_current_dir(), Some(cwd_secret.as_path()));
            Err(std::io::Error::other("private raw OS error"))
        };
        let error = run_supervised_process_with_spawner(
            request(
                vec![
                    executable_secret.to_string_lossy().into_owned(),
                    "private-argument".to_owned(),
                ],
                cwd_secret.clone(),
            ),
            &NeverCancelled,
            &spawner,
        )
        .expect_err("injected spawn failure must be classified without a child");

        assert_eq!(spawn_calls.load(Ordering::Relaxed), 1);
        assert_eq!(error, ProcessError::SpawnFailed);
        assert_eq!(error.to_string(), "process spawn failed");
        let diagnostics = format!("{error:?} {error}");
        for secret in [
            executable_secret.to_string_lossy().as_ref(),
            cwd_secret.to_string_lossy().as_ref(),
            "private-argument",
            "private raw OS error",
        ] {
            assert!(!diagnostics.contains(secret));
        }
    }

    #[test]
    fn wait_failure_is_sanitized_and_reaps_process_tree() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("wait-failure-tree.sh");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'while :; do sleep 1; done' &\nprintf \"%s\\n\" \"$!\" > \"$1\"\nwait\n",
        );
        let wait_calls = AtomicUsize::new(0);
        let waiter = |child: &mut Child| {
            if wait_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                while !pid_path.exists() {
                    thread::yield_now();
                }
                Err(std::io::Error::other("private wait error"))
            } else {
                child.try_wait()
            }
        };

        let error = run_supervised_process_with_waiter(
            request(
                vec![
                    script.to_string_lossy().into_owned(),
                    pid_path.to_string_lossy().into_owned(),
                ],
                dir.path().to_path_buf(),
            ),
            &NeverCancelled,
            &waiter,
        )
        .expect_err("injected child wait failure must be classified");

        assert_eq!(error, ProcessError::WaitFailed);
        assert_eq!(
            format!("{error:?} {error}"),
            "process wait failed process wait failed"
        );
        assert!(!format!("{error:?} {error}").contains("private wait error"));
        assert_recorded_process_exits(&pid_path);
    }

    #[test]
    fn cleanup_failure_is_sanitized_and_reaps_process_tree() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let leader_pid_path = dir.path().join("leader.pid");
        let pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("cleanup-failure-tree.sh");
        executable(
            &script,
            "#!/bin/sh\nprintf \"%s\\n\" \"$$\" > \"$1\"\nsh -c 'while :; do sleep 1; done' &\nprintf \"%s\\n\" \"$!\" > \"$2\"\nwait\n",
        );
        let mut process = request(
            vec![
                script.to_string_lossy().into_owned(),
                leader_pid_path.to_string_lossy().into_owned(),
                pid_path.to_string_lossy().into_owned(),
            ],
            dir.path().to_path_buf(),
        );
        process.deadline = Some(Instant::now() + Duration::from_millis(100));
        // This deterministically overflows the real cleanup-deadline calculation
        // after the supervisor has issued its process-tree termination request.
        process.cleanup_budget = Duration::MAX;

        let error = run_supervised_process(process, &NeverCancelled)
            .expect_err("unrepresentable cleanup deadline must be classified");

        assert_eq!(error, ProcessError::CleanupFailed);
        assert_eq!(
            format!("{error:?} {error}"),
            "process cleanup failed process cleanup failed"
        );
        assert_recorded_process_exits(&leader_pid_path);
        assert_recorded_process_exits(&pid_path);
    }

    #[test]
    fn worker_spawn_failure_is_sanitized_and_reaps_owned_process_tree() {
        let _fixture_lock = process_fixture_lock();
        struct FailingWorkerSpawn {
            ready: PathBuf,
        }

        impl ReaderFactory for FailingWorkerSpawn {
            fn stdout(
                &self,
                _reader: ChildStdout,
                _limit: Option<usize>,
            ) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
                wait_for_file(&self.ready);
                Err(ProcessError::PipeReadFailed {
                    stream: OutputStream::Stdout,
                })
            }

            fn stderr(
                &self,
                reader: ChildStderr,
                limit: Option<usize>,
            ) -> Result<PipeWorker<BoundedOutput>, ProcessError> {
                spawn_reader(reader, limit, OutputStream::Stderr)
            }
        }

        let dir = tempdir().unwrap();
        let leader_pid_path = dir.path().join("leader.pid");
        let descendant_pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("worker-spawn-failure-tree.sh");
        executable(
            &script,
            "#!/bin/sh\nprintf \"%s\\n\" \"$$\" > \"$1\"\nsh -c 'while :; do sleep 1; done' &\nprintf \"%s\\n\" \"$!\" > \"$2\"\nwait\n",
        );
        let spawner = |command: &mut Command| {
            let deadline = Instant::now() + CLEANUP_BUDGET;
            loop {
                match command.spawn() {
                    Ok(child) => break Ok(child),
                    Err(_) if Instant::now() < deadline => thread::yield_now(),
                    Err(error) => break Err(error),
                }
            }
        };

        let error = run_supervised_process_with_factories(
            request(
                vec![
                    script.to_string_lossy().into_owned(),
                    leader_pid_path.to_string_lossy().into_owned(),
                    descendant_pid_path.to_string_lossy().into_owned(),
                ],
                dir.path().to_path_buf(),
            ),
            &NeverCancelled,
            &spawner,
            &FailingWorkerSpawn {
                ready: descendant_pid_path.clone(),
            },
            &SystemWaiter,
        )
        .expect_err("injected worker spawn failure must fail supervision");

        assert_eq!(
            error,
            ProcessError::PipeReadFailed {
                stream: OutputStream::Stdout,
            }
        );
        assert_eq!(
            format!("{error:?} {error}"),
            "process output read failed process output read failed"
        );
        assert_recorded_process_exits(&leader_pid_path);
        assert_recorded_process_exits(&descendant_pid_path);
    }

    #[test]
    fn already_collected_output_survives_exit_and_timeout_cleanup() {
        use std::os::unix::process::CommandExt as _;
        let _fixture_lock = process_fixture_lock();
        // One reader is complete before the first supervision poll. The waiter
        // releases the other only after that poll, forcing partial collection.
        struct ReleaseReader {
            pending: std::sync::Mutex<Option<mpsc::Sender<Result<BoundedOutput, ProcessError>>>>,
            timeout: bool,
        }
        impl Waiter for ReleaseReader {
            fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
                if let Some(sender) = self.pending.lock().unwrap().take() {
                    sender
                        .send(Ok(BoundedOutput {
                            bytes: b"late".to_vec(),
                            truncated: false,
                        }))
                        .unwrap();
                    if self.timeout {
                        return Ok(None);
                    }
                }
                child.wait().map(Some)
            }
        }
        for timeout in [false, true] {
            for early_stdout in [false, true] {
                let child = Command::new("sh")
                    .args(["-c", "exit 0"])
                    .process_group(0)
                    .spawn()
                    .unwrap();
                let (early_tx, early_rx) = mpsc::channel();
                early_tx
                    .send(Ok(BoundedOutput {
                        bytes: b"early".to_vec(),
                        truncated: true,
                    }))
                    .unwrap();
                let early = PipeWorker {
                    receiver: early_rx,
                    handle: thread::spawn(|| {}),
                };
                let (late_tx, late_rx) = mpsc::channel();
                let late = PipeWorker {
                    receiver: late_rx,
                    handle: thread::spawn(|| {}),
                };
                let (stdout, stderr) = if early_stdout {
                    (early, late)
                } else {
                    (late, early)
                };
                let outcome = supervise(
                    child,
                    timeout.then(Instant::now),
                    CLEANUP_BUDGET,
                    &NeverCancelled,
                    ProcessWorkers {
                        writer: None,
                        stdout: Some(stdout),
                        stderr: Some(stderr),
                    },
                    &ReleaseReader {
                        pending: std::sync::Mutex::new(Some(late_tx)),
                        timeout,
                    },
                )
                .unwrap();
                let (early, late) = if early_stdout {
                    (&outcome.stdout, &outcome.stderr)
                } else {
                    (&outcome.stderr, &outcome.stdout)
                };
                assert_eq!(
                    early.bytes, b"early",
                    "timeout={timeout}, stdout={early_stdout}"
                );
                assert!(early.truncated);
                assert_eq!(late.bytes, b"late");
                assert!(!late.truncated);
                assert_eq!(outcome.timed_out, timeout);
            }
        }
    }

    #[test]
    fn nonzero_exit_preserves_status_and_output() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let outcome = run_supervised_process(
            request(
                vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "printf stdout-value; printf stderr-value >&2; exit 17".to_owned(),
                ],
                dir.path().to_path_buf(),
            ),
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(outcome.status, TerminationStatus::Exited { code: 17 });
        assert_eq!(outcome.stdout.bytes, b"stdout-value");
        assert_eq!(outcome.stderr.bytes, b"stderr-value");
        assert!(!outcome.stdout.truncated);
        assert!(!outcome.stderr.truncated);
        assert!(!outcome.timed_out);
    }

    #[test]
    fn successful_leader_exit_reaps_descendant_with_closed_streams() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let command = format!(
            "sh -c 'trap \"\" TERM; while :; do sleep 1; done' </dev/null >/dev/null 2>&1 & printf '%s\\n' $! > '{}'; printf leader-output; exit 17",
            pid_path.display()
        );

        let outcome = run_supervised_process(
            request(
                vec!["sh".to_owned(), "-c".to_owned(), command],
                dir.path().to_path_buf(),
            ),
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(outcome.status, TerminationStatus::Exited { code: 17 });
        assert_eq!(outcome.stdout.bytes, b"leader-output");
        assert_recorded_process_is_dead(&pid_path);
    }

    #[test]
    fn stdout_and_stderr_are_independently_bounded() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let mut process = request(
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                // Each stream exceeds a typical pipe buffer while both writers run in
                // parallel. A supervisor which stops reading after a cap, or drains
                // one stream before the other, cannot complete this command normally.
                "(head -c 131072 /dev/zero | tr '\\000' 0) & (head -c 131072 /dev/zero | tr '\\000' e >&2) & wait"
                    .to_owned(),
            ],
            dir.path().to_path_buf(),
        );
        process.stdout_limit = Some(4);
        process.stderr_limit = Some(7);
        process.deadline = Some(Instant::now() + Duration::from_secs(2));
        let outcome = run_supervised_process(process, &NeverCancelled).unwrap();

        assert_eq!(outcome.stdout.bytes, b"0000");
        assert_eq!(outcome.stderr.bytes, b"eeeeeee");
        assert!(outcome.stdout.truncated);
        assert!(outcome.stderr.truncated);
        assert!(
            !outcome.timed_out,
            "both streams must continue draining after their retained caps"
        );
    }

    #[test]
    fn pipe_read_failure_terminates_and_reaps_process_tree() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("pipe-failure-tree.sh");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'while :; do sleep 1; done' &\nprintf \"%s\\n\" \"$!\" > \"$1\"\nwait\n",
        );
        let error = run_supervised_process_with_stdout_failure(
            request(
                vec![
                    script.to_string_lossy().into_owned(),
                    pid_path.to_string_lossy().into_owned(),
                ],
                dir.path().to_path_buf(),
            ),
            &NeverCancelled,
            pid_path.clone(),
            "private reader error".to_owned(),
        )
        .expect_err("injected reader failure must fail supervision");

        assert_eq!(
            error,
            ProcessError::PipeReadFailed {
                stream: OutputStream::Stdout,
            }
        );
        assert_eq!(
            format!("{error:?} {error}"),
            "process output read failed process output read failed"
        );
        assert_recorded_process_exits(&pid_path);
    }

    #[test]
    fn signalled_command_has_explicit_termination_status() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let outcome = run_supervised_process(
            request(
                vec!["sh".to_owned(), "-c".to_owned(), "kill -TERM $$".to_owned()],
                dir.path().to_path_buf(),
            ),
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(outcome.status, TerminationStatus::Signalled);
        assert!(!outcome.timed_out);
    }

    #[test]
    fn timed_out_process_terminates_and_reaps_descendants() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("process-tree.sh");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'trap \"\" TERM; while :; do sleep 1; done' &\nprintf \"%s\\n\" \"$!\" > \"$1\"\nwait\n",
        );
        let mut process = request(
            vec![
                script.to_string_lossy().into_owned(),
                pid_path.to_string_lossy().into_owned(),
            ],
            dir.path().to_path_buf(),
        );
        let timeout_at = Instant::now() + Duration::from_millis(100);
        process.deadline = Some(timeout_at);
        let (result_tx, result_rx) = mpsc::channel();
        thread::spawn(move || {
            result_tx
                .send(run_supervised_process(process, &NeverCancelled))
                .unwrap();
        });

        wait_for_file(&pid_path);
        let completion_deadline = timeout_at + CLEANUP_BUDGET;
        let outcome = result_rx
            .recv_timeout(completion_deadline.saturating_duration_since(Instant::now()))
            .expect("timeout cleanup must finish within its separate budget")
            .expect("timeout cleanup must produce an outcome");

        assert!(outcome.timed_out);
        assert_recorded_process_is_dead(&pid_path);
    }

    #[test]
    fn cancelled_process_terminates_and_reaps_process_tree() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let script = dir.path().join("cancel-tree.sh");
        executable(
            &script,
            "#!/bin/sh\nsh -c 'trap \"\" TERM; while :; do sleep 1; done' &\nprintf \"%s\\n\" \"$!\" > \"$1\"\nwait\n",
        );
        let process = request(
            vec![
                script.to_string_lossy().into_owned(),
                pid_path.to_string_lossy().into_owned(),
            ],
            dir.path().to_path_buf(),
        );
        let cancellation = Arc::new(ToggleCancellation::default());
        let thread_cancellation = Arc::clone(&cancellation);
        let (result_tx, result_rx) = mpsc::channel();
        thread::spawn(move || {
            result_tx
                .send(run_supervised_process(
                    process,
                    thread_cancellation.as_ref(),
                ))
                .unwrap();
        });

        wait_for_file(&pid_path);
        let cancellation_at = Instant::now();
        cancellation.cancel();
        let error = result_rx
            .recv_timeout(CLEANUP_BUDGET.saturating_sub(cancellation_at.elapsed()))
            .expect("cancelled cleanup must finish within its separate budget")
            .expect_err("cancellation must return a typed error");

        assert_eq!(error, ProcessError::Cancelled);
        assert_eq!(format!("{error:?}"), "process execution cancelled");
        assert_recorded_process_is_dead(&pid_path);
    }

    #[test]
    fn stdin_write_failure_terminates_and_reaps_process_tree() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("parent.pid");
        let script = format!(
            "printf '%s\\n' $$ > '{}'; exec 0<&-; while :; do sleep 1; done",
            pid_path.display()
        );
        let mut process = request(
            vec!["sh".to_owned(), "-c".to_owned(), script],
            dir.path().to_path_buf(),
        );
        process.stdin = Some(vec![b'x'; 1024 * 1024]);
        let error = run_supervised_process(process, &NeverCancelled)
            .expect_err("closed child stdin must report the writer failure");

        assert_eq!(error, ProcessError::StdinWriteFailed);
        assert_eq!(error.to_string(), "process input write failed");
        assert_recorded_process_exits(&pid_path);
    }

    #[test]
    fn cancellation_takes_precedence_over_observed_exit() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let cancellation = ToggleCancellation::default();
        cancellation.cancel();
        let error = run_supervised_process(
            request(
                vec!["sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()],
                dir.path().to_path_buf(),
            ),
            &cancellation,
        )
        .expect_err("observed cancellation must win over child exit");

        assert_eq!(error, ProcessError::Cancelled);
    }

    #[test]
    fn cancellation_takes_precedence_over_expired_deadline() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let cancellation = ToggleCancellation::default();
        cancellation.cancel();
        let mut process = request(
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "while :; do :; done".to_owned(),
            ],
            dir.path().to_path_buf(),
        );
        process.deadline = Some(Instant::now());
        let error = run_supervised_process(process, &cancellation)
            .expect_err("observed cancellation must win over deadline expiry");

        assert_eq!(error, ProcessError::Cancelled);
    }

    #[test]
    fn timeout_race_produces_one_terminal_outcome() {
        let _fixture_lock = process_fixture_lock();
        let dir = tempdir().unwrap();
        let mut process = request(
            vec!["sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()],
            dir.path().to_path_buf(),
        );
        process.deadline = Some(Instant::now());
        let outcome = run_supervised_process(process, &NeverCancelled).unwrap();

        if !outcome.timed_out {
            assert_eq!(outcome.status, TerminationStatus::Exited { code: 0 });
        }
    }
}
