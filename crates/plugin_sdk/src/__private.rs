//! Internal glue called by the `#[plugin_main]`-generated code.
//!
//! This module is **not part of the public API**. Its types and functions may
//! change in any release. Downstream code MUST NOT import from `plugin_sdk::__private`.
//!
//! # Safety model
//!
//! The unsafe surface is deliberately small and documented here in one place:
//!
//! - [`vec_into_raw`]: leaks a `Vec<u8>` and returns the raw pointer. The
//!   caller (the host, via `__plugin_free`) is responsible for freeing it.
//! - [`free_buffer`]: reconstructs the `Vec` from a raw pointer + length and
//!   drops it. Must be called exactly once per `vec_into_raw` call.
//! - [`dispatch_call_tool`] and [`dispatch_on_hook`]: receive raw pointers from
//!   the host ABI; they are called from the `#[plugin_main]`-generated unsafe
//!   wrappers which already assert pointer validity.
//!
//! All other functions are safe.

use crate::types::{CallRequest, CallResponse, HookEnvelope, PluginManifest};
use crate::Plugin;

// ── Guest allocator export ────────────────────────────────────────────────────

/// Allocate `len` bytes in the guest heap and return a raw pointer.
///
/// This function is exported from the plugin binary as `__plugin_alloc` so the
/// host (11c) can write serialised arguments **into** the guest's linear memory
/// before calling `__plugin_call_tool` or `__plugin_on_hook`.
///
/// The returned buffer is initially zeroed; the host must write exactly
/// `len` bytes before passing the pointer to the guest export. Ownership is
/// transferred to the host: the host must pass the same `(ptr, len)` to the
/// relevant guest export (which consumes the slice by reference), and then the
/// guest export is responsible for freeing the argument buffer when done.
///
/// # Design: in-param allocation protocol
///
/// Because wasm linear memory is shared between host and guest (via the
/// `Memory` object), the host can write to any guest pointer directly after
/// receiving it. The protocol for passing serialised data IN to the guest is:
///
/// ```text
/// 1. host calls __plugin_alloc(len) -> ptr   // guest allocates
/// 2. host writes len bytes at ptr             // host fills buffer
/// 3. host calls __plugin_call_tool(ptr, len) // guest reads + frees
/// ```
///
/// The guest export (`__plugin_call_tool` / `__plugin_on_hook`) takes the
/// slice by reference and does NOT call `__plugin_free` on the argument pointer
/// because the argument buffer is a simple `Vec<u8>` that the guest owns and
/// can drop normally at the end of the call frame.
///
/// # Safety contract (for the generated wasm export)
///
/// - `len` must not be zero; the host must never pass a zero-length allocation.
/// - The caller must not free the returned pointer independently; it is
///   consumed by the guest export that receives it.
#[must_use]
pub fn alloc_guest_buffer(len: usize) -> *mut u8 {
    // Decision: allocate via Vec so the drop machinery is standard.
    // The generated __plugin_alloc export leaks this Vec and returns the pointer;
    // the vec is reconstructed and freed inside the receiving guest export.
    let mut v: Vec<u8> = vec![0u8; len];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr
}

/// Free a buffer previously returned by [`alloc_guest_buffer`].
///
/// # Safety
///
/// - `ptr` must have been returned by `alloc_guest_buffer(len)`.
/// - Must be called exactly once per `alloc_guest_buffer` call.
pub unsafe fn free_alloc_buffer(ptr: *mut u8, len: usize) {
    // Safety: ptr came from alloc_guest_buffer which used Vec::with_len,
    // so layout is consistent for Vec<u8> with capacity=len.
    let _ = unsafe { Vec::from_raw_parts(ptr, len, len) };
}

// ── Buffer helpers ────────────────────────────────────────────────────────────

/// Byte length of the length-prefix header prepended to every serialised buffer.
///
/// Every buffer returned by `__plugin_init` or `__plugin_call_tool` is laid out
/// as:
///
/// ```text
/// [ u32 payload_len (little-endian, 4 bytes) ][ payload_len bytes of postcard ]
/// ```
///
/// The total buffer length (the `len` that must be passed to `__plugin_free`) is
/// therefore `BUFFER_HEADER_LEN + payload_len`, where `payload_len` equals the
/// little-endian `u32` stored in the first 4 bytes.
///
/// A host implementer (spec 11c) that needs to call `__plugin_free` must compute:
///
/// ```text
/// let payload_len = u32::from_le_bytes(ptr[0..4]) as usize;
/// let total_len   = plugin_sdk::__private::BUFFER_HEADER_LEN + payload_len;
/// __plugin_free(ptr, total_len);
/// ```
pub const BUFFER_HEADER_LEN: usize = 4;

/// Encode a [`PluginManifest`] into a length-prefixed postcard buffer.
///
/// Layout: `[u32 little-endian payload length][payload bytes]`
///
/// The caller (the generated `__plugin_init`) leaks this buffer via
/// [`vec_into_raw`] and returns the pointer to the host. The host reads the
/// first 4 bytes to determine the payload length, then reads that many bytes
/// as the manifest payload. The host calls `__plugin_free` when done.
///
/// # Panics
///
/// Panics if postcard serialisation fails. This cannot happen for well-formed
/// `PluginManifest` values (no heap-allocated types that postcard cannot handle).
#[must_use]
pub fn encode_manifest(manifest: PluginManifest) -> Vec<u8> {
    let payload = postcard::to_allocvec(&manifest).expect("PluginManifest serialisation failed");
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&payload);
    buf
}

/// Leak a `Vec<u8>` as a boxed slice and return the raw pointer.
///
/// Converts `buf` to `Box<[u8]>` first (via [`Vec::into_boxed_slice`]), which
/// calls `shrink_to_fit` internally so `capacity == len` is guaranteed before
/// the pointer is leaked. This makes the alloc/dealloc layout unambiguous:
/// `free_buffer` can reconstruct the exact `Box<[u8]>` from `(ptr, len)` alone,
/// with no hidden capacity field.
///
/// The returned pointer is valid until the same `(ptr, len)` pair is passed to
/// [`free_buffer`]. This transfers ownership to the host.
///
/// # Safety contract
///
/// The caller must ensure that [`free_buffer`] is called exactly once with the
/// returned pointer and the length `buf.len()`.
#[must_use]
pub fn vec_into_raw(buf: Vec<u8>) -> *mut u8 {
    // Decision: use into_boxed_slice() + Box::into_raw() instead of ManuallyDrop.
    // Why: Vec::with_capacity(n) may allocate capacity > n; passing len as cap to
    //      Vec::from_raw_parts in free_buffer would be UB if the allocator gave a
    //      larger block. into_boxed_slice() calls shrink_to_fit first, making
    //      capacity == len before the pointer is leaked, so dealloc uses the
    //      correct Layout.
    // Trade-off: one potential realloc on shrink (negligible for plugin init payloads).
    Box::into_raw(buf.into_boxed_slice()) as *mut u8
}

/// Reconstruct and drop a buffer previously produced by [`vec_into_raw`].
///
/// # Safety
///
/// - `ptr` must have been returned by [`vec_into_raw`].
/// - `len` must be the exact `Vec::len()` at the time of the `vec_into_raw` call
///   (equivalently, the total buffer length including the 4-byte length prefix;
///   see [`BUFFER_HEADER_LEN`]).
/// - Must be called exactly once per [`vec_into_raw`] call.
///
/// # Panics
///
/// Does not panic. If the invariants above are violated the behaviour is
/// undefined (this is an `unsafe` function).
pub unsafe fn free_buffer(ptr: *mut u8, len: usize) {
    // Safety: caller guarantees ptr + len came from vec_into_raw, which used
    // into_boxed_slice() so capacity == len. Reconstructing as Box<[u8]> via a
    // raw slice pointer is the exact inverse of Box::into_raw(boxed_slice).
    let _ = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) };
}

// ── Tool call dispatch ────────────────────────────────────────────────────────

/// Deserialise a `CallRequest`, invoke `P::call_tool`, serialise the result.
///
/// Returns a length-prefixed postcard buffer (same layout as [`encode_manifest`]).
/// Called from the generated `__plugin_call_tool` unsafe wrapper.
///
/// If deserialisation fails (malformed bytes from the host), returns a
/// `CallResponse` carrying `SdkError::InvalidArgs`.
///
/// # Panics
///
/// Panics if serialising the `CallResponse` fails (cannot happen for
/// well-formed values).
#[must_use]
pub fn dispatch_call_tool<P: Plugin>(request_bytes: &[u8]) -> Vec<u8> {
    let response = match postcard::from_bytes::<CallRequest>(request_bytes) {
        Ok(req) => CallResponse {
            result: P::call_tool(&req.tool_name, req.args),
        },
        Err(e) => CallResponse {
            result: Err(crate::types::SdkError::InvalidArgs(e.to_string())),
        },
    };
    let payload = postcard::to_allocvec(&response).expect("CallResponse serialisation failed");
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&payload);
    buf
}

/// Deserialise a `HookEnvelope` and invoke `P::on_hook`.
///
/// Called from the generated `__plugin_on_hook` unsafe wrapper.
/// If deserialisation fails, the hook is silently dropped (the hook is
/// best-effort; the host should not fail for a malformed hook envelope).
pub fn dispatch_on_hook<P: Plugin>(hook_bytes: &[u8]) {
    if let Ok(envelope) = postcard::from_bytes::<HookEnvelope>(hook_bytes) {
        P::on_hook(envelope.kind, envelope.payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CallRequest, HookKind, HookPayload, SdkError, ToolDesc, Value};
    use crate::{Plugin, PluginManifest, ABI_VERSION};

    struct TestPlugin;

    impl Plugin for TestPlugin {
        fn init() -> PluginManifest {
            PluginManifest {
                name: "test".to_owned(),
                version: "0.1.0".to_owned(),
                abi: ABI_VERSION,
                tools: vec![ToolDesc {
                    name: "echo".to_owned(),
                    description: "Echo args".to_owned(),
                    schema_json: "{}".to_owned(),
                }],
                hooks: vec![],
            }
        }

        fn call_tool(name: &str, args: Value) -> Result<Value, SdkError> {
            if name == "echo" {
                Ok(args)
            } else {
                Err(SdkError::ToolNotFound(name.to_owned()))
            }
        }

        fn on_hook(_kind: HookKind, _payload: HookPayload) {}
    }

    #[test]
    fn dispatch_call_tool_echo_roundtrip() {
        let req = CallRequest {
            tool_name: "echo".to_owned(),
            args: Value::Text("hello".to_owned()),
        };
        let req_bytes = postcard::to_allocvec(&req).unwrap();

        let resp_buf = dispatch_call_tool::<TestPlugin>(&req_bytes);
        // Skip 4-byte length prefix.
        let resp: crate::types::CallResponse = postcard::from_bytes(&resp_buf[4..]).unwrap();
        assert_eq!(resp.result, Ok(Value::Text("hello".to_owned())));
    }

    #[test]
    fn dispatch_call_tool_not_found() {
        let req = CallRequest {
            tool_name: "missing".to_owned(),
            args: Value::Null,
        };
        let req_bytes = postcard::to_allocvec(&req).unwrap();

        let resp_buf = dispatch_call_tool::<TestPlugin>(&req_bytes);
        let resp: crate::types::CallResponse = postcard::from_bytes(&resp_buf[4..]).unwrap();
        assert!(matches!(resp.result, Err(SdkError::ToolNotFound(_))));
    }

    #[test]
    fn dispatch_call_tool_malformed_bytes() {
        // Garbage bytes should return InvalidArgs, not panic.
        let resp_buf = dispatch_call_tool::<TestPlugin>(b"\xff\xfe garbage");
        let resp: crate::types::CallResponse = postcard::from_bytes(&resp_buf[4..]).unwrap();
        assert!(matches!(resp.result, Err(SdkError::InvalidArgs(_))));
    }

    #[test]
    fn dispatch_on_hook_silently_drops_malformed() {
        // Should not panic on garbage bytes.
        dispatch_on_hook::<TestPlugin>(b"\xff\xfe garbage");
    }

    #[test]
    fn dispatch_on_hook_valid_envelope() {
        let envelope = crate::types::HookEnvelope {
            kind: HookKind::BeforeToolCall,
            payload: HookPayload::BeforeToolCall {
                tool_name: "echo".to_owned(),
                args: Value::Null,
            },
        };
        let hook_bytes = postcard::to_allocvec(&envelope).unwrap();
        // Should not panic.
        dispatch_on_hook::<TestPlugin>(&hook_bytes);
    }

    #[test]
    fn vec_into_raw_and_free_buffer_roundtrip() {
        let original = vec![1u8, 2, 3, 4, 5];
        let len = original.len();
        let ptr = vec_into_raw(original);
        // Safety: ptr came from vec_into_raw with len = 5.
        unsafe { free_buffer(ptr, len) };
        // If this test runs under sanitisers / miri: no double-free, no leak.
    }

    #[test]
    fn vec_into_raw_with_excess_capacity_frees_correctly() {
        // Allocate a Vec with capacity > len to reproduce the scenario the old
        // ManuallyDrop approach would mis-handle: capacity=128 but len=5.
        // into_boxed_slice shrinks before leaking, so free_buffer(ptr, 5) is safe.
        let mut over = Vec::with_capacity(128);
        over.extend_from_slice(&[10u8, 20, 30, 40, 50]);
        assert!(over.capacity() >= 128, "precondition: capacity > len");
        let len = over.len();
        let ptr = vec_into_raw(over);
        // Safety: ptr came from vec_into_raw above; len is correct.
        unsafe { free_buffer(ptr, len) };
    }

    #[test]
    fn encode_manifest_length_prefix_is_correct() {
        let manifest = PluginManifest {
            name: "x".to_owned(),
            version: "0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![],
            hooks: vec![],
        };
        let buf = encode_manifest(manifest.clone());
        let declared_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(buf.len(), 4 + declared_len);
        let decoded: PluginManifest = postcard::from_bytes(&buf[4..]).expect("decode");
        assert_eq!(decoded.name, "x");
    }
}
