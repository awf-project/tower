//! Unix socket paths and lifecycle helpers (connect / bind / spawn-lock).
#![forbid(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

#[derive(Debug)]
pub enum OwnershipError {
    Lock(std::io::Error),
    Metadata(std::io::Error),
    LivenessProbe(std::io::Error),
    LiveOwner,
    StaleSocketRemoval(std::io::Error),
    Bind(std::io::Error),
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(error) => write!(f, "ownership lock failure: {error}"),
            Self::Metadata(error) => write!(f, "ownership metadata failure: {error}"),
            Self::LivenessProbe(error) => write!(f, "owner liveness probe failure: {error}"),
            Self::LiveOwner => f.write_str("a live workspace daemon owner exists"),
            Self::StaleSocketRemoval(error) => {
                write!(f, "stale daemon socket removal failure: {error}")
            }
            Self::Bind(error) => write!(f, "daemon socket bind failure: {error}"),
        }
    }
}

impl std::error::Error for OwnershipError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lock(error)
            | Self::Metadata(error)
            | Self::LivenessProbe(error)
            | Self::StaleSocketRemoval(error)
            | Self::Bind(error) => Some(error),
            Self::LiveOwner => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerLiveness {
    Live,
    Dead,
    Absent,
}

#[derive(Debug)]
pub struct WorkspaceDaemonOwner {
    workspace_root: PathBuf,
    generation: u64,
    _lock: std::fs::File,
}

impl WorkspaceDaemonOwner {
    pub fn acquire(workspace_root: &Path) -> Result<Self, OwnershipError> {
        let runtime_dir = workspace_root.join(".tower");
        std::fs::create_dir_all(&runtime_dir).map_err(OwnershipError::Lock)?;
        let lock_path = owner_lock_path(workspace_root);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && socket_path(workspace_root).exists()
                {
                    OwnershipError::StaleSocketRemoval(error)
                } else {
                    OwnershipError::Lock(error)
                }
            })?;
        match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => {
                return Err(OwnershipError::LiveOwner);
            }
            Err(error) => {
                return Err(OwnershipError::Lock(std::io::Error::from_raw_os_error(
                    error.raw_os_error(),
                )));
            }
        }

        protect_or_remove_existing_socket(workspace_root)?;

        let metadata_path = owner_metadata_path(workspace_root);
        let previous = read_generation(&metadata_path)?;
        let generation = previous
            .checked_add(1)
            .ok_or_else(|| metadata_error("daemon ownership generation overflow"))?;
        write_owner_metadata(&metadata_path, generation)?;

        Ok(Self {
            workspace_root: workspace_root.to_path_buf(),
            generation,
            _lock: lock,
        })
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }
}

impl Drop for WorkspaceDaemonOwner {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self._lock, rustix::fs::FlockOperation::Unlock);
    }
}

pub fn probe_owner(workspace_root: &Path) -> Result<OwnerLiveness, OwnershipError> {
    if std::env::var_os("TOWER_TEST_FORCE_OWNER_LIVENESS_PROBE_FAILURE").is_some() {
        return Err(OwnershipError::LivenessProbe(std::io::Error::other(
            "injected owner liveness probe failure",
        )));
    }

    let lock_path = owner_lock_path(workspace_root);
    let metadata_path = owner_metadata_path(workspace_root);
    if !lock_path.exists() {
        return if metadata_path.exists() {
            read_generation(&metadata_path).map(|_| OwnerLiveness::Dead)
        } else {
            Ok(OwnerLiveness::Absent)
        };
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(OwnershipError::LivenessProbe)?;
    probe_owner_lock(&lock, &metadata_path)
}

fn probe_owner_lock(
    lock: &std::fs::File,
    metadata_path: &Path,
) -> Result<OwnerLiveness, OwnershipError> {
    match rustix::fs::flock(lock, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => Ok(OwnerLiveness::Live),
        Err(error) => Err(OwnershipError::LivenessProbe(
            std::io::Error::from_raw_os_error(error.raw_os_error()),
        )),
        Ok(()) => {
            // Probes may overlap without masquerading as an exclusive owner.
            // Explicit unlock also releases any copy inherited by a concurrent
            // fork before exec; closing only this descriptor is insufficient.
            let result = if metadata_path.exists() {
                read_generation(metadata_path).map(|_| OwnerLiveness::Dead)
            } else {
                Ok(OwnerLiveness::Absent)
            };
            rustix::fs::flock(lock, rustix::fs::FlockOperation::Unlock)
                .map_err(|error| OwnershipError::LivenessProbe(error.into()))?;
            result
        }
    }
}

fn owner_lock_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.owner.lock")
}

fn owner_metadata_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.owner")
}

fn metadata_error(message: impl Into<String>) -> OwnershipError {
    OwnershipError::Metadata(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

fn read_generation(path: &Path) -> Result<u64, OwnershipError> {
    if !path.exists() {
        return Ok(0);
    }
    let mut contents = String::new();
    std::fs::File::open(path)
        .and_then(|mut file| file.read_to_string(&mut contents))
        .map_err(OwnershipError::Metadata)?;
    contents
        .trim()
        .parse::<u64>()
        .map_err(|error| metadata_error(format!("invalid daemon owner generation: {error}")))
}

fn write_owner_metadata(path: &Path, generation: u64) -> Result<(), OwnershipError> {
    write_owner_metadata_with_hook(path, generation, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataPublicationStage {
    TemporaryOpened,
    TemporaryWritten,
    TemporarySynced,
    Renamed,
    DirectorySynced,
}

impl MetadataPublicationStage {
    #[cfg(test)]
    const ALL: [Self; 5] = [
        Self::TemporaryOpened,
        Self::TemporaryWritten,
        Self::TemporarySynced,
        Self::Renamed,
        Self::DirectorySynced,
    ];

    #[cfg(test)]
    const fn precedes_rename(self) -> bool {
        matches!(
            self,
            Self::TemporaryOpened | Self::TemporaryWritten | Self::TemporarySynced
        )
    }
}

fn write_owner_metadata_with_hook(
    path: &Path,
    generation: u64,
    mut checkpoint: impl FnMut(MetadataPublicationStage) -> std::io::Result<()>,
) -> Result<(), OwnershipError> {
    let parent = path
        .parent()
        .ok_or_else(|| metadata_error("daemon owner metadata has no parent directory"))?;
    let temporary = parent.join("daemon.owner.tmp");
    let mut publish = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temporary)?;
        checkpoint(MetadataPublicationStage::TemporaryOpened)?;
        writeln!(file, "{generation}")?;
        checkpoint(MetadataPublicationStage::TemporaryWritten)?;
        file.sync_all()?;
        checkpoint(MetadataPublicationStage::TemporarySynced)?;
        std::fs::rename(&temporary, path)?;
        checkpoint(MetadataPublicationStage::Renamed)?;
        std::fs::File::open(parent)?.sync_all()?;
        checkpoint(MetadataPublicationStage::DirectorySynced)
    };

    publish().map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        OwnershipError::Metadata(error)
    })
}

fn protect_or_remove_existing_socket(workspace_root: &Path) -> Result<(), OwnershipError> {
    let socket = socket_path(workspace_root);
    if !socket.exists() {
        return Ok(());
    }
    if socket.is_dir() {
        return Err(OwnershipError::Bind(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "daemon socket path is a directory",
        )));
    }
    match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(_) => Err(OwnershipError::LiveOwner),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;

                let parent = socket.parent().expect("socket path has runtime directory");
                if std::fs::metadata(parent)
                    .map_err(OwnershipError::StaleSocketRemoval)?
                    .permissions()
                    .mode()
                    & 0o222
                    == 0
                {
                    return Err(OwnershipError::StaleSocketRemoval(std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )));
                }
            }
            std::fs::remove_file(socket).map_err(OwnershipError::StaleSocketRemoval)
        }
        Err(error) => Err(OwnershipError::LivenessProbe(error)),
    }
}

/// `<workspace>/.tower/daemon.sock`.
#[must_use]
pub fn socket_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.sock")
}

/// `<workspace>/.tower/daemon.lock` — flock target for the spawn race.
#[must_use]
pub fn lock_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.lock")
}

/// `<workspace>/.tower/daemon.log` — detached daemon's stdout/stderr.
#[must_use]
pub fn log_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".tower").join("daemon.log")
}

/// Connect to the daemon socket. `None` when no daemon is listening
/// (file missing, or present-but-refused = stale after a crash).
pub async fn try_connect(path: &Path) -> std::io::Result<Option<UnixStream>> {
    classify_connect_result(UnixStream::connect(path).await)
}

fn classify_connect_result(
    result: std::io::Result<UnixStream>,
) -> std::io::Result<Option<UnixStream>> {
    match result {
        Ok(s) => Ok(Some(s)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Bind the daemon socket while its workspace lifetime ownership is held.
/// Stale cleanup is performed once, during owner acquisition.
pub fn bind_listener(owner: &WorkspaceDaemonOwner) -> Result<UnixListener, OwnershipError> {
    let path = socket_path(owner.workspace_root());
    if tokio::runtime::Handle::try_current().is_ok() {
        return UnixListener::bind(path).map_err(OwnershipError::Bind);
    }

    // Synchronous adapter tests also exercise stale-socket creation. Keep a
    // small reactor alive for listeners constructed outside an async context.
    static BIND_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let runtime = BIND_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("construct fallback socket-bind runtime")
    });
    let _entered = runtime.enter();
    UnixListener::bind(path).map_err(OwnershipError::Bind)
}

/// Acquire the exclusive spawn lock (blocking `flock`). The returned `File`
/// holds the lock; dropping it releases. Caller runs this on a blocking thread.
pub fn acquire_spawn_lock(workspace_root: &Path) -> std::io::Result<std::fs::File> {
    let path = lock_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    Ok(file)
}

/// Poll `try_connect` until it succeeds or `timeout` elapses.
pub async fn wait_for_socket(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(s) = try_connect(path).await? {
            return Ok(s);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for daemon socket",
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{MetadataPublicationStage, read_generation, write_owner_metadata_with_hook};

    #[test]
    fn concurrent_owner_probes_do_not_report_each_other_as_live_owners() {
        use super::{OwnerLiveness, WorkspaceDaemonOwner, owner_lock_path, probe_owner};
        use rustix::fs::{FlockOperation, flock};

        let dir = tempfile::tempdir().unwrap();
        let owner = WorkspaceDaemonOwner::acquire(dir.path()).unwrap();
        assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Live);
        drop(owner);

        let other_probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(owner_lock_path(dir.path()))
            .unwrap();
        flock(&other_probe, FlockOperation::NonBlockingLockShared).unwrap();
        assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Dead);
        assert!(matches!(
            WorkspaceDaemonOwner::acquire(dir.path()),
            Err(super::OwnershipError::LiveOwner)
        ));
        flock(&other_probe, FlockOperation::Unlock).unwrap();
        let owner = WorkspaceDaemonOwner::acquire(dir.path()).unwrap();
        assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Live);
        drop(owner);
        assert_eq!(probe_owner(dir.path()).unwrap(), OwnerLiveness::Dead);
    }

    #[test]
    fn owner_probe_unlocks_even_when_another_descriptor_retains_the_file() {
        use super::{
            OwnerLiveness, WorkspaceDaemonOwner, owner_lock_path, owner_metadata_path,
            probe_owner_lock,
        };

        let dir = tempfile::tempdir().unwrap();
        drop(WorkspaceDaemonOwner::acquire(dir.path()).unwrap());
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(owner_lock_path(dir.path()))
            .unwrap();
        // A duplicate shares the open file description, just like a descriptor
        // inherited by fork before exec. Closing the probe alone cannot unlock it.
        let retained = lock.try_clone().unwrap();
        assert_eq!(
            probe_owner_lock(&lock, &owner_metadata_path(dir.path())).unwrap(),
            OwnerLiveness::Dead
        );
        drop(lock);
        let owner = WorkspaceDaemonOwner::acquire(dir.path())
            .expect("retained probe descriptor must not retain the lock");
        drop(owner);
        drop(retained);
    }

    #[test]
    fn generation_publication_recovers_from_interruption_at_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = dir.path().join("daemon.owner");
        std::fs::write(&metadata, b"41\n").unwrap();

        for stage in MetadataPublicationStage::ALL {
            std::fs::write(&metadata, b"41\n").unwrap();
            let result = write_owner_metadata_with_hook(&metadata, 42, |reached| {
                if reached == stage {
                    Err(std::io::Error::other("injected publication interruption"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "stage {stage:?} must inject a failure");

            let durable = read_generation(&metadata).expect("metadata stays parseable");
            if stage.precedes_rename() {
                assert_eq!(durable, 41, "stage {stage:?} must preserve predecessor");
            } else {
                assert_eq!(durable, 42, "stage {stage:?} published the successor");
            }
            write_owner_metadata_with_hook(&metadata, durable + 1, |_| Ok(()))
                .expect("next publication recovers");
        }
    }
}

#[cfg(test)]
mod connect_error_tests {
    use super::*;
    #[test]
    fn only_missing_or_refused_sockets_authorize_spawn_retry() {
        use std::io::ErrorKind;
        for kind in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            assert!(
                classify_connect_result(Err(std::io::Error::from(kind)))
                    .unwrap()
                    .is_none()
            );
        }
        for kind in [ErrorKind::PermissionDenied, ErrorKind::Interrupted] {
            assert_eq!(
                classify_connect_result(Err(std::io::Error::from(kind)))
                    .unwrap_err()
                    .kind(),
                kind
            );
        }
        for errno in [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE] {
            let error = classify_connect_result(Err(std::io::Error::from_raw_os_error(
                errno.raw_os_error(),
            )))
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno.raw_os_error()));
        }
    }
}
