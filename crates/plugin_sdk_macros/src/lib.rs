//! Proc-macro crate for the tower plugin SDK.
//!
//! This crate is a **compile-time host dependency** — it is never linked into the
//! wasm guest. The generated tokens (wasm export functions) are emitted into the
//! plugin author's crate and compiled there for `wasm32-wasip1`.
//!
//! Do not add runtime logic here. All types referenced in generated code are
//! imported from `plugin_sdk`, which is the guest-side dependency.
//!
//! # Generated ABI
//!
//! `#[plugin_main]` on a `struct T: Plugin` expands to:
//! - `#[no_mangle] pub extern "C" fn __plugin_init() -> *mut u8`
//!   Serialises `T::init()` → postcard bytes, writes length-prefixed into a
//!   stable heap buffer, returns pointer. The host reads it via WASM memory.
//! - `#[no_mangle] pub extern "C" fn __plugin_call_tool(ptr: *mut u8, len: usize) -> *mut u8`
//!   Deserialises `CallRequest`, dispatches `T::call_tool`, serialises result.
//! - `#[no_mangle] pub extern "C" fn __plugin_on_hook(ptr: *mut u8, len: usize)`
//!   Deserialises `HookEnvelope`, dispatches `T::on_hook`.
//! - `#[no_mangle] pub extern "C" fn __plugin_free(ptr: *mut u8, len: usize)`
//!   Deallocates a buffer previously allocated by the plugin exports above.
//!
//! The ABI is deliberately minimal: 4 export symbols. The host (11c) calls them.
//!
//! # Safety
//!
//! The generated `__plugin_call_tool`, `__plugin_on_hook`, and `__plugin_free`
//! functions contain `unsafe` blocks at the wasm export boundary. The invariant
//! maintained by the host (11c): every pointer passed in was previously returned
//! by a prior `__plugin_init` / `__plugin_call_tool` call and has not been freed.
//! The plugin_sdk crate itself contains zero unsafe code; unsafety is confined
//! to the generated export glue here.
//!
//! # Example
//!
//! ```rust,ignore
//! use plugin_sdk::{plugin_main, Plugin, PluginManifest, HookKind, HookPayload, Value, SdkError};
//! use plugin_sdk::ABI_VERSION;
//!
//! #[plugin_main]
//! struct HelloPlugin;
//!
//! impl Plugin for HelloPlugin {
//!     fn init() -> PluginManifest { /* ... */ }
//!     fn call_tool(name: &str, args: Value) -> Result<Value, SdkError> { /* ... */ }
//!     fn on_hook(kind: HookKind, payload: HookPayload) { /* ... */ }
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemStruct};

/// Wires up the four wasm export symbols for a `struct T: Plugin`.
///
/// Place this attribute on the struct that implements `Plugin`. The macro
/// generates the extern-C entry points the host (11c) calls to drive the plugin.
///
/// The generated exports are:
/// - `__plugin_init() -> *mut u8` — serialise manifest, return length-prefixed buffer.
/// - `__plugin_call_tool(ptr, len) -> *mut u8` — dispatch a tool call.
/// - `__plugin_on_hook(ptr, len)` — dispatch a lifecycle hook.
/// - `__plugin_free(ptr, len)` — deallocate a buffer previously returned by this plugin.
///
/// # Panics
///
/// The generated `__plugin_init` will panic if `postcard::to_allocvec` fails,
/// which cannot happen for well-formed `PluginManifest` values (no unsupported
/// types). All other panics propagate the plugin author's own logic.
#[proc_macro_attribute]
pub fn plugin_main(attr: TokenStream, item: TokenStream) -> TokenStream {
    // We do not accept any arguments on the attribute itself.
    let attr2 = proc_macro2::TokenStream::from(attr);
    if !attr2.is_empty() {
        return syn::Error::new_spanned(attr2, "#[plugin_main] takes no arguments")
            .to_compile_error()
            .into();
    }

    let input = parse_macro_input!(item as ItemStruct);
    let struct_name = &input.ident;

    let expanded = quote! {
        // Emit the original struct definition unchanged.
        #input

        // ── wasm guest export surface ────────────────────────────────────────
        // All four symbols use `#[no_mangle]` + `extern "C"` so the host linker
        // can find them by name. They are the ONLY symbols the host (11c) calls.

        /// Initialise the plugin. Returns a length-prefixed postcard buffer
        /// containing the serialised `PluginManifest`. The host reads the
        /// first 4 bytes as a little-endian `u32` length, then reads that
        /// many bytes as the manifest payload.
        ///
        /// # Safety (generated)
        ///
        /// The returned pointer is valid until `__plugin_free` is called with
        /// the same pointer and the returned length. The host MUST call
        /// `__plugin_free` exactly once per `__plugin_init` call.
        #[no_mangle]
        pub extern "C" fn __plugin_init() -> *mut u8 {
            let manifest = <#struct_name as ::plugin_sdk::Plugin>::init();
            let encoded = ::plugin_sdk::__private::encode_manifest(manifest);
            ::plugin_sdk::__private::vec_into_raw(encoded)
        }

        /// Dispatch a tool call. `ptr`/`len` point to a postcard-encoded
        /// `CallRequest`. Returns a pointer to a postcard-encoded `CallResponse`.
        ///
        /// # Safety (generated)
        ///
        /// `ptr` must point to a valid, host-allocated buffer of exactly `len`
        /// bytes that will not be freed before this function returns.
        #[no_mangle]
        pub unsafe extern "C" fn __plugin_call_tool(ptr: *mut u8, len: usize) -> *mut u8 {
            let request_bytes = unsafe { ::std::slice::from_raw_parts(ptr, len) };
            let response = ::plugin_sdk::__private::dispatch_call_tool::<#struct_name>(request_bytes);
            ::plugin_sdk::__private::vec_into_raw(response)
        }

        /// Dispatch a lifecycle hook. `ptr`/`len` point to a postcard-encoded
        /// `HookEnvelope`.
        ///
        /// # Safety (generated)
        ///
        /// Same contract as `__plugin_call_tool`.
        #[no_mangle]
        pub unsafe extern "C" fn __plugin_on_hook(ptr: *mut u8, len: usize) {
            let hook_bytes = unsafe { ::std::slice::from_raw_parts(ptr, len) };
            ::plugin_sdk::__private::dispatch_on_hook::<#struct_name>(hook_bytes);
        }

        /// Free a buffer that was previously returned by `__plugin_init` or
        /// `__plugin_call_tool`.
        ///
        /// # Safety (generated)
        ///
        /// `ptr` must have been returned by a prior call to `__plugin_init` or
        /// `__plugin_call_tool` and must not have been freed previously.
        #[no_mangle]
        pub unsafe extern "C" fn __plugin_free(ptr: *mut u8, len: usize) {
            unsafe { ::plugin_sdk::__private::free_buffer(ptr, len) };
        }
    };

    TokenStream::from(expanded)
}

/// Marker attribute for tool handler functions (currently a no-op passthrough).
///
/// Annotate a function with `#[plugin_export]` to signal intent. In future
/// milestones this attribute will register the function in the tool dispatch
/// table. For now it emits the function unchanged so plugin authors adopt the
/// convention early.
///
/// # Example
///
/// ```rust,ignore
/// #[plugin_export]
/// fn greet(args: Value) -> Result<Value, SdkError> { /* ... */ }
/// ```
#[proc_macro_attribute]
pub fn plugin_export(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr2 = proc_macro2::TokenStream::from(attr);
    if !attr2.is_empty() {
        return syn::Error::new_spanned(attr2, "#[plugin_export] takes no arguments")
            .to_compile_error()
            .into();
    }
    // Pass the item through unchanged — the attribute is semantic documentation
    // for now; tool dispatch is explicit in Plugin::call_tool (U3).
    item
}

// Decision: trybuild compile-tests live in plugin_sdk (not plugin_sdk_macros)
// because trybuild's test-case Cargo.toml needs plugin_sdk as a dependency
// (the tests import plugin_sdk types). Running them here would require
// plugin_sdk_macros to depend on plugin_sdk, creating a cycle.
// Trade-off: tests are split across two crates; macro expand correctness is
// verified end-to-end via plugin_sdk's trybuild harness.
