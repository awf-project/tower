use std::{
    collections::BTreeMap,
    sync::atomic::AtomicBool,
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, Utc};
use extension_protocol::messages::{
    AgentMemoryCondition as Condition, AgentMemoryGetRequest, AgentMemoryMutation as Mutation,
    AgentMemoryScanRequest, AgentMemoryStorageFailure, AgentMemoryTransactionOutcome,
    AgentMemoryTransactionRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    AgentMemoryLimitsConfig, Clock, CompactHit, EnrichmentProvider, IdGenerator, MatchReason,
    MatchReasonField, MatchReasonKind, MemoryFilters, MemoryId, MemoryRecord, MemoryStore,
    MemoryType, PersistenceMode, PrivacyError, PrivacyFilter, Provenance, RankingMode, Revision,
    ValidationError,
};

const SCHEMA: u32 = 1;
const MAX_CONSISTENT_READ_ATTEMPTS: usize = 3;
const DEFAULT_PROVIDER_DEADLINE: Duration = Duration::from_millis(5_000);

pub struct MemoryApplication<S, C, I, P> {
    store: S,
    clock: C,
    id_generator: I,
    enrichment_provider: Option<P>,
    privacy: PrivacyFilter,
    limits: AgentMemoryLimitsConfig,
    persistence_mode: PersistenceMode,
    durable_data_unavailable: bool,
    provider_deadline: Duration,
}

impl<S, C, I, P> MemoryApplication<S, C, I, P> {
    pub fn new(store: S, clock: C, id_generator: I, enrichment_provider: Option<P>) -> Self {
        Self {
            store,
            clock,
            id_generator,
            enrichment_provider,
            privacy: PrivacyFilter::default(),
            limits: wide_limits(),
            persistence_mode: PersistenceMode::Durable,
            durable_data_unavailable: false,
            provider_deadline: DEFAULT_PROVIDER_DEADLINE,
        }
    }

    pub fn new_with_privacy(
        store: S,
        clock: C,
        id_generator: I,
        enrichment_provider: Option<P>,
        privacy: PrivacyFilter,
        limits: AgentMemoryLimitsConfig,
    ) -> Self {
        Self {
            store,
            clock,
            id_generator,
            enrichment_provider,
            privacy,
            limits,
            persistence_mode: PersistenceMode::Durable,
            durable_data_unavailable: false,
            provider_deadline: DEFAULT_PROVIDER_DEADLINE,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_configured(
        store: S,
        clock: C,
        id_generator: I,
        enrichment_provider: Option<P>,
        privacy: PrivacyFilter,
        limits: AgentMemoryLimitsConfig,
        persistence_mode: PersistenceMode,
        durable_data_unavailable: bool,
        provider_deadline: Duration,
    ) -> Self {
        Self {
            store,
            clock,
            id_generator,
            enrichment_provider,
            privacy,
            limits,
            persistence_mode,
            durable_data_unavailable,
            provider_deadline,
        }
    }
}

impl<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>
    MemoryApplication<S, C, I, P>
{
    pub fn limits(&self) -> &AgentMemoryLimitsConfig {
        &self.limits
    }

    pub fn persistence_status(&self) -> (PersistenceMode, bool) {
        (self.persistence_mode, self.durable_data_unavailable)
    }
    pub fn run_enrichment(
        &self,
        request: crate::EnrichmentRequest,
        cancelled: &AtomicBool,
    ) -> Result<Option<crate::EnrichmentResult>, MemoryApplicationError> {
        let deadline = self
            .clock
            .monotonic_now()
            .saturating_add(self.provider_deadline);
        self.enrichment_provider
            .as_ref()
            .map(|provider| {
                provider
                    .enrich(request, deadline, cancelled)
                    .map_err(map_provider)
            })
            .transpose()
    }

    pub fn initialize_store(&self) -> Result<StoreStatus, MemoryApplicationError> {
        let schema_key = b"meta/schema".to_vec();
        let schema_bytes = match self.store.get(AgentMemoryGetRequest {
            key: schema_key.clone(),
        }) {
            Ok(result) => result.value,
            Err(failure) => {
                if matches!(failure, AgentMemoryStorageFailure::Unavailable)
                    && self.namespace_schema_version() == Some(0)
                {
                    return Err(MemoryApplicationError::Migration { stage: "read" });
                }
                return Err(map_storage(failure));
            }
        };

        let Some(schema_bytes) = schema_bytes else {
            let namespace = self.scan_all(Vec::new()).map_err(map_storage)?;
            if !namespace.is_empty() {
                return Err(MemoryApplicationError::Compatibility {
                    found: None,
                    supported: SCHEMA,
                });
            }
            if self.persistence_mode == PersistenceMode::ReadOnly {
                return Err(MemoryApplicationError::Compatibility {
                    found: None,
                    supported: SCHEMA,
                });
            }
            let request = AgentMemoryTransactionRequest {
                conditions: vec![Condition::KeyAbsent {
                    key: schema_key.clone(),
                }],
                mutations: vec![Mutation::Put {
                    key: schema_key,
                    value: br#"{"version":1}"#.to_vec(),
                }],
            };
            return match self.store.transact(request).map_err(map_storage)? {
                AgentMemoryTransactionOutcome::Committed { .. } => Ok(self.store_status()),
                AgentMemoryTransactionOutcome::Conflict => {
                    Err(MemoryApplicationError::Migration { stage: "commit" })
                }
                AgentMemoryTransactionOutcome::Failure { failure } => Err(map_storage(failure)),
            };
        };

        let schema: Schema = serde_json::from_slice(&schema_bytes).map_err(|_| {
            MemoryApplicationError::Compatibility {
                found: None,
                supported: SCHEMA,
            }
        })?;
        match schema.version {
            SCHEMA => {
                self.validate_current_store()?;
                Ok(self.store_status())
            }
            0 if self.persistence_mode == PersistenceMode::ReadOnly => {
                Err(MemoryApplicationError::Compatibility {
                    found: Some(0),
                    supported: SCHEMA,
                })
            }
            0 => self.migrate_v0(schema_bytes),
            found => Err(MemoryApplicationError::Compatibility {
                found: Some(found),
                supported: SCHEMA,
            }),
        }
    }

    pub fn publish_enrichment(
        &self,
        enrichment: crate::EnrichmentResult,
    ) -> Result<EnrichmentPublication, MemoryApplicationError> {
        if self.persistence_mode == PersistenceMode::ReadOnly {
            return Ok(EnrichmentPublication::Discarded);
        }
        let key = record_key(&enrichment.id);
        let Some(record_bytes) = self
            .store
            .get(AgentMemoryGetRequest { key: key.clone() })
            .map_err(map_storage)?
            .value
        else {
            return Ok(EnrichmentPublication::Discarded);
        };
        let record: StoredRecord = decode(&record_bytes)?;
        if record.id != enrichment.id.as_str()
            || record.revision != enrichment.expected_revision.as_str()
        {
            return Ok(EnrichmentPublication::Discarded);
        }
        let projection_text = std::str::from_utf8(&enrichment.projection)
            .map_err(|_| MemoryApplicationError::ProviderUnavailable)?;
        let projection = self
            .privacy
            .apply(
                "provider.projection",
                projection_text,
                self.limits.max_body_bytes,
            )
            .map_err(map_privacy)?
            .into_bytes();
        let request = AgentMemoryTransactionRequest {
            conditions: vec![Condition::ValueEquals {
                key: key.clone(),
                value: record_bytes,
            }],
            mutations: vec![
                Mutation::Put {
                    key: projection_key(&enrichment.id),
                    value: projection,
                },
                Mutation::Delete {
                    key: job_key(&enrichment.id),
                },
            ],
        };
        match self.transact(request)? {
            Tx::Committed(durable) => Ok(EnrichmentPublication::Published {
                id: enrichment.id,
                revision: enrichment.expected_revision,
                durable,
            }),
            Tx::Conflict => Ok(EnrichmentPublication::Discarded),
        }
    }

    fn store_status(&self) -> StoreStatus {
        StoreStatus {
            schema_version: SCHEMA,
            persistence_mode: self.persistence_mode,
            durable_data_unavailable: self.durable_data_unavailable,
        }
    }

    fn namespace_schema_version(&self) -> Option<u32> {
        self.scan_all(Vec::new()).ok().and_then(|entries| {
            entries
                .into_iter()
                .find(|entry| entry.key == b"meta/schema")
                .and_then(|entry| serde_json::from_slice::<Schema>(&entry.value).ok())
                .map(|schema| schema.version)
        })
    }

    fn scan_all(
        &self,
        prefix: Vec<u8>,
    ) -> Result<Vec<extension_protocol::messages::AgentMemoryKeyValue>, AgentMemoryStorageFailure>
    {
        let mut entries = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.store.scan(AgentMemoryScanRequest {
                prefix: prefix.clone(),
                cursor,
                limit: self.limits.max_results,
            })?;
            entries.extend(page.entries);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(entries);
            }
        }
    }

    fn migrate_v0(&self, schema_bytes: Vec<u8>) -> Result<StoreStatus, MemoryApplicationError> {
        let checkpoint = self
            .store
            .get(AgentMemoryGetRequest {
                key: b"meta/migration".to_vec(),
            })
            .map_err(|_| MemoryApplicationError::Migration { stage: "read" })?
            .value;
        if checkpoint.as_deref().is_some_and(|bytes| {
            serde_json::from_slice::<MigrationCheckpoint>(bytes).ok()
                != Some(MigrationCheckpoint::prepared())
        }) {
            return Err(MemoryApplicationError::Migration {
                stage: "checkpoint",
            });
        }
        let entries = self
            .scan_all(b"record/".to_vec())
            .map_err(|_| MemoryApplicationError::Migration { stage: "read" })?;
        let mut conditions = vec![Condition::ValueEquals {
            key: b"meta/schema".to_vec(),
            value: schema_bytes,
        }];
        conditions.push(match checkpoint {
            Some(value) => Condition::ValueEquals {
                key: b"meta/migration".to_vec(),
                value,
            },
            None => Condition::KeyAbsent {
                key: b"meta/migration".to_vec(),
            },
        });
        let mut mutations = Vec::with_capacity(entries.len() * 2 + 2);
        for entry in entries {
            let mut stored: StoredRecord = serde_json::from_slice(&entry.value)
                .map_err(|_| MemoryApplicationError::Migration { stage: "validate" })?;
            if stored.schema != 0 || record_key_for_stored_id(&stored.id) != Some(entry.key.clone())
            {
                return Err(MemoryApplicationError::Migration { stage: "validate" });
            }
            let idem_digest = stored.idem_digest.clone();
            stored.schema = SCHEMA;
            let domain = stored
                .clone()
                .domain()
                .map_err(|_| MemoryApplicationError::Migration { stage: "validate" })?;
            let filtered = self
                .privacy_record(RecordInput {
                    record_type: domain.record_type(),
                    title: domain.title().to_owned(),
                    body: domain.body().to_owned(),
                    provenance: domain.provenance().clone(),
                    confidence: domain.confidence(),
                })
                .map_err(|_| MemoryApplicationError::Migration { stage: "validate" })?;
            stored.kind = kind_name(filtered.record_type).to_owned();
            stored.title = filtered.title;
            stored.body = filtered.body;
            stored.provenance = StoredProvenance::from(&filtered.provenance);
            stored.confidence = filtered.confidence;
            stored.idem_digest = idem_digest;
            conditions.push(Condition::ValueEquals {
                key: entry.key.clone(),
                value: entry.value,
            });
            mutations.push(Mutation::Put {
                key: entry.key,
                value: encode(&stored)
                    .map_err(|_| MemoryApplicationError::Migration { stage: "validate" })?,
            });
            mutations.push(Mutation::Put {
                key: projection_key(
                    &MemoryId::try_from(stored.id.clone())
                        .map_err(|_| MemoryApplicationError::Migration { stage: "validate" })?,
                ),
                value: projection(&stored),
            });
        }
        mutations.push(Mutation::Put {
            key: b"meta/schema".to_vec(),
            value: br#"{"version":1}"#.to_vec(),
        });
        mutations.push(Mutation::Delete {
            key: b"meta/migration".to_vec(),
        });
        let request = AgentMemoryTransactionRequest {
            conditions,
            mutations,
        };
        match self.store.transact(request) {
            Err(AgentMemoryStorageFailure::BoundsExceeded) => {
                Err(MemoryApplicationError::Migration { stage: "bounds" })
            }
            Err(AgentMemoryStorageFailure::Unavailable) => {
                Err(MemoryApplicationError::Migration { stage: "commit" })
            }
            Err(AgentMemoryStorageFailure::CommitOutcomeUnknown) => {
                Err(MemoryApplicationError::CommitOutcomeUnknown)
            }
            Ok(AgentMemoryTransactionOutcome::Committed { .. }) => Ok(self.store_status()),
            Ok(AgentMemoryTransactionOutcome::Conflict) => {
                Err(MemoryApplicationError::Migration { stage: "commit" })
            }
            Ok(AgentMemoryTransactionOutcome::Failure { failure }) => match failure {
                AgentMemoryStorageFailure::BoundsExceeded => {
                    Err(MemoryApplicationError::Migration { stage: "bounds" })
                }
                AgentMemoryStorageFailure::Unavailable => {
                    Err(MemoryApplicationError::Migration { stage: "commit" })
                }
                AgentMemoryStorageFailure::CommitOutcomeUnknown => {
                    Err(MemoryApplicationError::CommitOutcomeUnknown)
                }
            },
        }
    }

    fn validate_current_store(&self) -> Result<(), MemoryApplicationError> {
        let entries = self.scan_all(Vec::new()).map_err(map_storage)?;
        if entries.iter().any(|entry| entry.key == b"meta/migration") {
            return Err(MemoryApplicationError::Compatibility {
                found: Some(SCHEMA),
                supported: SCHEMA,
            });
        }
        for entry in entries
            .into_iter()
            .filter(|entry| entry.key.starts_with(b"record/"))
        {
            let stored: StoredRecord = serde_json::from_slice(&entry.value).map_err(|_| {
                MemoryApplicationError::Compatibility {
                    found: None,
                    supported: SCHEMA,
                }
            })?;
            if stored.schema != SCHEMA
                || record_key_for_stored_id(&stored.id) != Some(entry.key)
                || stored.domain().is_err()
            {
                return Err(MemoryApplicationError::Compatibility {
                    found: None,
                    supported: SCHEMA,
                });
            }
        }
        Ok(())
    }

    pub fn remember(
        &self,
        mut command: RememberCommand,
    ) -> Result<RememberResult, MemoryApplicationError> {
        if self.persistence_mode == PersistenceMode::ReadOnly {
            return Err(MemoryApplicationError::ReadOnly);
        }
        command.record = self.privacy_record(command.record)?;
        command.idempotency_key = self
            .privacy
            .apply(
                "idempotency_key",
                &command.idempotency_key,
                self.limits.max_metadata_string_bytes,
            )
            .map_err(map_privacy)?;
        let now = timestamp(self.clock.now())?;
        let payload_digest = hash(&encode(&StoredInput::from(&command.record))?);
        let idem_key = idem_key(&command.idempotency_key);
        if self.read_idem(&idem_key)?.is_some() {
            return self.reconcile_remember(&idem_key, &payload_digest);
        }

        for _ in 0..16 {
            let id = self.privacy_id(self.id_generator.next_id())?;
            let revision = Revision::try_from("1".to_owned()).map_err(map_validation)?;
            let record = MemoryRecord::try_new(
                id.clone(),
                revision.clone(),
                SCHEMA,
                command.record.record_type,
                command.record.title.clone(),
                command.record.body.clone(),
                command.record.provenance.clone(),
                command.record.confidence,
                "project".into(),
                now.clone(),
                now.clone(),
                &wide_limits(),
            )
            .map_err(map_validation)?;
            let mut stored = StoredRecord::from(&record);
            stored.idem_digest = hash(command.idempotency_key.as_bytes());
            let entry = Idem {
                id: id.as_str().into(),
                payload_digest: payload_digest.clone(),
                deleted: false,
            };
            let key = record_key(&id);
            let request = AgentMemoryTransactionRequest {
                conditions: vec![
                    Condition::KeyAbsent { key: key.clone() },
                    Condition::KeyAbsent {
                        key: idem_key.clone(),
                    },
                ],
                mutations: vec![
                    Mutation::Put {
                        key,
                        value: encode(&stored)?,
                    },
                    Mutation::Put {
                        key: projection_key(&id),
                        value: projection(&stored),
                    },
                    Mutation::Put {
                        key: job_key(&id),
                        value: b"pending".to_vec(),
                    },
                    Mutation::Put {
                        key: idem_key.clone(),
                        value: encode(&entry)?,
                    },
                    Mutation::Put {
                        key: b"meta/schema".to_vec(),
                        value: br#"{"version":1}"#.to_vec(),
                    },
                ],
            };
            match self.transact(request)? {
                Tx::Committed(durable) => {
                    return Ok(remember_result(
                        id,
                        revision,
                        self.persistence_mode,
                        self.durable_data_unavailable,
                        durable && self.persistence_mode == PersistenceMode::Durable,
                    ));
                }
                Tx::Conflict => {
                    if self.read_idem(&idem_key)?.is_some() {
                        return self.reconcile_remember(&idem_key, &payload_digest);
                    }
                    if self
                        .store
                        .get(AgentMemoryGetRequest {
                            key: record_key(&id),
                        })
                        .map_err(map_storage)?
                        .value
                        .is_none()
                    {
                        return Err(MemoryApplicationError::StorageUnavailable);
                    }
                }
            }
        }
        Err(MemoryApplicationError::StorageUnavailable)
    }

    pub fn get(&self, query: GetQuery) -> Result<GetResult, MemoryApplicationError> {
        self.check_schema()?;
        let ids = query
            .ids
            .into_iter()
            .map(|id| self.privacy_id(id))
            .collect::<Result<Vec<_>, _>>()?;
        for _ in 0..MAX_CONSISTENT_READ_ATTEMPTS {
            let mut generation = None;
            let mut items = Vec::with_capacity(ids.len());
            let mut consistent = true;
            for id in &ids {
                let result = self
                    .store
                    .get(AgentMemoryGetRequest {
                        key: record_key(id),
                    })
                    .map_err(map_storage)?;
                if generation.is_some_and(|expected| expected != result.generation) {
                    consistent = false;
                    break;
                }
                generation = Some(result.generation);
                if let Some(bytes) = result.value {
                    let record: StoredRecord = decode(&bytes)?;
                    items.push(GetItemResult::Found {
                        id: id.clone(),
                        record: record.domain()?,
                    });
                } else {
                    items.push(GetItemResult::NotFound { id: id.clone() });
                }
            }
            if consistent {
                return Ok(GetResult {
                    items,
                    persistence_mode: self.persistence_mode,
                    durable_data_unavailable: self.durable_data_unavailable,
                });
            }
        }
        Err(MemoryApplicationError::CursorExpired)
    }

    pub fn search(&self, query: SearchQuery) -> Result<SearchResult, MemoryApplicationError> {
        let query = SearchQuery::try_new(
            self.privacy
                .apply("query", &query.query, self.limits.max_query_bytes)
                .map_err(map_privacy)?,
            self.privacy_filters(query.filters)?,
            query.page,
            query.ranking,
            &self.limits,
        )?;
        let (records, generation) = self.retrieval_snapshot()?;
        let digest = request_digest(
            "search",
            &query.query,
            &query.filters,
            query.ranking,
            &query.page,
        )?;
        let offset = cursor_offset(&query.page, digest, generation)?;
        let mut ranked = records
            .into_iter()
            .filter(|record| matches_filters(record, &query.filters))
            .filter_map(|record| scored_hit(record, &query.query, &self.limits).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        ranked.sort_by(search_hit_order);
        let (hits, next_cursor) = page_hits(
            ranked,
            offset,
            &query.page,
            digest,
            generation,
            RetrievalEnvelope {
                result_field: "hits",
                applied_filters: None,
                persistence_status: self.persistence_status(),
            },
        )?;
        self.ensure_generation(generation)?;
        Ok(SearchResult {
            hits,
            next_cursor,
            persistence_mode: self.persistence_mode,
            durable_data_unavailable: self.durable_data_unavailable,
        })
    }

    pub fn timeline(&self, query: TimelineQuery) -> Result<TimelineResult, MemoryApplicationError> {
        let anchor = match query.anchor {
            TimelineAnchor::Record { id, before, after } => TimelineAnchor::Record {
                id: self.privacy_id(id)?,
                before,
                after,
            },
            TimelineAnchor::Query { query } => TimelineAnchor::Query {
                query: self
                    .privacy
                    .apply("query", &query, self.limits.max_query_bytes)
                    .map_err(map_privacy)?,
            },
            TimelineAnchor::Window { from, to } => TimelineAnchor::Window {
                from: self
                    .privacy
                    .apply("from", &from, self.limits.max_metadata_string_bytes)
                    .map_err(map_privacy)?,
                to: self
                    .privacy
                    .apply("to", &to, self.limits.max_metadata_string_bytes)
                    .map_err(map_privacy)?,
            },
        };
        let query = TimelineQuery::try_new(
            anchor,
            self.privacy_filters(query.filters)?,
            query.page,
            &self.limits,
        )?;
        let (records, generation) = self.retrieval_snapshot()?;
        let anchor_key = timeline_anchor_key(&query.anchor);
        let digest = request_digest(
            "timeline",
            &anchor_key,
            &query.filters,
            RankingMode::LexicalV1,
            &query.page,
        )?;
        let offset = cursor_offset(&query.page, digest, generation)?;
        let mut all = records;
        all.sort_by(timeline_record_order);
        let eligible = match &query.anchor {
            TimelineAnchor::Record { id, before, after } => {
                let Some(anchor_index) = all.iter().position(|record| record.id == id.as_str())
                else {
                    return Ok(TimelineResult::NotFound {
                        persistence_mode: self.persistence_mode,
                        durable_data_unavailable: self.durable_data_unavailable,
                    });
                };
                let mut neighborhood = all[..anchor_index]
                    .iter()
                    .rev()
                    .filter(|record| matches_filters(record, &query.filters))
                    .take(*before as usize)
                    .cloned()
                    .collect::<Vec<_>>();
                neighborhood.reverse();
                if matches_filters(&all[anchor_index], &query.filters) {
                    neighborhood.push(all[anchor_index].clone());
                }
                neighborhood.extend(
                    all[anchor_index + 1..]
                        .iter()
                        .filter(|record| matches_filters(record, &query.filters))
                        .take(*after as usize)
                        .cloned(),
                );
                neighborhood
            }
            TimelineAnchor::Query { query: text } => {
                let mut ranked = all
                    .iter()
                    .filter(|record| matches_filters(record, &query.filters))
                    .filter_map(|record| {
                        score_record(record, text).map(|(score, _)| (score, record))
                    })
                    .collect::<Vec<_>>();
                ranked.sort_by(|(ls, l), (rs, r)| {
                    rs.total_cmp(ls)
                        .then_with(|| timestamp_cmp(&r.created_at, &l.created_at))
                        .then_with(|| l.id.cmp(&r.id))
                });
                match ranked.first() {
                    None => Vec::new(),
                    Some((_, anchor)) => all
                        .iter()
                        .skip_while(|record| timeline_record_order(record, anchor).is_lt())
                        .filter(|record| matches_filters(record, &query.filters))
                        .cloned()
                        .collect(),
                }
            }
            TimelineAnchor::Window { from, to } => all
                .into_iter()
                .filter(|record| {
                    !timestamp_cmp(&record.created_at, from).is_lt()
                        && timestamp_cmp(&record.created_at, to).is_lt()
                        && matches_filters(record, &query.filters)
                })
                .collect(),
        };
        let hits = eligible
            .into_iter()
            .map(|record| compact_hit(record, 0.0, Vec::new(), &self.limits))
            .collect::<Result<Vec<_>, _>>()?;
        let (events, next_cursor) = page_hits(
            hits,
            offset,
            &query.page,
            digest,
            generation,
            RetrievalEnvelope {
                result_field: "events",
                applied_filters: None,
                persistence_status: self.persistence_status(),
            },
        )?;
        self.ensure_generation(generation)?;
        Ok(TimelineResult::Found {
            events,
            next_cursor,
            persistence_mode: self.persistence_mode,
            durable_data_unavailable: self.durable_data_unavailable,
        })
    }

    pub fn context_for_task(
        &self,
        query: TaskContextQuery,
    ) -> Result<TaskContextResult, MemoryApplicationError> {
        let query = TaskContextQuery::try_new(
            self.privacy
                .apply("query", &query.query, self.limits.max_query_bytes)
                .map_err(map_privacy)?,
            self.privacy_filters(query.filters)?,
            query.page,
            query.ranking,
            &self.limits,
        )?;
        let (records, generation) = self.retrieval_snapshot()?;
        let digest = request_digest(
            "context",
            &query.query,
            &query.filters,
            query.ranking,
            &query.page,
        )?;
        let offset = cursor_offset(&query.page, digest, generation)?;
        let explicit = has_explicit_metadata_filter(&query.filters);
        let mut ranked = records
            .into_iter()
            .filter(|record| matches_filters(record, &query.filters))
            .filter_map(
                |record| match scored_hit(record.clone(), &query.query, &self.limits) {
                    Ok(Some(hit)) => Some(Ok(hit)),
                    Ok(None) if explicit => Some(compact_hit(
                        record.clone(),
                        0.0,
                        matched_filter_reasons(&record, &query.filters),
                        &self.limits,
                    )),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect::<Result<Vec<_>, _>>()?;
        ranked.sort_by(search_hit_order);
        let (hits, next_cursor) = page_hits(
            ranked,
            offset,
            &query.page,
            digest,
            generation,
            RetrievalEnvelope {
                result_field: "hits",
                applied_filters: Some(&query.filters),
                persistence_status: self.persistence_status(),
            },
        )?;
        self.ensure_generation(generation)?;
        Ok(TaskContextResult {
            hits,
            applied_filters: query.filters,
            next_cursor,
            persistence_mode: self.persistence_mode,
            durable_data_unavailable: self.durable_data_unavailable,
        })
    }

    /// Strict production JSON dispatch boundary for sidecar runtime integration of the
    /// three retrieval tools. Shape errors remain protocol invalid-params;
    /// expected application failures are returned as successful tool payloads.
    pub fn dispatch_retrieval(
        &self,
        tool: &str,
        params: Value,
    ) -> Result<Value, RetrievalDispatchError> {
        let result: Result<Value, MemoryApplicationError> = match tool {
            "memory_search" => {
                let params: SearchParams = parse_params(params)?;
                params
                    .filters
                    .into_domain(&self.limits)
                    .and_then(|filters| {
                        params.page.into_domain(&self.limits).and_then(|page| {
                            SearchQuery::try_new(
                                params.query,
                                filters,
                                page,
                                RankingMode::LexicalV1,
                                &self.limits,
                            )
                        })
                    })
                    .and_then(|query| self.search(query))
                    .map(search_payload)
            }
            "memory_timeline" => {
                let params: TimelineParams = parse_params(params)?;
                params
                    .anchor
                    .into_domain()
                    .and_then(|anchor| {
                        params
                            .filters
                            .into_domain(&self.limits)
                            .and_then(|filters| {
                                params.page.into_domain(&self.limits).and_then(|page| {
                                    TimelineQuery::try_new(anchor, filters, page, &self.limits)
                                })
                            })
                    })
                    .and_then(|query| self.timeline(query))
                    .map(timeline_payload)
            }
            "memory_context_for_task" => {
                let params: ContextForTaskParams = parse_params(params)?;
                params
                    .filters
                    .into_domain(&self.limits)
                    .and_then(|filters| {
                        params.page.into_domain(&self.limits).and_then(|page| {
                            TaskContextQuery::try_new(
                                params.query,
                                filters,
                                page,
                                RankingMode::LexicalV1,
                                &self.limits,
                            )
                        })
                    })
                    .and_then(|query| self.context_for_task(query))
                    .map(context_payload)
            }
            _ => return Err(RetrievalDispatchError::UnknownTool),
        };
        match result {
            Ok(payload) => Ok(payload),
            Err(
                error @ MemoryApplicationError::InvalidArgument {
                    reason: "privacy_rejected",
                    ..
                },
            ) => Ok(application_error_payload(
                error,
                self.persistence_mode,
                self.durable_data_unavailable,
            )),
            Err(MemoryApplicationError::InvalidArgument { .. }) => {
                Err(RetrievalDispatchError::InvalidParams)
            }
            Err(error) => Ok(application_error_payload(
                error,
                self.persistence_mode,
                self.durable_data_unavailable,
            )),
        }
    }

    fn privacy_filters(
        &self,
        filters: MemoryFilters,
    ) -> Result<MemoryFilters, MemoryApplicationError> {
        let optional = |field, value: Option<&str>| {
            privacy_optional(&self.privacy, field, value, &self.limits)
        };
        MemoryFilters::try_new(
            filters.record_types().to_vec(),
            privacy_all(&self.privacy, "branches", filters.branches(), &self.limits)?,
            privacy_all(&self.privacy, "paths", filters.paths(), &self.limits)?,
            privacy_all(&self.privacy, "symbols", filters.symbols(), &self.limits)?,
            privacy_all(&self.privacy, "tags", filters.tags(), &self.limits)?,
            optional("source_agent", filters.source_agent())?,
            optional("source_client", filters.source_client())?,
            optional("created_from", filters.created_from())?,
            optional("created_to", filters.created_to())?,
            &self.limits,
        )
        .map_err(map_retrieval)
    }

    fn retrieval_snapshot(&self) -> Result<(Vec<StoredRecord>, u64), MemoryApplicationError> {
        self.check_schema()?;
        let mut cursor = None;
        let mut generation = None;
        let mut records = Vec::new();
        loop {
            let page = self
                .store
                .scan(AgentMemoryScanRequest {
                    prefix: b"record/".to_vec(),
                    cursor,
                    limit: self.limits.max_results,
                })
                .map_err(map_storage)?;
            if generation.is_some_and(|expected| expected != page.generation) {
                return Err(MemoryApplicationError::CursorExpired);
            }
            generation = Some(page.generation);
            for entry in page.entries {
                records.push(decode(&entry.value)?);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok((records, generation.unwrap_or(0)))
    }

    fn ensure_generation(&self, expected: u64) -> Result<(), MemoryApplicationError> {
        let generation = self
            .store
            .get(AgentMemoryGetRequest {
                key: b"meta/schema".to_vec(),
            })
            .map_err(map_storage)?
            .generation;
        if generation == expected {
            Ok(())
        } else {
            Err(MemoryApplicationError::CursorExpired)
        }
    }

    pub fn update(
        &self,
        mut command: UpdateCommand,
    ) -> Result<BatchMutationResult, MemoryApplicationError> {
        for item in &mut command.items {
            item.id = self.privacy_id(item.id.clone())?;
            item.expected_revision = self.privacy_revision(item.expected_revision.clone())?;
            item.record = self.privacy_record(item.record.clone())?;
        }
        if self.persistence_mode == PersistenceMode::ReadOnly {
            return Ok(self.batch_result(
                command
                    .items
                    .into_iter()
                    .map(|item| outcome(item.id, MutationOutcomeCode::ReadOnly, None, false))
                    .collect(),
            ));
        }
        let counts = counts(command.items.iter().map(|item| &item.id));
        let mut items = Vec::with_capacity(command.items.len());
        for item in command.items {
            if counts[item.id.as_str()] > 1 {
                items.push(outcome(
                    item.id,
                    MutationOutcomeCode::DuplicateId,
                    None,
                    false,
                ));
            } else {
                items.push(self.update_one(item)?);
            }
        }
        Ok(self.batch_result(items))
    }

    pub fn forget(
        &self,
        mut command: ForgetCommand,
    ) -> Result<BatchMutationResult, MemoryApplicationError> {
        for (id, revision) in &mut command.items {
            *id = self.privacy_id(id.clone())?;
            *revision = self.privacy_revision(revision.clone())?;
        }
        if self.persistence_mode == PersistenceMode::ReadOnly {
            return Ok(self.batch_result(
                command
                    .items
                    .into_iter()
                    .map(|(id, _)| outcome(id, MutationOutcomeCode::ReadOnly, None, false))
                    .collect(),
            ));
        }
        let counts = counts(command.items.iter().map(|item| &item.0));
        let mut items = Vec::with_capacity(command.items.len());
        for (id, revision) in command.items {
            if counts[id.as_str()] > 1 {
                items.push(outcome(id, MutationOutcomeCode::DuplicateId, None, false));
            } else {
                items.push(self.forget_one(id, revision)?);
            }
        }
        Ok(self.batch_result(items))
    }

    fn batch_result(&self, items: Vec<MutationItemResult>) -> BatchMutationResult {
        let durable = self.persistence_mode == PersistenceMode::Durable
            && items.iter().all(|item| item.durable);
        BatchMutationResult {
            items,
            persistence_mode: self.persistence_mode,
            durable_data_unavailable: self.durable_data_unavailable,
            durable,
        }
    }

    fn privacy_id(&self, id: MemoryId) -> Result<MemoryId, MemoryApplicationError> {
        let filtered = self
            .privacy
            .apply("id", id.as_str(), 1024)
            .map_err(map_privacy)?;
        MemoryId::try_from(filtered).map_err(map_validation)
    }

    fn privacy_revision(&self, revision: Revision) -> Result<Revision, MemoryApplicationError> {
        let filtered = self
            .privacy
            .apply("expected_revision", revision.as_str(), 1024)
            .map_err(map_privacy)?;
        Revision::try_from(filtered).map_err(map_validation)
    }

    fn privacy_record(&self, record: RecordInput) -> Result<RecordInput, MemoryApplicationError> {
        let provenance = record.provenance;
        let filtered_provenance = Provenance::try_new(
            privacy_optional(
                &self.privacy,
                "provenance.branch",
                provenance.branch(),
                &self.limits,
            )?,
            privacy_all(
                &self.privacy,
                "provenance.paths",
                provenance.paths(),
                &self.limits,
            )?,
            privacy_all(
                &self.privacy,
                "provenance.symbols",
                provenance.symbols(),
                &self.limits,
            )?,
            privacy_all(
                &self.privacy,
                "provenance.tags",
                provenance.tags(),
                &self.limits,
            )?,
            privacy_optional(
                &self.privacy,
                "provenance.source_agent",
                provenance.source_agent(),
                &self.limits,
            )?,
            privacy_optional(
                &self.privacy,
                "provenance.source_client",
                provenance.source_client(),
                &self.limits,
            )?,
            privacy_optional(
                &self.privacy,
                "provenance.commit",
                provenance.commit(),
                &self.limits,
            )?,
            privacy_optional(
                &self.privacy,
                "provenance.pull_request",
                provenance.pull_request(),
                &self.limits,
            )?,
            privacy_optional(
                &self.privacy,
                "provenance.issue",
                provenance.issue(),
                &self.limits,
            )?,
            &self.limits,
        )
        .map_err(map_validation)?;
        RecordInput::try_new(
            record.record_type,
            self.privacy
                .apply("title", &record.title, self.limits.max_title_bytes)
                .map_err(map_privacy)?,
            self.privacy
                .apply("body", &record.body, self.limits.max_body_bytes)
                .map_err(map_privacy)?,
            filtered_provenance,
            record.confidence,
            &self.limits,
        )
    }

    fn update_one(&self, item: UpdateItem) -> Result<MutationItemResult, MemoryApplicationError> {
        let key = record_key(&item.id);
        let Some(old_bytes) = self
            .store
            .get(AgentMemoryGetRequest { key: key.clone() })
            .map_err(map_storage)?
            .value
        else {
            return Ok(outcome(item.id, MutationOutcomeCode::NotFound, None, false));
        };
        let mut stored: StoredRecord = decode(&old_bytes)?;
        if stored.revision != item.expected_revision.as_str() {
            return Ok(outcome(
                item.id,
                MutationOutcomeCode::RevisionConflict,
                None,
                false,
            ));
        }
        if stored.provenance != StoredProvenance::from(&item.record.provenance) {
            return Err(invalid("provenance", "immutable"));
        }
        let revision = next_revision(&item.expected_revision)?;
        stored.revision = revision.as_str().into();
        stored.kind = kind_name(item.record.record_type).into();
        stored.title = item.record.title;
        stored.body = item.record.body;
        stored.confidence = item.record.confidence;
        stored.updated_at = timestamp(self.clock.now())?;
        let request = AgentMemoryTransactionRequest {
            conditions: vec![Condition::ValueEquals {
                key: key.clone(),
                value: old_bytes,
            }],
            mutations: vec![
                Mutation::Put {
                    key,
                    value: encode(&stored)?,
                },
                Mutation::Put {
                    key: projection_key(&item.id),
                    value: projection(&stored),
                },
                Mutation::Put {
                    key: job_key(&item.id),
                    value: b"pending".to_vec(),
                },
            ],
        };
        match self.transact(request)? {
            Tx::Committed(durable) => Ok(outcome(
                item.id,
                MutationOutcomeCode::Updated,
                Some(revision),
                durable,
            )),
            Tx::Conflict => Ok(outcome(
                item.id,
                MutationOutcomeCode::RevisionConflict,
                None,
                false,
            )),
        }
    }

    fn forget_one(
        &self,
        id: MemoryId,
        expected: Revision,
    ) -> Result<MutationItemResult, MemoryApplicationError> {
        let key = record_key(&id);
        let Some(old_bytes) = self
            .store
            .get(AgentMemoryGetRequest { key: key.clone() })
            .map_err(map_storage)?
            .value
        else {
            return Ok(outcome(id, MutationOutcomeCode::NotFound, None, false));
        };
        let stored: StoredRecord = decode(&old_bytes)?;
        if stored.revision != expected.as_str() {
            return Ok(outcome(
                id,
                MutationOutcomeCode::RevisionConflict,
                None,
                false,
            ));
        }
        let ikey = idem_digest_key(&stored.idem_digest);
        let Some(idem_bytes) = self
            .store
            .get(AgentMemoryGetRequest { key: ikey.clone() })
            .map_err(map_storage)?
            .value
        else {
            return Err(MemoryApplicationError::StorageUnavailable);
        };
        let mut idem: Idem = decode(&idem_bytes)?;
        idem.deleted = true;
        let request = AgentMemoryTransactionRequest {
            conditions: vec![Condition::ValueEquals {
                key: key.clone(),
                value: old_bytes,
            }],
            mutations: vec![
                Mutation::Delete { key },
                Mutation::Delete {
                    key: projection_key(&id),
                },
                Mutation::Delete { key: job_key(&id) },
                Mutation::Put {
                    key: tombstone_key(&id),
                    value: encode(&Tombstone { id: id.as_str() })?,
                },
                Mutation::Put {
                    key: ikey,
                    value: encode(&idem)?,
                },
            ],
        };
        match self.transact(request)? {
            Tx::Committed(durable) => {
                Ok(outcome(id, MutationOutcomeCode::Forgotten, None, durable))
            }
            Tx::Conflict => {
                let exists = self
                    .store
                    .get(AgentMemoryGetRequest {
                        key: record_key(&id),
                    })
                    .map_err(map_storage)?
                    .value
                    .is_some();
                Ok(outcome(
                    id,
                    if exists {
                        MutationOutcomeCode::RevisionConflict
                    } else {
                        MutationOutcomeCode::NotFound
                    },
                    None,
                    false,
                ))
            }
        }
    }

    fn reconcile_remember(
        &self,
        key: &[u8],
        payload_digest: &str,
    ) -> Result<RememberResult, MemoryApplicationError> {
        for _ in 0..16 {
            let bytes = self
                .store
                .get(AgentMemoryGetRequest { key: key.to_vec() })
                .map_err(map_storage)?
                .value
                .ok_or(MemoryApplicationError::StorageUnavailable)?;
            let mut result = reconcile(
                decode(&bytes)?,
                payload_digest,
                self.persistence_mode,
                self.durable_data_unavailable,
            )?;
            // A prior uncertain commit can be visible before it is durable.
            // Re-publish the identical idempotency entry conditionally so the
            // adapter flushes before promising durability. The CAS also stops
            // a racing forget from being overwritten with a live entry.
            match self.transact(AgentMemoryTransactionRequest {
                conditions: vec![Condition::ValueEquals {
                    key: key.to_vec(),
                    value: bytes.clone(),
                }],
                mutations: vec![Mutation::Put {
                    key: key.to_vec(),
                    value: bytes,
                }],
            })? {
                Tx::Committed(durable) => {
                    result.durable = durable && self.persistence_mode == PersistenceMode::Durable;
                    return Ok(result);
                }
                Tx::Conflict => continue,
            }
        }
        Err(MemoryApplicationError::StorageUnavailable)
    }

    fn read_idem(&self, key: &[u8]) -> Result<Option<Idem>, MemoryApplicationError> {
        self.store
            .get(AgentMemoryGetRequest { key: key.to_vec() })
            .map_err(map_storage)?
            .value
            .map(|bytes| decode(&bytes))
            .transpose()
    }
    fn check_schema(&self) -> Result<(), MemoryApplicationError> {
        if let Some(bytes) = self
            .store
            .get(AgentMemoryGetRequest {
                key: b"meta/schema".to_vec(),
            })
            .map_err(map_storage)?
            .value
        {
            let schema: Schema = serde_json::from_slice(&bytes).map_err(|_| {
                MemoryApplicationError::Compatibility {
                    found: None,
                    supported: SCHEMA,
                }
            })?;
            if schema.version != SCHEMA {
                return Err(MemoryApplicationError::Compatibility {
                    found: Some(schema.version),
                    supported: SCHEMA,
                });
            }
        }
        Ok(())
    }
    fn transact(
        &self,
        request: AgentMemoryTransactionRequest,
    ) -> Result<Tx, MemoryApplicationError> {
        match self.store.transact(request).map_err(map_storage)? {
            AgentMemoryTransactionOutcome::Committed { durable, .. } => Ok(Tx::Committed(durable)),
            AgentMemoryTransactionOutcome::Conflict => Ok(Tx::Conflict),
            AgentMemoryTransactionOutcome::Failure { failure } => Err(map_storage(failure)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordInput {
    pub record_type: MemoryType,
    pub title: String,
    pub body: String,
    pub provenance: Provenance,
    pub confidence: Option<f64>,
}
impl RecordInput {
    pub fn try_new(
        record_type: MemoryType,
        title: String,
        body: String,
        provenance: Provenance,
        confidence: Option<f64>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        if title.is_empty() {
            return Err(invalid("title", "empty"));
        }
        if title.len() > limits.max_title_bytes as usize {
            return Err(invalid("title", "limit_exceeded"));
        }
        if body.len() > limits.max_body_bytes as usize {
            return Err(invalid("body", "limit_exceeded"));
        }
        if confidence.is_some_and(|x| !x.is_finite() || !(0.0..=1.0).contains(&x)) {
            return Err(invalid("confidence", "invalid"));
        }
        Ok(Self {
            record_type,
            title,
            body,
            provenance,
            confidence,
        })
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct RememberCommand {
    pub record: RecordInput,
    pub idempotency_key: String,
}
impl RememberCommand {
    pub fn try_new(
        record: RecordInput,
        idempotency_key: String,
        _limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        opaque("idempotency_key", &idempotency_key)?;
        Ok(Self {
            record,
            idempotency_key,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetQuery {
    pub ids: Vec<MemoryId>,
}
impl GetQuery {
    pub fn try_new(
        ids: Vec<MemoryId>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        batch_bound(ids.len(), limits)?;
        Ok(Self { ids })
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateItem {
    pub id: MemoryId,
    pub expected_revision: Revision,
    pub record: RecordInput,
}
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateCommand {
    pub items: Vec<UpdateItem>,
}
impl UpdateCommand {
    pub fn try_new(
        items: Vec<UpdateItem>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        batch_bound(items.len(), limits)?;
        Ok(Self { items })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgetCommand {
    pub items: Vec<(MemoryId, Revision)>,
}
impl ForgetCommand {
    pub fn try_new(
        items: Vec<(MemoryId, Revision)>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        batch_bound(items.len(), limits)?;
        Ok(Self { items })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RememberResult {
    pub id: MemoryId,
    pub revision: Revision,
    pub persistence_mode: PersistenceMode,
    pub durable_data_unavailable: bool,
    pub durable: bool,
}
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum GetItemResult {
    Found { id: MemoryId, record: MemoryRecord },
    NotFound { id: MemoryId },
}
#[derive(Clone, Debug, PartialEq)]
pub struct GetResult {
    pub items: Vec<GetItemResult>,
    pub persistence_mode: PersistenceMode,
    pub durable_data_unavailable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    limit: u32,
    response_bytes: u32,
    cursor: Option<String>,
}
impl Page {
    pub fn try_new(
        limit: Option<u32>,
        response_bytes: Option<u32>,
        cursor: Option<String>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        let limit = limit.unwrap_or(limits.default_results);
        let response_bytes = response_bytes.unwrap_or(limits.default_response_bytes);
        if limit == 0 || limit > limits.max_results {
            return Err(invalid("limit", "out_of_range"));
        }
        if response_bytes == 0 || response_bytes > limits.max_response_bytes {
            return Err(invalid("response_bytes", "out_of_range"));
        }
        if cursor
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 1024)
        {
            return Err(MemoryApplicationError::CursorExpired);
        }
        Ok(Self {
            limit,
            response_bytes,
            cursor,
        })
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn response_bytes(&self) -> u32 {
        self.response_bytes
    }

    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineAnchor {
    Record {
        id: MemoryId,
        before: u32,
        after: u32,
    },
    Query {
        query: String,
    },
    Window {
        from: String,
        to: String,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct SearchQuery {
    pub query: String,
    pub filters: MemoryFilters,
    pub page: Page,
    pub ranking: RankingMode,
}
impl SearchQuery {
    pub fn try_new(
        query: String,
        filters: MemoryFilters,
        page: Page,
        ranking: RankingMode,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        let query = normalize_query(query, limits)?;
        Ok(Self {
            query,
            filters,
            page,
            ranking,
        })
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineQuery {
    pub anchor: TimelineAnchor,
    pub filters: MemoryFilters,
    pub page: Page,
}
impl TimelineQuery {
    pub fn try_new(
        anchor: TimelineAnchor,
        filters: MemoryFilters,
        page: Page,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        let anchor = match anchor {
            TimelineAnchor::Record { id, before, after } => {
                if before >= limits.max_results {
                    return Err(invalid("before", "out_of_range"));
                }
                if after >= limits.max_results {
                    return Err(invalid("after", "out_of_range"));
                }
                if before
                    .checked_add(after)
                    .and_then(|n| n.checked_add(1))
                    .is_none_or(|n| n > limits.max_results)
                {
                    return Err(invalid("after", "out_of_range"));
                }
                TimelineAnchor::Record { id, before, after }
            }
            TimelineAnchor::Query { query } => TimelineAnchor::Query {
                query: normalize_query(query, limits)?,
            },
            TimelineAnchor::Window { from, to } => {
                let from = normalize_timestamp_arg("from", from)?;
                let to = normalize_timestamp_arg("to", to)?;
                if timestamp_cmp(&to, &from).is_lt() {
                    return Err(invalid("to", "before_from"));
                }
                TimelineAnchor::Window { from, to }
            }
        };
        Ok(Self {
            anchor,
            filters,
            page,
        })
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct TaskContextQuery {
    pub query: String,
    pub filters: MemoryFilters,
    pub page: Page,
    pub ranking: RankingMode,
}
impl TaskContextQuery {
    pub fn try_new(
        query: String,
        filters: MemoryFilters,
        page: Page,
        ranking: RankingMode,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, MemoryApplicationError> {
        let query = normalize_query(query, limits)?;
        Ok(Self {
            query,
            filters,
            page,
            ranking,
        })
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct SearchResult {
    pub hits: Vec<CompactHit>,
    pub next_cursor: Option<String>,
    pub persistence_mode: PersistenceMode,
    pub durable_data_unavailable: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineResult {
    Found {
        events: Vec<CompactHit>,
        next_cursor: Option<String>,
        persistence_mode: PersistenceMode,
        durable_data_unavailable: bool,
    },
    NotFound {
        persistence_mode: PersistenceMode,
        durable_data_unavailable: bool,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct TaskContextResult {
    pub hits: Vec<CompactHit>,
    pub applied_filters: MemoryFilters,
    pub next_cursor: Option<String>,
    pub persistence_mode: PersistenceMode,
    pub durable_data_unavailable: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationOutcomeCode {
    Updated,
    Forgotten,
    NotFound,
    DuplicateId,
    RevisionConflict,
    ReadOnly,
    IdempotencyConflict,
    RecordForgotten,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationItemResult {
    pub id: MemoryId,
    pub code: MutationOutcomeCode,
    pub revision: Option<Revision>,
    pub durable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchMutationResult {
    pub items: Vec<MutationItemResult>,
    pub persistence_mode: PersistenceMode,
    pub durable_data_unavailable: bool,
    pub durable: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryApplicationError {
    InvalidArgument {
        field: &'static str,
        reason: &'static str,
    },
    StorageUnavailable,
    ReadOnly,
    CommitOutcomeUnknown,
    Compatibility {
        found: Option<u32>,
        supported: u32,
    },
    Migration {
        stage: &'static str,
    },
    CursorExpired,
    ResponseBudgetTooSmall {
        minimum_bytes: usize,
    },
    ProviderTimeout,
    ProviderCancelled,
    ProviderUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreStatus {
    pub schema_version: u32,
    pub persistence_mode: PersistenceMode,
    pub durable_data_unavailable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnrichmentPublication {
    Published {
        id: MemoryId,
        revision: Revision,
        durable: bool,
    },
    Discarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrievalDispatchError {
    InvalidParams,
    UnknownTool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    query: String,
    #[serde(default)]
    filters: FiltersParams,
    #[serde(default)]
    page: PageParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineParams {
    anchor: TimelineAnchorParams,
    #[serde(default)]
    filters: FiltersParams,
    #[serde(default)]
    page: PageParams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextForTaskParams {
    query: String,
    #[serde(default)]
    filters: FiltersParams,
    #[serde(default)]
    page: PageParams,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageParams {
    limit: Option<u32>,
    response_bytes: Option<u32>,
    cursor: Option<String>,
}

impl PageParams {
    fn into_domain(self, limits: &AgentMemoryLimitsConfig) -> Result<Page, MemoryApplicationError> {
        Page::try_new(self.limit, self.response_bytes, self.cursor, limits)
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FiltersParams {
    #[serde(default)]
    record_types: Vec<RecordTypeParam>,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    source_agent: Option<String>,
    source_client: Option<String>,
    created_from: Option<String>,
    created_to: Option<String>,
}

impl FiltersParams {
    fn into_domain(
        self,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<MemoryFilters, MemoryApplicationError> {
        MemoryFilters::try_new(
            self.record_types.into_iter().map(Into::into).collect(),
            self.branches,
            self.paths,
            self.symbols,
            self.tags,
            self.source_agent,
            self.source_client,
            self.created_from,
            self.created_to,
            limits,
        )
        .map_err(map_retrieval)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordTypeParam {
    Observation,
    Decision,
    Bugfix,
    ReviewRule,
    TaskNote,
    SessionSummary,
}

impl From<RecordTypeParam> for MemoryType {
    fn from(value: RecordTypeParam) -> Self {
        match value {
            RecordTypeParam::Observation => Self::Observation,
            RecordTypeParam::Decision => Self::Decision,
            RecordTypeParam::Bugfix => Self::Bugfix,
            RecordTypeParam::ReviewRule => Self::ReviewRule,
            RecordTypeParam::TaskNote => Self::TaskNote,
            RecordTypeParam::SessionSummary => Self::SessionSummary,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TimelineAnchorParams {
    Record { id: String, before: u32, after: u32 },
    Query { query: String },
    Window { from: String, to: String },
}

impl TimelineAnchorParams {
    fn into_domain(self) -> Result<TimelineAnchor, MemoryApplicationError> {
        Ok(match self {
            Self::Record { id, before, after } => TimelineAnchor::Record {
                id: MemoryId::try_from(id).map_err(map_validation)?,
                before,
                after,
            },
            Self::Query { query } => TimelineAnchor::Query { query },
            Self::Window { from, to } => TimelineAnchor::Window { from, to },
        })
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RetrievalDispatchError> {
    serde_json::from_value(params).map_err(|_| RetrievalDispatchError::InvalidParams)
}

fn search_payload(result: SearchResult) -> Value {
    json!({
        "code": "ok",
        "hits": result.hits.iter().map(compact_hit_payload).collect::<Vec<_>>(),
        "next_cursor": result.next_cursor,
        "persistence_mode": persistence_mode_name(result.persistence_mode),
        "durable_data_unavailable": result.durable_data_unavailable,
    })
}

fn timeline_payload(result: TimelineResult) -> Value {
    match result {
        TimelineResult::Found {
            events,
            next_cursor,
            persistence_mode,
            durable_data_unavailable,
        } => json!({
            "code": "ok",
            "events": events.iter().map(compact_hit_payload).collect::<Vec<_>>(),
            "next_cursor": next_cursor,
            "persistence_mode": persistence_mode_name(persistence_mode),
            "durable_data_unavailable": durable_data_unavailable,
        }),
        TimelineResult::NotFound {
            persistence_mode,
            durable_data_unavailable,
        } => json!({
            "code": "not_found",
            "persistence_mode": persistence_mode_name(persistence_mode),
            "durable_data_unavailable": durable_data_unavailable,
        }),
    }
}

fn context_payload(result: TaskContextResult) -> Value {
    json!({
        "code": "ok",
        "hits": result.hits.iter().map(compact_hit_payload).collect::<Vec<_>>(),
        "applied_filters": filters_payload(&result.applied_filters),
        "next_cursor": result.next_cursor,
        "persistence_mode": persistence_mode_name(result.persistence_mode),
        "durable_data_unavailable": result.durable_data_unavailable,
    })
}

fn compact_hit_payload(hit: &CompactHit) -> Value {
    crate::domain::compact_hit_json(hit)
}

fn filters_payload(filters: &MemoryFilters) -> Value {
    crate::domain::filters_json(filters)
}

fn persistence_mode_name(mode: PersistenceMode) -> &'static str {
    match mode {
        PersistenceMode::Durable => "durable",
        PersistenceMode::ReadOnly => "read_only",
        PersistenceMode::InMemory => "in_memory",
    }
}

pub(crate) fn application_error_payload(
    error: MemoryApplicationError,
    persistence_mode: PersistenceMode,
    durable_data_unavailable: bool,
) -> Value {
    let (code, minimum_bytes) = match error {
        MemoryApplicationError::StorageUnavailable => ("storage_unavailable", None),
        MemoryApplicationError::ReadOnly => ("read_only", None),
        MemoryApplicationError::CommitOutcomeUnknown => ("commit_outcome_unknown", None),
        MemoryApplicationError::Compatibility { .. } => ("incompatible_schema", None),
        MemoryApplicationError::Migration { .. } => ("migration_failed", None),
        MemoryApplicationError::CursorExpired => ("cursor_expired", None),
        MemoryApplicationError::ResponseBudgetTooSmall { minimum_bytes } => {
            ("response_budget_too_small", Some(minimum_bytes))
        }
        MemoryApplicationError::ProviderTimeout
        | MemoryApplicationError::ProviderCancelled
        | MemoryApplicationError::ProviderUnavailable => ("storage_unavailable", None),
        MemoryApplicationError::InvalidArgument {
            reason: "privacy_rejected",
            ..
        } => ("privacy_rejected", None),
        MemoryApplicationError::InvalidArgument { .. } => ("invalid_argument", None),
    };
    let mut payload = json!({
        "code": code,
        "persistence_mode": persistence_mode_name(persistence_mode),
        "durable_data_unavailable": durable_data_unavailable,
    });
    if let Some(minimum_bytes) = minimum_bytes {
        payload["minimum_bytes"] = json!(minimum_bytes);
    }
    payload
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorWire {
    v: u8,
    request_digest: String,
    generation: u64,
    offset: u64,
}

fn normalize_query(
    value: String,
    limits: &AgentMemoryLimitsConfig,
) -> Result<String, MemoryApplicationError> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return Err(invalid("query", "empty"));
    }
    if value.len() > limits.max_query_bytes as usize {
        return Err(invalid("query", "limit_exceeded"));
    }
    Ok(value)
}

fn normalize_timestamp_arg(
    field: &'static str,
    value: String,
) -> Result<String, MemoryApplicationError> {
    let parsed = DateTime::parse_from_rfc3339(&value).map_err(|_| invalid(field, "invalid"))?;
    Ok(parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn request_digest(
    tool: &str,
    query: &str,
    filters: &MemoryFilters,
    ranking: RankingMode,
    page: &Page,
) -> Result<[u8; 32], MemoryApplicationError> {
    let canonical = format!(
        "{tool}\n{query}\n{}\n{ranking:?}\n{}\n{}",
        canonical_filters(filters),
        page.limit(),
        page.response_bytes()
    );
    Ok(Sha256::digest(canonical.as_bytes()).into())
}

fn canonical_filters(filters: &MemoryFilters) -> String {
    fn sorted<T: std::fmt::Debug + Ord + Clone>(values: &[T]) -> String {
        let mut values = values.to_vec();
        values.sort();
        values.dedup();
        format!("{values:?}")
    }
    let mut record_types = filters
        .record_types()
        .iter()
        .map(|value| kind_name(*value))
        .collect::<Vec<_>>();
    record_types.sort_unstable();
    record_types.dedup();
    format!(
        "{record_types:?}|{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}",
        sorted(filters.branches()),
        sorted(filters.paths()),
        sorted(filters.symbols()),
        sorted(filters.tags()),
        filters.source_agent(),
        filters.source_client(),
        filters.created_from(),
        filters.created_to()
    )
}

fn cursor_offset(
    page: &Page,
    digest: [u8; 32],
    generation: u64,
) -> Result<usize, MemoryApplicationError> {
    let Some(encoded) = page.cursor() else {
        return Ok(0);
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| MemoryApplicationError::CursorExpired)?;
    let wire: CursorWire =
        serde_json::from_slice(&decoded).map_err(|_| MemoryApplicationError::CursorExpired)?;
    let canonical = serde_json::to_vec(&wire).map_err(|_| MemoryApplicationError::CursorExpired)?;
    if wire.v != 1
        || wire.offset == 0
        || wire.generation != generation
        || wire.request_digest != hash_bytes(&digest)
        || canonical != decoded
        || URL_SAFE_NO_PAD.encode(canonical) != encoded
    {
        return Err(MemoryApplicationError::CursorExpired);
    }
    usize::try_from(wire.offset).map_err(|_| MemoryApplicationError::CursorExpired)
}

fn encode_cursor(
    digest: [u8; 32],
    generation: u64,
    offset: usize,
) -> Result<String, MemoryApplicationError> {
    let wire = CursorWire {
        v: 1,
        request_digest: hash_bytes(&digest),
        generation,
        offset: u64::try_from(offset).map_err(|_| MemoryApplicationError::CursorExpired)?,
    };
    Ok(URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&wire).map_err(|_| MemoryApplicationError::StorageUnavailable)?))
}

fn hash_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn matches_filters(record: &StoredRecord, filters: &MemoryFilters) -> bool {
    (filters.record_types().is_empty()
        || filters
            .record_types()
            .iter()
            .any(|kind| kind_name(*kind) == record.kind))
        && (filters.branches().is_empty()
            || record
                .provenance
                .branch
                .as_ref()
                .is_some_and(|x| filters.branches().contains(x)))
        && (filters.paths().is_empty()
            || filters
                .paths()
                .iter()
                .any(|x| record.provenance.paths.contains(x)))
        && (filters.symbols().is_empty()
            || filters
                .symbols()
                .iter()
                .any(|x| record.provenance.symbols.contains(x)))
        && (filters.tags().is_empty()
            || filters
                .tags()
                .iter()
                .any(|x| record.provenance.tags.contains(x)))
        && filters
            .source_agent()
            .is_none_or(|x| record.provenance.source_agent.as_deref() == Some(x))
        && filters
            .source_client()
            .is_none_or(|x| record.provenance.source_client.as_deref() == Some(x))
        && filters
            .created_from()
            .is_none_or(|x| !timestamp_cmp(&record.created_at, x).is_lt())
        && filters
            .created_to()
            .is_none_or(|x| timestamp_cmp(&record.created_at, x).is_lt())
}

fn has_explicit_metadata_filter(filters: &MemoryFilters) -> bool {
    !filters.paths().is_empty()
        || !filters.branches().is_empty()
        || !filters.symbols().is_empty()
        || !filters.tags().is_empty()
        || filters.source_agent().is_some()
        || filters.source_client().is_some()
}

fn matched_filter_reasons(record: &StoredRecord, filters: &MemoryFilters) -> Vec<MatchReason> {
    let mut fields = Vec::new();
    if !filters.branches().is_empty()
        && record
            .provenance
            .branch
            .as_ref()
            .is_some_and(|branch| filters.branches().contains(branch))
    {
        fields.push(MatchReasonField::Branch);
    }
    if filters
        .paths()
        .iter()
        .any(|path| record.provenance.paths.contains(path))
    {
        fields.push(MatchReasonField::Path);
    }
    if filters
        .symbols()
        .iter()
        .any(|symbol| record.provenance.symbols.contains(symbol))
    {
        fields.push(MatchReasonField::Symbol);
    }
    if filters
        .tags()
        .iter()
        .any(|tag| record.provenance.tags.contains(tag))
    {
        fields.push(MatchReasonField::Tag);
    }
    if filters
        .source_agent()
        .is_some_and(|source| record.provenance.source_agent.as_deref() == Some(source))
    {
        fields.push(MatchReasonField::SourceAgent);
    }
    if filters
        .source_client()
        .is_some_and(|source| record.provenance.source_client.as_deref() == Some(source))
    {
        fields.push(MatchReasonField::SourceClient);
    }
    fields
        .into_iter()
        .map(|field| MatchReason::new(MatchReasonKind::Metadata, field))
        .collect()
}

fn tokens(value: &str) -> std::collections::BTreeSet<String> {
    value.split_whitespace().map(str::to_lowercase).collect()
}

fn score_record(record: &StoredRecord, query: &str) -> Option<(f64, Vec<MatchReason>)> {
    let query_tokens = tokens(query);
    let title = tokens(&record.title);
    let body = tokens(&record.body);
    let mut score = 0.0;
    let mut reasons = Vec::new();
    if query_tokens.iter().any(|token| title.contains(token)) {
        score += query_tokens
            .iter()
            .filter(|token| title.contains(*token))
            .count() as f64;
        reasons.push(MatchReason::new(
            MatchReasonKind::Lexical,
            MatchReasonField::Title,
        ));
    }
    if query_tokens.iter().any(|token| body.contains(token)) {
        score += query_tokens
            .iter()
            .filter(|token| body.contains(*token))
            .count() as f64;
        reasons.push(MatchReason::new(
            MatchReasonKind::Lexical,
            MatchReasonField::Body,
        ));
    }
    let metadata = [
        (
            record.provenance.branch.iter().collect::<Vec<_>>(),
            MatchReasonField::Branch,
        ),
        (
            record.provenance.paths.iter().collect(),
            MatchReasonField::Path,
        ),
        (
            record.provenance.symbols.iter().collect(),
            MatchReasonField::Symbol,
        ),
        (
            record.provenance.tags.iter().collect(),
            MatchReasonField::Tag,
        ),
        (
            record.provenance.source_agent.iter().collect(),
            MatchReasonField::SourceAgent,
        ),
        (
            record.provenance.source_client.iter().collect(),
            MatchReasonField::SourceClient,
        ),
    ];
    for (values, field) in metadata {
        let matches = query_tokens
            .iter()
            .filter(|token| values.iter().any(|value| value.to_lowercase() == **token))
            .count();
        if matches > 0 {
            score += matches as f64;
            reasons.push(MatchReason::new(MatchReasonKind::Metadata, field));
        }
    }
    (score > 0.0).then_some((score, reasons))
}

fn scored_hit(
    record: StoredRecord,
    query: &str,
    limits: &AgentMemoryLimitsConfig,
) -> Result<Option<CompactHit>, MemoryApplicationError> {
    score_record(&record, query)
        .map(|(score, reasons)| compact_hit(record, score, reasons, limits))
        .transpose()
}

fn compact_hit(
    record: StoredRecord,
    score: f64,
    reasons: Vec<MatchReason>,
    limits: &AgentMemoryLimitsConfig,
) -> Result<CompactHit, MemoryApplicationError> {
    CompactHit::try_new(
        MemoryId::try_from(record.id).map_err(map_validation)?,
        Revision::try_from(record.revision).map_err(map_validation)?,
        parse_kind(&record.kind)?,
        record.title,
        record.provenance.paths,
        record.created_at,
        score,
        reasons,
        Vec::new(),
        limits,
    )
    .map_err(map_retrieval)
}

fn search_hit_order(left: &CompactHit, right: &CompactHit) -> std::cmp::Ordering {
    right
        .score()
        .total_cmp(&left.score())
        .then_with(|| timestamp_cmp(right.timestamp(), left.timestamp()))
        .then_with(|| left.id().cmp(right.id()))
}

fn timeline_record_order(left: &StoredRecord, right: &StoredRecord) -> std::cmp::Ordering {
    timestamp_cmp(&left.created_at, &right.created_at).then_with(|| left.id.cmp(&right.id))
}

fn timestamp_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    match (
        DateTime::parse_from_rfc3339(left),
        DateTime::parse_from_rfc3339(right),
    ) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

fn timeline_anchor_key(anchor: &TimelineAnchor) -> String {
    format!("{anchor:?}")
}

struct RetrievalEnvelope<'a> {
    result_field: &'static str,
    applied_filters: Option<&'a MemoryFilters>,
    persistence_status: (PersistenceMode, bool),
}

fn page_hits(
    mut hits: Vec<CompactHit>,
    offset: usize,
    page: &Page,
    digest: [u8; 32],
    generation: u64,
    envelope: RetrievalEnvelope<'_>,
) -> Result<(Vec<CompactHit>, Option<String>), MemoryApplicationError> {
    if offset > hits.len() || (offset == hits.len() && offset != 0) {
        return Err(MemoryApplicationError::CursorExpired);
    }
    let total = hits.len();
    let limit = page.limit() as usize;
    let budget = page.response_bytes() as usize;
    hits = hits.into_iter().skip(offset).take(limit).collect();
    if hits.is_empty() {
        let minimum = serialized_retrieval_len(&[], None, &envelope)?;
        if minimum > budget {
            return Err(MemoryApplicationError::ResponseBudgetTooSmall {
                minimum_bytes: minimum,
            });
        }
    } else {
        let first_cursor = (offset + 1 < total)
            .then(|| encode_cursor(digest, generation, offset + 1))
            .transpose()?;
        let minimum = serialized_retrieval_len(&hits[..1], first_cursor.as_deref(), &envelope)?;
        if minimum > budget {
            return Err(MemoryApplicationError::ResponseBudgetTooSmall {
                minimum_bytes: minimum,
            });
        }
        while !hits.is_empty() {
            let candidate_offset = offset + hits.len();
            let candidate_cursor = (candidate_offset < total)
                .then(|| encode_cursor(digest, generation, candidate_offset))
                .transpose()?;
            if serialized_retrieval_len(&hits, candidate_cursor.as_deref(), &envelope)? <= budget {
                break;
            }
            hits.pop();
        }
    }
    let next_offset = offset + hits.len();
    let next_cursor = if next_offset < total {
        Some(encode_cursor(digest, generation, next_offset)?)
    } else {
        None
    };
    Ok((hits, next_cursor))
}

fn serialized_retrieval_len(
    hits: &[CompactHit],
    cursor: Option<&str>,
    envelope: &RetrievalEnvelope<'_>,
) -> Result<usize, MemoryApplicationError> {
    let mut value = serde_json::json!({
        "code": "ok",
        "persistence_mode": persistence_mode_name(envelope.persistence_status.0),
        "durable_data_unavailable": envelope.persistence_status.1,
        (envelope.result_field): hits.iter().map(compact_hit_payload).collect::<Vec<_>>(),
        "next_cursor": cursor,
    });
    if let Some(filters) = envelope.applied_filters {
        value
            .as_object_mut()
            .expect("retrieval envelope is an object")
            .insert("applied_filters".to_owned(), filters_payload(filters));
    }
    serde_json::to_vec(&value)
        .map(|bytes| bytes.len())
        .map_err(|_| MemoryApplicationError::StorageUnavailable)
}

fn map_retrieval(error: crate::RetrievalError) -> MemoryApplicationError {
    match error {
        crate::RetrievalError::InvalidArgument { field, reason } => invalid(field, reason),
        crate::RetrievalError::CursorExpired => MemoryApplicationError::CursorExpired,
        crate::RetrievalError::ResponseBudgetTooSmall { minimum_bytes } => {
            MemoryApplicationError::ResponseBudgetTooSmall { minimum_bytes }
        }
    }
}

#[derive(Serialize)]
struct StoredInput {
    kind: String,
    title: String,
    body: String,
    provenance: StoredProvenance,
    confidence: Option<f64>,
}
impl From<&RecordInput> for StoredInput {
    fn from(x: &RecordInput) -> Self {
        Self {
            kind: kind_name(x.record_type).into(),
            title: x.title.clone(),
            body: x.body.clone(),
            provenance: StoredProvenance::from(&x.provenance),
            confidence: x.confidence,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    id: String,
    revision: String,
    #[serde(rename = "schema_version")]
    schema: u32,
    kind: String,
    title: String,
    body: String,
    provenance: StoredProvenance,
    confidence: Option<f64>,
    project_key: String,
    created_at: String,
    updated_at: String,
    idem_digest: String,
}
impl From<&MemoryRecord> for StoredRecord {
    fn from(x: &MemoryRecord) -> Self {
        Self {
            id: x.id().as_str().into(),
            revision: x.revision().as_str().into(),
            schema: x.schema_version(),
            kind: kind_name(x.record_type()).into(),
            title: x.title().into(),
            body: x.body().into(),
            provenance: StoredProvenance::from(x.provenance()),
            confidence: x.confidence(),
            project_key: x.project_key().into(),
            created_at: x.created_at().into(),
            updated_at: x.updated_at().into(),
            idem_digest: String::new(),
        }
    }
}
impl StoredRecord {
    fn domain(self) -> Result<MemoryRecord, MemoryApplicationError> {
        MemoryRecord::try_new(
            MemoryId::try_from(self.id).map_err(map_validation)?,
            Revision::try_from(self.revision).map_err(map_validation)?,
            self.schema,
            parse_kind(&self.kind)?,
            self.title,
            self.body,
            self.provenance.domain()?,
            self.confidence,
            self.project_key,
            self.created_at,
            self.updated_at,
            &wide_limits(),
        )
        .map_err(map_validation)
    }
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct StoredProvenance {
    branch: Option<String>,
    paths: Vec<String>,
    symbols: Vec<String>,
    tags: Vec<String>,
    source_agent: Option<String>,
    source_client: Option<String>,
    commit: Option<String>,
    pull_request: Option<String>,
    issue: Option<String>,
}
impl From<&Provenance> for StoredProvenance {
    fn from(x: &Provenance) -> Self {
        Self {
            branch: x.branch().map(str::to_owned),
            paths: x.paths().to_vec(),
            symbols: x.symbols().to_vec(),
            tags: x.tags().to_vec(),
            source_agent: x.source_agent().map(str::to_owned),
            source_client: x.source_client().map(str::to_owned),
            commit: x.commit().map(str::to_owned),
            pull_request: x.pull_request().map(str::to_owned),
            issue: x.issue().map(str::to_owned),
        }
    }
}
impl StoredProvenance {
    fn domain(self) -> Result<Provenance, MemoryApplicationError> {
        Provenance::try_new(
            self.branch,
            self.paths,
            self.symbols,
            self.tags,
            self.source_agent,
            self.source_client,
            self.commit,
            self.pull_request,
            self.issue,
            &wide_limits(),
        )
        .map_err(map_validation)
    }
}
#[derive(Serialize, Deserialize)]
struct Idem {
    id: String,
    payload_digest: String,
    deleted: bool,
}
#[derive(Serialize)]
struct Tombstone<'a> {
    id: &'a str,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schema {
    version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationCheckpoint {
    source: u32,
    target: u32,
    state: String,
}

impl MigrationCheckpoint {
    fn prepared() -> Self {
        Self {
            source: 0,
            target: SCHEMA,
            state: "prepared".to_owned(),
        }
    }
}
enum Tx {
    Committed(bool),
    Conflict,
}

fn reconcile(
    entry: Idem,
    expected: &str,
    persistence_mode: PersistenceMode,
    durable_data_unavailable: bool,
) -> Result<RememberResult, MemoryApplicationError> {
    if entry.deleted {
        return Err(invalid("idempotency_key", "record_forgotten"));
    }
    if entry.payload_digest != expected {
        return Err(invalid("idempotency_key", "idempotency_conflict"));
    }
    Ok(remember_result(
        MemoryId::try_from(entry.id).map_err(map_validation)?,
        Revision::try_from("1".to_owned()).map_err(map_validation)?,
        persistence_mode,
        durable_data_unavailable,
        persistence_mode == PersistenceMode::Durable,
    ))
}
fn remember_result(
    id: MemoryId,
    revision: Revision,
    persistence_mode: PersistenceMode,
    durable_data_unavailable: bool,
    durable: bool,
) -> RememberResult {
    RememberResult {
        id,
        revision,
        persistence_mode,
        durable_data_unavailable,
        durable,
    }
}
fn outcome(
    id: MemoryId,
    code: MutationOutcomeCode,
    revision: Option<Revision>,
    durable: bool,
) -> MutationItemResult {
    MutationItemResult {
        id,
        code,
        revision,
        durable,
    }
}
fn batch_bound(n: usize, limits: &AgentMemoryLimitsConfig) -> Result<(), MemoryApplicationError> {
    if n == 0 {
        Err(invalid("items", "empty"))
    } else if n > limits.max_batch_items as usize {
        Err(invalid("items", "limit_exceeded"))
    } else {
        Ok(())
    }
}
fn opaque(field: &'static str, value: &str) -> Result<(), MemoryApplicationError> {
    if value.is_empty() {
        Err(invalid(field, "empty"))
    } else if value.len() > 1024 {
        Err(invalid(field, "limit_exceeded"))
    } else {
        Ok(())
    }
}
fn invalid(field: &'static str, reason: &'static str) -> MemoryApplicationError {
    MemoryApplicationError::InvalidArgument { field, reason }
}
fn map_storage(x: AgentMemoryStorageFailure) -> MemoryApplicationError {
    match x {
        AgentMemoryStorageFailure::BoundsExceeded => invalid("storage_request", "bounds_exceeded"),
        AgentMemoryStorageFailure::Unavailable => MemoryApplicationError::StorageUnavailable,
        AgentMemoryStorageFailure::CommitOutcomeUnknown => {
            MemoryApplicationError::CommitOutcomeUnknown
        }
    }
}
fn map_provider(x: crate::EnrichmentProviderError) -> MemoryApplicationError {
    match x {
        crate::EnrichmentProviderError::Timeout => MemoryApplicationError::ProviderTimeout,
        crate::EnrichmentProviderError::Cancelled => MemoryApplicationError::ProviderCancelled,
        crate::EnrichmentProviderError::Unavailable => MemoryApplicationError::ProviderUnavailable,
    }
}
fn map_validation(x: ValidationError) -> MemoryApplicationError {
    match x {
        ValidationError::EmptyField { field } => invalid(field, "empty"),
        ValidationError::LimitExceeded { field, .. } => invalid(field, "limit_exceeded"),
        ValidationError::InvalidTimestamp { .. } => invalid("timestamp", "out_of_range"),
        ValidationError::InvalidConfidence => invalid("confidence", "invalid"),
        ValidationError::InvalidMemoryId => invalid("id", "invalid"),
        ValidationError::InvalidRevision => invalid("expected_revision", "invalid"),
        ValidationError::MissingField { field }
        | ValidationError::InvalidMetadataType { field } => invalid(field, "invalid"),
        ValidationError::UnsupportedMemoryType => invalid("record_type", "unsupported"),
    }
}
fn map_privacy(error: PrivacyError) -> MemoryApplicationError {
    match error {
        PrivacyError::Rejected { field, .. } => invalid(field, "privacy_rejected"),
        PrivacyError::RedactionFailed { field, .. } => invalid(field, "privacy_redaction_failed"),
        PrivacyError::InvalidRule { .. } => invalid("privacy", "invalid_rule"),
    }
}
fn privacy_optional(
    filter: &PrivacyFilter,
    field: &'static str,
    value: Option<&str>,
    limits: &AgentMemoryLimitsConfig,
) -> Result<Option<String>, MemoryApplicationError> {
    value
        .map(|value| {
            filter
                .apply(field, value, limits.max_metadata_string_bytes)
                .map_err(map_privacy)
        })
        .transpose()
}
fn privacy_all(
    filter: &PrivacyFilter,
    field: &'static str,
    values: &[String],
    limits: &AgentMemoryLimitsConfig,
) -> Result<Vec<String>, MemoryApplicationError> {
    values
        .iter()
        .map(|value| {
            filter
                .apply(field, value, limits.max_metadata_string_bytes)
                .map_err(map_privacy)
        })
        .collect()
}
fn timestamp(x: SystemTime) -> Result<String, MemoryApplicationError> {
    let d = x
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| invalid("timestamp", "out_of_range"))?;
    let s = i64::try_from(d.as_secs()).map_err(|_| invalid("timestamp", "out_of_range"))?;
    Ok(DateTime::<Utc>::from_timestamp(s, d.subsec_nanos())
        .ok_or_else(|| invalid("timestamp", "out_of_range"))?
        .to_rfc3339_opts(SecondsFormat::AutoSi, true))
}
fn next_revision(x: &Revision) -> Result<Revision, MemoryApplicationError> {
    let n = x
        .as_str()
        .parse::<u64>()
        .map_err(|_| invalid("expected_revision", "invalid"))?;
    Revision::try_from(
        n.checked_add(1)
            .ok_or_else(|| invalid("expected_revision", "out_of_range"))?
            .to_string(),
    )
    .map_err(map_validation)
}
fn counts<'a>(ids: impl Iterator<Item = &'a MemoryId>) -> BTreeMap<String, usize> {
    let mut x = BTreeMap::new();
    for id in ids {
        *x.entry(id.as_str().into()).or_default() += 1;
    }
    x
}
fn encode<T: Serialize>(x: &T) -> Result<Vec<u8>, MemoryApplicationError> {
    serde_json::to_vec(x).map_err(|_| MemoryApplicationError::StorageUnavailable)
}
fn decode<T: for<'a> Deserialize<'a>>(x: &[u8]) -> Result<T, MemoryApplicationError> {
    serde_json::from_slice(x).map_err(|_| MemoryApplicationError::StorageUnavailable)
}
fn hash(x: &[u8]) -> String {
    Sha256::digest(x)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn idem_key(x: &str) -> Vec<u8> {
    idem_digest_key(&hash(x.as_bytes()))
}
fn idem_digest_key(x: &str) -> Vec<u8> {
    format!("idempotency/{x}").into_bytes()
}
fn record_key(x: &MemoryId) -> Vec<u8> {
    namespaced_id_key("record", x)
}
fn record_key_for_stored_id(id: &str) -> Option<Vec<u8>> {
    MemoryId::try_from(id.to_owned())
        .ok()
        .map(|id| record_key(&id))
}
fn projection_key(x: &MemoryId) -> Vec<u8> {
    namespaced_id_key("projection", x)
}
fn job_key(x: &MemoryId) -> Vec<u8> {
    namespaced_id_key("job", x)
}
fn tombstone_key(x: &MemoryId) -> Vec<u8> {
    namespaced_id_key("tombstone", x)
}
fn namespaced_id_key(prefix: &str, id: &MemoryId) -> Vec<u8> {
    format!(
        "{prefix}/{}",
        URL_SAFE_NO_PAD.encode(id.as_str().as_bytes())
    )
    .into_bytes()
}
fn projection(x: &StoredRecord) -> Vec<u8> {
    format!("{}\n{}", x.title, x.body)
        .to_lowercase()
        .into_bytes()
}
fn kind_name(x: MemoryType) -> &'static str {
    match x {
        MemoryType::Observation => "observation",
        MemoryType::Decision => "decision",
        MemoryType::Bugfix => "bugfix",
        MemoryType::ReviewRule => "review_rule",
        MemoryType::TaskNote => "task_note",
        MemoryType::SessionSummary => "session_summary",
    }
}
fn parse_kind(x: &str) -> Result<MemoryType, MemoryApplicationError> {
    match x {
        "observation" => Ok(MemoryType::Observation),
        "decision" => Ok(MemoryType::Decision),
        "bugfix" => Ok(MemoryType::Bugfix),
        "review_rule" => Ok(MemoryType::ReviewRule),
        "task_note" => Ok(MemoryType::TaskNote),
        "session_summary" => Ok(MemoryType::SessionSummary),
        _ => Err(invalid("record_type", "unsupported")),
    }
}
fn wide_limits() -> AgentMemoryLimitsConfig {
    AgentMemoryLimitsConfig {
        max_title_bytes: u32::MAX,
        max_body_bytes: u32::MAX,
        max_metadata_items: u32::MAX,
        max_metadata_string_bytes: u32::MAX,
        max_batch_items: u32::MAX,
        default_results: 1,
        max_results: u32::MAX,
        default_response_bytes: 1,
        max_response_bytes: u32::MAX,
        max_query_bytes: u32::MAX,
    }
}
