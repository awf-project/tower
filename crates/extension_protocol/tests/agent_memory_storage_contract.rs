use extension_protocol::Capability;
use extension_protocol::messages::{
    AgentMemoryCondition, AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryKeyValue,
    AgentMemoryMutation, AgentMemoryScanRequest, AgentMemoryScanResult, AgentMemoryStorageFailure,
    AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest, HostCall,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn assert_exact_round_trip<T>(value: T, expected: Value)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(&value).unwrap(), expected);
    assert_eq!(serde_json::from_value::<T>(expected).unwrap(), value);
}

fn assert_rejected<T: DeserializeOwned>(values: impl IntoIterator<Item = Value>) {
    for value in values {
        assert!(
            serde_json::from_value::<T>(value.clone()).is_err(),
            "malformed wire value was accepted: {value}"
        );
    }
}

#[test]
fn agent_memory_storage_messages_round_trip_exact_wire_shapes() {
    assert_exact_round_trip(
        Capability::AgentMemoryStorage,
        json!("agent_memory_storage"),
    );

    assert_exact_round_trip(
        HostCall::AgentMemoryStorageGet {
            request: AgentMemoryGetRequest { key: vec![0, 255] },
        },
        json!({"type":"AgentMemoryStorageGet","request":{"key":[0,255]}}),
    );
    assert_exact_round_trip(
        HostCall::AgentMemoryStorageScan {
            request: AgentMemoryScanRequest {
                prefix: vec![],
                cursor: None,
                limit: 2,
            },
        },
        json!({"type":"AgentMemoryStorageScan","request":{"prefix":[],"cursor":null,"limit":2}}),
    );

    let transaction = AgentMemoryTransactionRequest {
        conditions: vec![
            AgentMemoryCondition::KeyAbsent { key: vec![1] },
            AgentMemoryCondition::ValueEquals {
                key: vec![2],
                value: vec![3],
            },
        ],
        mutations: vec![
            AgentMemoryMutation::Put {
                key: vec![1],
                value: vec![4],
            },
            AgentMemoryMutation::Delete { key: vec![2] },
        ],
    };
    assert_exact_round_trip(
        HostCall::AgentMemoryStorageTransact {
            request: transaction.clone(),
        },
        json!({
            "type":"AgentMemoryStorageTransact",
            "request":{
                "conditions":[
                    {"type":"KeyAbsent","key":[1]},
                    {"type":"ValueEquals","key":[2],"value":[3]}
                ],
                "mutations":[
                    {"type":"Put","key":[1],"value":[4]},
                    {"type":"Delete","key":[2]}
                ]
            }
        }),
    );

    assert_exact_round_trip(
        AgentMemoryCondition::KeyAbsent { key: vec![1] },
        json!({"type":"KeyAbsent","key":[1]}),
    );
    assert_exact_round_trip(
        AgentMemoryCondition::ValueEquals {
            key: vec![2],
            value: vec![3],
        },
        json!({"type":"ValueEquals","key":[2],"value":[3]}),
    );
    assert_exact_round_trip(
        AgentMemoryMutation::Put {
            key: vec![1],
            value: vec![4],
        },
        json!({"type":"Put","key":[1],"value":[4]}),
    );
    assert_exact_round_trip(
        AgentMemoryMutation::Delete { key: vec![2] },
        json!({"type":"Delete","key":[2]}),
    );

    for (result, expected) in [
        (
            AgentMemoryGetResult {
                value: Some(vec![4]),
                generation: 1,
            },
            json!({"value":[4],"generation":1}),
        ),
        (
            AgentMemoryGetResult {
                value: None,
                generation: 1,
            },
            json!({"value":null,"generation":1}),
        ),
    ] {
        assert_exact_round_trip(result, expected);
    }

    for (result, expected) in [
        (
            AgentMemoryScanResult {
                entries: vec![AgentMemoryKeyValue {
                    key: vec![1],
                    value: vec![4],
                }],
                next_cursor: Some(vec![1]),
                generation: 1,
            },
            json!({"entries":[{"key":[1],"value":[4]}],"next_cursor":[1],"generation":1}),
        ),
        (
            AgentMemoryScanResult {
                entries: vec![],
                next_cursor: None,
                generation: 1,
            },
            json!({"entries":[],"next_cursor":null,"generation":1}),
        ),
    ] {
        assert_exact_round_trip(result, expected);
    }

    for (outcome, expected) in [
        (
            AgentMemoryTransactionOutcome::Committed {
                generation: 1,
                durable: true,
            },
            json!({"type":"Committed","generation":1,"durable":true}),
        ),
        (
            AgentMemoryTransactionOutcome::Committed {
                generation: 1,
                durable: false,
            },
            json!({"type":"Committed","generation":1,"durable":false}),
        ),
        (
            AgentMemoryTransactionOutcome::Conflict,
            json!({"type":"Conflict"}),
        ),
        (
            AgentMemoryTransactionOutcome::Failure {
                failure: AgentMemoryStorageFailure::BoundsExceeded,
            },
            json!({"type":"Failure","failure":{"type":"BoundsExceeded"}}),
        ),
        (
            AgentMemoryTransactionOutcome::Failure {
                failure: AgentMemoryStorageFailure::Unavailable,
            },
            json!({"type":"Failure","failure":{"type":"Unavailable"}}),
        ),
        (
            AgentMemoryTransactionOutcome::Failure {
                failure: AgentMemoryStorageFailure::CommitOutcomeUnknown,
            },
            json!({"type":"Failure","failure":{"type":"CommitOutcomeUnknown"}}),
        ),
    ] {
        assert_exact_round_trip(outcome, expected);
    }

    for (failure, expected) in [
        (
            AgentMemoryStorageFailure::BoundsExceeded,
            json!({"type":"BoundsExceeded"}),
        ),
        (
            AgentMemoryStorageFailure::Unavailable,
            json!({"type":"Unavailable"}),
        ),
        (
            AgentMemoryStorageFailure::CommitOutcomeUnknown,
            json!({"type":"CommitOutcomeUnknown"}),
        ),
    ] {
        assert_exact_round_trip(failure, expected);
    }

    assert_rejected::<AgentMemoryGetRequest>([
        json!({"key":[1],"unknown":true}),
        json!({}),
        json!({"key":[-1]}),
        json!({"key":[256]}),
        json!({"key":[1.5]}),
    ]);
    assert_rejected::<AgentMemoryScanRequest>([
        json!({"prefix":[],"cursor":null,"limit":2,"unknown":true}),
        json!({"prefix":[],"cursor":null}),
    ]);
    assert_rejected::<AgentMemoryCondition>([
        json!({"type":"Unknown","key":[1]}),
        json!({"type":"KeyAbsent","key":[1],"value":[2]}),
        json!({"type":"ValueEquals","key":[1]}),
    ]);
    assert_rejected::<AgentMemoryMutation>([
        json!({"type":"Unknown","key":[1]}),
        json!({"type":"Delete","key":[1],"value":[2]}),
        json!({"type":"Put","key":[1]}),
    ]);
    assert_rejected::<AgentMemoryTransactionOutcome>([
        json!({"type":"Unknown"}),
        json!({"type":"Conflict","generation":1}),
        json!({"type":"Committed","generation":1,"durable":true,"failure":{"type":"Unavailable"}}),
        json!({"type":"Failure","failure":{"type":"Unavailable"},"generation":1}),
    ]);
    assert_rejected::<AgentMemoryStorageFailure>([
        json!({"type":"Unknown"}),
        json!({"type":"Unavailable","message":"backend path leaked"}),
    ]);
    assert_rejected::<HostCall>([
        json!({"type":"AgentMemoryStorageGet","request":{"key":[1]},"unknown":true}),
        json!({"type":"AgentMemoryStorageGet","request":{"key":[1],"path":"/tmp/private"}}),
        json!({"type":"AgentMemoryStorageTransact","request":{"conditions":[],"mutations":[]},"failure":{"type":"Unavailable"}}),
    ]);

    let legacy_with_extension_field = serde_json::from_value::<HostCall>(json!({
        "type":"ReadFile",
        "path":"src/lib.rs",
        "strict":true,
        "extension_field":1
    }))
    .expect("legacy HostCall variants must remain forward-compatible");
    assert_eq!(
        legacy_with_extension_field,
        HostCall::ReadFile {
            path: "src/lib.rs".to_owned(),
            strict: Some(true),
        }
    );
}
