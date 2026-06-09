//! Watcher-specific error type (spec 06).
//!
//! Typed errors for the `NotifyWatcherAdapter` lifecycle and OS subscription.
//! Kept separate from `PortError` so callers can distinguish infrastructure
//! failures (OS notify not available) from domain failures (path outside root).

use std::fmt;

/// Errors that can be returned by the watcher adapter.
///
/// All variants are explicitly named so callers can handle them without
/// stringly-typed matching.
#[derive(Debug)]
pub enum WatcherError {
    /// The notify crate failed to create or configure the OS watcher.
    NotifyInit(notify::Error),
    /// The notify crate failed to add or remove a watch path.
    NotifyWatch(notify::Error),
    /// The worker thread channel is closed (the processor thread has exited).
    ChannelClosed,
    /// A storage operation on the watcher's event-processing path failed.
    Storage(crate::ports::PortError),
    /// The OS refused to create the watcher worker thread (thread-limit / OOM).
    ///
    /// Callers that rely on `Result<_, WatcherError>` can handle this gracefully
    /// instead of receiving an unrecoverable panic from `.expect()`.
    SpawnFailed(String),
}

impl fmt::Display for WatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotifyInit(e) => write!(f, "watcher initialisation failed: {e}"),
            Self::NotifyWatch(e) => write!(f, "watcher watch path failed: {e}"),
            Self::ChannelClosed => write!(f, "watcher event channel closed"),
            Self::Storage(e) => write!(f, "watcher storage error: {e}"),
            Self::SpawnFailed(reason) => write!(f, "failed to spawn watcher thread: {reason}"),
        }
    }
}

impl core::error::Error for WatcherError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::NotifyInit(e) | Self::NotifyWatch(e) => Some(e),
            Self::Storage(e) => Some(e),
            Self::ChannelClosed | Self::SpawnFailed(_) => None,
        }
    }
}

impl From<crate::ports::PortError> for WatcherError {
    fn from(e: crate::ports::PortError) -> Self {
        Self::Storage(e)
    }
}
