//! Guest-side signatures of host capabilities (U2).
//!
//! This module declares the `extern "C"` functions that the host (11c) exposes
//! to wasm guests. A plugin calls these to interact with the tower workspace.
//! **Nothing else is reachable from guest code** — the capability surface is
//! intentionally minimal and explicit (spec U2).
//!
//! # Safety
//!
//! Each function is `unsafe` because it crosses the wasm host ↔ guest boundary.
//! The invariants for each call are documented on the function.
//!
//! # Availability
//!
//! These symbols are only defined when the crate is compiled for `wasm32-wasip1`
//! (i.e. inside a wasm guest). On the host target they are declared as stubs so
//! the SDK still compiles for host-side tests; calling them on the host target
//! will panic.
//!
//! # Current capability surface (spec 11c)
//!
//! | Symbol | Description |
//! |--------|-------------|
//! | `host_log` | Write a log message to the host's diagnostic log. |
//! | `host_read_file` | Read a workspace-relative file through the host FileSystemPort. |
//!
//! # Wire protocol for `host_read_file`
//!
//! The guest calls `host_read_file(path_ptr, path_len, out_ptr, out_len_ptr)`:
//!
//! - `path_ptr`/`path_len`: UTF-8 workspace-relative path string (no NUL).
//! - `out_ptr`: pointer to an `*mut u8` (4-byte or 8-byte guest pointer) that
//!   the host will fill in with a guest-heap pointer to the file bytes. The host
//!   calls `__plugin_alloc(file_len)` to allocate this buffer.
//! - `out_len_ptr`: pointer to a `u32` the host will set to the file length.
//! - Returns `0` on success, `1` if the file was not found, `2` on I/O error.
//!
//! # Wire protocol for buffer exchange
//!
//! Buffers returned by the plugin to the host (`__plugin_init`, `__plugin_call_tool`)
//! are owned by the guest heap and freed by the host via `__plugin_free(ptr, len)`.
//! The `len` is the total buffer length: `BUFFER_HEADER_LEN + payload_len`
//! (see [`crate::__private::BUFFER_HEADER_LEN`]).

/// Minimum capability surface exposed by the host to a wasm guest.
///
/// All functions in this module are `extern "C"` and `unsafe`. Compile-time
/// stubs are provided for the host target so `cargo test` compiles.
pub mod ffi {
    // host_log(ptr, len): write a UTF-8 log message to the host diagnostic log.
    // ptr must be valid for len bytes of valid UTF-8. The host reads the bytes
    // synchronously; the guest retains ownership of the buffer.
    //
    // host_read_file(path_ptr, path_len, out_ptr, out_len_ptr) -> u32:
    //   Read a workspace-relative file. The host allocates a guest buffer via
    //   __plugin_alloc and writes the file content there. The guest receives
    //   the pointer and length through out_ptr/out_len_ptr.
    //   Returns: 0=ok, 1=not_found, 2=io_error.
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "tower_host")]
    extern "C" {
        pub fn host_log(ptr: *const u8, len: usize);
        pub fn host_read_file(
            path_ptr: *const u8,
            path_len: usize,
            out_ptr: *mut *mut u8,
            out_len_ptr: *mut u32,
        ) -> u32;
    }

    // ── Host-target stubs ────────────────────────────────────────────────────
    // These exist so `cargo test --workspace` (host target) compiles the SDK.
    // They panic because they cannot be called outside a wasm runtime.

    /// Host stub — panics on non-wasm targets.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn host_log(_ptr: *const u8, _len: usize) {
        panic!("host_log is only available inside a wasm32 guest");
    }

    /// Host stub — panics on non-wasm targets.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn host_read_file(
        _path_ptr: *const u8,
        _path_len: usize,
        _out_ptr: *mut *mut u8,
        _out_len_ptr: *mut u32,
    ) -> u32 {
        panic!("host_read_file is only available inside a wasm32 guest");
    }
}

/// Log a message via the host capability, with a safe Rust wrapper.
///
/// On non-wasm targets this is a no-op (test harness convenience).
///
/// # Examples
///
/// ```rust
/// // On wasm32 this calls host_log; on host it is a no-op for tests.
/// plugin_sdk::host::log("plugin initialised");
/// ```
pub fn log(message: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        // Safety: message is a valid UTF-8 &str; we pass pointer + len correctly.
        unsafe { ffi::host_log(message.as_ptr(), message.len()) }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // No-op in tests: the host's log is not available on the host target.
        let _ = message;
    }
}

/// Outcome of decoding the raw values returned by `host_read_file`.
///
/// This enum makes the three-way decision testable on the host target
/// without touching any `wasm32`-only unsafe pointer logic.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// `rc != 0`, or the contradictory case `rc == 0 && ptr_is_null && len > 0`.
    Failure,
    /// `rc == 0` and `len == 0`: the file exists but has no content.
    Empty,
    /// `rc == 0`, `ptr_is_null == false`, and `len > 0`: a buffer to reconstruct.
    Buffer,
}

/// Pure decision function for `host_read_file` return values.
///
/// Separating this logic from the unsafe pointer reconstruction makes it
/// testable on the host target without a wasm runtime.
///
/// # Decision table
///
/// | `rc` | `ptr_is_null` | `len` | Outcome         |
/// |------|---------------|-------|-----------------|
/// | 0    | true          | 0     | `Empty`         |
/// | 0    | false         | > 0   | `Buffer`        |
/// | 0    | false         | 0     | `Empty`         |
/// | != 0 | any           | any   | `Failure`       |
/// | 0    | true          | > 0   | `Failure` (contradictory) |
///
/// # Examples
///
/// ```rust
/// use plugin_sdk::host::{interpret_read, ReadOutcome};
///
/// assert_eq!(interpret_read(0, true, 0), ReadOutcome::Empty);
/// assert_eq!(interpret_read(0, false, 5), ReadOutcome::Buffer);
/// assert_eq!(interpret_read(1, true, 0), ReadOutcome::Failure);
/// assert_eq!(interpret_read(0, true, 5), ReadOutcome::Failure);
/// ```
pub fn interpret_read(rc: i32, ptr_is_null: bool, len: usize) -> ReadOutcome {
    if rc != 0 {
        return ReadOutcome::Failure;
    }
    // rc == 0 from here on.
    if len == 0 {
        // Empty file: null pointer is expected when len == 0.
        return ReadOutcome::Empty;
    }
    // len > 0: a null pointer with len > 0 is a contradictory/invalid response.
    if ptr_is_null {
        return ReadOutcome::Failure;
    }
    ReadOutcome::Buffer
}

/// Read a workspace-relative file through the host capability.
///
/// Returns the file bytes on success (`Some(Vec::new())` for an empty file),
/// or `None` if the file was not found or an I/O error occurred.
///
/// On non-wasm targets this always returns `None` (stub; not callable in tests).
///
/// # Examples
///
/// ```rust,ignore
/// // Inside a wasm32 plugin:
/// if let Some(bytes) = plugin_sdk::host::read_file("src/main.rs") {
///     let content = String::from_utf8_lossy(&bytes);
///     plugin_sdk::host::log(&format!("read {} bytes", bytes.len()));
/// }
/// ```
pub fn read_file(workspace_path: &str) -> Option<Vec<u8>> {
    #[cfg(target_arch = "wasm32")]
    {
        use std::ptr;

        let mut out_ptr: *mut u8 = ptr::null_mut();
        let mut out_len: u32 = 0;

        // Safety: workspace_path is a valid UTF-8 &str.
        let rc = unsafe {
            ffi::host_read_file(
                workspace_path.as_ptr(),
                workspace_path.len(),
                &raw mut out_ptr,
                &raw mut out_len,
            )
        };

        let len = out_len as usize;
        match interpret_read(rc as i32, out_ptr.is_null(), len) {
            ReadOutcome::Empty => Some(Vec::new()),
            ReadOutcome::Failure => None,
            ReadOutcome::Buffer => {
                // Safety: host allocated this buffer via __plugin_alloc(len);
                // interpret_read guarantees out_ptr is non-null and len > 0.
                // We reconstruct it as a Vec and take ownership; the host will not free it.
                Some(unsafe { Vec::from_raw_parts(out_ptr, len, len) })
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = workspace_path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{interpret_read, ReadOutcome};

    /// (rc=0, ptr_null=true, len=0) => Empty: zero-byte file, null pointer is valid.
    #[test]
    fn empty_file_rc0_null_ptr_len0_is_empty() {
        assert_eq!(interpret_read(0, true, 0), ReadOutcome::Empty);
    }

    /// (rc=0, ptr_null=false, len=5) => Buffer: normal non-empty file.
    #[test]
    fn normal_file_rc0_nonnull_ptr_len5_is_buffer() {
        assert_eq!(interpret_read(0, false, 5), ReadOutcome::Buffer);
    }

    /// (rc=0, ptr_null=false, len=0) => Empty: non-null but zero length, treat as empty.
    #[test]
    fn empty_file_rc0_nonnull_ptr_len0_is_empty() {
        assert_eq!(interpret_read(0, false, 0), ReadOutcome::Empty);
    }

    /// (rc=1, ...) => Failure: any non-zero rc is a failure.
    #[test]
    fn error_rc1_is_failure() {
        assert_eq!(interpret_read(1, true, 0), ReadOutcome::Failure);
        assert_eq!(interpret_read(1, false, 5), ReadOutcome::Failure);
        assert_eq!(interpret_read(2, true, 0), ReadOutcome::Failure);
    }

    /// (rc=0, ptr_null=true, len=5) => Failure: contradictory — null pointer with
    /// non-zero length is an invalid host response.
    #[test]
    fn contradictory_null_ptr_with_nonzero_len_is_failure() {
        assert_eq!(interpret_read(0, true, 5), ReadOutcome::Failure);
    }
}
