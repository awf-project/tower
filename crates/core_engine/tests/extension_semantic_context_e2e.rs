#![cfg(unix)]
#![forbid(unsafe_code)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{
    EmbeddingFixture, EmbeddingReply, SEMANTIC_TOOLS, SemanticMcpHarness, invalid_semantic_startup,
};

fn configured_harness() -> (EmbeddingFixture, SemanticMcpHarness) {
    let provider = EmbeddingFixture::start("provider-response-sentinel");
    let mut harness = SemanticMcpHarness::start(true, Some(provider.endpoint()), None);
    harness.await_semantic_tools();
    (provider, harness)
}

fn hits(payload: &Value) -> &[Value] {
    payload["hits"].as_array().expect("semantic hits array")
}

fn reindex(harness: &mut SemanticMcpHarness) -> Value {
    harness.call("tower_semantic_reindex", json!({}))
}

#[test]
fn configured_semantic_search_returns_ranked_workspace_chunks_through_mcp() {
    let (provider, mut harness) = configured_harness();
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let result = harness.call(
        "tower_semantic_search",
        json!({"query":"alpha ranked","limit":5}),
    );
    let first = hits(&result).first().expect("ranked semantic result");
    assert_eq!(first["path"], "src.rs");
    assert!(first["start_byte"].as_u64().unwrap() <= first["end_byte"].as_u64().unwrap());
    assert!(first["snippet"].as_str().unwrap().len() <= 4096);
    assert!(first["score"].as_f64().unwrap().is_finite());
    assert_eq!(first["language"], "rust");
    assert!(first["reason"].is_string());
    assert!(
        harness.sqlite_path().is_file(),
        "real SQLite store was not opened"
    );
    assert!(
        !provider.requests().is_empty(),
        "real provider adapter was bypassed"
    );
}

#[test]
fn semantic_reindex_indexes_supported_workspace_files() {
    let (provider, mut harness) = configured_harness();
    let result = reindex(&mut harness);
    assert_eq!(result["status"], "ok");
    let expected = [
        ("alpha_ranked_symbol", "src.rs"),
        ("watcher_delete_fixture", "watcher.rs"),
        ("semantic notes", "notes.md"),
        ("alpha_peer", "src_z.rs"),
    ];
    assert_eq!(result["indexed_files"], expected.len() as u64);
    assert!(result["indexed_chunks"].as_u64().unwrap() >= expected.len() as u64);
    assert!(result["generation"].as_u64().unwrap() > 0);
    for (query, path) in expected {
        let result = harness.call(
            "tower_semantic_search",
            json!({"query":query,"limit":20,"path_prefixes":[path]}),
        );
        assert_eq!(result["status"], "ok");
        assert!(
            hits(&result).iter().any(|hit| hit["path"] == path),
            "supported file {path} must be searchable through the public index"
        );
    }
    assert!(harness.sqlite_path().is_file());
    assert!(provider.requests().len() >= 3);
}

#[test]
fn semantic_search_returns_ranked_chunks_with_provenance() {
    let (_provider, mut harness) = configured_harness();
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let result = harness.call(
        "tower_semantic_search",
        json!({"query":"watcher delete fixture","limit":5}),
    );
    let found = hits(&result);
    assert_eq!(
        found
            .iter()
            .map(|hit| hit["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["watcher.rs", "notes.md", "src.rs", "src_z.rs"],
        "fixture-controlled embeddings must retain deterministic public ranking"
    );
    for hit in found {
        let path = hit["path"].as_str().unwrap();
        assert!(!path.starts_with('/') && !path.contains(".."));
        let bytes = std::fs::read(harness.workspace.path().join(path)).unwrap();
        let start = hit["start_byte"].as_u64().unwrap() as usize;
        let end = hit["end_byte"].as_u64().unwrap() as usize;
        assert!(start <= end && end <= bytes.len());
        assert!(std::str::from_utf8(&bytes[start..end]).is_ok());
    }
}

#[test]
fn file_change_replaces_semantic_entries_for_path() {
    let (provider, mut harness) = configured_harness();
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let unaffected: BTreeMap<_, _> = [
        ("watcher_delete_fixture", "watcher.rs"),
        ("semantic notes", "notes.md"),
        ("alpha_peer", "src_z.rs"),
    ]
    .into_iter()
    .map(|(query, path)| {
        let result = harness.call(
            "tower_semantic_search",
            json!({"query":query,"limit":1,"path_prefixes":[path]}),
        );
        let hit = hits(&result)
            .first()
            .expect("unaffected path must be indexed");
        assert_eq!(hit["path"], path);
        (path, hit.clone())
    })
    .collect();
    harness.write_file("src.rs", "pub fn replacement_only_symbol() {}\n");
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let stale = harness.call(
        "tower_semantic_search",
        json!({"query":"alpha_ranked_symbol","limit":20}),
    );
    assert!(!hits(&stale).iter().any(|hit| {
        hit["snippet"]
            .as_str()
            .unwrap()
            .contains("alpha_ranked_symbol")
    }));
    assert!(hits(&stale).iter().any(|hit| {
        hit["path"] == "src.rs"
            && hit["snippet"]
                .as_str()
                .unwrap()
                .contains("replacement_only_symbol")
    }));
    let current: BTreeMap<_, _> = [
        ("watcher_delete_fixture", "watcher.rs"),
        ("semantic notes", "notes.md"),
        ("alpha_peer", "src_z.rs"),
    ]
    .into_iter()
    .map(|(query, path)| {
        let result = harness.call(
            "tower_semantic_search",
            json!({"query":query,"limit":1,"path_prefixes":[path]}),
        );
        let hit = hits(&result)
            .first()
            .expect("unaffected path must remain indexed");
        assert_eq!(hit["path"], path);
        (path, hit.clone())
    })
    .collect();
    assert_eq!(
        current, unaffected,
        "every unaffected path is retained exactly"
    );
    assert!(harness.sqlite_path().is_file());
    assert!(provider.requests().len() >= 2);
}

#[test]
fn file_deletion_removes_semantic_entries_for_path() {
    let (provider, mut harness) = configured_harness();
    assert_eq!(reindex(&mut harness)["status"], "ok");
    harness.remove_file("watcher.rs");
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let result = harness.call(
        "tower_semantic_search",
        json!({"query":"watcher_delete_fixture","limit":20}),
    );
    assert!(!hits(&result).iter().any(|hit| hit["path"] == "watcher.rs"));
    assert!(harness.sqlite_path().is_file());
    assert!(!provider.requests().is_empty());
}

#[test]
fn provider_failures_return_stable_errors_and_preserve_liveness() {
    let (status, stderr) = support::invalid_semantic_startup(
        "[semantic_context]\nenabled=true\n[semantic_context.timeouts]\nreindex_ms=0\n",
    );
    assert!(!status.success());
    assert!(
        stderr.contains(
            "invalid semantic context configuration semantic_context.timeouts.reindex_ms: Zero"
        ),
        "invalid semantic host classification missing: {stderr}"
    );

    let init_provider = EmbeddingFixture::start("initialization-failure-sentinel");
    let mut failed_init = SemanticMcpHarness::start_with_store_collision(init_provider.endpoint());
    let deadline = Instant::now() + Duration::from_secs(10);
    let init_error = loop {
        let frame =
            failed_init.call_frame("tower_semantic_search", json!({"query":"alpha","limit":1}));
        if frame["result"]["structuredContent"]
            == json!({"dependency":"semantic_context","failure_class":"extension_protocol"})
        {
            break frame;
        }
        assert!(
            Instant::now() < deadline,
            "store collision never surfaced CapabilityInitializationFailed: {frame}"
        );
        thread::yield_now();
    };
    assert_eq!(init_error["result"]["isError"], true, "{init_error}");
    assert_eq!(
        init_error["result"]["content"][0]["text"], "capability initialization failed",
        "{init_error}"
    );
    assert_eq!(
        init_error["result"]["structuredContent"],
        json!({"dependency":"semantic_context","failure_class":"extension_protocol"}),
        "store collision must retain the typed capability initialization failure"
    );

    let (provider, mut harness) = configured_harness();
    for (reply, code) in [
        (EmbeddingReply::Unavailable, "provider_unavailable"),
        (EmbeddingReply::Malformed, "provider_invalid_response"),
        (
            EmbeddingReply::MissingEmbedding,
            "provider_invalid_response",
        ),
        (EmbeddingReply::NonFinite, "provider_invalid_response"),
        (
            EmbeddingReply::DimensionMismatch,
            "provider_dimension_mismatch",
        ),
    ] {
        provider.enqueue([reply.clone()]);
        let result = harness.call("tower_semantic_search", json!({"query":"alpha","limit":1}));
        assert_eq!(result["status"], "error", "{reply:?}: {result}");
        assert_eq!(result["code"], code, "{reply:?}: {result}");
    }
    let (held, release) = EmbeddingFixture::held_reply();
    provider.enqueue([held]);
    EmbeddingFixture::release(&release);
    assert_eq!(reindex(&mut harness)["status"], "ok");
    assert_eq!(
        harness.call("tower_semantic_search", json!({"query":"alpha","limit":1}))["status"],
        "ok"
    );
    let listed = harness.list_tools();
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == SEMANTIC_TOOLS[1])
    );

    for (search_ms, provider_ms, code, operation, layer, budget) in [
        (2000, 100, "provider_timeout", "embedding", "provider", 100),
        (100, 2000, "operation_timeout", "search", "semantic", 100),
    ] {
        let provider = EmbeddingFixture::start("deadline-private-sentinel");
        let mut harness = SemanticMcpHarness::start_with_budgets(
            true,
            Some(provider.endpoint()),
            None,
            search_ms,
            provider_ms,
        );
        harness.await_semantic_tools();
        let (held, release) = EmbeddingFixture::held_reply();
        provider.enqueue([held]);
        let before = provider.requests().len();
        let request =
            harness.send_call("tower_semantic_search", json!({"query":"alpha","limit":1}));
        provider.wait_for_requests(before + 1);
        harness.advance_clock_ms(101);
        EmbeddingFixture::release(&release);
        let frame = harness.receive(request);
        let failed: Value =
            serde_json::from_str(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["code"], code);
        assert_eq!(
            failed["timeout"],
            json!({"operation":operation,"layer":layer,"budget_ms":budget})
        );
        assert_eq!(
            harness.call("tower_semantic_search", json!({"query":"alpha","limit":1}))["status"],
            "ok"
        );
        assert!(harness.list_tools()["result"]["tools"].is_array());
    }

    let provider = EmbeddingFixture::start("rebuild-expiry-sentinel");
    let mut capped = SemanticMcpHarness::start_with_budgets(
        true,
        Some(provider.endpoint()),
        Some(150),
        2_000,
        100,
    );
    capped.await_semantic_tools();
    let before = capped.call("tower_semantic_search", json!({"query":"alpha","limit":20}));
    let held: Vec<_> = (0..4).map(|_| EmbeddingFixture::held_reply()).collect();
    provider.enqueue(held.iter().map(|(reply, _)| reply.clone()));
    let seen = provider.requests().len();
    let request = capped.send_call("tower_semantic_reindex", json!({}));
    provider.wait_for_requests(seen + 1);
    capped.advance_clock_ms(80);
    EmbeddingFixture::release(&held[0].1);
    provider.wait_for_requests(seen + 2);
    capped.advance_clock_ms(160);
    EmbeddingFixture::release(&held[1].1);
    let frame = capped.receive(request);
    for (_, release) in &held[2..] {
        EmbeddingFixture::release(release);
    }
    let failed: Value =
        serde_json::from_str(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(failed["status"], "error");
    assert_eq!(failed["code"], "operation_timeout");
    assert_eq!(
        failed["timeout"],
        json!({"operation":"reindex","layer":"semantic","budget_ms":150})
    );
    assert_eq!(
        capped.call("tower_semantic_search", json!({"query":"alpha","limit":20})),
        before,
        "expired rebuild preserves the complete old generation"
    );

    let provider = EmbeddingFixture::start("uncapped-rebuild-sentinel");
    let mut uncapped =
        SemanticMcpHarness::start_with_budgets(true, Some(provider.endpoint()), None, 2_000, 100);
    uncapped.await_semantic_tools();
    let held: Vec<_> = (0..4).map(|_| EmbeddingFixture::held_reply()).collect();
    provider.enqueue(held.iter().map(|(reply, _)| reply.clone()));
    let seen = provider.requests().len();
    let request = uncapped.send_call("tower_semantic_reindex", json!({}));
    for (index, (_, release)) in held.iter().enumerate() {
        provider.wait_for_requests(seen + index + 1);
        uncapped.advance_clock_ms((index as u64 + 1) * 80);
        EmbeddingFixture::release(release);
    }
    let frame = uncapped.receive(request);
    let rebuilt: Value =
        serde_json::from_str(frame["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(rebuilt["status"], "ok");
    assert!(provider.requests().len() >= seen + 4);
    assert_eq!(
        uncapped.call("tower_semantic_search", json!({"query":"alpha","limit":5}))["status"],
        "ok"
    );
}

#[test]
fn related_files_are_ranked_and_deduplicated() {
    let (_provider, mut harness) = configured_harness();
    harness.write_file("src.rs", &"alpha ".repeat(2_000));
    harness.write_file("query.rs", "pub fn alpha_relation_source() {}\n");
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let result = harness.call(
        "tower_semantic_related_files",
        json!({"path":"query.rs","limit":5,"path_prefixes":["src"],"languages":["rust"]}),
    );
    assert_eq!(result["status"], "ok");
    let files = result["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let unique: BTreeSet<_> = files
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(unique.len(), files.len());
    assert!(
        files
            .iter()
            .all(|file| file["path"].as_str().unwrap().starts_with("src"))
    );
    assert!(
        files
            .windows(2)
            .all(|pair| pair[0]["score"].as_f64().unwrap() >= pair[1]["score"].as_f64().unwrap())
    );
    assert_eq!(files[0]["path"], "src.rs");
    assert_eq!(
        files[0]["score"].as_f64(),
        Some(1.0),
        "multiple candidate chunks for src.rs must retain that path's best score"
    );
}

#[test]
fn task_context_respects_budget_and_diversifies_sources() {
    let (_provider, mut harness) = configured_harness();
    assert_eq!(reindex(&mut harness)["status"], "ok");
    let result = harness.call(
        "tower_semantic_context_for_task",
        json!({"task":"alpha watcher notes","token_budget":40,"limit":3,"path_prefixes":[""],"languages":["rust","markdown"]}),
    );
    assert_eq!(result["status"], "ok");
    let chunks = result["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 3);
    assert!(result["estimated_tokens"].as_u64().unwrap() <= 40);
    assert!(result["truncated"].as_bool().unwrap());
    assert!(result["eligible_unfitted"].as_u64().unwrap() > 0);
    let paths: BTreeSet<_> = chunks
        .iter()
        .map(|chunk| chunk["path"].as_str().unwrap())
        .collect();
    assert!(paths.len() >= 2);
    let spans: BTreeSet<_> = chunks
        .iter()
        .map(|chunk| {
            (
                chunk["path"].to_string(),
                chunk["start_byte"].to_string(),
                chunk["end_byte"].to_string(),
            )
        })
        .collect();
    assert_eq!(spans.len(), chunks.len());

    let filtered = harness.call(
        "tower_semantic_context_for_task",
        json!({"task":"semantic notes","token_budget":40,"limit":3,"path_prefixes":["notes"],"languages":["markdown"]}),
    );
    assert_eq!(filtered["status"], "ok");
    let filtered_chunks = filtered["chunks"].as_array().unwrap();
    assert!(!filtered_chunks.is_empty());
    assert!(filtered_chunks.iter().all(|chunk| {
        chunk["path"].as_str().unwrap().starts_with("notes") && chunk["language"] == "markdown"
    }));

    let no_eligible = harness.call(
        "tower_semantic_context_for_task",
        json!({"task":"semantic notes","token_budget":40,"limit":3,"path_prefixes":["src"],"languages":["markdown"]}),
    );
    assert_eq!(
        no_eligible,
        json!({"status":"ok","chunks":[],"estimated_tokens":0,"truncated":false,"eligible_unfitted":0})
    );
}

#[test]
fn semantic_host_frames_survive_parallel_public_mcp_calls() {
    let barrier = Arc::new(Barrier::new(20));
    let workers: Vec<_> = (0..20)
        .map(|iteration| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let provider = EmbeddingFixture::start("queue-safety-sentinel");
                let (held, release) = EmbeddingFixture::held_reply();
                provider.enqueue([held]);
                let mut harness = SemanticMcpHarness::start(true, Some(provider.endpoint()), None);
                barrier.wait();
                provider.wait_for_requests(1);
                let queued_list =
                    harness.send_call("tower_semantic_search", json!({"query":"alpha","limit":1}));
                EmbeddingFixture::release(&release);
                let queued_response = harness.receive(queued_list);
                assert_eq!(queued_response["jsonrpc"], "2.0", "iteration {iteration}");
                harness.await_semantic_tools();
                harness.await_semantic_observation("snapshot-event-delivered");
                harness.await_semantic_observation("catch-up-fence-complete");
                harness.write_file(
                    &format!("live-{iteration}.rs"),
                    &format!("pub fn fresh_{iteration}() {{}}\n"),
                );
                harness.await_semantic_observation("live-event-delivered");
                assert_eq!(
                    reindex(&mut harness)["status"],
                    "ok",
                    "iteration {iteration}"
                );
                let result = harness.call(
                    "tower_semantic_search",
                    json!({"query":format!("fresh_{iteration}"),"limit":1}),
                );
                let fresh = hits(&result)
                    .first()
                    .unwrap_or_else(|| panic!("iteration {iteration}: fresh result missing"));
                assert_eq!(fresh["path"], format!("live-{iteration}.rs"));
                assert!(harness.list_tools()["result"]["tools"].is_array());
                assert!(harness.sqlite_path().is_file(), "iteration {iteration}");
                assert!(provider.requests().len() >= 2, "iteration {iteration}");
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("parallel semantic MCP iteration");
    }
}

#[test]
fn semantic_public_results_and_logs_do_not_expose_confidential_inputs() {
    let provider = EmbeddingFixture::start("provider-response-private-sentinel");
    let endpoint_with_userinfo =
        provider.endpoint_with_userinfo("endpoint-userinfo-private-sentinel");
    let (userinfo_status, userinfo_stderr) = invalid_semantic_startup(&format!(
        "[semantic_context]\nenabled=true\n[semantic_context.embedding]\nbase_url={endpoint_with_userinfo:?}\n"
    ));
    assert!(!userinfo_status.success());
    assert!(userinfo_stderr.contains("UrlCredentialsForbidden"));
    let mut harness = SemanticMcpHarness::start(true, Some(provider.endpoint()), None);
    harness.write_file("private.rs", "permitted_result_snippet\n");
    harness.write_file("confidential.rs", "source-private-sentinel\n");
    harness.await_semantic_tools();
    let reindexed = reindex(&mut harness);
    let result = harness.call(
        "tower_semantic_search",
        json!({"query":"permitted_result_snippet","limit":5,"path_prefixes":["private.rs"]}),
    );
    provider.enqueue([EmbeddingReply::Malformed]);
    let provider_error = harness.call(
        "tower_semantic_search",
        json!({"query":"permitted_result_snippet","limit":1}),
    );
    assert_eq!(provider_error["code"], "provider_invalid_response");
    let json_rpc_error = harness.malformed_call_frame(json!({"name":7,"arguments":{}}));
    assert_eq!(json_rpc_error["error"]["code"], -32601, "{json_rpc_error}");
    assert!(json_rpc_error["result"].is_null());
    let daemon_stderr = harness.stderr();
    let rendered = format!(
        "{reindexed}{result}{provider_error}{json_rpc_error}{daemon_stderr}{userinfo_stderr}"
    );
    for secret in [
        "semantic-api-key-sentinel",
        "provider-response-private-sentinel",
        "source-private-sentinel",
        ".tower/semantic_context/index.sqlite3",
        "endpoint-userinfo-private-sentinel",
    ] {
        assert!(!rendered.contains(secret), "public payload leaked {secret}");
    }
    assert_eq!(hits(&result)[0]["path"], "private.rs");
    assert_eq!(hits(&result)[0]["snippet"], "permitted_result_snippet\n");
    let requests = provider.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("Bearer semantic-api-key-sentinel")),
        "API-key sentinel must reach its real producer boundary"
    );
    assert!(
        !requests.is_empty(),
        "privacy sentinel never reached the real provider"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("source-private-sentinel"))
    );
    // Raw provider requests are the credential producer; the fixture log is
    // deliberately metadata-only and is the observable no-leak boundary.
    assert!(
        requests
            .iter()
            .all(|request| request.contains("/embeddings"))
    );
    assert!(provider.logs().iter().all(|line| {
        !line.contains("semantic-api-key-sentinel")
            && !line.contains("provider-response-private-sentinel")
            && !line.contains("source-private-sentinel")
    }));
    assert!(daemon_stderr.contains("Embedding provider request failed"));
}
