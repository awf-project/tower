//! Shared durable-write primitive for the `fs` adapter layer (spec 05a / 13a).
//!
//! # Why this lives here
//!
//! Both [`super::RealFs`] and the format worker
//! (`adapters::formatter::worker`) need the same three-step sequence:
//!
//! 1. Create the temp file and write all bytes.
//! 2. `sync_all` — flush data **and** metadata to durable storage.
//! 3. `std::fs::rename` — OS-atomic overwrite of the destination.
//!
//! Keeping the logic in one place removes the risk of the two sites diverging
//! (e.g. one dropping the fsync, or using a different error-handling path).
//!
//! # Caller responsibilities
//!
//! * `tmp` must be on the **same filesystem volume** as `dest` so that
//!   `rename(2)` is an intra-volume atomic operation.  Both callers guarantee
//!   this by constructing `tmp` as a sibling of `dest` (same parent directory).
//! * `tmp` must not collide with another concurrent writer. Each caller uses a
//!   distinct suffix:
//!   - `RealFs::write` → `.~tmp` (not indexed by EventProcessor)
//!   - `FormatWorker` → `.tmp_write` (EventProcessor sentinel, never indexed)
//! * The caller is responsible for cleaning up `tmp` on error if needed;
//!   `durable_write` does **not** attempt to remove `tmp` on failure because the
//!   caller may want to inspect or report the partial file.

use std::io::{self, Write as _};
use std::path::Path;

/// Write `bytes` to `dest` durably via an atomic shadow-file pattern.
///
/// The sequence is:
/// 1. Create (or truncate) `tmp` and write all `bytes`.
/// 2. Call [`std::fs::File::sync_all`] to flush data and metadata to durable
///    storage — crash-safe before rename.
/// 3. [`std::fs::rename`] `tmp` → `dest` (POSIX `rename(2)` is atomic).
///
/// On failure at step 1 or 2, `tmp` may remain on disk as an orphan (the
/// caller decides whether to remove it).  On failure at step 3 the rename
/// did not happen; `dest` is untouched and `tmp` is the orphan.
///
/// # Errors
///
/// Returns the first `io::Error` encountered (create, write, sync, or rename).
pub(crate) fn durable_write(dest: &Path, tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    {
        let mut file = std::fs::File::create(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, dest)?;
    Ok(())
}
