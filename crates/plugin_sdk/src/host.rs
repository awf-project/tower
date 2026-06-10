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
//! # Future capabilities
//!
//! Additional host functions will be added in later specs (e.g. 11c). Each
//! addition bumps [`crate::ABI_VERSION`]. Capabilities such as file access
//! require a well-defined guest alloc/dealloc protocol that is designed in 11c.
//!
//! # Current capability surface
//!
//! | Symbol | Description |
//! |--------|-------------|
//! | `host_log` | Write a log message to the host's diagnostic log. |
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
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "tower_host")]
    extern "C" {
        pub fn host_log(ptr: *const u8, len: usize);
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
