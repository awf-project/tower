//! `RealFs` — `std::fs`-backed [`FileSystemPort`] adapter (spec 05a).
//!
//! # Wireframe
//!
//! ```text
//!  RealFs : FileSystemPort
//!  ┌───────────────────────────────────┐
//!  │  root: PathBuf                    │
//!  │  known_paths: HashSet<RelPath>    │
//!  │  resolve(rel) -> PathBuf          │
//!  │  read(path)   -> Vec<u8>          │
//!  │  write(path)  -> ()               │
//!  │  rename/delete/scan               │
//!  └───────────────────────────────────┘
//! ```
//!
//! # Path traversal policy
//!
//! `RealFs` holds an absolute `root` directory and resolves every
//! [`RelativePath`] against it.  Any path whose normalised form would escape
//! the root (e.g. `../../etc/passwd` or `sub/../../../secret`) is rejected with
//! `PortError::ReadFailed` / `PortError::WriteFailed` containing the text
//! `"path traversal"`.  Absolute paths (e.g. `/etc/passwd` on POSIX) are
//! likewise rejected: `PathBuf::join` silently discards the base when the
//! argument is absolute, which would escape the sandbox entirely.
//! The check is performed by walking `std::path::Component` values and
//! tracking the nesting depth; no `canonicalize` syscall is needed
//! so the check is O(n) on the path length and never touches the filesystem.
//!
//! Rationale: normalize-and-clamp would silently swallow `../` components,
//! making the policy opaque.  Rejecting immediately is easier to audit and
//! keeps the invariant visible at the port boundary.
//!
//! # Durable write strategy
//!
//! [`RealFs::write`] uses a **temp-file + `sync_all` + atomic rename** pattern:
//!
//! 1. Create a named temp file in the *same* directory as the destination
//!    (same volume, guaranteeing the rename is a filesystem-level atomic
//!    operation on Linux/macOS/Windows).
//! 2. Write all bytes and call `File::sync_all()` to flush to durable storage
//!    before returning.
//! 3. `std::fs::rename` the temp file over the destination path (POSIX
//!    `rename(2)` is atomic and unconditionally overwrites an existing target).
//!
//! A crash between steps 2 and 3 leaves the orphaned temp file on disk but
//! the destination is untouched — there is no window of partial content.
//! This primitive is intentionally reusable by spec 08 (shadow-file writes).
//!
//! # `scan()` semantics (FileSystemPort)
//!
//! `scan()` returns only paths that were written or renamed *into* via this
//! adapter instance and have not since been deleted or renamed *away*.  It does
//! not do a recursive walk of the root directory (recursive ingestion is a
//! separate use case).  This keeps the contract test behaviour identical to
//! `InMemoryFs`.
//!
//! # Error mapping
//!
//! | `std::io::ErrorKind`   | `PortError` variant     |
//! |------------------------|-------------------------|
//! | `NotFound`             | `NotFound`              |
//! | other (read context)   | `ReadFailed(reason)`    |
//! | other (write context)  | `WriteFailed(reason)`   |

mod real_fs;

pub use real_fs::RealFs;

#[cfg(test)]
mod tests;
