//! Socket helper integration tests (Task 4).
#![forbid(unsafe_code)]

use std::{path::Path, process::Command, time::Duration};

use core_engine::adapters::daemon::client::SpawnAttemptGuard;
use core_engine::adapters::daemon::socket::{
    OwnerLiveness, OwnershipError, WorkspaceDaemonOwner, acquire_spawn_lock, bind_listener,
    probe_owner, socket_path, try_connect, wait_for_socket,
};
use tempfile::tempdir;

#[test]
fn workspace_daemon_owner_acquire_retains_generation_and_root_until_drop() {
    let dir = tempdir().unwrap();

    let first = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire first owner");
    let first_generation = first.generation();
    assert_eq!(first.workspace_root(), dir.path());
    assert!(first_generation > 0);
    assert!(matches!(
        WorkspaceDaemonOwner::acquire(dir.path()),
        Err(OwnershipError::LiveOwner)
    ));

    drop(first);
    let successor = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire successor owner");
    assert!(successor.generation() > first_generation);
}

#[test]
fn failed_generation_publication_preserves_recoverable_metadata() {
    let dir = tempdir().unwrap();
    let first = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire first owner");
    let first_generation = first.generation();
    drop(first);

    let metadata = dir.path().join(".tower/daemon.owner");
    let publication_temp = dir.path().join(".tower/daemon.owner.tmp");
    std::fs::create_dir(&publication_temp).expect("block temporary metadata creation");

    let failed = WorkspaceDaemonOwner::acquire(dir.path());
    assert!(matches!(failed, Err(OwnershipError::Metadata(_))));
    assert_eq!(
        std::fs::read_to_string(&metadata).unwrap().trim(),
        first_generation.to_string(),
        "failed publication must leave the last durable generation intact"
    );

    std::fs::remove_dir(publication_temp).unwrap();
    let recovered =
        WorkspaceDaemonOwner::acquire(dir.path()).expect("recover after failed publish");
    assert_eq!(recovered.generation(), first_generation + 1);
}

#[test]
fn spawn_attempt_guard_never_creates_or_releases_daemon_owner() {
    let dir = tempdir().unwrap();
    assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Absent);

    let spawn_guard = SpawnAttemptGuard::acquire(dir.path()).expect("acquire spawn guard");
    assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Absent);

    let owner = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire daemon owner");
    drop(spawn_guard);
    assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Live);
    assert!(matches!(
        WorkspaceDaemonOwner::acquire(dir.path()),
        Err(OwnershipError::LiveOwner)
    ));
    drop(owner);
}

#[test]
fn owner_liveness_distinguishes_live_dead_and_absent() {
    let absent = tempdir().unwrap();
    assert_eq!(probe_owner(absent.path()).unwrap(), OwnerLiveness::Absent);

    let owned = tempdir().unwrap();
    let owner = WorkspaceDaemonOwner::acquire(owned.path()).expect("acquire owner");
    assert_eq!(probe_owner(owned.path()).unwrap(), OwnerLiveness::Live);
    drop(owner);
    assert_eq!(probe_owner(owned.path()).unwrap(), OwnerLiveness::Dead);
}

#[test]
fn ownership_failures_preserve_exact_error_variants() {
    const FORCE_LIVENESS_PROBE_FAILURE: &str = "TOWER_TEST_FORCE_OWNER_LIVENESS_PROBE_FAILURE";

    // Run this one injected case in a child process: changing process-wide environment
    // variables in a concurrently executing integration-test process is unsound.
    if std::env::var_os(FORCE_LIVENESS_PROBE_FAILURE).is_some() {
        let dir = tempdir().unwrap();
        assert_ownership_error(probe_owner(dir.path()), "liveness probe fixture", |error| {
            matches!(error, OwnershipError::LivenessProbe(_))
        });
        return;
    }

    let blocked_runtime = tempdir().unwrap();
    std::fs::write(blocked_runtime.path().join(".tower"), b"not a directory").unwrap();
    assert_ownership_error(
        WorkspaceDaemonOwner::acquire(blocked_runtime.path()),
        "runtime directory blocked by a file",
        |error| matches!(error, OwnershipError::Lock(_)),
    );

    let live = tempdir().unwrap();
    let owner = WorkspaceDaemonOwner::acquire(live.path()).expect("acquire owner");
    assert_ownership_error(
        WorkspaceDaemonOwner::acquire(live.path()),
        "second lifetime-owner acquisition",
        |error| matches!(error, OwnershipError::LiveOwner),
    );
    drop(owner);

    let malformed_metadata = tempdir().unwrap();
    let metadata_owner =
        WorkspaceDaemonOwner::acquire(malformed_metadata.path()).expect("create owner metadata");
    drop(metadata_owner);
    corrupt_owner_metadata(malformed_metadata.path());
    assert_ownership_error(
        probe_owner(malformed_metadata.path()),
        "corrupt persisted owner metadata",
        |error| matches!(error, OwnershipError::Metadata(_)),
    );

    let test = std::env::current_exe().expect("test executable");
    let status = Command::new(test)
        .arg("ownership_failures_preserve_exact_error_variants")
        .arg("--exact")
        .arg("--nocapture")
        .env(FORCE_LIVENESS_PROBE_FAILURE, "1")
        .status()
        .expect("run liveness-probe failure fixture");
    assert!(status.success(), "liveness probe fixture must pass");

    let stale_socket = tempdir().unwrap();
    let path = socket_path(stale_socket.path());
    let fixture_owner = WorkspaceDaemonOwner::acquire(stale_socket.path())
        .expect("acquire stale socket fixture owner");
    let listener = bind_listener(&fixture_owner).expect("create stale socket fixture");
    drop(listener);
    drop(fixture_owner);
    make_parent_non_writable(&path);
    let result = WorkspaceDaemonOwner::acquire(stale_socket.path());
    restore_parent_permissions(&path);
    assert_ownership_error(result, "unremovable stale socket", |error| {
        matches!(error, OwnershipError::StaleSocketRemoval(_))
    });

    let bind_collision = tempdir().unwrap();
    let socket = socket_path(bind_collision.path());
    std::fs::create_dir_all(&socket).expect("directory at socket path");
    assert_ownership_error(
        WorkspaceDaemonOwner::acquire(bind_collision.path()),
        "socket bind collision",
        |error| matches!(error, OwnershipError::Bind(_)),
    );
}

#[tokio::test]
async fn live_owner_and_socket_are_never_removed_by_elapsed_time() {
    let dir = tempdir().unwrap();
    let owner = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire owner");
    let path = socket_path(dir.path());
    let listener = bind_listener(&owner).expect("bind live socket");

    assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Live);
    age_socket_to_epoch(&path);
    let error = bind_listener(&owner).expect_err("live socket must not be replaced");
    assert!(matches!(error, OwnershipError::Bind(_)));
    assert!(
        path.exists(),
        "a live socket must remain after an aged retry"
    );
    assert!(try_connect(&path).await.unwrap().is_some());

    drop(listener);
    drop(owner);
}

fn assert_ownership_error(
    result: Result<impl Sized, OwnershipError>,
    fixture: &str,
    expected: impl FnOnce(&OwnershipError) -> bool,
) {
    match result {
        Err(error) if expected(&error) => {}
        Err(error) => panic!("{fixture}: unexpected ownership error: {error}"),
        Ok(_) => panic!("{fixture}: operation unexpectedly succeeded"),
    }
}

fn corrupt_owner_metadata(workspace_root: &Path) {
    let runtime = workspace_root.join(".tower");
    let metadata_files: Vec<_> = std::fs::read_dir(&runtime)
        .expect("read runtime directory")
        .map(|entry| entry.expect("runtime entry").path())
        .filter(|path| path.is_file() && path.file_name().is_some_and(|name| name != "daemon.lock"))
        .collect();
    assert!(
        !metadata_files.is_empty(),
        "acquiring an owner must persist owner metadata"
    );
    for path in metadata_files {
        std::fs::write(path, b"corrupt owner metadata").expect("corrupt owner metadata");
    }
}

#[cfg(unix)]
fn age_socket_to_epoch(path: &Path) {
    let status = Command::new("touch")
        .arg("--date=@0")
        .arg(path)
        .status()
        .expect("run touch for aged live socket evidence");
    assert!(status.success(), "age live socket evidence");
}

#[cfg(unix)]
fn make_parent_non_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().expect("socket parent");
    let mut permissions = std::fs::metadata(parent)
        .expect("socket parent metadata")
        .permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(parent, permissions).expect("make socket parent non-writable");
}

#[cfg(unix)]
fn restore_parent_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().expect("socket parent");
    let mut permissions = std::fs::metadata(parent)
        .expect("socket parent metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(parent, permissions).expect("restore socket parent permissions");
}

#[tokio::test]
async fn try_connect_returns_none_when_absent() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    let path = socket_path(dir.path());
    assert!(try_connect(&path).await.unwrap().is_none());
}

#[tokio::test]
async fn bind_then_connect_succeeds() {
    let dir = tempdir().unwrap();
    let owner = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire owner");
    let path = socket_path(dir.path());
    let _listener = bind_listener(&owner).expect("bind");
    let conn = wait_for_socket(&path, Duration::from_secs(2)).await;
    assert!(conn.is_ok());
}

#[test]
fn acquire_spawn_lock_creates_runtime_dir() {
    let dir = tempdir().unwrap();
    let lock = acquire_spawn_lock(dir.path()).expect("lock");
    assert!(dir.path().join(".tower").is_dir());
    drop(lock);
}

#[tokio::test]
async fn bind_cleans_a_stale_socket_file() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    let path = socket_path(dir.path());
    // First listener, then drop it: leaves a stale socket file behind.
    {
        let owner = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire first owner");
        let _l = bind_listener(&owner).expect("bind 1");
    }
    // Successor acquisition must prove the listener is dead, clean the stale
    // socket, and permit a new bind.
    let successor = WorkspaceDaemonOwner::acquire(dir.path()).expect("recover stale socket");
    let _l2 = bind_listener(&successor).expect("rebind over stale");
    assert!(try_connect(&path).await.unwrap().is_some());
}

#[tokio::test]
async fn bind_refuses_when_a_live_listener_exists() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    let owner = WorkspaceDaemonOwner::acquire(dir.path()).expect("acquire owner");
    let _live = bind_listener(&owner).expect("bind 1");
    let err = bind_listener(&owner).expect_err("second bind must fail");
    assert!(matches!(err, OwnershipError::Bind(_)));
}

#[cfg(unix)]
#[test]
fn non_refusal_socket_probe_error_preserves_ownerless_active_socket() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempdir().unwrap();
    let path = socket_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o000);
    std::fs::set_permissions(&path, permissions).unwrap();

    let result = WorkspaceDaemonOwner::acquire(dir.path());
    assert!(matches!(result, Err(OwnershipError::LivenessProbe(_))));
    assert!(
        path.exists(),
        "an inconclusive probe must not unlink the socket"
    );
}
