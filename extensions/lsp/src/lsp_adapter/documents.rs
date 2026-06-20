//! `DocumentTracker` — ref-counted open-document state for an LSP session.
//!
//! The LSP `textDocument/did{Open,Change,Close}` lifecycle is reference-counted
//! so that several concurrent requests (and the watcher) can share one open
//! document without racing `didOpen`/`didClose`. The tracker is the pure
//! decision core: it owns no transport and performs no I/O. Each call returns
//! the [`DocSync`] action the caller must serialise and send.
//!
//! Invariants (spec 14b U2/AC4):
//! - exactly one `didOpen` per (uri, open-lifetime) — the first acquire;
//! - exactly one `didClose` — when the last reference is released;
//! - `version` is monotonic per uri, incremented only when content changes.
//!
//! # Decision: re-acquire suppresses `didChange` for unchanged content
//! # Why: the `OpenDocument` carries a last-synced content hash precisely so a
//! #      re-acquire (or a formatter echo) with identical bytes emits nothing,
//! #      keeping the sync loop-free (UN1). A `didChange` is emitted only when
//! #      the text hash actually differs from the last sync.
//! # Trade-off: callers that want to *force* a re-sync of identical bytes can't
//! #            do it through `acquire` — but no current flow needs that.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// The LSP document-sync action a caller must perform after a tracker call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocSync {
    /// Send `textDocument/didOpen`.
    Open {
        version: i64,
        language_id: String,
        text: String,
    },
    /// Send `textDocument/didChange` (full content replace).
    Change { version: i64, text: String },
    /// Send `textDocument/didClose`.
    Close,
}

/// Per-uri open-document state.
struct OpenDocument {
    version: i64,
    refcount: u32,
    content_hash: u64,
}

/// Reference-counted registry of open documents for one LSP session.
#[derive(Default)]
pub struct DocumentTracker {
    docs: HashMap<String, OpenDocument>,
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl DocumentTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            docs: HashMap::new(),
        }
    }

    /// Acquire a hold on `uri` with the given current `text`.
    ///
    /// - First hold → [`DocSync::Open`] at version 1.
    /// - Later hold, content changed → [`DocSync::Change`] with bumped version.
    /// - Later hold, content identical → `None` (no spurious sync).
    pub fn acquire(&mut self, uri: &str, language_id: &str, text: &str) -> Option<DocSync> {
        let hash = hash_text(text);
        match self.docs.get_mut(uri) {
            None => {
                self.docs.insert(
                    uri.to_owned(),
                    OpenDocument {
                        version: 1,
                        refcount: 1,
                        content_hash: hash,
                    },
                );
                Some(DocSync::Open {
                    version: 1,
                    language_id: language_id.to_owned(),
                    text: text.to_owned(),
                })
            }
            Some(doc) => {
                doc.refcount += 1;
                if doc.content_hash == hash {
                    // Identical content: only the refcount moved.
                    None
                } else {
                    doc.version += 1;
                    doc.content_hash = hash;
                    Some(DocSync::Change {
                        version: doc.version,
                        text: text.to_owned(),
                    })
                }
            }
        }
    }

    /// Release a hold on `uri`. Returns [`DocSync::Close`] when the last
    /// reference drops; `None` otherwise (including for an unknown uri).
    pub fn release(&mut self, uri: &str) -> Option<DocSync> {
        let doc = self.docs.get_mut(uri)?;
        doc.refcount -= 1;
        if doc.refcount == 0 {
            self.docs.remove(uri);
            Some(DocSync::Close)
        } else {
            None
        }
    }

    /// Push new content into an already-open document **without** changing its
    /// reference count (used for watcher Modify and `check` re-sync). Returns
    /// [`DocSync::Change`] when the content differs, `None` when it is unchanged
    /// or the uri is not open.
    pub fn sync(&mut self, uri: &str, text: &str) -> Option<DocSync> {
        let doc = self.docs.get_mut(uri)?;
        let hash = hash_text(text);
        if doc.content_hash == hash {
            return None;
        }
        doc.version += 1;
        doc.content_hash = hash;
        Some(DocSync::Change {
            version: doc.version,
            text: text.to_owned(),
        })
    }

    /// Whether `uri` currently has at least one open reference.
    #[must_use]
    pub fn is_open(&self, uri: &str) -> bool {
        self.docs.contains_key(uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "file:///w/src/a.rs";

    #[test]
    fn first_acquire_emits_open_at_version_1() {
        let mut tracker = DocumentTracker::new();
        let sync = tracker.acquire(URI, "rust", "fn main() {}");
        assert_eq!(
            sync,
            Some(DocSync::Open {
                version: 1,
                language_id: "rust".to_owned(),
                text: "fn main() {}".to_owned(),
            })
        );
    }

    #[test]
    fn second_acquire_same_text_emits_nothing() {
        let mut tracker = DocumentTracker::new();
        tracker.acquire(URI, "rust", "fn main() {}");
        let sync = tracker.acquire(URI, "rust", "fn main() {}");
        assert_eq!(
            sync, None,
            "re-acquire of identical content must not re-sync"
        );
    }

    #[test]
    fn acquire_changed_text_emits_change_with_incremented_version() {
        let mut tracker = DocumentTracker::new();
        tracker.acquire(URI, "rust", "fn main() {}");
        let sync = tracker.acquire(URI, "rust", "fn main() { let x = 1; }");
        assert_eq!(
            sync,
            Some(DocSync::Change {
                version: 2,
                text: "fn main() { let x = 1; }".to_owned(),
            })
        );
    }

    /// AC4: two concurrent holders → exactly one didOpen, exactly one didClose.
    #[test]
    fn exactly_one_open_and_one_close_across_two_holders() {
        let mut tracker = DocumentTracker::new();
        let open = tracker.acquire(URI, "rust", "fn main() {}");
        let second = tracker.acquire(URI, "rust", "fn main() {}");
        assert!(matches!(open, Some(DocSync::Open { .. })));
        assert_eq!(second, None);

        // First release: still one holder left — nothing sent.
        assert_eq!(tracker.release(URI), None);
        // Second release: last holder — didClose.
        assert_eq!(tracker.release(URI), Some(DocSync::Close));
    }

    #[test]
    fn release_unknown_uri_is_none() {
        let mut tracker = DocumentTracker::new();
        assert_eq!(tracker.release("file:///nope.rs"), None);
    }

    #[test]
    fn sync_on_unopened_document_is_none() {
        let mut tracker = DocumentTracker::new();
        assert_eq!(tracker.sync(URI, "anything"), None);
    }

    #[test]
    fn sync_changed_content_emits_change_and_bumps_version() {
        let mut tracker = DocumentTracker::new();
        tracker.acquire(URI, "rust", "v1");
        assert_eq!(
            tracker.sync(URI, "v2"),
            Some(DocSync::Change {
                version: 2,
                text: "v2".to_owned(),
            })
        );
    }

    #[test]
    fn sync_identical_content_is_none() {
        let mut tracker = DocumentTracker::new();
        tracker.acquire(URI, "rust", "same");
        assert_eq!(tracker.sync(URI, "same"), None);
    }

    #[test]
    fn sync_does_not_change_refcount() {
        let mut tracker = DocumentTracker::new();
        tracker.acquire(URI, "rust", "v1");
        tracker.sync(URI, "v2");
        // A single release still closes — sync did not add a reference.
        assert_eq!(tracker.release(URI), Some(DocSync::Close));
    }

    #[test]
    fn is_open_reflects_acquire_and_release() {
        let mut tracker = DocumentTracker::new();
        assert!(!tracker.is_open(URI));
        tracker.acquire(URI, "rust", "x");
        assert!(tracker.is_open(URI));
        tracker.release(URI);
        assert!(!tracker.is_open(URI));
    }

    #[test]
    fn reopen_after_full_close_emits_open_again() {
        let mut tracker = DocumentTracker::new();
        tracker.acquire(URI, "rust", "a");
        tracker.release(URI);
        // Fresh open-lifetime: a new didOpen (version resets to 1).
        let sync = tracker.acquire(URI, "rust", "a");
        assert!(matches!(sync, Some(DocSync::Open { version: 1, .. })));
    }
}
