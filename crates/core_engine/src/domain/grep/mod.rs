//! `TextSearch` — brute-force, CPU-parallel content search (spec 07).
//!
//! # Architecture
//!
//! ```text
//!  search_text(pattern, cap):
//!    file_ids = workspace.all_file_ids()      (read lock, snapshot — O(n))
//!         │ par_iter (Rayon work-stealing pool — U1)
//!         ▼
//!    for each FileId:
//!      path = workspace.get(file_id).path
//!      bytes = fs_port.read(path)             (through FileSystemPort — DI)
//!          ├── NotFound / ReadFailed  → skip  (UN1)
//!          └── non-UTF-8             → skip   (UN2, policy: skip binary)
//!      scan lines for literal substring match
//!          │ early-stop when AtomicUsize cap reached (OP1)
//!          ▼
//!      collect Match { path, line_number (1-based), line_content, file_id }
//!         │
//!         ▼
//!    flatten → sort by (path, line_number) → Vec<Match>   (bounded by cap / deterministic AC1)
//! ```
//!
//! # Design decisions
//!
//! **Pattern matching — literal substring (YAGNI)**
//!
//! The spec permits literal+regex via the `regex` crate but does not require it.
//! Literal `str::contains` is sufficient for spec 07 acceptance criteria and
//! avoids a ~3 MB transitive dependency. Regex support is deferred to the spec
//! that explicitly requires it.
//!
//! **Determinism — sort by `(path, line_number)` before returning**
//!
//! Rayon `par_iter` delivers results in non-deterministic order. We collect into
//! a `Vec<Match>` and then sort once. `Match` derives `Ord` with `path` first so
//! `Vec::sort` is stable across test runs regardless of core count or scheduling.
//!
//! **Cancellation / cap (OP1) — shared `AtomicUsize`**
//!
//! A `cap: Option<usize>` parameter bounds the result count. Each parallel
//! closure reserves result slots with an `AtomicUsize` counter. When the counter
//! reaches the cap the closure returns an empty `Vec`, which costs nothing and
//! terminates the work item without a panic.
//!
//! **Rayon inside domain**
//!
//! Rayon provides CPU parallelism with no I/O API surface. It is in the same
//! category as `std::collections` — an algorithmic tool, not infrastructure.
//! The hexagonal-boundary invariant forbids `sled`, `std::fs`, `wasmtime`, and
//! `notify`; `rayon` is not on that list. The domain depends only on the
//! `FileSystemPort` *trait*, never a concrete adapter.
//!
//! **Binary-file policy (UN2) — skip**
//!
//! `std::str::from_utf8` failure means the file is binary or uses a non-UTF-8
//! encoding. We skip such files silently: returning garbage matches from a lossy
//! decode is worse than returning no matches. The policy is documented here so
//! future callers know what to expect.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::domain::workspace::ProjectWorkspace;
use crate::domain::{DomainError, FileId};
use crate::ports::FileSystemPort;
use crate::ports::inbound::Match;

// ── TextSearch ────────────────────────────────────────────────────────────────

/// Brute-force parallel content search over all tracked files (spec 07).
///
/// # Examples
///
/// ```
/// use core_engine::domain::grep::TextSearch;
/// use core_engine::domain::workspace::ProjectWorkspace;
/// use core_engine::domain::virtual_file::FileMetadata;
/// use core_engine::domain::RelativePath;
/// use core_engine::adapters::InMemoryFs;
/// use core_engine::ports::FileSystemPort;
///
/// let mut workspace = ProjectWorkspace::new();
/// let mut fs = InMemoryFs::new();
///
/// let path = RelativePath::new("src/main.rs");
/// workspace.insert(path.clone(), FileMetadata::default()).unwrap();
/// fs.write(path, b"fn main() { println!(\"hello\"); }\n".to_vec()).unwrap();
///
/// let results = TextSearch::new(&workspace, &fs)
///     .search("hello", None)
///     .unwrap();
///
/// assert_eq!(results.len(), 1);
/// assert_eq!(results[0].line_number, 1);
/// ```
pub struct TextSearch<'a> {
    workspace: &'a ProjectWorkspace,
    // Decision: `dyn FileSystemPort + Sync` so the reference can be shared
    // across Rayon worker threads. `Sync` is satisfied by every concrete
    // adapter we have (InMemoryFs, RealFs) since none uses interior mutability
    // visible to readers. The search only *reads* through the port.
    fs: &'a (dyn FileSystemPort + Sync),
}

impl<'a> TextSearch<'a> {
    /// Create a `TextSearch` that reads workspace metadata from `workspace` and
    /// file contents from `fs`.
    ///
    /// # Sync bound
    ///
    /// `fs` must be `Sync` so the reference can be shared across Rayon worker
    /// threads. All provided adapters (`InMemoryFs`, `RealFs`) satisfy this
    /// bound. The search never mutates the file system.
    #[must_use]
    pub fn new(workspace: &'a ProjectWorkspace, fs: &'a (dyn FileSystemPort + Sync)) -> Self {
        Self { workspace, fs }
    }

    /// Search all tracked files for lines containing `pattern` (literal
    /// substring, case-sensitive).
    ///
    /// # Parameters
    ///
    /// - `pattern` — the literal string to search for.
    /// - `cap` — optional upper bound on the number of matches to collect.
    ///   When the cap is reached the search stops early and returns partial
    ///   results (OP1). Pass `None` for an unbounded search.
    ///
    /// # Errors
    ///
    /// Currently infallible — returns `Ok` always. The `Result` wrapper is
    /// preserved for `SearchUseCase` compatibility.
    ///
    /// # Notes
    ///
    /// - Unreadable files (deleted, permissions) are skipped — not fatal (UN1).
    /// - Binary / non-UTF-8 files are skipped without panicking (UN2).
    /// - Results are sorted by `(path, line_number)` for determinism (AC1).
    pub fn search(&self, pattern: &str, cap: Option<usize>) -> Result<Vec<Match>, DomainError> {
        let file_ids: Vec<FileId> = self.workspace.all_file_ids();

        // Shared match counter for the early-stop cap (OP1).
        // Decision: plain &AtomicUsize rather than Arc<AtomicUsize> — Rayon's
        // par_iter closures require Sync + Send but not 'static, so a borrow
        // bounded by this stack frame compiles directly. AtomicUsize is Sync,
        // so &AtomicUsize is Send + Sync. No heap allocation needed.
        let counter = AtomicUsize::new(0);
        let effective_cap = cap.unwrap_or(usize::MAX);

        let mut matches: Vec<Match> = file_ids
            .par_iter()
            .flat_map(|&file_id| {
                // Check cap before doing any work for this file.
                if counter.load(Ordering::Relaxed) >= effective_cap {
                    return vec![];
                }

                // Resolve the path (workspace is read-only here — no lock needed).
                let path = match self.workspace.get(file_id) {
                    Ok(vf) => vf.path.clone(),
                    // Stale handle — skip (UN1).
                    Err(_) => return vec![],
                };

                // Read file contents through the port (UN1: skip on any error).
                let bytes = match self.fs.read(&path) {
                    Ok(b) => b,
                    Err(_) => return vec![],
                };

                // Convert to UTF-8 — skip binary files (UN2, policy: skip).
                let text = match std::str::from_utf8(&bytes) {
                    Ok(t) => t,
                    Err(_) => return vec![],
                };

                // Scan lines for the pattern, collecting matches.
                let mut file_matches: Vec<Match> = Vec::new();
                for (zero_idx, line) in text.lines().enumerate() {
                    if counter.load(Ordering::Relaxed) >= effective_cap {
                        break;
                    }
                    if line.contains(pattern) {
                        if counter
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                                (current < effective_cap).then_some(current + 1)
                            })
                            .is_err()
                        {
                            break;
                        }
                        // Saturate at u32::MAX rather than panicking in a Rayon
                        // worker thread (UN2 / panic-free contract). A file with
                        // 4 billion lines is pathological; saturating is correct
                        // enough for any real input.
                        let line_number = (zero_idx as u64 + 1).min(u64::from(u32::MAX)) as u32;
                        file_matches.push(Match {
                            path: path.clone(),
                            line_number,
                            line_content: line.to_owned(),
                            file_id,
                        });
                    }
                }
                file_matches
            })
            .collect();

        // Sort by (path, line_number) so results are deterministic regardless
        // of Rayon scheduling order (AC1).
        matches.sort();
        Ok(matches)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::InMemoryFs;
    use crate::domain::RelativePath;
    use crate::domain::virtual_file::FileMetadata;
    use crate::ports::FileSystemPort;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn path(s: &str) -> RelativePath {
        RelativePath::new(s)
    }

    /// Insert a file into the workspace and write its content to the fs.
    fn add_file(ws: &mut ProjectWorkspace, fs: &mut InMemoryFs, p: &str, content: &[u8]) -> FileId {
        let rel = path(p);
        let id = ws.insert(rel.clone(), FileMetadata::default()).unwrap();
        fs.write(rel, content.to_vec()).unwrap();
        id
    }

    // ── TDD step 1/2: single-file match with line numbers ────────────────────

    /// AC1 — single file: correct line numbers and content returned.
    #[test]
    fn single_file_match_returns_correct_line_numbers() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        let id = add_file(
            &mut ws,
            &mut fs,
            "src/main.rs",
            b"fn main() {}\nlet x = needle;\nprintln!();\n",
        );

        let results = TextSearch::new(&ws, &fs).search("needle", None).unwrap();

        assert_eq!(results.len(), 1, "expected exactly one match");
        assert_eq!(results[0].file_id, id);
        assert_eq!(results[0].path, path("src/main.rs"));
        assert_eq!(results[0].line_number, 2, "needle is on line 2");
        assert_eq!(results[0].line_content, "let x = needle;");
    }

    /// Multiple matches in one file preserve all line numbers.
    #[test]
    fn multiple_matches_in_one_file_all_returned() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        add_file(
            &mut ws,
            &mut fs,
            "src/lib.rs",
            b"needle first\nsomething else\nneedle second\n",
        );

        let results = TextSearch::new(&ws, &fs).search("needle", None).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].line_number, 1);
        assert_eq!(results[0].line_content, "needle first");
        assert_eq!(results[1].line_number, 3);
        assert_eq!(results[1].line_content, "needle second");
    }

    /// Pattern not in file returns empty vec.
    #[test]
    fn no_match_returns_empty() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        add_file(&mut ws, &mut fs, "a.txt", b"hello world\n");

        let results = TextSearch::new(&ws, &fs).search("zzz", None).unwrap();
        assert!(results.is_empty());
    }

    // ── TDD step 3/4: multi-file match set ───────────────────────────────────

    /// AC1 — pattern in 3 files: exactly those matches returned with correct
    /// line numbers, sorted deterministically by (path, line_number).
    #[test]
    fn ac1_three_files_all_matches_with_correct_metadata() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();

        let id_a = add_file(&mut ws, &mut fs, "a.rs", b"no match\nfoo here\n");
        let id_b = add_file(&mut ws, &mut fs, "b.rs", b"foo bar\nnothing\n");
        let id_c = add_file(&mut ws, &mut fs, "c.rs", b"other\nfoo end\n");

        let results = TextSearch::new(&ws, &fs).search("foo", None).unwrap();

        // 3 matches total across 3 files.
        assert_eq!(results.len(), 3, "expected 3 matches; got {results:#?}");

        // Sorted by (path, line_number): a.rs:2, b.rs:1, c.rs:2
        assert_eq!(results[0].file_id, id_a);
        assert_eq!(results[0].path, path("a.rs"));
        assert_eq!(results[0].line_number, 2);
        assert_eq!(results[0].line_content, "foo here");

        assert_eq!(results[1].file_id, id_b);
        assert_eq!(results[1].path, path("b.rs"));
        assert_eq!(results[1].line_number, 1);
        assert_eq!(results[1].line_content, "foo bar");

        assert_eq!(results[2].file_id, id_c);
        assert_eq!(results[2].path, path("c.rs"));
        assert_eq!(results[2].line_number, 2);
        assert_eq!(results[2].line_content, "foo end");
    }

    /// Results are deterministic across repeated calls (AC1 / Rayon nondeterminism).
    #[test]
    fn results_are_deterministic_across_calls() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();

        for i in 0..20u32 {
            add_file(
                &mut ws,
                &mut fs,
                &format!("file_{i:02}.rs"),
                format!("line one\ntarget here {i}\nline three\n").as_bytes(),
            );
        }

        let searcher = TextSearch::new(&ws, &fs);
        let first = searcher.search("target", None).unwrap();
        let second = searcher.search("target", None).unwrap();

        assert_eq!(first, second, "search must be deterministic");
    }

    // ── TDD step 5/6: skip unreadable + binary safety ────────────────────────

    /// AC3 — file deleted between snapshot and read is skipped, search completes.
    #[test]
    fn ac3_unreadable_file_skipped_search_completes() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();

        // File A exists and has a match.
        let id_a = add_file(&mut ws, &mut fs, "a.rs", b"target line\n");

        // File B is inserted into the workspace but not the fs (simulates a file
        // that disappeared after the workspace snapshot).
        let id_b = ws
            .insert(path("missing.rs"), FileMetadata::default())
            .unwrap();
        // Confirm id_b is distinct.
        assert_ne!(id_a, id_b);

        let results = TextSearch::new(&ws, &fs).search("target", None).unwrap();

        // Only the readable file contributes matches.
        assert_eq!(results.len(), 1, "missing file must be skipped");
        assert_eq!(results[0].path, path("a.rs"));
    }

    /// AC4 — binary (non-UTF-8) file does not cause a panic.
    #[test]
    fn ac4_binary_file_does_not_unwind() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();

        // Inject raw bytes that are not valid UTF-8.
        let binary: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, b't', b'a', b'r', b'g', b'e', b't'];
        add_file(&mut ws, &mut fs, "binary.bin", &binary);
        // Also add a plain text file so we confirm that one is still found.
        add_file(&mut ws, &mut fs, "text.rs", b"target\n");

        let results = TextSearch::new(&ws, &fs).search("target", None).unwrap();

        // Binary file must be silently skipped (no panic, no result from it).
        assert_eq!(results.len(), 1, "only the text file should match");
        assert_eq!(results[0].path, path("text.rs"));
    }

    // ── TDD step 7/8: cancellation / result cap ───────────────────────────────

    /// AC5 — cap stops the search and returns partial results promptly.
    #[test]
    fn ac5_cap_limits_result_count() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();

        // 10 files, each with 3 matching lines → 30 total without a cap.
        for i in 0..10u32 {
            add_file(
                &mut ws,
                &mut fs,
                &format!("f{i:02}.rs"),
                b"match line\nmatch line\nmatch line\n",
            );
        }

        let cap = 5usize;
        // Uncapped total is 30 (10 files × 3 lines each).
        let uncapped_total = 30usize;
        let results = TextSearch::new(&ws, &fs)
            .search("match", Some(cap))
            .unwrap();

        // Must return at least one result.
        assert!(
            !results.is_empty(),
            "capped search must return at least one result"
        );

        // Must not return the full uncapped total — the cap must have had effect.
        assert!(
            results.len() < uncapped_total,
            "cap={cap} must stop before full set of {uncapped_total}; got {}",
            results.len()
        );

        // Tight upper bound: Relaxed atomics allow at most (lines_per_file - 1)
        // extra matches from the file whose read straddles the cap check, so
        // cap + max_lines_per_file = 5 + 3 = 8 is the sound ceiling.
        let tight_bound = cap + 3;
        assert!(
            results.len() <= tight_bound,
            "results {} must not exceed cap+overshoot={tight_bound}",
            results.len()
        );
    }

    /// Cap of 0 returns empty results.
    #[test]
    fn cap_of_zero_returns_empty() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        add_file(&mut ws, &mut fs, "a.rs", b"target\ntarget\n");

        let results = TextSearch::new(&ws, &fs).search("target", Some(0)).unwrap();
        assert!(results.is_empty(), "cap=0 must return empty results");
    }

    /// Empty workspace returns empty results without error.
    #[test]
    fn empty_workspace_returns_empty() {
        let ws = ProjectWorkspace::new();
        let fs = InMemoryFs::new();
        let results = TextSearch::new(&ws, &fs).search("anything", None).unwrap();
        assert!(results.is_empty());
    }

    /// Empty pattern matches every line.
    #[test]
    fn empty_pattern_matches_every_line() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        add_file(&mut ws, &mut fs, "a.rs", b"line one\nline two\n");

        let results = TextSearch::new(&ws, &fs).search("", None).unwrap();
        assert_eq!(results.len(), 2, "empty pattern must match all lines");
    }

    /// First line is reported with line_number = 1 (1-based spec U2).
    #[test]
    fn first_line_has_line_number_one() {
        let mut ws = ProjectWorkspace::new();
        let mut fs = InMemoryFs::new();
        add_file(&mut ws, &mut fs, "a.rs", b"target\n");

        let results = TextSearch::new(&ws, &fs).search("target", None).unwrap();
        assert_eq!(results[0].line_number, 1);
    }
}
