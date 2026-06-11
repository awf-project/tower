//! `FileSearch` — concrete `SearchUseCase` implementation (spec 03b / EV3).
//!
//! Combines an [`InvertedIndex`] with a borrow of [`ProjectWorkspace`] to
//! implement [`SearchUseCase::find_file`] entirely in memory (spec U2 / AC4).
//! No I/O, no persistence.
//!
//! `search_text` is delegated to [`crate::domain::grep::TextSearch`] when a
//! [`FileSystemPort`] is supplied via [`FileSearch::with_fs`] (spec 07). When
//! no `fs` is provided (as in the spec-03b tests) `search_text` returns an
//! empty vec — the stub behaviour from the original placeholder.
//!
//! # Ranking
//!
//! Three tiers (lower ordinal = higher priority):
//!
//! | Tier | Condition |
//! |------|-----------|
//! | 0 | the file's last path component equals `query` (case-insensitive) |
//! | 1 | the file's last path component starts with `query` (case-insensitive) |
//! | 2 | any other token hit (substring / prefix token match) |
//!
//! Within each tier paths are sorted lexicographically for determinism (AC2).
//!
//! # Decision: union vs. intersection of posting lists
//!
//! A multi-token query unions all posting lists: candidates appear if they
//! match *any* token. This maximises recall for file-name search where the
//! user rarely types a complete tokenised path. An intersection strategy
//! would require the file to contain every token simultaneously — too strict
//! for typical fuzzy filename queries (e.g. `"Client.rs"` → tokens `["cli",
//! "clie", "clien", "client", "rs"]`; most short files would not contain `rs`
//! as a separate token).
#![forbid(unsafe_code)]

use std::collections::HashSet;

use crate::domain::grep::TextSearch;
use crate::domain::token::tokenize;
use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, FileId, RelativePath};
use crate::ports::FileSystemPort;
use crate::ports::inbound::{Match, SearchUseCase};

use super::InvertedIndex;

// ── FileSearch ────────────────────────────────────────────────────────────────

/// In-memory file search backed by an [`InvertedIndex`] and a
/// [`ProjectWorkspace`] borrow (spec 03b).
///
/// Optionally holds a [`FileSystemPort`] reference for `search_text` (spec 07).
/// When no `fs` is supplied, `search_text` returns empty (stub behaviour).
///
/// # Examples
///
/// ```
/// use core_engine::domain::index::{InvertedIndex, FileSearch};
/// use core_engine::domain::token::tokenize;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::domain::virtual_file::FileMetadata;
/// use core_engine::domain::{RelativePath};
/// use core_engine::ports::inbound::SearchUseCase;
///
/// let mut workspace = ProjectWorkspace::new();
/// let mut index = InvertedIndex::new();
///
/// let path = RelativePath::new("src/http/Client.rs");
/// let id = workspace.insert(path.clone(), FileMetadata::default()).unwrap();
/// index.insert(id, &tokenize("src/http/Client.rs"));
///
/// let search = FileSearch::new(&index, &workspace);
/// let results = search.find_file("client").unwrap();
/// assert_eq!(results, vec![path]);
/// ```
pub struct FileSearch<'ws> {
    index: &'ws InvertedIndex,
    workspace: &'ws ProjectWorkspace,
    /// Optional fs port for `search_text` delegation (spec 07).
    ///
    /// `Sync` bound required so the reference can be shared across Rayon
    /// worker threads inside `TextSearch`.
    fs: Option<&'ws (dyn FileSystemPort + Sync)>,
}

impl<'ws> FileSearch<'ws> {
    /// Create a `FileSearch` for `find_file` only (spec 03b).
    ///
    /// `search_text` returns empty until an `fs` is attached via
    /// [`FileSearch::with_fs`].
    #[must_use]
    pub fn new(index: &'ws InvertedIndex, workspace: &'ws ProjectWorkspace) -> Self {
        Self {
            index,
            workspace,
            fs: None,
        }
    }

    /// Attach a [`FileSystemPort`] so that `search_text` is fully implemented
    /// via [`TextSearch`] (spec 07).
    ///
    /// # Examples
    ///
    /// ```
    /// use core_engine::domain::index::{InvertedIndex, FileSearch};
    /// use core_engine::domain::workspace::ProjectWorkspace;
    /// use core_engine::domain::virtual_file::FileMetadata;
    /// use core_engine::domain::RelativePath;
    /// use core_engine::adapters::InMemoryFs;
    /// use core_engine::ports::{FileSystemPort, inbound::SearchUseCase};
    ///
    /// let mut workspace = ProjectWorkspace::new();
    /// let index = InvertedIndex::new();
    /// let mut fs = InMemoryFs::new();
    ///
    /// let path = RelativePath::new("src/lib.rs");
    /// workspace.insert(path.clone(), FileMetadata::default()).unwrap();
    /// fs.write(path, b"fn lib() {}\n".to_vec()).unwrap();
    ///
    /// let search = FileSearch::new(&index, &workspace).with_fs(&fs);
    /// let results = search.search_text("lib").unwrap();
    /// assert_eq!(results.len(), 1);
    /// ```
    #[must_use]
    pub fn with_fs(mut self, fs: &'ws (dyn FileSystemPort + Sync)) -> Self {
        self.fs = Some(fs);
        self
    }

    /// Resolve a set of candidate `FileId`s to ranked `RelativePath` results.
    ///
    /// Candidates that the workspace no longer recognises (stale handles) are
    /// silently skipped — the index may lag behind workspace removes that have
    /// not yet been propagated.
    fn resolve_and_rank(&self, candidates: HashSet<FileId>, query: &str) -> Vec<RelativePath> {
        // Pre-compute both case-sensitive and case-insensitive forms of the
        // query for tier classification.
        let query_lower = query.to_lowercase();

        // Four tiers (lower ordinal = higher priority):
        //   0 — exact case-sensitive filename match  (e.g. "Client.rs" == "Client.rs")
        //   1 — exact case-insensitive filename match (e.g. "client.rs" == "client.rs")
        //   2 — filename starts with query (case-insensitive prefix)
        //   3 — any other token hit (substring / prefix token)
        //
        // Within each tier paths are sorted lexicographically for determinism (AC2).
        let mut exact_cs: Vec<RelativePath> = Vec::new();
        let mut exact_ci: Vec<RelativePath> = Vec::new();
        let mut prefix_ci: Vec<RelativePath> = Vec::new();
        let mut other: Vec<RelativePath> = Vec::new();

        for id in candidates {
            // Silently skip stale handles (spec U2 / EV2 lag tolerance).
            let Ok(file) = self.workspace.get(id) else {
                continue;
            };
            let path = file.path.clone();
            let filename = last_component(path.as_str());
            let filename_lower = filename.to_lowercase();

            if filename == query {
                // Exact case-sensitive match: highest priority.
                exact_cs.push(path);
            } else if filename_lower == query_lower {
                // Exact case-insensitive match.
                exact_ci.push(path);
            } else if filename_lower.starts_with(&query_lower) {
                // Filename prefix match (case-insensitive).
                prefix_ci.push(path);
            } else {
                // General token hit.
                other.push(path);
            }
        }

        exact_cs.sort();
        exact_ci.sort();
        prefix_ci.sort();
        other.sort();

        exact_cs.extend(exact_ci);
        exact_cs.extend(prefix_ci);
        exact_cs.extend(other);
        exact_cs
    }
}

impl SearchUseCase for FileSearch<'_> {
    /// Find files matching `query`, ranked by relevance (spec EV3 / AC1/AC2).
    ///
    /// Returns `Ok(Vec::new())` for empty or whitespace queries (spec UN1 / AC5).
    ///
    /// # Errors
    ///
    /// Currently infallible for well-formed queries — returns `Ok` always.
    /// The `Result` wrapper is kept for API compatibility with `SearchUseCase`.
    fn find_file(&self, query: &str) -> Result<Vec<RelativePath>, DomainError> {
        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        // Union all posting lists across all query tokens.
        let mut candidates: HashSet<FileId> = HashSet::new();
        for token in &tokens {
            candidates.extend(self.index.query(token));
        }

        Ok(self.resolve_and_rank(candidates, query.trim()))
    }

    /// Search all tracked files for lines containing `pattern` (spec 07 EV1).
    ///
    /// Delegates to [`TextSearch`] when a [`FileSystemPort`] was supplied via
    /// [`FileSearch::with_fs`]. Returns empty when no `fs` is available (the
    /// stub behaviour preserved for spec-03b tests that do not supply an `fs`).
    fn search_text(&self, pattern: &str) -> Result<Vec<Match>, DomainError> {
        match self.fs {
            Some(fs) => TextSearch::new(self.workspace, fs).search(pattern, None),
            None => Ok(Vec::new()),
        }
    }

    /// Search with an early-stop cap (spec 07 OP1).
    ///
    /// Overrides the default trait implementation to pass the cap directly to
    /// [`TextSearch`], stopping parallel workers as soon as `cap` matches are
    /// collected rather than buffering the full result set first.
    fn search_text_capped(&self, pattern: &str, cap: usize) -> Result<Vec<Match>, DomainError> {
        match self.fs {
            Some(fs) => TextSearch::new(self.workspace, fs).search(pattern, Some(cap)),
            None => Ok(Vec::new()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the last path component (filename) from a slash-separated path.
///
/// Returns the full string if no slash is present.
fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ── PartialOrd / Ord for RelativePath (needed for sort) ───────────────────────
//
// RelativePath derives PartialEq + Eq + Hash but not Ord. We implement it here
// via the inner string so `.sort()` works without pulling in extra types.

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::InMemoryFs;
    use crate::domain::token::tokenize;
    use crate::domain::virtual_file::FileMetadata;
    use crate::domain::workspace::ProjectWorkspace;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn path(s: &str) -> RelativePath {
        RelativePath::new(s)
    }

    /// Insert a file into both workspace and index; return the assigned FileId.
    fn add_file(ws: &mut ProjectWorkspace, idx: &mut InvertedIndex, p: &str) -> FileId {
        let rel = path(p);
        let id = ws.insert(rel.clone(), FileMetadata::default()).unwrap();
        idx.insert(id, &tokenize(p));
        id
    }

    fn make_search<'a>(idx: &'a InvertedIndex, ws: &'a ProjectWorkspace) -> FileSearch<'a> {
        FileSearch::new(idx, ws)
    }

    // ── TDD step 5/6: find_file basics ────────────────────────────────────────

    /// AC1 — both client files returned for query "client".
    #[test]
    fn ac1_both_client_files_returned() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        add_file(&mut ws, &mut idx, "src/http/Client.rs");
        add_file(&mut ws, &mut idx, "src/db/client.rs");

        let results = make_search(&idx, &ws).find_file("client").unwrap();
        let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();

        assert!(
            paths.contains(&"src/http/Client.rs"),
            "expected src/http/Client.rs; got {paths:?}"
        );
        assert!(
            paths.contains(&"src/db/client.rs"),
            "expected src/db/client.rs; got {paths:?}"
        );
    }

    /// AC2 — exact-filename match ranks first.
    #[test]
    fn ac2_exact_filename_ranks_first() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        add_file(&mut ws, &mut idx, "src/http/Client.rs");
        add_file(&mut ws, &mut idx, "src/db/client.rs");

        let results = make_search(&idx, &ws).find_file("Client.rs").unwrap();
        assert!(
            !results.is_empty(),
            "expected at least one result for 'Client.rs'"
        );
        assert_eq!(
            results[0].as_str(),
            "src/http/Client.rs",
            "exact-filename match must rank first; got {results:?}"
        );
    }

    /// AC3 — removed file is absent from results for a token it held uniquely.
    #[test]
    fn ac3_removed_file_absent() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        let id = add_file(&mut ws, &mut idx, "src/http/Client.rs");
        add_file(&mut ws, &mut idx, "src/db/client.rs");

        // Remove from workspace AND index.
        ws.remove(id).unwrap();
        idx.remove(id);

        let results = make_search(&idx, &ws).find_file("http").unwrap();
        let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();
        assert!(
            !paths.contains(&"src/http/Client.rs"),
            "removed file must not appear; got {paths:?}"
        );
    }

    /// AC5 — whitespace-only query returns empty, no error.
    #[test]
    fn ac5_whitespace_query_returns_empty() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        add_file(&mut ws, &mut idx, "src/main.rs");

        let result = make_search(&idx, &ws).find_file("  ");
        assert!(result.is_ok(), "whitespace query must not error");
        assert!(
            result.unwrap().is_empty(),
            "whitespace query must return empty vec"
        );
    }

    /// UN1 — empty query returns empty, no error.
    #[test]
    fn empty_query_returns_empty() {
        let ws = ProjectWorkspace::new();
        let idx = InvertedIndex::new();
        let result = make_search(&idx, &ws).find_file("");
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    /// Stale handles in the index (workspace removed but index not cleaned up)
    /// are silently skipped.
    #[test]
    fn stale_handle_in_index_is_silently_skipped() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        let id = add_file(&mut ws, &mut idx, "src/http/Client.rs");
        // Remove from workspace only (simulating a lag between workspace and index).
        ws.remove(id).unwrap();
        // Index still contains the stale FileId.

        let result = make_search(&idx, &ws).find_file("client");
        assert!(result.is_ok());
        // Stale entry must not appear in results.
        assert!(
            result.unwrap().is_empty(),
            "stale handle must be silently skipped"
        );
    }

    // ── TDD step 7/8: ranking determinism ────────────────────────────────────

    /// Ranking is deterministic: same workspace + query produces same order.
    #[test]
    fn ranking_is_deterministic() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        add_file(&mut ws, &mut idx, "src/http/Client.rs");
        add_file(&mut ws, &mut idx, "src/db/client.rs");
        add_file(&mut ws, &mut idx, "src/client_utils.rs");

        let search = make_search(&idx, &ws);
        let first = search.find_file("client").unwrap();
        let second = search.find_file("client").unwrap();
        assert_eq!(first, second, "find_file must be deterministic");
    }

    /// Within the same tier paths are sorted lexicographically.
    #[test]
    fn within_tier_paths_sorted_lexicographically() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        // All are "other" tier (token hit, not exact/prefix filename match for "rs")
        // Actually "rs" is a full token; filenames ending in ".rs" have "rs" component.
        // Insert in reverse alphabetical order to confirm sorting.
        add_file(&mut ws, &mut idx, "src/z_module.rs");
        add_file(&mut ws, &mut idx, "src/a_module.rs");
        add_file(&mut ws, &mut idx, "src/m_module.rs");

        let results = make_search(&idx, &ws).find_file("module").unwrap();
        let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();

        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(
            paths, sorted,
            "results within each tier must be lexicographically sorted"
        );
    }

    /// Prefix match ranks above plain token match.
    #[test]
    fn prefix_tier_ranks_above_other_tier() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();
        // "client_utils.rs" — filename starts with "client" → prefix tier
        // "http_client.rs"  — filename contains "client" but doesn't start with it → other tier
        add_file(&mut ws, &mut idx, "src/http_client.rs");
        add_file(&mut ws, &mut idx, "src/client_utils.rs");

        let results = make_search(&idx, &ws).find_file("client").unwrap();
        let paths: Vec<&str> = results.iter().map(|p| p.as_str()).collect();

        let pos_prefix = paths
            .iter()
            .position(|p| *p == "src/client_utils.rs")
            .expect("client_utils.rs must be present");
        let pos_other = paths
            .iter()
            .position(|p| *p == "src/http_client.rs")
            .expect("http_client.rs must be present");

        assert!(
            pos_prefix < pos_other,
            "prefix-filename match must rank above other; got {paths:?}"
        );
    }

    /// AC4 — 10k-file workspace; find_file completes correctly (sub-ms, informational).
    #[test]
    fn ac4_ten_thousand_files_resolve_correctly() {
        let mut ws = ProjectWorkspace::new();
        let mut idx = InvertedIndex::new();

        for i in 0..10_000u32 {
            let p = format!("src/module_{i}/file_{i}.rs");
            let rel = RelativePath::new(p.clone());
            let id = ws.insert(rel, FileMetadata::default()).unwrap();
            idx.insert(id, &tokenize(&p));
        }

        let start = std::time::Instant::now();
        let results = FileSearch::new(&idx, &ws).find_file("file_42").unwrap();
        let elapsed = start.elapsed();

        // Correctness: the specific file must appear.
        let paths: Vec<&str> = results.iter().map(|r| r.as_str()).collect();
        assert!(
            paths.contains(&"src/module_42/file_42.rs"),
            "must find file_42; got {paths:?}"
        );

        // Informational: sub-millisecond on any reasonable CI machine.
        assert!(
            elapsed.as_millis() < 100,
            "find_file over 10k files took {}ms (expected < 100ms)",
            elapsed.as_millis()
        );
    }

    // ── TDD step 9/10: search_text_capped (OP1 reachable from trait) ─────────

    /// OP1 — search_text_capped stops early: result count is bounded by cap,
    /// not the full uncapped total.
    ///
    /// Fixture: 10 files × 3 matching lines = 30 total without a cap.
    /// With cap=5 and Relaxed atomics the overshoot is at most (lines_per_file - 1)
    /// extra matches from the file that straddles the cap boundary, so the tight
    /// upper bound is cap + max_lines_per_file = 5 + 3 = 8.
    #[test]
    fn op1_search_text_capped_stops_before_full_set() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        let idx = InvertedIndex::new();

        for i in 0..10u32 {
            let p = format!("f{i:02}.rs");
            let rel = RelativePath::new(p.clone());
            let id = ws.insert(rel.clone(), FileMetadata::default()).unwrap();
            let _ = id; // index not needed for grep
            fs.write(rel, b"match line\nmatch line\nmatch line\n".to_vec())
                .unwrap();
        }

        let cap = 5usize;
        let results = FileSearch::new(&idx, &ws)
            .with_fs(&fs)
            .search_text_capped("match", cap)
            .unwrap();

        // Must return at least one result.
        assert!(
            !results.is_empty(),
            "capped search must return at least one result"
        );

        // Must not return the full uncapped total — cap must have had effect.
        assert!(
            results.len() < 30,
            "cap={cap} must stop before full set; got {}",
            results.len()
        );

        // Tight upper bound: cap + max_lines_per_file (Relaxed overshoot at most 1 file).
        let tight_bound = cap + 3;
        assert!(
            results.len() <= tight_bound,
            "results {} must not exceed cap+overshoot={tight_bound}",
            results.len()
        );
    }

    // ── Property test: deterministic ranking ─────────────────────────────────

    #[cfg(test)]
    mod prop_tests {
        use super::*;
        use crate::domain::token::tokenize;
        use proptest::prelude::*;

        proptest! {
            /// Two identical calls to find_file on the same index+workspace must
            /// return exactly the same ordered result.
            #[test]
            fn prop_find_file_is_deterministic(
                query in "[a-z]{3,8}",
                paths in prop::collection::vec("[a-z]{3,6}/[a-z]{3,8}\\.rs", 1..20),
            ) {
                let mut ws = ProjectWorkspace::new();
                let mut idx = InvertedIndex::new();

                for p in &paths {
                    // Deduplicate paths before inserting.
                    if ws.get_by_path(&RelativePath::new(p)).is_none() {
                        let rel = RelativePath::new(p.clone());
                        let id = ws.insert(rel, FileMetadata::default()).unwrap();
                        idx.insert(id, &tokenize(p));
                    }
                }

                let search = FileSearch::new(&idx, &ws);
                let first = search.find_file(&query).unwrap();
                let second = search.find_file(&query).unwrap();
                prop_assert_eq!(first, second);
            }
        }
    }
}
