use extension_protocol::messages::{
    AgentMemoryCondition, AgentMemoryGetRequest, AgentMemoryMutation, AgentMemoryScanRequest,
    AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
};

use super::{
    InMemoryAgentMemoryStorage, SledAgentMemoryStorage, SledBackendFailure, generation_key,
    value_key,
};
use crate::ports::{
    AgentMemoryNamespace, AgentMemoryStorageBounds, AgentMemoryStorageError, AgentMemoryStoragePort,
};

fn bounds() -> AgentMemoryStorageBounds {
    AgentMemoryStorageBounds::try_new(4, 8, 2, 2, 2, 24, 1).unwrap()
}

fn namespace(project: &[u8], extension: &[u8]) -> AgentMemoryNamespace {
    AgentMemoryNamespace::from_trusted_identity(project, extension).unwrap()
}

fn put(key: &[u8], value: &[u8]) -> AgentMemoryMutation {
    AgentMemoryMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn transact(
    storage: &impl AgentMemoryStoragePort,
    namespace: &AgentMemoryNamespace,
    conditions: Vec<AgentMemoryCondition>,
    mutations: Vec<AgentMemoryMutation>,
) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageError> {
    storage.transact(
        namespace,
        AgentMemoryTransactionRequest {
            conditions,
            mutations,
        },
    )
}

/// Shared contract scenario for every agent-memory storage adapter.
pub(crate) fn storage_rejects_out_of_bounds_requests<S>(
    make: impl FnOnce() -> S,
    expected_durable: bool,
) where
    S: AgentMemoryStoragePort,
{
    for invalid in [
        (0, 8, 2, 2, 2, 24, 1),
        (4, 0, 2, 2, 2, 24, 1),
        (4, 8, 0, 2, 2, 24, 1),
        (4, 8, 2, 0, 2, 24, 1),
        (4, 8, 2, 2, 0, 24, 1),
        (4, 8, 2, 2, 2, 0, 1),
        (4, 8, 2, 2, 2, 24, 0),
    ] {
        assert_eq!(
            AgentMemoryStorageBounds::try_new(
                invalid.0, invalid.1, invalid.2, invalid.3, invalid.4, invalid.5, invalid.6,
            ),
            Err(AgentMemoryStorageError::BoundsExceeded)
        );
    }

    assert_eq!(
        AgentMemoryNamespace::from_trusted_identity(b"", b"extension"),
        Err(AgentMemoryStorageError::BoundsExceeded)
    );
    assert_eq!(
        AgentMemoryNamespace::from_trusted_identity(b"project", b""),
        Err(AgentMemoryStorageError::BoundsExceeded)
    );

    let storage = make();
    let namespace = namespace(b"project", b"extension");
    let get_error = storage
        .get(
            &namespace,
            AgentMemoryGetRequest {
                key: b"large".to_vec(),
            },
        )
        .unwrap_err();
    assert_eq!(get_error, AgentMemoryStorageError::BoundsExceeded);
    assert_eq!(
        get_error.to_string(),
        "agent memory storage bounds exceeded"
    );

    for request in [
        AgentMemoryScanRequest {
            prefix: b"large".to_vec(),
            cursor: None,
            limit: 1,
        },
        AgentMemoryScanRequest {
            prefix: Vec::new(),
            cursor: Some(b"large".to_vec()),
            limit: 1,
        },
        AgentMemoryScanRequest {
            prefix: Vec::new(),
            cursor: None,
            limit: 3,
        },
    ] {
        assert_eq!(
            storage.scan(&namespace, request),
            Err(AgentMemoryStorageError::BoundsExceeded)
        );
    }

    // Validation failures are not transactions: all rejected requests leave the
    // namespace at its initial generation.
    assert_eq!(
        storage
            .get(
                &namespace,
                AgentMemoryGetRequest {
                    key: b"safe".to_vec(),
                },
            )
            .unwrap()
            .generation,
        0
    );
    assert_eq!(
        transact(&storage, &namespace, Vec::new(), vec![put(b"safe", b"ok")]).unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 1,
            durable: expected_durable,
        }
    );

    for request in [
        AgentMemoryTransactionRequest {
            conditions: Vec::new(),
            mutations: Vec::new(),
        },
        AgentMemoryTransactionRequest {
            conditions: Vec::new(),
            mutations: vec![put(b"large", b"v")],
        },
        AgentMemoryTransactionRequest {
            conditions: Vec::new(),
            mutations: vec![put(b"key", b"value-too-large")],
        },
        AgentMemoryTransactionRequest {
            conditions: Vec::new(),
            mutations: vec![put(b"key", b"one"), put(b"key", b"two")],
        },
        AgentMemoryTransactionRequest {
            conditions: vec![
                AgentMemoryCondition::KeyAbsent { key: vec![1] },
                AgentMemoryCondition::KeyAbsent { key: vec![2] },
                AgentMemoryCondition::KeyAbsent { key: vec![3] },
            ],
            mutations: vec![put(b"key", b"v")],
        },
        AgentMemoryTransactionRequest {
            conditions: Vec::new(),
            mutations: vec![put(b"a", b"1"), put(b"b", b"2"), put(b"c", b"3")],
        },
        AgentMemoryTransactionRequest {
            conditions: vec![AgentMemoryCondition::ValueEquals {
                key: b"abcd".to_vec(),
                value: b"12345678".to_vec(),
            }],
            mutations: vec![put(b"efgh", b"87654321")],
        },
    ] {
        assert_eq!(
            storage.transact(&namespace, request),
            Err(AgentMemoryStorageError::BoundsExceeded)
        );
    }

    assert_eq!(
        AgentMemoryStorageError::Unavailable.to_string(),
        "agent memory storage unavailable"
    );
    assert_eq!(
        AgentMemoryStorageError::CommitOutcomeUnknown.to_string(),
        "agent memory storage commit outcome unknown"
    );
}

/// Shared contract scenario for every agent-memory storage adapter.
pub(crate) fn storage_reports_conflicts_without_partial_writes<S>(
    make: impl FnOnce() -> S,
    expected_durable: bool,
) where
    S: AgentMemoryStoragePort,
{
    let storage = make();
    let namespace = namespace(b"project", b"extension");

    assert_eq!(
        transact(&storage, &namespace, Vec::new(), vec![put(b"a", b"old")]).unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 1,
            durable: expected_durable,
        }
    );
    assert_eq!(
        transact(
            &storage,
            &namespace,
            vec![AgentMemoryCondition::ValueEquals {
                key: b"a".to_vec(),
                value: b"wrong".to_vec(),
            }],
            vec![put(b"a", b"new"), put(b"b", b"new")],
        )
        .unwrap(),
        AgentMemoryTransactionOutcome::Conflict
    );

    let existing = storage
        .get(&namespace, AgentMemoryGetRequest { key: b"a".to_vec() })
        .unwrap();
    assert_eq!(existing.value, Some(b"old".to_vec()));
    assert_eq!(existing.generation, 1);
    let absent = storage
        .get(&namespace, AgentMemoryGetRequest { key: b"b".to_vec() })
        .unwrap();
    assert_eq!(absent.value, None);
    assert_eq!(absent.generation, 1);

    assert_eq!(
        transact(&storage, &namespace, Vec::new(), vec![put(b"b", b"new")]).unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 2,
            durable: expected_durable,
        }
    );
}

/// Shared contract scenario for every agent-memory storage adapter.
pub(crate) fn storage_scans_in_order_with_bounded_cursor_progress<S>(
    make: impl FnOnce() -> S,
    expected_durable: bool,
) where
    S: AgentMemoryStoragePort,
{
    let storage = make();
    let namespace = namespace(b"project", b"extension");
    assert_eq!(
        transact(
            &storage,
            &namespace,
            Vec::new(),
            vec![put(b"ab", b"2"), put(b"aa", b"1")],
        )
        .unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 1,
            durable: expected_durable,
        }
    );
    assert_eq!(
        transact(
            &storage,
            &namespace,
            Vec::new(),
            vec![put(b"b", b"4"), put(b"ac", b"3")],
        )
        .unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 2,
            durable: expected_durable,
        }
    );

    let first = storage
        .scan(
            &namespace,
            AgentMemoryScanRequest {
                prefix: b"a".to_vec(),
                cursor: None,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.key.as_slice())
            .collect::<Vec<_>>(),
        vec![b"aa".as_slice(), b"ab".as_slice()]
    );
    assert_eq!(first.next_cursor, Some(b"ab".to_vec()));
    assert_eq!(first.generation, 2);

    let second = storage
        .scan(
            &namespace,
            AgentMemoryScanRequest {
                prefix: b"a".to_vec(),
                cursor: first.next_cursor,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_eq!(second.entries[0].key, b"ac");
    assert_eq!(second.next_cursor, None);
    assert_eq!(second.generation, 2);
}

/// Shared contract scenario for every agent-memory storage adapter.
pub(crate) fn storage_private_namespaces_cannot_observe_each_other<S>(
    make: impl FnOnce() -> S,
    expected_durable: bool,
) where
    S: AgentMemoryStoragePort,
{
    assert_eq!(
        namespace(b"project", b"extension").test_bytes(),
        [
            0xb3, 0xf1, 0x5e, 0x46, 0x0e, 0x95, 0x14, 0xf6, 0x0d, 0x5b, 0xb4, 0x31, 0x5f, 0x89,
            0xaa, 0x25, 0x97, 0x76, 0xa2, 0x70, 0xfc, 0x91, 0xe8, 0x80, 0x3a, 0x33, 0x78, 0x02,
            0x78, 0xc7, 0x2a, 0xde,
        ]
    );
    assert_ne!(
        namespace(b"ab", b"c").test_bytes(),
        namespace(b"a", b"bc").test_bytes(),
        "length framing must distinguish otherwise ambiguous concatenations"
    );

    let storage = make();
    let first = namespace(b"project", b"first-extension");
    let second = namespace(b"project", b"second-extension");

    assert_eq!(
        transact(&storage, &first, Vec::new(), vec![put(b"key", b"secret")]).unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 1,
            durable: expected_durable,
        }
    );
    assert_eq!(
        storage
            .get(
                &second,
                AgentMemoryGetRequest {
                    key: b"key".to_vec()
                }
            )
            .unwrap()
            .value,
        None
    );
    assert!(
        storage
            .scan(
                &second,
                AgentMemoryScanRequest {
                    prefix: Vec::new(),
                    cursor: None,
                    limit: 2,
                },
            )
            .unwrap()
            .entries
            .is_empty()
    );

    assert_eq!(
        transact(&storage, &second, Vec::new(), vec![put(b"key", b"other")]).unwrap(),
        AgentMemoryTransactionOutcome::Committed {
            generation: 1,
            durable: expected_durable,
        }
    );
    assert_eq!(
        storage
            .get(
                &first,
                AgentMemoryGetRequest {
                    key: b"key".to_vec()
                }
            )
            .unwrap()
            .value,
        Some(b"secret".to_vec())
    );
}

#[test]
fn in_memory_storage_rejects_out_of_bounds_requests() {
    storage_rejects_out_of_bounds_requests(|| InMemoryAgentMemoryStorage::new(bounds()), false);

    let (storage, barrier) = InMemoryAgentMemoryStorage::with_request_barrier(bounds());
    let storage = std::sync::Arc::new(storage);
    let namespace = namespace(b"project", b"extension");
    let active_storage = std::sync::Arc::clone(&storage);
    let active_namespace = namespace.clone();
    let active = std::thread::spawn(move || {
        active_storage.get(
            &active_namespace,
            AgentMemoryGetRequest {
                key: b"key".to_vec(),
            },
        )
    });
    barrier.entered.wait();
    assert_eq!(
        storage.get(
            &namespace,
            AgentMemoryGetRequest {
                key: b"key".to_vec()
            }
        ),
        Err(AgentMemoryStorageError::BoundsExceeded)
    );
    barrier.release.wait();
    assert!(active.join().unwrap().is_ok());
}

#[test]
fn in_memory_storage_reports_conflicts_without_partial_writes() {
    storage_reports_conflicts_without_partial_writes(
        || InMemoryAgentMemoryStorage::new(bounds()),
        false,
    );
}

#[test]
fn in_memory_storage_scans_in_order_with_bounded_cursor_progress() {
    storage_scans_in_order_with_bounded_cursor_progress(
        || InMemoryAgentMemoryStorage::new(bounds()),
        false,
    );
}

#[test]
fn private_namespaces_cannot_observe_each_other() {
    storage_private_namespaces_cannot_observe_each_other(
        || InMemoryAgentMemoryStorage::new(bounds()),
        false,
    );
}

#[test]
fn durable_storage_rejects_out_of_bounds_requests() {
    let root = tempfile::tempdir().unwrap();
    storage_rejects_out_of_bounds_requests(
        || SledAgentMemoryStorage::open(root.path(), bounds()).unwrap(),
        true,
    );
}

#[test]
fn durable_storage_reports_conflicts_without_partial_writes() {
    let root = tempfile::tempdir().unwrap();
    storage_reports_conflicts_without_partial_writes(
        || SledAgentMemoryStorage::open(root.path(), bounds()).unwrap(),
        true,
    );
}

#[test]
fn durable_generation_overflow_is_bounds_exceeded_without_partial_writes() {
    let root = tempfile::tempdir().unwrap();
    let namespace = namespace(b"project", b"extension");
    {
        let db =
            crate::adapters::storage::SledStorageAdapter::open_db_with_retry(root.path()).unwrap();
        let tree = db.open_tree("agent_memory_v1").unwrap();
        tree.insert(
            generation_key(namespace.as_bytes()),
            &u64::MAX.to_be_bytes(),
        )
        .unwrap();
        tree.insert(value_key(namespace.as_bytes(), b"key"), b"old")
            .unwrap();
        tree.flush().unwrap();
    }

    let storage = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
    assert_eq!(
        transact(&storage, &namespace, Vec::new(), vec![put(b"key", b"new")]),
        Err(AgentMemoryStorageError::BoundsExceeded)
    );
    drop(storage);

    let reopened = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
    assert_eq!(
        reopened.get(
            &namespace,
            AgentMemoryGetRequest {
                key: b"key".to_vec(),
            },
        ),
        Ok(extension_protocol::messages::AgentMemoryGetResult {
            value: Some(b"old".to_vec()),
            generation: u64::MAX,
        })
    );
}

#[test]
fn durable_storage_scans_in_order_with_bounded_cursor_progress() {
    let scan_root = tempfile::tempdir().unwrap();
    storage_scans_in_order_with_bounded_cursor_progress(
        || SledAgentMemoryStorage::open(scan_root.path(), bounds()).unwrap(),
        true,
    );

    let namespace_root = tempfile::tempdir().unwrap();
    storage_private_namespaces_cannot_observe_each_other(
        || SledAgentMemoryStorage::open(namespace_root.path(), bounds()).unwrap(),
        true,
    );
}

#[test]
fn durable_transaction_survives_reopen_after_acknowledgement() {
    let root = tempfile::tempdir().unwrap();
    let namespace = namespace(b"project", b"extension");
    {
        let storage = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
        assert_eq!(
            transact(
                &storage,
                &namespace,
                Vec::new(),
                vec![put(b"fst", b"one"), put(b"snd", b"two")],
            )
            .unwrap(),
            AgentMemoryTransactionOutcome::Committed {
                generation: 1,
                durable: true,
            }
        );
    }

    let reopened = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
    let result = reopened
        .scan(
            &namespace,
            AgentMemoryScanRequest {
                prefix: Vec::new(),
                cursor: None,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(
        result
            .entries
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect::<Vec<_>>(),
        vec![
            (b"fst".to_vec(), b"one".to_vec()),
            (b"snd".to_vec(), b"two".to_vec()),
        ]
    );
    assert_eq!(result.generation, 1);
}

#[test]
fn aborted_transaction_leaves_every_key_and_generation_unchanged_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let namespace = namespace(b"project", b"extension");
    {
        let storage = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
        assert_eq!(
            transact(&storage, &namespace, Vec::new(), vec![put(b"key", b"old")]),
            Ok(AgentMemoryTransactionOutcome::Committed {
                generation: 1,
                durable: true,
            })
        );
    }

    {
        let aborted = SledAgentMemoryStorage::open_with_backend_failure(
            root.path(),
            bounds(),
            SledBackendFailure::AbortBeforePublication,
        )
        .unwrap();
        assert_eq!(
            transact(&aborted, &namespace, Vec::new(), vec![put(b"key", b"new")]),
            Err(AgentMemoryStorageError::Unavailable)
        );
    }

    let reopened = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
    assert_eq!(
        reopened.get(
            &namespace,
            AgentMemoryGetRequest {
                key: b"key".to_vec(),
            },
        ),
        Ok(extension_protocol::messages::AgentMemoryGetResult {
            value: Some(b"old".to_vec()),
            generation: 1,
        })
    );
}

#[test]
fn uncertain_commit_requires_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    let namespace = namespace(b"project", b"extension");
    {
        let storage = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
        assert_eq!(
            transact(&storage, &namespace, Vec::new(), vec![put(b"key", b"old")]),
            Ok(AgentMemoryTransactionOutcome::Committed {
                generation: 1,
                durable: true,
            })
        );
    }

    {
        let uncertain = SledAgentMemoryStorage::open_with_backend_failure(
            root.path(),
            bounds(),
            SledBackendFailure::FlushAfterCommit,
        )
        .unwrap();
        assert_eq!(
            transact(
                &uncertain,
                &namespace,
                Vec::new(),
                vec![put(b"key", b"new")]
            ),
            Err(AgentMemoryStorageError::CommitOutcomeUnknown)
        );
    }

    // The failed acknowledgement cannot be retried blindly. Reopen and read
    // the committed state first, then perform a new conditional transaction.
    let reconciled = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
    assert_eq!(
        reconciled.get(
            &namespace,
            AgentMemoryGetRequest {
                key: b"key".to_vec(),
            },
        ),
        Ok(extension_protocol::messages::AgentMemoryGetResult {
            value: Some(b"new".to_vec()),
            generation: 2,
        })
    );
    assert_eq!(
        transact(
            &reconciled,
            &namespace,
            vec![AgentMemoryCondition::ValueEquals {
                key: b"key".to_vec(),
                value: b"new".to_vec(),
            }],
            vec![put(b"key", b"retried")],
        ),
        Ok(AgentMemoryTransactionOutcome::Committed {
            generation: 3,
            durable: true,
        })
    );
}

#[test]
fn backend_phase_failures_map_to_sanitized_public_errors() {
    let open_root = tempfile::tempdir().unwrap();
    assert_eq!(
        SledAgentMemoryStorage::open_with_backend_failure(
            open_root.path(),
            bounds(),
            SledBackendFailure::Open,
        )
        .unwrap_err(),
        AgentMemoryStorageError::Unavailable
    );

    let read_root = tempfile::tempdir().unwrap();
    let namespace = namespace(b"project", b"extension");
    let storage = SledAgentMemoryStorage::open_with_backend_failure(
        read_root.path(),
        bounds(),
        SledBackendFailure::SnapshotRead,
    )
    .unwrap();
    assert_eq!(
        storage.get(
            &namespace,
            AgentMemoryGetRequest {
                key: b"key".to_vec(),
            },
        ),
        Err(AgentMemoryStorageError::Unavailable)
    );
    assert_eq!(
        storage.scan(
            &namespace,
            AgentMemoryScanRequest {
                prefix: Vec::new(),
                cursor: None,
                limit: 1,
            },
        ),
        Err(AgentMemoryStorageError::Unavailable)
    );
}

#[test]
fn transaction_barriers_surround_publication_and_durable_acknowledgement() {
    let root = tempfile::tempdir().unwrap();
    let (storage, barriers) =
        SledAgentMemoryStorage::open_with_transaction_barriers(root.path(), bounds()).unwrap();
    let storage = std::sync::Arc::new(storage);
    let namespace = namespace(b"project", b"extension");
    let active_storage = std::sync::Arc::clone(&storage);
    let active_namespace = namespace.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let active = std::thread::spawn(move || {
        sender
            .send(transact(
                active_storage.as_ref(),
                &active_namespace,
                Vec::new(),
                vec![put(b"key", b"value")],
            ))
            .unwrap();
    });

    barriers.before_transaction_entered.wait();
    assert!(receiver.try_recv().is_err());
    barriers.before_transaction_release.wait();
    barriers.before_flush_entered.wait();
    assert!(receiver.try_recv().is_err());
    barriers.before_flush_release.wait();
    assert_eq!(
        receiver.recv().unwrap(),
        Ok(AgentMemoryTransactionOutcome::Committed {
            generation: 1,
            durable: true,
        })
    );
    active.join().unwrap();
    drop(storage);

    let reopened = SledAgentMemoryStorage::open(root.path(), bounds()).unwrap();
    assert_eq!(
        reopened
            .get(
                &namespace,
                AgentMemoryGetRequest {
                    key: b"key".to_vec(),
                },
            )
            .unwrap()
            .value,
        Some(b"value".to_vec())
    );
}
