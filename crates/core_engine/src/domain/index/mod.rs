//! Inverted index — spec 03b.
//!
//! Maps each [`Token`] to the set of [`FileId`]s whose path contains that
//! token. Maintains compact posting lists through incremental insert/remove
//! operations. Pure domain: no I/O, no persistence, no unsafe code.
//!
//! # Architecture
//!
//! ```text
//!  InvertedIndex
//!  ┌──────────────────────────────────────────────────────┐
//!  │ map: HashMap<Token, HashSet<FileId>>                  │
//!  │ insert(file_id, &[Token])  ← EV1                      │
//!  │ remove(file_id)            ← EV2                      │
//!  │ query(&Token) -> impl Iterator<Item = FileId>  ← U1   │
//!  └──────────────────────────────────────────────────────┘
//!
//!  FileSearch (implements SearchUseCase::find_file)
//!  ┌──────────────────────────────────────────────────────┐
//!  │ index: InvertedIndex                                  │
//!  │ workspace: &ProjectWorkspace                          │
//!  │                                                       │
//!  │ find_file(query):                                     │
//!  │   tokens = tokenize(query)          (spec 03a)        │
//!  │   candidates = union of posting lists                 │
//!  │   resolve FileId → RelativePath via workspace         │
//!  │   rank: exact filename > prefix > substring           │
//!  │   return Vec<RelativePath>          (EV3)             │
//!  └──────────────────────────────────────────────────────┘
//! ```
//!
//! # Posting-list design decision
//!
//! **Decision**: `HashSet<FileId>` per token rather than a roaring bitmap.
//!
//! **Why**: `FileId` contains both `index` (u32) and `generation` (u32).
//! Roaring bitmaps key on a single u32; mapping `FileId` to a bare index would
//! require a separate generation cross-check on every resolve, and mapping to a
//! packed 64-bit key would inflate the u64 roaring bitmap API surface. Using
//! `HashSet<FileId>` stores the full generational handle directly: when a slot
//! is reused, the old `FileId` (generation N) and the new `FileId` (generation
//! N+1) are distinct keys, so remove/insert are naturally correct without any
//! cross-check. The workspace is still the authority for liveness; we simply
//! skip any `FileId` that `workspace.get()` rejects during resolve.
//!
//! **Trade-off**: slightly higher memory per entry vs. a bitmap (24 bytes per
//! `FileId` vs. ~2 bits amortised for roaring), acceptable for domain-layer
//! indexes over typical project sizes (<1M files).
//!
//! # Ranking decision
//!
//! Three tiers (lower = better rank):
//!
//! - **0 — exact filename**: the query (case-insensitive) equals the file's
//!   last path component.
//! - **1 — filename prefix**: the filename starts with the query (case-
//!   insensitive).
//! - **2 — substring / token match**: anything else that appears in the
//!   posting list.
//!
//! Within a tier, paths are sorted lexicographically for determinism (AC2).
#![forbid(unsafe_code)]

pub mod search;

use std::collections::{HashMap, HashSet};

use super::file_id::FileId;
use super::token::Token;

pub use search::FileSearch;

// ── InvertedIndex ─────────────────────────────────────────────────────────────

/// In-memory inverted index: maps each [`Token`] to the set of [`FileId`]s
/// whose path contains it (spec U1).
///
/// # Examples
///
/// ```
/// use core_engine::domain::token::{tokenize, Token};
/// use core_engine::domain::index::InvertedIndex;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::domain::virtual_file::FileMetadata;
/// use core_engine::domain::RelativePath;
///
/// let mut workspace = ProjectWorkspace::new();
/// let mut index = InvertedIndex::new();
///
/// let id = workspace
///     .insert(RelativePath::new("src/http/Client.rs"), FileMetadata::default())
///     .unwrap();
/// let tokens = tokenize("src/http/Client.rs");
/// index.insert(id, &tokens);
///
/// let count = index.query(&Token::new("client")).count();
/// assert_eq!(count, 1);
/// ```
#[derive(Debug, Default)]
pub struct InvertedIndex {
    /// Token → set of FileIds containing that token.
    ///
    /// Decision: HashMap outer, HashSet<FileId> inner.
    /// FileId includes generation, so a reused slot produces a distinct key;
    /// no aliasing possible between old and new inhabitants of the same slot.
    map: HashMap<Token, HashSet<FileId>>,
}

impl InvertedIndex {
    /// Create a new, empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `file_id` under every token in `tokens` (spec EV1).
    ///
    /// Idempotent per `(file_id, token)` pair.
    pub fn insert(&mut self, file_id: FileId, tokens: &[Token]) {
        for token in tokens {
            self.map.entry(token.clone()).or_default().insert(file_id);
        }
    }

    /// Remove `file_id` from every posting list that contains it (spec EV2).
    ///
    /// Empty posting lists are pruned to avoid unbounded memory growth.
    pub fn remove(&mut self, file_id: FileId) {
        self.map.retain(|_, ids| {
            ids.remove(&file_id);
            !ids.is_empty()
        });
    }

    /// Return an iterator over all [`FileId`]s associated with `token` (spec U1).
    ///
    /// Returns an empty iterator if no files are indexed under `token`.
    pub fn query(&self, token: &Token) -> impl Iterator<Item = FileId> + '_ + use<'_> {
        self.map
            .get(token)
            .into_iter()
            .flat_map(|ids| ids.iter().copied())
    }

    /// Return the total number of distinct tokens in the index.
    ///
    /// Intended for diagnostics and tests.
    #[must_use]
    pub fn token_count(&self) -> usize {
        self.map.len()
    }

    /// Return the number of file IDs currently indexed under `token`.
    ///
    /// Returns 0 if the token is absent.
    #[must_use]
    pub fn posting_count(&self, token: &Token) -> usize {
        self.map.get(token).map_or(0, HashSet::len)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::token::tokenize;

    fn fid(index: u32, r#gen: u32) -> FileId {
        FileId::new_for_testing(index, r#gen)
    }

    fn tok(s: &str) -> Token {
        Token::new(s)
    }

    // ── TDD step 1/2: insert + query ─────────────────────────────────────────

    #[test]
    fn insert_then_query_returns_file_id() {
        let mut idx = InvertedIndex::new();
        let id = fid(0, 0);
        idx.insert(id, &[tok("client")]);

        let hits: Vec<FileId> = idx.query(&tok("client")).collect();
        assert_eq!(hits, vec![id]);
    }

    #[test]
    fn query_absent_token_returns_empty() {
        let idx = InvertedIndex::new();
        let hits: Vec<FileId> = idx.query(&tok("client")).collect();
        assert!(hits.is_empty());
    }

    #[test]
    fn insert_multiple_files_under_same_token() {
        let mut idx = InvertedIndex::new();
        let a = fid(0, 0);
        let b = fid(1, 0);
        idx.insert(a, &[tok("client")]);
        idx.insert(b, &[tok("client")]);

        let mut hits: Vec<FileId> = idx.query(&tok("client")).collect();
        hits.sort_by_key(|f| f.index());
        assert_eq!(hits, vec![a, b]);
    }

    #[test]
    fn insert_is_idempotent_per_file_token_pair() {
        let mut idx = InvertedIndex::new();
        let id = fid(0, 0);
        idx.insert(id, &[tok("client")]);
        idx.insert(id, &[tok("client")]);

        assert_eq!(idx.posting_count(&tok("client")), 1);
    }

    #[test]
    fn insert_indexes_every_token_in_slice() {
        let mut idx = InvertedIndex::new();
        let id = fid(0, 0);
        let tokens = tokenize("src/http/Client.rs");
        idx.insert(id, &tokens);

        // tokenize produces "client", "http", "src", "rs" and prefixes
        assert!(idx.query(&tok("client")).next().is_some());
        assert!(idx.query(&tok("http")).next().is_some());
        assert!(idx.query(&tok("src")).next().is_some());
    }

    // ── TDD step 3/4: remove consistency ──────────────────────────────────────

    #[test]
    fn remove_drops_file_from_all_its_tokens() {
        let mut idx = InvertedIndex::new();
        let id = fid(0, 0);
        let tokens = tokenize("src/http/Client.rs");
        idx.insert(id, &tokens);
        idx.remove(id);

        assert!(idx.query(&tok("client")).next().is_none());
        assert!(idx.query(&tok("http")).next().is_none());
    }

    #[test]
    fn remove_leaves_other_files_intact() {
        let mut idx = InvertedIndex::new();
        let a = fid(0, 0);
        let b = fid(1, 0);
        idx.insert(a, &[tok("client")]);
        idx.insert(b, &[tok("client"), tok("db")]);
        idx.remove(a);

        let hits: Vec<FileId> = idx.query(&tok("client")).collect();
        assert_eq!(hits, vec![b]);
    }

    #[test]
    fn remove_prunes_empty_posting_list() {
        let mut idx = InvertedIndex::new();
        let id = fid(0, 0);
        idx.insert(id, &[tok("unique")]);
        assert_eq!(idx.token_count(), 1);

        idx.remove(id);
        assert_eq!(idx.token_count(), 0);
    }

    #[test]
    fn remove_on_absent_id_is_noop() {
        let mut idx = InvertedIndex::new();
        let a = fid(0, 0);
        let ghost = fid(99, 5);
        idx.insert(a, &[tok("client")]);
        idx.remove(ghost); // must not panic or corrupt

        assert_eq!(idx.posting_count(&tok("client")), 1);
    }

    /// AC3 — removed file absent from a token it held uniquely.
    #[test]
    fn ac3_removed_file_absent_from_unique_token() {
        let mut idx = InvertedIndex::new();
        let id = fid(0, 0);
        idx.insert(id, &[tok("uniquetoken")]);
        idx.remove(id);

        assert!(
            idx.query(&tok("uniquetoken")).next().is_none(),
            "removed file must not appear in any posting list"
        );
    }

    /// Generational correctness: after remove + reuse with bumped generation,
    /// the old FileId and the new FileId are distinct in posting lists.
    #[test]
    fn generation_bump_produces_distinct_file_ids() {
        let mut idx = InvertedIndex::new();
        let old_id = fid(0, 0);
        let new_id = fid(0, 1); // same slot, bumped generation

        idx.insert(old_id, &[tok("shared")]);
        idx.remove(old_id);
        idx.insert(new_id, &[tok("shared")]);

        let hits: Vec<FileId> = idx.query(&tok("shared")).collect();
        assert_eq!(hits, vec![new_id], "only new-generation id must be present");
    }

    // ── Property test: incremental == rebuild from scratch ───────────────────

    #[cfg(test)]
    mod prop_tests {
        use super::*;
        use proptest::prelude::*;

        /// Generate a list of (token_strings) per operation step.
        ///
        /// Each step gets a globally-unique `FileId` (index = step position,
        /// generation = 0). This avoids collisions where the same `FileId` is
        /// inserted and removed multiple times, which would be correct behaviour
        /// for the index but would make the "survivors" accounting ambiguous in
        /// this test harness.
        fn arb_ops() -> impl Strategy<Value = Vec<Vec<String>>> {
            prop::collection::vec(prop::collection::vec("[a-z]{2,6}", 1..5), 2..20)
        }

        proptest! {
            /// Property: an index built incrementally (insert then remove some)
            /// equals one built from scratch with only the surviving entries.
            ///
            /// Each operation step uses a *unique* `FileId` (step index as slot,
            /// generation 0) so there is no ambiguity about which inserts survive.
            #[test]
            fn prop_incremental_equals_rebuild(ops in arb_ops()) {
                // Build incrementally: insert all with unique ids, remove odd steps.
                let mut incremental = InvertedIndex::new();
                let mut survivors: Vec<(FileId, Vec<Token>)> = Vec::new();

                for (step, words) in ops.iter().enumerate() {
                    // Unique id per step: slot = step as u32, generation = 0.
                    let id = FileId::new_for_testing(step as u32, 0);
                    let tokens: Vec<Token> = words.iter().map(|w| Token::new(w.clone())).collect();
                    incremental.insert(id, &tokens);
                    if step % 2 == 0 {
                        survivors.push((id, tokens));
                    } else {
                        incremental.remove(id);
                    }
                }

                // Build from scratch with only the survivors.
                let mut scratch = InvertedIndex::new();
                for (id, tokens) in &survivors {
                    scratch.insert(*id, tokens);
                }

                // Collect every distinct token that appeared in any operation.
                let mut all_tokens: std::collections::BTreeSet<Token> =
                    std::collections::BTreeSet::new();
                for words in &ops {
                    for w in words {
                        all_tokens.insert(Token::new(w.clone()));
                    }
                }

                // Both indexes must yield the same sorted set of ids for every token.
                for token in &all_tokens {
                    let mut inc: Vec<FileId> = incremental.query(token).collect();
                    let mut scr: Vec<FileId> = scratch.query(token).collect();
                    inc.sort_by_key(|f| (f.index(), f.generation()));
                    scr.sort_by_key(|f| (f.index(), f.generation()));
                    prop_assert_eq!(
                        inc, scr,
                        "mismatch for token {:?}", token.as_str()
                    );
                }
            }
        }
    }
}
