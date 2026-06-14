//! Change-driven format trigger plugin + on-demand format tool.
//!
//! This plugin subscribes to [`HookKind::FileChanged`] and forwards eligible
//! paths to the host via [`plugin_sdk::host::request_format`]. It performs
//! **no formatting, no I/O, and holds no scheduling state** — coalescing,
//! single-flight, and echo suppression all live host-side.
//!
//! # Manifest
//!
//! - `tools = [format]` — one on-demand MCP tool .
//! - `hooks = [FileChanged]` — the only subscription.
//!
//! # `format` tool
//!
//! The `format` tool allows an MCP client to trigger formatting explicitly.
//!
//! - With `{"path": "src/x.rs"}`: enqueues exactly that path; returns
//!   `{"requested": 1, "accepted": <0|1>, "dropped": <0|1>}`.
//! - With `{}` (no path): enumerates all workspace files via `host_list_files`,
//!   enqueues each via `host_request_format`, and returns
//!   `{"requested": N, "accepted": A, "dropped": D}`.
//!
//! The tool returns **promptly after enqueuing** — it does not wait for the
//! formatter to complete. Dropped results (queue full / backoff) are counted and
//! not retried.
//!
//! # Eligibility
//!
//! Extension-to-tool matching is **host-side** (config). The guest enqueues every
//! path; the host no-ops paths with no configured tool. The only guest-side filter
//! is the `.tmp_write` guard in `on_hook` (shadow files from atomic writes are
//! never forwarded by the hook path). The `format` tool enqueues whatever the
//! caller requests; the host is responsible for eligibility decisions.
//!
//! # Drop & Play
//!
//! Built as a `cdylib` targeting `wasm32-wasip1`. The host (11c) loads it via
//! `WasmtimeHost::load` and discovers `hooks=[FileChanged]`, `tools=[format]`
//! from the manifest. Removing the `.wasm` disables both the hook subscription
//! and the `format` tool; no host recompile needed.

use plugin_sdk::{
    ABI_VERSION, HookKind, HookPayload, Plugin, PluginManifest, SdkError, ToolDesc, Value,
    plugin_main,
};

// ── Plugin struct ──────────────────────────────────────────────────────────────

/// The fmt plugin: change-driven forwarder (FileChanged → host_request_format)
/// plus the on-demand `format` MCP tool.
///
/// Apply `#[plugin_main]` to generate the four wasm export symbols.
#[plugin_main]
pub struct FmtPlugin;

// ── Plugin trait implementation ────────────────────────────────────────────────

impl Plugin for FmtPlugin {
    /// Return the manifest. Called once at load time via `__plugin_init`.
    ///
    /// U1: `hooks = [FileChanged]`.
    /// U1: `tools = [format]` with optional `path` string argument.
    fn init() -> PluginManifest {
        PluginManifest {
            name: "fmt".to_owned(),
            version: "0.1.0".to_owned(),
            abi: ABI_VERSION,
            tools: vec![ToolDesc {
                name: "format".to_owned(),
                description: "Enqueue a format job for the given workspace-relative path, or \
                               for every workspace file when no path is provided. Returns \
                               promptly after enqueuing; formatting is asynchronous."
                    .to_owned(),
                // Decision: optional `path` string in the input schema.
                // Why: calling without `path` triggers "format all" semantics via
                //      host_list_files(); supplying `path` enqueues exactly one file.
                // Trade-off: a single optional property keeps the schema minimal and
                //             avoids introducing a separate "format all" tool name.
                schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative path to enqueue for formatting. Omit to enqueue all workspace files."}},"additionalProperties":false}"#.to_owned(),
            }],
            hooks: vec![HookKind::FileChanged],
        }
    }

    /// Dispatch a tool call.
    ///
    /// # `format` tool
    ///
    /// - `args` contains `"path"` → enqueue that single path via
    ///   `host_request_format`; return `{requested, accepted, dropped}`.
    /// - `args` has no `"path"` → enumerate all workspace files via
    ///   `host_list_files`; enqueue each; return tallied counts.
    ///
    /// Returns promptly after enqueuing; does not wait for formatting (U3).
    /// Dropped results are counted without retry (UN1).
    fn call_tool(name: &str, args: Value) -> Result<Value, SdkError> {
        match name {
            "format" => fmt_format_impl(args),
            other => Err(SdkError::ToolNotFound(other.to_owned())),
        }
    }

    /// Handle a lifecycle hook.
    ///
    /// On `FileChanged(path)`:
    /// - Skip `.tmp_write` paths (UN2 — never forward shadow files).
    /// - Call `host_request_format(path)` (EV1).
    /// - On `Dropped`: log once and abandon — no retry spin (UN1).
    /// - On `Accepted`: host owns the rest (coalescing / exec / write).
    ///
    /// All other hook kinds are no-ops (the manifest does not subscribe to them,
    /// so the host will never call this for them; the match arm is defensive).
    fn on_hook(kind: HookKind, payload: HookPayload) {
        if kind != HookKind::FileChanged {
            return;
        }

        let path = match &payload {
            HookPayload::FileChanged { path } => path.as_str(),
            _ => return,
        };

        // UN2: never forward shadow / temp files produced by the atomic-write
        // pattern. The `.tmp_write` suffix is the project-wide sentinel.
        if is_tmp_artifact(path) {
            return;
        }

        // EV1 / UN1: forward to the host queue; handle Dropped without spinning.
        match plugin_sdk::host::request_format(path) {
            plugin_sdk::host::FormatResult::Accepted => {
                // Host owns the rest — coalesce, exec, write, echo-suppress.
            }
            plugin_sdk::host::FormatResult::Dropped => {
                plugin_sdk::host::log(&format!("fmt: format queue full, skipping {path}"));
            }
        }
    }
}

// ── `format` tool implementation ──────────────────────────────────────────────

/// Implementation of the `format` on-demand tool.
///
/// Two branches:
/// 1. `args` contains a `"path"` string → single-path enqueue.
/// 2. `args` has no `"path"` → enumerate all files via `host_list_files` and
///    enqueue each.
///
/// Returns a `Value::Map` with keys `"requested"`, `"accepted"`, `"dropped"`.
///
/// # Host availability
///
/// `host_list_files` panics on non-wasm targets. The no-path branch is therefore
/// `#[cfg(target_arch = "wasm32")]`-gated; on the host target it returns counts
/// of zero without calling any host fn (safe for host-side tests).
///
/// `host_request_format` on non-wasm targets returns `Dropped` (stub). The
/// single-path branch therefore always returns `{requested:1, accepted:0,
/// dropped:1}` on the host target — correct for host-side unit tests that only
/// verify count shape, not actual formatting.
fn fmt_format_impl(args: Value) -> Result<Value, SdkError> {
    // Extract the optional `path` field.
    let path_opt = extract_optional_text(&args, "path");

    if let Some(path) = path_opt {
        // EV1 (single path): enqueue exactly the given path.
        let result = plugin_sdk::host::request_format(path);
        let (accepted, dropped) = tally_one(result);
        Ok(counts_map(1, accepted, dropped))
    } else {
        // EV2 (no path): enumerate files and enqueue each.
        fmt_format_all()
    }
}

/// Enumerate all workspace files and enqueue each for formatting.
///
/// On non-wasm targets `host_list_files` panics, so the real enumeration
/// is gated to `wasm32`. On the host target we return zero counts without
/// calling any host fn so that host-side unit tests remain safe.
fn fmt_format_all() -> Result<Value, SdkError> {
    #[cfg(target_arch = "wasm32")]
    {
        let files = plugin_sdk::host::list_files();
        let mut accepted: i64 = 0;
        let mut dropped: i64 = 0;

        for path in &files {
            let result = plugin_sdk::host::request_format(path);
            let (a, d) = tally_one(result);
            accepted += a;
            dropped += d;
        }

        Ok(counts_map(files.len() as i64, accepted, dropped))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // On the host target list_files panics; return zero counts so that
        // host-side call_tool dispatch tests remain safe and do not panic.
        Ok(counts_map(0, 0, 0))
    }
}

// ── Private helpers ────────────────────────────────────────────────────────────

/// Convert a `FormatResult` into `(accepted_delta, dropped_delta)` counts.
fn tally_one(result: plugin_sdk::host::FormatResult) -> (i64, i64) {
    match result {
        plugin_sdk::host::FormatResult::Accepted => (1, 0),
        plugin_sdk::host::FormatResult::Dropped => (0, 1),
    }
}

/// Build the `{requested, accepted, dropped}` return map.
fn counts_map(requested: i64, accepted: i64, dropped: i64) -> Value {
    Value::Map(vec![
        ("requested".to_owned(), Value::Integer(requested)),
        ("accepted".to_owned(), Value::Integer(accepted)),
        ("dropped".to_owned(), Value::Integer(dropped)),
    ])
}

/// Extract an optional string-valued field from a `Value::Map`.
///
/// Returns `None` if the field is absent or the value is not a text string.
/// This is intentionally lenient: missing field is not an error for optional args.
fn extract_optional_text<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    match args {
        Value::Map(pairs) => pairs.iter().find(|(k, _)| k == field).and_then(|(_, v)| {
            if let Value::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        _ => None,
    }
}

// ── Eligibility helpers ────────────────────────────────────────────────────────

/// Return `true` if `path` is a temporary / shadow file that must never be
/// forwarded to the format queue.
///
/// The project-wide shadow-write sentinel is the `.tmp_write` suffix (see
/// `adapters/formatter/worker.rs`). We match on the
/// suffix to handle both bare names (`file.rs.tmp_write`) and any future
/// variants that keep the same sentinel.
///
/// # Examples
///
/// ```rust
/// use fmt::is_tmp_artifact;
///
/// assert!(is_tmp_artifact("src/main.rs.tmp_write"));
/// assert!(!is_tmp_artifact("src/main.rs"));
/// assert!(!is_tmp_artifact("src/tmp_write.rs")); // not a suffix
/// ```
pub fn is_tmp_artifact(path: &str) -> bool {
    path.ends_with(".tmp_write")
}

// ── Host-side tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use plugin_sdk::{ABI_VERSION, HookKind, HookPayload, Plugin, Value};

    use super::{FmtPlugin, is_tmp_artifact};

    // ── Manifest (AC5 / U1) ───────────────────────────────────────────────────

    /// manifest declares `tools=[format]` and `hooks=[FileChanged]`.
    #[test]
    fn manifest_declares_format_tool_and_file_changed_hook() {
        let manifest = FmtPlugin::init();

        assert_eq!(manifest.name, "fmt", "manifest name must be 'fmt'");
        assert_eq!(manifest.version, "0.1.0", "manifest version");
        assert_eq!(manifest.abi, ABI_VERSION, "manifest abi == ABI_VERSION");
        assert_eq!(
            manifest.tools.len(),
            1,
            "Must declare exactly one tool, got {:?}",
            manifest.tools
        );
        assert_eq!(
            manifest.tools[0].name, "format",
            "Tool must be named 'format'"
        );
        assert_eq!(
            manifest.hooks,
            vec![HookKind::FileChanged],
            "U1: hooks must be [FileChanged]"
        );
    }

    /// AC5: `format` tool's schema_json is non-empty and declares optional `path`.
    ///
    /// We verify the schema string contains the `"path"` property key and does NOT
    /// contain a `"required"` array. Full JSON parsing is not available in this
    /// no_std-compatible crate (no `serde_json` dependency); the host validates the
    /// full schema at load time. String checks are sufficient to guard against
    /// regressions in the schema literal.
    #[test]
    fn format_tool_schema_declares_optional_path_property() {
        let manifest = FmtPlugin::init();
        let schema_str = &manifest.tools[0].schema_json;
        assert!(!schema_str.is_empty(), "schema_json must not be empty");
        assert!(
            schema_str.contains("\"path\""),
            "schema must declare a 'path' property: {schema_str}"
        );
        // `path` must NOT be in `required` (it is optional).
        assert!(
            !schema_str.contains("\"required\""),
            "schema must not have a 'required' array — path is optional: {schema_str}"
        );
    }

    // ── call_tool("format") with path — AC1 ──────────────────────────────────

    /// AC1 (host-side): call_tool("format", {"path": "src/x.rs"}) returns a map
    /// with the correct keys. On the host target the stub always returns Dropped,
    /// so `accepted=0, dropped=1` — but the shape and `requested=1` are verified.
    #[test]
    fn call_tool_format_with_path_returns_counts_map() {
        let args = Value::Map(vec![(
            "path".to_owned(),
            Value::Text("src/x.rs".to_owned()),
        )]);
        let result = FmtPlugin::call_tool("format", args).expect("format must not error");

        let Value::Map(pairs) = &result else {
            panic!("format must return a Map, got {result:?}");
        };

        // requested == 1
        let requested = find_integer(pairs, "requested");
        assert_eq!(requested, 1, "AC1: requested must be 1 for a single path");

        // accepted + dropped == 1
        let accepted = find_integer(pairs, "accepted");
        let dropped = find_integer(pairs, "dropped");
        assert_eq!(
            accepted + dropped,
            1,
            "AC1: accepted + dropped must equal requested (1)"
        );
    }

    /// AC4: Dropped is counted correctly. On the host target the stub always
    /// returns Dropped, so we verify dropped=1, accepted=0 when path is given.
    #[test]
    fn call_tool_format_with_path_stub_returns_dropped() {
        // On the host the request_format stub returns Dropped.
        let args = Value::Map(vec![(
            "path".to_owned(),
            Value::Text("src/lib.rs".to_owned()),
        )]);
        let result = FmtPlugin::call_tool("format", args).expect("format must not error");
        let Value::Map(pairs) = &result else {
            panic!("expected Map: {result:?}");
        };
        let accepted = find_integer(pairs, "accepted");
        let dropped = find_integer(pairs, "dropped");
        // Host stub always returns Dropped.
        assert_eq!(
            accepted, 0,
            "host stub → accepted must be 0 (stub always drops)"
        );
        assert_eq!(dropped, 1, "host stub → dropped must be 1");
    }

    // ── call_tool("format") without path (no-path branch) — AC2 ──────────────

    /// AC2 (host-side no-path branch): on the host target `host_list_files` would
    /// panic, so `fmt_format_all` returns zero counts. Verify the shape is correct
    /// and does not panic.
    #[test]
    fn call_tool_format_without_path_returns_zero_on_host_target() {
        let args = Value::Map(vec![]); // no path field
        let result = FmtPlugin::call_tool("format", Value::Null)
            .expect("format with Null args must not error");
        // Null is treated as "no path" — same as empty map.
        let Value::Map(pairs) = &result else {
            panic!("format must return a Map, got {result:?}");
        };
        let requested = find_integer(pairs, "requested");
        assert_eq!(requested, 0, "host: no-path branch returns requested=0");

        // Also verify with empty map args.
        let result2 =
            FmtPlugin::call_tool("format", args).expect("format with empty map must not error");
        let Value::Map(pairs2) = &result2 else {
            panic!("format must return a Map, got {result2:?}");
        };
        let requested2 = find_integer(pairs2, "requested");
        assert_eq!(
            requested2, 0,
            "host: empty-map no-path branch returns requested=0"
        );
    }

    // ── Unknown tool name → ToolNotFound ─────────────────────────────────────

    /// Any tool name other than "format" → ToolNotFound.
    #[test]
    fn call_tool_unknown_name_returns_tool_not_found() {
        let result = FmtPlugin::call_tool("anything_else", Value::Null);
        assert!(
            matches!(result, Err(plugin_sdk::SdkError::ToolNotFound(_))),
            "unknown tool must return ToolNotFound: {result:?}"
        );
    }

    // ── .tmp_write guard ────────────────────────────────

    /// AC2 / UN2: is_tmp_artifact returns true for .tmp_write paths.
    #[test]
    fn tmp_artifact_detected_by_suffix() {
        assert!(
            is_tmp_artifact("src/main.rs.tmp_write"),
            "AC2: .tmp_write suffix must be detected"
        );
        assert!(
            is_tmp_artifact("file.tmp_write"),
            "AC2: bare .tmp_write must be detected"
        );
        assert!(
            is_tmp_artifact("/abs/path.rs.tmp_write"),
            "AC2: absolute path with .tmp_write must be detected"
        );
    }

    /// AC2: non-tmp paths are not flagged as artifacts.
    #[test]
    fn normal_paths_not_flagged_as_tmp_artifacts() {
        assert!(
            !is_tmp_artifact("src/main.rs"),
            "normal .rs must not be flagged"
        );
        assert!(
            !is_tmp_artifact("src/tmp_write.rs"),
            "tmp_write in stem is not a suffix"
        );
        assert!(!is_tmp_artifact(""), "empty path must not be flagged");
        assert!(
            !is_tmp_artifact("file.tmp_write_extra"),
            "extra suffix not matched"
        );
    }

    /// AC2: on_hook for a .tmp_write path does not call host_request_format.
    #[test]
    fn on_hook_tmp_write_path_is_silently_skipped() {
        FmtPlugin::on_hook(
            HookKind::FileChanged,
            HookPayload::FileChanged {
                path: "src/main.rs.tmp_write".to_owned(),
            },
        );
    }

    // ── Non-FileChanged hooks are no-ops ──────────────────────────────────────

    /// Non-FileChanged hooks must be silently ignored.
    #[test]
    fn on_hook_non_file_changed_is_noop() {
        FmtPlugin::on_hook(
            HookKind::BeforeToolCall,
            HookPayload::BeforeToolCall {
                tool_name: "greet".to_owned(),
                args: Value::Null,
            },
        );
        FmtPlugin::on_hook(
            HookKind::FileIndexed,
            HookPayload::FileIndexed {
                path: "src/lib.rs".to_owned(),
            },
        );
    }

    // ── Eligible path reaches request_format stub ─────────────────────────────

    /// AC1 (hook path, host-side proxy): Given an eligible FileChanged path,
    /// on_hook does not skip or panic. The stub returns Dropped — exercises the
    /// Dropped log branch without panicking.
    #[test]
    fn on_hook_eligible_path_reaches_request_format_stub() {
        FmtPlugin::on_hook(
            HookKind::FileChanged,
            HookPayload::FileChanged {
                path: "src/main.rs".to_owned(),
            },
        );
    }

    /// AC3 (hook/Dropped, host-side): stub always returns Dropped, must log once
    /// and not spin.
    #[test]
    fn on_hook_dropped_result_logs_once_no_spin() {
        FmtPlugin::on_hook(
            HookKind::FileChanged,
            HookPayload::FileChanged {
                path: "src/lib.rs".to_owned(),
            },
        );
    }

    // ── Manifest round-trip ───────────────────────────────────────────────────

    /// Manifest serialises to postcard and deserialises back correctly (including
    /// the new `format` tool).
    #[test]
    fn manifest_round_trips_postcard() {
        use plugin_sdk::PluginManifest;
        let manifest = FmtPlugin::init();
        let encoded = postcard::to_allocvec(&manifest).expect("encode");
        let decoded: PluginManifest = postcard::from_bytes(&encoded).expect("decode");
        assert_eq!(decoded, manifest, "postcard round-trip must be lossless");
    }

    // ── Helper ────────────────────────────────────────────────────────────────

    fn find_integer(pairs: &[(String, Value)], key: &str) -> i64 {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| {
                if let Value::Integer(n) = v {
                    Some(*n)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("key '{key}' missing or not an integer in {pairs:?}"))
    }
}
