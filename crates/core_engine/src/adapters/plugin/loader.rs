//! Wasmtime loader and capability linker (spec 11c).
//!
//! # Wireframe
//!
//! ```text
//! WasmtimeHost::load(path, fs_port)
//!   Engine(config)
//!   Module::from_file(engine, path)          ← U1: single .wasm file
//!   Store<WasmStoreData> { wasi_p1, host }
//!   Linker::new(engine)
//!     + p1::add_to_linker_sync (ZERO preopened dirs, no net) ← WASI tension
//!     + func_wrap("tower_host","host_log",…)  ← explicit allow-list
//!     + func_wrap("tower_host","host_read_file",…) via FileSystemPort
//!   linker.instantiate(store, module)         ← link error = denied import
//!   instance.__plugin_init()                  ← ABI handshake
//!   verify manifest.abi == ABI_VERSION        ← reject on mismatch (AC4)
//!   → Box<dyn PluginInstance>                 ← implements 11b trait
//! ```
//!
//! # WASI lockdown (critical — U2)
//!
//! `hello_plugin` (and all plugins) compile for `wasm32-wasip1`, whose std
//! library emits `wasi_snapshot_preview1::*` imports. We MUST link those imports
//! or the module fails to instantiate. However, full WASI with preopened
//! directories and network would grant the guest raw filesystem/network access —
//! a sandbox escape.
//!
//! Resolution: link WASIp1 via `wasmtime_wasi::p1::add_to_linker_sync` but
//! build the `WasiCtx` with `WasiCtxBuilder::new()` and NO further configuration:
//! - zero preopened directories → `path_open` / `fd_read` return ENOENT / EBADF
//! - no environment variables → no host env leakage
//! - no network → socket APIs unavailable
//! - stdio → no-op sink (no host stdio leakage)
//!
//! Note: `WasiCtxBuilder::new()` leaves CLOCKS (`clock_time_get`) and RNG
//! (`random_get`) functional by default. This is accepted by design — both are
//! required by Rust's standard library (HashMap seeding, time ops) and grant no
//! filesystem, network, or environment access.
//!
//! The **only** real capability surface is our explicit `tower_host` allow-list
//! (two functions: `host_log` and `host_read_file`). Any other `tower_host`
//! import — whether from an attacker or a misconfigured plugin — produces a
//! `LinkError` at instantiation time (AC3 / UN2).
//!
//! # Unsafe surface
//!
//! One `unsafe impl Send for WasmInstance` — rationale at the impl site.
//! All guest memory reads/writes use wasmtime's safe `Memory::data()` /
//! `data_mut()` with explicit bounds checks; no `unsafe { }` blocks exist
//! in this file.

use std::path::Path;
use std::sync::Arc;

use wasmtime::{AsContext, AsContextMut, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::{add_to_linker_sync, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

// IsolationConfig is defined in super::isolation but imported here only
// for the apply_compute_bounds method parameter. Using the module path avoids
// a re-export that would leak it from loader.rs into the public API.
// The import is inline in the method below.

use plugin_sdk::__private::BUFFER_HEADER_LEN;
use plugin_sdk::{HookKind, HookPayload, PluginManifest, Value, ABI_VERSION};

use crate::domain::RelativePath;
use crate::domain::{PluginHostError, PluginInstance};
use crate::ports::{FileSystemPort, PortError};

use super::error::PluginLoadError;

// ── HostState ─────────────────────────────────────────────────────────────────

/// Custom host state accessible from capability host functions via
/// `Caller::data()` / `Caller::data_mut()`.
struct HostState {
    /// Outbound filesystem port — the only real I/O surface for file reads.
    ///
    /// `Arc<dyn ... + Send + Sync>` so the Store (which embeds this) is Send.
    fs_port: Arc<dyn FileSystemPort + Send + Sync>,
}

// ── WasmStoreData ─────────────────────────────────────────────────────────────

/// Data held inside the wasmtime `Store<WasmStoreData>`.
///
/// Combines the WASIp1 context (zero-capability) with our custom host state.
///
/// `pub(crate)` so [`super::isolation::IsolatedSandbox`] can name the store
/// type in `Store<WasmStoreData>` for `apply_compute_bounds`.
pub(crate) struct WasmStoreData {
    /// WASIp1 context: built with zero preopened dirs and no network.
    ///
    /// Decision: embed `WasiP1Ctx` directly (not behind a Box) so
    /// `p1::add_to_linker_sync(linker, |s| &mut s.wasi)` is a simple field
    /// projection — the closure is `Copy + Send + Sync` as required.
    wasi: WasiP1Ctx,
    /// Custom tower_host capabilities.
    host: HostState,
}

// ── WasmtimeHost ─────────────────────────────────────────────────────────────

/// Loader that compiles and instantiates a single `.wasm` plugin binary.
///
/// # Security boundary
///
/// The linker exposes exactly two host functions under the `"tower_host"` module:
/// - `host_log`: log to host stderr.
/// - `host_read_file`: read a workspace-relative file via [`FileSystemPort`].
///
/// Any other `"tower_host"` import causes an instantiation link error (AC3).
/// WASI syscalls are linked but with a zero-capability context so they are
/// effectively no-ops from a security perspective (U2).
pub struct WasmtimeHost;

impl WasmtimeHost {
    /// Load a `.wasm` plugin, perform the ABI handshake, and return a
    /// `Box<dyn PluginInstance>` ready for registration.
    ///
    /// # Errors
    ///
    /// Returns a typed [`PluginLoadError`] for every failure mode. No panics.
    pub fn load(
        path: impl AsRef<Path>,
        fs_port: Arc<dyn FileSystemPort + Send + Sync>,
    ) -> Result<Box<dyn PluginInstance>, PluginLoadError> {
        let path = path.as_ref();

        // ── 1. Engine and module ─────────────────────────────────────────────
        // Decision: one Engine per load call for simplicity (11d shares one via load_with_engine).
        // Trade-off: ~1–2 ms Engine::default() cost at load time; acceptable.
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|e| PluginLoadError::WasmLoad(e.to_string()))?;

        // ── 2. WasiCtx with zero capabilities ───────────────────────────────
        // `WasiCtxBuilder::new()` with no further configuration produces a
        // WASIp1 context with:
        //   - no preopened directories  (path_open → ENOENT)
        //   - no inherited env          (no leakage)
        //   - no network sockets        (connect/bind → error)
        //   - no stdin/stdout/stderr    (all closed / null)
        //
        // Note: wasmtime-wasi's WasiCtxBuilder leaves CLOCKS (clock_time_get)
        // and RNG (random_get) functional by default. This is accepted by
        // design — they grant no filesystem, network, or environment access
        // and are required by Rust's standard library (e.g. HashMap seeding,
        // time-based operations). Disabling them would break most plugins
        // without any meaningful security benefit.
        //
        // This is the lockdown described in the spec's WASI TENSION note.
        let wasi_p1 = WasiCtxBuilder::new().build_p1();

        // ── 3. Store ─────────────────────────────────────────────────────────
        let store_data = WasmStoreData {
            wasi: wasi_p1,
            host: HostState { fs_port },
        };
        let mut store = Store::new(&engine, store_data);

        // ── 4. Linker: WASI stubs + capability allow-list ────────────────────
        let mut linker: Linker<WasmStoreData> = Linker::new(&engine);

        // Link WASIp1 imports (wasi_snapshot_preview1::*).
        // The zero-capability WasiCtx makes these harmless stubs.
        add_to_linker_sync(&mut linker, |s: &mut WasmStoreData| &mut s.wasi)
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        // ── tower_host::host_log ─────────────────────────────────────────────
        // Allow-listed capability #1: write a UTF-8 log message to host stderr.
        // The guest retains ownership of the message buffer (no free needed).
        linker
            .func_wrap(
                "tower_host",
                "host_log",
                |mut caller: wasmtime::Caller<'_, WasmStoreData>, ptr: i32, len: i32| {
                    let ptr = ptr as u32 as usize;
                    let len = len as u32 as usize;
                    let mem = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return, // no memory — silently drop
                    };
                    let data = mem.data(&caller);
                    if let Some(slice) = data.get(ptr..ptr.saturating_add(len)) {
                        if let Ok(msg) = std::str::from_utf8(slice) {
                            // Cap at 4096 bytes on a char boundary to prevent
                            // log-flooding DoS from a misbehaving plugin.
                            const MAX_LOG_BYTES: usize = 4096;
                            let truncated = if msg.len() > MAX_LOG_BYTES {
                                // Walk back to the last char boundary at or before MAX_LOG_BYTES.
                                let mut end = MAX_LOG_BYTES;
                                while !msg.is_char_boundary(end) {
                                    end -= 1;
                                }
                                &msg[..end]
                            } else {
                                msg
                            };
                            eprintln!("[plugin] {truncated}");
                        }
                    }
                    // Out-of-bounds / invalid UTF-8: silently ignore.
                },
            )
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        // ── tower_host::host_read_file ────────────────────────────────────────
        // Allow-listed capability #2: read a workspace-relative file via the
        // FileSystemPort outbound port. Security properties:
        //
        // - Path decoded from guest memory (bounds-checked before FS call).
        // - Path validated: rejects empty, absolute ('/'), and '..' components.
        // - Reads through `FileSystemPort::read` — no raw `std::fs` access.
        // - Guest buffer allocated in guest memory via `__plugin_alloc`.
        // - All writes to guest memory are bounds-checked.
        //
        // Signature: (path_ptr: i32, path_len: i32, out_ptr: i32, out_len_ptr: i32) -> i32
        // Returns: 0=ok, 1=not_found, 2=io_error
        linker
            .func_wrap(
                "tower_host",
                "host_read_file",
                |mut caller: wasmtime::Caller<'_, WasmStoreData>,
                 path_ptr: i32,
                 path_len: i32,
                 out_ptr: i32,
                 out_len_ptr: i32|
                 -> i32 {
                    host_read_file_impl(&mut caller, path_ptr, path_len, out_ptr, out_len_ptr)
                },
            )
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        // ── 5. Instantiate ───────────────────────────────────────────────────
        // A guest importing a function NOT in the above allow-list and NOT in
        // wasi_snapshot_preview1 will fail here with LinkError (AC3 / UN2).
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        // ── 6. ABI handshake: call __plugin_init ─────────────────────────────
        let init_fn = instance
            .get_typed_func::<(), u32>(&mut store, "__plugin_init")
            .map_err(|e| PluginLoadError::MissingExport(format!("__plugin_init: {e}")))?;

        let manifest_ptr = init_fn
            .call(&mut store, ())
            .map_err(|e| PluginLoadError::InitTrap(e.to_string()))?
            as usize;

        // Read the length-prefixed manifest from guest memory (bounds-checked).
        let manifest = read_manifest_from_guest(&instance, &mut store, manifest_ptr)?;

        // Free the manifest buffer in the guest (buffer ownership protocol).
        free_manifest_buffer(&instance, &mut store, manifest_ptr)?;

        // ── 7. Verify ABI version (AC4 / UN1) ────────────────────────────────
        if manifest.abi != ABI_VERSION {
            return Err(PluginLoadError::AbiMismatch {
                expected: ABI_VERSION,
                got: manifest.abi,
            });
        }

        // ── 8. Wrap as PluginInstance ─────────────────────────────────────────
        Ok(Box::new(WasmInstance {
            instance,
            store,
            manifest,
        }))
    }

    /// Load a `.wasm` plugin using a provided [`Engine`], returning the concrete
    /// [`WasmInstance`] (spec 11d).
    ///
    /// Used by the fault isolation layer so all sandboxes share one engine with
    /// fuel and epoch interruption enabled. Returns the concrete type so the
    /// isolation layer can call `apply_compute_bounds` directly on the store
    /// without Any-downcasting through the domain trait.
    ///
    /// `pub(crate)`: returns `WasmInstance` which is `pub(crate)`; callers
    /// outside `core_engine` use `IsolatedSandbox` (which wraps this).
    ///
    /// # Errors
    ///
    /// Same as [`Self::load`].
    pub(crate) fn load_with_engine(
        engine: &Engine,
        path: impl AsRef<Path>,
        fs_port: Arc<dyn FileSystemPort + Send + Sync>,
    ) -> Result<WasmInstance, PluginLoadError> {
        let path = path.as_ref();

        let module = Module::from_file(engine, path)
            .map_err(|e| PluginLoadError::WasmLoad(e.to_string()))?;

        let wasi_p1 = WasiCtxBuilder::new().build_p1();
        let store_data = WasmStoreData {
            wasi: wasi_p1,
            host: HostState { fs_port },
        };
        let mut store = Store::new(engine, store_data);

        let mut linker: Linker<WasmStoreData> = Linker::new(engine);

        add_to_linker_sync(&mut linker, |s: &mut WasmStoreData| &mut s.wasi)
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        linker
            .func_wrap(
                "tower_host",
                "host_log",
                |mut caller: wasmtime::Caller<'_, WasmStoreData>, ptr: i32, len: i32| {
                    let ptr = ptr as u32 as usize;
                    let len = len as u32 as usize;
                    let mem = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(m)) => m,
                        _ => return,
                    };
                    let data = mem.data(&caller);
                    if let Some(slice) = data.get(ptr..ptr.saturating_add(len)) {
                        if let Ok(msg) = std::str::from_utf8(slice) {
                            const MAX_LOG_BYTES: usize = 4096;
                            let truncated = if msg.len() > MAX_LOG_BYTES {
                                let mut end = MAX_LOG_BYTES;
                                while !msg.is_char_boundary(end) {
                                    end -= 1;
                                }
                                &msg[..end]
                            } else {
                                msg
                            };
                            eprintln!("[plugin] {truncated}");
                        }
                    }
                },
            )
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        linker
            .func_wrap(
                "tower_host",
                "host_read_file",
                |mut caller: wasmtime::Caller<'_, WasmStoreData>,
                 path_ptr: i32,
                 path_len: i32,
                 out_ptr: i32,
                 out_len_ptr: i32|
                 -> i32 {
                    host_read_file_impl(&mut caller, path_ptr, path_len, out_ptr, out_len_ptr)
                },
            )
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| PluginLoadError::LinkError(e.to_string()))?;

        // If the engine has fuel consumption enabled (as IsolationEngine does),
        // we must set a generous fuel budget before calling __plugin_init so the
        // init sequence does not immediately run out of fuel (the default is 0).
        // This is the load-time budget, not the per-call runtime budget.
        //
        // Decision: 100_000_000 units for init. Init is one-time work (manifest
        // serialisation, memory allocation) — far less than a tool call.
        // We ignore the error if fuel consumption is not enabled on this engine.
        let _ = store.set_fuel(100_000_000);

        // If epoch interruption is enabled, set a large deadline so __plugin_init
        // and the ABI handshake calls are not interrupted. The per-call deadline
        // is applied fresh before each tool/hook call by apply_compute_bounds.
        //
        // Decision: u64::MAX / 2 ticks (~292 years at 1 tick/ns, or ~13 billion
        // years at 10 ms/tick). This avoids the overflow that occurs when using
        // u64::MAX - 1 with a background epoch ticker: set_epoch_deadline(n)
        // computes current_epoch + n, which overflows if current_epoch > 0 and
        // n = u64::MAX - 1.
        store.set_epoch_deadline(u64::MAX / 2);

        let init_fn = instance
            .get_typed_func::<(), u32>(&mut store, "__plugin_init")
            .map_err(|e| PluginLoadError::MissingExport(format!("__plugin_init: {e}")))?;

        let manifest_ptr = init_fn
            .call(&mut store, ())
            .map_err(|e| PluginLoadError::InitTrap(e.to_string()))?
            as usize;

        let manifest = read_manifest_from_guest(&instance, &mut store, manifest_ptr)?;
        free_manifest_buffer(&instance, &mut store, manifest_ptr)?;

        if manifest.abi != ABI_VERSION {
            return Err(PluginLoadError::AbiMismatch {
                expected: ABI_VERSION,
                got: manifest.abi,
            });
        }

        Ok(WasmInstance {
            instance,
            store,
            manifest,
        })
    }
}

// ── host_read_file implementation ─────────────────────────────────────────────

/// Extracted host_read_file logic. Reads through FileSystemPort and writes
/// the file bytes into a newly allocated guest buffer.
///
/// Returns 0=ok, 1=not_found, 2=io_error.
fn host_read_file_impl(
    caller: &mut wasmtime::Caller<'_, WasmStoreData>,
    path_ptr: i32,
    path_len: i32,
    out_ptr: i32,
    out_len_ptr: i32,
) -> i32 {
    let path_ptr = path_ptr as u32 as usize;
    let path_len = path_len as u32 as usize;

    // Decode the workspace-relative path from guest memory.
    let path_str: String = {
        let mem = match caller.get_export("memory") {
            Some(wasmtime::Extern::Memory(m)) => m,
            _ => return 2,
        };
        // Use explicit reborrow so caller is not consumed.
        let data = mem.data(&*caller);
        let slice = match data.get(path_ptr..path_ptr.saturating_add(path_len)) {
            Some(s) => s,
            None => return 2,
        };
        match std::str::from_utf8(slice) {
            Ok(s) => s.to_owned(),
            Err(_) => return 2,
        }
    };

    // Security: reject dangerous paths (traversal prevention).
    if path_str.is_empty() || path_str.starts_with('/') || path_str.contains("..") {
        return 1; // treat as not-found rather than leaking error detail
    }

    // Read through the outbound port (no raw std::fs).
    let rel_path = RelativePath::new(&path_str);
    let file_bytes: Vec<u8> = {
        let fs = Arc::clone(&caller.data().host.fs_port);
        match fs.read(&rel_path) {
            Ok(bytes) => bytes,
            Err(PortError::NotFound) => return 1,
            Err(_) => return 2,
        }
    };

    let file_len = file_bytes.len();

    // Handle empty file: write 0 to out_len_ptr, leave out_ptr null.
    if file_len == 0 {
        let mem = match caller.get_export("memory") {
            Some(wasmtime::Extern::Memory(m)) => m,
            _ => return 2,
        };
        let out_len_ptr = out_len_ptr as u32 as usize;
        let data = mem.data_mut(&mut *caller);
        if out_len_ptr.saturating_add(4) > data.len() {
            return 2;
        }
        data[out_len_ptr..out_len_ptr + 4].copy_from_slice(&0u32.to_le_bytes());
        return 0;
    }

    // Allocate guest buffer via __plugin_alloc(file_len).
    let alloc_fn = match caller.get_export("__plugin_alloc") {
        Some(wasmtime::Extern::Func(f)) => f,
        _ => return 2,
    };
    let alloc_typed = match alloc_fn.typed::<u32, u32>(&*caller) {
        Ok(f) => f,
        Err(_) => return 2,
    };
    let guest_buf_ptr = match alloc_typed.call(&mut *caller, file_len as u32) {
        Ok(p) => p as usize,
        Err(_) => return 2,
    };

    // Write file bytes into the allocated guest buffer (bounds-checked).
    let mem = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => m,
        _ => return 2,
    };
    {
        let data = mem.data_mut(&mut *caller);

        if guest_buf_ptr.saturating_add(file_len) > data.len() {
            return 2;
        }
        data[guest_buf_ptr..guest_buf_ptr + file_len].copy_from_slice(&file_bytes);

        let out_ptr = out_ptr as u32 as usize;
        let out_len_ptr = out_len_ptr as u32 as usize;

        if out_ptr.saturating_add(4) > data.len() || out_len_ptr.saturating_add(4) > data.len() {
            return 2;
        }

        // Write guest_buf_ptr as little-endian u32 at *out_ptr.
        data[out_ptr..out_ptr + 4].copy_from_slice(&(guest_buf_ptr as u32).to_le_bytes());
        // Write file_len as little-endian u32 at *out_len_ptr.
        data[out_len_ptr..out_len_ptr + 4].copy_from_slice(&(file_len as u32).to_le_bytes());
    }

    0 // ok
}

// ── Guest memory helpers ──────────────────────────────────────────────────────

/// Read and deserialise a length-prefixed postcard manifest buffer from guest
/// linear memory.
///
/// Buffer layout (from `plugin_sdk::__private::encode_manifest`):
/// ```text
/// [ u32 le payload_len (4 bytes) ][ payload_len bytes of postcard ]
/// ```
///
/// All offset arithmetic is bounds-checked against the live memory size before
/// any read. Returns [`PluginLoadError::InvalidManifestPointer`] on any bounds
/// violation.
fn read_manifest_from_guest(
    instance: &wasmtime::Instance,
    store: &mut Store<WasmStoreData>,
    manifest_ptr: usize,
) -> Result<PluginManifest, PluginLoadError> {
    let memory = instance
        .get_memory(store.as_context_mut(), "memory")
        .ok_or(PluginLoadError::InvalidManifestPointer)?;

    let mem_data = memory.data(store.as_context());
    let mem_size = mem_data.len();

    // Read 4-byte length header.
    let header_end = manifest_ptr
        .checked_add(BUFFER_HEADER_LEN)
        .ok_or(PluginLoadError::InvalidManifestPointer)?;
    if header_end > mem_size {
        return Err(PluginLoadError::InvalidManifestPointer);
    }
    let payload_len = u32::from_le_bytes([
        mem_data[manifest_ptr],
        mem_data[manifest_ptr + 1],
        mem_data[manifest_ptr + 2],
        mem_data[manifest_ptr + 3],
    ]) as usize;

    // Read payload bytes.
    let payload_start = header_end;
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or(PluginLoadError::InvalidManifestPointer)?;
    if payload_end > mem_size {
        return Err(PluginLoadError::InvalidManifestPointer);
    }

    let payload_bytes = &mem_data[payload_start..payload_end];
    postcard::from_bytes::<PluginManifest>(payload_bytes)
        .map_err(|e| PluginLoadError::ManifestDeserialize(e.to_string()))
}

/// Call `__plugin_free` in the guest to release the manifest buffer.
///
/// `total_len = BUFFER_HEADER_LEN + payload_len`.
fn free_manifest_buffer(
    instance: &wasmtime::Instance,
    store: &mut Store<WasmStoreData>,
    manifest_ptr: usize,
) -> Result<(), PluginLoadError> {
    let memory = instance
        .get_memory(store.as_context_mut(), "memory")
        .ok_or(PluginLoadError::InvalidManifestPointer)?;

    let mem_data = memory.data(store.as_context());
    let mem_size = mem_data.len();

    let header_end = manifest_ptr
        .checked_add(BUFFER_HEADER_LEN)
        .ok_or(PluginLoadError::InvalidManifestPointer)?;
    if header_end > mem_size {
        return Err(PluginLoadError::InvalidManifestPointer);
    }
    let payload_len = u32::from_le_bytes([
        mem_data[manifest_ptr],
        mem_data[manifest_ptr + 1],
        mem_data[manifest_ptr + 2],
        mem_data[manifest_ptr + 3],
    ]) as usize;
    let total_len = BUFFER_HEADER_LEN
        .checked_add(payload_len)
        .ok_or(PluginLoadError::InvalidManifestPointer)?;

    let free_fn = instance
        .get_typed_func::<(u32, u32), ()>(store.as_context_mut(), "__plugin_free")
        .map_err(|e| PluginLoadError::MissingExport(format!("__plugin_free: {e}")))?;

    free_fn
        .call(
            store.as_context_mut(),
            (manifest_ptr as u32, total_len as u32),
        )
        .map_err(|e| PluginLoadError::InitTrap(format!("__plugin_free trap: {e}")))?;

    Ok(())
}

// ── WasmInstance ─────────────────────────────────────────────────────────────

/// A live wasm plugin instance implementing [`PluginInstance`] (spec 11b).
///
/// Holds the wasmtime `Instance`, its `Store`, and the cached manifest returned
/// by `__plugin_init`. All calls go through the four guest exports using the
/// length-prefixed postcard protocol (spec 11a).
///
/// This type is `pub(crate)` — callers outside `core_engine` interact with it
/// exclusively through the [`PluginInstance`] trait returned by
/// [`WasmtimeHost::load`]. The concrete wasmtime runtime type must not leak
/// across the hexagonal adapter boundary.
pub(crate) struct WasmInstance {
    instance: wasmtime::Instance,
    store: Store<WasmStoreData>,
    manifest: PluginManifest,
}

// WasmInstance contains a wasmtime::Instance and a Store<WasmStoreData>.
// Store<T> is NOT auto-Send: it contains interior raw pointers that wasmtime
// intentionally does not mark Send on its own (the runtime does not guarantee
// cross-thread use without external synchronisation). WasmStoreData itself is
// Send (Arc<dyn FileSystemPort + Send + Sync> + WasiP1Ctx are both Send), but
// that is not sufficient because the raw pointers inside Store break auto-Send.
//
// Safety: WasmInstance is accessed exclusively — the PluginHostRegistry wraps
// every plugin in a Mutex<Box<dyn PluginInstance>>, so at most one thread holds
// the lock at a time. No concurrent access to the Store or Instance is possible.
// This exclusive access makes the impl sound despite the raw-pointer interior.
//
// Decision: explicit impl (not derived) because wasmtime::Instance and
// Store<T> do not auto-derive Send.
unsafe impl Send for WasmInstance {}

impl PluginInstance for WasmInstance {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value, PluginHostError> {
        use plugin_sdk::{CallRequest, CallResponse};

        let request = CallRequest {
            tool_name: name.to_owned(),
            args,
        };
        let request_bytes = postcard::to_allocvec(&request)
            .map_err(|e| PluginHostError::CallFailed(e.to_string()))?;

        // Allocate guest buffer, write args, call __plugin_call_tool.
        let resp_ptr = self
            .call_guest_with_bytes("__plugin_call_tool", &request_bytes)
            .map_err(|e| PluginHostError::CallFailed(e.to_string()))?;

        // Read + free result buffer.
        let response_bytes = self
            .read_and_free_result_buffer(resp_ptr)
            .map_err(|e| PluginHostError::CallFailed(e.to_string()))?;

        let response: CallResponse = postcard::from_bytes(&response_bytes)
            .map_err(|e| PluginHostError::CallFailed(e.to_string()))?;

        response.result.map_err(|sdk_err| match sdk_err {
            plugin_sdk::SdkError::ToolNotFound(n) => PluginHostError::ToolNotFound(n),
            plugin_sdk::SdkError::InvalidArgs(m) => PluginHostError::InvalidArgs(m),
            plugin_sdk::SdkError::CallFailed(m) => PluginHostError::CallFailed(m),
        })
    }

    fn deliver_hook(
        &mut self,
        kind: HookKind,
        payload: HookPayload,
    ) -> Result<(), PluginHostError> {
        use plugin_sdk::HookEnvelope;

        let envelope = HookEnvelope { kind, payload };
        let envelope_bytes = postcard::to_allocvec(&envelope)
            .map_err(|e| PluginHostError::HookDeliveryFailed(e.to_string()))?;

        self.call_guest_hook(&envelope_bytes)
            .map_err(|e| PluginHostError::HookDeliveryFailed(e.to_string()))?;

        Ok(())
    }
}

impl WasmInstance {
    /// Allocate a guest buffer via `__plugin_alloc`, write `bytes` into it,
    /// call `export_name(ptr, len)`, and return the result pointer.
    ///
    /// Used for `__plugin_call_tool`.
    fn call_guest_with_bytes(&mut self, export_name: &str, bytes: &[u8]) -> Result<usize, String> {
        debug_assert!(
            !bytes.is_empty(),
            "__plugin_alloc(0) is contractually forbidden"
        );
        let len = bytes.len() as u32;

        // Step 1: allocate guest buffer.
        let alloc_fn = self
            .instance
            .get_typed_func::<u32, u32>(&mut self.store, "__plugin_alloc")
            .map_err(|e| format!("__plugin_alloc not found: {e}"))?;
        let guest_ptr = alloc_fn
            .call(&mut self.store, len)
            .map_err(|e| format!("__plugin_alloc trap: {e}"))? as usize;

        // Step 2: write bytes into guest memory (bounds-checked).
        {
            let memory = self
                .instance
                .get_memory(&mut self.store, "memory")
                .ok_or_else(|| "guest has no 'memory' export".to_owned())?;

            let mem_data = memory.data_mut(&mut self.store);
            let end = guest_ptr
                .checked_add(bytes.len())
                .ok_or_else(|| "argument pointer overflows usize".to_owned())?;
            if end > mem_data.len() {
                return Err(format!(
                    "guest buffer OOB: ptr={guest_ptr} len={} mem={}",
                    bytes.len(),
                    mem_data.len()
                ));
            }
            mem_data[guest_ptr..end].copy_from_slice(bytes);
        }

        // Step 3: call the guest export (ptr + len → result_ptr).
        let call_fn = self
            .instance
            .get_typed_func::<(u32, u32), u32>(&mut self.store, export_name)
            .map_err(|e| format!("{export_name} not found: {e}"))?;
        let result_ptr = call_fn
            .call(&mut self.store, (guest_ptr as u32, len))
            .map_err(|e| format!("{export_name} trap: {e}"))? as usize;

        Ok(result_ptr)
    }

    /// Allocate a guest buffer, write `bytes`, and call
    /// `__plugin_on_hook(ptr, len)`.
    ///
    /// Hooks return no value.
    fn call_guest_hook(&mut self, bytes: &[u8]) -> Result<(), String> {
        debug_assert!(
            !bytes.is_empty(),
            "__plugin_alloc(0) is contractually forbidden"
        );
        let len = bytes.len() as u32;

        let alloc_fn = self
            .instance
            .get_typed_func::<u32, u32>(&mut self.store, "__plugin_alloc")
            .map_err(|e| format!("__plugin_alloc not found: {e}"))?;
        let guest_ptr = alloc_fn
            .call(&mut self.store, len)
            .map_err(|e| format!("__plugin_alloc trap: {e}"))? as usize;

        {
            let memory = self
                .instance
                .get_memory(&mut self.store, "memory")
                .ok_or_else(|| "guest has no 'memory' export".to_owned())?;
            let mem_data = memory.data_mut(&mut self.store);
            let end = guest_ptr
                .checked_add(bytes.len())
                .ok_or_else(|| "hook pointer overflows usize".to_owned())?;
            if end > mem_data.len() {
                return Err(format!(
                    "hook buffer OOB: ptr={guest_ptr} len={} mem={}",
                    bytes.len(),
                    mem_data.len()
                ));
            }
            mem_data[guest_ptr..end].copy_from_slice(bytes);
        }

        let hook_fn = self
            .instance
            .get_typed_func::<(u32, u32), ()>(&mut self.store, "__plugin_on_hook")
            .map_err(|e| format!("__plugin_on_hook not found: {e}"))?;
        hook_fn
            .call(&mut self.store, (guest_ptr as u32, len))
            .map_err(|e| format!("__plugin_on_hook trap: {e}"))?;

        Ok(())
    }

    /// Read the postcard payload from a length-prefixed result buffer in guest
    /// memory, call `__plugin_free` to release it, and return the payload bytes.
    ///
    /// Buffer layout: `[ u32 le payload_len ][ payload_bytes ]`
    fn read_and_free_result_buffer(&mut self, result_ptr: usize) -> Result<Vec<u8>, String> {
        let (payload, total_len) = {
            let memory = self
                .instance
                .get_memory(&mut self.store, "memory")
                .ok_or_else(|| "guest has no 'memory' export".to_owned())?;

            let mem_data = memory.data(&self.store);
            let mem_size = mem_data.len();

            let header_end = result_ptr
                .checked_add(BUFFER_HEADER_LEN)
                .ok_or_else(|| "result pointer overflows usize".to_owned())?;
            if header_end > mem_size {
                return Err(format!(
                    "result buffer header OOB: ptr={result_ptr} mem={mem_size}"
                ));
            }
            let payload_len = u32::from_le_bytes([
                mem_data[result_ptr],
                mem_data[result_ptr + 1],
                mem_data[result_ptr + 2],
                mem_data[result_ptr + 3],
            ]) as usize;

            let payload_end = header_end
                .checked_add(payload_len)
                .ok_or_else(|| "payload length overflows usize".to_owned())?;
            if payload_end > mem_size {
                return Err(format!(
                    "result payload OOB: start={header_end} len={payload_len} mem={mem_size}"
                ));
            }

            let total = BUFFER_HEADER_LEN
                .checked_add(payload_len)
                .ok_or_else(|| "total_len overflows usize".to_owned())?;

            (mem_data[header_end..payload_end].to_vec(), total)
        };

        // Free the result buffer in the guest.
        let free_fn = self
            .instance
            .get_typed_func::<(u32, u32), ()>(&mut self.store, "__plugin_free")
            .map_err(|e| format!("__plugin_free not found: {e}"))?;
        free_fn
            .call(&mut self.store, (result_ptr as u32, total_len as u32))
            .map_err(|e| format!("__plugin_free trap: {e}"))?;

        Ok(payload)
    }

    /// Apply per-call fuel budget and epoch deadline to the store.
    ///
    /// Called by [`super::isolation::IsolatedSandbox`] before each guest
    /// invocation. Lives on the concrete type (not the trait) so the isolation
    /// layer can access the store directly without Any-downcasting.
    ///
    /// `config` is [`super::isolation::IsolationConfig`]; named by full path
    /// to avoid a circular import (isolation imports loader, loader would import
    /// isolation — resolved by using the full module path only here).
    pub(crate) fn apply_compute_bounds(&mut self, config: &super::isolation::IsolationConfig) {
        if let Some(fuel) = config.fuel_budget {
            // set_fuel errors only if the engine was not built with consume_fuel(true).
            // IsolationEngine always enables it; the error is a programming mistake.
            if let Err(e) = self.store.set_fuel(fuel) {
                eprintln!("[tower] set_fuel error (engine not configured for fuel?): {e}");
            }
        }
        if let Some(ticks) = config.epoch_deadline_ticks {
            self.store.set_epoch_deadline(ticks);
        }
    }
}
