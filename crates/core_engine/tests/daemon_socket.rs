//! Socket helper integration tests (Task 4).
#![forbid(unsafe_code)]

use std::time::Duration;

use core_engine::adapters::daemon::socket::{
    acquire_spawn_lock, bind_listener, socket_path, try_connect, wait_for_socket,
};
use tempfile::tempdir;

#[tokio::test]
async fn try_connect_returns_none_when_absent() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    let path = socket_path(dir.path());
    assert!(try_connect(&path).await.is_none());
}

#[tokio::test]
async fn bind_then_connect_succeeds() {
    let dir = tempdir().unwrap();
    let path = socket_path(dir.path());
    let _listener = bind_listener(&path).expect("bind");
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
        let _l = bind_listener(&path).expect("bind 1");
    }
    // Stale file present but nobody listening → connect refused/absent.
    assert!(try_connect(&path).await.is_none());
    // Re-bind must clean the stale file and succeed.
    let _l2 = bind_listener(&path).expect("rebind over stale");
    assert!(try_connect(&path).await.is_some());
}

#[tokio::test]
async fn bind_refuses_when_a_live_listener_exists() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".tower")).unwrap();
    let path = socket_path(dir.path());
    let _live = bind_listener(&path).expect("bind 1");
    let err = bind_listener(&path).expect_err("second bind must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
}
