;; Test fixture: a wasm module that imports a forbidden host function.
;;
;; This module imports `wasi_snapshot_preview1::fd_write` (a raw WASI syscall)
;; which is NOT in the tower_host capability allow-list. When loaded by
;; WasmtimeHost::load, the Linker must fail to link this module because the
;; import is absent from our explicit func_wrap allow-list (spec 11c AC3 / U2).
;;
;; The module also exports the four __plugin_* symbols so it looks like a valid
;; plugin to the loader — the failure must be at link time, not before.
;;
;; Decision: use wasi_snapshot_preview1::fd_write as the forbidden import.
;; Why: fd_write is a well-known WASI raw I/O syscall (raw file-descriptor write).
;; It is not wrapped by the capability linker and should be absent or stubbed
;; to a trap, proving that raw WASI I/O is not reachable.

(module
  ;; Import a forbidden WASI raw I/O function that is NOT in our allow-list.
  ;; The tower_host linker does not expose this; attempting to call it must fail.
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32))
  )

  ;; Minimal memory required for any wasm module.
  (memory (export "memory") 1)

  ;; __plugin_alloc: allocate guest memory (required by host protocol).
  (func (export "__plugin_alloc") (param $len i32) (result i32)
    ;; Return a fixed page-0 offset — not a real allocator, just enough
    ;; to satisfy the export requirement for the link-failure test.
    i32.const 64
  )

  ;; __plugin_init: return a pointer to a minimal (but invalid) manifest buffer.
  ;; The host should never reach this point if link failure occurs as expected.
  (func (export "__plugin_init") (result i32)
    i32.const 0
  )

  ;; __plugin_call_tool: call the forbidden fd_write import.
  ;; If the sandbox were broken, this would perform raw I/O.
  (func (export "__plugin_call_tool") (param $ptr i32) (param $len i32) (result i32)
    ;; Attempt to call the forbidden import.
    i32.const 1  ;; fd
    i32.const 0  ;; iovs ptr
    i32.const 0  ;; iovs_len
    i32.const 0  ;; nwritten ptr
    call $fd_write
    drop
    i32.const 0
  )

  ;; __plugin_on_hook: no-op stub.
  (func (export "__plugin_on_hook") (param $ptr i32) (param $len i32)
    nop
  )

  ;; __plugin_free: no-op stub.
  (func (export "__plugin_free") (param $ptr i32) (param $len i32)
    nop
  )
)
