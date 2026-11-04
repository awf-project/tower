use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use extension_protocol::messages::{
    AgentMemoryCondition, AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryKeyValue,
    AgentMemoryMutation, AgentMemoryScanRequest, AgentMemoryScanResult, AgentMemoryStorageFailure,
    AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::*;

#[derive(Clone, Default)]
struct MemoryStoreDouble {
    state: Arc<Mutex<StoreState>>,
}

#[derive(Default)]
struct StoreState {
    values: BTreeMap<Vec<u8>, Vec<u8>>,
    generation: u64,
    durable: bool,
    next_failure: Option<AgentMemoryStorageFailure>,
    next_get_failure: Option<AgentMemoryStorageFailure>,
    next_scan_failure: Option<AgentMemoryStorageFailure>,
    next_transaction_failure: Option<AgentMemoryStorageFailure>,
    next_transaction_outcome: Option<AgentMemoryTransactionOutcome>,
    transaction_mutation_limit: Option<usize>,
    transaction_requests: Vec<AgentMemoryTransactionRequest>,
    committed_transactions: Vec<AgentMemoryTransactionRequest>,
    get_generations: VecDeque<u64>,
    checkpoint_before_next_transaction: Option<Vec<u8>>,
}

impl MemoryStoreDouble {
    fn durable() -> Self {
        let store = Self::default();
        store.state.lock().unwrap().durable = true;
        store
    }

    fn fail_next(&self, failure: AgentMemoryStorageFailure) {
        self.state.lock().unwrap().next_failure = Some(failure);
    }

    fn fail_next_get(&self, failure: AgentMemoryStorageFailure) {
        self.state.lock().unwrap().next_get_failure = Some(failure);
    }

    fn fail_next_scan(&self, failure: AgentMemoryStorageFailure) {
        self.state.lock().unwrap().next_scan_failure = Some(failure);
    }

    fn return_next(&self, outcome: AgentMemoryTransactionOutcome) {
        self.state.lock().unwrap().next_transaction_outcome = Some(outcome);
    }

    fn fail_next_transaction(&self, failure: AgentMemoryStorageFailure) {
        self.state.lock().unwrap().next_transaction_failure = Some(failure);
    }

    fn limit_transaction_mutations(&self, limit: usize) {
        self.state.lock().unwrap().transaction_mutation_limit = Some(limit);
    }

    fn snapshot(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.state.lock().unwrap().values.clone()
    }

    fn seed(&self, key: &[u8], value: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .values
            .insert(key.to_vec(), value.to_vec());
    }

    fn remove(&self, key: &[u8]) {
        self.state.lock().unwrap().values.remove(key);
    }

    fn transaction_requests(&self) -> Vec<AgentMemoryTransactionRequest> {
        self.state.lock().unwrap().transaction_requests.clone()
    }

    fn committed_transactions(&self) -> Vec<AgentMemoryTransactionRequest> {
        self.state.lock().unwrap().committed_transactions.clone()
    }

    fn return_get_generations(&self, generations: impl IntoIterator<Item = u64>) {
        self.state.lock().unwrap().get_generations = generations.into_iter().collect();
    }

    fn change_checkpoint_before_next_transaction(&self, value: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .checkpoint_before_next_transaction = Some(value.to_vec());
    }
}

impl MemoryStore for MemoryStoreDouble {
    fn get(
        &self,
        request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageFailure> {
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = state.next_get_failure.take() {
            return Err(failure);
        }
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        let generation = state
            .get_generations
            .pop_front()
            .unwrap_or(state.generation);
        Ok(AgentMemoryGetResult {
            value: state.values.get(&request.key).cloned(),
            generation,
        })
    }

    fn scan(
        &self,
        request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageFailure> {
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = state.next_scan_failure.take() {
            return Err(failure);
        }
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        let mut matching = state
            .values
            .range(request.prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&request.prefix))
            .filter(|(key, _)| request.cursor.as_ref().is_none_or(|cursor| *key > cursor))
            .map(|(key, value)| AgentMemoryKeyValue {
                key: key.clone(),
                value: value.clone(),
            })
            .collect::<Vec<_>>();
        let has_more = matching.len() > request.limit as usize;
        matching.truncate(request.limit as usize);
        let next_cursor = has_more.then(|| matching.last().unwrap().key.clone());
        Ok(AgentMemoryScanResult {
            entries: matching,
            next_cursor,
            generation: state.generation,
        })
    }

    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageFailure> {
        let mut state = self.state.lock().unwrap();
        state.transaction_requests.push(request.clone());
        if let Some(value) = state.checkpoint_before_next_transaction.take() {
            state.values.insert(b"meta/migration".to_vec(), value);
        }
        if let Some(failure) = state.next_transaction_failure.take() {
            return Err(failure);
        }
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        if state
            .transaction_mutation_limit
            .is_some_and(|limit| request.mutations.len() > limit)
        {
            return Err(AgentMemoryStorageFailure::BoundsExceeded);
        }
        if let Some(outcome) = state.next_transaction_outcome.take() {
            return Ok(outcome);
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
        state.committed_transactions.push(request.clone());
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
            durable: state.durable,
        })
    }
}

#[derive(Clone)]
struct FixedClock {
    wall: SystemTime,
    monotonic: Duration,
}

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.wall
    }

    fn monotonic_now(&self) -> Duration {
        self.monotonic
    }
}

#[derive(Clone)]
struct SequenceIds(Arc<Mutex<VecDeque<MemoryId>>>);

impl SequenceIds {
    fn new(ids: &[&str]) -> Self {
        Self(Arc::new(Mutex::new(
            ids.iter().map(|id| memory_id(id)).collect(),
        )))
    }
}

impl IdGenerator for SequenceIds {
    fn next_id(&self) -> MemoryId {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("ID fixture exhausted")
    }
}

#[derive(Clone, Default)]
struct ProviderDouble {
    calls: Arc<AtomicUsize>,
    result: Arc<Mutex<Option<Result<EnrichmentResult, EnrichmentProviderError>>>>,
    observed: Arc<Mutex<Option<(Duration, bool)>>>,
}

impl EnrichmentProvider for ProviderDouble {
    fn enrich(
        &self,
        _request: EnrichmentRequest,
        deadline: Duration,
        cancelled: &AtomicBool,
    ) -> Result<EnrichmentResult, EnrichmentProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.observed.lock().unwrap() = Some((deadline, cancelled.load(Ordering::Acquire)));
        self.result
            .lock()
            .unwrap()
            .take()
            .unwrap_or(Err(EnrichmentProviderError::Unavailable))
    }
}

type App = MemoryApplication<MemoryStoreDouble, FixedClock, SequenceIds, ProviderDouble>;

fn limits() -> AgentMemoryLimitsConfig {
    AgentMemoryLimitsConfig {
        max_title_bytes: 128,
        max_body_bytes: 1024,
        max_metadata_items: 16,
        max_metadata_string_bytes: 128,
        max_batch_items: 16,
        default_results: 10,
        max_results: 16,
        default_response_bytes: 4096,
        max_response_bytes: 8192,
        max_query_bytes: 256,
    }
}

fn memory_id(value: &str) -> MemoryId {
    MemoryId::try_from(value.to_owned()).unwrap()
}

fn revision(value: &str) -> Revision {
    Revision::try_from(value.to_owned()).unwrap()
}

fn input(title: &str) -> RecordInput {
    let provenance = Provenance::try_new(
        Some("main".into()),
        vec!["src/lib.rs".into()],
        vec!["MemoryApplication".into()],
        vec!["memory".into()],
        Some("tester".into()),
        Some("codex".into()),
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    RecordInput::try_new(
        MemoryType::Observation,
        title.into(),
        format!("body for {title}"),
        provenance,
        Some(0.8),
        &limits(),
    )
    .unwrap()
}

fn remember(record: RecordInput, key: &str) -> RememberCommand {
    RememberCommand::try_new(record, key.into(), &limits()).unwrap()
}

fn app(store: MemoryStoreDouble, ids: &[&str]) -> App {
    MemoryApplication::new(
        store,
        FixedClock {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            monotonic: Duration::from_secs(10),
        },
        SequenceIds::new(ids),
        None,
    )
}

fn app_in_mode(
    store: MemoryStoreDouble,
    ids: &[&str],
    mode: PersistenceMode,
    durable_data_unavailable: bool,
) -> App {
    MemoryApplication::new_configured(
        store,
        FixedClock {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            monotonic: Duration::from_secs(10),
        },
        SequenceIds::new(ids),
        None,
        PrivacyFilter::default(),
        limits(),
        mode,
        durable_data_unavailable,
        Duration::from_millis(5_000),
    )
}

fn app_at(store: MemoryStoreDouble, ids: &[&str], seconds: u64) -> App {
    MemoryApplication::new(
        store,
        FixedClock {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
            monotonic: Duration::from_secs(10),
        },
        SequenceIds::new(ids),
        None,
    )
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
        &limits(),
    )
    .unwrap()
}

fn page(limit: u32) -> Page {
    Page::try_new(
        Some(limit),
        Some(limits().max_response_bytes),
        None,
        &limits(),
    )
    .unwrap()
}

fn search_query(query: &str, page: Page) -> SearchQuery {
    SearchQuery::try_new(
        query.into(),
        empty_filters(),
        page,
        RankingMode::LexicalV1,
        &limits(),
    )
    .unwrap()
}

fn input_with(title: &str, body: &str, branch: &str, path: &str) -> RecordInput {
    RecordInput::try_new(
        MemoryType::Observation,
        title.into(),
        body.into(),
        Provenance::try_new(
            Some(branch.into()),
            vec![path.into()],
            vec![],
            vec![],
            Some("tester".into()),
            Some("codex".into()),
            None,
            None,
            None,
            &limits(),
        )
        .unwrap(),
        Some(0.8),
        &limits(),
    )
    .unwrap()
}

#[test]
fn search_filters_compact_hits_and_breaks_ties_deterministically() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["b", "a", "filtered"]);
    application
        .remember(remember(
            input_with("needle beta", "unrelated", "main", "src/b.rs"),
            "b-key",
        ))
        .unwrap();
    application
        .remember(remember(
            input_with("needle alpha", "unrelated", "main", "src/a.rs"),
            "a-key",
        ))
        .unwrap();
    application
        .remember(remember(
            input_with("needle filtered", "unrelated", "other", "src/x.rs"),
            "filtered-key",
        ))
        .unwrap();
    let filters = MemoryFilters::try_new(
        vec![],
        vec!["main".into()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();

    let result = application
        .search(
            SearchQuery::try_new(
                "needle".into(),
                filters,
                page(10),
                RankingMode::LexicalV1,
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();

    assert_eq!(result.hits.len(), 2);
    assert!(format!("{:?}", result.hits[0]).contains("MemoryId(\"a\")"));
    assert!(format!("{:?}", result.hits[1]).contains("MemoryId(\"b\")"));
    let compact = format!("{result:?}");
    assert!(!compact.contains("unrelated"));
    assert!(!compact.contains("body for"));
    assert!(!compact.contains("filtered"));
}

#[test]
fn timeline_orders_creation_events_for_anchor_query_and_window_modes() {
    let store = MemoryStoreDouble::durable();
    for (id, timestamp) in [
        // a/b deliberately tie: timeline order must fall back to the ID at both
        // sides of a window boundary.
        ("b", 1_800_000_000),
        ("a", 1_800_000_000),
        ("c", 1_800_000_001),
        ("filtered", 1_800_000_001),
        ("d", 1_800_000_002),
    ] {
        let title = if id == "a" {
            "needle event a".to_owned()
        } else {
            format!("event {id}")
        };
        app_at(store.clone(), &[id], timestamp)
            .remember(remember(
                input_with(
                    &title,
                    "needle",
                    if id == "filtered" { "other" } else { "main" },
                    "src/lib.rs",
                ),
                &format!("{id}-key"),
            ))
            .unwrap();
    }

    let record = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Record {
                    id: memory_id("c"),
                    before: 1,
                    after: 1,
                },
                empty_filters(),
                page(2),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    let (first_page, cursor) = match record {
        TimelineResult::Found {
            events,
            next_cursor,
            ..
        } => (events, next_cursor.unwrap()),
        TimelineResult::NotFound { .. } => panic!("known anchor must be found"),
    };
    assert_eq!(first_page.len(), 2);
    assert!(format!("{:?}", first_page[0]).contains("MemoryId(\"b\")"));
    assert!(format!("{:?}", first_page[1]).contains("MemoryId(\"c\")"));
    let continued = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Record {
                    id: memory_id("c"),
                    before: 1,
                    after: 1,
                },
                empty_filters(),
                Page::try_new(
                    Some(2),
                    Some(limits().max_response_bytes),
                    Some(cursor),
                    &limits(),
                )
                .unwrap(),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(continued, TimelineResult::Found { ref events, .. } if events.len() == 1 && format!("{:?}", events[0]).contains("MemoryId(\"filtered\")"))
    );

    let main_only = MemoryFilters::try_new(
        vec![],
        vec!["main".into()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    let filtered_neighborhood = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Record {
                    id: memory_id("d"),
                    before: 1,
                    after: 0,
                },
                main_only,
                page(10),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        filtered_neighborhood,
        TimelineResult::Found { ref events, .. }
            if events.len() == 2
                && format!("{:?}", events[0]).contains("MemoryId(\"c\")")
                && format!("{:?}", events[1]).contains("MemoryId(\"d\")")
    ));

    let query = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Query {
                    query: "needle".into(),
                },
                empty_filters(),
                page(10),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(query, TimelineResult::Found { ref events, .. } if events.len() == 5 && format!("{:?}", events[0]).contains("MemoryId(\"a\")"))
    );
    let empty = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Query {
                    query: "absent".into(),
                },
                empty_filters(),
                page(10),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(empty, TimelineResult::Found { ref events, .. } if events.is_empty()));

    let window = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Window {
                    from: "2027-01-15T08:00:00Z".into(),
                    to: "2027-01-15T08:00:01Z".into(),
                },
                empty_filters(),
                page(10),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(window, TimelineResult::Found { ref events, .. } if events.len() == 2 && format!("{:?}", events[0]).contains("MemoryId(\"a\")") && format!("{:?}", events[1]).contains("MemoryId(\"b\")"))
    );

    let fractional_store = MemoryStoreDouble::durable();
    for (id, offset) in [
        ("whole-second", Duration::ZERO),
        ("fractional-second", Duration::from_millis(900)),
    ] {
        MemoryApplication::new(
            fractional_store.clone(),
            FixedClock {
                wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000) + offset,
                monotonic: Duration::from_secs(10),
            },
            SequenceIds::new(&[id]),
            None::<ProviderDouble>,
        )
        .remember(remember(
            input_with("needle", "event", "main", "src/lib.rs"),
            &format!("{id}-key"),
        ))
        .unwrap();
    }
    let fractional_window = app(fractional_store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Window {
                    from: "2027-01-15T08:00:00Z".into(),
                    to: "2027-01-15T08:00:00.9Z".into(),
                },
                empty_filters(),
                page(10),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        matches!(fractional_window, TimelineResult::Found { ref events, .. }
            if events.len() == 1
                && format!("{:?}", events[0]).contains("MemoryId(\"whole-second\")"))
    );
    let fractional_search = app(fractional_store, &[])
        .search(search_query("needle", page(10)))
        .unwrap();
    assert!(format!("{:?}", fractional_search.hits[0]).contains("MemoryId(\"fractional-second\")"));

    let filtered = MemoryFilters::try_new(
        vec![],
        vec!["main".into()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    assert!(matches!(
        app(store.clone(), &[]).timeline(TimelineQuery::try_new(
            TimelineAnchor::Record { id: memory_id("filtered"), before: 0, after: 0 },
            filtered,
            page(1),
            &limits(),
        ).unwrap()).unwrap(),
        TimelineResult::Found { ref events, .. } if events.is_empty()
    ));

    let instant = "2027-01-15T08:00:01Z".to_owned();
    let equal = app(store.clone(), &[])
        .timeline(
            TimelineQuery::try_new(
                TimelineAnchor::Window {
                    from: instant.clone(),
                    to: instant,
                },
                empty_filters(),
                page(10),
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(equal, TimelineResult::Found { ref events, .. } if events.is_empty()));
    let before = store.snapshot();
    assert_eq!(
        TimelineQuery::try_new(
            TimelineAnchor::Window {
                from: "2027-01-15T08:00:02Z".into(),
                to: "2027-01-15T08:00:01Z".into()
            },
            empty_filters(),
            page(10),
            &limits()
        ),
        Err(MemoryApplicationError::InvalidArgument {
            field: "to",
            reason: "before_from"
        })
    );
    assert_eq!(store.snapshot(), before);
    for (anchor, field) in [
        (
            TimelineAnchor::Record {
                id: memory_id("a"),
                before: limits().max_results,
                after: 0,
            },
            "before",
        ),
        (
            TimelineAnchor::Record {
                id: memory_id("a"),
                before: 1,
                after: limits().max_results,
            },
            "after",
        ),
    ] {
        assert_eq!(
            TimelineQuery::try_new(anchor, empty_filters(), page(1), &limits()),
            Err(MemoryApplicationError::InvalidArgument {
                field,
                reason: "out_of_range"
            })
        );
        assert_eq!(store.snapshot(), before);
    }
    let max_results = limits().max_results;
    assert_eq!(
        TimelineQuery::try_new(
            TimelineAnchor::Record {
                id: memory_id("a"),
                before: max_results - 1,
                after: 1,
            },
            empty_filters(),
            page(1),
            &limits(),
        ),
        Err(MemoryApplicationError::InvalidArgument {
            field: "after",
            reason: "out_of_range",
        })
    );
    assert_eq!(store.snapshot(), before);
    assert!(matches!(
        app(store, &[])
            .timeline(
                TimelineQuery::try_new(
                    TimelineAnchor::Record {
                        id: memory_id("missing"),
                        before: 0,
                        after: 0
                    },
                    empty_filters(),
                    page(1),
                    &limits()
                )
                .unwrap()
            )
            .unwrap(),
        TimelineResult::NotFound { .. }
    ));
}

#[test]
fn task_context_requires_lexical_or_explicit_metadata_relevance_without_provider() {
    let store = MemoryStoreDouble::durable();
    let application = app(store, &["lexical", "path", "unrelated"]);
    application
        .remember(remember(
            input_with("deploy guide", "steps", "main", "docs/deploy.md"),
            "lexical-key",
        ))
        .unwrap();
    application
        .remember(remember(
            input_with("operations", "notes", "main", "src/task.rs"),
            "path-key",
        ))
        .unwrap();
    application
        .remember(remember(
            input_with("unrelated", "noise", "other", "src/noise.rs"),
            "noise-key",
        ))
        .unwrap();

    let lexical = application
        .context_for_task(
            TaskContextQuery::try_new(
                "deploy".into(),
                empty_filters(),
                page(10),
                RankingMode::LexicalV1,
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(lexical.hits.len(), 1);
    assert!(format!("{:?}", lexical.hits[0]).contains("MemoryId(\"lexical\")"));

    let path_filter = MemoryFilters::try_new(
        vec![],
        vec![],
        vec!["src/task.rs".into()],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    let explicit = application
        .context_for_task(
            TaskContextQuery::try_new(
                "no lexical match".into(),
                path_filter.clone(),
                page(10),
                RankingMode::LexicalV1,
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(explicit.hits.len(), 1);
    assert_eq!(explicit.applied_filters, path_filter);
    assert!(format!("{:?}", explicit.hits[0]).contains("MemoryId(\"path\")"));

    let branch_filter = MemoryFilters::try_new(
        vec![],
        vec!["main".into()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    let branch_matches = application
        .context_for_task(
            TaskContextQuery::try_new(
                "no lexical match".into(),
                branch_filter,
                page(10),
                RankingMode::LexicalV1,
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(branch_matches.hits.len(), 2);
    for hit in &branch_matches.hits {
        assert_eq!(
            hit.reasons(),
            &[MatchReason::new(
                MatchReasonKind::Metadata,
                MatchReasonField::Branch,
            )]
        );
    }

    let none = application
        .context_for_task(
            TaskContextQuery::try_new(
                "absent".into(),
                empty_filters(),
                page(10),
                RankingMode::LexicalV1,
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(none.hits.is_empty());
}

#[test]
fn compact_pages_respect_utf8_and_total_response_budgets() {
    let mut constrained = limits();
    constrained.max_title_bytes = 9;
    constrained.default_response_bytes = 512;
    constrained.max_response_bytes = 512;
    let store = MemoryStoreDouble::durable();
    let seeder = app(store.clone(), &["utf8-a", "utf8-b", "utf8-c"]);
    for (id, path) in [("a", "src/a.rs"), ("b", "src/b.rs"), ("c", "src/c.rs")] {
        seeder
            .remember(remember(
                input_with("ééééé needle", "x", "main", path),
                &format!("utf8-{id}-key"),
            ))
            .unwrap();
    }
    let application = MemoryApplication::new_with_privacy(
        store,
        FixedClock {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            monotonic: Duration::ZERO,
        },
        SequenceIds::new(&[]),
        None::<ProviderDouble>,
        PrivacyFilter::default(),
        constrained.clone(),
    );
    let full_page = application
        .dispatch_retrieval(
            "memory_search",
            json!({"query": "needle", "page": {"limit": 1, "response_bytes": 512}}),
        )
        .unwrap();
    let one_hit_bytes = serde_json::to_vec(&full_page).unwrap().len() as u32;
    assert_eq!(full_page["hits"].as_array().unwrap().len(), 1);
    assert!(
        full_page["hits"][0]["truncated_fields"]
            .as_array()
            .unwrap()
            .contains(&json!("title"))
    );
    assert_eq!(full_page["hits"][0]["title"], "éééé");

    let rejected = application
        .dispatch_retrieval(
            "memory_search",
            json!({"query": "needle", "page": {"limit": 3, "response_bytes": one_hit_bytes - 1}}),
        )
        .unwrap();
    assert_eq!(rejected["code"], "response_budget_too_small");
    assert_eq!(rejected["minimum_bytes"], one_hit_bytes);
    let result = application
        .dispatch_retrieval(
            "memory_search",
            json!({"query": "needle", "page": {"limit": 3, "response_bytes": one_hit_bytes}}),
        )
        .unwrap();
    assert_eq!(result["hits"].as_array().unwrap().len(), 1);
    assert!(result["next_cursor"].is_string());
    assert_eq!(
        serde_json::to_vec(&result).unwrap().len(),
        one_hit_bytes as usize
    );

    let empty = app(MemoryStoreDouble::durable(), &[]);
    for (tool, params) in [
        (
            "memory_search",
            json!({"query": "absent", "page": {"response_bytes": 1}}),
        ),
        (
            "memory_timeline",
            json!({
                "anchor": {"kind": "query", "query": "absent"},
                "page": {"response_bytes": 1}
            }),
        ),
        (
            "memory_context_for_task",
            json!({"query": "absent", "page": {"response_bytes": 1}}),
        ),
    ] {
        let rejected = empty.dispatch_retrieval(tool, params).unwrap();
        assert_eq!(rejected["code"], "response_budget_too_small");
        assert!(rejected["minimum_bytes"].as_u64().unwrap() > 1);
    }
}

#[test]
fn unchanged_store_pages_are_complete_without_duplicates() {
    let store = MemoryStoreDouble::durable();
    let application = app(store, &["a", "b", "c"]);
    for id in ["a", "b", "c"] {
        application
            .remember(remember(
                input_with(&format!("needle {id}"), "x", "main", "src/lib.rs"),
                &format!("key-{id}"),
            ))
            .unwrap();
    }
    let mut cursor = None;
    let mut observed = Vec::new();
    loop {
        let result = application
            .search(search_query(
                "needle",
                Page::try_new(
                    Some(1),
                    Some(limits().max_response_bytes),
                    cursor,
                    &limits(),
                )
                .unwrap(),
            ))
            .unwrap();
        observed.extend(result.hits.iter().map(|hit| format!("{hit:?}")));
        cursor = result.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(observed.len(), 3);
    observed.sort();
    observed.dedup();
    assert_eq!(observed.len(), 3);
}

#[test]
fn retrieval_mutation_expires_existing_cursors() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["a", "b", "new"]);
    let remembered = ["a", "b"]
        .into_iter()
        .map(|id| {
            application
                .remember(remember(
                    input_with(&format!("needle {id}"), "x", "main", "src/lib.rs"),
                    &format!("key-{id}"),
                ))
                .unwrap()
        })
        .collect::<Vec<_>>();
    let cursor = application
        .search(search_query("needle", page(1)))
        .unwrap()
        .next_cursor
        .unwrap();
    application
        .remember(remember(
            input_with("needle new", "x", "main", "src/lib.rs"),
            "new-key",
        ))
        .unwrap();
    let before = store.snapshot();
    let error = application
        .search(search_query(
            "needle",
            Page::try_new(
                Some(1),
                Some(limits().max_response_bytes),
                Some(cursor),
                &limits(),
            )
            .unwrap(),
        ))
        .unwrap_err();
    assert_eq!(error, MemoryApplicationError::CursorExpired);
    assert_eq!(store.snapshot(), before);

    let cursor = application
        .search(search_query("needle", page(1)))
        .unwrap()
        .next_cursor
        .unwrap();
    let updated = application
        .update(
            UpdateCommand::try_new(
                vec![UpdateItem {
                    id: remembered[0].id.clone(),
                    expected_revision: remembered[0].revision.clone(),
                    record: input_with("needle a updated", "x", "main", "src/lib.rs"),
                }],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(updated.items[0].code, MutationOutcomeCode::Updated);
    let before = store.snapshot();
    assert_eq!(
        application
            .search(search_query(
                "needle",
                Page::try_new(
                    Some(1),
                    Some(limits().max_response_bytes),
                    Some(cursor),
                    &limits(),
                )
                .unwrap(),
            ))
            .unwrap_err(),
        MemoryApplicationError::CursorExpired
    );
    assert_eq!(store.snapshot(), before);

    let cursor = application
        .search(search_query("needle", page(1)))
        .unwrap()
        .next_cursor
        .unwrap();
    let forgotten = application
        .forget(
            ForgetCommand::try_new(
                vec![(remembered[1].id.clone(), remembered[1].revision.clone())],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(forgotten.items[0].code, MutationOutcomeCode::Forgotten);
    let before = store.snapshot();
    assert_eq!(
        application
            .search(search_query(
                "needle",
                Page::try_new(
                    Some(1),
                    Some(limits().max_response_bytes),
                    Some(cursor),
                    &limits(),
                )
                .unwrap(),
            ))
            .unwrap_err(),
        MemoryApplicationError::CursorExpired
    );
    assert_eq!(store.snapshot(), before);
}

#[test]
fn malformed_noncanonical_and_out_of_range_cursors_expire_without_mutating_store() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["a", "b"]);
    for id in ["a", "b"] {
        application
            .remember(remember(
                input_with(&format!("needle {id}"), "x", "main", "src/lib.rs"),
                &format!("key-{id}"),
            ))
            .unwrap();
    }
    let valid = application
        .search(search_query("needle", page(1)))
        .unwrap()
        .next_cursor
        .unwrap();
    let wire: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&valid).unwrap()).unwrap();
    let request_digest = wire["request_digest"].as_str().unwrap();
    let generation = wire["generation"].as_u64().unwrap();
    let encode =
        |value: serde_json::Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
    let noncanonical = URL_SAFE_NO_PAD.encode(
        format!(
            r#"{{\"offset\":1,\"v\":1,\"request_digest\":\"{request_digest}\",\"generation\":{generation}}}"#
        )
        .as_bytes(),
    );
    let cursors = vec![
        "not-base64".to_owned(),
        encode(
            json!({"v": 1, "request_digest": request_digest, "generation": generation, "offset": 0}),
        ),
        encode(
            json!({"v": 1, "request_digest": request_digest, "generation": generation, "offset": 3}),
        ),
        encode(
            json!({"v": 1, "request_digest": request_digest, "generation": generation, "offset": u64::MAX}),
        ),
        encode(
            json!({"v": 1, "request_digest": request_digest, "generation": generation, "offset": 1, "extra": true}),
        ),
        noncanonical,
    ];
    let before = store.snapshot();
    for cursor in cursors {
        assert_eq!(
            application
                .search(search_query(
                    "needle",
                    Page::try_new(
                        Some(1),
                        Some(limits().max_response_bytes),
                        Some(cursor),
                        &limits(),
                    )
                    .unwrap(),
                ))
                .unwrap_err(),
            MemoryApplicationError::CursorExpired
        );
        assert_eq!(store.snapshot(), before);
    }
}

#[test]
fn retrieval_cursors_expire_when_the_normalized_request_changes() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["a", "b"]);
    for id in ["a", "b"] {
        application
            .remember(remember(
                input_with(&format!("needle {id}"), "x", "main", "src/lib.rs"),
                &format!("key-{id}"),
            ))
            .unwrap();
    }
    let cursor = application
        .search(search_query("needle", page(1)))
        .unwrap()
        .next_cursor
        .unwrap();
    let before = store.snapshot();
    for query in ["other", " needle two "] {
        let error = application
            .search(
                SearchQuery::try_new(
                    query.into(),
                    empty_filters(),
                    Page::try_new(
                        Some(1),
                        Some(limits().max_response_bytes),
                        Some(cursor.clone()),
                        &limits(),
                    )
                    .unwrap(),
                    RankingMode::LexicalV1,
                    &limits(),
                )
                .unwrap(),
            )
            .unwrap_err();
        assert_eq!(error, MemoryApplicationError::CursorExpired);
        assert_eq!(store.snapshot(), before);
    }
    let filtered = MemoryFilters::try_new(
        vec![],
        vec!["main".into()],
        vec![],
        vec![],
        vec![],
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    for page in [
        Page::try_new(
            Some(2),
            Some(limits().max_response_bytes),
            Some(cursor.clone()),
            &limits(),
        )
        .unwrap(),
        Page::try_new(
            Some(1),
            Some(limits().max_response_bytes - 1),
            Some(cursor.clone()),
            &limits(),
        )
        .unwrap(),
    ] {
        let error = application
            .search(
                SearchQuery::try_new(
                    "needle".into(),
                    filtered.clone(),
                    page,
                    RankingMode::LexicalV1,
                    &limits(),
                )
                .unwrap(),
            )
            .unwrap_err();
        assert_eq!(error, MemoryApplicationError::CursorExpired);
        assert_eq!(store.snapshot(), before);
    }
}

#[test]
fn retrieval_query_constructors_reject_invalid_inputs_without_mutating_store() {
    let store = MemoryStoreDouble::durable();
    let before = store.snapshot();
    let validated_page = Page::try_new(None, None, None, &limits()).unwrap();
    assert_eq!(validated_page.limit(), limits().default_results);
    assert_eq!(
        validated_page.response_bytes(),
        limits().default_response_bytes
    );
    assert_eq!(validated_page.cursor(), None);
    assert_eq!(
        SearchQuery::try_new(
            " ".into(),
            empty_filters(),
            page(1),
            RankingMode::LexicalV1,
            &limits()
        ),
        Err(MemoryApplicationError::InvalidArgument {
            field: "query",
            reason: "empty"
        })
    );
    assert_eq!(store.snapshot(), before);
    assert_eq!(
        Page::try_new(Some(0), Some(limits().max_response_bytes), None, &limits()),
        Err(MemoryApplicationError::InvalidArgument {
            field: "limit",
            reason: "out_of_range"
        })
    );
    assert_eq!(store.snapshot(), before);
    assert_eq!(
        Page::try_new(
            Some(limits().max_results + 1),
            Some(limits().max_response_bytes),
            None,
            &limits()
        ),
        Err(MemoryApplicationError::InvalidArgument {
            field: "limit",
            reason: "out_of_range"
        })
    );
    assert_eq!(store.snapshot(), before);
    assert_eq!(
        Page::try_new(
            Some(1),
            Some(limits().max_response_bytes + 1),
            None,
            &limits()
        ),
        Err(MemoryApplicationError::InvalidArgument {
            field: "response_bytes",
            reason: "out_of_range"
        })
    );
    assert_eq!(store.snapshot(), before);
}

#[test]
fn one_unfittable_hit_returns_response_budget_too_small() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["large"]);
    application
        .remember(remember(
            input_with(
                "needle with a sizeable compact title",
                "x",
                "main",
                "src/lib.rs",
            ),
            "large-key",
        ))
        .unwrap();
    let expected = application
        .dispatch_retrieval(
            "memory_search",
            json!({"query": "needle", "page": {"limit": 1, "response_bytes": 8192}}),
        )
        .unwrap();
    let minimum_bytes = serde_json::to_vec(&expected).unwrap().len();
    let before = store.snapshot();
    let error = application
        .search(search_query(
            "needle",
            Page::try_new(Some(1), Some(1), None, &limits()).unwrap(),
        ))
        .unwrap_err();
    assert_eq!(
        error,
        MemoryApplicationError::ResponseBudgetTooSmall { minimum_bytes }
    );
    assert_eq!(store.snapshot(), before);
}

#[test]
fn public_retrieval_entrypoints_share_real_application_wiring_and_surface_budget_failures() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["entry", "other"]);
    application
        .remember(remember(
            input_with("needle entry", "task context", "main", "src/entry.rs"),
            "entry-key",
        ))
        .unwrap();
    application
        .remember(remember(
            input_with("needle excluded", "private body", "other", "src/other.rs"),
            "other-key",
        ))
        .unwrap();

    let filters = json!({"branches": ["main"]});
    let search = application
        .dispatch_retrieval(
            "memory_search",
            json!({
                "query": "needle",
                "filters": filters,
                "page": {"limit": 10, "response_bytes": 8192}
            }),
        )
        .unwrap();
    assert_eq!(search["code"], "ok");
    assert_eq!(search["hits"].as_array().unwrap().len(), 1);
    assert_eq!(search["hits"][0]["title"], "needle entry");
    assert!(search["hits"][0].get("body").is_none());

    let timeline = application
        .dispatch_retrieval(
            "memory_timeline",
            json!({
                "anchor": {"kind": "query", "query": "needle"},
                "filters": {"branches": ["main"]},
                "page": {"limit": 10, "response_bytes": 8192}
            }),
        )
        .unwrap();
    assert_eq!(timeline["code"], "ok");
    assert_eq!(timeline["events"].as_array().unwrap().len(), 1);

    let context = application
        .dispatch_retrieval(
            "memory_context_for_task",
            json!({
                "query": "needle",
                "filters": {"paths": ["src/entry.rs"]},
                "page": {"limit": 10, "response_bytes": 8192}
            }),
        )
        .unwrap();
    assert_eq!(context["hits"].as_array().unwrap().len(), 1);
    assert_eq!(context["applied_filters"]["paths"][0], "src/entry.rs");

    let budget_failure = application
        .dispatch_retrieval(
            "memory_search",
            json!({"query": "needle", "page": {"limit": 1, "response_bytes": 1}}),
        )
        .unwrap();
    assert_eq!(budget_failure["code"], "response_budget_too_small");
    let expected_budget_envelope = application
        .dispatch_retrieval(
            "memory_search",
            json!({"query": "needle", "page": {"limit": 1, "response_bytes": 8192}}),
        )
        .unwrap();
    assert_eq!(
        budget_failure["minimum_bytes"].as_u64().unwrap() as usize,
        serde_json::to_vec(&expected_budget_envelope).unwrap().len()
    );

    let before = store.snapshot();
    for malformed in [
        json!({"anchor": {"kind": "record", "id": "entry", "after": 1}}),
        json!({"anchor": {"kind": "record", "id": "entry", "before": 1}}),
        json!({"anchor": {"kind": "unknown", "id": "entry"}}),
        json!({"anchor": {"kind": "query", "query": "needle", "before": 1}}),
    ] {
        assert_eq!(
            application.dispatch_retrieval("memory_timeline", malformed),
            Err(RetrievalDispatchError::InvalidParams)
        );
        assert_eq!(store.snapshot(), before);
    }
}

#[test]
fn commands_apply_privacy_before_hashing_and_persistence() {
    let store = MemoryStoreDouble::durable();
    let filter = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new(
            "reject-secret".into(),
            "SECRET".into(),
            PrivacyAction::Reject,
        )
        .unwrap(),
        PrivacyRule::try_new(
            "redact-token".into(),
            "TOKEN".into(),
            PrivacyAction::Redact {
                replacement: "[redacted]".into(),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let application = MemoryApplication::new_with_privacy(
        store.clone(),
        FixedClock {
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            monotonic: Duration::from_secs(10),
        },
        SequenceIds::new(&["private"]),
        None::<ProviderDouble>,
        filter,
        limits(),
    );

    let rejected = application
        .remember(remember(input("SECRET"), "safe-key"))
        .unwrap_err();
    assert_eq!(
        rejected,
        MemoryApplicationError::InvalidArgument {
            field: "title",
            reason: "privacy_rejected",
        }
    );
    assert!(store.transaction_requests().is_empty());

    let _remembered = application
        .remember(remember(input("TOKEN"), "TOKEN-key"))
        .unwrap();
    let snapshot = store.snapshot();
    let filtered_idempotency_digest = Sha256::digest(b"[redacted]-key")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(
        snapshot
            .keys()
            .any(|key| { key == format!("idempotency/{filtered_idempotency_digest}").as_bytes() })
    );
    let record = snapshot
        .iter()
        .find(|(key, _)| key.starts_with(b"record/"))
        .map(|(_, value)| value)
        .unwrap();
    assert!(!String::from_utf8_lossy(record).contains("TOKEN"));
    assert!(String::from_utf8_lossy(record).contains("[redacted]"));
}

#[test]
fn opaque_ids_are_base64url_encoded_in_storage_keys() {
    let store = MemoryStoreDouble::durable();
    let id = "id/é";
    let encoded = URL_SAFE_NO_PAD.encode(id.as_bytes());
    let application = app(store.clone(), &[id]);
    let remembered = application
        .remember(remember(input("encoded key"), "encoded-key"))
        .unwrap();

    let snapshot = store.snapshot();
    for prefix in ["record", "projection", "job"] {
        assert!(snapshot.contains_key(format!("{prefix}/{encoded}").as_bytes()));
        assert!(!snapshot.contains_key(format!("{prefix}/{id}").as_bytes()));
    }

    application
        .forget(
            ForgetCommand::try_new(vec![(remembered.id, remembered.revision)], &limits()).unwrap(),
        )
        .unwrap();
    let snapshot = store.snapshot();
    assert!(snapshot.contains_key(format!("tombstone/{encoded}").as_bytes()));
    assert!(!snapshot.contains_key(format!("tombstone/{id}").as_bytes()));
}

#[test]
fn batch_get_rejects_persistently_mixed_generations() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["first", "second"]);
    let first = application
        .remember(remember(input("first"), "first-key"))
        .unwrap();
    let second = application
        .remember(remember(input("second"), "second-key"))
        .unwrap();
    store.return_get_generations(0..16);

    let error = application
        .get(GetQuery::try_new(vec![first.id, second.id], &limits()).unwrap())
        .unwrap_err();

    assert_eq!(error, MemoryApplicationError::CursorExpired);
}

#[test]
fn remember_retries_preserve_stable_identity() {
    let application = app(MemoryStoreDouble::durable(), &["memory-a", "memory-b"]);
    let first = application
        .remember(remember(input("stable"), "request-a"))
        .unwrap();
    let retried = application
        .remember(remember(input("stable"), "request-a"))
        .unwrap();

    assert_eq!(retried.id, first.id);
    assert_eq!(retried.revision, first.revision);
    assert!(retried.durable);
}

#[test]
fn batch_mutations_report_duplicate_unknown_and_successful_items_independently() {
    let application = app(
        MemoryStoreDouble::durable(),
        &[
            "known",
            "other",
            "stale",
            "update-sibling",
            "forget-sibling",
            "forgotten",
        ],
    );
    let known = application
        .remember(remember(input("known"), "known-key"))
        .unwrap();
    let other = application
        .remember(remember(input("other"), "other-key"))
        .unwrap();
    let command = UpdateCommand::try_new(
        vec![
            UpdateItem {
                id: known.id.clone(),
                expected_revision: known.revision.clone(),
                record: input("duplicate one"),
            },
            UpdateItem {
                id: memory_id("missing"),
                expected_revision: revision("1"),
                record: input("missing"),
            },
            UpdateItem {
                id: known.id.clone(),
                expected_revision: known.revision,
                record: input("duplicate two"),
            },
            UpdateItem {
                id: other.id.clone(),
                expected_revision: other.revision,
                record: input("updated"),
            },
        ],
        &limits(),
    )
    .unwrap();

    let result = application.update(command).unwrap();
    assert_eq!(
        result
            .items
            .iter()
            .map(|item| item.code)
            .collect::<Vec<_>>(),
        vec![
            MutationOutcomeCode::DuplicateId,
            MutationOutcomeCode::NotFound,
            MutationOutcomeCode::DuplicateId,
            MutationOutcomeCode::Updated,
        ]
    );

    let stale = application
        .remember(remember(input("stale"), "stale-key"))
        .unwrap();
    let update_sibling = application
        .remember(remember(input("update sibling"), "update-sibling-key"))
        .unwrap();
    let advanced = application
        .update(
            UpdateCommand::try_new(
                vec![UpdateItem {
                    id: stale.id.clone(),
                    expected_revision: stale.revision.clone(),
                    record: input("advanced"),
                }],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(advanced.items[0].code, MutationOutcomeCode::Updated);

    let stale_update = application
        .update(
            UpdateCommand::try_new(
                vec![
                    UpdateItem {
                        id: stale.id.clone(),
                        expected_revision: stale.revision.clone(),
                        record: input("stale update"),
                    },
                    UpdateItem {
                        id: update_sibling.id.clone(),
                        expected_revision: update_sibling.revision,
                        record: input("independent update"),
                    },
                ],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stale_update
            .items
            .iter()
            .map(|item| item.code)
            .collect::<Vec<_>>(),
        vec![
            MutationOutcomeCode::RevisionConflict,
            MutationOutcomeCode::Updated
        ]
    );

    let forget_sibling = application
        .remember(remember(input("forget sibling"), "forget-sibling-key"))
        .unwrap();
    let stale_forget = application
        .forget(
            ForgetCommand::try_new(
                vec![
                    (stale.id.clone(), stale.revision.clone()),
                    (forget_sibling.id.clone(), forget_sibling.revision),
                ],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        stale_forget
            .items
            .iter()
            .map(|item| item.code)
            .collect::<Vec<_>>(),
        vec![
            MutationOutcomeCode::RevisionConflict,
            MutationOutcomeCode::Forgotten
        ]
    );

    let forgotten = application
        .remember(remember(input("forgotten"), "forgotten-key"))
        .unwrap();
    let unknown_and_valid = application
        .forget(
            ForgetCommand::try_new(
                vec![
                    (memory_id("unknown"), revision("1")),
                    (forgotten.id.clone(), forgotten.revision.clone()),
                ],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        unknown_and_valid
            .items
            .iter()
            .map(|item| item.code)
            .collect::<Vec<_>>(),
        vec![
            MutationOutcomeCode::NotFound,
            MutationOutcomeCode::Forgotten
        ]
    );
    let already_forgotten = application
        .forget(
            ForgetCommand::try_new(vec![(forgotten.id, forgotten.revision)], &limits()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        already_forgotten.items[0].code,
        MutationOutcomeCode::NotFound
    );
}

#[test]
fn one_expected_revision_wins_concurrent_update_and_forget() {
    let application = Arc::new(app(MemoryStoreDouble::durable(), &["race"]));
    let original = application
        .remember(remember(input("race"), "race-key"))
        .unwrap();
    let update_app = Arc::clone(&application);
    let forget_app = Arc::clone(&application);
    let update_id = original.id.clone();
    let forget_id = original.id.clone();
    let update_revision = original.revision.clone();
    let forget_revision = original.revision;
    let update = std::thread::spawn(move || {
        update_app
            .update(
                UpdateCommand::try_new(
                    vec![UpdateItem {
                        id: update_id,
                        expected_revision: update_revision,
                        record: input("winner"),
                    }],
                    &limits(),
                )
                .unwrap(),
            )
            .unwrap()
    });
    let forget = std::thread::spawn(move || {
        forget_app
            .forget(ForgetCommand::try_new(vec![(forget_id, forget_revision)], &limits()).unwrap())
            .unwrap()
    });
    let codes = [
        update.join().unwrap().items[0].code,
        forget.join().unwrap().items[0].code,
    ];

    assert_eq!(
        codes
            .iter()
            .filter(|code| matches!(
                code,
                MutationOutcomeCode::Updated | MutationOutcomeCode::Forgotten
            ))
            .count(),
        1
    );
    assert_eq!(
        codes
            .iter()
            .filter(|code| matches!(
                code,
                MutationOutcomeCode::RevisionConflict | MutationOutcomeCode::NotFound
            ))
            .count(),
        1
    );
}

#[test]
fn identical_remember_retry_reuses_the_original_record() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["original", "unused"]);
    let first = application
        .remember(remember(input("same"), "same-key"))
        .unwrap();
    let transactions_before_retry = store.transaction_requests().len();
    let second = application
        .remember(remember(input("same"), "same-key"))
        .unwrap();
    let fetched = application
        .get(GetQuery::try_new(vec![first.id.clone()], &limits()).unwrap())
        .unwrap();

    assert_eq!(second.id, first.id);
    assert!(second.durable);
    let retry_transaction = store.transaction_requests().last().cloned().unwrap();
    assert_eq!(
        store.transaction_requests().len(),
        transactions_before_retry + 1
    );
    let idempotency_value = match retry_transaction.conditions.as_slice() {
        [AgentMemoryCondition::ValueEquals { key, value }] => {
            assert!(key.starts_with(b"idempotency/"));
            value
        }
        conditions => panic!("unexpected retry conditions: {conditions:?}"),
    };
    match retry_transaction.mutations.as_slice() {
        [AgentMemoryMutation::Put { key, value }] => {
            assert!(key.starts_with(b"idempotency/"));
            assert_eq!(value, idempotency_value);
        }
        mutations => panic!("unexpected retry mutations: {mutations:?}"),
    }
    assert!(
        matches!(&fetched.items[0], GetItemResult::Found { id, record } if id == &first.id && record.title() == "same")
    );
}

#[test]
fn identical_remember_retry_reports_unknown_commit_outcome() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["original", "unused"]);
    application
        .remember(remember(input("same"), "same-key"))
        .unwrap();
    let transactions_before_retry = store.transaction_requests().len();
    store.fail_next_transaction(AgentMemoryStorageFailure::CommitOutcomeUnknown);

    let error = application
        .remember(remember(input("same"), "same-key"))
        .unwrap_err();

    assert_eq!(error, MemoryApplicationError::CommitOutcomeUnknown);
    assert_eq!(
        store.transaction_requests().len(),
        transactions_before_retry + 1
    );
}

#[test]
fn conflicting_remember_retry_preserves_the_original_record() {
    let application = app(MemoryStoreDouble::durable(), &["original", "unused"]);
    let first = application
        .remember(remember(input("original"), "shared-key"))
        .unwrap();
    let error = application
        .remember(remember(input("changed"), "shared-key"))
        .unwrap_err();
    let fetched = application
        .get(GetQuery::try_new(vec![first.id.clone()], &limits()).unwrap())
        .unwrap();

    assert_eq!(
        error,
        MemoryApplicationError::InvalidArgument {
            field: "idempotency_key",
            reason: "idempotency_conflict"
        }
    );
    assert!(
        matches!(&fetched.items[0], GetItemResult::Found { record, .. } if record.title() == "original")
    );
}

#[test]
fn remember_retry_after_forget_does_not_resurrect_content() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["forgotten", "unused"]);
    let original = application
        .remember(remember(input("secret"), "forgotten-key"))
        .unwrap();
    let transactions_before_forget = store.transaction_requests().len();
    let committed_before_forget = store.committed_transactions().len();
    let forgotten = application
        .forget(
            ForgetCommand::try_new(vec![(original.id.clone(), original.revision)], &limits())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(forgotten.items[0].code, MutationOutcomeCode::Forgotten);

    let transactions_after_forget = store.transaction_requests();
    let committed_after_forget = store.committed_transactions();
    assert_eq!(
        transactions_after_forget.len(),
        transactions_before_forget + 1,
        "forget must publish all owned-key changes in one conditional transaction"
    );
    assert_eq!(committed_after_forget.len(), committed_before_forget + 1);
    let forget_transaction = committed_after_forget.last().unwrap();
    assert!(
        !forget_transaction.conditions.is_empty(),
        "forget must remain conditional on the expected revision"
    );
    for prefix in [b"record/".as_slice(), b"projection/", b"job/"] {
        assert!(
            forget_transaction.mutations.iter().any(|mutation| matches!(
                mutation,
                AgentMemoryMutation::Delete { key } if key.starts_with(prefix)
            )),
            "forget must delete its {prefix:?} entry"
        );
    }
    let snapshot = store.snapshot();
    assert!(snapshot.keys().all(|key| {
        !key.starts_with(b"record/")
            && !key.starts_with(b"projection/")
            && !key.starts_with(b"job/")
    }));
    assert!(snapshot.keys().any(|key| key.starts_with(b"tombstone/")));
    assert!(snapshot.keys().any(|key| key.starts_with(b"idempotency/")));
    assert!(snapshot.iter().all(|(key, value)| {
        !key.windows(b"secret".len())
            .any(|window| window == b"secret")
            && !value
                .windows(b"secret".len())
                .any(|window| window == b"secret")
    }));

    let error = application
        .remember(remember(input("secret"), "forgotten-key"))
        .unwrap_err();
    assert_eq!(
        error,
        MemoryApplicationError::InvalidArgument {
            field: "idempotency_key",
            reason: "record_forgotten"
        }
    );
    let fetched = application
        .get(GetQuery::try_new(vec![original.id], &limits()).unwrap())
        .unwrap();
    assert!(matches!(fetched.items[0], GetItemResult::NotFound { .. }));
}

#[test]
fn generated_id_collision_retries_without_overwrite() {
    let application = app(
        MemoryStoreDouble::durable(),
        &["collision", "collision", "fresh"],
    );
    let original = application
        .remember(remember(input("original"), "first-key"))
        .unwrap();
    let second = application
        .remember(remember(input("second"), "second-key"))
        .unwrap();

    assert_eq!(original.id, memory_id("collision"));
    assert_eq!(second.id, memory_id("fresh"));
    let fetched = application
        .get(GetQuery::try_new(vec![original.id, second.id], &limits()).unwrap())
        .unwrap();
    assert!(
        matches!(&fetched.items[0], GetItemResult::Found { record, .. } if record.title() == "original")
    );
    assert!(
        matches!(&fetched.items[1], GetItemResult::Found { record, .. } if record.title() == "second")
    );
}

#[test]
fn batch_get_preserves_request_order_and_repeated_ids() {
    let application = app(MemoryStoreDouble::durable(), &["a", "b"]);
    let a = application
        .remember(remember(input("alpha"), "key-a"))
        .unwrap();
    let b = application
        .remember(remember(input("beta"), "key-b"))
        .unwrap();
    let result = application
        .get(
            GetQuery::try_new(
                vec![
                    b.id.clone(),
                    a.id.clone(),
                    b.id.clone(),
                    memory_id("missing"),
                ],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();

    assert!(matches!(&result.items[0], GetItemResult::Found { id, .. } if id == &b.id));
    assert!(matches!(&result.items[1], GetItemResult::Found { id, .. } if id == &a.id));
    assert!(matches!(&result.items[2], GetItemResult::Found { id, .. } if id == &b.id));
    assert!(
        matches!(&result.items[3], GetItemResult::NotFound { id } if id == &memory_id("missing"))
    );
}

#[test]
fn application_storage_ports_map_every_failure_without_leaking_data() {
    let store = MemoryStoreDouble::durable();
    let application = app(
        store.clone(),
        &[
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ],
    );
    for (failure, expected) in [
        (
            AgentMemoryStorageFailure::BoundsExceeded,
            MemoryApplicationError::InvalidArgument {
                field: "storage_request",
                reason: "bounds_exceeded",
            },
        ),
        (
            AgentMemoryStorageFailure::Unavailable,
            MemoryApplicationError::StorageUnavailable,
        ),
        (
            AgentMemoryStorageFailure::CommitOutcomeUnknown,
            MemoryApplicationError::CommitOutcomeUnknown,
        ),
    ] {
        store.fail_next_get(failure.clone());
        let get_error = application
            .get(GetQuery::try_new(vec![memory_id("private-record")], &limits()).unwrap())
            .unwrap_err();
        assert_eq!(get_error, expected);
        assert!(!format!("{get_error:?}").contains("private-record"));

        store.fail_next(failure.clone());
        let transaction_error = application
            .remember(remember(input("PRIVATE_RECORD_SENTINEL"), "private-key"))
            .unwrap_err();
        assert_eq!(transaction_error, expected);
        assert!(!format!("{transaction_error:?}").contains("PRIVATE_RECORD_SENTINEL"));
        assert!(!format!("{transaction_error:?}").contains("private-key"));

        // Scan is the storage boundary used by T013 retrieval. T007 has no public scan
        // command, so retain a deterministic port-level failure injection for that later
        // application mapping rather than introducing an uncontracted application API.
        store.fail_next_scan(failure.clone());
        assert_eq!(
            store.scan(AgentMemoryScanRequest {
                prefix: b"record/".to_vec(),
                cursor: None,
                limit: 1,
            }),
            Err(failure)
        );
    }
    for (failure, expected) in [
        (
            AgentMemoryStorageFailure::BoundsExceeded,
            MemoryApplicationError::InvalidArgument {
                field: "storage_request",
                reason: "bounds_exceeded",
            },
        ),
        (
            AgentMemoryStorageFailure::Unavailable,
            MemoryApplicationError::StorageUnavailable,
        ),
        (
            AgentMemoryStorageFailure::CommitOutcomeUnknown,
            MemoryApplicationError::CommitOutcomeUnknown,
        ),
    ] {
        store.return_next(AgentMemoryTransactionOutcome::Failure { failure });
        let error = application
            .remember(remember(
                input("PRIVATE_OUTCOME_SENTINEL"),
                "private-outcome-key",
            ))
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?}").contains("PRIVATE_OUTCOME_SENTINEL"));
        assert!(!format!("{error:?}").contains("private-outcome-key"));
    }
    let before = store.snapshot();
    store.return_next(AgentMemoryTransactionOutcome::Conflict);
    let _ = application.remember(remember(input("conflict"), "conflict-key"));
    assert_eq!(store.snapshot(), before);

    let acknowledged = application
        .remember(remember(input("durable acknowledgement"), "durable-key"))
        .unwrap();
    assert!(
        acknowledged.durable,
        "only the committed acknowledgement is durable"
    );

    store.seed(b"meta/schema", br#"{"version":99}"#);
    assert_eq!(
        application
            .get(GetQuery::try_new(vec![memory_id("compatibility")], &limits()).unwrap())
            .unwrap_err(),
        MemoryApplicationError::Compatibility {
            found: Some(99),
            supported: 1,
        }
    );
}

#[test]
fn application_clock_and_id_ports_are_deterministic() {
    let store = MemoryStoreDouble::durable();
    let application = app(store, &["same", "same", "next"]);
    let first = application
        .remember(remember(input("first"), "clock-one"))
        .unwrap();
    let second = application
        .remember(remember(input("second"), "clock-two"))
        .unwrap();

    assert_eq!(first.id, memory_id("same"));
    assert_eq!(second.id, memory_id("next"));
    let fetched = application
        .get(GetQuery::try_new(vec![first.id], &limits()).unwrap())
        .unwrap();
    assert!(
        matches!(&fetched.items[0], GetItemResult::Found { record, .. } if record.created_at() == "2027-01-15T08:00:00Z")
    );

    let invalid_clock = MemoryApplication::new(
        MemoryStoreDouble::durable(),
        FixedClock {
            wall: SystemTime::UNIX_EPOCH - Duration::from_secs(1),
            monotonic: Duration::ZERO,
        },
        SequenceIds::new(&["invalid-time"]),
        None::<ProviderDouble>,
    );
    assert_eq!(
        invalid_clock
            .remember(remember(input("invalid"), "invalid-time-key"))
            .unwrap_err(),
        MemoryApplicationError::InvalidArgument {
            field: "timestamp",
            reason: "out_of_range"
        }
    );

    assert_eq!(
        RecordInput::try_new(
            MemoryType::Observation,
            String::new(),
            "body".into(),
            input("validation").provenance,
            None,
            &limits(),
        ),
        Err(MemoryApplicationError::InvalidArgument {
            field: "title",
            reason: "empty",
        })
    );
    assert_eq!(
        RememberCommand::try_new(input("validation"), String::new(), &limits()),
        Err(MemoryApplicationError::InvalidArgument {
            field: "idempotency_key",
            reason: "empty",
        })
    );
    assert_eq!(
        UpdateCommand::try_new(Vec::new(), &limits()),
        Err(MemoryApplicationError::InvalidArgument {
            field: "items",
            reason: "empty",
        })
    );
    assert_eq!(
        ForgetCommand::try_new(Vec::new(), &limits()),
        Err(MemoryApplicationError::InvalidArgument {
            field: "items",
            reason: "empty",
        })
    );
    let over_limit = (0..=limits().max_batch_items)
        .map(|index| UpdateItem {
            id: memory_id(&format!("over-limit-{index}")),
            expected_revision: revision("1"),
            record: input("over limit"),
        })
        .collect();
    assert_eq!(
        UpdateCommand::try_new(over_limit, &limits()),
        Err(MemoryApplicationError::InvalidArgument {
            field: "items",
            reason: "limit_exceeded",
        })
    );
}

#[test]
fn application_provider_port_maps_failures_and_honors_control() {
    let no_provider_calls = ProviderDouble::default();
    let no_provider = app(MemoryStoreDouble::durable(), &["baseline"]);
    let baseline = no_provider
        .remember(remember(input("baseline"), "baseline-key"))
        .unwrap();
    let baseline_record = match no_provider
        .get(GetQuery::try_new(vec![baseline.id], &limits()).unwrap())
        .unwrap()
        .items
        .remove(0)
    {
        GetItemResult::Found { record, .. } => record,
        GetItemResult::NotFound { .. } => panic!("remembered baseline must be readable"),
    };
    assert_eq!(no_provider_calls.calls.load(Ordering::SeqCst), 0);

    let cancelled = AtomicBool::new(true);
    // The application owns the validated configured budget; callers cannot
    // replace it for an individual provider operation.
    let deadline = Duration::from_millis(4_321);
    for (failure, expected) in [
        (
            EnrichmentProviderError::Timeout,
            MemoryApplicationError::ProviderTimeout,
        ),
        (
            EnrichmentProviderError::Cancelled,
            MemoryApplicationError::ProviderCancelled,
        ),
        (
            EnrichmentProviderError::Unavailable,
            MemoryApplicationError::ProviderUnavailable,
        ),
    ] {
        let provider = ProviderDouble::default();
        *provider.result.lock().unwrap() = Some(Err(failure));
        let application = MemoryApplication::new_configured(
            MemoryStoreDouble::durable(),
            FixedClock {
                wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
                monotonic: Duration::from_secs(20),
            },
            SequenceIds::new(&["provider-unused"]),
            Some(provider.clone()),
            PrivacyFilter::default(),
            limits(),
            PersistenceMode::Durable,
            false,
            deadline,
        );
        assert_eq!(
            application.run_enrichment(
                EnrichmentRequest {
                    record: baseline_record.clone(),
                },
                &cancelled,
            ),
            Err(expected.clone())
        );
        assert_eq!(
            *provider.observed.lock().unwrap(),
            Some((Duration::from_millis(24_321), true))
        );
        assert_eq!(
            application_error_payload(expected.clone(), PersistenceMode::Durable, false),
            json!({
                "code": "storage_unavailable",
                "persistence_mode": "durable",
                "durable_data_unavailable": false,
            })
        );
    }

    assert_eq!(
        no_provider
            .run_enrichment(
                EnrichmentRequest {
                    record: baseline_record,
                },
                &cancelled,
            )
            .unwrap(),
        None
    );
}

fn legacy_store() -> MemoryStoreDouble {
    let store = MemoryStoreDouble::durable();
    let seeded = app(store.clone(), &["legacy"])
        .remember(remember(input("legacy record"), "legacy-key"))
        .unwrap();
    let record_key = format!(
        "record/{}",
        URL_SAFE_NO_PAD.encode(seeded.id.as_str().as_bytes())
    )
    .into_bytes();
    let projection_key = format!(
        "projection/{}",
        URL_SAFE_NO_PAD.encode(seeded.id.as_str().as_bytes())
    )
    .into_bytes();
    let mut record: serde_json::Value =
        serde_json::from_slice(&store.snapshot()[&record_key]).unwrap();
    record["schema_version"] = json!(0);
    store.seed(&record_key, &serde_json::to_vec(&record).unwrap());
    store.remove(&projection_key);
    store.seed(b"meta/schema", br#"{"version":0}"#);
    store
}

#[test]
fn schema_migration_publishes_only_a_complete_supported_store() {
    let store = legacy_store();
    let record_key = store
        .snapshot()
        .into_keys()
        .find(|key| key.starts_with(b"record/"))
        .unwrap();
    let projection_key = [b"projection/".as_slice(), &record_key[b"record/".len()..]].concat();

    let status = app(store.clone(), &[]).initialize_store().unwrap();

    assert_eq!(status.schema_version, 1);
    assert_eq!(status.persistence_mode, PersistenceMode::Durable);
    assert!(!status.durable_data_unavailable);
    let migrated = store.snapshot();
    assert_eq!(migrated[b"meta/schema".as_slice()], br#"{"version":1}"#);
    assert!(migrated.contains_key(&record_key));
    assert!(migrated.contains_key(&projection_key));
    assert!(!migrated.contains_key(b"meta/migration".as_slice()));

    // Each failure is injected at the store boundary and must retain the exact
    // pre-operation bytes.  The implementation owns stage mapping; these are
    // RED assertions rather than test-double behavior.
    let read_failure = legacy_store();
    let before = read_failure.snapshot();
    read_failure.fail_next_get(AgentMemoryStorageFailure::Unavailable);
    assert_eq!(
        app(read_failure.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration { stage: "read" })
    );
    assert_eq!(read_failure.snapshot(), before);

    let validate_failure = legacy_store();
    let bad_record = validate_failure
        .snapshot()
        .into_keys()
        .find(|key| key.starts_with(b"record/"))
        .unwrap();
    validate_failure.seed(&bad_record, br#"{"schema_version":0,"title":"corrupt"}"#);
    let before = validate_failure.snapshot();
    assert_eq!(
        app(validate_failure.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration { stage: "validate" })
    );
    assert_eq!(validate_failure.snapshot(), before);

    let bounds_failure = legacy_store();
    let before = bounds_failure.snapshot();
    bounds_failure.limit_transaction_mutations(0);
    assert_eq!(
        app(bounds_failure.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration { stage: "bounds" })
    );
    assert_eq!(bounds_failure.snapshot(), before);

    let checkpoint_failure = legacy_store();
    checkpoint_failure.seed(
        b"meta/migration",
        br#"{"source":0,"target":1,"state":"invalid"}"#,
    );
    let before = checkpoint_failure.snapshot();
    assert_eq!(
        app(checkpoint_failure.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration {
            stage: "checkpoint"
        })
    );
    assert_eq!(checkpoint_failure.snapshot(), before);

    let commit_failure = legacy_store();
    let before = commit_failure.snapshot();
    commit_failure.fail_next_transaction(AgentMemoryStorageFailure::Unavailable);
    assert_eq!(
        app(commit_failure.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration { stage: "commit" })
    );
    assert_eq!(commit_failure.snapshot(), before);

    let interrupted = legacy_store();
    let before = interrupted.snapshot();
    interrupted.return_next(AgentMemoryTransactionOutcome::Conflict);
    assert_eq!(
        app(interrupted.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration { stage: "commit" })
    );
    assert_eq!(interrupted.snapshot(), before);

    let checkpoint_race = legacy_store();
    let before = checkpoint_race.snapshot();
    checkpoint_race
        .change_checkpoint_before_next_transaction(br#"{"source":0,"target":1,"state":"changed"}"#);
    assert_eq!(
        app(checkpoint_race.clone(), &[]).initialize_store(),
        Err(MemoryApplicationError::Migration { stage: "commit" })
    );
    let after = checkpoint_race.snapshot();
    assert_eq!(
        after[b"meta/schema".as_slice()],
        before[b"meta/schema".as_slice()]
    );
    assert_eq!(
        after[b"meta/migration".as_slice()],
        br#"{"source":0,"target":1,"state":"changed"}"#
    );

    for stage in ["read", "validate", "bounds", "checkpoint", "commit"] {
        assert_eq!(
            application_error_payload(
                MemoryApplicationError::Migration { stage },
                PersistenceMode::Durable,
                false,
            ),
            json!({
                "code": "migration_failed",
                "persistence_mode": "durable",
                "durable_data_unavailable": false,
            })
        );
    }

    let unknown_commit = legacy_store();
    let before = unknown_commit.snapshot();
    let requests_before = unknown_commit.transaction_requests().len();
    unknown_commit.return_next(AgentMemoryTransactionOutcome::Failure {
        failure: AgentMemoryStorageFailure::CommitOutcomeUnknown,
    });
    let error = app(unknown_commit.clone(), &[])
        .initialize_store()
        .unwrap_err();
    assert_eq!(error, MemoryApplicationError::CommitOutcomeUnknown);
    assert_eq!(unknown_commit.snapshot(), before);
    assert_eq!(
        unknown_commit.transaction_requests().len(),
        requests_before + 1
    );
    assert_eq!(
        application_error_payload(error, PersistenceMode::Durable, false),
        json!({
            "code": "commit_outcome_unknown",
            "persistence_mode": "durable",
            "durable_data_unavailable": false,
        })
    );
}

#[test]
fn future_schema_is_rejected_without_overwrite() {
    let store = MemoryStoreDouble::durable();
    store.seed(b"meta/schema", br#"{"version":2}"#);
    store.seed(
        b"record/private",
        br#"{"sentinel":"PRIVATE_SCHEMA_SENTINEL"}"#,
    );
    let before = store.snapshot();

    let error = app(store.clone(), &[]).initialize_store().unwrap_err();

    assert_eq!(
        error,
        MemoryApplicationError::Compatibility {
            found: Some(2),
            supported: 1,
        }
    );
    assert_eq!(store.snapshot(), before);
    assert!(!format!("{error:?}").contains("PRIVATE_SCHEMA_SENTINEL"));

    let corrupt = MemoryStoreDouble::durable();
    corrupt.seed(b"meta/schema", b"{not-json");
    corrupt.seed(
        b"record/private",
        br#"{"sentinel":"PRIVATE_CORRUPT_SENTINEL"}"#,
    );
    let before = corrupt.snapshot();
    let error = app(corrupt.clone(), &[]).initialize_store().unwrap_err();
    assert_eq!(
        error,
        MemoryApplicationError::Compatibility {
            found: None,
            supported: 1,
        }
    );
    assert_eq!(corrupt.snapshot(), before);
    assert_eq!(
        application_error_payload(error, PersistenceMode::Durable, false),
        json!({
            "code": "incompatible_schema",
            "persistence_mode": "durable",
            "durable_data_unavailable": false,
        })
    );
}

#[test]
fn configured_fallback_reports_read_only_or_volatile_mode() {
    let store = MemoryStoreDouble::default();
    let application = app_in_mode(
        store.clone(),
        &["volatile"],
        PersistenceMode::InMemory,
        true,
    );
    let status = application.initialize_store().unwrap();

    assert_eq!(status.schema_version, 1);
    assert_eq!(status.persistence_mode, PersistenceMode::InMemory);
    assert!(status.durable_data_unavailable);
    assert_eq!(
        store.snapshot()[b"meta/schema".as_slice()],
        br#"{"version":1}"#
    );
    let volatile = application
        .remember(remember(input("volatile acknowledgement"), "volatile-key"))
        .unwrap();
    assert_eq!(volatile.persistence_mode, PersistenceMode::InMemory);
    assert!(volatile.durable_data_unavailable);
    assert!(!volatile.durable);

    let read_only_store = MemoryStoreDouble::durable();
    read_only_store.seed(b"meta/schema", br#"{"version":1}"#);
    let writable = app(read_only_store.clone(), &["read-only"]);
    let remembered = writable
        .remember(remember(input("read only baseline"), "read-only-key"))
        .unwrap();
    let baseline = read_only_store.snapshot();
    let read_only = app_in_mode(
        read_only_store.clone(),
        &[],
        PersistenceMode::ReadOnly,
        false,
    );
    assert_eq!(
        read_only.initialize_store().unwrap(),
        StoreStatus {
            schema_version: 1,
            persistence_mode: PersistenceMode::ReadOnly,
            durable_data_unavailable: false,
        }
    );
    assert_eq!(
        read_only.remember(remember(input("rejected read only"), "read-only-rejected")),
        Err(MemoryApplicationError::ReadOnly)
    );
    assert_eq!(read_only_store.snapshot(), baseline);
    let result = read_only
        .forget(
            ForgetCommand::try_new(vec![(remembered.id, remembered.revision)], &limits()).unwrap(),
        )
        .unwrap();
    assert_eq!(result.persistence_mode, PersistenceMode::ReadOnly);
    assert!(!result.durable_data_unavailable);
    assert_eq!(result.items[0].code, MutationOutcomeCode::ReadOnly);
    assert!(!result.items[0].durable);
    assert_eq!(read_only_store.snapshot(), baseline);
}

#[test]
fn runtime_storage_failure_never_switches_persistence_mode() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["unused"]);
    let initial = application.initialize_store().unwrap();
    store.fail_next_get(AgentMemoryStorageFailure::Unavailable);

    assert_eq!(
        application.initialize_store(),
        Err(MemoryApplicationError::StorageUnavailable)
    );
    assert_eq!(
        application_error_payload(
            MemoryApplicationError::StorageUnavailable,
            PersistenceMode::Durable,
            false,
        ),
        json!({
            "code": "storage_unavailable",
            "persistence_mode": "durable",
            "durable_data_unavailable": false,
        })
    );
    assert_eq!(
        application_error_payload(
            MemoryApplicationError::StorageUnavailable,
            PersistenceMode::ReadOnly,
            false,
        ),
        json!({
            "code": "storage_unavailable",
            "persistence_mode": "read_only",
            "durable_data_unavailable": false,
        })
    );
    assert_eq!(
        application_error_payload(
            MemoryApplicationError::ProviderUnavailable,
            PersistenceMode::InMemory,
            true,
        ),
        json!({
            "code": "storage_unavailable",
            "persistence_mode": "in_memory",
            "durable_data_unavailable": true,
        })
    );
    assert_eq!(initial.persistence_mode, PersistenceMode::Durable);
    let recovered = application.initialize_store().unwrap();
    assert_eq!(recovered.persistence_mode, PersistenceMode::Durable);
    assert!(!recovered.durable_data_unavailable);
}

#[test]
fn stale_enrichment_results_are_discarded_after_update_forget_and_restart() {
    let store = MemoryStoreDouble::durable();
    let application = app(store.clone(), &["memory"]);
    let remembered = application
        .remember(remember(input("baseline"), "baseline-key"))
        .unwrap();
    let stale = EnrichmentResult {
        id: remembered.id.clone(),
        expected_revision: remembered.revision.clone(),
        projection: b"PRIVATE_STALE_PROJECTION".to_vec(),
    };
    let updated = application
        .update(
            UpdateCommand::try_new(
                vec![UpdateItem {
                    id: remembered.id.clone(),
                    expected_revision: remembered.revision,
                    record: input("updated"),
                }],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        application.publish_enrichment(stale.clone()).unwrap(),
        EnrichmentPublication::Discarded
    );
    let projection_after_update = store
        .snapshot()
        .into_iter()
        .find(|(key, _)| key.starts_with(b"projection/"))
        .unwrap()
        .1;
    assert!(
        !projection_after_update
            .windows(stale.projection.len())
            .any(|value| value == stale.projection)
    );

    application
        .forget(
            ForgetCommand::try_new(
                vec![(
                    remembered.id.clone(),
                    updated.items[0].revision.clone().unwrap(),
                )],
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        app(store.clone(), &[]).publish_enrichment(stale).unwrap(),
        EnrichmentPublication::Discarded
    );
    assert!(store.snapshot().iter().all(|(key, value)| {
        !key.starts_with(b"projection/")
            && !value
                .windows(b"PRIVATE_STALE_PROJECTION".len())
                .any(|window| window == b"PRIVATE_STALE_PROJECTION")
    }));

    let published_store = MemoryStoreDouble::durable();
    let published_application = app(published_store.clone(), &["published"]);
    let remembered = published_application
        .remember(remember(input("published baseline"), "published-key"))
        .unwrap();
    let fresh = EnrichmentResult {
        id: remembered.id.clone(),
        expected_revision: remembered.revision.clone(),
        projection: b"sanitized-fresh-projection".to_vec(),
    };
    assert_eq!(
        published_application
            .publish_enrichment(fresh.clone())
            .unwrap(),
        EnrichmentPublication::Published {
            id: remembered.id,
            revision: remembered.revision,
            durable: true,
        }
    );
    assert!(
        published_store
            .snapshot()
            .values()
            .any(|value| value == &fresh.projection)
    );

    for (provider_failure, expected) in [
        (
            EnrichmentProviderError::Timeout,
            MemoryApplicationError::ProviderTimeout,
        ),
        (
            EnrichmentProviderError::Cancelled,
            MemoryApplicationError::ProviderCancelled,
        ),
        (
            EnrichmentProviderError::Unavailable,
            MemoryApplicationError::ProviderUnavailable,
        ),
    ] {
        let provider = ProviderDouble::default();
        *provider.result.lock().unwrap() = Some(Err(provider_failure));
        let provider_application = MemoryApplication::new(
            published_store.clone(),
            FixedClock {
                wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
                monotonic: Duration::ZERO,
            },
            SequenceIds::new(&[]),
            Some(provider.clone()),
        );
        let baseline = published_store.snapshot();
        let cancelled = AtomicBool::new(matches!(
            provider_failure,
            EnrichmentProviderError::Cancelled
        ));
        assert_eq!(
            provider_application.run_enrichment(
                EnrichmentRequest {
                    record: match app(published_store.clone(), &[])
                        .get(GetQuery::try_new(vec![fresh.id.clone()], &limits()).unwrap())
                        .unwrap()
                        .items
                        .remove(0)
                    {
                        GetItemResult::Found { record, .. } => record,
                        GetItemResult::NotFound { .. } =>
                            panic!("baseline record must remain available"),
                    }
                },
                &cancelled,
            ),
            Err(expected.clone())
        );
        assert_eq!(
            *provider.observed.lock().unwrap(),
            Some((
                Duration::from_millis(5_000),
                cancelled.load(Ordering::Acquire)
            ))
        );
        assert_eq!(published_store.snapshot(), baseline);
        assert!(
            app(published_store.clone(), &[])
                .get(GetQuery::try_new(vec![fresh.id.clone()], &limits()).unwrap())
                .unwrap()
                .items
                .iter()
                .any(|item| matches!(item, GetItemResult::Found { .. }))
        );
    }
}

#[test]
fn retrieval_privacy_rejects_text_before_storage_and_redacts_before_cursor_binding() {
    let store = MemoryStoreDouble::durable();
    app(store.clone(), &["entry"])
        .remember(remember(
            input_with("needle", "body", "main", "src/a.rs"),
            "key",
        ))
        .unwrap();
    let privacy = PrivacyFilter::try_new(vec![
        PrivacyRule::try_new("reject".into(), "SECRET".into(), PrivacyAction::Reject).unwrap(),
        PrivacyRule::try_new(
            "redact".into(),
            "TOKEN".into(),
            PrivacyAction::Redact {
                replacement: "needle".into(),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let application = MemoryApplication::new_with_privacy(
        store.clone(),
        FixedClock {
            wall: SystemTime::UNIX_EPOCH,
            monotonic: Duration::ZERO,
        },
        SequenceIds::new(&[]),
        None::<ProviderDouble>,
        privacy,
        limits(),
    );
    let before = store.snapshot();
    assert_eq!(
        application.search(search_query("SECRET", page(10))),
        Err(MemoryApplicationError::InvalidArgument {
            field: "query",
            reason: "privacy_rejected"
        }),
    );

    for (tool, params, privacy_payload) in [
        ("memory_search", json!({"query":"SECRET"}), true),
        ("memory_context_for_task", json!({"query":"SECRET"}), true),
        (
            "memory_timeline",
            json!({"anchor":{"kind":"query","query":"SECRET"}}),
            true,
        ),
        (
            "memory_timeline",
            json!({"anchor":{"kind":"record","id":"SECRET","before":0,"after":0}}),
            true,
        ),
        (
            "memory_timeline",
            json!({"anchor":{"kind":"window","from":"SECRET","to":"2026-01-01T00:00:00Z"}}),
            false,
        ),
    ] {
        let expected = if privacy_payload {
            Ok(json!({
                "code":"privacy_rejected",
                "persistence_mode":"durable",
                "durable_data_unavailable":false
            }))
        } else {
            Err(RetrievalDispatchError::InvalidParams)
        };
        assert_eq!(application.dispatch_retrieval(tool, params), expected);
        assert_eq!(store.snapshot(), before);
    }
    for field in [
        "branches",
        "paths",
        "symbols",
        "tags",
        "source_agent",
        "source_client",
    ] {
        let value = if field.starts_with("source_") {
            json!("SECRET")
        } else {
            json!(["SECRET"])
        };
        let mut filters = json!({});
        filters[field] = value;
        assert_eq!(
            application
                .dispatch_retrieval("memory_search", json!({"query":"needle","filters":filters})),
            Ok(json!({
                "code":"privacy_rejected",
                "persistence_mode":"durable",
                "durable_data_unavailable":false
            }))
        );
        assert_eq!(store.snapshot(), before);
    }
    let redacted = application
        .dispatch_retrieval("memory_search", json!({"query":"TOKEN"}))
        .unwrap();
    let normalized = application
        .dispatch_retrieval("memory_search", json!({"query":"needle"}))
        .unwrap();
    assert_eq!(redacted, normalized);
    assert_eq!(redacted["hits"].as_array().unwrap().len(), 1);
    assert_eq!(store.snapshot(), before);
}

#[test]
fn metadata_score_counts_each_distinct_matching_token_per_category() {
    let store = MemoryStoreDouble::durable();
    let application = app(store, &["entry"]);
    let mut record = input_with("unrelated", "body", "main", "src/a.rs");
    record.provenance = Provenance::try_new(
        Some("main".into()),
        vec!["src/a.rs".into()],
        vec![],
        vec!["alpha".into(), "beta".into()],
        None,
        None,
        None,
        None,
        None,
        &limits(),
    )
    .unwrap();
    application.remember(remember(record, "key")).unwrap();
    let result = application
        .search(
            SearchQuery::try_new(
                "alpha beta alpha".into(),
                empty_filters(),
                page(10),
                RankingMode::LexicalV1,
                &limits(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(result.hits[0].score(), 2.0);
    assert_eq!(result.hits[0].reasons().len(), 1);
}

#[test]
fn retrieval_budgets_match_emitted_envelopes_in_every_persistence_mode() {
    for (mode, unavailable) in [
        (PersistenceMode::Durable, false),
        (PersistenceMode::ReadOnly, false),
        (PersistenceMode::InMemory, true),
    ] {
        let store = MemoryStoreDouble::durable();
        app(store.clone(), &["budget-record"])
            .remember(remember(
                input_with("needle", "body", "main", "src/lib.rs"),
                "budget-key",
            ))
            .unwrap();
        let application = app_in_mode(store, &[], mode, unavailable);
        for (tool, params) in [
            ("memory_search", json!({"query":"needle"})),
            (
                "memory_timeline",
                json!({"anchor":{"kind":"query","query":"needle"}}),
            ),
            ("memory_context_for_task", json!({"query":"needle"})),
        ] {
            let result = application
                .dispatch_retrieval(tool, params.clone())
                .unwrap();
            assert_eq!(result["code"], "ok");
            let length = serde_json::to_vec(&result).unwrap().len();
            let mut bounded = params;
            bounded["page"] = json!({"response_bytes":length});
            let exact = application
                .dispatch_retrieval(tool, bounded.clone())
                .unwrap();
            assert_eq!(exact, result, "{mode:?}: {tool}");
            bounded["page"]["response_bytes"] = json!(length - 1);
            let rejected = application.dispatch_retrieval(tool, bounded).unwrap();
            assert_eq!(
                rejected["code"], "response_budget_too_small",
                "{mode:?}: {tool}: {rejected}"
            );
            assert_eq!(rejected["minimum_bytes"], length);
        }
    }
}
