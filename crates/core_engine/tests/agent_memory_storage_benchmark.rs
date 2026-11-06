//! Ignored, operator-run performance evidence for durable agent-memory operations.
//!
//! This stays as one integration-test target so Cargo discovers its benchmark
//! exactly once. Validation helpers below are intentionally deterministic so the
//! real-disk benchmark remains the only environment-dependent test.
#![forbid(unsafe_code)]

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use agent_memory_extension::{
    AgentMemoryLimitsConfig, Clock, EnrichmentProvider, EnrichmentProviderError, EnrichmentRequest,
    EnrichmentResult, GetQuery, IdGenerator, MemoryApplication, MemoryFilters, MemoryId,
    MemoryStore, MemoryType, Page, PersistenceMode, PrivacyFilter, Provenance, RankingMode,
    RecordInput, RememberCommand, SearchQuery, TimelineAnchor, TimelineQuery,
};
use core_engine::{
    adapters::SledAgentMemoryStorage,
    ports::{
        AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryNamespace, AgentMemoryScanRequest,
        AgentMemoryScanResult, AgentMemoryStorageBounds, AgentMemoryStorageError,
        AgentMemoryStoragePort, AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
    },
};
use sha2::{Digest as _, Sha256};

const RECORD_COUNT: usize = 10_000;
const SAMPLE_COUNT: usize = 50;
const WARM_UP_COUNT: usize = 5;
const P95_TARGET: Duration = Duration::from_millis(500);

struct OwnedStore {
    storage: Arc<SledAgentMemoryStorage>,
    namespace: AgentMemoryNamespace,
}
impl MemoryStore for OwnedStore {
    fn get(
        &self,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, extension_protocol::messages::AgentMemoryStorageFailure> {
        self.storage
            .get(&self.namespace, request)
            .map_err(map_storage_error)
    }
    fn scan(
        &self,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, extension_protocol::messages::AgentMemoryStorageFailure>
    {
        self.storage
            .scan(&self.namespace, request)
            .map_err(map_storage_error)
    }
    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<
        AgentMemoryTransactionOutcome,
        extension_protocol::messages::AgentMemoryStorageFailure,
    > {
        self.storage
            .transact(&self.namespace, request)
            .map_err(map_storage_error)
    }
}
fn map_storage_error(
    error: AgentMemoryStorageError,
) -> extension_protocol::messages::AgentMemoryStorageFailure {
    match error {
        AgentMemoryStorageError::BoundsExceeded => {
            extension_protocol::messages::AgentMemoryStorageFailure::BoundsExceeded
        }
        AgentMemoryStorageError::Unavailable => {
            extension_protocol::messages::AgentMemoryStorageFailure::Unavailable
        }
        AgentMemoryStorageError::CommitOutcomeUnknown => {
            extension_protocol::messages::AgentMemoryStorageFailure::CommitOutcomeUnknown
        }
    }
}

#[derive(Clone, Copy)]
struct BenchClock;
impl Clock for BenchClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn monotonic_now(&self) -> Duration {
        Duration::ZERO
    }
}
#[derive(Clone, Default)]
struct SequenceIds(Arc<AtomicU64>);
impl IdGenerator for SequenceIds {
    fn next_id(&self) -> MemoryId {
        MemoryId::try_from(format!(
            "benchmark-{:08}",
            self.0.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap()
    }
}
struct NoProvider;
impl EnrichmentProvider for NoProvider {
    fn enrich(
        &self,
        _: EnrichmentRequest,
        _: Duration,
        _: &AtomicBool,
    ) -> Result<EnrichmentResult, EnrichmentProviderError> {
        Err(EnrichmentProviderError::Unavailable)
    }
}

#[test]
#[ignore = "local real-disk performance evidence; never a default or CI latency gate"]
fn durable_memory_operations_meet_latency_target_on_real_disk() {
    let checkout = std::fs::canonicalize(
        std::env::var_os("PWD").expect("PWD must identify the checkout under test"),
    )
    .expect("PWD must resolve");
    let bench_root = checkout.join(".tower/bench-tmp");
    std::fs::create_dir_all(&bench_root).expect("create explicit benchmark root");
    let bench_root = std::fs::canonicalize(bench_root).expect("benchmark root must resolve");
    let (mount, filesystem) =
        filesystem_provenance(&bench_root).expect("resolve benchmark filesystem mount");
    assert_supported_filesystem(&filesystem)
        .expect("benchmark filesystem must be local persistent disk");

    let executable =
        std::fs::canonicalize(std::env::current_exe().expect("current benchmark executable"))
            .expect("resolve benchmark executable");
    let binary = verify_current_release_binary(&checkout, &bench_root, &executable)
        .expect("verify current release benchmark provenance");
    let commit = git(&checkout, &["rev-parse", "HEAD"]);
    let dirty = if git(&checkout, &["status", "--porcelain"]).is_empty() {
        "clean"
    } else {
        "dirty"
    };
    println!(
        "executable={} sha256={}",
        executable.display(),
        binary.running_sha256
    );
    println!(
        "rebuilt_executable={} sha256={}",
        binary.rebuilt_path.display(),
        binary.rebuilt_sha256
    );
    println!("git_commit={commit} dirty_state={dirty}");
    println!(
        "os={} filesystem={} mount={}",
        std::env::consts::OS,
        filesystem,
        mount
    );
    println!(
        "build_profile=release record_count={RECORD_COUNT} sample_count={SAMPLE_COUNT} warm_up={WARM_UP_COUNT}"
    );

    let run_root = bench_root.join(format!("run-{}", std::process::id()));
    let bounds =
        AgentMemoryStorageBounds::try_new(4096, 1 << 20, 1024, 4096, 4096, 1 << 30, 32).unwrap();
    let storage = Arc::new(SledAgentMemoryStorage::open(&run_root, bounds).unwrap());
    let namespace =
        AgentMemoryNamespace::from_trusted_identity(b"benchmark-project", b"agent_memory").unwrap();
    let limits = limits();
    let application = MemoryApplication::<_, _, _, NoProvider>::new_configured(
        OwnedStore { storage, namespace },
        BenchClock,
        SequenceIds::default(),
        None,
        PrivacyFilter::default(),
        limits.clone(),
        PersistenceMode::Durable,
        false,
        Duration::from_secs(5),
    );
    application.initialize_store().unwrap();

    let mut seeded_ids = Vec::with_capacity(RECORD_COUNT);
    for index in 0..RECORD_COUNT {
        seeded_ids.push(
            application
                .remember(remember_command(index, &limits))
                .unwrap()
                .id,
        );
    }
    let page = Page::try_new(Some(20), None, None, &limits).unwrap();
    let filters = MemoryFilters::try_new(
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits,
    )
    .unwrap();
    let next_index = AtomicU64::new(RECORD_COUNT as u64);
    let remember = measure(|| {
        let index = next_index.fetch_add(1, Ordering::Relaxed) as usize;
        application
            .remember(remember_command(index, &limits))
            .unwrap();
    });
    let get = measure(|| {
        application
            .get(GetQuery::try_new(vec![seeded_ids[RECORD_COUNT / 2].clone()], &limits).unwrap())
            .unwrap();
    });
    let search = measure(|| {
        application
            .search(
                SearchQuery::try_new(
                    "benchmark needle".into(),
                    filters.clone(),
                    page.clone(),
                    RankingMode::LexicalV1,
                    &limits,
                )
                .unwrap(),
            )
            .unwrap();
    });
    let timeline = measure(|| {
        application
            .timeline(
                TimelineQuery::try_new(
                    TimelineAnchor::Record {
                        id: seeded_ids[RECORD_COUNT / 2].clone(),
                        before: 5,
                        after: 5,
                    },
                    filters.clone(),
                    page.clone(),
                    &limits,
                )
                .unwrap(),
            )
            .unwrap();
    });
    report("remember", remember);
    report("get", get);
    report("lexical_search", search);
    report("timeline", timeline);
}

fn measure(mut operation: impl FnMut()) -> Vec<Duration> {
    for _ in 0..WARM_UP_COUNT {
        operation();
    }
    (0..SAMPLE_COUNT)
        .map(|_| {
            let start = Instant::now();
            operation();
            start.elapsed()
        })
        .collect()
}
fn report(operation: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let p50 = samples[(samples.len() * 50).div_ceil(100) - 1];
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    println!(
        "operation={operation} p50_ms={:.3} p95_ms={:.3}",
        p50.as_secs_f64() * 1000.0,
        p95.as_secs_f64() * 1000.0
    );
    assert!(
        p95 < P95_TARGET,
        "{operation} p95 {p95:?} exceeded {P95_TARGET:?}"
    );
}
fn remember_command(index: usize, limits: &AgentMemoryLimitsConfig) -> RememberCommand {
    let provenance = Provenance::try_new(
        Some("main".into()),
        vec!["src/lib.rs".into()],
        vec![],
        vec!["benchmark".into()],
        Some("benchmark".into()),
        Some("tower".into()),
        None,
        None,
        None,
        limits,
    )
    .unwrap();
    let record = RecordInput::try_new(
        MemoryType::Observation,
        format!("benchmark needle {index}"),
        format!("durable benchmark record {index}"),
        provenance,
        Some(0.8),
        limits,
    )
    .unwrap();
    RememberCommand::try_new(record, format!("benchmark-idempotency-{index}"), limits).unwrap()
}
fn limits() -> AgentMemoryLimitsConfig {
    AgentMemoryLimitsConfig {
        max_title_bytes: 1024,
        max_body_bytes: 65_536,
        max_metadata_items: 100,
        max_metadata_string_bytes: 1024,
        max_batch_items: 100,
        default_results: 20,
        max_results: 100,
        default_response_bytes: 32_768,
        max_response_bytes: 131_072,
        max_query_bytes: 4096,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum FilesystemProvenanceError {
    Unresolved,
    Ambiguous(String),
    Rejected(String),
}
fn filesystem_provenance(path: &Path) -> Result<(String, String), FilesystemProvenanceError> {
    let output = Command::new("findmnt")
        .args(["--noheadings", "--output", "TARGET,FSTYPE", "--target"])
        .arg(path)
        .output()
        .map_err(|_| FilesystemProvenanceError::Unresolved)?;
    if !output.status.success() {
        return Err(FilesystemProvenanceError::Unresolved);
    }
    parse_filesystem_provenance(&String::from_utf8_lossy(&output.stdout))
}
fn parse_filesystem_provenance(
    output: &str,
) -> Result<(String, String), FilesystemProvenanceError> {
    let mut fields = output.split_whitespace();
    let mount = fields.next().ok_or(FilesystemProvenanceError::Unresolved)?;
    let filesystem = fields.next().ok_or(FilesystemProvenanceError::Unresolved)?;
    if fields.next().is_some() {
        return Err(FilesystemProvenanceError::Ambiguous(output.to_owned()));
    }
    Ok((mount.to_owned(), filesystem.to_owned()))
}
fn assert_supported_filesystem(filesystem: &str) -> Result<(), FilesystemProvenanceError> {
    // Evidence is valid only for known local, persistent filesystems. An allowlist
    // prevents newly observed network, virtual, or RAM-backed types being trusted by default.
    const LOCAL_PERSISTENT: &[&str] = &[
        "apfs", "btrfs", "ext2", "ext3", "ext4", "f2fs", "hfs", "hfsplus", "ntfs", "reiserfs",
        "ufs", "xfs", "zfs",
    ];
    if LOCAL_PERSISTENT.contains(&filesystem.to_ascii_lowercase().as_str()) {
        Ok(())
    } else {
        Err(FilesystemProvenanceError::Rejected(filesystem.to_owned()))
    }
}

#[derive(Debug, Eq, PartialEq)]
enum BinaryProvenanceError {
    NotReleaseBenchmark(PathBuf),
    RebuildFailed,
    RebuiltExecutableMissing,
    DigestMismatch { running: String, rebuilt: String },
}
struct BinaryProvenance {
    running_sha256: String,
    rebuilt_sha256: String,
    rebuilt_path: PathBuf,
}
fn validate_binary_provenance(
    checkout: &Path,
    executable: &Path,
    running_sha256: String,
    rebuilt_sha256: String,
) -> Result<(), BinaryProvenanceError> {
    let release_dir = checkout.join("target/release");
    if !executable.starts_with(&release_dir)
        || !executable.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .contains("agent_memory_storage_benchmark")
        })
    {
        return Err(BinaryProvenanceError::NotReleaseBenchmark(
            executable.to_owned(),
        ));
    }
    if running_sha256 != rebuilt_sha256 {
        return Err(BinaryProvenanceError::DigestMismatch {
            running: running_sha256,
            rebuilt: rebuilt_sha256,
        });
    }
    Ok(())
}
fn verify_current_release_binary(
    checkout: &Path,
    _bench_root: &Path,
    executable: &Path,
) -> Result<BinaryProvenance, BinaryProvenanceError> {
    // Capture the executing image before Cargo can unlink and replace its
    // pathname. Hashing `executable` after the rebuild would hash the newly
    // written file twice and allow a stale process image to pass provenance.
    let running_sha256 = sha256(executable).map_err(|_| BinaryProvenanceError::RebuildFailed)?;

    // Rebuild in the checkout's actual release target. A separate target directory
    // changes Rust link metadata, so matching two binaries there would reject a
    // genuine current-checkout binary. Cleaning only this package's release
    // artifacts makes Cargo re-link the exact integration-test target we inspect.
    let clean = Command::new(env!("CARGO"))
        .current_dir(checkout)
        .args(["clean", "-p", "core_engine", "--release"])
        .status()
        .map_err(|_| BinaryProvenanceError::RebuildFailed)?;
    if !clean.success() {
        return Err(BinaryProvenanceError::RebuildFailed);
    }
    let status = Command::new(env!("CARGO"))
        .current_dir(checkout)
        .args([
            "test",
            "-p",
            "core_engine",
            "--test",
            "agent_memory_storage_benchmark",
            "--release",
            "--no-run",
        ])
        .status()
        .map_err(|_| BinaryProvenanceError::RebuildFailed)?;
    if !status.success() {
        return Err(BinaryProvenanceError::RebuildFailed);
    }
    let rebuilt_path = std::fs::read_dir(checkout.join("target/release/deps"))
        .map_err(|_| BinaryProvenanceError::RebuiltExecutableMissing)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file()
                && path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with("agent_memory_storage_benchmark-")
                })
                && is_executable(path)
        })
        .ok_or(BinaryProvenanceError::RebuiltExecutableMissing)?;
    let rebuilt_sha256 = sha256(&rebuilt_path).map_err(|_| BinaryProvenanceError::RebuildFailed)?;
    validate_binary_provenance(
        checkout,
        executable,
        running_sha256.clone(),
        rebuilt_sha256.clone(),
    )?;
    Ok(BinaryProvenance {
        running_sha256,
        rebuilt_sha256,
        rebuilt_path,
    })
}
fn sha256(path: &Path) -> std::io::Result<String> {
    Ok(Sha256::digest(std::fs::read(path)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    true
}
fn git(checkout: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(checkout)
        .output()
        .expect("git is required");
    assert!(
        output.status.success(),
        "git provenance command failed: git {}",
        args.join(" ")
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn provenance_validation_accepts_local_disk_and_rejects_network_and_virtual_filesystems() {
    assert_eq!(assert_supported_filesystem("ext4"), Ok(()));
    for filesystem in [
        "tmpfs",
        "overlay",
        "nfs",
        "ceph",
        "glusterfs",
        "afs",
        "fuse.sshfs",
    ] {
        assert_eq!(
            assert_supported_filesystem(filesystem),
            Err(FilesystemProvenanceError::Rejected(filesystem.into()))
        );
    }
}
#[test]
fn filesystem_provenance_validation_rejects_unresolved_and_ambiguous_findmnt_output() {
    assert_eq!(
        parse_filesystem_provenance(""),
        Err(FilesystemProvenanceError::Unresolved)
    );
    assert_eq!(
        parse_filesystem_provenance("/disk ext4 extra"),
        Err(FilesystemProvenanceError::Ambiguous(
            "/disk ext4 extra".into()
        ))
    );
}
#[test]
fn executable_provenance_validation_rejects_non_release_or_digest_mismatch() {
    let checkout = Path::new("/checkout");
    assert_eq!(
        validate_binary_provenance(
            checkout,
            Path::new("/copied/agent_memory_storage_benchmark"),
            "a".repeat(64),
            "a".repeat(64)
        ),
        Err(BinaryProvenanceError::NotReleaseBenchmark(PathBuf::from(
            "/copied/agent_memory_storage_benchmark"
        )))
    );
    assert_eq!(
        validate_binary_provenance(
            checkout,
            Path::new("/checkout/target/release/deps/agent_memory_storage_benchmark-current"),
            "a".repeat(64),
            "b".repeat(64),
        ),
        Err(BinaryProvenanceError::DigestMismatch {
            running: "a".repeat(64),
            rebuilt: "b".repeat(64),
        })
    );
}
