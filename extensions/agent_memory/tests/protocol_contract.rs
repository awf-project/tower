use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

use agent_memory_extension::{
    AgentMemoryLimitsConfig, Clock, EnrichmentProvider, EnrichmentProviderError, EnrichmentRequest,
    EnrichmentResult, IdGenerator, MemoryApplication, MemoryId, MemoryRecord, MemoryStore,
    MemoryType, PersistenceMode, PrivacyAction, PrivacyFilter, PrivacyRule, Provenance, Revision,
    protocol::{
        self, ContextForTaskParams, ContextForTaskResult, ForgetParams, ForgetResult, GetParams,
        GetResult, RememberParams, RememberResult, SearchParams, SearchResult, TimelineParams,
        TimelineResult, ToolProtocolError, UpdateParams, UpdateResult,
    },
    runtime::RuntimeProvider,
};
use extension_protocol::{Capability, messages::*};
use serde_json::{Value, json};

const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
type DtoFixture = (Value, fn(Value) -> Result<Value, serde_json::Error>);

fn expected_declarations() -> [(&'static str, &'static str, Value); 7] {
    let provenance = json!({
        "type":"object",
        "properties":{
            "branch":{"type":["string","null"]}, "paths":{"type":"array","items":{"type":"string"},"default":[]},
            "symbols":{"type":"array","items":{"type":"string"},"default":[]}, "tags":{"type":"array","items":{"type":"string"},"default":[]},
            "source_agent":{"type":["string","null"]}, "source_client":{"type":["string","null"]},
            "commit":{"type":["string","null"]}, "pull_request":{"type":["string","null"]}, "issue":{"type":["string","null"]}
        },
        "additionalProperties":false
    });
    let record = json!({
        "type":"object",
        "properties":{
            "record_type":{"enum":["observation","decision","bugfix","review_rule","task_note","session_summary"]},
            "title":{"type":"string"}, "body":{"type":"string"}, "provenance":provenance,
            "confidence":{"type":["number","null"],"minimum":0,"maximum":1}
        },
        "required":["record_type","title","body","provenance"],
        "additionalProperties":false
    });
    let filters = json!({
        "type":"object",
        "properties":{
            "record_types":{"type":"array","items":{"enum":["observation","decision","bugfix","review_rule","task_note","session_summary"]},"default":[]},
            "branches":{"type":"array","items":{"type":"string"},"default":[]}, "paths":{"type":"array","items":{"type":"string"},"default":[]},
            "symbols":{"type":"array","items":{"type":"string"},"default":[]}, "tags":{"type":"array","items":{"type":"string"},"default":[]},
            "source_agent":{"type":["string","null"]}, "source_client":{"type":["string","null"]},
            "created_from":{"type":["string","null"],"format":"date-time"}, "created_to":{"type":["string","null"],"format":"date-time"}
        },
        "additionalProperties":false
    });
    let page = json!({
        "type":"object",
        "properties":{"limit":{"type":["integer","null"],"minimum":1},"response_bytes":{"type":["integer","null"],"minimum":1},"cursor":{"type":["string","null"]}},
        "additionalProperties":false
    });
    let revision_item = json!({
        "type":"object","properties":{"id":{"type":"string"},"expected_revision":{"type":"string"}},
        "required":["id","expected_revision"],"additionalProperties":false
    });
    let update_item = json!({
        "type":"object","properties":{"id":{"type":"string"},"expected_revision":{"type":"string"},"record":record.clone()},
        "required":["id","expected_revision","record"],"additionalProperties":false
    });
    let timeline_anchor = json!({
        "oneOf":[
            {"type":"object","properties":{"kind":{"const":"record"},"id":{"type":"string"},"before":{"type":"integer","minimum":0},"after":{"type":"integer","minimum":0}},"required":["kind","id","before","after"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"const":"query"},"query":{"type":"string"}},"required":["kind","query"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"const":"window"},"from":{"type":"string","format":"date-time"},"to":{"type":"string","format":"date-time"}},"required":["kind","from","to"],"additionalProperties":false}
        ]
    });
    [
        (
            "memory_remember",
            "Store one durable memory record. Returns code, id, revision, persistence_mode, durable_data_unavailable, and durable; durable is true only after durable acknowledgement. Privacy-rule changes are non-retroactive and external backups/providers may retain prior data.",
            json!({"type":"object","properties":{"record":record.clone(),"idempotency_key":{"type":"string"}},"required":["record","idempotency_key"],"additionalProperties":false}),
        ),
        (
            "memory_update",
            "Update memory records independently. Returns ordered item outcomes plus persistence_mode, durable_data_unavailable, and durable; durable is true only after every successful item is durably acknowledged.",
            json!({"type":"object","properties":{"items":{"type":"array","minItems":1,"items":update_item}},"required":["items"],"additionalProperties":false}),
        ),
        (
            "memory_forget",
            "Logically delete memory records; deletion is not forensic erasure and external backups/providers may retain data. Returns ordered item outcomes and durability metadata.",
            json!({"type":"object","properties":{"items":{"type":"array","minItems":1,"items":revision_item}},"required":["items"],"additionalProperties":false}),
        ),
        (
            "memory_search",
            "Search compact hits, then use memory_get for batched full-record retrieval. Compact hits omit bodies. Returns hits, next_cursor, persistence_mode, and durable_data_unavailable.",
            json!({"type":"object","properties":{"query":{"type":"string"},"filters":filters.clone(),"page":page.clone()},"required":["query"],"additionalProperties":false}),
        ),
        (
            "memory_timeline",
            "Read a bounded half-open memory timeline. Returns compact events, next_cursor, persistence_mode, and durable_data_unavailable.",
            json!({"type":"object","properties":{"anchor":timeline_anchor,"filters":filters.clone(),"page":page.clone()},"required":["anchor"],"additionalProperties":false}),
        ),
        (
            "memory_get",
            "Batch-retrieve full records by ID after compact search. Returns ordered item outcomes, persistence_mode, and durable_data_unavailable.",
            json!({"type":"object","properties":{"ids":{"type":"array","minItems":1,"items":{"type":"string"}}},"required":["ids"],"additionalProperties":false}),
        ),
        (
            "memory_context_for_task",
            "Search compact task context without unrelated budget fillers. Returns hits, applied_filters, next_cursor, persistence_mode, and durable_data_unavailable; provider retention limits and non-retroactive privacy changes apply.",
            json!({"type":"object","properties":{"query":{"type":"string"},"filters":filters,"page":page},"required":["query"],"additionalProperties":false}),
        ),
    ]
}

/// The protocol DTOs are deliberately checked with literals rather than values
/// produced by the serializer under test.  This makes schema/DTO drift visible
/// before the runtime sidecar exists (T015 owns that composition).
#[test]
fn memory_protocol_dtos_are_strict_and_preserve_every_normative_field() {
    let provenance = json!({
        "branch":"feature/f017", "paths":["src/lib.rs"], "symbols":["Memory"],
        "tags":["contract"], "source_agent":"codex", "source_client":"tower",
        "commit":"abc", "pull_request":"17", "issue":"F017"
    });
    let record = json!({
        "record_type":"observation", "title":"title", "body":"body",
        "provenance":provenance, "confidence":0.5
    });
    let filters = json!({
        "record_types":["observation"], "branches":["feature/f017"],
        "paths":["src/lib.rs"], "symbols":["Memory"], "tags":["contract"],
        "source_agent":"codex", "source_client":"tower",
        "created_from":"2026-01-01T00:00:00Z", "created_to":"2026-01-02T00:00:00Z"
    });
    let page = json!({"limit":2,"response_bytes":512,"cursor":"cursor-1"});
    let hit = json!({
        "id":"id-1", "revision":"1", "record_type":"observation", "title":"title",
        "paths":["src/lib.rs"], "timestamp":"2026-01-01T00:00:00Z", "score":1.0,
        "reasons":[{"kind":"lexical","field":"title"}], "truncated_fields":[]
    });
    let stored_record = json!({
        "id":"id-1", "revision":"1", "schema_version":1, "record_type":"observation",
        "title":"title", "body":"body", "provenance":record["provenance"].clone(),
        "confidence":0.5, "project_key":"project", "created_at":"2026-01-01T00:00:00Z",
        "updated_at":"2026-01-01T00:00:00Z"
    });

    let params: [DtoFixture; 7] = [
        (
            json!({"record":record.clone(),"idempotency_key":"idem-1"}),
            |v| serde_json::from_value::<RememberParams>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"items":[{"id":"id-1","expected_revision":"1","record":record.clone()}]}),
            |v| serde_json::from_value::<UpdateParams>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"items":[{"id":"id-1","expected_revision":"1"}]}),
            |v| serde_json::from_value::<ForgetParams>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"query":"memory","filters":filters.clone(),"page":page.clone()}),
            |v| serde_json::from_value::<SearchParams>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"anchor":{"kind":"record","id":"id-1","before":0,"after":0},"filters":filters.clone(),"page":page.clone()}),
            |v| serde_json::from_value::<TimelineParams>(v).and_then(serde_json::to_value),
        ),
        (json!({"ids":["id-1","missing"]}), |v| {
            serde_json::from_value::<GetParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"query":"task","filters":filters,"page":page}), |v| {
            serde_json::from_value::<ContextForTaskParams>(v).and_then(serde_json::to_value)
        }),
    ];
    for (fixture, parse) in params {
        assert_eq!(
            parse(fixture.clone()).expect("request DTO must accept its literal fixture"),
            fixture,
            "request DTO serialization must preserve every normative field"
        );
    }

    let results: [DtoFixture; 7] = [
        (
            json!({"code":"ok","id":"id-1","revision":"1","persistence_mode":"durable","durable_data_unavailable":false,"durable":true}),
            |v| serde_json::from_value::<RememberResult>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"code":"ok","items":[{"id":"id-1","code":"ok","revision":"2","durable":true},{"id":"dupe","code":"duplicate_id","durable":false}],"persistence_mode":"durable","durable_data_unavailable":false,"durable":false}),
            |v| serde_json::from_value::<UpdateResult>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"code":"ok","items":[{"id":"id-1","code":"ok","revision":"2","durable":true},{"id":"missing","code":"not_found","durable":false}],"persistence_mode":"durable","durable_data_unavailable":false,"durable":false}),
            |v| serde_json::from_value::<ForgetResult>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"code":"ok","hits":[hit.clone()],"next_cursor":"cursor-2","persistence_mode":"durable","durable_data_unavailable":false}),
            |v| serde_json::from_value::<SearchResult>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"code":"ok","events":[hit.clone()],"next_cursor":null,"persistence_mode":"durable","durable_data_unavailable":false}),
            |v| serde_json::from_value::<TimelineResult>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"code":"ok","items":[{"id":"id-1","code":"ok","record":stored_record},{"id":"missing","code":"not_found"}],"persistence_mode":"durable","durable_data_unavailable":false}),
            |v| serde_json::from_value::<GetResult>(v).and_then(serde_json::to_value),
        ),
        (
            json!({"code":"ok","hits":[hit],"applied_filters":{"record_types":[],"branches":[],"paths":[],"symbols":[],"tags":[],"source_agent":null,"source_client":null,"created_from":null,"created_to":null},"next_cursor":null,"persistence_mode":"durable","durable_data_unavailable":false}),
            |v| serde_json::from_value::<ContextForTaskResult>(v).and_then(serde_json::to_value),
        ),
    ];
    for (fixture, parse) in results {
        assert_eq!(
            parse(fixture.clone()).expect("result DTO must accept its literal fixture"),
            fixture,
            "result DTO serialization must preserve every normative field"
        );
    }

    // Strictness is recursive: accepting either of these would leave a typo in
    // an application-owned field invisible at the public JSON boundary.
    for malformed in [
        json!({"record":{"record_type":"observation","title":"t","body":"b","provenance":{"paths":[],"nested_typo":true}},"idempotency_key":"k"}),
        json!({"items":[{"id":"id-1","code":"ok","record":{"id":"id-1","revision":"1","schema_version":1,"record_type":"observation","title":"t","body":"b","provenance":{},"project_key":"p","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","nested_typo":true}}],"persistence_mode":"durable","durable_data_unavailable":false}),
    ] {
        assert!(
            serde_json::from_value::<Value>(malformed.clone()).is_ok(),
            "fixture itself must be JSON"
        );
        let rejected = if malformed.get("record").is_some() {
            serde_json::from_value::<RememberParams>(malformed).is_err()
        } else {
            serde_json::from_value::<GetResult>(malformed).is_err()
        };
        assert!(rejected, "nested unknown fields must be InvalidParams");
    }

    for anchor in [
        json!({"kind":"query","query":"memory"}),
        json!({"kind":"window","from":"2026-01-01T00:00:00Z","to":"2026-01-02T00:00:00Z"}),
    ] {
        serde_json::from_value::<TimelineParams>(json!({"anchor":anchor})).unwrap();
    }
    for malformed_anchor in [
        json!({"kind":"record","id":"id","before":0}),
        json!({"kind":"query","query":"q","after":1}),
        json!({"kind":"window","from":"2026-01-01T00:00:00Z","to":"2026-01-02T00:00:00Z","before":1}),
    ] {
        assert!(
            serde_json::from_value::<TimelineParams>(json!({"anchor":malformed_anchor})).is_err()
        );
    }

    let required_and_unknown: [DtoFixture; 7] = [
        (json!({"idempotency_key":"k"}), |v| {
            serde_json::from_value::<RememberParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"items":[],"unknown":true}), |v| {
            serde_json::from_value::<UpdateParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"items":[],"unknown":true}), |v| {
            serde_json::from_value::<ForgetParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"unknown":true}), |v| {
            serde_json::from_value::<SearchParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"unknown":true}), |v| {
            serde_json::from_value::<TimelineParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"unknown":true}), |v| {
            serde_json::from_value::<GetParams>(v).and_then(serde_json::to_value)
        }),
        (json!({"unknown":true}), |v| {
            serde_json::from_value::<ContextForTaskParams>(v).and_then(serde_json::to_value)
        }),
    ];
    for (malformed, parse) in required_and_unknown {
        assert!(parse(malformed).is_err());
    }

    for invalid in [
        json!({"code":"ok","persistence_mode":"durable","durable_data_unavailable":false,"durable":true}),
        json!({"code":"invented","id":"id","revision":"1","persistence_mode":"durable","durable_data_unavailable":false,"durable":true}),
    ] {
        assert!(serde_json::from_value::<RememberResult>(invalid).is_err());
    }
    for invalid in [
        json!({"code":"ok","items":[{"id":"id","code":"ok","durable":true}],"persistence_mode":"durable","durable_data_unavailable":false,"durable":true}),
        json!({"code":"ok","items":[{"id":"id","code":"anything","durable":false}],"persistence_mode":"durable","durable_data_unavailable":false,"durable":false}),
    ] {
        assert!(serde_json::from_value::<UpdateResult>(invalid).is_err());
    }

    for code in [
        "not_found",
        "duplicate_id",
        "revision_conflict",
        "idempotency_conflict",
        "record_forgotten",
        "read_only",
        "storage_unavailable",
        "commit_outcome_unknown",
        "migration_failed",
        "incompatible_schema",
        "cursor_expired",
        "response_budget_too_small",
        "privacy_rejected",
    ] {
        serde_json::from_value::<RememberResult>(json!({"code":code,"persistence_mode":"durable","durable_data_unavailable":false,"durable":false})).unwrap();
    }
}

#[test]
fn agent_memory_initialize_result_declares_exact_tools_schemas_and_capabilities() {
    let init = protocol::initialize_result();
    assert_eq!(init.events, Vec::<String>::new());
    assert_eq!(init.capabilities, [Capability::AgentMemoryStorage]);
    assert_eq!(init.tools.len(), 7);

    for ((expected_name, expected_description, expected_schema), actual) in
        expected_declarations().into_iter().zip(&init.tools)
    {
        assert_eq!(actual.name, expected_name);
        assert_eq!(actual.description, expected_description);
        assert_eq!(
            serde_json::from_str::<Value>(&actual.schema_json).unwrap(),
            expected_schema
        );
    }

    assert_eq!(protocol::TOWER_MEMORY_REMEMBER, "tower_memory_remember");
    assert_eq!(protocol::TOWER_MEMORY_UPDATE, "tower_memory_update");
    assert_eq!(protocol::TOWER_MEMORY_FORGET, "tower_memory_forget");
    assert_eq!(protocol::TOWER_MEMORY_SEARCH, "tower_memory_search");
    assert_eq!(protocol::TOWER_MEMORY_TIMELINE, "tower_memory_timeline");
    assert_eq!(protocol::TOWER_MEMORY_GET, "tower_memory_get");
    assert_eq!(
        protocol::TOWER_MEMORY_CONTEXT_FOR_TASK,
        "tower_memory_context_for_task"
    );
}

#[test]
fn agent_memory_manifest_and_initialize_result_declare_identical_tools_and_capabilities() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/extension.toml"))
        .expect("read agent-memory manifest");
    let manifest: extension_protocol::ExtensionManifest =
        toml::from_str(&source).expect("agent-memory manifest must parse");
    let initialized = protocol::initialize_result();

    assert_eq!(manifest.name, "agent_memory");
    assert_eq!(manifest.command, ["agent_memory_extension"]);
    assert_eq!(manifest.tools.len(), initialized.tools.len());
    for (declared, canonical) in manifest.tools.iter().zip(&initialized.tools) {
        assert_eq!(declared.name, canonical.name);
        assert_eq!(declared.description, canonical.description);
        assert_eq!(
            serde_json::from_str::<Value>(&declared.schema_json).unwrap(),
            serde_json::from_str::<Value>(&canonical.schema_json).unwrap()
        );
    }
    let initialized_capabilities = initialized
        .capabilities
        .iter()
        .map(|capability| {
            serde_json::to_value(capability)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(manifest.capabilities.required, initialized_capabilities);
}

#[test]
fn malformed_memory_tool_params_return_invalid_params() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent_memory_extension"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn memory sidecar");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            tx.send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .ok();
        }
    });
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"invokeTool","params":{"name":"memory_remember","params":{"record":{},"idempotency_key":"k","unknown":true}}})).unwrap();
    stdin.flush().unwrap();
    let frame = rx
        .recv_timeout(FRAME_TIMEOUT)
        .expect("malformed request must receive an error envelope");
    assert_eq!(
        frame,
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}})
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "no late frame may follow the error"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Default)]
struct TestStore {
    state: Arc<Mutex<TestStoreState>>,
}
#[derive(Default)]
struct TestStoreState {
    values: BTreeMap<Vec<u8>, Vec<u8>>,
    generation: u64,
    next_failure: Option<AgentMemoryStorageFailure>,
    transaction_count: usize,
}
impl TestStore {
    fn fail_next(&self, failure: AgentMemoryStorageFailure) {
        self.state.lock().unwrap().next_failure = Some(failure);
    }
    fn transaction_count(&self) -> usize {
        self.state.lock().unwrap().transaction_count
    }
    fn put(&self, key: &[u8], value: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .values
            .insert(key.to_vec(), value.to_vec());
    }
}
impl MemoryStore for TestStore {
    fn get(
        &self,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageFailure> {
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        Ok(AgentMemoryGetResult {
            value: state.values.get(&request.key).cloned(),
            generation: state.generation,
        })
    }
    fn scan(
        &self,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageFailure> {
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        let mut entries = state
            .values
            .range(request.prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&request.prefix))
            .filter(|(key, _)| request.cursor.as_ref().is_none_or(|cursor| *key > cursor))
            .map(|(key, value)| AgentMemoryKeyValue {
                key: key.clone(),
                value: value.clone(),
            })
            .collect::<Vec<_>>();
        let has_more = entries.len() > request.limit as usize;
        entries.truncate(request.limit as usize);
        let next_cursor = has_more.then(|| entries.last().unwrap().key.clone());
        Ok(AgentMemoryScanResult {
            entries,
            next_cursor,
            generation: state.generation,
        })
    }
    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageFailure> {
        let mut state = self.state.lock().unwrap();
        state.transaction_count += 1;
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        let matches = request.conditions.iter().all(|condition| match condition {
            AgentMemoryCondition::KeyAbsent { key } => !state.values.contains_key(key),
            AgentMemoryCondition::ValueEquals { key, value } => {
                state.values.get(key) == Some(value)
            }
        });
        if !matches {
            return Ok(AgentMemoryTransactionOutcome::Conflict);
        }
        for mutation in request.mutations {
            match mutation {
                AgentMemoryMutation::Put { key, value } => {
                    state.values.insert(key, value);
                }
                AgentMemoryMutation::Delete { key } => {
                    state.values.remove(&key);
                }
            }
        }
        state.generation += 1;
        Ok(AgentMemoryTransactionOutcome::Committed {
            generation: state.generation,
            durable: true,
        })
    }
}
#[derive(Clone)]
struct FixedClock(Arc<AtomicU64>);
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.0.fetch_add(1, Ordering::SeqCst))
    }
    fn monotonic_now(&self) -> Duration {
        Duration::ZERO
    }
}
#[derive(Clone)]
struct FixedId(Arc<Mutex<u64>>);
impl IdGenerator for FixedId {
    fn next_id(&self) -> MemoryId {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        MemoryId::try_from(format!("id-{n}")).unwrap()
    }
}
#[derive(Clone)]
struct NoProvider;
impl EnrichmentProvider for NoProvider {
    fn enrich(
        &self,
        _: EnrichmentRequest,
        _: Duration,
        _: &AtomicBool,
    ) -> Result<EnrichmentResult, EnrichmentProviderError> {
        Err(EnrichmentProviderError::Unavailable)
    }
}

#[test]
fn memory_failures_return_stable_sanitized_payloads() {
    let store = TestStore::default();
    let app = MemoryApplication::new_with_privacy(
        store.clone(),
        FixedClock(Arc::new(AtomicU64::new(1_767_225_600))),
        FixedId(Arc::new(Mutex::new(0))),
        None::<NoProvider>,
        PrivacyFilter::default(),
        limits(),
    );
    let payload = protocol::handle_tool("memory_get", json!({"ids":["missing"]}), &app)
        .expect("not-found is a successful tool payload");
    assert_eq!(
        payload,
        json!({"code":"ok","items":[{"id":"missing","code":"not_found"}],"persistence_mode":"durable","durable_data_unavailable":false})
    );

    for malformed in [
        json!({"ids":[]}),
        json!({"ids":["id"],"unknown":true}),
        json!({"ids":[""]}),
    ] {
        assert_eq!(
            protocol::handle_tool("memory_get", malformed, &app),
            Err(ToolProtocolError::InvalidParams)
        );
    }

    let serialized = serde_json::to_string(&payload).unwrap();
    for forbidden in [
        "/home/",
        ".tower/agent_memory",
        "secret",
        "api_key",
        "backend",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "public failure leaked {forbidden}"
        );
    }

    // A real successful mutation is the baseline for the stable envelope. The
    // in-memory test store acknowledges the transaction durably, so this also
    // distinguishes per-item and aggregate durable fields.
    assert_eq!(
        protocol::handle_tool(
            "memory_remember",
            json!({"record":{"record_type":"observation","title":"title","body":"body","provenance":{}},"idempotency_key":"idem-1"}),
            &app,
        ),
        Ok(
            json!({"code":"ok","id":"id-1","revision":"1","persistence_mode":"durable","durable_data_unavailable":false,"durable":true})
        )
    );

    for (key, title, body) in [("idem-2", "title", "title"), ("idem-3", "later", "body")] {
        protocol::handle_tool(
            "memory_remember",
            json!({"record":{"record_type":"observation","title":title,"body":body,"provenance":{}},"idempotency_key":key}),
            &app,
        ).unwrap();
    }
    let timeline = |anchor| {
        protocol::handle_tool(
            "memory_timeline",
            json!({"anchor":anchor,"page":{"limit":100,"response_bytes":32768}}),
            &app,
        )
    };
    let equal_window = timeline(
        json!({"kind":"window","from":"2026-01-01T00:00:00Z","to":"2026-01-01T00:00:00Z"}),
    )
    .unwrap();
    assert_eq!(equal_window["events"], json!([]), "{equal_window}");
    assert_eq!(
        timeline(
            json!({"kind":"window","from":"2026-01-01T00:00:00Z","to":"2026-01-01T00:00:10Z"})
        )
        .unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["id-1", "id-2", "id-3"]
    );
    assert_eq!(
        timeline(
            json!({"kind":"window","from":"2026-01-01T00:00:02Z","to":"2026-01-01T00:00:01Z"})
        ),
        Err(ToolProtocolError::InvalidParams)
    );
    assert_eq!(
        timeline(json!({"kind":"record","id":"id-2","before":0,"after":0})).unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["id-2"]
    );
    assert_eq!(
        timeline(json!({"kind":"record","id":"id-2","before":1,"after":1})).unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["id-1", "id-2", "id-3"]
    );
    for invalid in [
        json!({"kind":"record","id":"id-2","before":0}),
        json!({"kind":"record","id":"id-2","before":100,"after":0}),
        json!({"kind":"record","id":"id-2","before":u32::MAX,"after":u32::MAX}),
    ] {
        assert_eq!(timeline(invalid), Err(ToolProtocolError::InvalidParams));
    }
    assert_eq!(
        timeline(json!({"kind":"query","query":"title"})).unwrap()["events"]
            .as_array()
            .unwrap()
            .first()
            .unwrap()["id"],
        "id-2",
        "query mode anchors at the first ranked match"
    );

    let duplicate_batch = protocol::handle_tool(
        "memory_update",
        json!({"items":[
            {"id":"id-2","expected_revision":"1","record":{"record_type":"observation","title":"first","body":"body","provenance":{}}},
            {"id":"id-3","expected_revision":"1","record":{"record_type":"observation","title":"valid","body":"body","provenance":{}}},
            {"id":"id-2","expected_revision":"1","record":{"record_type":"observation","title":"second","body":"body","provenance":{}}}
        ]}),
        &app,
    )
    .expect("duplicate IDs are stable item outcomes, not transport errors");
    assert_eq!(
        duplicate_batch["items"][0],
        json!({"id":"id-2","code":"duplicate_id","durable":false})
    );
    assert_eq!(
        duplicate_batch["items"][2],
        json!({"id":"id-2","code":"duplicate_id","durable":false})
    );
    assert_eq!(duplicate_batch["items"][1]["code"], "ok");
    let after_update =
        protocol::handle_tool("memory_get", json!({"ids":["id-2","id-3"]}), &app).unwrap();
    assert_eq!(
        after_update["items"][0]["record"]["revision"], "1",
        "duplicate update occurrences mutate nothing"
    );
    assert_eq!(
        after_update["items"][1]["record"]["revision"], "2",
        "valid sibling commits"
    );

    let before_forget_transactions = store.transaction_count();
    let duplicate_forget = protocol::handle_tool("memory_forget", json!({"items":[
        {"id":"id-2","expected_revision":"1"}, {"id":"id-1","expected_revision":"1"}, {"id":"id-2","expected_revision":"1"}
    ]}), &app).unwrap();
    assert_eq!(duplicate_forget["items"][0]["code"], "duplicate_id");
    assert_eq!(duplicate_forget["items"][2]["code"], "duplicate_id");
    assert_eq!(duplicate_forget["items"][1]["code"], "ok");
    assert_eq!(
        store.transaction_count(),
        before_forget_transactions + 1,
        "only the valid sibling mutates storage"
    );
    let after_forget =
        protocol::handle_tool("memory_get", json!({"ids":["id-2","id-1"]}), &app).unwrap();
    assert_eq!(after_forget["items"][0]["code"], "ok");
    assert_eq!(after_forget["items"][1]["code"], "not_found");

    let revision_conflict = protocol::handle_tool("memory_update", json!({"items":[{"id":"id-2","expected_revision":"wrong","record":{"record_type":"observation","title":"x","body":"x","provenance":{}}}]}), &app).unwrap();
    assert_sanitized_code(&revision_conflict["items"][0], "revision_conflict");
    let idempotency_conflict = protocol::handle_tool("memory_remember", json!({"record":{"record_type":"observation","title":"changed","body":"body","provenance":{}},"idempotency_key":"idem-2"}), &app).unwrap();
    assert_sanitized_code(&idempotency_conflict, "idempotency_conflict");
    let record_forgotten = protocol::handle_tool("memory_remember", json!({"record":{"record_type":"observation","title":"title","body":"body","provenance":{}},"idempotency_key":"idem-1"}), &app).unwrap();
    assert_sanitized_code(&record_forgotten, "record_forgotten");

    let read_only = MemoryApplication::new_configured(
        store.clone(),
        FixedClock(Arc::new(AtomicU64::new(0))),
        FixedId(Arc::new(Mutex::new(20))),
        None::<NoProvider>,
        PrivacyFilter::default(),
        limits(),
        PersistenceMode::ReadOnly,
        false,
        Duration::from_secs(1),
    );
    let read_only_payload = protocol::handle_tool("memory_remember", json!({"record":{"record_type":"observation","title":"title","body":"body","provenance":{}},"idempotency_key":"readonly"}), &read_only).unwrap();
    assert_sanitized_code(&read_only_payload, "read_only");

    let unavailable_store = TestStore::default();
    unavailable_store.fail_next(AgentMemoryStorageFailure::Unavailable);
    let unavailable_app = MemoryApplication::new(
        unavailable_store,
        FixedClock(Arc::new(AtomicU64::new(0))),
        FixedId(Arc::new(Mutex::new(30))),
        None::<NoProvider>,
    );
    assert_sanitized_code(
        &protocol::handle_tool("memory_get", json!({"ids":["id"]}), &unavailable_app).unwrap(),
        "storage_unavailable",
    );

    let unknown_store = TestStore::default();
    unknown_store.fail_next(AgentMemoryStorageFailure::CommitOutcomeUnknown);
    let unknown_app = MemoryApplication::new(
        unknown_store,
        FixedClock(Arc::new(AtomicU64::new(0))),
        FixedId(Arc::new(Mutex::new(40))),
        None::<NoProvider>,
    );
    assert_sanitized_code(&protocol::handle_tool("memory_remember", json!({"record":{"record_type":"observation","title":"title","body":"body","provenance":{}},"idempotency_key":"unknown"}), &unknown_app).unwrap(), "commit_outcome_unknown");

    let incompatible_store = TestStore::default();
    incompatible_store.put(b"meta/schema", br#"{"version":99}"#);
    let incompatible_app = MemoryApplication::new(
        incompatible_store,
        FixedClock(Arc::new(AtomicU64::new(0))),
        FixedId(Arc::new(Mutex::new(50))),
        None::<NoProvider>,
    );
    let incompatible = incompatible_app.initialize_store().unwrap_err();
    assert_sanitized_code(
        &protocol::map_application_error(
            incompatible,
            incompatible_app.persistence_status(),
            false,
        )
        .unwrap(),
        "incompatible_schema",
    );

    let migration_store = TestStore::default();
    migration_store.put(b"meta/schema", br#"{"version":0}"#);
    migration_store.fail_next(AgentMemoryStorageFailure::Unavailable);
    let migration_app = MemoryApplication::new(
        migration_store,
        FixedClock(Arc::new(AtomicU64::new(0))),
        FixedId(Arc::new(Mutex::new(60))),
        None::<NoProvider>,
    );
    let migration = migration_app.initialize_store().unwrap_err();
    assert_sanitized_code(
        &protocol::map_application_error(migration, migration_app.persistence_status(), false)
            .unwrap(),
        "migration_failed",
    );

    assert_sanitized_code(
        &protocol::handle_tool(
            "memory_search",
            json!({"query":"title","page":{"cursor":"invalid"}}),
            &app,
        )
        .unwrap(),
        "cursor_expired",
    );
    assert_sanitized_code(
        &protocol::handle_tool(
            "memory_search",
            json!({"query":"title","page":{"response_bytes":1}}),
            &app,
        )
        .unwrap(),
        "response_budget_too_small",
    );

    let privacy = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new("secret-rule".into(), "SECRET".into(), PrivacyAction::Reject).unwrap(),
    ])
    .unwrap();
    let privacy_app = MemoryApplication::new_with_privacy(
        TestStore::default(),
        FixedClock(Arc::new(AtomicU64::new(0))),
        FixedId(Arc::new(Mutex::new(70))),
        None::<NoProvider>,
        privacy,
        limits(),
    );
    assert_sanitized_code(&protocol::handle_tool("memory_remember", json!({"record":{"record_type":"observation","title":"SECRET","body":"body","provenance":{}},"idempotency_key":"privacy"}), &privacy_app).unwrap(), "privacy_rejected");
}

fn limits() -> AgentMemoryLimitsConfig {
    AgentMemoryLimitsConfig {
        max_title_bytes: 1024,
        max_body_bytes: 65_536,
        max_metadata_items: 100,
        max_metadata_string_bytes: 1024,
        max_batch_items: 100,
        default_results: 20,
        max_results: 100,
        default_response_bytes: 32_768,
        max_response_bytes: 131_072,
        max_query_bytes: 4096,
    }
}

fn assert_sanitized_code(payload: &Value, expected: &str) {
    assert_eq!(payload["code"], expected);
    let serialized = serde_json::to_string(payload).unwrap();
    for forbidden in [
        "/home/",
        ".tower/agent_memory",
        "secret",
        "api_key",
        "backend",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "public failure leaked {forbidden}: {serialized}"
        );
    }
}

fn spawn_protocol_sidecar() -> (
    std::process::Child,
    std::process::ChildStdin,
    mpsc::Receiver<Value>,
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent_memory_extension"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn memory sidecar");
    let stdin = child.stdin.take().expect("memory sidecar stdin");
    let stdout = child.stdout.take().expect("memory sidecar stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                break;
            };
            if tx.send(frame).is_err() {
                break;
            }
        }
    });
    (child, stdin, rx)
}

fn send_frame(stdin: &mut std::process::ChildStdin, frame: Value) {
    writeln!(stdin, "{frame}").expect("write protocol frame");
    stdin.flush().expect("flush protocol frame");
}

fn durable_init_config(provider: Option<Value>) -> Value {
    json!({
        "project_key":"project-test",
        "persistence_mode":"durable",
        "durable_data_unavailable":false,
        "limits":{
            "max_title_bytes":1024,"max_body_bytes":65536,
            "max_metadata_items":100,"max_metadata_string_bytes":1024,
            "max_batch_items":100,"default_results":20,"max_results":100,
            "default_response_bytes":32768,"max_response_bytes":131072,
            "max_query_bytes":4096
        },
        "privacy":{"rules":[]},
        "provider":provider
    })
}

/// A host-owned storage fixture.  It deliberately validates both halves of the
/// HostCall boundary: the RPC method chosen by the sidecar and the independently
/// decoded, typed request in `params`.  Reply selection must not be based only
/// on the tagged variant, otherwise a method/variant mismatch would pass.
struct MemoryHostFixture {
    values: BTreeMap<Vec<u8>, Vec<u8>>,
    generation: u64,
    calls: BTreeMap<String, usize>,
    migration_committed: bool,
}

impl MemoryHostFixture {
    fn empty() -> Self {
        Self {
            values: BTreeMap::new(),
            generation: 0,
            calls: BTreeMap::new(),
            migration_committed: false,
        }
    }

    fn legacy_schema_zero() -> Self {
        let mut fixture = Self::empty();
        fixture
            .values
            .insert(b"meta/schema".to_vec(), br#"{"version":0}"#.to_vec());
        // This is an independently constructed schema-0 record.  The normal
        // migration must add its projection and atomically publish schema 1.
        fixture.values.insert(
            b"record/bGVnYWN5".to_vec(),
            serde_json::to_vec(&json!({
                "id":"legacy", "revision":"1", "schema_version":0,
                "kind":"observation", "title":"legacy title", "body":"legacy body",
                "provenance":{"branch":null,"paths":[],"symbols":[],"tags":[],"source_agent":null,"source_client":null,"commit":null,"pull_request":null,"issue":null},
                "confidence":null, "project_key":"project-test",
                "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z",
                "idem_digest":""
            }))
            .expect("legacy fixture must serialize"),
        );
        fixture
    }

    fn reply(&mut self, stdin: &mut std::process::ChildStdin, frame: &Value) {
        let id = frame["id"].clone();
        let method = frame["method"]
            .as_str()
            .expect("storage HostCall must have an RPC method");
        let call: HostCall = serde_json::from_value(frame["params"].clone())
            .expect("storage HostCall params must be a typed HostCall");

        let result = match (method, call) {
            ("agent_memory/storage/get", HostCall::AgentMemoryStorageGet { request }) => {
                *self.calls.entry(method.to_owned()).or_default() += 1;
                json!({
                    "value": self.values.get(&request.key),
                    "generation": self.generation,
                })
            }
            ("agent_memory/storage/scan", HostCall::AgentMemoryStorageScan { request }) => {
                *self.calls.entry(method.to_owned()).or_default() += 1;
                let mut entries = self
                    .values
                    .range(request.prefix.clone()..)
                    .take_while(|(key, _)| key.starts_with(&request.prefix))
                    .filter(|(key, _)| request.cursor.as_ref().is_none_or(|cursor| *key > cursor))
                    .take(request.limit as usize + 1)
                    .map(|(key, value)| json!({"key":key,"value":value}))
                    .collect::<Vec<_>>();
                let has_more = entries.len() > request.limit as usize;
                entries.truncate(request.limit as usize);
                let next_cursor =
                    has_more.then(|| entries.last().expect("emitted entry exists")["key"].clone());
                json!({"entries":entries,"next_cursor":next_cursor,"generation":self.generation})
            }
            ("agent_memory/storage/transact", HostCall::AgentMemoryStorageTransact { request }) => {
                *self.calls.entry(method.to_owned()).or_default() += 1;
                let conditions_match = request.conditions.iter().all(|condition| match condition {
                    AgentMemoryCondition::KeyAbsent { key } => !self.values.contains_key(key),
                    AgentMemoryCondition::ValueEquals { key, value } => {
                        self.values.get(key) == Some(value)
                    }
                });
                if !conditions_match {
                    json!({"type":"Conflict"})
                } else {
                    self.migration_committed |= request.mutations.iter().any(|mutation| {
                        matches!(mutation, AgentMemoryMutation::Put { key, value }
                            if key == b"meta/schema" && value == br#"{"version":1}"#)
                    });
                    for mutation in request.mutations {
                        match mutation {
                            AgentMemoryMutation::Put { key, value } => {
                                self.values.insert(key, value);
                            }
                            AgentMemoryMutation::Delete { key } => {
                                self.values.remove(&key);
                            }
                        }
                    }
                    self.generation += 1;
                    json!({"type":"Committed","generation":self.generation,"durable":true})
                }
            }
            (wrong_method, HostCall::AgentMemoryStorageGet { .. }) => {
                panic!("get must use agent_memory/storage/get, got {wrong_method}")
            }
            (wrong_method, HostCall::AgentMemoryStorageScan { .. }) => {
                panic!("scan must use agent_memory/storage/scan, got {wrong_method}")
            }
            (wrong_method, HostCall::AgentMemoryStorageTransact { .. }) => {
                panic!("transact must use agent_memory/storage/transact, got {wrong_method}")
            }
            (method, other) => panic!("unexpected non-memory HostCall {method}: {other:?}"),
        };
        send_frame(stdin, json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
}

#[test]
fn host_frames_are_preserved_while_memory_host_calls_are_pending() {
    let (mut child, mut stdin, rx) = spawn_protocol_sidecar();
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocol_version":1,"client_info":"protocol-contract",
            "extension_config":durable_init_config(None)
        }}),
    );

    let first_host_call = rx
        .recv_timeout(FRAME_TIMEOUT)
        .expect("initialization must issue a bounded storage HostCall before Initialized");
    assert!(first_host_call.get("method").is_some());

    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"deliverEvent","params":{
            "event":"memory/runtimeBarrier","payload":{"stage":"initialization"}
        }}),
    );
    let mut storage = MemoryHostFixture::legacy_schema_zero();
    storage.reply(&mut stdin, &first_host_call);
    let mut replies = BTreeMap::<u64, Value>::new();
    let mut migration_frame_was_queued = false;
    while replies.len() < 3 {
        let frame = rx
            .recv_timeout(FRAME_TIMEOUT)
            .expect("queued initialization and migration frames must be replayed");
        if frame.get("method").is_some() {
            let call: HostCall = serde_json::from_value(frame["params"].clone())
                .expect("storage HostCall params must be a typed HostCall");
            let is_migration = matches!(
                call,
                HostCall::AgentMemoryStorageTransact { ref request }
                    if request.mutations.iter().any(|mutation| {
                        matches!(mutation, AgentMemoryMutation::Put { key, value }
                            if key == b"meta/schema" && value == br#"{"version":1}"#)
                    })
            );
            if is_migration {
                send_frame(
                    &mut stdin,
                    json!({"jsonrpc":"2.0","id":3,"method":"deliverEvent","params":{
                        "event":"memory/runtimeBarrier","payload":{"stage":"migration"}
                    }}),
                );
                migration_frame_was_queued = true;
            }
            storage.reply(&mut stdin, &frame);
        } else if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            if id == 1 {
                assert!(
                    storage.migration_committed,
                    "all schema-0 migration HostCalls must complete before Initialized"
                );
            }
            replies.insert(id, frame);
        }
    }

    assert_eq!(replies[&1]["result"]["type"], "Initialized");
    assert_eq!(replies[&2]["result"]["type"], "Ack");
    assert_eq!(replies[&3]["result"]["type"], "Ack");
    assert!(
        migration_frame_was_queued,
        "the schema migration HostCall must be held while a nonmatching frame arrives"
    );

    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":4,"method":"invokeTool","params":{
            "name":"memory_remember","params":{
                "record":{"record_type":"observation","title":"queued","body":"body","provenance":{},"confidence":null},
                "idempotency_key":"queued-during-ordinary-host-call"
            }
        }}),
    );
    let ordinary_host_call = rx
        .recv_timeout(FRAME_TIMEOUT)
        .expect("ordinary request must issue a storage HostCall");
    assert!(ordinary_host_call.get("method").is_some());
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":5,"method":"deliverEvent","params":{
            "event":"memory/runtimeBarrier","payload":{"stage":"ordinary-request"}
        }}),
    );
    storage.reply(&mut stdin, &ordinary_host_call);

    let mut ordinary_replies = BTreeMap::<u64, Value>::new();
    while ordinary_replies.len() < 2 {
        let frame = rx
            .recv_timeout(FRAME_TIMEOUT)
            .expect("queued ordinary-request frame and ToolResult must be replayed");
        if frame.get("method").is_some() {
            storage.reply(&mut stdin, &frame);
        } else {
            let id = frame["id"]
                .as_u64()
                .expect("queued ordinary-request responses must retain their IDs");
            assert!(
                ordinary_replies.insert(id, frame).is_none(),
                "each queued ordinary-request frame must be replayed exactly once"
            );
        }
    }
    assert_eq!(ordinary_replies[&4]["result"]["type"], "ToolResult");
    assert_eq!(
        ordinary_replies[&4]["result"]["data"]["persistence_mode"],
        "durable"
    );
    assert_eq!(ordinary_replies[&4]["result"]["data"]["durable"], true);
    assert_eq!(ordinary_replies[&5]["result"]["type"], "Ack");
    assert!(storage.calls.contains_key("agent_memory/storage/get"));
    assert!(storage.calls.contains_key("agent_memory/storage/scan"));
    assert!(storage.calls.contains_key("agent_memory/storage/transact"));
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "no duplicate or leaked frame may follow the replay milestone"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn provider_timeout_preserves_baseline_tool_availability() {
    // Exercise the production runtime adapter so the configured finite budget,
    // cancellation publication, and sanitized failure are observable.
    let provider = RuntimeProvider::new();
    let app = Arc::new(MemoryApplication::new_configured(
        TestStore::default(),
        FixedClock(Arc::new(AtomicU64::new(1_767_225_600))),
        FixedId(Arc::new(Mutex::new(0))),
        Some(provider),
        PrivacyFilter::default(),
        limits(),
        PersistenceMode::Durable,
        false,
        Duration::from_millis(25),
    ));
    let record = MemoryRecord::try_new(
        MemoryId::try_from("provider-record".to_owned()).unwrap(),
        Revision::try_from("1".to_owned()).unwrap(),
        1,
        MemoryType::Observation,
        "provider record".to_owned(),
        "provider body".to_owned(),
        Provenance::try_new(
            None,
            vec![],
            vec![],
            vec![],
            None,
            None,
            None,
            None,
            None,
            &limits(),
        )
        .unwrap(),
        None,
        "project-test".to_owned(),
        "2026-01-01T00:00:00Z".to_owned(),
        "2026-01-01T00:00:00Z".to_owned(),
        &limits(),
    )
    .unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let application = app.clone();
    let cancellation = cancelled.clone();
    let enrichment = std::thread::spawn(move || {
        application.run_enrichment(EnrichmentRequest { record }, cancellation.as_ref())
    });
    let started = Instant::now();
    assert_eq!(
        enrichment.join().expect("provider worker must join"),
        Err(agent_memory_extension::MemoryApplicationError::ProviderTimeout)
    );
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "the runtime provider must honor the deadline instead of timing out immediately"
    );
    assert!(cancelled.load(Ordering::Acquire));
    assert_eq!(
        protocol::map_application_error(
            agent_memory_extension::MemoryApplicationError::ProviderTimeout,
            (PersistenceMode::Durable, false),
            false,
        )
        .unwrap(),
        json!({"code":"storage_unavailable","persistence_mode":"durable","durable_data_unavailable":false}),
        "provider failures must remain fixed and sanitized"
    );

    // The process assertions independently prove that provider configuration
    // does not make baseline memory tools depend on provider availability.
    let (mut child, mut stdin, rx) = spawn_protocol_sidecar();
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":10,"method":"initialize","params":{
            "protocol_version":1,"client_info":"protocol-contract",
            "extension_config":durable_init_config(Some(json!({
                "name":"held-local-provider","timeout_ms":500
            })))
        }}),
    );

    let mut storage = MemoryHostFixture::empty();
    loop {
        let frame = rx
            .recv_timeout(FRAME_TIMEOUT)
            .expect("configured runtime must initialize within the process deadline");
        if frame.get("method").is_some() {
            storage.reply(&mut stdin, &frame);
        } else if frame.get("id") == Some(&json!(10)) {
            assert_eq!(frame["result"]["type"], "Initialized");
            break;
        }
    }

    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":13,"method":"invokeTool","params":{
            "name":"memory_remember","params":{
                "record":{"record_type":"observation","title":"baseline memory","body":"provider candidate","provenance":{},"confidence":null},
                "idempotency_key":"provider-timeout-candidate"
            }
        }}),
    );

    let mut remember_completed = false;
    let (provider_started, provider_read) = loop {
        let frame = rx
            .recv_timeout(FRAME_TIMEOUT)
            .expect("configured provider operation must start after baseline publication");
        if frame.get("method").is_some() {
            if remember_completed {
                break (Instant::now(), frame);
            }
            storage.reply(&mut stdin, &frame);
        } else if frame.get("id") == Some(&json!(13)) {
            assert_eq!(frame["result"]["data"]["code"], "ok");
            assert_eq!(frame["result"]["data"]["durable"], true);
            remember_completed = true;
        }
    };

    // Queue baseline requests while the enrichment adapter is inside its
    // configured operation. The pending storage HostCall preserves them, and
    // the baseline mutation response has already been delivered.
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":11,"method":"invokeTool","params":{
            "name":"memory_context_for_task","params":{"query":"baseline memory"}
        }}),
    );
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":12,"method":"invokeTool","params":{
            "name":"memory_search","params":{"query":"baseline memory"}
        }}),
    );

    assert!(provider_read.get("method").is_some());
    storage.reply(&mut stdin, &provider_read);

    let mut results = BTreeMap::<u64, Value>::new();
    while results.len() < 2 {
        let frame = rx
            .recv_timeout(FRAME_TIMEOUT)
            .expect("provider timeout must not block baseline memory tools");
        if frame.get("method").is_some() {
            storage.reply(&mut stdin, &frame);
        } else if let Some(id @ (11 | 12)) = frame.get("id").and_then(Value::as_u64) {
            results.insert(id, frame["result"]["data"].clone());
        }
    }

    assert!(
        provider_started.elapsed() < Duration::from_millis(300),
        "baseline tools must respond while the configured provider operation is still pending"
    );

    for payload in results.values() {
        assert_eq!(payload["code"], "ok");
        assert_eq!(payload["persistence_mode"], "durable");
        let wire = serde_json::to_string(payload).unwrap();
        assert!(!wire.contains("held-local-provider"));
        assert!(!wire.contains("timeout_ms"));
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "expired provider work must not emit a late result"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn every_failure_dto_rejects_outbound_durability() {
    use protocol::{MutationItemDto, PersistenceModeDto, StableFailureCodeDto};
    let code = StableFailureCodeDto::StorageUnavailable;
    let persistence_mode = PersistenceModeDto::Durable;
    for durable in [false, true] {
        let results = [
            serde_json::to_value(MutationItemDto::Failure {
                id: "missing".into(),
                code,
                durable,
            }),
            serde_json::to_value(RememberResult::Failure {
                code,
                persistence_mode,
                durable_data_unavailable: false,
                durable,
            }),
            serde_json::to_value(UpdateResult::Failure {
                code,
                persistence_mode,
                durable_data_unavailable: false,
                durable,
            }),
            serde_json::to_value(ForgetResult::Failure {
                code,
                persistence_mode,
                durable_data_unavailable: false,
                durable,
            }),
        ];
        for result in results {
            if durable {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap()["durable"], false);
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn enrichment_workers_are_optional_bounded_and_joined_on_shutdown() {
    use std::net::TcpListener;
    for configured in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let provider = configured.then(|| json!({"name":format!("http://{}/enrich", listener.local_addr().unwrap()),"timeout_ms":60000}));
        let (mut child, mut stdin, rx) = spawn_protocol_sidecar();
        let mut storage = MemoryHostFixture::empty();
        let exchange = |stdin: &mut std::process::ChildStdin,
                        storage: &mut MemoryHostFixture,
                        frame: Value| {
            let id = frame["id"].clone();
            send_frame(stdin, frame);
            loop {
                let response = rx.recv_timeout(FRAME_TIMEOUT).unwrap();
                if response.get("method").is_some() {
                    storage.reply(stdin, &response);
                } else if response["id"] == id {
                    break response;
                }
            }
        };
        let initialized = exchange(
            &mut stdin,
            &mut storage,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":1,"client_info":"worker-contract","extension_config":durable_init_config(provider)}}),
        );
        assert_eq!(initialized["result"]["type"], "Initialized");
        for id in 2..34 {
            let response = exchange(
                &mut stdin,
                &mut storage,
                json!({"jsonrpc":"2.0","id":id,"method":"invokeTool","params":{"name":"memory_remember","params":{"record":{"record_type":"observation","title":"bounded worker","body":"held provider","provenance":{}},"idempotency_key":format!("worker-{id}")}}}),
            );
            assert_eq!(response["result"]["data"]["code"], "ok");
        }
        exchange(
            &mut stdin,
            &mut storage,
            json!({"jsonrpc":"2.0","id":34,"method":"deliverEvent","params":{"event":"barrier","payload":{}}}),
        );
        let task_count = std::fs::read_dir(format!("/proc/{}/task", child.id()))
            .unwrap()
            .count();
        assert_eq!(
            task_count,
            if configured { 2 } else { 1 },
            "worker count after 32 remembers"
        );
        // Hold the actual HTTP connection without replying. Shutdown must cancel
        // it and join the worker rather than waiting for its 60-second deadline.
        let held = if configured {
            let deadline = Instant::now() + FRAME_TIMEOUT;
            Some(loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "provider was never called");
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("{error}"),
                }
            })
        } else {
            assert!(
                listener.accept().is_err(),
                "provider called without configuration"
            );
            None
        };
        let started = Instant::now();
        let shutdown = exchange(
            &mut stdin,
            &mut storage,
            json!({"jsonrpc":"2.0","id":35,"method":"shutdown","params":{}}),
        );
        assert_eq!(shutdown["result"]["type"], "Ack");
        let deadline = Instant::now() + FRAME_TIMEOUT;
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "sidecar failed to join its worker"
            );
            std::thread::yield_now();
        }
        assert!(started.elapsed() < FRAME_TIMEOUT);
        drop(held);
    }
}
