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

/// Read a workspace-relative file through the host capability.
///
/// Returns the file bytes on success, or `None` if the file was not found.
/// On I/O error the result is also `None` (conservative: treat errors as
/// not-found from the plugin's perspective to avoid leaking error detail).
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

        if rc != 0 || out_ptr.is_null() {
            return None;
        }

        let len = out_len as usize;
        // Safety: host allocated this buffer via __plugin_alloc(len); we
        // reconstruct it as a Vec and take ownership. The host will not free it.
        let bytes = unsafe { Vec::from_raw_parts(out_ptr, len, len) };
        Some(bytes)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = workspace_path;
        None
    }
}
