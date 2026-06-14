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
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::adapters::config::{FormatterConfig, FormatterMode};
use crate::adapters::fs::durable_write;

/// Subprocess timeout — matches the 11d epoch deadline of ~30 s.
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);

// ── FormatJob ─────────────────────────────────────────────────────────────────

/// Describes a single format job (not currently persisted, used for clarity).
#[derive(Debug)]
pub struct FormatJob {
    pub path: String,
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
    pub fn run(&self, relative_path: &str) -> bool {
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
            FormatterMode::Filter => self.run_filter_job(relative_path, &abs_path_str, &tool.argv),
            FormatterMode::Inplace => {
                self.run_inplace_job(relative_path, &abs_path_str, &tool.argv)
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
    fn run_filter_job(&self, relative_path: &str, abs_path: &str, argv: &[String]) -> bool {
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
        let out = match run_filter_command(&substituted, &bytes, cwd) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[tower/fmt] tool error for {relative_path}: {e}");
                return false;
            }
        };

        // Step 3: if output == input, nothing to write.
        if out == bytes {
            return true;
        }

        // Step 4: CAS check — re-read file and hash.
        let current_bytes = match fs::read(abs_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tower/fmt] CAS re-read failed for {relative_path}: {e}");
                return false;
            }
        };
        let h_current = hash_bytes(&current_bytes);
        if h_current != h0 {
            eprintln!(
                "[tower/fmt] CAS abort for {relative_path}: file was modified by a concurrent writer"
            );
            // Not a tool failure — concurrent write won, which is correct.
            return true;
        }

        // Step 5: atomic write via shadow → fsync → rename.
        let dest = Path::new(abs_path);
        let tmp_path = PathBuf::from(format!("{abs_path}.tmp_write"));
        if let Err(e) = durable_write(dest, &tmp_path, &out) {
            eprintln!("[tower/fmt] atomic write failed for {relative_path}: {e}");
            return false;
        }

        // Step 6: insert the RELATIVE path into echo-suppression set.
        // The relative path is what `PluginHostRegistry::fan_out` receives from the
        // watcher's `on_file_changed` broadcast — the two must match for suppression
        // to fire. The rename already happened inside atomic_write; this is
        // best-effort. There is a narrow window where the watcher sees the rename
        // before we insert — the worst case is one extra (harmless) format call.
        {
            let mut es = self.echo_set.lock().unwrap_or_else(|p| p.into_inner());
            es.insert(relative_path.to_owned(), ());
        }

        true
    }

    /// Run an inplace-mode job: copy file to a temp path, run the tool, read back.
    ///
    /// `relative_path` is the workspace-relative path (used as the echo-set key).
    /// `abs_path` is the resolved absolute path (used for all `std::fs` operations).
    ///
    /// Returns `true` if the job completed without error.
    fn run_inplace_job(&self, relative_path: &str, abs_path: &str, argv: &[String]) -> bool {
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
        let ok = match run_inplace_command(&substituted, cwd) {
            Ok(success) => success,
            Err(e) => {
                eprintln!("[tower/fmt] inplace tool error for {relative_path}: {e}");
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

        // Step 5: no-op if identical.
        if out == bytes {
            return true;
        }

        // Step 6: CAS check.
        let current_bytes = match fs::read(abs_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[tower/fmt] CAS re-read failed for {relative_path}: {e}");
                return false;
            }
        };
        let h_current = hash_bytes(&current_bytes);
        if h_current != h0 {
            eprintln!("[tower/fmt] CAS abort for {relative_path}: concurrent write detected");
            return true;
        }

        // Step 7: atomic write.
        let dest = Path::new(abs_path);
        let shadow_path = PathBuf::from(format!("{abs_path}.tmp_write"));
        if let Err(e) = durable_write(dest, &shadow_path, &out) {
            eprintln!("[tower/fmt] atomic write failed for {relative_path}: {e}");
            return false;
        }

        // Step 8: echo suppression — keyed on the relative path to match fan_out.
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
fn run_filter_command(argv: &[String], input: &[u8], cwd: &Path) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!("tool not found: '{}' (check PATH)", argv[0]));
        }
        Err(e) => return Err(format!("spawn failed: {e}")),
    };

    // Write stdin in a separate scope to close the pipe.
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(input)
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        // Broken pipe is acceptable when the tool exits early; only other
        // write errors are propagated.
        return Err(format!("stdin write error: {e}"));
    }

    // Wait with a timeout via a watchdog thread.
    let output = wait_with_timeout(child)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[tower/fmt] tool exited {}: {stderr}", output.status);
        return Err(format!("exit {}", output.status));
    }

    Ok(output.stdout)
}

/// Run an inplace-mode command: the tool writes to the temp copy in place.
///
/// Returns `Ok(true)` on exit 0, `Ok(false)` on non-zero exit (stderr logged),
/// `Err(message)` on spawn failure.
fn run_inplace_command(argv: &[String], cwd: &Path) -> Result<bool, String> {
    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!("tool not found: '{}' (check PATH)", argv[0]));
        }
        Err(e) => return Err(format!("spawn failed: {e}")),
    };

    let output = wait_with_timeout(child)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "[tower/fmt] inplace tool exited {}: {stderr}",
            output.status
        );
        return Ok(false);
    }

    Ok(true)
}

/// Collect output from a child process, killing it if it exceeds the timeout.
fn wait_with_timeout(mut child: std::process::Child) -> Result<std::process::Output, String> {
    // We implement a timeout via a watchdog thread that kills the child after
    // SUBPROCESS_TIMEOUT. This avoids pulling in a tokio/async runtime for
    // what is otherwise a simple std::thread worker pool.
    let start = std::time::Instant::now();
    let deadline = start + SUBPROCESS_TIMEOUT;

    // Poll for completion at 10 ms intervals.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!(
                        "subprocess timed out after {}s",
                        SUBPROCESS_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("wait error: {e}")),
        }
    }

    child
        .wait_with_output()
        .map_err(|e| format!("wait_with_output: {e}"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;

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
        let result = worker.run_inplace_job(&path_str, &path_str, &argv);

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
        let result = worker.run_filter_job(&path_str, &path_str, &argv);

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
        let result = worker.run_filter_job(&path_str, &path_str, &argv);

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
        let result = worker.run_filter_job(&path_str, &path_str, &argv);

        assert!(!result, "missing tool must return false");
        let content = fs::read(&file_path).unwrap();
        assert_eq!(content, b"content\n", "file unchanged after missing tool");
    }
}
