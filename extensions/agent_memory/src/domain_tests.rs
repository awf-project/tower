use crate::{
    AgentMemoryLimitsConfig, CompactHit, ContinuationCursor, MatchReason, MatchReasonField,
    MatchReasonKind, MemoryFilters, MemoryId, MemoryRecord, MemoryType, PrivacyAction,
    PrivacyError, PrivacyFilter, PrivacyProducer, PrivacyProducerInput, PrivacyProvenanceInput,
    PrivacyRule, PrivacyRuleConfig, Provenance, RankingMode, ResponseBudget, RetrievalError,
    Revision, SearchOrderKey, TaskContext, TimelineEvent, TimelineOrderKey, Truncation,
    ValidationError,
};

fn limits() -> AgentMemoryLimitsConfig {
    AgentMemoryLimitsConfig {
        max_title_bytes: 12,
        max_body_bytes: 32,
        max_metadata_items: 2,
        max_metadata_string_bytes: 16,
        max_batch_items: 4,
        default_results: 2,
        max_results: 4,
        default_response_bytes: 128,
        max_response_bytes: 256,
        max_query_bytes: 24,
    }
}

fn provenance() -> Provenance {
    Provenance::try_new(
        Some("main".into()),
        vec!["src/lib.rs".into()],
        vec!["MemoryRecord".into()],
        vec!["domain".into()],
        Some("agent".into()),
        Some("client".into()),
        Some("abc123".into()),
        Some("17".into()),
        Some("42".into()),
        &limits(),
    )
    .expect("valid provenance")
}

fn record(
    title: &str,
    body: &str,
    confidence: Option<f64>,
    created_at: &str,
    updated_at: &str,
) -> Result<MemoryRecord, ValidationError> {
    MemoryRecord::try_new(
        MemoryId::try_from("memory-1".to_owned()).expect("valid memory id"),
        Revision::try_from("revision-1".to_owned()).expect("valid revision"),
        1,
        MemoryType::Observation,
        title.to_owned(),
        body.to_owned(),
        provenance(),
        confidence,
        "project".to_owned(),
        created_at.to_owned(),
        updated_at.to_owned(),
        &limits(),
    )
}

#[allow(clippy::too_many_arguments)]
fn record_with(
    id: MemoryId,
    revision: Revision,
    schema_version: u32,
    title: &str,
    body: &str,
    project_key: &str,
) -> Result<MemoryRecord, ValidationError> {
    MemoryRecord::try_new(
        id,
        revision,
        schema_version,
        MemoryType::Observation,
        title.to_owned(),
        body.to_owned(),
        provenance(),
        None,
        project_key.to_owned(),
        "2026-11-05T10:00:00Z".to_owned(),
        "2026-11-05T10:00:01Z".to_owned(),
        &limits(),
    )
}

fn valid_id() -> MemoryId {
    MemoryId::try_from("memory-1".to_owned()).expect("valid memory id")
}

fn valid_revision() -> Revision {
    Revision::try_from("revision-1".to_owned()).expect("valid revision")
}

fn retrieval_limits() -> AgentMemoryLimitsConfig {
    AgentMemoryLimitsConfig {
        max_title_bytes: 16,
        max_body_bytes: 32,
        max_metadata_items: 4,
        max_metadata_string_bytes: 24,
        max_batch_items: 8,
        default_results: 4,
        max_results: 8,
        default_response_bytes: 1024,
        max_response_bytes: 4096,
        max_query_bytes: 64,
    }
}

fn empty_filters() -> MemoryFilters {
    MemoryFilters::try_new(
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &retrieval_limits(),
    )
    .expect("valid empty filters")
}

fn compact_hit(id: &str, title: &str, timestamp: &str, score: f64) -> CompactHit {
    CompactHit::try_new(
        MemoryId::try_from(id.to_owned()).expect("valid memory id"),
        Revision::try_from(format!("revision-{id}")).expect("valid revision"),
        MemoryType::Observation,
        title.to_owned(),
        vec!["src/lib.rs".into()],
        timestamp.to_owned(),
        score,
        vec![],
        vec![],
        &retrieval_limits(),
    )
    .expect("valid compact hit")
}

fn compact_hit_with_reason_and_truncation(id: &str, title: &str) -> CompactHit {
    CompactHit::try_new(
        MemoryId::try_from(id.to_owned()).expect("valid memory id"),
        Revision::try_from(format!("revision-{id}")).expect("valid revision"),
        MemoryType::Observation,
        title.to_owned(),
        vec!["src/lib.rs".into()],
        "2026-11-05T10:00:00Z".into(),
        1.0,
        vec![MatchReason::new(
            MatchReasonKind::Lexical,
            MatchReasonField::Title,
        )],
        vec!["paths".into()],
        &retrieval_limits(),
    )
    .expect("valid compact hit")
}

const ENVELOPE_CURSOR: &str = "eyJ2IjoxLCJyZXF1ZXN0X2RpZ2VzdCI6IjAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAiLCJnZW5lcmF0aW9uIjo3LCJvZmZzZXQiOjF9";

fn literal_compact_envelope(id: &str, title: &str) -> String {
    format!(
        r#"{{"code":"ok","persistence_mode":"in_memory","durable_data_unavailable":false,"hits":[{{"id":"{id}","revision":"revision-{id}","record_type":"observation","title":"{title}","paths":["src/lib.rs"],"timestamp":"2026-11-05T10:00:00Z","score":1.0,"reasons":[{{"kind":"lexical","field":"title"}}],"truncated_fields":["paths"]}}],"applied_filters":{{"record_types":[],"branches":[],"paths":[],"symbols":[],"tags":[],"source_agent":null,"source_client":null,"created_from":null,"created_to":null}},"next_cursor":"{ENVELOPE_CURSOR}"}}"#
    )
}

fn literal_empty_envelope() -> &'static str {
    r#"{"code":"ok","persistence_mode":"in_memory","durable_data_unavailable":false,"hits":[],"applied_filters":{"record_types":[],"branches":[],"paths":[],"symbols":[],"tags":[],"source_agent":null,"source_client":null,"created_from":null,"created_to":null},"next_cursor":null}"#
}

#[test]
fn compact_results_order_deterministically_for_equal_scores_and_timestamps() {
    let context = TaskContext::try_new(
        vec![
            compact_hit("memory-c", "older", "2026-11-05T09:00:00Z", 4.0),
            compact_hit("memory-b", "same time", "2026-11-05T10:00:00Z", 4.0),
            compact_hit("memory-a", "same time", "2026-11-05T10:00:00Z", 4.0),
            compact_hit("memory-z", "higher score", "2026-11-05T08:00:00Z", 5.0),
        ],
        empty_filters(),
        None,
        ResponseBudget::try_new(4, 4096, &retrieval_limits()).expect("valid response budget"),
    )
    .expect("hits fit in the response budget");

    let rendered = format!("{context:?}");
    let positions = ["memory-z", "memory-a", "memory-b", "memory-c"].map(|id| {
        rendered
            .find(id)
            .unwrap_or_else(|| panic!("ordered context must contain {id}: {rendered}"))
    });
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn timeline_events_order_by_creation_without_update_reordering() {
    let mut events = [
        (
            TimelineOrderKey::try_new(
                "2026-11-05T10:00:00Z".into(),
                MemoryId::try_from("memory-b".to_owned()).expect("valid memory id"),
            )
            .expect("valid timeline key"),
            TimelineEvent::try_new(compact_hit(
                "memory-b",
                "updated later",
                "2026-11-05T10:00:00Z",
                1.0,
            ))
            .expect("valid timeline event"),
        ),
        (
            TimelineOrderKey::try_new(
                "2026-11-05T10:00:00.1Z".into(),
                MemoryId::try_from("memory-fractional".to_owned()).expect("valid memory id"),
            )
            .expect("valid fractional timeline key"),
            TimelineEvent::try_new(compact_hit(
                "memory-fractional",
                "created fractionally later",
                "2026-11-05T10:00:00.1Z",
                1.0,
            ))
            .expect("valid timeline event"),
        ),
        (
            TimelineOrderKey::try_new(
                "2026-11-05T09:00:00.000Z".into(),
                MemoryId::try_from("memory-z".to_owned()).expect("valid memory id"),
            )
            .expect("valid normalized UTC timeline key"),
            TimelineEvent::try_new(compact_hit(
                "memory-z",
                "created first",
                "2026-11-05T09:00:00Z",
                1.0,
            ))
            .expect("valid timeline event"),
        ),
        (
            TimelineOrderKey::try_new(
                "2026-11-05T10:00:00Z".into(),
                MemoryId::try_from("memory-a".to_owned()).expect("valid memory id"),
            )
            .expect("valid timeline key"),
            TimelineEvent::try_new(compact_hit(
                "memory-a",
                "updated earlier",
                "2026-11-05T10:00:00Z",
                1.0,
            ))
            .expect("valid timeline event"),
        ),
    ];

    events.sort_by(|(left, _), (right, _)| left.cmp(right));

    assert_eq!(
        events
            .iter()
            .map(|(key, _)| format!("{key:?}"))
            .collect::<Vec<_>>(),
        vec![
            "TimelineOrderKey { created_at: \"2026-11-05T09:00:00Z\", id: MemoryId(\"memory-z\") }",
            "TimelineOrderKey { created_at: \"2026-11-05T10:00:00Z\", id: MemoryId(\"memory-a\") }",
            "TimelineOrderKey { created_at: \"2026-11-05T10:00:00Z\", id: MemoryId(\"memory-b\") }",
            "TimelineOrderKey { created_at: \"2026-11-05T10:00:00.100Z\", id: MemoryId(\"memory-fractional\") }",
        ]
    );
}

#[test]
fn compact_fields_truncate_only_at_utf8_boundaries() {
    let hit = CompactHit::try_new(
        MemoryId::try_from("memory-utf8".to_owned()).expect("valid memory id"),
        Revision::try_from("revision-utf8".to_owned()).expect("valid revision"),
        MemoryType::Observation,
        "ééééééééé".into(),
        vec!["src/ééééééééé.rs".into()],
        "2026-11-05T10:00:00Z".into(),
        1.0,
        vec![],
        vec![],
        &retrieval_limits(),
    )
    .expect("compact fields are UTF-8 safely truncated");

    let rendered = format!("{hit:?}");
    assert_eq!(
        rendered,
        "CompactHit { id: MemoryId(\"memory-utf8\"), revision: Revision(\"revision-utf8\"), record_type: Observation, title: \"éééééééé\", paths: [\"src/éééééééé.rs\"], timestamp: \"2026-11-05T10:00:00Z\", score: 1.0, reasons: [], truncated_fields: [\"title\", \"paths\"] }"
    );
}

#[test]
fn compact_title_rejects_a_limit_that_cannot_fit_one_scalar() {
    let mut limits = retrieval_limits();
    limits.max_title_bytes = 1;

    assert_eq!(
        CompactHit::try_new(
            valid_id(),
            valid_revision(),
            MemoryType::Observation,
            "é".into(),
            vec![],
            "2026-11-05T10:00:00Z".into(),
            1.0,
            vec![],
            vec![],
            &limits,
        ),
        Err(RetrievalError::InvalidArgument {
            field: "title",
            reason: "empty",
        })
    );
}

#[test]
fn compact_path_with_overlong_extension_stays_within_the_metadata_bound() {
    let hit = CompactHit::try_new(
        valid_id(),
        valid_revision(),
        MemoryType::Observation,
        "title".into(),
        vec!["a.abcdefghijklmnopqrstuvwxyz".into()],
        "2026-11-05T10:00:00Z".into(),
        1.0,
        vec![],
        vec![],
        &retrieval_limits(),
    )
    .expect("the path can be safely bounded");

    assert!(format!("{hit:?}").contains("paths: [\"a.abcdefghijklmnopqrstuv\"]"));
}

#[test]
fn retrieval_values_store_normalized_timestamps_and_paths() {
    let filters = MemoryFilters::try_new(
        vec![],
        vec![],
        vec!["src//./lib.rs".into()],
        vec![],
        vec![],
        None,
        None,
        Some("2026-11-05T10:00:00.000Z".into()),
        None,
        &retrieval_limits(),
    )
    .expect("valid filters normalize their values");
    assert!(format!("{filters:?}").contains("paths: [\"src/lib.rs\"]"));
    assert!(format!("{filters:?}").contains("created_from: Some(\"2026-11-05T10:00:00Z\")"));

    let hit = CompactHit::try_new(
        valid_id(),
        valid_revision(),
        MemoryType::Observation,
        "title".into(),
        vec!["src//./lib.rs".into()],
        "2026-11-05T10:00:00.000Z".into(),
        1.0,
        vec![],
        vec![],
        &retrieval_limits(),
    )
    .expect("valid compact hits normalize their values");
    let rendered = format!("{hit:?}");
    assert!(rendered.contains("paths: [\"src/lib.rs\"]"));
    assert!(rendered.contains("timestamp: \"2026-11-05T10:00:00Z\""));
}

#[test]
fn response_envelope_reserves_ids_revisions_reasons_and_cursor() {
    let cursor = ContinuationCursor::try_new(ENVELOPE_CURSOR, [0; 32], 7).expect("bound cursor");
    let hit = compact_hit_with_reason_and_truncation("memory-envelope", "title");
    let budget = ResponseBudget::try_new(1, 128, &retrieval_limits()).expect("valid budget");

    let error = TaskContext::try_new(vec![hit], empty_filters(), Some(cursor), budget)
        .expect_err("the complete envelope cannot fit into 128 bytes");
    assert!(matches!(
        error,
        RetrievalError::ResponseBudgetTooSmall { minimum_bytes }
            if minimum_bytes == literal_compact_envelope("memory-envelope", "title").len()
    ));
}

#[test]
fn task_context_constructor_reports_exact_minimum_for_unfittable_hit() {
    let hit = compact_hit_with_reason_and_truncation("memory-unfittable", "full title");
    let cursor = ContinuationCursor::try_new(ENVELOPE_CURSOR, [0; 32], 7).expect("bound cursor");
    let error = TaskContext::try_new(
        vec![hit],
        empty_filters(),
        Some(cursor),
        ResponseBudget::try_new(1, 1, &retrieval_limits()).expect("valid positive budget"),
    )
    .expect_err("the first hit must not be silently omitted");

    assert!(matches!(
        error,
        RetrievalError::ResponseBudgetTooSmall { minimum_bytes }
            if minimum_bytes == literal_compact_envelope("memory-unfittable", "full title").len()
    ));
}

#[test]
fn response_budget_omits_only_complete_later_hits() {
    let cursor = ContinuationCursor::try_new(ENVELOPE_CURSOR, [0; 32], 7).expect("bound cursor");
    let first = compact_hit_with_reason_and_truncation("memory-a", "first");
    let second = compact_hit_with_reason_and_truncation("memory-b", "second");
    let first_envelope_bytes = literal_compact_envelope("memory-a", "first").len() as u32;

    let context = TaskContext::try_new(
        vec![second, first],
        empty_filters(),
        Some(cursor),
        ResponseBudget::try_new(2, first_envelope_bytes, &retrieval_limits())
            .expect("valid response budget"),
    )
    .expect("the first complete hit fits and the later hit is omitted");

    let rendered = format!("{context:?}");
    assert!(rendered.contains("memory-a"));
    assert!(!rendered.contains("memory-b"));
}

#[test]
fn empty_envelope_still_obeys_response_budget() {
    let error = TaskContext::try_new(
        vec![],
        empty_filters(),
        None,
        ResponseBudget::try_new(1, 1, &retrieval_limits()).expect("valid positive budget"),
    )
    .expect_err("the complete empty envelope cannot fit into one byte");

    assert_eq!(
        error,
        RetrievalError::ResponseBudgetTooSmall {
            minimum_bytes: literal_empty_envelope().len(),
        }
    );
}

#[test]
fn retrieval_constructors_reject_invalid_values_with_exact_errors() {
    assert_eq!(
        RankingMode::try_from("lexical_v1"),
        Ok(RankingMode::LexicalV1)
    );
    assert_eq!(
        RankingMode::try_from("semantic_v1"),
        Err(RetrievalError::InvalidArgument {
            field: "ranking",
            reason: "unsupported",
        })
    );
    assert_eq!(
        SearchOrderKey::try_new(f64::NAN, "2026-11-05T10:00:00Z".into(), valid_id(),),
        Err(RetrievalError::InvalidArgument {
            field: "score",
            reason: "not_finite",
        })
    );
    assert_eq!(
        TimelineOrderKey::try_new("2026-11-05T11:00:00+01:00".into(), valid_id()),
        Err(RetrievalError::InvalidArgument {
            field: "created_at",
            reason: "not_utc",
        })
    );
    assert_eq!(
        ResponseBudget::try_new(0, 128, &retrieval_limits()),
        Err(RetrievalError::InvalidArgument {
            field: "limit",
            reason: "out_of_range",
        })
    );
    assert_eq!(
        ContinuationCursor::try_new("not-base64url", [0; 32], 7),
        Err(RetrievalError::CursorExpired)
    );
    assert_eq!(
        Truncation::try_new(vec!["body".into()], 1),
        Err(RetrievalError::InvalidArgument {
            field: "truncated_fields",
            reason: "unsupported_field",
        })
    );
    assert_eq!(
        MemoryFilters::try_new(
            vec![],
            vec![
                "branch-a".into(),
                "branch-b".into(),
                "branch-c".into(),
                "branch-d".into(),
                "branch-e".into(),
            ],
            vec![],
            vec![],
            vec![],
            None,
            None,
            None,
            None,
            &retrieval_limits(),
        ),
        Err(RetrievalError::InvalidArgument {
            field: "branches",
            reason: "limit_exceeded",
        })
    );
    assert_eq!(
        MemoryFilters::try_new(
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            None,
            Some("2026-11-05T11:00:00+01:00".into()),
            None,
            &retrieval_limits(),
        ),
        Err(RetrievalError::InvalidArgument {
            field: "created_from",
            reason: "not_utc",
        })
    );
    assert_eq!(
        CompactHit::try_new(
            valid_id(),
            valid_revision(),
            MemoryType::Observation,
            "title".into(),
            vec![],
            "2026-11-05T10:00:00Z".into(),
            -1.0,
            vec![],
            vec![],
            &retrieval_limits(),
        ),
        Err(RetrievalError::InvalidArgument {
            field: "score",
            reason: "negative",
        })
    );
    assert_eq!(
        CompactHit::try_new(
            valid_id(),
            valid_revision(),
            MemoryType::Observation,
            "title".into(),
            vec![],
            "2026-11-05T10:00:00+01:00".into(),
            1.0,
            vec![],
            vec![],
            &retrieval_limits(),
        ),
        Err(RetrievalError::InvalidArgument {
            field: "timestamp",
            reason: "not_utc",
        })
    );
    assert_eq!(
        CompactHit::try_new(
            valid_id(),
            valid_revision(),
            MemoryType::Observation,
            String::new(),
            vec![],
            "2026-11-05T10:00:00Z".into(),
            1.0,
            vec![],
            vec![],
            &retrieval_limits(),
        ),
        Err(RetrievalError::InvalidArgument {
            field: "title",
            reason: "empty",
        })
    );
}

#[test]
fn records_validate_required_fields_and_metadata_bounds() {
    assert!(
        record(
            "title",
            "body",
            Some(0.75),
            "2026-11-05T10:00:00Z",
            "2026-11-05T10:00:01Z",
        )
        .is_ok()
    );

    assert_eq!(
        crate::require_field::<String>("title", None),
        Err(ValidationError::MissingField { field: "title" })
    );
    assert_eq!(
        MemoryId::try_from(String::new()),
        Err(ValidationError::InvalidMemoryId)
    );
    assert_eq!(
        Revision::try_from(String::new()),
        Err(ValidationError::InvalidRevision)
    );
    assert_eq!(
        MemoryId::try_from("m".repeat(1025)),
        Err(ValidationError::InvalidMemoryId)
    );
    assert_eq!(
        Revision::try_from("r".repeat(1025)),
        Err(ValidationError::InvalidRevision)
    );
    assert_eq!(
        record_with(valid_id(), valid_revision(), 1, "title", "body", ""),
        Err(ValidationError::EmptyField {
            field: "project_key",
        })
    );
    assert_eq!(
        record_with(valid_id(), valid_revision(), 2, "title", "body", "project"),
        Err(ValidationError::UnsupportedMemoryType)
    );
    assert_eq!(
        record(
            "",
            "body",
            None,
            "2026-11-05T10:00:00Z",
            "2026-11-05T10:00:01Z",
        ),
        Err(ValidationError::EmptyField { field: "title" })
    );
    assert_eq!(
        record(
            "title is too long",
            "body",
            None,
            "2026-11-05T10:00:00Z",
            "2026-11-05T10:00:01Z",
        ),
        Err(ValidationError::LimitExceeded {
            field: "title",
            limit: limits().max_title_bytes,
        })
    );
    assert_eq!(
        record(
            "title",
            &"x".repeat(33),
            None,
            "2026-11-05T10:00:00Z",
            "2026-11-05T10:00:01Z",
        ),
        Err(ValidationError::LimitExceeded {
            field: "body",
            limit: limits().max_body_bytes,
        })
    );
    assert_eq!(
        record(
            "title",
            "body",
            Some(f64::NAN),
            "2026-11-05T10:00:00Z",
            "2026-11-05T10:00:01Z",
        ),
        Err(ValidationError::InvalidConfidence)
    );
    assert_eq!(
        record(
            "title",
            "body",
            Some(1.01),
            "2026-11-05T10:00:00Z",
            "2026-11-05T10:00:01Z",
        ),
        Err(ValidationError::InvalidConfidence)
    );
    assert_eq!(
        record(
            "title",
            "body",
            None,
            "not-a-timestamp",
            "2026-11-05T10:00:01Z",
        ),
        Err(ValidationError::InvalidTimestamp {
            field: "created_at",
        })
    );
    assert_eq!(
        record(
            "title",
            "body",
            None,
            "2026-11-05T10:00:01Z",
            "2026-11-05T10:00:00Z",
        ),
        Err(ValidationError::InvalidTimestamp {
            field: "updated_at",
        })
    );

    assert_eq!(
        Provenance::try_new(
            None,
            vec!["one".into(), "two".into(), "three".into()],
            vec![],
            vec![],
            None,
            None,
            None,
            None,
            None,
            &limits(),
        ),
        Err(ValidationError::LimitExceeded {
            field: "paths",
            limit: limits().max_metadata_items,
        })
    );
    assert_eq!(
        Provenance::try_new(
            None,
            vec!["/outside/workspace".into()],
            vec![],
            vec![],
            None,
            None,
            None,
            None,
            None,
            &limits(),
        ),
        Err(ValidationError::InvalidMetadataType { field: "paths" })
    );
    assert_eq!(
        Provenance::try_new(
            None,
            vec!["x".repeat(17)],
            vec![],
            vec![],
            None,
            None,
            None,
            None,
            None,
            &limits(),
        ),
        Err(ValidationError::LimitExceeded {
            field: "paths",
            limit: limits().max_metadata_string_bytes,
        })
    );
}

#[test]
fn privacy_rules_filter_every_persisted_and_provider_bound_field() {
    let reject = PrivacyRule::try_new(
        "reject-secret".into(),
        "SECRET".into(),
        PrivacyAction::Reject,
    )
    .expect("valid reject rule");
    let redact = PrivacyRule::try_new(
        "redact-token".into(),
        "TOKEN".into(),
        PrivacyAction::Redact {
            replacement: "[redacted]".into(),
        },
    )
    .expect("valid redact rule");

    let rejecting_filter = PrivacyFilter::try_new(vec![reject]).expect("valid filter");
    let redacting_filter = PrivacyFilter::try_new(vec![redact]).expect("valid filter");

    let reject_before_redact = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "redact-secret".into(),
            "SECRET".into(),
            PrivacyAction::Redact {
                replacement: "safe".into(),
            },
        )
        .expect("valid redact rule"),
        PrivacyRule::try_new(
            "reject-secret".into(),
            "SECRET".into(),
            PrivacyAction::Reject,
        )
        .expect("valid reject rule"),
    ])
    .expect("valid filter");
    assert_eq!(
        reject_before_redact.apply("body", "SECRET", limits().max_body_bytes),
        Err(PrivacyError::Rejected {
            field: "body",
            rule_id: "reject-secret".into(),
        })
    );

    for field in ["title", "body", "metadata", "idempotency_key", "query"] {
        assert_eq!(
            rejecting_filter.apply(field, "SECRET TOKEN", limits().max_body_bytes),
            Err(PrivacyError::Rejected {
                field,
                rule_id: "reject-secret".into(),
            }),
            "reject rules must win before redaction for {field}",
        );
        assert_eq!(
            redacting_filter
                .apply(field, "TOKEN TOKEN", limits().max_body_bytes)
                .as_deref(),
            Ok("[redacted] [redacted]"),
            "redaction is literal and global for {field}",
        );
    }
}

#[test]
fn privacy_failures_identify_field_and_rule_without_echoing_secret() {
    const SENTINELS: [&str; 5] = [
        "TITLE_SECRET",
        "BODY_PRIVATE_SENTINEL",
        "META_SECRET",
        "IDEM_SECRET",
        "QUERY_PRIVATE_SENTINEL",
    ];

    #[derive(Default)]
    struct CapturingProducer {
        persisted: Vec<PrivacyProducerInput>,
        derived: Vec<PrivacyProducerInput>,
        logged: Vec<PrivacyProducerInput>,
        provider_requests: Vec<PrivacyProducerInput>,
    }

    impl PrivacyProducer for CapturingProducer {
        fn persist(&mut self, input: &PrivacyProducerInput) {
            self.persisted.push(input.clone());
        }
        fn derive(&mut self, input: &PrivacyProducerInput) {
            self.derived.push(input.clone());
        }
        fn log(&mut self, input: &PrivacyProducerInput) {
            self.logged.push(input.clone());
        }
        fn dispatch_provider(&mut self, input: &PrivacyProducerInput) {
            self.provider_requests.push(input.clone());
        }
    }

    let input = PrivacyProducerInput {
        title: SENTINELS[0].into(),
        body: SENTINELS[1].into(),
        provenance: PrivacyProvenanceInput {
            branch: Some(SENTINELS[2].into()),
            paths: vec!["src/lib.rs".into()],
            symbols: vec!["MemoryRecord".into()],
            tags: vec!["domain".into()],
            source_agent: Some("agent".into()),
            source_client: Some("client".into()),
            commit: Some("abc123".into()),
            pull_request: Some("17".into()),
            issue: Some("42".into()),
        },
        idempotency_key: SENTINELS[3].into(),
        query: SENTINELS[4].into(),
    };
    let mut producer = CapturingProducer::default();
    let invalid_pattern = format!("({}", SENTINELS[0]);
    let error = PrivacyFilter::try_from_configs(vec![PrivacyRuleConfig {
        id: "invalid-regex".into(),
        pattern: invalid_pattern,
        action: PrivacyAction::Reject,
    }])
    .expect_err("malformed regex must be rejected before dispatch");

    assert!(matches!(
        error,
        PrivacyError::InvalidRule {
            ref rule_id,
            reason: "invalid_pattern",
        } if rule_id == "invalid-regex"
    ));
    assert!(
        SENTINELS
            .iter()
            .all(|sentinel| !format!("{error:?}").contains(sentinel))
    );
    assert!(producer.persisted.is_empty());
    assert!(producer.derived.is_empty());
    assert!(producer.logged.is_empty());
    assert!(producer.provider_requests.is_empty());

    let clean_input = PrivacyProducerInput {
        title: "title".into(),
        body: "body".into(),
        provenance: PrivacyProvenanceInput {
            branch: Some("main".into()),
            paths: vec!["src/lib.rs".into()],
            symbols: vec!["MemoryRecord".into()],
            tags: vec!["domain".into()],
            source_agent: Some("agent".into()),
            source_client: Some("client".into()),
            commit: Some("abc123".into()),
            pull_request: Some("17".into()),
            issue: Some("42".into()),
        },
        idempotency_key: "request-1".into(),
        query: "find memory".into(),
    };
    type PrivacyLeafCase = (&'static str, fn(&mut PrivacyProducerInput));
    let privacy_leaf_cases: [PrivacyLeafCase; 13] = [
        ("title", |value| value.title = "LEAF_SECRET".into()),
        ("body", |value| value.body = "LEAF_SECRET".into()),
        ("provenance.branch", |value| {
            value.provenance.branch = Some("LEAF_SECRET".into())
        }),
        ("provenance.paths", |value| {
            value.provenance.paths = vec!["LEAF_SECRET".into()]
        }),
        ("provenance.symbols", |value| {
            value.provenance.symbols = vec!["LEAF_SECRET".into()]
        }),
        ("provenance.tags", |value| {
            value.provenance.tags = vec!["LEAF_SECRET".into()]
        }),
        ("provenance.source_agent", |value| {
            value.provenance.source_agent = Some("LEAF_SECRET".into())
        }),
        ("provenance.source_client", |value| {
            value.provenance.source_client = Some("LEAF_SECRET".into())
        }),
        ("provenance.commit", |value| {
            value.provenance.commit = Some("LEAF_SECRET".into())
        }),
        ("provenance.pull_request", |value| {
            value.provenance.pull_request = Some("LEAF_SECRET".into())
        }),
        ("provenance.issue", |value| {
            value.provenance.issue = Some("LEAF_SECRET".into())
        }),
        ("idempotency_key", |value| {
            value.idempotency_key = "LEAF_SECRET".into()
        }),
        ("query", |value| value.query = "LEAF_SECRET".into()),
    ];
    let leaf_filter = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "reject-leaf".into(),
            "LEAF_SECRET".into(),
            PrivacyAction::Reject,
        )
        .expect("valid reject rule"),
    ])
    .expect("valid filter");
    for (field, inject) in privacy_leaf_cases {
        let mut case = clean_input.clone();
        inject(&mut case);
        assert_eq!(
            leaf_filter.produce(&case, &limits(), &mut producer),
            Err(PrivacyError::Rejected {
                field,
                rule_id: "reject-leaf".into(),
            })
        );
        assert!(producer.persisted.is_empty());
        assert!(producer.derived.is_empty());
        assert!(producer.logged.is_empty());
        assert!(producer.provider_requests.is_empty());
    }

    let rejected = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "reject-sentinel".into(),
            SENTINELS[4].into(),
            PrivacyAction::Reject,
        )
        .expect("valid reject rule"),
    ])
    .expect("valid filter")
    .produce(&input, &limits(), &mut producer)
    .expect_err("reject rule must stop dispatch");
    assert!(matches!(
        rejected,
        PrivacyError::Rejected {
            field: "query",
            ref rule_id,
        } if rule_id == "reject-sentinel"
    ));
    assert!(
        SENTINELS
            .iter()
            .all(|sentinel| !format!("{rejected:?}").contains(sentinel))
    );
    assert!(producer.persisted.is_empty());
    assert!(producer.derived.is_empty());
    assert!(producer.logged.is_empty());
    assert!(producer.provider_requests.is_empty());

    let redaction_filter = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "cannot-remove-sentinel".into(),
            SENTINELS[1].into(),
            PrivacyAction::Redact {
                replacement: SENTINELS[1].into(),
            },
        )
        .expect("valid redact rule"),
    ])
    .expect("valid filter");
    let redaction_failure = redaction_filter
        .produce(&input, &limits(), &mut producer)
        .expect_err("a final matching value must fail instead of being dispatched");
    assert!(matches!(
        redaction_failure,
        PrivacyError::RedactionFailed {
            field: "body",
            ref rule_id,
        } if rule_id == "cannot-remove-sentinel"
    ));
    assert!(
        SENTINELS
            .iter()
            .all(|sentinel| !format!("{redaction_failure:?}").contains(sentinel))
    );

    let bounds_failure = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "expanding-redaction".into(),
            "TOKEN".into(),
            PrivacyAction::Redact {
                replacement: "x".repeat(33),
            },
        )
        .expect("valid redact rule"),
    ])
    .expect("valid filter")
    .apply("body", "TOKEN", limits().max_body_bytes);
    assert_eq!(
        bounds_failure,
        Err(PrivacyError::RedactionFailed {
            field: "body",
            rule_id: "expanding-redaction".into(),
        })
    );

    let empty_title = PrivacyFilter::try_new(vec![])
        .expect("valid filter")
        .produce(
            &PrivacyProducerInput {
                title: "   ".into(),
                ..input.clone()
            },
            &limits(),
            &mut producer,
        );
    assert_eq!(
        empty_title,
        Err(PrivacyError::RedactionFailed {
            field: "title",
            rule_id: "output_bounds".into(),
        })
    );

    let empty_query = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "remove-query".into(),
            SENTINELS[4].into(),
            PrivacyAction::Redact {
                replacement: String::new(),
            },
        )
        .expect("valid redact rule"),
    ])
    .expect("valid filter")
    .produce(&input, &limits(), &mut producer);
    assert_eq!(
        empty_query,
        Err(PrivacyError::RedactionFailed {
            field: "query",
            rule_id: "remove-query".into(),
        })
    );

    assert!(producer.persisted.is_empty());
    assert!(producer.derived.is_empty());
    assert!(producer.logged.is_empty());
    assert!(producer.provider_requests.is_empty());
}
