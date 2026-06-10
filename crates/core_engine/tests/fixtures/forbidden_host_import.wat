;; Test fixture: a wasm module that imports an UNREGISTERED tower_host function.
;;
;; This module imports `tower_host::raw_fs_write` which is NOT in the capability
;; allow-list registered by WasmtimeHost::load. The linker has no binding for
;; this import, so instantiation must fail with a LinkError (spec 11c AC3 / U2).
;;
;; This is distinct from forbidden_import.wasm (which imports a WASI function that
;; IS resolved by the WASI linker). This fixture proves the security property that
;; matters: an unregistered tower_host import is positively rejected at link time,
;; not silently resolved or silently dropped.
;;
;; Decision: use "raw_fs_write" as the name — it is obviously dangerous and not
;; a real capability — so no reader can mistake its intent.

(module
  ;; Import an UNREGISTERED tower_host capability.
  ;; The WasmtimeHost linker only registers "host_log" and "host_read_file".
  ;; "raw_fs_write" has no binding, so the linker must return a LinkError.
  (import "tower_host" "raw_fs_write"
    (func $raw_fs_write (param i32 i32) (result i32))
  )

  ;; Minimal memory required for any wasm module.
  (memory (export "memory") 1)

  ;; __plugin_alloc: stub required by the plugin protocol.
  (func (export "__plugin_alloc") (param $len i32) (result i32)
    i32.const 64
  )

  ;; __plugin_init: stub — the host must never reach this (link fails first).
  (func (export "__plugin_init") (result i32)
    i32.const 0
  )

  ;; __plugin_call_tool: stub.
  (func (export "__plugin_call_tool") (param $ptr i32) (param $len i32) (result i32)
    i32.const 0
  )

  ;; __plugin_on_hook: stub.
  (func (export "__plugin_on_hook") (param $ptr i32) (param $len i32)
    nop
  )

  ;; __plugin_free: stub.
  (func (export "__plugin_free") (param $ptr i32) (param $len i32)
    nop
  )
)
