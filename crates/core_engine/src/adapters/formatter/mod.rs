//! Host-side format queue and worker pool (spec 13a).
//!
//! # Wireframe
//!
//! ```text
//!  host_request_format(path) → Accepted | Dropped
//!    FormatQueue::enqueue(path)
//!      path in-flight/queued → mark Dirty, return Accepted   (coalesce)
//!      queue full            → return Dropped                 (best-effort)
//!      else                  → push to deque, mark InFlight, spawn job
//!
//!  FormatWorker::run_filter_job(path, tool)
//!    bytes, h0 = read(path)
//!    run argv with stdin=bytes, capture stdout
//!    exit != 0  → no-op, log stderr, increment failure count
//!    out == bytes → no-op (already a fixed point)
//!    out != bytes → CAS check, shadow-write, insert(path) into echo_set
//!
//!  FormatWorker::run_inplace_job(path, tool)
//!    create <dir>/<stem>.fmt.tmp
//!    copy source bytes there
//!    substitute {path} in argv with tmp path
//!    run argv, on success read tmp back, shadow-write
//! ```
//!
//! # Hexagonal boundary
//!
//! This module imports `std::fs`, `std::process::Command`, and config types.
//! It never imports domain types except `RelativePath` (a plain newtype with no I/O).
//! The domain `PluginHostRegistry` receives the echo-set as
//! `Arc<Mutex<HashMap<String, ()>>>` — a stdlib-only type.

pub mod format_queue;
pub mod worker;

pub use format_queue::{EnqueueResult, FormatQueue, FormatQueuePort, NoOpFormatQueue};
