use super::process_binary::selected_binary;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use core_engine::adapters::cli::GlobalOpts;
use core_engine::adapters::daemon::client::send_control_async;
use core_engine::adapters::daemon::socket::{
    OwnerLiveness, probe_owner, socket_path, wait_for_socket,
};
use core_engine::adapters::daemon::wire::StartupInstrumentationSnapshot;
use core_engine::adapters::daemon::wire::StatusSnapshot;
use core_engine::adapters::daemon::wire::{
    ClientRole, ControlRequest, ControlResponse, Handshake, read_line_capped, write_line,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::{TempDir, tempdir};
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;

type PersistedEntries = Vec<(Vec<u8>, Vec<u8>)>;

fn install_slow_start_extension_from(workspace: &Path, binary: &Path) {
    let extension_dir = workspace.join(".tower/extensions/slow_start");
    fs::create_dir_all(&extension_dir).expect("create slow-start extension directory");
    let binary = binary.to_string_lossy().replace('\\', "\\\\");
    let manifest = fs::read_to_string(
        super::workspace_root().join("extensions/fixtures/slow_start/extension.toml"),
    )
    .expect("read slow-start manifest")
    .lines()
    .map(|line| {
        if line.trim_start().starts_with("command") {
            format!("command = [\"{binary}\"]")
        } else {
            line.to_owned()
        }
    })
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(extension_dir.join("extension.toml"), manifest).expect("write slow-start manifest");
}

fn child_process_ids(parent_id: u32) -> Vec<u32> {
    // Linux attributes children to the spawning thread, which need not be
    // the daemon's main thread (extensions spawn on its preparation worker).
    let mut children = Vec::new();
    let tasks = match fs::read_dir(format!("/proc/{parent_id}/task")) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return children,
        Err(error) => panic!("inspect daemon threads through procfs: {error}"),
    };
    for task in tasks {
        let task = task.expect("daemon task entry");
        let Ok(pids) = fs::read_to_string(task.path().join("children")) else {
            // A worker can exit between enumerating tasks and reading children.
            continue;
        };
        children.extend(
            pids.split_whitespace()
                .map(|pid| pid.parse::<u32>().expect("procfs child pid")),
        );
    }
    children.sort_unstable();
    children.dedup();
    children
}

fn find_named_thread(process_id: u32, expected_name: &str) -> Option<u32> {
    let tasks = fs::read_dir(format!("/proc/{process_id}/task")).ok()?;
    tasks.filter_map(Result::ok).find_map(|entry| {
        let name = fs::read_to_string(entry.path().join("comm")).ok()?;
        (name.trim() == expected_name).then(|| {
            entry
                .file_name()
                .to_string_lossy()
                .parse()
                .expect("numeric Linux thread id")
        })
    })
}

/// Inputs collected from the caller-selected storage location.  Keeping the
/// validation pure lets the evidence test cover every rejected mount case
/// without depending on the host's mount table.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct RealDiskCandidate {
    pub(crate) tmpdir: PathBuf,
    pub(crate) source: String,
    pub(crate) target: PathBuf,
    pub(crate) filesystem: String,
    pub(crate) writable: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum RealDiskValidationError {
    RelativeTmpdir,
    TmpDirectory,
    MemoryFilesystem(String),
    Unwritable(PathBuf),
}

impl std::fmt::Display for RealDiskValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelativeTmpdir => write!(formatter, "TMPDIR must be an absolute path"),
            Self::TmpDirectory => write!(formatter, "TMPDIR must not resolve to /tmp"),
            Self::MemoryFilesystem(filesystem) => {
                write!(formatter, "TMPDIR filesystem must not be {filesystem}")
            }
            Self::Unwritable(path) => {
                write!(formatter, "TMPDIR is not writable: {}", path.display())
            }
        }
    }
}

pub(crate) fn validate_real_disk_candidate(
    candidate: RealDiskCandidate,
) -> Result<VerifiedRealDisk, RealDiskValidationError> {
    if !candidate.tmpdir.is_absolute() {
        return Err(RealDiskValidationError::RelativeTmpdir);
    }
    if candidate.target == Path::new("/tmp") {
        return Err(RealDiskValidationError::TmpDirectory);
    }
    if matches!(candidate.filesystem.as_str(), "tmpfs" | "ramfs") {
        return Err(RealDiskValidationError::MemoryFilesystem(
            candidate.filesystem,
        ));
    }
    if !candidate.writable {
        return Err(RealDiskValidationError::Unwritable(candidate.tmpdir));
    }
    Ok(VerifiedRealDisk {
        tmpdir: candidate.tmpdir,
        source: candidate.source,
        target: candidate.target,
        filesystem: candidate.filesystem,
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct BinaryCandidate {
    pub(crate) tower_path: PathBuf,
    pub(crate) tower_sha256: String,
    pub(crate) slow_start_extension_path: PathBuf,
    pub(crate) slow_start_extension_sha256: String,
    pub(crate) target_dir: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum BinaryProvenanceError {
    ForeignTargetDirectory(PathBuf),
    DigestChanged { binary: &'static str },
}

pub(crate) fn validate_binary_provenance(
    expected: &BinaryCandidate,
    observed: BinaryCandidate,
) -> Result<BinaryProvenance, BinaryProvenanceError> {
    for path in [&observed.tower_path, &observed.slow_start_extension_path] {
        if path.parent() != Some(observed.target_dir.as_path()) {
            return Err(BinaryProvenanceError::ForeignTargetDirectory(path.clone()));
        }
    }
    if observed.tower_path != expected.tower_path || observed.tower_sha256 != expected.tower_sha256
    {
        return Err(BinaryProvenanceError::DigestChanged { binary: "tower" });
    }
    if observed.slow_start_extension_path != expected.slow_start_extension_path
        || observed.slow_start_extension_sha256 != expected.slow_start_extension_sha256
    {
        return Err(BinaryProvenanceError::DigestChanged {
            binary: "slow_start_extension",
        });
    }
    Ok(BinaryProvenance {
        tower_path: observed.tower_path,
        tower_sha256: observed.tower_sha256,
        slow_start_extension_path: observed.slow_start_extension_path,
        slow_start_extension_sha256: observed.slow_start_extension_sha256,
    })
}

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct VerifiedRealDisk {
    pub(crate) tmpdir: PathBuf,
    pub(crate) source: String,
    pub(crate) target: PathBuf,
    pub(crate) filesystem: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct BinaryProvenance {
    pub(crate) tower_path: PathBuf,
    pub(crate) tower_sha256: String,
    pub(crate) slow_start_extension_path: PathBuf,
    pub(crate) slow_start_extension_sha256: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct StartupEvidenceRun {
    pub(crate) connection_deadline_ms: u64,
    pub(crate) initialize_elapsed_ms: u64,
    pub(crate) catch_up_release_elapsed_ms: u64,
    pub(crate) generation: u64,
    pub(crate) capabilities_catching_up_ms: u64,
    pub(crate) instrumentation: StartupInstrumentationSnapshot,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct StartupEvidenceArtifact {
    pub(crate) disk: VerifiedRealDisk,
    pub(crate) binaries: BinaryProvenance,
    pub(crate) runs: [StartupEvidenceRun; 3],
}

#[allow(dead_code)]
pub(crate) struct ReconciliationProcessHarness {
    workspace: TempDir,
    configuration: SlowStartInitializeConfiguration,
    child: Option<Child>,
    mcp: Option<UnixStream>,
    logs_path: PathBuf,
    logs: CapturedProcessLogs,
    last_generation: u64,
    last_initialize_elapsed_ms: u64,
    committed_events: u64,
    resources_released: bool,
    seed_ready_fixture: bool,
    process_guard: Option<tokio::sync::MutexGuard<'static, ()>>,
    evidence_binaries: Option<BinaryProvenance>,
    watcher_thread: Option<(u32, u32)>,
    sidecar_pids: Vec<u32>,
    generation_started: Option<Instant>,
    catch_up_release_elapsed_ms: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SlowStartInitializeConfiguration {
    Block,
    Resume,
    Fail,
    Crash,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum StartupMilestone {
    TransportBound,
    WorkspaceReady,
    CatchUpBlocked,
    CatchUpReleased,
    PreparationCancelled,
    ResourcesReleased,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct CatchUpProbe {
    pub(crate) released: bool,
    pub(crate) pending_fences: u64,
    pub(crate) processed_events: u64,
    pub(crate) processed_ordinals: Vec<u64>,
    pub(crate) barrier_observed: bool,
}

/// Publicly observable source of a committed workspace change.  The harness
/// deliberately exposes the producer boundary rather than letting callers
/// synthesize extension events.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum CommittedChangeSource {
    Watcher,
    Mcp,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum CommittedChangeKind {
    Create,
    Change,
    Delete,
}

/// Release evidence is intentionally public-resource oriented: no test is
/// allowed to infer cleanup from a private coordinator state.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct ReleasedResources {
    pub(crate) sled: bool,
    pub(crate) watcher: bool,
    pub(crate) socket: bool,
    pub(crate) sidecars: bool,
    pub(crate) ownership: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct CapturedProcessLogs {
    pub(crate) daemon: String,
    pub(crate) sidecars: String,
}

#[allow(dead_code)]
impl ReconciliationProcessHarness {
    pub(crate) fn verified_real_disk_from_tmpdir() -> VerifiedRealDisk {
        let configured = std::env::var_os("TMPDIR").expect("TMPDIR must be explicitly set");
        let tmpdir = PathBuf::from(configured);
        assert!(tmpdir.is_absolute(), "TMPDIR must be an absolute path");
        let tmpdir = tmpdir.canonicalize().expect("canonicalize TMPDIR");
        assert!(tmpdir.is_dir(), "TMPDIR must name an existing directory");

        let mount = Command::new("findmnt")
            .args([
                "--noheadings",
                "--output",
                "SOURCE,TARGET,FSTYPE",
                "--target",
            ])
            .arg(&tmpdir)
            .output()
            .expect("run findmnt for TMPDIR");
        assert!(
            mount.status.success(),
            "findmnt failed for TMPDIR: {}",
            String::from_utf8_lossy(&mount.stderr)
        );
        let output = String::from_utf8(mount.stdout).expect("findmnt output must be UTF-8");
        let mut fields = output.split_whitespace();
        let source = fields.next().expect("findmnt source").to_owned();
        let target = PathBuf::from(fields.next().expect("findmnt target"));
        let filesystem = fields.next().expect("findmnt filesystem").to_owned();
        assert!(
            fields.next().is_none(),
            "unexpected findmnt output: {output:?}"
        );

        let writable_probe = tempfile::Builder::new()
            .prefix(".tower-write-probe-")
            .tempfile_in(&tmpdir);
        let writable = writable_probe.is_ok();
        drop(writable_probe);
        validate_real_disk_candidate(RealDiskCandidate {
            tmpdir,
            source,
            target,
            filesystem,
            writable,
        })
        .unwrap_or_else(|error| panic!("unusable evidence TMPDIR: {error}"))
    }

    pub(crate) fn binary_provenance() -> BinaryProvenance {
        let target_dir = super::workspace_root().join("target/debug");
        let target_dir = target_dir
            .canonicalize()
            .expect("canonicalize current-checkout target directory");
        let tower_path = target_dir
            .join("tower")
            .canonicalize()
            .expect("resolve current-checkout tower binary; build workspace binaries first");
        let slow_start_extension_path = target_dir
            .join("slow_start_extension")
            .canonicalize()
            .expect("resolve current-checkout slow-start binary; build workspace binaries first");
        let digest = |path: &Path| {
            Sha256::digest(fs::read(path).expect("read evidence binary"))
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let observed = BinaryCandidate {
            tower_sha256: digest(&tower_path),
            slow_start_extension_sha256: digest(&slow_start_extension_path),
            tower_path,
            slow_start_extension_path,
            target_dir,
        };
        validate_binary_provenance(&observed, observed.clone())
            .expect("current-checkout binaries must share the expected target directory")
    }

    pub(crate) fn new_on_verified_disk(
        disk: &VerifiedRealDisk,
        binaries: BinaryProvenance,
        configuration: SlowStartInitializeConfiguration,
    ) -> Self {
        let workspace = tempfile::Builder::new()
            .prefix("tower-startup-evidence-")
            .tempdir_in(&disk.tmpdir)
            .expect("create evidence workspace beneath verified TMPDIR");
        let logs_path = workspace.path().join(".tower/reconciliation-process.log");
        Self {
            workspace,
            configuration,
            child: None,
            mcp: None,
            logs_path,
            logs: CapturedProcessLogs {
                daemon: String::new(),
                sidecars: String::new(),
            },
            last_generation: 0,
            last_initialize_elapsed_ms: 0,
            committed_events: 0,
            resources_released: false,
            seed_ready_fixture: false,
            process_guard: None,
            evidence_binaries: Some(binaries),
            watcher_thread: None,
            sidecar_pids: Vec::new(),
            generation_started: None,
            catch_up_release_elapsed_ms: None,
        }
    }

    pub(crate) async fn collect_startup_evidence(&mut self) -> StartupEvidenceRun {
        self.start_generation().await;
        let initialized_ms = self.initialize_elapsed_ms();
        let blocked = self.observe_probe().await;
        assert!(!blocked.released, "catch-up unexpectedly released early");
        self.release_probe().await;
        self.await_milestone(
            StartupMilestone::CatchUpReleased,
            Instant::now() + Duration::from_secs(30),
        )
        .await;
        let status = self.status().await;
        let capabilities_catching_up_ms = status
            .instrumentation
            .phases
            .iter()
            .find(|timing| {
                timing.phase == core_engine::adapters::daemon::StartupPhase::CapabilitiesCatchingUp
            })
            .expect("completed catch-up phase timing")
            .duration_ms;
        StartupEvidenceRun {
            connection_deadline_ms: 2_000,
            initialize_elapsed_ms: initialized_ms,
            catch_up_release_elapsed_ms: self
                .catch_up_release_elapsed_ms
                .expect("catch-up release milestone"),
            generation: status.generation,
            capabilities_catching_up_ms,
            instrumentation: status.instrumentation,
        }
    }

    pub(crate) fn write_startup_evidence(&self, artifact: &StartupEvidenceArtifact) -> PathBuf {
        let root = self.evidence_root();
        fs::create_dir_all(&root).expect("create startup evidence directory");
        let raw_runs = artifact
            .runs
            .iter()
            .enumerate()
            .map(|(index, run)| {
                let replay = run
                    .instrumentation
                    .extension_replays
                    .iter()
                    .find(|metrics| metrics.extension_id.as_str() == "slow_start")
                    .expect("slow-start replay metrics");
                json!({
                    "ordinal": index + 1,
                    "generation": run.generation,
                    "connection_deadline_ms": run.connection_deadline_ms,
                    "initialize_elapsed_ms": run.initialize_elapsed_ms,
                    "catch_up_release_elapsed_ms": run.catch_up_release_elapsed_ms,
                    "capabilities_catching_up_ms": run.capabilities_catching_up_ms,
                    "event_count": replay.event_count,
                    "duration_ms": replay.duration_ms,
                })
            })
            .collect::<Vec<_>>();
        let document = json!({
            "filesystem": {
                "source": artifact.disk.source,
                "target": artifact.disk.target,
                "type": artifact.disk.filesystem,
            },
            "binaries": {
                "tower": {
                    "path": artifact.binaries.tower_path,
                    "sha256": artifact.binaries.tower_sha256,
                },
                "slow_start_extension": {
                    "path": artifact.binaries.slow_start_extension_path,
                    "sha256": artifact.binaries.slow_start_extension_sha256,
                },
            },
            "raw_runs": raw_runs,
        });
        let bytes = serde_json::to_vec_pretty(&document).expect("serialize startup evidence");
        assert!(
            bytes.len() <= 64 * 1024,
            "startup evidence exceeds size bound"
        );
        let path = root.join("slow-extension-replay.json");
        fs::write(&path, bytes).expect("write startup evidence artifact");
        path
    }

    pub(crate) fn new(configuration: SlowStartInitializeConfiguration) -> Self {
        let workspace = tempdir().expect("create reconciliation process workspace");
        let logs_path = workspace.path().join(".tower/reconciliation-process.log");
        Self {
            workspace,
            configuration,
            child: None,
            mcp: None,
            logs_path,
            logs: CapturedProcessLogs {
                daemon: String::new(),
                sidecars: String::new(),
            },
            last_generation: 0,
            last_initialize_elapsed_ms: 0,
            committed_events: 0,
            resources_released: false,
            seed_ready_fixture: true,
            process_guard: None,
            evidence_binaries: None,
            watcher_thread: None,
            sidecar_pids: Vec::new(),
            generation_started: None,
            catch_up_release_elapsed_ms: None,
        }
    }

    pub(crate) fn new_with_files(
        configuration: SlowStartInitializeConfiguration,
        files: &[(&str, &str)],
    ) -> Self {
        let mut harness = Self::new(configuration);
        harness.seed_ready_fixture = false;
        for (path, content) in files {
            harness.seed_file(path, content);
        }
        harness
    }

    pub(crate) async fn await_indexed_path(
        &mut self,
        path: &str,
        present: bool,
        deadline: Instant,
    ) {
        super::await_watcher_path(
            self.mcp.as_mut().expect("active MCP connection"),
            20_000,
            path,
            present,
            deadline,
        )
        .await;
    }

    pub(crate) fn workspace(&self) -> PathBuf {
        self.workspace.path().to_path_buf()
    }

    pub(crate) fn seed_file(&self, path: &str, content: &str) {
        fs::write(self.workspace.path().join(path), content)
            .expect("seed a real startup replay input");
    }

    pub(crate) fn evidence_root(&self) -> PathBuf {
        self.workspace.path().join(".tower/perf-evidence")
    }

    /// A clean cold-start run retains its workspace and configuration but must
    /// not retain the exited child or probe evidence into the next generation.
    pub(crate) fn prepare_next_cold_start(&mut self) {
        assert!(
            self.resources_released,
            "previous generation must be released"
        );
        self.child.take();
    }

    pub(crate) async fn start_generation(&mut self) {
        self.catch_up_release_elapsed_ms = None;
        assert!(
            self.child.is_none(),
            "a daemon generation is already active"
        );
        if self.process_guard.is_none() {
            self.process_guard = Some(super::slow_start_process_lock().lock().await);
        }
        self.watcher_thread = None;
        self.sidecar_pids.clear();
        let binary = self.prepare_generation_binary();
        self.seed_generation_workspace();
        // Setup, process serialization and binary hashing are not part of the
        // client connection/initialize interval. Release uses this same clock.
        let initialize_started = Instant::now();
        self.generation_started = Some(initialize_started);
        self.child = Some(self.spawn_generation(&binary));
        self.mcp = Some(self.initialize_generation().await);
        self.last_initialize_elapsed_ms = initialize_started.elapsed().as_millis() as u64;
        self.resources_released = false;
        self.last_generation = self.status().await.generation;
        self.observe_owned_processes();
    }

    fn prepare_generation_binary(&self) -> PathBuf {
        if let Some(expected) = &self.evidence_binaries {
            let observed = Self::binary_provenance();
            assert_eq!(
                &observed, expected,
                "attested binary paths and digests changed before launch"
            );
            install_slow_start_extension_from(
                self.workspace.path(),
                &expected.slow_start_extension_path,
            );
            expected.tower_path.clone()
        } else {
            super::install_slow_start_extension(self.workspace.path());
            super::selected_production_binary()
        }
    }

    fn seed_generation_workspace(&self) {
        fs::create_dir_all(self.workspace.path().join(".tower"))
            .expect("create excluded daemon state directory");
        fs::write(
            self.workspace.path().join(".tower/config.toml"),
            format!(
                "[daemon]\nidle_timeout_secs = 300\nconnection_timeout_secs = 2\npreparation_timeout_secs = {}\n\
                 [extensions]\nrequest_timeout_secs = {}\n\
                 [extensions.readiness]\nslow_start = \"ast\"\n\
                 [extensions.readiness_controls]\nslow_start = [\"probe_catch_up\"]\n\
                 [extensions.initialize.slow_start]\nmode = \"{}\"\n",
                if self.seed_ready_fixture { 1 } else { 30 },
                if self.seed_ready_fixture { 1 } else { 30 },
                self.configuration.as_str(),
            ),
        )
        .expect("write slow-start daemon configuration");

        if self.seed_ready_fixture
            && self.configuration != SlowStartInitializeConfiguration::Fail
            && !self.workspace.path().join("ready.rs").exists()
        {
            fs::write(
                self.workspace.path().join("ready.rs"),
                "pub fn ready() {}\n",
            )
            .expect("seed deterministic startup replay");
        }
    }

    fn spawn_generation(&self, tower_binary: &Path) -> Child {
        let log = File::create(&self.logs_path).expect("create excluded daemon log");
        Command::new(tower_binary)
            .arg("daemon")
            .arg("--workspace-dir")
            .arg(self.workspace.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn selected production daemon")
    }

    async fn initialize_generation(&mut self) -> UnixStream {
        let mut mcp = match wait_for_socket(
            &socket_path(self.workspace.path()),
            Duration::from_secs(5),
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => {
                if let Some(child) = self.child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                panic!("daemon did not bind its public socket: {error}");
            }
        };
        mcp.write_all(Handshake::new(ClientRole::Mcp).to_line().as_bytes())
            .await
            .expect("send MCP handshake");
        write_line(
            &mut mcp,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "reconciliation-process", "version": "1"}
                }
            })
            .to_string(),
        )
        .await
        .expect("send MCP initialize");
        read_line_capped(&mut mcp, 1 << 20)
            .await
            .expect("read MCP initialize")
            .expect("daemon returned MCP initialize response");
        mcp
    }

    fn observe_owned_processes(&mut self) {
        let Some(child) = &self.child else {
            return;
        };
        let pid = child.id();
        if let Some(thread) = find_named_thread(pid, "tower-watcher") {
            self.watcher_thread = Some((pid, thread));
        }
        self.sidecar_pids.extend(child_process_ids(pid));
        self.sidecar_pids.sort_unstable();
        self.sidecar_pids.dedup();
    }

    pub(crate) fn initialize_elapsed_ms(&self) -> u64 {
        self.last_initialize_elapsed_ms
    }

    pub(crate) fn catch_up_release_elapsed_ms(&self) -> Option<u64> {
        self.catch_up_release_elapsed_ms
    }

    pub(crate) async fn join_generation(&mut self) {
        let mut joined =
            wait_for_socket(&socket_path(self.workspace.path()), Duration::from_secs(2))
                .await
                .expect("join the active daemon generation");
        joined
            .write_all(Handshake::new(ClientRole::Control).to_line().as_bytes())
            .await
            .expect("send joined control handshake");
        write_line(
            &mut joined,
            &serde_json::to_string(&ControlRequest::Status).expect("encode joined status"),
        )
        .await
        .expect("send joined status");
        let response = read_line_capped(&mut joined, 1 << 20)
            .await
            .expect("read joined status")
            .expect("joined client received status");
        let ControlResponse::Status(status) =
            serde_json::from_str(&response).expect("decode joined status")
        else {
            panic!("joined client received a non-status response");
        };
        assert_eq!(status.generation, self.last_generation);
    }

    pub(crate) async fn await_milestone(&mut self, milestone: StartupMilestone, deadline: Instant) {
        loop {
            let reached = match milestone {
                StartupMilestone::TransportBound => socket_path(self.workspace.path()).exists(),
                StartupMilestone::WorkspaceReady => matches!(
                    self.try_status().await,
                    Some(status)
                        if status.workspace == core_engine::adapters::daemon::CapabilityReadiness::Ready
                ),
                StartupMilestone::CatchUpBlocked => self
                    .try_probe(false)
                    .await
                    .is_some_and(|probe| !probe.released && probe.pending_fences > 0),
                StartupMilestone::CatchUpReleased => {
                    self.try_probe(false).await.is_some_and(|probe| probe.released && probe.barrier_observed)
                        && self.try_status().await.is_some_and(|status| {
                            status.ast == core_engine::adapters::daemon::CapabilityReadiness::Ready
                                && status.instrumentation.phases.iter().any(|phase| phase.phase == core_engine::adapters::daemon::StartupPhase::CapabilitiesCatchingUp)
                        })
                },
                StartupMilestone::PreparationCancelled => {
                    self.try_status().await.is_some_and(|status| {
                        matches!(
                            status.lifecycle,
                            core_engine::adapters::daemon::DaemonLifecycle::Failed { .. }
                                | core_engine::adapters::daemon::DaemonLifecycle::Stopping
                        ) || matches!(
                            status.ast,
                            core_engine::adapters::daemon::CapabilityReadiness::Failed { .. }
                                | core_engine::adapters::daemon::CapabilityReadiness::Degraded { .. }
                                | core_engine::adapters::daemon::CapabilityReadiness::Stopping
                        )
                    }) || self.resources_released
                }
                StartupMilestone::ResourcesReleased => {
                    self.released_resources().await
                        == (ReleasedResources {
                            sled: true,
                            watcher: true,
                            socket: true,
                            sidecars: true,
                            ownership: true,
                        })
                }
            };
            if reached {
                self.observe_owned_processes();
                return;
            }
            if Instant::now() >= deadline {
                let status = self.try_status().await;
                let resources = self.released_resources().await;
                panic!(
                    "startup milestone {milestone:?} was not observed before its deadline; status={status:?}; resources={resources:?}"
                );
            }
            tokio::task::yield_now().await;
        }
    }

    pub(crate) async fn observe_probe(&mut self) -> CatchUpProbe {
        self.try_probe(false)
            .await
            .expect("public catch-up probe remains callable")
    }

    pub(crate) async fn release_probe(&mut self) -> CatchUpProbe {
        self.catch_up_release_elapsed_ms = Some(
            self.generation_started
                .expect("active generation start milestone")
                .elapsed()
                .as_millis() as u64,
        );
        self.try_probe(true)
            .await
            .expect("public catch-up release remains callable")
    }

    pub(crate) async fn status(&mut self) -> StatusSnapshot {
        self.try_status()
            .await
            .expect("daemon status remains callable")
    }

    pub(crate) async fn generation(&mut self) -> u64 {
        if let Some(status) = self.try_status().await {
            self.last_generation = status.generation;
        }
        self.last_generation
    }

    /// Issue a real MCP tool call and return its complete JSON-RPC envelope.
    pub(crate) async fn call_tool(
        &mut self,
        id: u64,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        self.send_mcp(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
        .await
    }

    /// Commit through the selected real producer while catch-up is blocked.
    /// The harness assigns/returns the fixture ordinal from the sidecar probe.
    pub(crate) async fn commit_change(
        &mut self,
        source: CommittedChangeSource,
        kind: CommittedChangeKind,
        path: &str,
        content: Option<&str>,
    ) -> u64 {
        match source {
            CommittedChangeSource::Watcher => match kind {
                CommittedChangeKind::Create | CommittedChangeKind::Change => fs::write(
                    self.workspace.path().join(path),
                    content.expect("create/change requires content"),
                )
                .expect("commit watcher write"),
                CommittedChangeKind::Delete => {
                    fs::remove_file(self.workspace.path().join(path))
                        .expect("commit watcher deletion");
                }
            },
            CommittedChangeSource::Mcp => {
                let id = 10_000 + self.committed_events;
                let (name, arguments) = match kind {
                    CommittedChangeKind::Create => (
                        "tower_create_file",
                        json!({"path": path, "content": content.expect("create requires content")}),
                    ),
                    CommittedChangeKind::Change => {
                        let current = fs::read_to_string(self.workspace.path().join(path))
                            .expect("read file before MCP change");
                        (
                            "tower_edit_range",
                            json!({"path": path, "start_byte": 0, "end_byte": current.len(), "replacement": content.expect("change requires content")}),
                        )
                    }
                    CommittedChangeKind::Delete => ("tower_delete_file", json!({"path": path})),
                };
                let response = self.call_tool(id, name, arguments).await;
                assert!(
                    response.get("result").is_some(),
                    "MCP producer failed to commit change: {response}"
                );
            }
        }
        self.committed_events += 1;
        self.committed_events
    }

    pub(crate) async fn hold_released_storage(&self, deadline: Instant) -> sled::Db {
        loop {
            if let Ok(storage) = sled::open(self.workspace.path().join(".tower/db")) {
                return storage;
            }
            assert!(
                Instant::now() < deadline,
                "released storage could not be acquired"
            );
            tokio::task::yield_now().await;
        }
    }

    pub(crate) fn stopped_workers_remain_absent(&mut self) -> bool {
        self.child
            .as_mut()
            .is_none_or(|child| child.try_wait().expect("poll stopped daemon").is_some())
            && !socket_path(self.workspace.path()).exists()
            && matches!(probe_owner(self.workspace.path()), Ok(OwnerLiveness::Dead))
            && self.watcher_thread.is_some_and(|(pid, tid)| {
                !PathBuf::from(format!("/proc/{pid}/task/{tid}")).exists()
            })
            && !self.sidecar_pids.is_empty()
            && self
                .sidecar_pids
                .iter()
                .all(|pid| !PathBuf::from(format!("/proc/{pid}")).exists())
    }

    pub(crate) async fn released_resources(&mut self) -> ReleasedResources {
        let child_exited = self
            .child
            .as_mut()
            .is_none_or(|child| child.try_wait().expect("poll daemon child").is_some());
        let socket = !socket_path(self.workspace.path()).exists();
        let ownership = matches!(probe_owner(self.workspace.path()), Ok(OwnerLiveness::Dead));
        let sled = if child_exited {
            sled::open(self.workspace.path().join(".tower/db")).is_ok()
        } else {
            false
        };
        let watcher = self.watcher_thread.is_some()
            && self.watcher_thread.iter().all(|(process_id, thread_id)| {
                !PathBuf::from(format!("/proc/{process_id}/task/{thread_id}")).exists()
            });
        let sidecars = !self.sidecar_pids.is_empty()
            && self
                .sidecar_pids
                .iter()
                .all(|process_id| !PathBuf::from(format!("/proc/{process_id}")).exists());
        ReleasedResources {
            sled,
            watcher,
            socket,
            sidecars,
            ownership,
        }
    }

    /// Return bytes captured from the daemon and all selected sidecar stderr
    /// streams. The implementation stores these under excluded `.tower/` state.
    pub(crate) async fn captured_logs(&mut self) -> CapturedProcessLogs {
        if self.logs_path.exists() {
            self.logs.daemon =
                fs::read_to_string(&self.logs_path).expect("read captured daemon log");
            // SidecarHostAdapter configures child stderr with Stdio::inherit(),
            // so the daemon's redirected stderr is the authoritative aggregate
            // for the daemon and every sidecar it owns.
            self.logs.sidecars.clone_from(&self.logs.daemon);
        }
        self.logs.clone()
    }

    pub(crate) async fn shutdown_and_await_release(&mut self, deadline: Instant) {
        self.observe_owned_processes();
        assert!(
            self.watcher_thread.is_some(),
            "shutdown must first observe the generation's named watcher thread"
        );
        assert!(
            !self.sidecar_pids.is_empty(),
            "shutdown must first observe the generation's sidecar process"
        );
        self.mcp.take();
        let opts = self.opts();
        let _ = send_control_async(&opts, ControlRequest::Shutdown).await;
        loop {
            let exited = self
                .child
                .as_mut()
                .is_none_or(|child| child.try_wait().expect("poll daemon shutdown").is_some());
            if exited {
                break;
            }
            if Instant::now() >= deadline {
                let child = self.child.as_mut().expect("active daemon child");
                let _ = child.kill();
                let _ = child.wait();
                panic!("daemon exceeded its bounded shutdown deadline");
            }
            tokio::task::yield_now().await;
        }
        self.resources_released = true;
        let _ = self.captured_logs().await;
    }

    pub(crate) async fn restart(&mut self) {
        assert!(self.resources_released, "restart requires joined cleanup");
        self.child.take();
        self.configuration = SlowStartInitializeConfiguration::Resume;
        self.start_generation().await;
    }

    fn opts(&self) -> GlobalOpts {
        GlobalOpts {
            workspace_dir: Some(self.workspace.path().to_path_buf()),
            extensions_dir: None,
        }
    }

    async fn try_status(&self) -> Option<StatusSnapshot> {
        match send_control_async(&self.opts(), ControlRequest::Status)
            .await
            .ok()?
        {
            ControlResponse::Status(status) => Some(status),
            _ => None,
        }
    }

    async fn send_mcp(&mut self, request: Value) -> Value {
        let mcp = self.mcp.as_mut().expect("active MCP connection");
        write_line(mcp, &request.to_string())
            .await
            .expect("send MCP request");
        let response = read_line_capped(mcp, 1 << 20)
            .await
            .expect("read MCP response")
            .expect("daemon returned MCP response");
        serde_json::from_str(&response).expect("decode MCP response")
    }

    async fn try_probe(&mut self, release: bool) -> Option<CatchUpProbe> {
        self.mcp.as_ref()?;
        let response = self
            .send_mcp(json!({
                "jsonrpc": "2.0",
                "id": 9_000 + u64::from(release),
                "method": "tools/call",
                "params": {
                    "name": "tower_slow_start_probe_catch_up",
                    "arguments": {"release": release}
                }
            }))
            .await;
        let text = response.pointer("/result/content/0/text")?.as_str()?;
        let value: Value = serde_json::from_str(text).ok()?;
        let probe = CatchUpProbe {
            released: value.get("released")?.as_bool()?,
            pending_fences: value.get("pending_fences")?.as_u64()?,
            processed_events: value.get("processed_events")?.as_u64()?,
            processed_ordinals: value
                .get("processed_ordinals")?
                .as_array()?
                .iter()
                .map(Value::as_u64)
                .collect::<Option<Vec<_>>>()?,
            barrier_observed: value.get("barrier_observed")?.as_bool()?,
        };
        Some(probe)
    }
}

impl SlowStartInitializeConfiguration {
    fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Resume => "resume",
            Self::Fail => "fail",
            Self::Crash => "crash",
        }
    }
}

impl Drop for ReconciliationProcessHarness {
    fn drop(&mut self) {
        self.mcp.take();
        if let Some(child) = self.child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PersistedState(BTreeMap<String, PersistedEntries>);

fn run_bounded_daemon(binary: &Path, workspace: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(binary);
    command
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run selected tower daemon")
}

fn write_idle_config(workspace: &Path, secret: &str) {
    std::fs::create_dir_all(workspace.join(".tower")).unwrap();
    std::fs::write(
        workspace.join(".tower/config.toml"),
        format!("# {secret}\n[daemon]\nidle_timeout_secs = 1\n"),
    )
    .unwrap();
}

fn install_launch_sentinel(workspace: &Path, sentinel: &Path) {
    let extension = workspace.join("extensions/sentinel");
    std::fs::create_dir_all(&extension).unwrap();
    let script = extension.join("sentinel.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf launched >> '{}'\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        extension.join("extension.toml"),
        "name = \"sentinel\"\nversion = \"0.1.0\"\ncommand = [\"sentinel.sh\"]\nactivation = \"eager\"\n[capabilities]\nrequired = []\n",
    )
    .unwrap();
}

fn assert_process_output_is_confidential(output: &Output, secret: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty(),
        "daemon startup wrote non-protocol stdout: {stdout}"
    );
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
}

fn assert_reconciliation_metrics_absent(output: &Output) {
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("tower: reconciliation-metrics "),
        "reconciliation telemetry must be disabled: {output:?}"
    );
}

/// Read the `paths` projection itself rather than rebuilding an in-memory
/// workspace. This makes the process fixture detect a partially-published
/// reconciliation transaction.
fn persisted_path_inventory(workspace: &Path) -> BTreeMap<String, (u32, u32)> {
    let db = sled::open(workspace.join(".tower/db")).expect("open persisted Sled database");
    let paths = db.open_tree("paths").expect("open persisted paths tree");
    paths
        .iter()
        .map(|entry| {
            let (path, encoded_id) = entry.expect("read persisted path entry");
            let encoded_id: [u8; 8] = encoded_id
                .as_ref()
                .try_into()
                .expect("persisted FileId must have eight bytes");
            let index = u32::from_be_bytes(encoded_id[..4].try_into().unwrap());
            let generation = u32::from_be_bytes(encoded_id[4..].try_into().unwrap());
            (
                String::from_utf8(path.to_vec()).expect("persisted path must be UTF-8"),
                (index, generation),
            )
        })
        .collect()
}

fn persisted_state(workspace: &Path) -> PersistedState {
    let db = sled::open(workspace.join(".tower/db")).expect("open persisted Sled database");
    PersistedState(
        ["files", "paths", "blobs", "meta", "index"]
            .into_iter()
            .map(|tree_name| {
                let tree = db.open_tree(tree_name).expect("open persisted tree");
                let entries = tree
                    .iter()
                    .map(|entry| {
                        let (key, value) = entry.expect("read persisted tree entry");
                        (key.to_vec(), value.to_vec())
                    })
                    .collect();
                (tree_name.to_owned(), entries)
            })
            .collect(),
    )
}

/// Reusable T008 process contract. T012 can call this helper with the same
/// explicitly provisioned binaries instead of duplicating lifecycle assertions.
pub(crate) fn assert_reconciliation_failure_contract_from_env() {
    assert_reconciliation_failure_contract(
        &selected_binary("TOWER_PRODUCTION_BIN"),
        &selected_binary("TOWER_TEST_BIN"),
    );
}

struct ReconciliationProcessFixture<'a> {
    production: &'a Path,
    test_hooks: &'a Path,
    workspace: tempfile::TempDir,
    unrelated: tempfile::TempDir,
    extensions: tempfile::TempDir,
    selector: PathBuf,
    extensions_dir: PathBuf,
    sentinel: PathBuf,
}

impl<'a> ReconciliationProcessFixture<'a> {
    const SECRET: &'static str = "F009_RECONCILIATION_SECRET_7cbbe8";
    const SOURCE_CONTENT_SENTINEL: &'static str = "F009_SOURCE_CONTENT_SENTINEL_repaired_body";

    fn new(production: &'a Path, test_hooks: &'a Path) -> Self {
        let workspace = tempdir().unwrap();
        let unrelated = tempdir().unwrap();
        let extensions = tempdir().unwrap();
        write_idle_config(workspace.path(), Self::SECRET);
        write_idle_config(unrelated.path(), Self::SECRET);
        std::fs::write(
            workspace.path().join("baseline.rs"),
            "pub fn baseline() {}\n",
        )
        .unwrap();
        let selector = workspace.path().canonicalize().unwrap();
        let extensions_dir = extensions.path().join("extensions");
        let sentinel = extensions.path().join("sidecar-launched");
        Self {
            production,
            test_hooks,
            workspace,
            unrelated,
            extensions,
            selector,
            extensions_dir,
            sentinel,
        }
    }

    fn hook_environment(&self, fail_before_commit: bool, metrics: bool) -> Vec<(&str, &str)> {
        let mut env = vec![
            ("TOWER_TEST_WORKSPACE", self.selector.to_str().unwrap()),
            (
                "TOWER_EXTENSIONS_DIR",
                self.extensions_dir.to_str().unwrap(),
            ),
        ];
        if fail_before_commit {
            env.push(("TOWER_TEST_FAIL_RECONCILIATION", "before_commit"));
        }
        if metrics {
            env.push(("TOWER_TEST_METRICS", "1"));
        }
        env
    }

    fn establish_baseline(&self) -> PersistedState {
        let baseline = run_bounded_daemon(self.production, self.workspace.path(), &[]);
        assert!(baseline.status.success(), "baseline failed: {baseline:?}");
        let prior_good_inventory = BTreeMap::from([("baseline.rs".to_owned(), (0, 0))]);
        assert_eq!(
            persisted_path_inventory(self.workspace.path()),
            prior_good_inventory,
            "baseline must establish the exact persisted inventory used for rollback"
        );
        persisted_state(self.workspace.path())
    }

    fn assert_invalid_selector_is_inert(&self) {
        let secret = Self::SECRET;
        let invalid_selector = self.workspace.path().join("selector-does-not-exist");
        let invalid_selector = invalid_selector.to_str().unwrap();
        let invalid_selector_env = [
            ("TOWER_TEST_WORKSPACE", invalid_selector),
            ("TOWER_TEST_FAIL_RECONCILIATION", "before_commit"),
            ("TOWER_TEST_METRICS", "1"),
        ];
        let invalid_selector_output = run_bounded_daemon(
            self.test_hooks,
            self.workspace.path(),
            &invalid_selector_env,
        );
        assert!(invalid_selector_output.status.success());
        assert_process_output_is_confidential(&invalid_selector_output, secret);
        assert_reconciliation_metrics_absent(&invalid_selector_output);
    }

    fn prepare_missing_file_and_sidecar(&self) {
        let secret = Self::SECRET;
        let source_content_sentinel = Self::SOURCE_CONTENT_SENTINEL;
        std::fs::write(
            self.workspace.path().join("repaired.rs"),
            format!(
                "pub fn repaired() {{ let _ = (\"{secret}\", \"{source_content_sentinel}\"); }}\n"
            ),
        )
        .unwrap();
        install_launch_sentinel(self.extensions.path(), &self.sentinel);
    }

    fn assert_failed_repair_preserves(&self, prior_good_state: &PersistedState) {
        let secret = Self::SECRET;
        let source_content_sentinel = Self::SOURCE_CONTENT_SENTINEL;
        let trigger = self.hook_environment(true, true);
        let failed = run_bounded_daemon(self.test_hooks, self.workspace.path(), &trigger);
        assert!(!failed.status.success());
        assert_process_output_is_confidential(&failed, secret);
        assert_process_output_is_confidential(&failed, source_content_sentinel);
        let failure_stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(failure_stderr.contains("apply_reconciliation_delta"));
        assert!(failure_stderr.contains(&self.workspace.path().display().to_string()));
        assert!(!self.sentinel.exists());
        assert_eq!(
            persisted_state(self.workspace.path()),
            *prior_good_state,
            "injected reconciliation failure must leave every persisted projection intact"
        );
    }

    fn assert_unrelated_workspace_is_unaffected(&self) {
        let secret = Self::SECRET;
        let trigger = self.hook_environment(true, true);
        let unrelated_output = run_bounded_daemon(self.test_hooks, self.unrelated.path(), &trigger);
        assert!(unrelated_output.status.success());
        assert_process_output_is_confidential(&unrelated_output, secret);
        assert_reconciliation_metrics_absent(&unrelated_output);
    }

    fn assert_first_clean_retry_repairs(&self, prior_good_state: &PersistedState) {
        let secret = Self::SECRET;
        let source_content_sentinel = Self::SOURCE_CONTENT_SENTINEL;
        let retry_env = self.hook_environment(false, true);
        let retry = run_bounded_daemon(self.test_hooks, self.workspace.path(), &retry_env);
        assert!(
            retry.status.success(),
            "first clean retry failed: {retry:?}"
        );
        assert_process_output_is_confidential(&retry, secret);
        assert_process_output_is_confidential(&retry, source_content_sentinel);
        let retry_stderr = String::from_utf8_lossy(&retry.stderr);
        assert!(retry_stderr.contains("tower: reconciled index — "));
        assert!(!retry_stderr.contains("tower: reconciled index — 0 added"));
        assert_eq!(
            retry_stderr
                .matches("tower: reconciliation-metrics ")
                .count(),
            1
        );
        assert_eq!(
            persisted_path_inventory(self.workspace.path()),
            BTreeMap::from([
                ("baseline.rs".to_owned(), (0, 0)),
                ("repaired.rs".to_owned(), (1, 0)),
            ]),
            "first clean retry must publish the exact repaired persisted inventory"
        );
        let repaired_state = persisted_state(self.workspace.path());
        assert_repaired_projections(&repaired_state, prior_good_state);
    }

    fn assert_unchanged_restart_has_zero_metrics(&self) {
        let secret = Self::SECRET;
        let retry_env = self.hook_environment(false, true);
        let unchanged = run_bounded_daemon(self.test_hooks, self.workspace.path(), &retry_env);
        assert!(unchanged.status.success());
        assert_process_output_is_confidential(&unchanged, secret);
        assert_eq!(
            String::from_utf8_lossy(&unchanged.stderr)
                .matches("tower: reconciliation-metrics {\"content_reads\":0,\"content_hashes\":0,\"file_record_rewrites\":0}")
                .count(),
            1
        );
    }

    fn assert_metrics_require_opt_in(&self) {
        let secret = Self::SECRET;
        let metrics_disabled_env = self.hook_environment(false, false);
        let metrics_disabled = run_bounded_daemon(
            self.test_hooks,
            self.workspace.path(),
            &metrics_disabled_env,
        );
        assert!(metrics_disabled.status.success());
        assert_process_output_is_confidential(&metrics_disabled, secret);
        assert_reconciliation_metrics_absent(&metrics_disabled);
    }

    fn assert_production_ignores_hooks(&self) {
        let secret = Self::SECRET;
        let trigger = self.hook_environment(true, true);
        let production_output =
            run_bounded_daemon(self.production, self.workspace.path(), &trigger);
        assert!(production_output.status.success());
        assert_process_output_is_confidential(&production_output, secret);
        assert_reconciliation_metrics_absent(&production_output);
    }
}

fn assert_repaired_projections(repaired_state: &PersistedState, prior_good_state: &PersistedState) {
    for projection in ["files", "paths", "meta", "index"] {
        assert_ne!(
            repaired_state.0[projection], prior_good_state.0[projection],
            "clean retry must update the {projection} projection"
        );
    }
    assert_eq!(
        repaired_state.0["blobs"], prior_good_state.0["blobs"],
        "path reconciliation must leave the content-addressed blob projection unchanged"
    );
    assert_eq!(repaired_state.0["files"].len(), 2);
    assert_eq!(repaired_state.0["paths"].len(), 2);
    let search_terms = repaired_state.0["index"]
        .iter()
        .map(|(key, _)| String::from_utf8(key.clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        search_terms,
        [
            "bas", "base", "basel", "baseli", "baselin", "baseline", "rep", "repa", "repai",
            "repair", "repaire", "repaired", "rs",
        ]
    );
    let metadata_keys = repaired_state.0["meta"]
        .iter()
        .map(|(key, _)| String::from_utf8(key.clone()).unwrap())
        .collect::<Vec<_>>();
    assert!(metadata_keys.iter().any(|key| key == "slot_generations"));
    assert!(metadata_keys.iter().any(|key| key == "free_slots"));
}

pub(crate) fn assert_reconciliation_failure_contract(production: &Path, test_hooks: &Path) {
    let fixture = ReconciliationProcessFixture::new(production, test_hooks);
    let prior_good_state = fixture.establish_baseline();
    fixture.assert_invalid_selector_is_inert();
    fixture.prepare_missing_file_and_sidecar();
    fixture.assert_failed_repair_preserves(&prior_good_state);
    fixture.assert_unrelated_workspace_is_unaffected();
    fixture.assert_first_clean_retry_repairs(&prior_good_state);
    fixture.assert_unchanged_restart_has_zero_metrics();
    fixture.assert_metrics_require_opt_in();
    fixture.assert_production_ignores_hooks();
}
