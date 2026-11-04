use std::cmp::Ordering;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat};
use serde::{Deserialize, Serialize};

const CURRENT_SCHEMA_VERSION: u32 = 1;
const MAX_OPAQUE_ID_BYTES: usize = 1024;

/// Converts a required parsed input field into its strongly typed value.
///
/// Wire DTOs use this before calling domain constructors so field absence and
/// an explicitly empty value remain distinct validation failures.
pub fn require_field<T>(field: &'static str, value: Option<T>) -> Result<T, ValidationError> {
    value.ok_or(ValidationError::MissingField { field })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMemoryLimitsConfig {
    pub max_title_bytes: u32,
    pub max_body_bytes: u32,
    pub max_metadata_items: u32,
    pub max_metadata_string_bytes: u32,
    pub max_batch_items: u32,
    pub default_results: u32,
    pub max_results: u32,
    pub default_response_bytes: u32,
    pub max_response_bytes: u32,
    pub max_query_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryId(String);

impl TryFrom<String> for MemoryId {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > MAX_OPAQUE_ID_BYTES {
            return Err(ValidationError::InvalidMemoryId);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(String);

impl TryFrom<String> for Revision {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > MAX_OPAQUE_ID_BYTES {
            return Err(ValidationError::InvalidRevision);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryType {
    Observation,
    Decision,
    Bugfix,
    ReviewRule,
    TaskNote,
    SessionSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceMode {
    Durable,
    ReadOnly,
    InMemory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
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

impl Provenance {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        branch: Option<String>,
        paths: Vec<String>,
        symbols: Vec<String>,
        tags: Vec<String>,
        source_agent: Option<String>,
        source_client: Option<String>,
        commit: Option<String>,
        pull_request: Option<String>,
        issue: Option<String>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, ValidationError> {
        let paths = validate_paths(paths, limits)?;
        validate_metadata_values("symbols", &symbols, limits)?;
        validate_metadata_values("tags", &tags, limits)?;
        validate_optional_metadata("branch", branch.as_deref(), limits)?;
        validate_optional_metadata("source_agent", source_agent.as_deref(), limits)?;
        validate_optional_metadata("source_client", source_client.as_deref(), limits)?;
        validate_optional_metadata("commit", commit.as_deref(), limits)?;
        validate_optional_metadata("pull_request", pull_request.as_deref(), limits)?;
        validate_optional_metadata("issue", issue.as_deref(), limits)?;

        Ok(Self {
            branch,
            paths,
            symbols,
            tags,
            source_agent,
            source_client,
            commit,
            pull_request,
            issue,
        })
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }
    pub fn paths(&self) -> &[String] {
        &self.paths
    }
    pub fn symbols(&self) -> &[String] {
        &self.symbols
    }
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
    pub fn source_agent(&self) -> Option<&str> {
        self.source_agent.as_deref()
    }
    pub fn source_client(&self) -> Option<&str> {
        self.source_client.as_deref()
    }
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }
    pub fn pull_request(&self) -> Option<&str> {
        self.pull_request.as_deref()
    }
    pub fn issue(&self) -> Option<&str> {
        self.issue.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRecord {
    id: MemoryId,
    revision: Revision,
    schema_version: u32,
    record_type: MemoryType,
    title: String,
    body: String,
    provenance: Provenance,
    confidence: Option<f64>,
    project_key: String,
    created_at: String,
    updated_at: String,
}

impl MemoryRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: MemoryId,
        revision: Revision,
        schema_version: u32,
        record_type: MemoryType,
        title: String,
        body: String,
        provenance: Provenance,
        confidence: Option<f64>,
        project_key: String,
        created_at: String,
        updated_at: String,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, ValidationError> {
        if schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedMemoryType);
        }
        if title.is_empty() {
            return Err(ValidationError::EmptyField { field: "title" });
        }
        validate_bound("title", &title, limits.max_title_bytes)?;
        validate_bound("body", &body, limits.max_body_bytes)?;
        if project_key.is_empty() {
            return Err(ValidationError::EmptyField {
                field: "project_key",
            });
        }
        if confidence.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
            return Err(ValidationError::InvalidConfidence);
        }

        let created = parse_canonical_timestamp("created_at", &created_at)?;
        let updated = parse_canonical_timestamp("updated_at", &updated_at)?;
        if updated < created {
            return Err(ValidationError::InvalidTimestamp {
                field: "updated_at",
            });
        }

        Ok(Self {
            id,
            revision,
            schema_version,
            record_type,
            title,
            body,
            provenance,
            confidence,
            project_key,
            created_at,
            updated_at,
        })
    }

    pub fn id(&self) -> &MemoryId {
        &self.id
    }
    pub fn revision(&self) -> &Revision {
        &self.revision
    }
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
    pub fn record_type(&self) -> MemoryType {
        self.record_type
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn body(&self) -> &str {
        &self.body
    }
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }
    pub fn project_key(&self) -> &str {
        &self.project_key
    }
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}

impl MemoryId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Revision {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_bound(field: &'static str, value: &str, limit: u32) -> Result<(), ValidationError> {
    if value.len() > limit as usize {
        return Err(ValidationError::LimitExceeded { field, limit });
    }
    Ok(())
}

fn validate_metadata_values(
    field: &'static str,
    values: &[String],
    limits: &AgentMemoryLimitsConfig,
) -> Result<(), ValidationError> {
    if values.len() > limits.max_metadata_items as usize {
        return Err(ValidationError::LimitExceeded {
            field,
            limit: limits.max_metadata_items,
        });
    }
    for value in values {
        validate_bound(field, value, limits.max_metadata_string_bytes)?;
    }
    Ok(())
}

fn validate_optional_metadata(
    field: &'static str,
    value: Option<&str>,
    limits: &AgentMemoryLimitsConfig,
) -> Result<(), ValidationError> {
    if let Some(value) = value {
        validate_bound(field, value, limits.max_metadata_string_bytes)?;
    }
    Ok(())
}

fn validate_paths(
    paths: Vec<String>,
    limits: &AgentMemoryLimitsConfig,
) -> Result<Vec<String>, ValidationError> {
    if paths.len() > limits.max_metadata_items as usize {
        return Err(ValidationError::LimitExceeded {
            field: "paths",
            limit: limits.max_metadata_items,
        });
    }

    paths
        .into_iter()
        .map(|path| {
            if path.starts_with('/') {
                return Err(ValidationError::InvalidMetadataType { field: "paths" });
            }
            let mut normalized = Vec::new();
            for component in path.split('/') {
                match component {
                    "" | "." => {}
                    ".." => {
                        return Err(ValidationError::InvalidMetadataType { field: "paths" });
                    }
                    component => normalized.push(component),
                }
            }
            if normalized.is_empty() {
                return Err(ValidationError::InvalidMetadataType { field: "paths" });
            }
            let normalized = normalized.join("/");
            validate_bound("paths", &normalized, limits.max_metadata_string_bytes)?;
            Ok(normalized)
        })
        .collect()
}

fn parse_canonical_timestamp(
    field: &'static str,
    value: &str,
) -> Result<DateTime<chrono::FixedOffset>, ValidationError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| ValidationError::InvalidTimestamp { field })?;
    if parsed.offset().local_minus_utc() != 0
        || parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true) != value
    {
        return Err(ValidationError::InvalidTimestamp { field });
    }
    Ok(parsed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    MissingField { field: &'static str },
    EmptyField { field: &'static str },
    UnsupportedMemoryType,
    InvalidMetadataType { field: &'static str },
    InvalidConfidence,
    InvalidTimestamp { field: &'static str },
    LimitExceeded { field: &'static str, limit: u32 },
    InvalidMemoryId,
    InvalidRevision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetrievalError {
    InvalidArgument {
        field: &'static str,
        reason: &'static str,
    },
    CursorExpired,
    ResponseBudgetTooSmall {
        minimum_bytes: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryFilters {
    record_types: Vec<MemoryType>,
    branches: Vec<String>,
    paths: Vec<String>,
    symbols: Vec<String>,
    tags: Vec<String>,
    source_agent: Option<String>,
    source_client: Option<String>,
    created_from: Option<String>,
    created_to: Option<String>,
}

impl MemoryFilters {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        record_types: Vec<MemoryType>,
        branches: Vec<String>,
        paths: Vec<String>,
        symbols: Vec<String>,
        tags: Vec<String>,
        source_agent: Option<String>,
        source_client: Option<String>,
        created_from: Option<String>,
        created_to: Option<String>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, RetrievalError> {
        validate_filter_values("branches", &branches, limits)?;
        let paths = validate_retrieval_paths(paths, limits)?;
        validate_filter_values("symbols", &symbols, limits)?;
        validate_filter_values("tags", &tags, limits)?;
        validate_filter_optional("source_agent", source_agent.as_deref(), limits)?;
        validate_filter_optional("source_client", source_client.as_deref(), limits)?;
        let created_from = created_from
            .map(|value| normalize_retrieval_timestamp("created_from", value))
            .transpose()?;
        let created_to = created_to
            .map(|value| normalize_retrieval_timestamp("created_to", value))
            .transpose()?;
        if let (Some(from), Some(to)) = (&created_from, &created_to)
            && parse_retrieval_timestamp("created_from", from)?
                > parse_retrieval_timestamp("created_to", to)?
        {
            return Err(invalid("created_to", "before_created_from"));
        }
        if record_types.len() > limits.max_metadata_items as usize {
            return Err(invalid("record_types", "limit_exceeded"));
        }

        Ok(Self {
            record_types,
            branches,
            paths,
            symbols,
            tags,
            source_agent,
            source_client,
            created_from,
            created_to,
        })
    }

    pub(crate) fn record_types(&self) -> &[MemoryType] {
        &self.record_types
    }
    pub(crate) fn branches(&self) -> &[String] {
        &self.branches
    }
    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }
    pub(crate) fn symbols(&self) -> &[String] {
        &self.symbols
    }
    pub(crate) fn tags(&self) -> &[String] {
        &self.tags
    }
    pub(crate) fn source_agent(&self) -> Option<&str> {
        self.source_agent.as_deref()
    }
    pub(crate) fn source_client(&self) -> Option<&str> {
        self.source_client.as_deref()
    }
    pub(crate) fn created_from(&self) -> Option<&str> {
        self.created_from.as_deref()
    }
    pub(crate) fn created_to(&self) -> Option<&str> {
        self.created_to.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RankingMode {
    LexicalV1,
}

impl TryFrom<&str> for RankingMode {
    type Error = RetrievalError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "lexical_v1" => Ok(Self::LexicalV1),
            _ => Err(invalid("ranking", "unsupported")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchOrderKey {
    score: f64,
    created_at: String,
    id: MemoryId,
}

impl SearchOrderKey {
    pub fn try_new(score: f64, created_at: String, id: MemoryId) -> Result<Self, RetrievalError> {
        validate_score(score)?;
        let created_at = normalize_retrieval_timestamp("created_at", created_at)?;
        Ok(Self {
            score,
            created_at,
            id,
        })
    }
}

impl Eq for SearchOrderKey {}

impl Ord for SearchOrderKey {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| {
                parse_valid_timestamp(&other.created_at)
                    .cmp(&parse_valid_timestamp(&self.created_at))
            })
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for SearchOrderKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineOrderKey {
    created_at: String,
    id: MemoryId,
}

impl Ord for TimelineOrderKey {
    fn cmp(&self, other: &Self) -> Ordering {
        parse_valid_timestamp(&self.created_at)
            .cmp(&parse_valid_timestamp(&other.created_at))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for TimelineOrderKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl TimelineOrderKey {
    pub fn try_new(created_at: String, id: MemoryId) -> Result<Self, RetrievalError> {
        let created_at = normalize_retrieval_timestamp("created_at", created_at)?;
        Ok(Self { created_at, id })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseBudget {
    limit: u32,
    response_bytes: u32,
}

impl ResponseBudget {
    pub fn try_new(
        limit: u32,
        response_bytes: u32,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, RetrievalError> {
        if limit == 0 || limit > limits.max_results {
            return Err(invalid("limit", "out_of_range"));
        }
        if response_bytes == 0 || response_bytes > limits.max_response_bytes {
            return Err(invalid("response_bytes", "out_of_range"));
        }
        Ok(Self {
            limit,
            response_bytes,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchReasonKind {
    Lexical,
    Metadata,
    Semantic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchReasonField {
    Title,
    Body,
    Branch,
    Path,
    Symbol,
    Tag,
    SourceAgent,
    SourceClient,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchReason {
    kind: MatchReasonKind,
    field: MatchReasonField,
}

impl MatchReason {
    pub fn new(kind: MatchReasonKind, field: MatchReasonField) -> Self {
        Self { kind, field }
    }

    pub(crate) fn kind(&self) -> &MatchReasonKind {
        &self.kind
    }

    pub(crate) fn field(&self) -> &MatchReasonField {
        &self.field
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactHit {
    id: MemoryId,
    revision: Revision,
    record_type: MemoryType,
    title: String,
    paths: Vec<String>,
    timestamp: String,
    score: f64,
    reasons: Vec<MatchReason>,
    truncated_fields: Vec<String>,
}

impl CompactHit {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: MemoryId,
        revision: Revision,
        record_type: MemoryType,
        title: String,
        paths: Vec<String>,
        timestamp: String,
        score: f64,
        reasons: Vec<MatchReason>,
        truncated_fields: Vec<String>,
        limits: &AgentMemoryLimitsConfig,
    ) -> Result<Self, RetrievalError> {
        if title.is_empty() {
            return Err(invalid("title", "empty"));
        }
        validate_score(score)?;
        let timestamp = normalize_retrieval_timestamp("timestamp", timestamp)?;
        if paths.len() > limits.max_metadata_items as usize {
            return Err(invalid("paths", "limit_exceeded"));
        }
        let mut truncated_fields = Truncation::try_new(truncated_fields, 0)?.truncated_fields;
        let (title, title_truncated) = truncate_utf8(title, limits.max_title_bytes as usize);
        if title.is_empty() {
            return Err(invalid("title", "empty"));
        }
        let mut any_path_truncated = false;
        let paths = paths
            .into_iter()
            .map(|path| {
                validate_retrieval_path(&path)?;
                let path = path
                    .split('/')
                    .filter(|part| !part.is_empty() && *part != ".")
                    .collect::<Vec<_>>()
                    .join("/");
                let (path, truncated) =
                    truncate_path_utf8(path, limits.max_metadata_string_bytes as usize);
                any_path_truncated |= truncated;
                if path.len() > limits.max_metadata_string_bytes as usize {
                    return Err(invalid("paths", "limit_exceeded"));
                }
                Ok(path)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if title_truncated {
            insert_truncated_field(&mut truncated_fields, "title");
        }
        if any_path_truncated {
            insert_truncated_field(&mut truncated_fields, "paths");
        }
        Ok(Self {
            id,
            revision,
            record_type,
            title,
            paths,
            timestamp,
            score,
            reasons,
            truncated_fields,
        })
    }

    pub(crate) fn id(&self) -> &MemoryId {
        &self.id
    }

    pub(crate) fn revision(&self) -> &Revision {
        &self.revision
    }

    pub(crate) fn record_type(&self) -> MemoryType {
        self.record_type
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }

    pub(crate) fn timestamp(&self) -> &str {
        &self.timestamp
    }

    pub(crate) fn score(&self) -> f64 {
        self.score
    }

    pub(crate) fn reasons(&self) -> &[MatchReason] {
        &self.reasons
    }

    pub(crate) fn truncated_fields(&self) -> &[String] {
        &self.truncated_fields
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineEvent {
    hit: CompactHit,
}

impl TimelineEvent {
    pub fn try_new(hit: CompactHit) -> Result<Self, RetrievalError> {
        Ok(Self { hit })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskContext {
    hits: Vec<CompactHit>,
    applied_filters: MemoryFilters,
    next_cursor: Option<ContinuationCursor>,
    budget: ResponseBudget,
}

impl TaskContext {
    pub fn try_new(
        hits: Vec<CompactHit>,
        applied_filters: MemoryFilters,
        next_cursor: Option<ContinuationCursor>,
        budget: ResponseBudget,
    ) -> Result<Self, RetrievalError> {
        if hits.len() > budget.limit as usize {
            return Err(invalid("hits", "limit_exceeded"));
        }
        let mut keyed = hits
            .into_iter()
            .map(|hit| {
                SearchOrderKey::try_new(hit.score, hit.timestamp.clone(), hit.id.clone())
                    .map(|key| (key, hit))
            })
            .collect::<Result<Vec<_>, _>>()?;
        keyed.sort_by(|(left, _), (right, _)| left.cmp(right));
        let mut hits = keyed.into_iter().map(|(_, hit)| hit).collect::<Vec<_>>();
        let empty_envelope_bytes =
            serialized_context_len(&[], &applied_filters, next_cursor.as_ref())?;
        if hits.is_empty() && empty_envelope_bytes > budget.response_bytes as usize {
            return Err(RetrievalError::ResponseBudgetTooSmall {
                minimum_bytes: empty_envelope_bytes,
            });
        }
        if !hits.is_empty() {
            let first_hit_bytes =
                serialized_context_len(&hits[..1], &applied_filters, next_cursor.as_ref())?;
            if first_hit_bytes > budget.response_bytes as usize {
                return Err(RetrievalError::ResponseBudgetTooSmall {
                    minimum_bytes: first_hit_bytes,
                });
            }
            while serialized_context_len(&hits, &applied_filters, next_cursor.as_ref())?
                > budget.response_bytes as usize
            {
                hits.pop();
            }
        }
        Ok(Self {
            hits,
            applied_filters,
            next_cursor,
            budget,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContinuationCursor {
    encoded: String,
    expected_request_digest: [u8; 32],
    expected_generation: u64,
}

impl ContinuationCursor {
    pub fn try_new(
        encoded: &str,
        expected_request_digest: [u8; 32],
        expected_generation: u64,
    ) -> Result<Self, RetrievalError> {
        if encoded.is_empty() || encoded.len() > MAX_OPAQUE_ID_BYTES {
            return Err(RetrievalError::CursorExpired);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| RetrievalError::CursorExpired)?;
        let wire: CursorWire =
            serde_json::from_slice(&decoded).map_err(|_| RetrievalError::CursorExpired)?;
        let canonical = serde_json::to_vec(&wire).map_err(|_| RetrievalError::CursorExpired)?;
        if canonical != decoded
            || wire.v != 1
            || wire.offset == 0
            || wire.request_digest != hex_digest(expected_request_digest)
            || wire.generation != expected_generation
            || URL_SAFE_NO_PAD.encode(&canonical) != encoded
        {
            return Err(RetrievalError::CursorExpired);
        }
        Ok(Self {
            encoded: encoded.to_owned(),
            expected_request_digest,
            expected_generation,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncation {
    truncated_fields: Vec<String>,
    omitted_hits: u64,
}

impl Truncation {
    pub fn try_new(
        truncated_fields: Vec<String>,
        omitted_hits: u64,
    ) -> Result<Self, RetrievalError> {
        if truncated_fields
            .iter()
            .any(|field| !matches!(field.as_str(), "title" | "paths" | "reasons"))
        {
            return Err(invalid("truncated_fields", "unsupported_field"));
        }
        let mut unique = Vec::new();
        for field in truncated_fields {
            if !unique.contains(&field) {
                unique.push(field);
            }
        }
        Ok(Self {
            truncated_fields: unique,
            omitted_hits,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorWire {
    v: u8,
    request_digest: String,
    generation: u64,
    offset: u64,
}

fn invalid(field: &'static str, reason: &'static str) -> RetrievalError {
    RetrievalError::InvalidArgument { field, reason }
}

fn validate_score(score: f64) -> Result<(), RetrievalError> {
    if !score.is_finite() {
        return Err(invalid("score", "not_finite"));
    }
    if score < 0.0 {
        return Err(invalid("score", "negative"));
    }
    Ok(())
}

fn parse_retrieval_timestamp(
    field: &'static str,
    value: &str,
) -> Result<DateTime<chrono::FixedOffset>, RetrievalError> {
    DateTime::parse_from_rfc3339(value).map_err(|_| invalid(field, "invalid_timestamp"))
}

fn parse_valid_timestamp(value: &str) -> DateTime<chrono::FixedOffset> {
    DateTime::parse_from_rfc3339(value).expect("validated timestamp")
}

fn normalize_retrieval_timestamp(
    field: &'static str,
    value: String,
) -> Result<String, RetrievalError> {
    let parsed = parse_retrieval_timestamp(field, &value)?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(invalid(field, "not_utc"));
    }
    Ok(parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn validate_filter_values(
    field: &'static str,
    values: &[String],
    limits: &AgentMemoryLimitsConfig,
) -> Result<(), RetrievalError> {
    if values.len() > limits.max_metadata_items as usize {
        return Err(invalid(field, "limit_exceeded"));
    }
    if values
        .iter()
        .any(|value| value.len() > limits.max_metadata_string_bytes as usize)
    {
        return Err(invalid(field, "limit_exceeded"));
    }
    Ok(())
}

fn validate_filter_optional(
    field: &'static str,
    value: Option<&str>,
    limits: &AgentMemoryLimitsConfig,
) -> Result<(), RetrievalError> {
    if value.is_some_and(|value| value.len() > limits.max_metadata_string_bytes as usize) {
        return Err(invalid(field, "limit_exceeded"));
    }
    Ok(())
}

fn validate_retrieval_paths(
    paths: Vec<String>,
    limits: &AgentMemoryLimitsConfig,
) -> Result<Vec<String>, RetrievalError> {
    validate_filter_values("paths", &paths, limits)?;
    paths
        .into_iter()
        .map(|path| {
            validate_retrieval_path(&path)?;
            Ok(path
                .split('/')
                .filter(|part| !part.is_empty() && *part != ".")
                .collect::<Vec<_>>()
                .join("/"))
        })
        .collect()
}

fn validate_retrieval_path(path: &str) -> Result<(), RetrievalError> {
    if path.starts_with('/')
        || path.split('/').any(|part| part == "..")
        || path.split('/').all(|part| part.is_empty() || part == ".")
    {
        return Err(invalid("paths", "invalid_path"));
    }
    Ok(())
}

fn truncate_utf8(mut value: String, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

fn truncate_path_utf8(value: String, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let extension_start = value.rfind('.').unwrap_or(value.len());
    let extension = &value[extension_start..];
    if extension.len() > limit {
        return truncate_utf8(value, limit);
    }
    let stem_limit = limit.saturating_sub(extension.len());
    let (mut stem, _) = truncate_utf8(value[..extension_start].to_owned(), stem_limit);
    stem.push_str(extension);
    (stem, true)
}

fn insert_truncated_field(fields: &mut Vec<String>, field: &str) {
    if !fields.iter().any(|candidate| candidate == field) {
        fields.push(field.to_owned());
    }
}

fn hex_digest(digest: [u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn serialized_context_len(
    hits: &[CompactHit],
    filters: &MemoryFilters,
    cursor: Option<&ContinuationCursor>,
) -> Result<usize, RetrievalError> {
    let value = serde_json::json!({
        "code": "ok",
        // Use the longest valid common-field encodings so this domain-level budget remains safe
        // for every persistence mode selected later by the application layer.
        "persistence_mode": "in_memory",
        "durable_data_unavailable": false,
        "hits": hits.iter().map(compact_hit_json).collect::<Vec<_>>(),
        "applied_filters": filters_json(filters),
        "next_cursor": cursor.map(|cursor| cursor.encoded.as_str()),
    });
    serde_json::to_vec(&value)
        .map(|bytes| bytes.len())
        .map_err(|_| invalid("response", "serialization_failed"))
}

pub(crate) fn compact_hit_json(hit: &CompactHit) -> serde_json::Value {
    serde_json::json!({
        "id": hit.id().as_str(), "revision": hit.revision().as_str(), "record_type": memory_type_name(hit.record_type()),
        "title": hit.title(), "paths": hit.paths(), "timestamp": hit.timestamp(), "score": hit.score(),
        "reasons": hit.reasons().iter().map(|reason| serde_json::json!({"kind": reason_kind_name(reason.kind()), "field": reason_field_name(reason.field())})).collect::<Vec<_>>(),
        "truncated_fields": hit.truncated_fields(),
    })
}

pub(crate) fn filters_json(filters: &MemoryFilters) -> serde_json::Value {
    serde_json::json!({
        "record_types": filters.record_types.iter().map(|kind| memory_type_name(*kind)).collect::<Vec<_>>(),
        "branches": filters.branches, "paths": filters.paths, "symbols": filters.symbols, "tags": filters.tags,
        "source_agent": filters.source_agent, "source_client": filters.source_client,
        "created_from": filters.created_from, "created_to": filters.created_to,
    })
}

fn memory_type_name(kind: MemoryType) -> &'static str {
    match kind {
        MemoryType::Observation => "observation",
        MemoryType::Decision => "decision",
        MemoryType::Bugfix => "bugfix",
        MemoryType::ReviewRule => "review_rule",
        MemoryType::TaskNote => "task_note",
        MemoryType::SessionSummary => "session_summary",
    }
}
fn reason_kind_name(kind: &MatchReasonKind) -> &'static str {
    match kind {
        MatchReasonKind::Lexical => "lexical",
        MatchReasonKind::Metadata => "metadata",
        MatchReasonKind::Semantic => "semantic",
    }
}
fn reason_field_name(field: &MatchReasonField) -> &'static str {
    match field {
        MatchReasonField::Title => "title",
        MatchReasonField::Body => "body",
        MatchReasonField::Branch => "branch",
        MatchReasonField::Path => "path",
        MatchReasonField::Symbol => "symbol",
        MatchReasonField::Tag => "tag",
        MatchReasonField::SourceAgent => "source_agent",
        MatchReasonField::SourceClient => "source_client",
    }
}
