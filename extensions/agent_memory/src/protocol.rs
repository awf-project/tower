use extension_protocol::{Capability, InitResult, ToolDecl};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    AgentMemoryLimitsConfig, BatchMutationResult, Clock, EnrichmentProvider, ForgetCommand,
    GetItemResult as AppGetItem, GetQuery, IdGenerator, MemoryApplication, MemoryApplicationError,
    MemoryId, MemoryStore, MemoryType, MutationOutcomeCode, PersistenceMode, Provenance,
    RecordInput, RememberCommand, Revision, UpdateCommand, UpdateItem,
};

pub const TOWER_MEMORY_REMEMBER: &str = "tower_memory_remember";
pub const TOWER_MEMORY_UPDATE: &str = "tower_memory_update";
pub const TOWER_MEMORY_FORGET: &str = "tower_memory_forget";
pub const TOWER_MEMORY_SEARCH: &str = "tower_memory_search";
pub const TOWER_MEMORY_TIMELINE: &str = "tower_memory_timeline";
pub const TOWER_MEMORY_GET: &str = "tower_memory_get";
pub const TOWER_MEMORY_CONTEXT_FOR_TASK: &str = "tower_memory_context_for_task";

const REMEMBER: &str = "memory_remember";
const UPDATE: &str = "memory_update";
const FORGET: &str = "memory_forget";
const SEARCH: &str = "memory_search";
const TIMELINE: &str = "memory_timeline";
const GET: &str = "memory_get";
const CONTEXT: &str = "memory_context_for_task";

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecordTypeDto {
    Observation,
    Decision,
    Bugfix,
    ReviewRule,
    TaskNote,
    SessionSummary,
}
impl From<RecordTypeDto> for MemoryType {
    fn from(v: RecordTypeDto) -> Self {
        match v {
            RecordTypeDto::Observation => Self::Observation,
            RecordTypeDto::Decision => Self::Decision,
            RecordTypeDto::Bugfix => Self::Bugfix,
            RecordTypeDto::ReviewRule => Self::ReviewRule,
            RecordTypeDto::TaskNote => Self::TaskNote,
            RecordTypeDto::SessionSummary => Self::SessionSummary,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceDto {
    branch: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    source_agent: Option<String>,
    source_client: Option<String>,
    commit: Option<String>,
    pull_request: Option<String>,
    issue: Option<String>,
}
impl ProvenanceDto {
    fn domain(self, l: &AgentMemoryLimitsConfig) -> Result<Provenance, ()> {
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
            l,
        )
        .map_err(|_| ())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordInputDto {
    record_type: RecordTypeDto,
    title: String,
    body: String,
    provenance: ProvenanceDto,
    confidence: Option<f64>,
}
impl RecordInputDto {
    fn domain(self, l: &AgentMemoryLimitsConfig) -> Result<RecordInput, ()> {
        let p = self.provenance.domain(l)?;
        RecordInput::try_new(
            self.record_type.into(),
            self.title,
            self.body,
            p,
            self.confidence,
            l,
        )
        .map_err(|_| ())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FiltersDto {
    #[serde(default)]
    record_types: Vec<RecordTypeDto>,
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
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PageDto {
    limit: Option<u32>,
    response_bytes: Option<u32>,
    cursor: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TimelineAnchorDto {
    Record { id: String, before: u32, after: u32 },
    Query { query: String },
    Window { from: String, to: String },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdateItemDto {
    id: String,
    expected_revision: String,
    record: RecordInputDto,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionItemDto {
    id: String,
    expected_revision: String,
}

macro_rules! dto { ($n:ident{$($f:ident:$t:ty),*$(,)?}) => { #[derive(Clone,Debug,Deserialize,Serialize)] #[serde(deny_unknown_fields)] pub struct $n { $( $f:$t ),* } }; }
dto!(RememberParams {
    record: RecordInputDto,
    idempotency_key: String
});
dto!(UpdateParams{items:Vec<UpdateItemDto>});
dto!(ForgetParams{items:Vec<RevisionItemDto>});
dto!(GetParams{ids:Vec<String>});

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    query: String,
    #[serde(default)]
    filters: FiltersDto,
    #[serde(default)]
    page: PageDto,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineParams {
    anchor: TimelineAnchorDto,
    #[serde(default)]
    filters: FiltersDto,
    #[serde(default)]
    page: PageDto,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextForTaskParams {
    query: String,
    #[serde(default)]
    filters: FiltersDto,
    #[serde(default)]
    page: PageDto,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceModeDto {
    Durable,
    ReadOnly,
    InMemory,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StableFailureCodeDto {
    NotFound,
    DuplicateId,
    RevisionConflict,
    IdempotencyConflict,
    RecordForgotten,
    ReadOnly,
    StorageUnavailable,
    CommitOutcomeUnknown,
    MigrationFailed,
    IncompatibleSchema,
    CursorExpired,
    ResponseBudgetTooSmall,
    PrivacyRejected,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum MutationItemDto {
    Ok {
        id: String,
        code: OkCodeDto,
        revision: String,
        durable: bool,
    },
    Failure {
        id: String,
        code: StableFailureCodeDto,
        #[serde(
            deserialize_with = "false_only",
            serialize_with = "serialize_false_only"
        )]
        durable: bool,
    },
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKindDto {
    Lexical,
    Metadata,
    Semantic,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonFieldDto {
    Title,
    Body,
    Branch,
    Path,
    Symbol,
    Tag,
    SourceAgent,
    SourceClient,
}
dto!(ReasonDto {
    kind: ReasonKindDto,
    field: ReasonFieldDto
});
dto!(HitDto{id:String,revision:String,record_type:RecordTypeDto,title:String,paths:Vec<String>,timestamp:String,score:f64,reasons:Vec<ReasonDto>,truncated_fields:Vec<String>});
dto!(MemoryRecordDto{id:String,revision:String,schema_version:u32,record_type:RecordTypeDto,title:String,body:String,provenance:ProvenanceDto,confidence:Option<f64>,project_key:String,created_at:String,updated_at:String});
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OkCodeDto {
    Ok,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum GetItemDto {
    Ok {
        id: String,
        code: OkCodeDto,
        record: Box<MemoryRecordDto>,
    },
    NotFound {
        id: String,
        code: NotFoundCodeDto,
    },
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum NotFoundCodeDto {
    #[serde(rename = "not_found")]
    NotFound,
}

fn false_only<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = bool::deserialize(deserializer)?;
    if value {
        Err(serde::de::Error::custom(
            "durable must be false for failures",
        ))
    } else {
        Ok(false)
    }
}

fn serialize_false_only<S: serde::Serializer>(
    value: &bool,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if *value {
        Err(serde::ser::Error::custom(
            "durable must be false for failures",
        ))
    } else {
        serializer.serialize_bool(false)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum RememberResult {
    Ok {
        code: OkCodeDto,
        id: String,
        revision: String,
        persistence_mode: PersistenceModeDto,
        durable_data_unavailable: bool,
        durable: bool,
    },
    Failure {
        code: StableFailureCodeDto,
        persistence_mode: PersistenceModeDto,
        durable_data_unavailable: bool,
        #[serde(
            deserialize_with = "false_only",
            serialize_with = "serialize_false_only"
        )]
        durable: bool,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum UpdateResult {
    Ok {
        code: OkCodeDto,
        items: Vec<MutationItemDto>,
        persistence_mode: PersistenceModeDto,
        durable_data_unavailable: bool,
        durable: bool,
    },
    Failure {
        code: StableFailureCodeDto,
        persistence_mode: PersistenceModeDto,
        durable_data_unavailable: bool,
        #[serde(
            deserialize_with = "false_only",
            serialize_with = "serialize_false_only"
        )]
        durable: bool,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ForgetResult {
    Ok {
        code: OkCodeDto,
        items: Vec<MutationItemDto>,
        persistence_mode: PersistenceModeDto,
        durable_data_unavailable: bool,
        durable: bool,
    },
    Failure {
        code: StableFailureCodeDto,
        persistence_mode: PersistenceModeDto,
        durable_data_unavailable: bool,
        #[serde(
            deserialize_with = "false_only",
            serialize_with = "serialize_false_only"
        )]
        durable: bool,
    },
}
macro_rules! retrieval_result {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Deserialize, Serialize)]
        #[serde(untagged, deny_unknown_fields)]
        pub enum $name {
            Ok { code: OkCodeDto, $($field: $ty,)* persistence_mode: PersistenceModeDto, durable_data_unavailable: bool },
            Failure { code: StableFailureCodeDto, persistence_mode: PersistenceModeDto, durable_data_unavailable: bool },
        }
    };
}
retrieval_result!(SearchResult { hits: Vec<HitDto>, next_cursor: Option<String> });
retrieval_result!(TimelineResult { events: Vec<HitDto>, next_cursor: Option<String> });
retrieval_result!(GetResult { items: Vec<GetItemDto> });
retrieval_result!(ContextForTaskResult { hits: Vec<HitDto>, applied_filters: Box<FiltersDto>, next_cursor: Option<String> });

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolProtocolError {
    InvalidParams,
    UnknownTool,
}

pub fn tool_declarations() -> Vec<ToolDecl> {
    let provenance = json!({"type":"object","properties":{"branch":{"type":["string","null"]},"paths":{"type":"array","items":{"type":"string"},"default":[]},"symbols":{"type":"array","items":{"type":"string"},"default":[]},"tags":{"type":"array","items":{"type":"string"},"default":[]},"source_agent":{"type":["string","null"]},"source_client":{"type":["string","null"]},"commit":{"type":["string","null"]},"pull_request":{"type":["string","null"]},"issue":{"type":["string","null"]}},"additionalProperties":false});
    let record = json!({"type":"object","properties":{"record_type":{"enum":["observation","decision","bugfix","review_rule","task_note","session_summary"]},"title":{"type":"string"},"body":{"type":"string"},"provenance":provenance,"confidence":{"type":["number","null"],"minimum":0,"maximum":1}},"required":["record_type","title","body","provenance"],"additionalProperties":false});
    let filters = json!({"type":"object","properties":{"record_types":{"type":"array","items":{"enum":["observation","decision","bugfix","review_rule","task_note","session_summary"]},"default":[]},"branches":{"type":"array","items":{"type":"string"},"default":[]},"paths":{"type":"array","items":{"type":"string"},"default":[]},"symbols":{"type":"array","items":{"type":"string"},"default":[]},"tags":{"type":"array","items":{"type":"string"},"default":[]},"source_agent":{"type":["string","null"]},"source_client":{"type":["string","null"]},"created_from":{"type":["string","null"],"format":"date-time"},"created_to":{"type":["string","null"],"format":"date-time"}},"additionalProperties":false});
    let page = json!({"type":"object","properties":{"limit":{"type":["integer","null"],"minimum":1},"response_bytes":{"type":["integer","null"],"minimum":1},"cursor":{"type":["string","null"]}},"additionalProperties":false});
    let rev = json!({"type":"object","properties":{"id":{"type":"string"},"expected_revision":{"type":"string"}},"required":["id","expected_revision"],"additionalProperties":false});
    let upd = json!({"type":"object","properties":{"id":{"type":"string"},"expected_revision":{"type":"string"},"record":record.clone()},"required":["id","expected_revision","record"],"additionalProperties":false});
    let anchor = json!({"oneOf":[{"type":"object","properties":{"kind":{"const":"record"},"id":{"type":"string"},"before":{"type":"integer","minimum":0},"after":{"type":"integer","minimum":0}},"required":["kind","id","before","after"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"query"},"query":{"type":"string"}},"required":["kind","query"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"window"},"from":{"type":"string","format":"date-time"},"to":{"type":"string","format":"date-time"}},"required":["kind","from","to"],"additionalProperties":false}]});
    let rows = [
        (
            REMEMBER,
            "Store one durable memory record. Returns code, id, revision, persistence_mode, durable_data_unavailable, and durable; durable is true only after durable acknowledgement. Privacy-rule changes are non-retroactive and external backups/providers may retain prior data.",
            json!({"type":"object","properties":{"record":record.clone(),"idempotency_key":{"type":"string"}},"required":["record","idempotency_key"],"additionalProperties":false}),
        ),
        (
            UPDATE,
            "Update memory records independently. Returns ordered item outcomes plus persistence_mode, durable_data_unavailable, and durable; durable is true only after every successful item is durably acknowledged.",
            json!({"type":"object","properties":{"items":{"type":"array","minItems":1,"items":upd}},"required":["items"],"additionalProperties":false}),
        ),
        (
            FORGET,
            "Logically delete memory records; deletion is not forensic erasure and external backups/providers may retain data. Returns ordered item outcomes and durability metadata.",
            json!({"type":"object","properties":{"items":{"type":"array","minItems":1,"items":rev}},"required":["items"],"additionalProperties":false}),
        ),
        (
            SEARCH,
            "Search compact hits, then use memory_get for batched full-record retrieval. Compact hits omit bodies. Returns hits, next_cursor, persistence_mode, and durable_data_unavailable.",
            json!({"type":"object","properties":{"query":{"type":"string"},"filters":filters.clone(),"page":page.clone()},"required":["query"],"additionalProperties":false}),
        ),
        (
            TIMELINE,
            "Read a bounded half-open memory timeline. Returns compact events, next_cursor, persistence_mode, and durable_data_unavailable.",
            json!({"type":"object","properties":{"anchor":anchor,"filters":filters.clone(),"page":page.clone()},"required":["anchor"],"additionalProperties":false}),
        ),
        (
            GET,
            "Batch-retrieve full records by ID after compact search. Returns ordered item outcomes, persistence_mode, and durable_data_unavailable.",
            json!({"type":"object","properties":{"ids":{"type":"array","minItems":1,"items":{"type":"string"}}},"required":["ids"],"additionalProperties":false}),
        ),
        (
            CONTEXT,
            "Search compact task context without unrelated budget fillers. Returns hits, applied_filters, next_cursor, persistence_mode, and durable_data_unavailable; provider retention limits and non-retroactive privacy changes apply.",
            json!({"type":"object","properties":{"query":{"type":"string"},"filters":filters,"page":page},"required":["query"],"additionalProperties":false}),
        ),
    ];
    rows.into_iter()
        .map(|(name, description, schema)| ToolDecl {
            name: name.into(),
            description: description.into(),
            schema_json: schema.to_string(),
        })
        .collect()
}
pub fn initialize_result() -> InitResult {
    InitResult {
        tools: tool_declarations(),
        events: vec![],
        capabilities: vec![Capability::AgentMemoryStorage],
    }
}

fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, ToolProtocolError> {
    serde_json::from_value(v).map_err(|_| ToolProtocolError::InvalidParams)
}
pub fn validate_tool_arguments(name: &str, args: Value) -> Result<(), ToolProtocolError> {
    match name {
        REMEMBER => parse::<RememberParams>(args).map(drop),
        UPDATE => parse::<UpdateParams>(args).map(drop),
        FORGET => parse::<ForgetParams>(args).map(drop),
        SEARCH => parse::<SearchParams>(args).map(drop),
        TIMELINE => parse::<TimelineParams>(args).map(drop),
        GET => parse::<GetParams>(args).map(drop),
        CONTEXT => parse::<ContextForTaskParams>(args).map(drop),
        _ => Err(ToolProtocolError::UnknownTool),
    }
}

pub fn handle_tool<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>(
    name: &str,
    args: Value,
    app: &MemoryApplication<S, C, I, P>,
) -> Result<Value, ToolProtocolError> {
    match name {
        REMEMBER => remember(parse(args)?, app),
        UPDATE => update(parse(args)?, app),
        FORGET => forget(parse(args)?, app),
        GET => get(parse(args)?, app),
        SEARCH | TIMELINE | CONTEXT => {
            validate_tool_arguments(name, args.clone())?;
            app.dispatch_retrieval(name, args).map_err(|e| match e {
                crate::RetrievalDispatchError::InvalidParams => ToolProtocolError::InvalidParams,
                crate::RetrievalDispatchError::UnknownTool => ToolProtocolError::UnknownTool,
            })
        }
        _ => Err(ToolProtocolError::UnknownTool),
    }
}
fn remember<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>(
    p: RememberParams,
    a: &MemoryApplication<S, C, I, P>,
) -> Result<Value, ToolProtocolError> {
    let r = p
        .record
        .domain(a.limits())
        .map_err(|_| ToolProtocolError::InvalidParams)?;
    let c = RememberCommand::try_new(r, p.idempotency_key, a.limits())
        .map_err(|_| ToolProtocolError::InvalidParams)?;
    match a.remember(c) {
        Ok(x) => Ok(
            json!({"code":"ok","id":x.id.as_str(),"revision":x.revision.as_str(),"persistence_mode":mode(x.persistence_mode),"durable_data_unavailable":x.durable_data_unavailable,"durable":x.durable}),
        ),
        Err(e) => failure(e, a, true),
    }
}
fn update<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>(
    p: UpdateParams,
    a: &MemoryApplication<S, C, I, P>,
) -> Result<Value, ToolProtocolError> {
    let xs = p
        .items
        .into_iter()
        .map(|x| {
            Ok(UpdateItem {
                id: MemoryId::try_from(x.id).map_err(|_| ToolProtocolError::InvalidParams)?,
                expected_revision: Revision::try_from(x.expected_revision)
                    .map_err(|_| ToolProtocolError::InvalidParams)?,
                record: x
                    .record
                    .domain(a.limits())
                    .map_err(|_| ToolProtocolError::InvalidParams)?,
            })
        })
        .collect::<Result<Vec<_>, ToolProtocolError>>()?;
    let c = UpdateCommand::try_new(xs, a.limits()).map_err(|_| ToolProtocolError::InvalidParams)?;
    match a.update(c) {
        Ok(x) => Ok(batch(x)),
        Err(e) => failure(e, a, true),
    }
}
fn forget<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>(
    p: ForgetParams,
    a: &MemoryApplication<S, C, I, P>,
) -> Result<Value, ToolProtocolError> {
    let xs = p
        .items
        .into_iter()
        .map(|x| {
            Ok((
                MemoryId::try_from(x.id).map_err(|_| ToolProtocolError::InvalidParams)?,
                Revision::try_from(x.expected_revision)
                    .map_err(|_| ToolProtocolError::InvalidParams)?,
            ))
        })
        .collect::<Result<Vec<_>, ToolProtocolError>>()?;
    let c = ForgetCommand::try_new(xs, a.limits()).map_err(|_| ToolProtocolError::InvalidParams)?;
    match a.forget(c) {
        Ok(x) => Ok(batch(x)),
        Err(e) => failure(e, a, true),
    }
}
fn get<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>(
    p: GetParams,
    a: &MemoryApplication<S, C, I, P>,
) -> Result<Value, ToolProtocolError> {
    let ids = p
        .ids
        .into_iter()
        .map(|x| MemoryId::try_from(x).map_err(|_| ToolProtocolError::InvalidParams))
        .collect::<Result<Vec<_>, _>>()?;
    let q = GetQuery::try_new(ids, a.limits()).map_err(|_| ToolProtocolError::InvalidParams)?;
    match a.get(q) {
        Ok(x) => Ok(
            json!({"code":"ok","items":x.items.iter().map(get_item).collect::<Vec<_>>(),"persistence_mode":mode(x.persistence_mode),"durable_data_unavailable":x.durable_data_unavailable}),
        ),
        Err(e) => failure(e, a, false),
    }
}

fn failure<S: MemoryStore, C: Clock, I: IdGenerator, P: EnrichmentProvider>(
    e: MemoryApplicationError,
    a: &MemoryApplication<S, C, I, P>,
    mutation: bool,
) -> Result<Value, ToolProtocolError> {
    map_application_error(e, a.persistence_status(), mutation)
}

pub fn map_application_error(
    e: MemoryApplicationError,
    persistence_status: (PersistenceMode, bool),
    mutation: bool,
) -> Result<Value, ToolProtocolError> {
    let code = match e {
        MemoryApplicationError::InvalidArgument {
            reason: "idempotency_conflict",
            ..
        } => "idempotency_conflict",
        MemoryApplicationError::InvalidArgument {
            reason: "record_forgotten",
            ..
        } => "record_forgotten",
        MemoryApplicationError::InvalidArgument {
            reason: "privacy_rejected" | "privacy_redaction_failed",
            ..
        } => "privacy_rejected",
        MemoryApplicationError::InvalidArgument { .. } => {
            return Err(ToolProtocolError::InvalidParams);
        }
        MemoryApplicationError::StorageUnavailable
        | MemoryApplicationError::ProviderTimeout
        | MemoryApplicationError::ProviderCancelled
        | MemoryApplicationError::ProviderUnavailable => "storage_unavailable",
        MemoryApplicationError::ReadOnly => "read_only",
        MemoryApplicationError::CommitOutcomeUnknown => "commit_outcome_unknown",
        MemoryApplicationError::Compatibility { .. } => "incompatible_schema",
        MemoryApplicationError::Migration { .. } => "migration_failed",
        MemoryApplicationError::CursorExpired => "cursor_expired",
        MemoryApplicationError::ResponseBudgetTooSmall { .. } => "response_budget_too_small",
    };
    let (m, d) = persistence_status;
    let mut v = json!({"code":code,"persistence_mode":mode(m),"durable_data_unavailable":d});
    if mutation {
        v["durable"] = json!(false)
    }
    Ok(v)
}
fn batch(x: BatchMutationResult) -> Value {
    json!({"code":"ok","items":x.items.iter().map(|i|{let mut v=json!({"id":i.id.as_str(),"code":mutation_code(i.code),"durable":i.durable});if let Some(r)=&i.revision{v["revision"]=json!(r.as_str())}v}).collect::<Vec<_>>(),"persistence_mode":mode(x.persistence_mode),"durable_data_unavailable":x.durable_data_unavailable,"durable":x.durable})
}
fn get_item(x: &AppGetItem) -> Value {
    match x {
        AppGetItem::NotFound { id } => json!({"id":id.as_str(),"code":"not_found"}),
        AppGetItem::Found { id, record: r } => {
            json!({"id":id.as_str(),"code":"ok","record":{"id":r.id().as_str(),"revision":r.revision().as_str(),"schema_version":r.schema_version(),"record_type":record_type(r.record_type()),"title":r.title(),"body":r.body(),"provenance":{"branch":r.provenance().branch(),"paths":r.provenance().paths(),"symbols":r.provenance().symbols(),"tags":r.provenance().tags(),"source_agent":r.provenance().source_agent(),"source_client":r.provenance().source_client(),"commit":r.provenance().commit(),"pull_request":r.provenance().pull_request(),"issue":r.provenance().issue()},"confidence":r.confidence(),"project_key":r.project_key(),"created_at":r.created_at(),"updated_at":r.updated_at()}})
        }
    }
}
fn mutation_code(x: MutationOutcomeCode) -> &'static str {
    match x {
        MutationOutcomeCode::Updated | MutationOutcomeCode::Forgotten => "ok",
        MutationOutcomeCode::NotFound => "not_found",
        MutationOutcomeCode::DuplicateId => "duplicate_id",
        MutationOutcomeCode::RevisionConflict => "revision_conflict",
        MutationOutcomeCode::ReadOnly => "read_only",
        MutationOutcomeCode::IdempotencyConflict => "idempotency_conflict",
        MutationOutcomeCode::RecordForgotten => "record_forgotten",
    }
}
fn mode(x: PersistenceMode) -> &'static str {
    match x {
        PersistenceMode::Durable => "durable",
        PersistenceMode::ReadOnly => "read_only",
        PersistenceMode::InMemory => "in_memory",
    }
}
fn record_type(x: MemoryType) -> &'static str {
    match x {
        MemoryType::Observation => "observation",
        MemoryType::Decision => "decision",
        MemoryType::Bugfix => "bugfix",
        MemoryType::ReviewRule => "review_rule",
        MemoryType::TaskNote => "task_note",
        MemoryType::SessionSummary => "session_summary",
    }
}
