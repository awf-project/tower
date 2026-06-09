//! Event debouncer/coalescer for burst suppression (spec 06 U1).
//!
//! # Design
//!
//! ```text
//!  raw notify events ──► debounce_events(events) ──► Vec<WatchEvent>
//!
//!  Coalescing rules per path (last-write-wins for same-path events):
//!    Create + Create  → Create
//!    Create + Modify  → Create   (first create wins — don't emit spurious Modify)
//!    Create + Delete  → (nothing — created and deleted before flush)
//!    Modify + Modify  → Modify
//!    Delete + Create  → Create   (re-created: treat as fresh file)
//!    Delete + Modify  → Create   (file came back while we weren't looking)
//!    Rename(a→b) + Modify(b) → Rename(a→b)  (already at new path)
//! ```
//!
//! # Decision: in-process coalescing, not time-window debouncing
//!
//! The spec NOTE mandates injectable event channels for deterministic testing.
//! A time-window debouncer (typical for notify) requires wall-clock sleeps and
//! is therefore incompatible with deterministic tests. Instead:
//!
//! - The notify producer (see `NotifyWatcherAdapter`) collects raw events and
//!   calls `debounce_events` before forwarding to the channel.
//! - This coalesces multiple events about the same path into a single canonical
//!   event (last-wins strategy).
//! - Tests can call `debounce_events` directly without any timing dependency.
//!
//! # Trade-off
//!
//! In-process coalescing requires buffering a batch of raw events before
//! flushing them. The `RecommendedWatcher` from notify already provides a
//! `ReceiverStream` of debounced events via `notify::recommended_watcher`, but
//! that watcher introduces a configurable delay and cannot be driven
//! synchronously in tests. The in-process approach trades some latency
//! precision for full testability.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use super::event_processor::WatchEvent;

/// Coalesced event kind per path.
///
/// Note: `Rename` is never stored directly — the caller splits a Rename into
/// Delete(from) + Create(to) before calling `coalesce_into`. The enum variant
/// is kept for documentation clarity and potential future use.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Coalesced {
    Create,
    Modify,
    Delete,
}

/// Coalesce a batch of raw [`WatchEvent`]s into a deduplicated, canonicalised
/// list suitable for processing.
///
/// # Rules (per path, in order of arrival)
///
/// | Previous     | Incoming     | Result         |
/// |--------------|--------------|----------------|
/// | Create       | Modify       | Create         |
/// | Create       | Delete       | (removed)      |
/// | Modify       | Modify       | Modify         |
/// | Modify       | Delete       | Delete         |
/// | Delete       | Create       | Create         |
/// | Delete       | Modify       | Create         |
/// | —            | Rename(a→b)  | Delete(a) + Create(b) |
///
/// Events for paths not yet in the map are inserted as-is.
///
/// # Output order
///
/// The returned `Vec<WatchEvent>` preserves relative insertion order of paths
/// (HashMap iteration, which is arbitrary but deterministic per run). For
/// correctness, Rename events are emitted as a Delete followed by a Create.
///
/// # Example
///
/// ```rust
/// use std::path::PathBuf;
/// use core_engine::adapters::watcher::debounce::debounce_events;
/// use core_engine::adapters::watcher::event_processor::WatchEvent;
///
/// let events = vec![
///     WatchEvent::Create(PathBuf::from("/root/a.rs")),
///     WatchEvent::Create(PathBuf::from("/root/a.rs")), // duplicate → one Create
///     WatchEvent::Delete(PathBuf::from("/root/b.rs")),
///     WatchEvent::Create(PathBuf::from("/root/b.rs")), // Delete+Create → Create
/// ];
/// let coalesced = debounce_events(events);
/// assert_eq!(coalesced.len(), 2); // a.rs Create, b.rs Create
/// ```
#[must_use]
pub fn debounce_events(events: Vec<WatchEvent>) -> Vec<WatchEvent> {
    // Maintain insertion order for determinism by tracking order separately.
    let mut order: Vec<PathBuf> = Vec::new();
    let mut map: HashMap<PathBuf, Coalesced> = HashMap::new();

    for event in events {
        match event {
            WatchEvent::Create(path) => {
                coalesce_into(&mut order, &mut map, path, Coalesced::Create);
            }
            WatchEvent::Modify(path) => {
                coalesce_into(&mut order, &mut map, path, Coalesced::Modify);
            }
            WatchEvent::Delete(path) => {
                coalesce_into(&mut order, &mut map, path, Coalesced::Delete);
            }
            WatchEvent::Rename { from, to } => {
                // Rename is split: Delete(from) + Create(to).
                coalesce_into(&mut order, &mut map, from, Coalesced::Delete);
                coalesce_into(&mut order, &mut map, to.clone(), Coalesced::Create);
            }
        }
    }

    // Re-assemble in insertion order.
    let mut result = Vec::with_capacity(order.len());
    // Deduplicate order (a path may appear multiple times if it was coalesced
    // away and re-inserted).
    let mut seen = std::collections::HashSet::new();
    for path in order {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some(kind) = map.remove(&path) {
            match kind {
                Coalesced::Create => result.push(WatchEvent::Create(path)),
                Coalesced::Modify => result.push(WatchEvent::Modify(path)),
                Coalesced::Delete => result.push(WatchEvent::Delete(path)),
            }
        }
    }
    result
}

/// Apply the coalescing rules when inserting a new event into the map.
///
/// Decision: Rename is not a `Coalesced` variant — the caller splits Rename
/// into Delete(from) + Create(to) before calling this function. This keeps the
/// coalescing table exhaustive and avoids a phantom variant.
fn coalesce_into(
    order: &mut Vec<PathBuf>,
    map: &mut HashMap<PathBuf, Coalesced>,
    path: PathBuf,
    incoming: Coalesced,
) {
    use Coalesced::{Create, Delete, Modify};

    match map.get(&path) {
        None => {
            order.push(path.clone());
            map.insert(path, incoming);
        }
        Some(existing) => {
            let merged = match (existing, &incoming) {
                // Create + Modify: keep Create (just appeared, Modify is noise).
                (Create, Modify) => Some(Create),
                // Create + Delete: cancel out entirely (appeared and vanished).
                (Create, Delete) => {
                    map.remove(&path);
                    return;
                }
                // Create + Create: idempotent.
                (Create, Create) => Some(Create),
                // Modify + Modify: deduplicate.
                (Modify, Modify) => Some(Modify),
                // Modify + Delete: file is gone.
                (Modify, Delete) => Some(Delete),
                // Modify + Create: treat as fresh create (path re-appeared).
                (Modify, Create) => Some(Create),
                // Delete + Create: file came back.
                (Delete, Create) | (Delete, Modify) => Some(Create),
                // Delete + Delete: idempotent.
                (Delete, Delete) => Some(Delete),
            };
            if let Some(new_kind) = merged {
                map.insert(path, new_kind);
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// U1: burst of Creates on the same path → single Create.
    #[test]
    fn burst_of_creates_coalesces_to_one() {
        let events = vec![
            WatchEvent::Create(p("/root/a.rs")),
            WatchEvent::Create(p("/root/a.rs")),
            WatchEvent::Create(p("/root/a.rs")),
        ];
        let result = debounce_events(events);
        assert_eq!(result, vec![WatchEvent::Create(p("/root/a.rs"))]);
    }

    /// Create + Modify → Create (the file was just created, Modify is noise).
    #[test]
    fn create_then_modify_yields_create() {
        let events = vec![
            WatchEvent::Create(p("/root/a.rs")),
            WatchEvent::Modify(p("/root/a.rs")),
        ];
        let result = debounce_events(events);
        assert_eq!(result, vec![WatchEvent::Create(p("/root/a.rs"))]);
    }

    /// Create + Delete → nothing (file appeared and disappeared before flush).
    #[test]
    fn create_then_delete_cancels_out() {
        let events = vec![
            WatchEvent::Create(p("/root/a.rs")),
            WatchEvent::Delete(p("/root/a.rs")),
        ];
        let result = debounce_events(events);
        assert!(result.is_empty(), "create+delete must cancel: {result:?}");
    }

    /// Delete + Create → Create (file re-appeared).
    #[test]
    fn delete_then_create_yields_create() {
        let events = vec![
            WatchEvent::Delete(p("/root/a.rs")),
            WatchEvent::Create(p("/root/a.rs")),
        ];
        let result = debounce_events(events);
        assert_eq!(result, vec![WatchEvent::Create(p("/root/a.rs"))]);
    }

    /// Modify + Modify → single Modify.
    #[test]
    fn burst_of_modifies_coalesces_to_one() {
        let events = vec![
            WatchEvent::Modify(p("/root/a.rs")),
            WatchEvent::Modify(p("/root/a.rs")),
            WatchEvent::Modify(p("/root/a.rs")),
        ];
        let result = debounce_events(events);
        assert_eq!(result, vec![WatchEvent::Modify(p("/root/a.rs"))]);
    }

    /// Rename is split into Delete(from) + Create(to).
    #[test]
    fn rename_splits_into_delete_and_create() {
        let events = vec![WatchEvent::Rename {
            from: p("/root/a.rs"),
            to: p("/root/b.rs"),
        }];
        let result = debounce_events(events);
        // a.rs → Delete, b.rs → Create
        assert!(
            result.contains(&WatchEvent::Delete(p("/root/a.rs"))),
            "must contain Delete(a.rs); got {result:?}"
        );
        assert!(
            result.contains(&WatchEvent::Create(p("/root/b.rs"))),
            "must contain Create(b.rs); got {result:?}"
        );
    }

    /// Multiple independent paths are all preserved.
    #[test]
    fn independent_paths_all_preserved() {
        let events = vec![
            WatchEvent::Create(p("/root/a.rs")),
            WatchEvent::Modify(p("/root/b.rs")),
            WatchEvent::Delete(p("/root/c.rs")),
        ];
        let result = debounce_events(events);
        assert_eq!(result.len(), 3);
    }

    /// Empty input → empty output.
    #[test]
    fn empty_input_yields_empty_output() {
        assert!(debounce_events(vec![]).is_empty());
    }

    /// Burst of mixed events on two distinct paths.
    #[test]
    fn burst_two_paths_both_coalesced() {
        let events = vec![
            WatchEvent::Create(p("/root/a.rs")),
            WatchEvent::Create(p("/root/b.rs")),
            WatchEvent::Modify(p("/root/a.rs")), // a: Create+Modify → Create
            WatchEvent::Delete(p("/root/b.rs")), // b: Create+Delete → cancel
        ];
        let result = debounce_events(events);
        // Only a.rs as Create survives.
        assert_eq!(result, vec![WatchEvent::Create(p("/root/a.rs"))]);
    }
}
