use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::dependency_source::{
    ConfinedReadError, DependencyFileReader, FsDependencySource, RealDependencyFileReader,
};
use crate::adapters::config::ResolvedDependencyRoot;
use crate::domain::RelativePath;
use crate::domain::search::{
    DependencyIndexIssueReason, DependencyIndexLimits, DependencyLimitReason, DependencyPath,
    DependencyRootId,
};
use crate::ports::DependencySourcePort;
use crate::test_support::{DependencySourceFault, DependencySourceFixture};

fn root_id(value: &str) -> DependencyRootId {
    DependencyRootId::new(value.to_owned()).expect("valid root id")
}

fn dependency_path(root: &str, relative: &str) -> DependencyPath {
    DependencyPath::new(root_id(root), RelativePath::new(relative)).expect("valid dependency path")
}

fn resolved_root(id: &str, path: impl Into<PathBuf>) -> ResolvedDependencyRoot {
    ResolvedDependencyRoot {
        id: root_id(id),
        path: path.into(),
    }
}

fn limits(max_files: usize, max_file_bytes: u64, max_total_bytes: u64) -> DependencyIndexLimits {
    DependencyIndexLimits::new(max_files, max_file_bytes, max_total_bytes).expect("positive limits")
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn visible_paths(snapshot: &crate::domain::search::DependencyIndexSnapshot) -> Vec<DependencyPath> {
    snapshot
        .files()
        .iter()
        .map(|file| file.path().clone())
        .collect()
}

fn issue_reasons(
    snapshot: &crate::domain::search::DependencyIndexSnapshot,
) -> Vec<DependencyIndexIssueReason> {
    snapshot
        .report()
        .issues
        .iter()
        .map(|issue| issue.reason)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderFault {
    None,
    Unreadable,
    Rewrite,
    Remove,
    Partial,
    DisappearBeforeRead,
    BoundaryViolation,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ReaderStats {
    candidate_count: usize,
    metadata_calls: usize,
    read_requests: Vec<u64>,
    returned_bytes: Vec<usize>,
    attempted_bytes: u64,
    accepted_files: usize,
    retained_bytes: usize,
    targeted_canonicalizations: usize,
}

struct InstrumentedReader {
    fault_name: Option<String>,
    fault: ReaderFault,
    raw_error: Option<String>,
    stats: Arc<Mutex<ReaderStats>>,
}

#[cfg(unix)]
struct SwapBeforeConfinedOpenReader {
    outside: PathBuf,
    swapped: Mutex<bool>,
}

#[cfg(unix)]
impl DependencyFileReader for SwapBeforeConfinedOpenReader {
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    fn read_bounded(&self, path: &Path, max_bytes: NonZeroU64) -> io::Result<Vec<u8>> {
        RealDependencyFileReader.read_bounded(path, max_bytes)
    }

    fn read_confined(
        &self,
        root: &Path,
        relative: &Path,
        max_bytes: NonZeroU64,
    ) -> Result<Vec<u8>, ConfinedReadError> {
        let mut swapped = self.swapped.lock().unwrap();
        if !*swapped {
            let candidate = root.join(relative);
            fs::remove_file(&candidate).map_err(|_| ConfinedReadError::Unreadable)?;
            std::os::unix::fs::symlink(&self.outside, candidate)
                .map_err(|_| ConfinedReadError::Unreadable)?;
            *swapped = true;
        }
        drop(swapped);
        RealDependencyFileReader.read_confined(root, relative, max_bytes)
    }

    fn probe_confined(
        &self,
        root: &Path,
        relative: &Path,
        max_bytes: NonZeroU64,
    ) -> Result<(), ConfinedReadError> {
        let candidate = root.join(relative);
        fs::remove_file(&candidate).map_err(|_| ConfinedReadError::Unreadable)?;
        std::os::unix::fs::symlink(&self.outside, candidate)
            .map_err(|_| ConfinedReadError::Unreadable)?;
        RealDependencyFileReader.probe_confined(root, relative, max_bytes)
    }
}

impl InstrumentedReader {
    fn new(fault_name: Option<&str>, fault: ReaderFault) -> (Arc<Self>, Arc<Mutex<ReaderStats>>) {
        Self::with_raw_error(fault_name, fault, None)
    }

    fn with_raw_error(
        fault_name: Option<&str>,
        fault: ReaderFault,
        raw_error: Option<&str>,
    ) -> (Arc<Self>, Arc<Mutex<ReaderStats>>) {
        let stats = Arc::new(Mutex::new(ReaderStats::default()));
        (
            Arc::new(Self {
                fault_name: fault_name.map(str::to_owned),
                fault,
                raw_error: raw_error.map(str::to_owned),
                stats: Arc::clone(&stats),
            }),
            stats,
        )
    }

    fn targets(&self, path: &Path) -> bool {
        self.fault_name
            .as_deref()
            .is_some_and(|name| path.file_name().and_then(|value| value.to_str()) == Some(name))
    }
}

impl DependencyFileReader for InstrumentedReader {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if self.targets(path) {
            let mut stats = self.stats.lock().unwrap();
            stats.targeted_canonicalizations += 1;
            if self.fault == ReaderFault::DisappearBeforeRead
                && stats.targeted_canonicalizations == 2
            {
                drop(stats);
                fs::remove_file(path)?;
            }
        }
        if self.targets(path) && self.fault == ReaderFault::BoundaryViolation {
            return Ok(path
                .parent()
                .and_then(Path::parent)
                .expect("fixture path has a root parent")
                .join("outside-boundary-fixture"));
        }
        path.canonicalize()
    }

    fn candidate_seen(&self) {
        self.stats.lock().unwrap().candidate_count += 1;
    }

    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        self.stats.lock().unwrap().metadata_calls += 1;
        fs::metadata(path)
    }

    fn read_bounded(&self, path: &Path, max_bytes: NonZeroU64) -> io::Result<Vec<u8>> {
        let mut stats = self.stats.lock().unwrap();
        stats.read_requests.push(max_bytes.get());
        stats.attempted_bytes += max_bytes.get();
        drop(stats);
        if self.targets(path) && self.fault == ReaderFault::Unreadable {
            return Err(io::Error::other(
                self.raw_error.as_deref().unwrap_or("injected read denial"),
            ));
        }

        let mut bytes = fs::read(path)?;
        bytes.truncate(max_bytes.get() as usize);
        if self.targets(path) {
            match self.fault {
                ReaderFault::Rewrite => fs::write(path, b"replacement with a distinct size")?,
                ReaderFault::Remove => fs::remove_file(path)?,
                ReaderFault::Partial => bytes.truncate(bytes.len().saturating_sub(1)),
                ReaderFault::None
                | ReaderFault::Unreadable
                | ReaderFault::DisappearBeforeRead
                | ReaderFault::BoundaryViolation => {}
            }
        }
        self.stats.lock().unwrap().returned_bytes.push(bytes.len());
        Ok(bytes)
    }

    fn accepted(&self, bytes: usize) {
        let mut stats = self.stats.lock().unwrap();
        stats.accepted_files += 1;
        stats.retained_bytes += bytes;
    }
}

fn adapter_with_reader(
    roots: Vec<ResolvedDependencyRoot>,
    workspace: &Path,
    ignore: Vec<String>,
    fault_name: Option<&str>,
    fault: ReaderFault,
) -> (FsDependencySource, Arc<Mutex<ReaderStats>>) {
    let (reader, stats) = InstrumentedReader::new(fault_name, fault);
    let adapter = FsDependencySource::with_reader(roots, workspace.to_path_buf(), ignore, reader)
        .expect("valid dependency source fixture");
    (adapter, stats)
}

fn materialize_contract_fixture(fixture: DependencySourceFixture) -> Box<dyn DependencySourcePort> {
    let container = TempDir::new().unwrap().keep();
    let workspace = container.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let mut roots = Vec::new();
    for (index, id) in fixture.roots.iter().enumerate() {
        let is_primary = id == &root_id("primary");
        let path = container.join(format!("root-{index}"));
        if fixture.fault == DependencySourceFault::RootNotDirectory && is_primary {
            fs::write(&path, b"not a directory").unwrap();
        } else if fixture.fault != DependencySourceFault::RootUnavailable || !is_primary {
            fs::create_dir(&path).unwrap();
        }
        roots.push(ResolvedDependencyRoot {
            id: id.clone(),
            path,
        });
    }

    for (path, bytes) in &fixture.entries {
        let root = roots
            .iter()
            .find(|root| &root.id == path.root_id())
            .expect("fixture root");
        if fixture.fault == DependencySourceFault::RootUnavailable
            || fixture.fault == DependencySourceFault::RootNotDirectory
        {
            continue;
        }
        let bytes = if fixture.fault == DependencySourceFault::InvalidEncoding {
            &[0xff][..]
        } else {
            bytes.as_slice()
        };
        write_file(&root.path, path.relative_path().as_str(), bytes);
    }

    #[cfg(unix)]
    if fixture.fault == DependencySourceFault::BoundaryViolation {
        use std::os::unix::fs::symlink;

        let primary = roots
            .iter()
            .find(|root| root.id == root_id("primary"))
            .unwrap();
        let outside = container.join("outside");
        fs::create_dir(&outside).unwrap();
        write_file(&outside, "file.rs", b"outside");
        let src = primary.path.join("src");
        if src.exists() {
            fs::remove_dir_all(&src).unwrap();
        }
        symlink(&outside, src).unwrap();
    }

    let fault = match fixture.fault {
        DependencySourceFault::EntryUnreadable => ReaderFault::Unreadable,
        DependencySourceFault::ChangedDuringRead => ReaderFault::Rewrite,
        #[cfg(not(unix))]
        DependencySourceFault::BoundaryViolation => ReaderFault::BoundaryViolation,
        _ => ReaderFault::None,
    };
    let (adapter, _) = adapter_with_reader(roots, &workspace, Vec::new(), Some("file.rs"), fault);
    Box::new(adapter)
}

crate::dependency_source_contract_tests!(super::materialize_contract_fixture);

#[test]
fn dependency_ignore_rules_precede_limit_accounting() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let dependency = container.path().join("dependency");
    fs::create_dir_all(&workspace).unwrap();
    write_file(&dependency, "ignored/large.rs", b"0123456789");
    write_file(&dependency, "generated/keep.rs", b"keep");
    write_file(&dependency, "accepted.rs", b"ok");
    write_file(&dependency, ".hidden.rs", b"hidden");
    write_file(&dependency, ".towerignore", b"local-only.rs\n");
    write_file(&dependency, "local-only.rs", b"local");

    let adapter = FsDependencySource::new(
        vec![resolved_root("deps", &dependency)],
        workspace,
        vec![
            "ignored/".into(),
            "generated/**".into(),
            "!generated/keep.rs".into(),
            "!ignored/large.rs".into(),
        ],
    )
    .unwrap();
    let snapshot = adapter.build_snapshot(&limits(2, 4, 6)).unwrap();

    assert_eq!(
        visible_paths(&snapshot),
        vec![
            dependency_path("deps", "accepted.rs"),
            dependency_path("deps", "generated/keep.rs"),
        ]
    );
    assert_eq!(snapshot.report().indexed_files, 2);
    assert_eq!(snapshot.report().indexed_bytes, 6);
    assert!(!snapshot.report().truncated);
}

#[test]
fn dependency_index_deduplicates_overlapping_roots() {
    let container = TempDir::new().unwrap();
    let outer = container.path().join("outer");
    let nested = outer.join("nested");
    let workspace = outer.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_file(&outer, "outer.rs", b"outer");
    write_file(&nested, "nested.rs", b"nested");
    write_file(&workspace, "owned.rs", b"workspace");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = container.path().join("outside");
        fs::create_dir(&outside).unwrap();
        write_file(&outside, "secret.rs", b"secret");
        symlink(&outside, outer.join("escape")).unwrap();
        symlink(&outer, outer.join("cycle")).unwrap();
    }

    for roots in [
        vec![
            resolved_root("outer", &outer),
            resolved_root("outer-alias", outer.join(".")),
            resolved_root("nested", &nested),
        ],
        vec![
            resolved_root("nested", &nested),
            resolved_root("outer-alias", outer.join(".")),
            resolved_root("outer", &outer),
        ],
    ] {
        let adapter = FsDependencySource::new(roots, workspace.clone(), Vec::new()).unwrap();
        let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
        assert_eq!(snapshot.report().indexed_files, 2);
        assert_eq!(snapshot.report().indexed_bytes, 11);
        assert!(visible_paths(&snapshot).iter().all(|path| {
            !matches!(
                path.relative_path().as_str(),
                "workspace/owned.rs" | "escape/secret.rs"
            )
        }));
    }
}

#[test]
fn dependency_scan_maps_unavailable_and_non_directory_roots() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let missing = container.path().join("missing");
    let file = container.path().join("file-root");
    fs::create_dir(&workspace).unwrap();
    fs::write(&file, b"not a directory").unwrap();
    let adapter = FsDependencySource::new(
        vec![
            resolved_root("missing", missing),
            resolved_root("file", file),
        ],
        workspace,
        Vec::new(),
    )
    .unwrap();

    let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
    assert_eq!(
        issue_reasons(&snapshot),
        vec![
            DependencyIndexIssueReason::RootNotDirectory,
            DependencyIndexIssueReason::RootUnavailable,
        ]
    );
}

#[test]
fn dependency_scan_maps_unreadable_entries() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "denied.rs", b"private");
    let (adapter, _) = adapter_with_reader(
        vec![resolved_root("deps", root)],
        &workspace,
        Vec::new(),
        Some("denied.rs"),
        ReaderFault::Unreadable,
    );

    let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
    assert_eq!(
        issue_reasons(&snapshot),
        vec![DependencyIndexIssueReason::EntryUnreadable]
    );
    assert!(snapshot.files().is_empty());
}

#[test]
fn dependency_scan_discards_changed_or_disappearing_content() {
    for fault in [
        ReaderFault::Rewrite,
        ReaderFault::Remove,
        ReaderFault::Partial,
        ReaderFault::DisappearBeforeRead,
    ] {
        let container = TempDir::new().unwrap();
        let workspace = container.path().join("workspace");
        let root = container.path().join("root");
        fs::create_dir(&workspace).unwrap();
        write_file(&root, "changing.rs", b"original");
        let (adapter, _) = adapter_with_reader(
            vec![resolved_root("deps", root)],
            &workspace,
            Vec::new(),
            Some("changing.rs"),
            fault,
        );

        let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
        assert_eq!(
            issue_reasons(&snapshot),
            vec![DependencyIndexIssueReason::ChangedDuringRead]
        );
        assert!(snapshot.files().is_empty());
    }
}

#[test]
fn dependency_scan_rejects_invalid_utf8_without_partial_content() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "binary.rs", &[b'v', b'a', 0xff, b'l']);
    let adapter =
        FsDependencySource::new(vec![resolved_root("deps", root)], workspace, Vec::new()).unwrap();

    let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
    assert_eq!(
        issue_reasons(&snapshot),
        vec![DependencyIndexIssueReason::InvalidEncoding]
    );
    assert!(snapshot.files().is_empty());
}

#[test]
fn dependency_scan_excludes_boundary_violations() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    let outside = container.path().join("outside");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&root).unwrap();
    write_file(&outside, "secret.rs", b"secret");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    #[cfg(not(unix))]
    write_file(&root, "escape/secret.rs", b"secret");

    #[cfg(unix)]
    let adapter = FsDependencySource::new(
        vec![resolved_root("deps", root)],
        workspace.clone(),
        Vec::new(),
    )
    .unwrap();
    #[cfg(not(unix))]
    let (adapter, _) = adapter_with_reader(
        vec![resolved_root("deps", root)],
        &workspace,
        Vec::new(),
        Some("secret.rs"),
        ReaderFault::BoundaryViolation,
    );
    let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();

    assert_eq!(
        issue_reasons(&snapshot),
        vec![DependencyIndexIssueReason::BoundaryViolation]
    );
    assert!(snapshot.files().is_empty());

    #[cfg(unix)]
    {
        let raced_root = container.path().join("raced-root");
        let raced_outside = container.path().join("raced-outside.rs");
        write_file(&raced_root, "candidate.rs", b"inside");
        fs::write(&raced_outside, b"secret").unwrap();
        let adapter = FsDependencySource::with_reader(
            vec![resolved_root("raced", &raced_root)],
            workspace,
            Vec::new(),
            Arc::new(SwapBeforeConfinedOpenReader {
                outside: raced_outside,
                swapped: Mutex::new(false),
            }),
        )
        .unwrap();

        let raced = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
        assert_eq!(
            issue_reasons(&raced),
            vec![DependencyIndexIssueReason::BoundaryViolation]
        );
        assert!(raced.files().is_empty());
    }
}

#[test]
fn dependency_scan_continues_after_non_fatal_failures() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let broken = container.path().join("broken");
    let valid = container.path().join("valid");
    fs::create_dir(&workspace).unwrap();
    write_file(&broken, "denied.rs", b"private");
    write_file(&valid, "accepted.rs", b"accepted");
    let (adapter, _) = adapter_with_reader(
        vec![
            resolved_root("broken", broken),
            resolved_root("valid", valid),
        ],
        &workspace,
        Vec::new(),
        Some("denied.rs"),
        ReaderFault::Unreadable,
    );

    let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
    assert_eq!(
        issue_reasons(&snapshot),
        vec![DependencyIndexIssueReason::EntryUnreadable]
    );
    assert_eq!(
        visible_paths(&snapshot),
        vec![dependency_path("valid", "accepted.rs")]
    );
}

#[test]
fn dependency_scan_enforces_file_count_cap() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "a.rs", b"a");
    write_file(&root, "b.rs", b"b");
    let adapter =
        FsDependencySource::new(vec![resolved_root("deps", root)], workspace, Vec::new()).unwrap();

    let snapshot = adapter.build_snapshot(&limits(1, 8, 8)).unwrap();
    assert_eq!(snapshot.report().indexed_files, 1);
    assert_eq!(
        snapshot.report().limit_reasons,
        vec![DependencyLimitReason::FileCount]
    );
}

#[test]
fn dependency_scan_enforces_exact_and_oversize_file_bounds() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "exact.rs", b"1234");
    write_file(&root, "oversize.rs", b"12345");
    let adapter =
        FsDependencySource::new(vec![resolved_root("deps", &root)], workspace, Vec::new()).unwrap();

    let unbounded_for_platform = adapter
        .read_bounded(
            &dependency_path("deps", "exact.rs"),
            NonZeroU64::new(u64::MAX).unwrap(),
        )
        .expect("large caller limit must not cause an allocation panic");
    assert_eq!(unbounded_for_platform.bytes, b"1234");

    let snapshot = adapter.build_snapshot(&limits(2, 4, 16)).unwrap();
    assert_eq!(
        visible_paths(&snapshot),
        vec![dependency_path("deps", "exact.rs")]
    );
    assert_eq!(snapshot.report().indexed_bytes, 4);
    assert_eq!(
        snapshot.report().limit_reasons,
        vec![DependencyLimitReason::FileSize]
    );
}

#[test]
fn dependency_scan_enforces_total_byte_cap() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "a.rs", b"123");
    write_file(&root, "b.rs", b"456");
    let adapter =
        FsDependencySource::new(vec![resolved_root("deps", root)], workspace, Vec::new()).unwrap();

    let snapshot = adapter.build_snapshot(&limits(2, 8, 5)).unwrap();
    assert_eq!(snapshot.report().indexed_files, 1);
    assert_eq!(snapshot.report().indexed_bytes, 3);
    assert_eq!(
        snapshot.report().limit_reasons,
        vec![DependencyLimitReason::TotalBytes]
    );
}

#[test]
fn dependency_scan_diagnostics_do_not_expose_private_paths_or_errors() {
    const RAW_ERROR: &str = "RAW_PRIVATE_READER_ERROR";
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace-private-sentinel");
    let root = container.path().join("dependency-private-sentinel");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "denied.rs", b"PRIVATE_CONTENT_SENTINEL");
    let (reader, _) = InstrumentedReader::with_raw_error(
        Some("denied.rs"),
        ReaderFault::Unreadable,
        Some(RAW_ERROR),
    );
    let adapter = FsDependencySource::with_reader(
        vec![resolved_root("public-dependency", &root)],
        workspace.clone(),
        Vec::new(),
        reader,
    )
    .unwrap();

    let snapshot = adapter.build_snapshot(&limits(10, 32, 64)).unwrap();
    let diagnostic = format!("{:?}", snapshot.report());
    assert!(diagnostic.contains("EntryUnreadable"));
    for private in [
        RAW_ERROR,
        "PRIVATE_CONTENT_SENTINEL",
        &workspace.display().to_string(),
        &root.display().to_string(),
    ] {
        assert!(
            !diagnostic.contains(private),
            "leaked {private:?}: {diagnostic}"
        );
    }
}

#[test]
fn dependency_scan_respects_bounded_reads() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "a.rs", b"1234");
    write_file(&root, "b.rs", b"5678");
    write_file(&root, "c.rs", b"oversize");
    let (adapter, stats) = adapter_with_reader(
        vec![resolved_root("deps", root)],
        &workspace,
        Vec::new(),
        None,
        ReaderFault::None,
    );

    let snapshot = adapter.build_snapshot(&limits(2, 5, 6)).unwrap();
    let stats = stats.lock().unwrap();
    assert_eq!(snapshot.report().indexed_files, 1);
    assert_eq!(snapshot.report().indexed_bytes, 4);
    assert_eq!(snapshot.report().skipped_files, 2);
    assert_eq!(stats.candidate_count, 3);
    assert_eq!(stats.attempted_bytes, 7);
    assert_eq!(stats.accepted_files, 1);
    assert_eq!(stats.retained_bytes, 4);
    assert_eq!(stats.read_requests, vec![5, 2]);
    assert_eq!(stats.returned_bytes, vec![4, 2]);
    assert!(stats.read_requests.iter().all(|requested| *requested <= 5));
    assert!(
        stats
            .returned_bytes
            .iter()
            .zip(&stats.read_requests)
            .all(|(returned, requested)| { *returned as u64 <= *requested })
    );
}

#[test]
fn dependency_policies_are_evaluated_independently_per_root() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let first = container.path().join("first");
    let second = container.path().join("second");
    fs::create_dir(&workspace).unwrap();
    write_file(&first, ".towerignore", b"same.rs\n");
    write_file(&first, "same.rs", b"first");
    write_file(&second, ".towerignore", b"!same.rs\n");
    write_file(&second, "same.rs", b"second");
    for roots in [
        vec![
            resolved_root("first", &first),
            resolved_root("second", &second),
        ],
        vec![
            resolved_root("second", &second),
            resolved_root("first", &first),
        ],
    ] {
        let adapter = FsDependencySource::new(roots, workspace.clone(), Vec::new()).unwrap();
        let snapshot = adapter.build_snapshot(&limits(10, 16, 32)).unwrap();
        assert_eq!(
            visible_paths(&snapshot),
            vec![dependency_path("second", "same.rs")]
        );
    }
}

#[test]
fn dependency_scan_recovers_when_missing_root_becomes_available() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("late-root");
    fs::create_dir(&workspace).unwrap();
    let adapter =
        FsDependencySource::new(vec![resolved_root("deps", &root)], workspace, Vec::new()).unwrap();

    let unavailable = adapter.build_snapshot(&limits(10, 16, 32)).unwrap();
    assert_eq!(
        issue_reasons(&unavailable),
        vec![DependencyIndexIssueReason::RootUnavailable]
    );

    write_file(&root, "available.rs", b"ready");
    let recovered = adapter.build_snapshot(&limits(10, 16, 32)).unwrap();
    assert!(recovered.report().issues.is_empty());
    assert_eq!(
        visible_paths(&recovered),
        vec![dependency_path("deps", "available.rs")]
    );
}

/// The aggregate-budget branch must enforce the same boundary as content reads.
#[cfg(unix)]
#[test]
fn dependency_aggregate_probe_rejects_symlink_swap() {
    let container = TempDir::new().unwrap();
    let workspace = container.path().join("workspace");
    let root = container.path().join("root");
    let outside = container.path().join("outside.rs");
    fs::create_dir(&workspace).unwrap();
    write_file(&root, "candidate.rs", b"inside");
    fs::write(&outside, b"private outside bytes").unwrap();
    let source = FsDependencySource::with_reader(
        vec![resolved_root("deps", &root)],
        workspace,
        Vec::new(),
        Arc::new(SwapBeforeConfinedOpenReader {
            outside,
            swapped: Mutex::new(false),
        }),
    )
    .unwrap();
    let snapshot = source.build_snapshot(&limits(10, 32, 2)).unwrap();
    assert!(snapshot.files().is_empty());
    assert_eq!(
        issue_reasons(&snapshot),
        vec![DependencyIndexIssueReason::BoundaryViolation]
    );
}
