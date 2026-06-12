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
//! # Current capability surface (spec 11c / stage 4a)
//!
//! | Symbol | Description |
//! |--------|-------------|
//! | `host_log` | Write a log message to the host's diagnostic log. |
//! | `host_read_file` | Read a workspace-relative file through the host FileSystemPort. |
//! | `ast_store_put` | Persist opaque bytes under an opaque key via AstIndexPort. |
//! | `ast_store_get` | Retrieve bytes previously stored under an opaque key. |
//! | `host_list_files` | Enumerate all workspace-relative file paths as a postcard `Vec<String>`. |
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
//! # Wire protocol for `ast_store_put` / `ast_store_get`
//!
//! The guest calls `ast_store_put(key_ptr, key_len, val_ptr, val_len)`:
//!
//! - `key_ptr`/`key_len`: UTF-8 opaque key string (no NUL, no `/`, no `..`).
//! - `val_ptr`/`val_len`: raw bytes to persist.
//! - Returns `0` on success, `1` if the key is invalid (`InvalidArgs`),
//!   `2` on a write error.
//!
//! The guest calls `ast_store_get(key_ptr, key_len, out_ptr, out_len_ptr)`:
//!
//! - `key_ptr`/`key_len`: UTF-8 opaque key string.
//! - `out_ptr`: pointer to a `*mut u8` the host fills with an allocated guest
//!   buffer (via `__plugin_alloc`) containing the stored bytes.
//! - `out_len_ptr`: pointer to a `u32` set to the buffer length.
//! - Returns `0` if the key is present (including a zero-byte value, which sets
//!   `out_len = 0` and leaves `out_ptr` null — `Some(vec![])` on the Rust side),
//!   `1` if the key is absent, `2` on error.
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
    //
    // ast_store_put(key_ptr, key_len, val_ptr, val_len) -> u32:
    //   Persist raw bytes under an opaque key via AstIndexPort.
    //   Returns: 0=ok, 1=InvalidArgs (bad key), 2=write error.
    //
    // ast_store_get(key_ptr, key_len, out_ptr, out_len_ptr) -> u32:
    //   Retrieve bytes stored under an opaque key. The host allocates a guest
    //   buffer via __plugin_alloc when len > 0.
    //   Returns: 0=present (out_len may be 0 for an empty value),
    //            1=absent, 2=error.
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "tower_host")]
    unsafe extern "C" {
        pub fn host_log(ptr: *const u8, len: usize);
        pub fn host_read_file(
            path_ptr: *const u8,
            path_len: usize,
            out_ptr: *mut *mut u8,
            out_len_ptr: *mut u32,
        ) -> u32;
        pub fn ast_store_put(
            key_ptr: *const u8,
            key_len: usize,
            val_ptr: *const u8,
            val_len: usize,
        ) -> u32;
        pub fn ast_store_get(
            key_ptr: *const u8,
            key_len: usize,
            out_ptr: *mut *mut u8,
            out_len_ptr: *mut u32,
        ) -> u32;
        // host_list_files(out_ptr, out_len_ptr) -> u32:
        //   Enumerate all workspace-relative file paths. The host serialises a
        //   Vec<String> with postcard, allocates a guest buffer via __plugin_alloc,
        //   and writes the payload there.
        //   Returns: 0=ok (out_len bytes of postcard payload), 2=error.
        //   rc=1 is intentionally unused.
        pub fn host_list_files(out_ptr: *mut *mut u8, out_len_ptr: *mut u32) -> u32;
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

    /// Host stub — panics on non-wasm targets.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn ast_store_put(
        _key_ptr: *const u8,
        _key_len: usize,
        _val_ptr: *const u8,
        _val_len: usize,
    ) -> u32 {
        panic!("ast_store_put is only available inside a wasm32 guest");
    }

    /// Host stub — panics on non-wasm targets.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn ast_store_get(
        _key_ptr: *const u8,
        _key_len: usize,
        _out_ptr: *mut *mut u8,
        _out_len_ptr: *mut u32,
    ) -> u32 {
        panic!("ast_store_get is only available inside a wasm32 guest");
    }

    /// Host stub — panics on non-wasm targets.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn host_list_files(_out_ptr: *mut *mut u8, _out_len_ptr: *mut u32) -> u32 {
        panic!("host_list_files is only available inside a wasm32 guest");
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

/// Persist raw bytes under an opaque key via the host AST index capability.
///
/// The `key` must be non-empty, must not contain `/`, and must not be `..` or
/// start with `../` — the same rules enforced by `AstIndexPort::put` on the
/// host side. Invalid keys are caught by the host and surfaced as `Err`.
///
/// Returns `Ok(())` on success, `Err(String)` on failure (invalid key or write
/// error). On non-wasm targets this always panics (stub).
///
/// # Examples
///
/// ```rust,ignore
/// // Inside a wasm32 plugin:
/// plugin_sdk::host::ast_store_put("symbols", &serialised_index)?;
/// ```
pub fn ast_store_put(key: &str, value: &[u8]) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    {
        // Safety: key and value are valid Rust slices; we pass ptr+len correctly.
        let rc =
            unsafe { ffi::ast_store_put(key.as_ptr(), key.len(), value.as_ptr(), value.len()) };
        match rc {
            0 => Ok(()),
            1 => Err(format!("ast_store_put: invalid key '{key}'")),
            _ => Err(format!("ast_store_put: write error for key '{key}'")),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (key, value);
        panic!("ast_store_put is only available inside a wasm32 guest");
    }
}

/// Retrieve bytes stored under an opaque key via the host AST index capability.
///
/// # Return convention
///
/// | Host rc | Meaning | Rust return |
/// |---------|---------|-------------|
/// | `0` | Key is **present**; `out_len` may be 0 for a stored empty value | `Some(bytes)` (may be `Some(vec![])`) |
/// | `1` | Key is **absent** | `None` |
/// | `2` | Error (bad key or I/O) | `None` |
///
/// `rc 0` with `out_len == 0` is disambiguated from absent (`rc 1`) — an
/// explicitly stored empty value returns `Some(vec![])`, not `None`.
///
/// On non-wasm targets this always panics (stub).
///
/// # Examples
///
/// ```rust,ignore
/// // Inside a wasm32 plugin:
/// match plugin_sdk::host::ast_store_get("symbols") {
///     Some(bytes) => { /* deserialise index */ }
///     None        => { /* cache miss — build the index */ }
/// }
/// ```
pub fn ast_store_get(key: &str) -> Option<Vec<u8>> {
    #[cfg(target_arch = "wasm32")]
    {
        use std::ptr;

        let mut out_ptr: *mut u8 = ptr::null_mut();
        let mut out_len: u32 = 0;

        // Safety: key is a valid UTF-8 &str; we pass ptr+len correctly.
        let rc = unsafe {
            ffi::ast_store_get(key.as_ptr(), key.len(), &raw mut out_ptr, &raw mut out_len)
        };

        match rc {
            0 => {
                let len = out_len as usize;
                if len == 0 {
                    // Key present but value is empty. out_ptr may be null (host
                    // skips alloc(0)); return Some(vec![]) to distinguish from
                    // absent (rc 1 → None).
                    Some(Vec::new())
                } else {
                    // Safety: host allocated this buffer via __plugin_alloc(len);
                    // rc==0 and len>0 guarantee out_ptr is non-null and valid.
                    // We reconstruct and take ownership; the host will not free it.
                    Some(unsafe { Vec::from_raw_parts(out_ptr, len, len) })
                }
            }
            _ => None, // rc 1 = absent, rc 2 = error — both map to None
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = key;
        panic!("ast_store_get is only available inside a wasm32 guest");
    }
}

/// Enumerate all workspace-relative file paths known to the host.
///
/// The host takes a read-lock snapshot of the `ProjectWorkspace`, collects
/// every live file's path, serialises the `Vec<String>` with postcard, and
/// writes the payload into a guest buffer allocated via `__plugin_alloc`.
///
/// # Return value
///
/// Returns a `Vec<String>` of workspace-relative path strings (e.g.
/// `"src/main.rs"`, `"Cargo.toml"`). Returns an empty `Vec` on error or
/// when the workspace contains no indexed files.
///
/// # RC convention
///
/// | Host rc | Meaning | Rust return |
/// |---------|---------|-------------|
/// | `0` | Success; payload is a valid postcard `Vec<String>` | deserialized paths |
/// | `2` | Error (serialisation or guest memory fault) | `vec![]` |
///
/// rc=1 is intentionally unused — there is no "absent" concept for a file list.
///
/// On non-wasm targets this always panics (stub — not callable outside a wasm runtime).
///
/// # Examples
///
/// ```rust,ignore
/// // Inside a wasm32 plugin:
/// let files = plugin_sdk::host::list_files();
/// for path in &files {
///     plugin_sdk::host::log(&format!("file: {path}"));
/// }
/// ```
pub fn list_files() -> Vec<String> {
    #[cfg(target_arch = "wasm32")]
    {
        use std::ptr;

        let mut out_ptr: *mut u8 = ptr::null_mut();
        let mut out_len: u32 = 0;

        // Safety: we pass valid mutable references; the host writes the pointer
        // and length of the allocated guest buffer through them.
        let rc = unsafe { ffi::host_list_files(&raw mut out_ptr, &raw mut out_len) };

        if rc != 0 {
            return Vec::new();
        }

        let len = out_len as usize;
        if len == 0 {
            // Host allocated nothing — workspace is empty. postcard encodes
            // Vec::<String>::new() as a single zero byte, so len==0 here
            // would only happen if the host skipped the alloc for an empty
            // workspace.  Return empty vec as documented.
            return Vec::new();
        }

        // Safety: host allocated this buffer via __plugin_alloc(len);
        // rc==0 and len>0 guarantee out_ptr is non-null and valid.
        // We reconstruct it as a Vec<u8>, deserialise, and take ownership.
        let payload = unsafe { Vec::from_raw_parts(out_ptr, len, len) };
        postcard::from_bytes::<Vec<String>>(&payload).unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        panic!("host_list_files is only available inside a wasm32 guest");
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadOutcome, interpret_read};

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
