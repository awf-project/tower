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

pub mod debounce;
pub mod error;
pub mod event_processor;
mod notify_watcher;

pub use error::WatcherError;
pub use event_processor::{EventProcessor, WatchEvent};
pub use notify_watcher::NotifyWatcherAdapter;
