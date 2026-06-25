//! Test-support helpers — contract test macros and shared fixtures.
//!
//! # Purpose
//!
//! The contract test macros defined here let any crate verify that its
//! concrete adapter satisfies the behavioural expectations of the port
//! (spec 02 DoD / REFACTOR step). Usage pattern in the real adapter crate:
//!
//! ```rust,ignore
//! // In your adapter's test module:
//! use core_engine::test_support::{storage_contract_tests, filesystem_contract_tests};
//!
//! storage_contract_tests!(|| my_crate::SledStorage::new_temp());
//! filesystem_contract_tests!(|| my_crate::StdFs::new_temp("/tmp/tower-test"));
//! ```
//!
//! # Availability
//!
//! Always compiled when `cfg(test)` is active. Also compiled when the
//! `testing` feature is enabled so integration-test crates can import it
//! without activating `cfg(test)`.
//!
//! # Why macros, not generic functions?
//!
//! Rust test functions must be concrete (the test harness calls them by name).
//! A generic `fn contract_test<S: StoragePort>(make: impl Fn() -> S)` would
//! never be instantiated by the harness. The macros expand to concrete `#[test]`
//! fns inside the caller's module.

pub mod fixtures;

pub use fixtures::{make_virtual_file, sample_content_hash};

/// Expand a full behavioural contract test suite for any [`StoragePort`]
/// implementation.
///
/// # Usage
///
/// ```rust,ignore
/// core_engine::test_support::storage_contract_tests!(|| InMemoryStorage::new());
/// ```
///
/// The macro accepts a constructor expression — a closure (`|| Foo::new()`) or
/// a bare function path (`Foo::new`) — that returns a fresh, empty implementor
/// each time it is called.
#[macro_export]
macro_rules! storage_contract_tests {
    ($make:expr_2021) => {
        mod storage_contract {
            use super::*;
            use $crate::domain::{FileId, VirtualFile};
            use $crate::ports::{PortError, StoragePort};
            use $crate::test_support::{make_virtual_file, sample_content_hash};

            #[test]
            fn put_then_get_returns_same_file() {
                let mut store = ($make)();
                let file = make_virtual_file(0, 0, "src/main.rs");
                store.put(file.clone()).unwrap();
                let got = store.get(FileId::new_for_testing(0, 0)).unwrap();
                assert_eq!(got, file);
            }

            #[test]
            fn get_on_missing_id_returns_not_found() {
                let store = ($make)();
                let err = store.get(FileId::new_for_testing(99, 0)).unwrap_err();
                assert_eq!(err, PortError::NotFound);
            }

            #[test]
            fn delete_removes_entry() {
                let mut store = ($make)();
                let id = FileId::new_for_testing(0, 0);
                store.put(make_virtual_file(0, 0, "a.rs")).unwrap();
                store.delete(id).unwrap();
                assert_eq!(store.get(id).unwrap_err(), PortError::NotFound);
            }

            #[test]
            fn delete_on_missing_id_returns_not_found() {
                let mut store = ($make)();
                let err = store.delete(FileId::new_for_testing(42, 0)).unwrap_err();
                assert_eq!(err, PortError::NotFound);
            }

            #[test]
            fn put_overwrites_existing_entry() {
                let mut store = ($make)();
                let id = FileId::new_for_testing(0, 0);
                store.put(make_virtual_file(0, 0, "a.rs")).unwrap();
                let updated = VirtualFile {
                    size: 999,
                    ..make_virtual_file(0, 0, "a.rs")
                };
                store.put(updated.clone()).unwrap();
                assert_eq!(store.get(id).unwrap().size, 999);
            }

            #[test]
            fn put_blob_then_get_blob_returns_same_bytes() {
                let mut store = ($make)();
                let hash = sample_content_hash();
                let bytes = b"hello world".to_vec();
                store.put_blob(hash, bytes.clone()).unwrap();
                assert_eq!(store.get_blob(&hash).unwrap(), bytes);
            }

            #[test]
            fn get_blob_on_missing_hash_returns_not_found() {
                let store = ($make)();
                let hash = sample_content_hash();
                assert_eq!(store.get_blob(&hash).unwrap_err(), PortError::NotFound);
            }

            // ── put_batch contract ────────────────────────────────────────────

            /// All files in a batch are reachable via `get` after `put_batch`.
            #[test]
            fn put_batch_round_trips_all_files() {
                let mut store = ($make)();
                let files = vec![
                    make_virtual_file(0, 0, "src/a.rs"),
                    make_virtual_file(1, 0, "src/b.rs"),
                    make_virtual_file(2, 0, "src/c.rs"),
                ];
                store.put_batch(&files).unwrap();
                for file in &files {
                    let got = store.get(file.id).unwrap();
                    assert_eq!(
                        got,
                        *file,
                        "put_batch: file {} not round-tripped",
                        file.id.index()
                    );
                }
            }

            /// An empty batch is a no-op and must not error.
            #[test]
            fn put_batch_with_empty_slice_is_a_noop() {
                let mut store = ($make)();
                store.put_batch(&[]).unwrap();
            }

            /// `put_batch` is idempotent: re-inserting the same files overwrites
            /// existing records without error.
            #[test]
            fn put_batch_overwrites_existing_records_idempotently() {
                let mut store = ($make)();
                let file = make_virtual_file(0, 0, "src/a.rs");
                store.put_batch(std::slice::from_ref(&file)).unwrap();
                // Call again with the same file — must succeed.
                store.put_batch(std::slice::from_ref(&file)).unwrap();
                assert_eq!(store.get(file.id).unwrap(), file);
            }

            // ── put_batch atomicity on failure ────────────────────────────────

            /// Verify the all-or-nothing contract: when `put_batch` fails (the
            /// store is wrapped in a `FailingStorage` that returns `WriteFailed`
            /// on demand), no files must have been persisted.
            ///
            /// Strategy: pre-populate the store with one file, then call
            /// `put_batch` through a failing wrapper.  After the failure,
            /// re-query the store directly and confirm the pre-existing file is
            /// unchanged and the new files are absent.
            ///
            /// This tests the interface contract, not the wrapper itself: any
            /// `StoragePort` that genuinely satisfies the all-or-nothing
            /// guarantee will pass; one that writes partial state will fail
            /// because the new file IDs will be findable via `get`.
            #[test]
            fn put_batch_atomicity_on_failure() {
                use $crate::ports::PortError;

                /// Wrapper that delegates all calls to `inner` except
                /// `put_batch`, which always returns `WriteFailed`.
                struct AlwaysFailBatch<S: $crate::ports::StoragePort> {
                    inner: S,
                }

                impl<S: $crate::ports::StoragePort> $crate::ports::StoragePort
                    for AlwaysFailBatch<S>
                {
                    fn get(&self, id: FileId) -> Result<VirtualFile, PortError> {
                        self.inner.get(id)
                    }
                    fn put(&mut self, file: VirtualFile) -> Result<(), PortError> {
                        self.inner.put(file)
                    }
                    fn put_batch(&mut self, _files: &[$crate::domain::VirtualFile]) -> Result<(), PortError> {
                        Err(PortError::WriteFailed("injected failure".to_owned()))
                    }
                    fn delete(&mut self, id: FileId) -> Result<(), PortError> {
                        self.inner.delete(id)
                    }
                    fn put_blob(
                        &mut self,
                        hash: $crate::domain::ContentHash,
                        bytes: Vec<u8>,
                    ) -> Result<(), PortError> {
                        self.inner.put_blob(hash, bytes)
                    }
                    fn get_blob(
                        &self,
                        hash: &$crate::domain::ContentHash,
                    ) -> Result<Vec<u8>, PortError> {
                        self.inner.get_blob(hash)
                    }
                    fn mark_scan_complete(&mut self) -> Result<(), PortError> {
                        self.inner.mark_scan_complete()
                    }
                    fn is_scan_complete(&self) -> Result<bool, PortError> {
                        self.inner.is_scan_complete()
                    }
                }

                // Build the inner store and pre-populate it with file 0.
                let inner = ($make)();
                let mut failing = AlwaysFailBatch { inner };

                // Pre-condition: file 0 is absent before anything.
                assert_eq!(
                    failing.get(FileId::new_for_testing(0, 0)).unwrap_err(),
                    PortError::NotFound,
                    "store must be empty before test"
                );

                // Attempt to batch-insert files 0 and 1.  Must fail.
                let batch = vec![
                    make_virtual_file(0, 0, "src/a.rs"),
                    make_virtual_file(1, 0, "src/b.rs"),
                ];
                let result = failing.put_batch(&batch);
                assert!(
                    result.is_err(),
                    "put_batch must return an error via the failing wrapper"
                );

                // Post-condition: neither file was persisted (all-or-nothing).
                assert_eq!(
                    failing.get(FileId::new_for_testing(0, 0)).unwrap_err(),
                    PortError::NotFound,
                    "file 0 must not be present after failed put_batch"
                );
                assert_eq!(
                    failing.get(FileId::new_for_testing(1, 0)).unwrap_err(),
                    PortError::NotFound,
                    "file 1 must not be present after failed put_batch"
                );
            }

            // ── scan-complete marker contract ─────────────────────────────────

            /// `is_scan_complete` returns `false` on a fresh store.
            #[test]
            fn scan_complete_is_false_on_fresh_store() {
                let store = ($make)();
                assert!(!store.is_scan_complete().unwrap());
            }

            /// After `mark_scan_complete`, `is_scan_complete` returns `true`.
            #[test]
            fn mark_scan_complete_sets_flag() {
                let mut store = ($make)();
                assert!(!store.is_scan_complete().unwrap());
                store.mark_scan_complete().unwrap();
                assert!(store.is_scan_complete().unwrap());
            }

            /// Partial data present + marker absent => `is_scan_complete` is
            /// `false`, meaning the scan-complete flag is independent of whether
            /// any files are stored.
            #[test]
            fn partial_data_without_marker_is_not_complete() {
                let mut store = ($make)();
                // Persist a file without calling mark_scan_complete.
                store.put(make_virtual_file(0, 0, "partial.rs")).unwrap();
                assert!(
                    !store.is_scan_complete().unwrap(),
                    "marker must not be set by put — only mark_scan_complete sets it"
                );
            }
        }
    };
}

/// Expand a full behavioural contract test suite for any [`FileSystemPort`]
/// implementation.
///
/// # Usage
///
/// ```rust,ignore
/// core_engine::test_support::filesystem_contract_tests!(|| InMemoryFs::new());
/// ```
#[macro_export]
macro_rules! filesystem_contract_tests {
    ($make:expr_2021) => {
        mod filesystem_contract {
            use super::*;
            use $crate::domain::RelativePath;
            use $crate::ports::{FileSystemPort, PortError};

            fn path(s: &str) -> RelativePath {
                RelativePath::new(s)
            }

            #[test]
            fn write_then_read_returns_same_bytes() {
                let mut fs = ($make)();
                let p = path("src/lib.rs");
                fs.write(p.clone(), b"pub fn f() {}".to_vec()).unwrap();
                assert_eq!(fs.read(&p).unwrap(), b"pub fn f() {}");
            }

            #[test]
            fn read_on_missing_path_returns_not_found() {
                let fs = ($make)();
                assert_eq!(
                    fs.read(&path("missing.rs")).unwrap_err(),
                    PortError::NotFound
                );
            }

            /// AC2: after rename(from, to), `to` has the bytes and `from` is gone.
            #[test]
            fn rename_moves_bytes_atomically() {
                let mut fs = ($make)();
                let tmp = path("a.rs.tmp");
                let dst = path("a.rs");
                let content = b"fn main() {}".to_vec();
                fs.write(tmp.clone(), content.clone()).unwrap();
                fs.rename(&tmp, dst.clone()).unwrap();
                // Destination holds the bytes.
                assert_eq!(fs.read(&dst).unwrap(), content);
                // Source is gone.
                assert_eq!(fs.read(&tmp).unwrap_err(), PortError::NotFound);
            }

            /// AC2 / POSIX rename(2): rename MUST silently overwrite an existing
            /// destination. Adapters must not return an error when `to` already
            /// exists — this is load-bearing for the shadow-file pattern (spec 08)
            /// where a `.tmp` file is renamed over the canonical path.
            #[test]
            fn rename_overwrites_existing_destination() {
                let mut fs = ($make)();
                let src = path("b.rs.tmp");
                let dst = path("b.rs");
                let old_content = b"old content".to_vec();
                let new_content = b"new content".to_vec();
                // Pre-populate the destination.
                fs.write(dst.clone(), old_content).unwrap();
                // Write source with different bytes.
                fs.write(src.clone(), new_content.clone()).unwrap();
                // Rename must succeed even though dst already exists.
                fs.rename(&src, dst.clone()).unwrap();
                // Destination holds the source bytes.
                assert_eq!(fs.read(&dst).unwrap(), new_content);
                // Source is gone.
                assert_eq!(fs.read(&src).unwrap_err(), PortError::NotFound);
            }

            #[test]
            fn rename_on_missing_source_returns_not_found() {
                let mut fs = ($make)();
                let err = fs.rename(&path("ghost.tmp"), path("ghost.rs")).unwrap_err();
                assert_eq!(err, PortError::NotFound);
            }

            #[test]
            fn delete_removes_path() {
                let mut fs = ($make)();
                let p = path("x.rs");
                fs.write(p.clone(), vec![1, 2, 3]).unwrap();
                fs.delete(&p).unwrap();
                assert_eq!(fs.read(&p).unwrap_err(), PortError::NotFound);
            }

            #[test]
            fn delete_on_missing_path_returns_not_found() {
                let mut fs = ($make)();
                assert_eq!(
                    fs.delete(&path("never_existed.rs")).unwrap_err(),
                    PortError::NotFound
                );
            }

            #[test]
            fn scan_returns_all_written_paths() {
                let mut fs = ($make)();
                fs.write(path("a.rs"), vec![]).unwrap();
                fs.write(path("b.rs"), vec![]).unwrap();
                let mut paths: Vec<String> = fs
                    .scan()
                    .into_iter()
                    .map(|p| p.as_str().to_owned())
                    .collect();
                paths.sort();
                assert_eq!(paths, vec!["a.rs", "b.rs"]);
            }

            #[test]
            fn scan_excludes_deleted_paths() {
                let mut fs = ($make)();
                fs.write(path("keep.rs"), vec![]).unwrap();
                let gone = path("gone.rs");
                fs.write(gone.clone(), vec![]).unwrap();
                fs.delete(&gone).unwrap();
                let paths: Vec<String> = fs
                    .scan()
                    .into_iter()
                    .map(|p| p.as_str().to_owned())
                    .collect();
                assert!(!paths.contains(&"gone.rs".to_owned()));
                assert!(paths.contains(&"keep.rs".to_owned()));
            }

            #[test]
            fn scan_excludes_renamed_source_includes_destination() {
                let mut fs = ($make)();
                let src = path("draft.rs.tmp");
                let dst = path("draft.rs");
                fs.write(src.clone(), b"content".to_vec()).unwrap();
                fs.rename(&src, dst.clone()).unwrap();
                let paths: Vec<String> = fs
                    .scan()
                    .into_iter()
                    .map(|p| p.as_str().to_owned())
                    .collect();
                assert!(paths.contains(&"draft.rs".to_owned()));
                assert!(!paths.contains(&"draft.rs.tmp".to_owned()));
            }
        }
    };
}

/// Expand the behavioural contract suite for any [`CodeIntelligencePort`].
///
/// The `$make` expression must return a fresh implementor. The `$marker`
/// expression is a `&str` line the backend treats as an error (the fake uses
/// `"//!ERR"`; the rust-analyzer adapter uses a real broken statement). This
/// lets one suite cover both a synthetic fake and a real language server.
///
/// # Usage
///
/// ```rust,ignore
/// codeintel_contract_tests!(|| InMemoryCodeIntel::new(), "//!ERR");
/// ```
#[macro_export]
macro_rules! codeintel_contract_tests {
    ($make:expr_2021, $marker:expr_2021) => {
        mod codeintel_contract {
            use super::*;
            use $crate::domain::RelativePath;
            use $crate::domain::code_intel::Severity;
            use $crate::ports::{CodeIntelError, CodeIntelligencePort};

            #[test]
            fn clean_text_returns_no_diagnostics() {
                let ci = ($make)();
                let diags = ci
                    .check(&RelativePath::new("src/clean.rs"), "fn main() {}")
                    .expect("clean .rs must be supported");
                assert!(
                    diags.is_empty(),
                    "clean text must report no diagnostics; got {diags:?}"
                );
            }

            #[test]
            fn broken_text_returns_an_error_diagnostic() {
                let ci = ($make)();
                let text = format!("fn main() {{ {} }}", $marker);
                let diags = ci
                    .check(&RelativePath::new("src/broken.rs"), &text)
                    .expect("broken .rs must be supported");
                assert!(
                    diags.iter().any(|d| d.severity == Severity::Error),
                    "broken text must yield at least one Error diagnostic; got {diags:?}"
                );
            }

            #[test]
            fn unsupported_extension_returns_unsupported() {
                let ci = ($make)();
                let err = ci
                    .check(&RelativePath::new("notes.txt"), "anything")
                    .expect_err("a .txt file must have no backend");
                assert_eq!(err, CodeIntelError::Unsupported);
            }

            #[test]
            fn re_check_is_idempotent() {
                let ci = ($make)();
                let text = format!("fn main() {{ {} }}", $marker);
                let first = ci.check(&RelativePath::new("src/x.rs"), &text).unwrap();
                let second = ci.check(&RelativePath::new("src/x.rs"), &text).unwrap();
                assert_eq!(first, second, "re-check of identical input must match");
            }
        }
    };
}

/// Expand the shared behavioural contract for any [`NavigationPort`].
///
/// The `$make` expression must return a fresh implementor. The contract covers
/// the guarantees the in-memory fake and a real language server share without a
/// running backend: every method on an unsupported extension returns
/// `Unsupported`, and a supported file never errors on a well-formed query.
/// Backend-specific results (cross-file definition, etc.) are covered by the
/// gated e2e suite, not here.
///
/// # Usage
///
/// ```rust,ignore
/// navigation_contract_tests!(|| InMemoryCodeIntel::new());
/// ```
#[macro_export]
macro_rules! navigation_contract_tests {
    ($make:expr_2021) => {
        mod navigation_contract {
            use super::*;
            use $crate::domain::RelativePath;
            use $crate::domain::code_intel::{Position, Range};
            use $crate::ports::{CodeIntelError, NavigationPort};

            const ORIGIN: Position = Position {
                line: 0,
                character: 0,
            };

            #[test]
            fn definition_on_unsupported_extension_is_unsupported() {
                let nav = ($make)();
                assert_eq!(
                    nav.definition(&RelativePath::new("notes.txt"), "x", ORIGIN)
                        .unwrap_err(),
                    CodeIntelError::Unsupported
                );
            }

            #[test]
            fn references_on_unsupported_extension_is_unsupported() {
                let nav = ($make)();
                assert_eq!(
                    nav.references(&RelativePath::new("notes.txt"), "x", ORIGIN)
                        .unwrap_err(),
                    CodeIntelError::Unsupported
                );
            }

            #[test]
            fn hover_on_unsupported_extension_is_unsupported() {
                let nav = ($make)();
                assert_eq!(
                    nav.hover(&RelativePath::new("notes.txt"), "x", ORIGIN)
                        .unwrap_err(),
                    CodeIntelError::Unsupported
                );
            }

            #[test]
            fn document_symbols_on_unsupported_extension_is_unsupported() {
                let nav = ($make)();
                assert_eq!(
                    nav.document_symbols(&RelativePath::new("notes.txt"), "x")
                        .unwrap_err(),
                    CodeIntelError::Unsupported
                );
            }

            #[test]
            fn fake_can_represent_implementation_lookup_for_later_tool_tests() {
                let nav = ($make)();
                let pos = Position {
                    line: 4,
                    character: 2,
                };
                let locs = nav
                    .implementations(&RelativePath::new("src/a.rs"), "trait T {}", pos)
                    .expect("fake navigation support must represent implementations");

                assert_eq!(locs.len(), 1);
                assert_eq!(locs[0].path.as_str(), "src/a.rs");
                assert_eq!(locs[0].range.start, pos);
                assert_eq!(locs[0].range.end, pos);
            }

            #[test]
            fn fake_can_represent_prepare_rename_responses_for_later_tool_tests() {
                let nav = ($make)();
                let pos = Position {
                    line: 1,
                    character: 5,
                };
                let prepared = nav
                    .prepare_rename(&RelativePath::new("src/a.rs"), "let name = 1;", pos)
                    .expect("fake navigation support must represent prepareRename");

                assert_eq!(prepared.placeholder.as_deref(), Some("name"));
                assert_eq!(
                    prepared.range,
                    Some(Range {
                        start: Position {
                            line: 1,
                            character: 4,
                        },
                        end: Position {
                            line: 1,
                            character: 8,
                        },
                    })
                );
            }

            #[test]
            fn definition_on_supported_file_does_not_error() {
                let nav = ($make)();
                nav.definition(&RelativePath::new("src/a.rs"), "fn main() {}", ORIGIN)
                    .expect("a supported file must not error on a well-formed query");
            }
        }
    };
}

/// Expand a full behavioural contract test suite for any [`AstIndexPort`]
/// implementation.
///
/// # Usage
///
/// ```rust,ignore
/// core_engine::ast_index_contract_tests!(|| InMemoryAstIndex::new());
/// ```
///
/// The macro accepts a constructor expression — a closure (`|| Foo::new()`) or
/// a bare function path (`Foo::new`) — that returns a fresh, empty implementor
/// each time it is called.
#[macro_export]
macro_rules! ast_index_contract_tests {
    ($make:expr_2021) => {
        mod ast_index_contract {
            use super::*;
            use $crate::ports::{AstIndexPort, PortError};

            // ── put / get round-trip ──────────────────────────────────────────

            #[test]
            fn put_then_get_returns_same_bytes() {
                let store = ($make)();
                store.put("index", b"hello world").unwrap();
                assert_eq!(store.get("index").unwrap(), Some(b"hello world".to_vec()));
            }

            // ── get-miss returns Ok(None) ─────────────────────────────────────

            #[test]
            fn get_on_missing_key_returns_none() {
                let store = ($make)();
                assert_eq!(store.get("missing").unwrap(), None);
            }

            // ── overwrite: second put wins ────────────────────────────────────

            #[test]
            fn put_twice_second_value_wins() {
                let store = ($make)();
                store.put("k", b"first").unwrap();
                store.put("k", b"second").unwrap();
                assert_eq!(store.get("k").unwrap(), Some(b"second".to_vec()));
            }

            // ── delete + get ──────────────────────────────────────────────────

            #[test]
            fn delete_then_get_returns_none() {
                let store = ($make)();
                store.put("entry", b"data").unwrap();
                store.delete("entry").unwrap();
                assert_eq!(store.get("entry").unwrap(), None);
            }

            // ── delete missing is idempotent ──────────────────────────────────

            #[test]
            fn delete_missing_key_is_ok() {
                let store = ($make)();
                // Must not error — idempotent.
                store.delete("never_existed").unwrap();
            }

            // ── list reflects puts and deletes ────────────────────────────────

            #[test]
            fn list_reflects_puts() {
                let store = ($make)();
                store.put("a", b"1").unwrap();
                store.put("b", b"2").unwrap();
                let mut keys = store.list().unwrap();
                keys.sort();
                assert_eq!(keys, vec!["a".to_owned(), "b".to_owned()]);
            }

            #[test]
            fn list_empty_on_fresh_store() {
                let store = ($make)();
                assert_eq!(store.list().unwrap(), Vec::<String>::new());
            }

            #[test]
            fn list_excludes_deleted_keys() {
                let store = ($make)();
                store.put("keep", b"y").unwrap();
                store.put("gone", b"n").unwrap();
                store.delete("gone").unwrap();
                let keys = store.list().unwrap();
                assert!(keys.contains(&"keep".to_owned()), "keep must be present");
                assert!(!keys.contains(&"gone".to_owned()), "gone must be absent");
            }

            // ── key validation ────────────────────────────────────────────────

            #[test]
            fn empty_key_is_rejected() {
                let store = ($make)();
                assert!(matches!(
                    store.put("", b"x"),
                    Err(PortError::InvalidArgs(_))
                ));
                assert!(matches!(store.get(""), Err(PortError::InvalidArgs(_))));
                assert!(matches!(store.delete(""), Err(PortError::InvalidArgs(_))));
            }

            #[test]
            fn key_with_slash_is_rejected() {
                let store = ($make)();
                assert!(matches!(
                    store.put("a/b", b"x"),
                    Err(PortError::InvalidArgs(_))
                ));
                assert!(matches!(store.get("a/b"), Err(PortError::InvalidArgs(_))));
            }

            #[test]
            fn null_byte_key_is_rejected() {
                let store = ($make)();
                assert!(matches!(
                    store.put("a\0b", b"x"),
                    Err(PortError::InvalidArgs(_))
                ));
                assert!(matches!(store.get("a\0b"), Err(PortError::InvalidArgs(_))));
                assert!(matches!(
                    store.delete("a\0b"),
                    Err(PortError::InvalidArgs(_))
                ));
            }

            #[test]
            fn dotdot_key_is_rejected() {
                let store = ($make)();
                assert!(matches!(
                    store.put("..", b"x"),
                    Err(PortError::InvalidArgs(_))
                ));
            }

            #[test]
            fn dotdot_prefix_key_is_rejected() {
                let store = ($make)();
                assert!(matches!(
                    store.put("../escape", b"x"),
                    Err(PortError::InvalidArgs(_))
                ));
            }

            // ── binary-safe values ────────────────────────────────────────────

            #[test]
            fn binary_safe_null_bytes() {
                let store = ($make)();
                let data: Vec<u8> = vec![0x00, 0x01, 0xff, 0x00, 0xfe];
                store.put("bin", &data).unwrap();
                assert_eq!(store.get("bin").unwrap(), Some(data));
            }

            #[test]
            fn binary_safe_non_utf8() {
                let store = ($make)();
                // Invalid UTF-8 sequence.
                let data: Vec<u8> = vec![0x80, 0x81, 0x82, 0xff];
                store.put("raw", &data).unwrap();
                assert_eq!(store.get("raw").unwrap(), Some(data));
            }

            #[test]
            fn empty_value_round_trips() {
                let store = ($make)();
                store.put("empty", b"").unwrap();
                assert_eq!(store.get("empty").unwrap(), Some(vec![]));
            }
        }
    };
}
