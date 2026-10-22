//! `NotifyWatcherAdapter` — reactive filesystem monitoring (spec 06).
//!
//! # Wireframe
//!
//! ```text
//!  OS FS events ──notify──► debounce/coalesce ──channel──► EventProcessor
//!                                                                │
//!   ┌──────────────────────────────────────────────────────────┼────────────────┐
//!   │ Create  → ws.insert + idx.insert + storage.put            │                │
//!   │ Modify  → remove+reinsert metadata + delta reindex + put  │                │
//!   │ Delete  → ws.remove + idx.remove + storage.delete         │                │
//!   │ Rename  → Delete(from) + Create(to)  (bijection preserved)│                │
//!   └──────────────────────────────────────────────────────────┬────────────────┘
//!                                                              ▼ on_file_changed
//!   Shared state: Arc<RwLock<ProjectWorkspace>>
//!                 Arc<RwLock<InvertedIndex>>
//!     Lock order: workspace → index  (total order prevents deadlock)
//!     Critical sections: domain mutation only; storage I/O outside the lock
//! ```
//!
//! # Architecture note (spec NOTE)
//!
//! The event-processing CORE ([`EventProcessor`]) is decoupled from the
//! notify PRODUCER ([`NotifyWatcherAdapter`]).  Tests drive `EventProcessor`
//! directly with injected [`WatchEvent`] values — no OS timing, no sleeps.
//! The `NotifyWatcherAdapter` has only a thin smoke test.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::domain::refactor::PathMove;
use crate::domain::{FileMetadata, RelativePath, VirtualFile};

pub mod debounce;
pub mod error;
pub mod event_processor;
mod notify_watcher;
mod staged_storage;

pub use error::WatcherError;
pub use event_processor::{EventProcessor, WatchEvent};
pub use notify_watcher::NotifyWatcherAdapter;

const ECHO_RETENTION: Duration = Duration::from_secs(2);
const MAX_RETAINED_REGISTRATIONS: usize = 1_024;
const WAIT_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub struct RenameCoordinator {
    shared: Arc<RenameCoordinationShared>,
}

#[derive(Debug)]
pub struct EngineOwnedRenameGuard {
    shared: Arc<RenameCoordinationShared>,
    registration_id: Option<u64>,
}

#[derive(Debug, Default)]
struct RenameCoordinationShared {
    state: Mutex<RenameCoordinationState>,
    released: Condvar,
}

#[derive(Debug, Default)]
struct RenameCoordinationState {
    next_registration_id: u64,
    watcher_active: bool,
    active: HashMap<u64, ActiveRegistration>,
    retained: VecDeque<CommittedRegistration>,
}

#[derive(Debug)]
struct ActiveRegistration {
    moves: Vec<PathMove>,
    committed_files: Option<Vec<VirtualFile>>,
}

#[derive(Debug)]
struct CommittedRegistration {
    retained_at: Instant,
    pairs: Vec<CommittedPair>,
}

#[derive(Debug)]
struct CommittedPair {
    source: RelativePath,
    destination: RelativePath,
    file: VirtualFile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameEventDisposition {
    WaitForCommit,
    ConsumeEcho,
    External,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameCoordinationError {
    EmptyMoveSet,
    DuplicateMove {
        source: RelativePath,
        destination: RelativePath,
    },
    OverlappingMove {
        path: RelativePath,
    },
    Cancelled,
    Poisoned,
}

impl RenameCoordinator {
    pub fn register_engine_owned(
        &self,
        moves: &[PathMove],
    ) -> Result<EngineOwnedRenameGuard, RenameCoordinationError> {
        if moves.is_empty() {
            return Err(RenameCoordinationError::EmptyMoveSet);
        }

        let mut endpoints = HashSet::with_capacity(moves.len() * 2);
        let mut exact_pairs = HashSet::with_capacity(moves.len());
        for path_move in moves {
            if !exact_pairs.insert((&path_move.source, &path_move.destination)) {
                return Err(RenameCoordinationError::DuplicateMove {
                    source: path_move.source.clone(),
                    destination: path_move.destination.clone(),
                });
            }
            for path in [&path_move.source, &path_move.destination] {
                if !endpoints.insert(path) {
                    return Err(RenameCoordinationError::OverlappingMove { path: path.clone() });
                }
            }
        }

        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| RenameCoordinationError::Poisoned)?;
        expire_retained(&mut state, Instant::now());
        for path_move in moves {
            for active in state.active.values() {
                if active.moves.iter().any(|registered| {
                    move_contains_path(registered, &path_move.source)
                        || move_contains_path(registered, &path_move.destination)
                }) {
                    return Err(RenameCoordinationError::OverlappingMove {
                        path: if active
                            .moves
                            .iter()
                            .any(|registered| move_contains_path(registered, &path_move.source))
                        {
                            path_move.source.clone()
                        } else {
                            path_move.destination.clone()
                        },
                    });
                }
            }
        }

        let registration_id = state.next_registration_id;
        state.next_registration_id = state.next_registration_id.wrapping_add(1);
        state.active.insert(
            registration_id,
            ActiveRegistration {
                moves: moves.to_vec(),
                committed_files: None,
            },
        );
        Ok(EngineOwnedRenameGuard {
            shared: Arc::clone(&self.shared),
            registration_id: Some(registration_id),
        })
    }

    pub fn classify_event(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
    ) -> Result<RenameEventDisposition, RenameCoordinationError> {
        self.classify_event_at(source, destination, Instant::now())
    }

    pub(crate) fn classify_event_at(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
        now: Instant,
    ) -> Result<RenameEventDisposition, RenameCoordinationError> {
        self.classify_event_matching(source, destination, now, |_| true)
    }

    fn classify_event_matching(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
        now: Instant,
        retained_matches: impl Fn(&CommittedPair) -> bool,
    ) -> Result<RenameEventDisposition, RenameCoordinationError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| RenameCoordinationError::Poisoned)?;
        expire_retained(&mut state, now);
        if state.active.values().any(|registration| {
            registration.moves.iter().any(|path_move| {
                path_move.source == *source && path_move.destination == *destination
            })
        }) {
            return Ok(RenameEventDisposition::WaitForCommit);
        }

        for registration in &mut state.retained {
            if let Some(index) = registration.pairs.iter().position(|pair| {
                pair.source == *source && pair.destination == *destination && retained_matches(pair)
            }) {
                registration.pairs.remove(index);
                state.retained.retain(|entry| !entry.pairs.is_empty());
                return Ok(RenameEventDisposition::ConsumeEcho);
            }
        }
        Ok(RenameEventDisposition::External)
    }

    /// Classify a native rename pair and consume a retained echo only when the
    /// destination still has the metadata committed by the explicit rename.
    /// A mismatch is an external filesystem action and deliberately leaves the
    /// retained echo available for the real delayed notification.
    pub fn classify_verified_event(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
        observed: &FileMetadata,
    ) -> Result<RenameEventDisposition, RenameCoordinationError> {
        self.classify_event_matching(source, destination, Instant::now(), |pair| {
            pair.file.size == observed.size
                && pair.file.modified == observed.modified
                && pair
                    .file
                    .content_hash
                    .is_none_or(|hash| observed.content_hash == Some(hash))
        })
    }

    pub fn wait_until_released(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
        cancelled: &AtomicBool,
    ) -> Result<(), RenameCoordinationError> {
        self.wait_while(cancelled, |registration| {
            registration.moves.iter().any(|path_move| {
                path_move.source == *source && path_move.destination == *destination
            })
        })
    }

    pub fn wait_for_path(
        &self,
        path: &RelativePath,
        cancelled: &AtomicBool,
    ) -> Result<(), RenameCoordinationError> {
        self.wait_while(cancelled, |registration| {
            registration
                .moves
                .iter()
                .any(|path_move| move_contains_path(path_move, path))
        })
    }

    pub fn wake_waiters(&self) {
        self.shared.released.notify_all();
    }

    pub fn set_watcher_active(&self, active: bool) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.watcher_active = active;
            if !active {
                state.retained.clear();
            }
        }
        self.shared.released.notify_all();
    }

    fn wait_while(
        &self,
        cancelled: &AtomicBool,
        matches: impl Fn(&ActiveRegistration) -> bool,
    ) -> Result<(), RenameCoordinationError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| RenameCoordinationError::Poisoned)?;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(RenameCoordinationError::Cancelled);
            }
            if !state.active.values().any(&matches) {
                return Ok(());
            }
            let (next_state, _) = self
                .shared
                .released
                .wait_timeout(state, WAIT_INTERVAL)
                .map_err(|_| RenameCoordinationError::Poisoned)?;
            state = next_state;
        }
    }
}

impl EngineOwnedRenameGuard {
    pub fn commit(&mut self, committed_files: &[VirtualFile]) {
        let Some(registration_id) = self.registration_id else {
            return;
        };
        if let Ok(mut state) = self.shared.state.lock()
            && let Some(registration) = state.active.get_mut(&registration_id)
        {
            registration.committed_files = Some(committed_files.to_vec());
        }
    }
}

impl Drop for EngineOwnedRenameGuard {
    fn drop(&mut self) {
        let Some(registration_id) = self.registration_id.take() else {
            return;
        };
        if let Ok(mut state) = self.shared.state.lock()
            && let Some(registration) = state.active.remove(&registration_id)
            && state.watcher_active
            && let Some(committed_files) = registration.committed_files
        {
            let pairs = registration
                .moves
                .into_iter()
                .filter_map(|path_move| {
                    committed_files
                        .iter()
                        .find(|file| file.path == path_move.destination)
                        .cloned()
                        .map(|file| CommittedPair {
                            source: path_move.source,
                            destination: path_move.destination,
                            file,
                        })
                })
                .collect::<Vec<_>>();
            if !pairs.is_empty() {
                expire_retained(&mut state, Instant::now());
                state.retained.push_back(CommittedRegistration {
                    retained_at: Instant::now(),
                    pairs,
                });
                while state.retained.len() > MAX_RETAINED_REGISTRATIONS {
                    state.retained.pop_front();
                }
            }
        }
        self.shared.released.notify_all();
    }
}

fn move_contains_path(path_move: &PathMove, path: &RelativePath) -> bool {
    path_move.source == *path || path_move.destination == *path
}

fn expire_retained(state: &mut RenameCoordinationState, now: Instant) {
    state.retained.retain(|registration| {
        now.checked_duration_since(registration.retained_at)
            .is_none_or(|age| age < ECHO_RETENTION)
    });
}
