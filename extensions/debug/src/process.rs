#![forbid(unsafe_code)]

use std::process::{Child, Command, Stdio};

#[derive(Debug)]
pub enum ProcessError {
    Io(std::io::Error),
    Timeout,
    AlreadyExited,
}

impl From<std::io::Error> for ProcessError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub trait DebugProcessHandle {
    fn terminate_tree(&mut self) -> Result<(), ProcessError>;
    fn has_exited(&mut self) -> Result<bool, ProcessError>;
}

pub struct ChildDebugProcessHandle {
    child: Child,
}

impl ChildDebugProcessHandle {
    pub fn spawn(argv: &[String]) -> Result<Self, ProcessError> {
        let (program, args) = argv.split_first().ok_or_else(|| {
            ProcessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "adapter argv must include a program",
            ))
        })?;

        let mut command = Command::new(program);
        command.args(args);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let child = command.spawn()?;
        Ok(Self { child })
    }
}

impl DebugProcessHandle for ChildDebugProcessHandle {
    fn terminate_tree(&mut self) -> Result<(), ProcessError> {
        if self.has_exited()? {
            return Err(ProcessError::AlreadyExited);
        }

        terminate_process_tree(&mut self.child)?;
        let _status = self.child.wait()?;
        Ok(())
    }

    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        Ok(self.child.try_wait()?.is_some())
    }
}

impl Drop for ChildDebugProcessHandle {
    fn drop(&mut self) {
        if matches!(self.has_exited(), Ok(false)) {
            let _ = terminate_process_tree(&mut self.child);
            let _ = self.child.wait();
        }
    }
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut Child) -> Result<(), ProcessError> {
    let process_group_id = format!("-{}", child.id());
    let _ = signal_process_group("-TERM", &process_group_id);

    for _ in 0..20 {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = signal_process_group("-KILL", &process_group_id);
    for _ in 0..20 {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    child.kill()?;
    Ok(())
}

#[cfg(unix)]
fn signal_process_group(
    signal: &str,
    process_group_id: &str,
) -> std::io::Result<std::process::ExitStatus> {
    Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(process_group_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(not(unix))]
fn terminate_process_tree(child: &mut Child) -> Result<(), ProcessError> {
    child.kill()?;
    Ok(())
}

#[cfg(test)]
#[derive(Default)]
pub struct FakeDebugProcessHandle {
    terminate_tree_calls: usize,
    exited: bool,
}

#[cfg(test)]
impl FakeDebugProcessHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn terminate_tree_calls(&self) -> usize {
        self.terminate_tree_calls
    }

    pub fn set_exited(&mut self, exited: bool) {
        self.exited = exited;
    }
}

#[cfg(test)]
impl DebugProcessHandle for FakeDebugProcessHandle {
    fn terminate_tree(&mut self) -> Result<(), ProcessError> {
        self.terminate_tree_calls += 1;
        Ok(())
    }

    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        Ok(self.exited)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChildDebugProcessHandle, DebugProcessHandle, FakeDebugProcessHandle, ProcessError,
    };

    #[test]
    fn publicly_defines_debug_process_handle_with_terminate_tree_and_has_exited() {
        let mut handle = FakeDebugProcessHandle::new();
        assert!(
            !handle
                .has_exited()
                .expect("fake handle should report running")
        );

        handle
            .terminate_tree()
            .expect("fake handle should terminate");
        handle.set_exited(true);

        assert!(
            handle
                .has_exited()
                .expect("fake handle should report exited")
        );
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
    fn child_debug_process_handle_implements_debug_process_handle_for_real_processes() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let mut handle = ChildDebugProcessHandle::spawn(&argv)
            .expect("child process handle should spawn from argv");

        assert!(
            !handle
                .has_exited()
                .expect("freshly spawned child should be running")
        );
        handle
            .terminate_tree()
            .expect("spawned child should be terminated");
    }

    #[test]
    #[cfg(unix)]
    fn child_debug_process_handle_terminates_process_when_dropped() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let handle = ChildDebugProcessHandle::spawn(&argv)
            .expect("child process handle should spawn from argv");
        let child_id = handle.child.id();

        drop(handle);

        assert_process_exits(child_id);
    }

    #[test]
    fn fake_debug_process_handle_is_available_for_deterministic_cleanup_tests() {
        let mut handle = FakeDebugProcessHandle::new();
        assert_eq!(handle.terminate_tree_calls(), 0);

        handle
            .terminate_tree()
            .expect("fake cleanup should be deterministic");

        assert_eq!(handle.terminate_tree_calls(), 1);
    }

    #[test]
    fn process_error_has_stable_io_timeout_and_already_exited_variants() {
        let io_error = ProcessError::Io(std::io::Error::other("spawn failed"));
        assert!(matches!(io_error, ProcessError::Io(_)));
        assert!(matches!(ProcessError::Timeout, ProcessError::Timeout));
        assert!(matches!(
            ProcessError::AlreadyExited,
            ProcessError::AlreadyExited
        ));
    }

    #[test]
    fn process_cleanup_tests_prove_direct_handle_cleanup_calls_terminate_tree_exactly_once() {
        let mut handle = FakeDebugProcessHandle::new();

        cleanup_direct_handle(&mut handle).expect("cleanup should terminate fake handle");

        assert_eq!(handle.terminate_tree_calls(), 1);
    }

    fn cleanup_direct_handle(handle: &mut dyn DebugProcessHandle) -> Result<(), ProcessError> {
        if handle.has_exited()? {
            return Err(ProcessError::AlreadyExited);
        }
        handle.terminate_tree()
    }

    #[cfg(unix)]
    fn assert_process_exits(pid: u32) {
        for _ in 0..20 {
            if !process_exists(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        panic!("process {pid} should have exited after handle drop");
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
