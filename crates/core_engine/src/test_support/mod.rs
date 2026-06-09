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
    ($make:expr) => {
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
    ($make:expr) => {
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
