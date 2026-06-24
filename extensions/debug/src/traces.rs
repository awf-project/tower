use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::protocol::DebugRecordConfig;

const TRACE_METADATA_FILE: &str = "metadata.json";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TraceId(String);

impl TraceId {
    pub fn new(trace_id: impl Into<String>) -> Result<Self, TraceStoreError> {
        let trace_id = trace_id.into();
        let valid = !trace_id.is_empty()
            && !trace_id.contains('/')
            && !trace_id.contains('\\')
            && !trace_id.contains("..")
            && trace_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid {
            return Err(TraceStoreError::InvalidTraceId { trace_id });
        }

        Ok(Self(trace_id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TraceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let trace_id = String::deserialize(deserializer)?;
        Self::new(trace_id).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceMetadata {
    pub trace_id: TraceId,
    pub path: String,
    pub created_unix_secs: u64,
    pub program: String,
    pub args_summary: Vec<String>,
    pub exit_code: Option<i64>,
    pub output_summary: Vec<String>,
    pub output_truncated: bool,
    pub expires_unix_secs: Option<u64>,
    pub ttl_secs: Option<u64>,
    pub prune_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TracePolicy {
    pub trace_root: PathBuf,
    pub ttl_secs: Option<u64>,
    pub max_traces: usize,
    pub record_timeout_secs: u64,
}

impl TracePolicy {
    pub fn from_record_config(config: &DebugRecordConfig) -> Result<TracePolicy, TraceStoreError> {
        let raw_trace_root = config
            .trace_dir
            .clone()
            .unwrap_or_else(|| ".tower/traces".to_owned());
        let trace_root = normalize_trace_root(&raw_trace_root)?;

        Ok(TracePolicy {
            trace_root,
            ttl_secs: config.ttl_secs,
            max_traces: config.max_traces.unwrap_or(20),
            record_timeout_secs: config.record_timeout_secs.unwrap_or(60),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceStoreError {
    InvalidTraceId { trace_id: String },
    InvalidTraceRoot { path: String },
    TraceNotFound { trace_id: TraceId },
    TracePathEscaped { trace_id: TraceId, path: PathBuf },
    DeleteFailed { trace_id: TraceId, message: String },
    MetadataWriteFailed { trace_id: TraceId, message: String },
    MetadataReadFailed { trace_id: TraceId, message: String },
}

impl fmt::Display for TraceStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTraceId { trace_id } => {
                write!(formatter, "invalid trace id: {trace_id}")
            }
            Self::InvalidTraceRoot { path } => {
                write!(formatter, "invalid trace root: {path}")
            }
            Self::TraceNotFound { trace_id } => {
                write!(formatter, "trace not found: {trace_id}")
            }
            Self::TracePathEscaped { trace_id, path } => {
                write!(
                    formatter,
                    "trace path escaped root: {trace_id} at {}",
                    path.display()
                )
            }
            Self::DeleteFailed { trace_id, message } => {
                write!(formatter, "delete failed for trace {trace_id}: {message}")
            }
            Self::MetadataWriteFailed { trace_id, message } => {
                write!(
                    formatter,
                    "metadata write failed for trace {trace_id}: {message}"
                )
            }
            Self::MetadataReadFailed { trace_id, message } => {
                write!(
                    formatter,
                    "metadata read failed for trace {trace_id}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for TraceStoreError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocatedTrace {
    pub trace_id: TraceId,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceCompletion {
    pub program: String,
    pub args_summary: Vec<String>,
    pub exit_code: Option<i64>,
    pub output_summary: Vec<String>,
    pub output_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    pub expired: Vec<TraceId>,
    pub overflow: Vec<TraceId>,
    pub remaining: usize,
}

#[derive(Clone, Debug)]
pub struct TraceStore {
    policy: TracePolicy,
    traces: BTreeMap<TraceId, TraceMetadata>,
    prune_generation: u64,
    next_sequence: u64,
}

impl TraceStore {
    pub fn new(policy: TracePolicy) -> Self {
        Self {
            policy,
            traces: BTreeMap::new(),
            prune_generation: 0,
            next_sequence: 0,
        }
    }

    pub fn open(policy: TracePolicy) -> Result<Self, TraceStoreError> {
        let mut store = Self::new(policy);
        store.load_completed_traces()?;
        Ok(store)
    }

    pub fn policy(&self) -> &TracePolicy {
        &self.policy
    }

    pub fn allocate_trace(
        &mut self,
        program: &str,
        now_unix_secs: u64,
    ) -> Result<AllocatedTrace, TraceStoreError> {
        fs::create_dir_all(&self.policy.trace_root).map_err(|error| {
            TraceStoreError::MetadataWriteFailed {
                trace_id: fallback_trace_id(),
                message: error.to_string(),
            }
        })?;

        loop {
            self.next_sequence += 1;
            let trace_id = TraceId::new(format!(
                "{}-{}-{}-{}",
                now_unix_secs,
                std::process::id(),
                self.next_sequence,
                trace_id_slug(program)
            ))?;
            let path = self.policy.trace_root.join(trace_id.as_str());

            if self.traces.contains_key(&trace_id) || path.exists() {
                continue;
            }

            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(TraceStoreError::MetadataWriteFailed {
                        trace_id: trace_id.clone(),
                        message: error.to_string(),
                    });
                }
            }

            return Ok(AllocatedTrace { trace_id, path });
        }
    }

    pub fn register_completed(
        &mut self,
        allocation: AllocatedTrace,
        completion: TraceCompletion,
        now_unix_secs: u64,
    ) -> Result<TraceMetadata, TraceStoreError> {
        self.ensure_contained_path(&allocation.trace_id, &allocation.path)?;

        let metadata = TraceMetadata {
            trace_id: allocation.trace_id.clone(),
            path: allocation.path.display().to_string(),
            created_unix_secs: now_unix_secs,
            program: completion.program,
            args_summary: completion.args_summary,
            exit_code: completion.exit_code,
            output_summary: completion.output_summary,
            output_truncated: completion.output_truncated,
            expires_unix_secs: self
                .policy
                .ttl_secs
                .map(|ttl_secs| now_unix_secs.saturating_add(ttl_secs)),
            ttl_secs: self.policy.ttl_secs,
            prune_generation: self.prune_generation,
        };

        self.write_metadata(&metadata)?;
        self.traces.insert(allocation.trace_id, metadata.clone());
        Ok(metadata)
    }

    pub fn list_traces(&self) -> Result<Vec<TraceMetadata>, TraceStoreError> {
        let mut traces: Vec<_> = self.traces.values().cloned().collect();
        traces.sort_by(|left, right| {
            (left.created_unix_secs, &left.trace_id)
                .cmp(&(right.created_unix_secs, &right.trace_id))
        });
        Ok(traces)
    }

    pub fn trace(&self, trace_id: &TraceId) -> Result<TraceMetadata, TraceStoreError> {
        let metadata =
            self.traces
                .get(trace_id)
                .cloned()
                .ok_or_else(|| TraceStoreError::TraceNotFound {
                    trace_id: trace_id.clone(),
                })?;
        let path = PathBuf::from(&metadata.path);
        self.ensure_contained_path(trace_id, &path)?;
        if path.exists() {
            Ok(metadata)
        } else {
            Err(TraceStoreError::TraceNotFound {
                trace_id: trace_id.clone(),
            })
        }
    }

    pub fn abort_trace(&self, allocation: &AllocatedTrace) -> Result<(), TraceStoreError> {
        self.ensure_contained_path(&allocation.trace_id, &allocation.path)?;
        if allocation.path.exists() {
            fs::remove_dir_all(&allocation.path).map_err(|error| {
                TraceStoreError::DeleteFailed {
                    trace_id: allocation.trace_id.clone(),
                    message: error.to_string(),
                }
            })?;
        }
        Ok(())
    }

    pub fn delete_trace(&mut self, trace_id: &TraceId) -> Result<(), TraceStoreError> {
        let metadata =
            self.traces
                .get(trace_id)
                .cloned()
                .ok_or_else(|| TraceStoreError::TraceNotFound {
                    trace_id: trace_id.clone(),
                })?;
        let path = PathBuf::from(&metadata.path);
        self.ensure_contained_path(trace_id, &path)?;
        if !path.exists() {
            self.traces.remove(trace_id);
            return Err(TraceStoreError::TraceNotFound {
                trace_id: trace_id.clone(),
            });
        }

        fs::remove_dir_all(&path).map_err(|error| TraceStoreError::DeleteFailed {
            trace_id: trace_id.clone(),
            message: error.to_string(),
        })?;
        self.traces.remove(trace_id);
        Ok(())
    }

    pub fn prune(&mut self, now_unix_secs: u64) -> Result<PruneReport, TraceStoreError> {
        self.prune_generation = self.prune_generation.saturating_add(1);

        let expired = self.expired_trace_ids(now_unix_secs);
        for trace_id in &expired {
            self.remove_trace_for_prune(trace_id)?;
        }

        let mut remaining = self.list_traces()?;
        let overflow_count = remaining.len().saturating_sub(self.policy.max_traces);
        let overflow: Vec<TraceId> = remaining
            .drain(..overflow_count)
            .map(|metadata| metadata.trace_id)
            .collect();
        for trace_id in &overflow {
            self.remove_trace_for_prune(trace_id)?;
        }

        Ok(PruneReport {
            expired,
            overflow,
            remaining: self.traces.len(),
        })
    }

    fn expired_trace_ids(&self, now_unix_secs: u64) -> Vec<TraceId> {
        let mut expired: Vec<_> = self
            .traces
            .values()
            .filter(|metadata| {
                metadata
                    .expires_unix_secs
                    .is_some_and(|expires_unix_secs| expires_unix_secs <= now_unix_secs)
            })
            .cloned()
            .collect();
        expired.sort_by(|left, right| {
            (left.created_unix_secs, &left.trace_id)
                .cmp(&(right.created_unix_secs, &right.trace_id))
        });
        expired
            .into_iter()
            .map(|metadata| metadata.trace_id)
            .collect()
    }

    fn remove_trace_for_prune(&mut self, trace_id: &TraceId) -> Result<(), TraceStoreError> {
        let Some(metadata) = self.traces.get(trace_id).cloned() else {
            return Ok(());
        };
        let path = PathBuf::from(&metadata.path);
        if !path.exists() {
            self.traces.remove(trace_id);
            return Ok(());
        }
        if self.ensure_contained_path(trace_id, &path).is_err() {
            if path.exists() {
                return self.ensure_contained_path(trace_id, &path);
            }
            self.traces.remove(trace_id);
            return Ok(());
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(TraceStoreError::DeleteFailed {
                    trace_id: trace_id.clone(),
                    message: error.to_string(),
                });
            }
        }
        self.traces.remove(trace_id);
        Ok(())
    }

    fn load_completed_traces(&mut self) -> Result<(), TraceStoreError> {
        if !self.policy.trace_root.exists() {
            return Ok(());
        }
        if !self.policy.trace_root.is_dir() {
            return Err(TraceStoreError::MetadataReadFailed {
                trace_id: fallback_trace_id(),
                message: "trace root is not a directory".to_owned(),
            });
        }

        let entries = fs::read_dir(&self.policy.trace_root).map_err(|error| {
            TraceStoreError::MetadataReadFailed {
                trace_id: fallback_trace_id(),
                message: error.to_string(),
            }
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| TraceStoreError::MetadataReadFailed {
                trace_id: fallback_trace_id(),
                message: error.to_string(),
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let metadata_path = path.join(TRACE_METADATA_FILE);
            if !metadata_path.is_file() {
                continue;
            }

            let metadata_bytes =
                fs::read(&metadata_path).map_err(|error| TraceStoreError::MetadataReadFailed {
                    trace_id: fallback_trace_id(),
                    message: error.to_string(),
                })?;
            if metadata_bytes.is_empty() {
                continue;
            }
            let Ok(metadata) = serde_json::from_slice::<TraceMetadata>(&metadata_bytes) else {
                continue;
            };
            if self
                .ensure_contained_path(&metadata.trace_id, &PathBuf::from(&metadata.path))
                .is_err()
            {
                continue;
            }
            self.traces.insert(metadata.trace_id.clone(), metadata);
        }

        Ok(())
    }

    fn write_metadata(&self, metadata: &TraceMetadata) -> Result<(), TraceStoreError> {
        let path = PathBuf::from(&metadata.path).join(TRACE_METADATA_FILE);
        let tmp_path = PathBuf::from(&metadata.path).join(format!("{TRACE_METADATA_FILE}.tmp"));
        let bytes = serde_json::to_vec_pretty(metadata).map_err(|error| {
            TraceStoreError::MetadataWriteFailed {
                trace_id: metadata.trace_id.clone(),
                message: error.to_string(),
            }
        })?;
        fs::write(&tmp_path, bytes).map_err(|error| TraceStoreError::MetadataWriteFailed {
            trace_id: metadata.trace_id.clone(),
            message: error.to_string(),
        })?;
        fs::rename(tmp_path, path).map_err(|error| TraceStoreError::MetadataWriteFailed {
            trace_id: metadata.trace_id.clone(),
            message: error.to_string(),
        })
    }

    fn ensure_contained_path(
        &self,
        trace_id: &TraceId,
        path: &Path,
    ) -> Result<(), TraceStoreError> {
        let root = canonicalize_existing(&self.policy.trace_root).ok_or_else(|| {
            TraceStoreError::TracePathEscaped {
                trace_id: trace_id.clone(),
                path: path.to_path_buf(),
            }
        })?;
        let candidate =
            canonicalize_existing(path).ok_or_else(|| TraceStoreError::TracePathEscaped {
                trace_id: trace_id.clone(),
                path: path.to_path_buf(),
            })?;

        if candidate.parent() == Some(root.as_path())
            && candidate.file_name() == Some(OsStr::new(trace_id.as_str()))
        {
            Ok(())
        } else {
            Err(TraceStoreError::TracePathEscaped {
                trace_id: trace_id.clone(),
                path: path.to_path_buf(),
            })
        }
    }
}

fn normalize_trace_root(raw_path: &str) -> Result<PathBuf, TraceStoreError> {
    if raw_path.is_empty() || raw_path.contains('\\') || raw_path.contains("//") {
        return Err(TraceStoreError::InvalidTraceRoot {
            path: raw_path.to_owned(),
        });
    }

    let path = Path::new(raw_path);
    if path.is_absolute() {
        return Err(TraceStoreError::InvalidTraceRoot {
            path: raw_path.to_owned(),
        });
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            _ => {
                return Err(TraceStoreError::InvalidTraceRoot {
                    path: raw_path.to_owned(),
                });
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(TraceStoreError::InvalidTraceRoot {
            path: raw_path.to_owned(),
        });
    }

    Ok(normalized)
}

fn trace_id_slug(program: &str) -> String {
    let mut slug: String = program
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(16)
        .collect();
    if slug.is_empty() {
        slug.push_str("trace");
    }
    slug
}

fn fallback_trace_id() -> TraceId {
    TraceId("trace-allocation".to_owned())
}

fn canonicalize_existing(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    fn record_config() -> DebugRecordConfig {
        DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: None,
            ttl_secs: None,
            max_traces: None,
            record_timeout_secs: None,
        }
    }

    fn policy(trace_root: PathBuf) -> TracePolicy {
        TracePolicy {
            trace_root,
            ttl_secs: None,
            max_traces: 20,
            record_timeout_secs: 60,
        }
    }

    fn temp_trace_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tower-traces-{name}-{unique}"));
        fs::create_dir_all(&root).expect("create temp trace root");
        root
    }

    fn completion(program: &str) -> TraceCompletion {
        TraceCompletion {
            program: program.to_owned(),
            args_summary: vec!["--case".to_owned(), "smoke".to_owned()],
            exit_code: Some(0),
            output_summary: vec!["ok".to_owned()],
            output_truncated: false,
        }
    }

    fn metadata(trace_id: &str, path: &Path, created_unix_secs: u64) -> TraceMetadata {
        TraceMetadata {
            trace_id: TraceId::new(trace_id).expect("valid trace id"),
            path: path.display().to_string(),
            created_unix_secs,
            program: "target/debug/probe".to_owned(),
            args_summary: vec!["--case".to_owned(), "smoke".to_owned()],
            exit_code: Some(0),
            output_summary: vec!["ok".to_owned()],
            output_truncated: false,
            expires_unix_secs: None,
            ttl_secs: None,
            prune_generation: 0,
        }
    }

    fn register_trace(
        store: &mut TraceStore,
        program: &str,
        created_unix_secs: u64,
    ) -> TraceMetadata {
        let allocation = store
            .allocate_trace(program, created_unix_secs)
            .expect("trace allocation");
        store
            .register_completed(allocation, completion(program), created_unix_secs)
            .expect("trace registration")
    }

    #[test]
    fn traces_trace_id_exists_as_a_stable_typed_identifier_suitable_for_serde_round_trip() {
        for valid in ["trace-1", "trace_2", "trace.3", "ABCxyz09"] {
            let trace_id = TraceId::new(valid).expect("valid trace id");
            let serialized = serde_json::to_value(&trace_id).expect("trace id serializes");
            assert_eq!(serialized, json!(valid));
            assert_eq!(
                serde_json::from_value::<TraceId>(serialized).expect("trace id deserializes"),
                trace_id
            );
        }

        for invalid in ["", "../trace", "trace/1", "trace\\1", "trace..1"] {
            assert_eq!(
                TraceId::new(invalid).expect_err("invalid trace id"),
                TraceStoreError::InvalidTraceId {
                    trace_id: invalid.to_owned()
                }
            );
        }
    }

    #[test]
    fn traces_trace_metadata_records_exact_public_serde_fields() {
        let metadata = metadata("trace-1", Path::new(".tower/traces/trace-1"), 100);

        assert_eq!(
            serde_json::to_value(&metadata).expect("metadata serializes"),
            json!({
                "trace_id": "trace-1",
                "path": ".tower/traces/trace-1",
                "created_unix_secs": 100,
                "program": "target/debug/probe",
                "args_summary": ["--case", "smoke"],
                "exit_code": 0,
                "output_summary": ["ok"],
                "output_truncated": false,
                "expires_unix_secs": null,
                "ttl_secs": null,
                "prune_generation": 0
            })
        );
    }

    #[test]
    fn traces_trace_policy_exists_with_exact_public_fields() {
        let policy = TracePolicy {
            trace_root: PathBuf::from(".tower/traces"),
            ttl_secs: Some(86_400),
            max_traces: 25,
            record_timeout_secs: 30,
        };

        assert_eq!(policy.trace_root, PathBuf::from(".tower/traces"));
        assert_eq!(policy.ttl_secs, Some(86_400));
        assert_eq!(policy.max_traces, 25);
        assert_eq!(policy.record_timeout_secs, 30);
    }

    #[test]
    fn traces_trace_policy_from_record_config_normalizes_defaults_and_configured_values() {
        let defaults = TracePolicy::from_record_config(&record_config()).expect("default policy");
        assert_eq!(defaults.trace_root, PathBuf::from(".tower/traces"));
        assert_eq!(defaults.ttl_secs, None);
        assert_eq!(defaults.max_traces, 20);
        assert_eq!(defaults.record_timeout_secs, 60);

        let configured = TracePolicy::from_record_config(&DebugRecordConfig {
            backend: "rr".to_owned(),
            trace_dir: Some("debug/traces".to_owned()),
            ttl_secs: Some(86_400),
            max_traces: Some(25),
            record_timeout_secs: Some(30),
        })
        .expect("configured policy");
        assert_eq!(configured.trace_root, PathBuf::from("debug/traces"));
        assert_eq!(configured.ttl_secs, Some(86_400));
        assert_eq!(configured.max_traces, 25);
        assert_eq!(configured.record_timeout_secs, 30);
    }

    #[test]
    fn traces_trace_policy_from_record_config_returns_invalid_trace_root_for_escaping_paths() {
        for path in [
            "/tmp/tower-traces",
            "../traces",
            "trace//child",
            "trace/../child",
        ] {
            let error = TracePolicy::from_record_config(&DebugRecordConfig {
                backend: "rr".to_owned(),
                trace_dir: Some(path.to_owned()),
                ttl_secs: None,
                max_traces: None,
                record_timeout_secs: None,
            })
            .expect_err("escaping root should be rejected");

            assert_eq!(
                error,
                TraceStoreError::InvalidTraceRoot {
                    path: path.to_owned()
                }
            );
        }
    }

    #[test]
    fn traces_trace_store_error_exists_with_exact_variants() {
        let trace_id = TraceId::new("trace-1").expect("valid trace id");

        assert_eq!(
            TraceStoreError::InvalidTraceId {
                trace_id: "bad/id".to_owned()
            },
            TraceStoreError::InvalidTraceId {
                trace_id: "bad/id".to_owned()
            }
        );
        assert_eq!(
            TraceStoreError::InvalidTraceRoot {
                path: "../traces".to_owned()
            },
            TraceStoreError::InvalidTraceRoot {
                path: "../traces".to_owned()
            }
        );
        assert_eq!(
            TraceStoreError::TraceNotFound {
                trace_id: trace_id.clone()
            },
            TraceStoreError::TraceNotFound {
                trace_id: trace_id.clone()
            }
        );
        assert_eq!(
            TraceStoreError::TracePathEscaped {
                trace_id: trace_id.clone(),
                path: PathBuf::from("/tmp/outside")
            },
            TraceStoreError::TracePathEscaped {
                trace_id: trace_id.clone(),
                path: PathBuf::from("/tmp/outside")
            }
        );
        assert_eq!(
            TraceStoreError::DeleteFailed {
                trace_id: trace_id.clone(),
                message: "permission denied".to_owned()
            },
            TraceStoreError::DeleteFailed {
                trace_id: trace_id.clone(),
                message: "permission denied".to_owned()
            }
        );
        assert_eq!(
            TraceStoreError::MetadataWriteFailed {
                trace_id: trace_id.clone(),
                message: "disk full".to_owned()
            },
            TraceStoreError::MetadataWriteFailed {
                trace_id: trace_id.clone(),
                message: "disk full".to_owned()
            }
        );
        assert_eq!(
            TraceStoreError::MetadataReadFailed {
                trace_id: trace_id.clone(),
                message: "malformed metadata".to_owned()
            },
            TraceStoreError::MetadataReadFailed {
                trace_id,
                message: "malformed metadata".to_owned()
            }
        );
    }

    #[test]
    fn traces_public_trace_apis_return_trace_store_error_results() {
        fn assert_result<T>(result: Result<T, TraceStoreError>) -> Result<T, TraceStoreError> {
            result
        }

        let root = temp_trace_root("api-results");
        let mut store = TraceStore::new(policy(root));
        let trace_id = TraceId::new("missing").expect("valid trace id");

        let _ = assert_result(store.allocate_trace("target/debug/probe", 100));
        let _ = assert_result(store.list_traces());
        let _ = assert_result(store.delete_trace(&trace_id));
        let _ = assert_result(store.prune(100));
    }

    #[test]
    fn traces_allocate_trace_creates_unique_trace_id_and_contained_directory_path() {
        let root = temp_trace_root("allocate");
        let mut store = TraceStore::new(policy(root.clone()));

        let first = store
            .allocate_trace("target/debug/probe", 100)
            .expect("first trace allocation");
        let second = store
            .allocate_trace("target/debug/probe", 100)
            .expect("second trace allocation");

        assert_ne!(first.trace_id, second.trace_id);
        assert!(first.path.starts_with(&root));
        assert!(second.path.starts_with(&root));
        assert!(first.path.is_dir());
        assert!(second.path.is_dir());
    }

    #[test]
    fn traces_register_completed_persists_completed_trace_metadata_without_escaping_root() {
        let root = temp_trace_root("register");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root.clone(),
            ttl_secs: Some(60),
            max_traces: 20,
            record_timeout_secs: 60,
        });
        let allocation = store
            .allocate_trace("target/debug/probe", 100)
            .expect("trace allocation");

        let metadata = store
            .register_completed(allocation, completion("target/debug/probe"), 120)
            .expect("completed trace metadata");

        assert!(PathBuf::from(&metadata.path).starts_with(&root));
        assert_eq!(metadata.created_unix_secs, 120);
        assert_eq!(metadata.program, "target/debug/probe");
        assert_eq!(metadata.args_summary, ["--case", "smoke"]);
        assert_eq!(metadata.exit_code, Some(0));
        assert_eq!(metadata.output_summary, ["ok"]);
        assert!(!metadata.output_truncated);
        assert_eq!(metadata.expires_unix_secs, Some(180));
        assert_eq!(metadata.ttl_secs, Some(60));
        assert!(
            PathBuf::from(&metadata.path)
                .join(TRACE_METADATA_FILE)
                .is_file()
        );
    }

    #[test]
    fn traces_trace_completion_has_exact_public_fields() {
        let completion = TraceCompletion {
            program: "target/debug/probe".to_owned(),
            args_summary: vec!["--case".to_owned(), "smoke".to_owned()],
            exit_code: Some(17),
            output_summary: vec!["failed".to_owned()],
            output_truncated: true,
        };

        assert_eq!(
            serde_json::to_value(&completion).expect("completion serializes"),
            json!({
                "program": "target/debug/probe",
                "args_summary": ["--case", "smoke"],
                "exit_code": 17,
                "output_summary": ["failed"],
                "output_truncated": true
            })
        );
    }

    #[test]
    fn traces_list_traces_returns_traces_sorted_by_created_time_and_trace_id_ascending() {
        let root = temp_trace_root("list");
        let mut store = TraceStore::new(policy(root));
        let trace_c = register_trace(&mut store, "target/debug/probe-c", 200);
        let trace_a = register_trace(&mut store, "target/debug/probe-a", 100);
        let trace_b = register_trace(&mut store, "target/debug/probe-b", 100);

        let traces = store.list_traces().expect("list traces");
        let created_and_ids: Vec<(u64, TraceId)> = traces
            .into_iter()
            .map(|trace| (trace.created_unix_secs, trace.trace_id))
            .collect();

        assert_eq!(
            created_and_ids,
            [
                (100, trace_a.trace_id),
                (100, trace_b.trace_id),
                (200, trace_c.trace_id)
            ]
        );
    }

    #[test]
    fn traces_open_loads_completed_trace_metadata_from_existing_trace_directories() {
        let root = temp_trace_root("open");
        let mut store = TraceStore::new(policy(root.clone()));
        let trace = register_trace(&mut store, "target/debug/probe", 100);

        let reopened = TraceStore::open(policy(root)).expect("open trace store");

        assert_eq!(reopened.list_traces().expect("list traces"), [trace]);
    }

    #[test]
    fn traces_open_ignores_missing_empty_or_partial_metadata_files_from_in_progress_traces() {
        let root = temp_trace_root("open-partial");
        fs::create_dir_all(root.join("missing-metadata")).expect("create trace without metadata");
        let empty = root.join("empty-metadata");
        fs::create_dir_all(&empty).expect("create empty metadata trace");
        fs::write(empty.join(TRACE_METADATA_FILE), "").expect("write empty metadata");
        let partial = root.join("partial-metadata");
        fs::create_dir_all(&partial).expect("create partial metadata trace");
        fs::write(partial.join(TRACE_METADATA_FILE), "{").expect("write partial metadata");

        let reopened = TraceStore::open(policy(root)).expect("partial traces do not fail open");

        assert_eq!(reopened.list_traces().expect("list traces"), []);
    }

    #[test]
    fn traces_prune_applies_max_traces_when_ttl_is_disabled() {
        let root = temp_trace_root("prune-max-without-ttl");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root,
            ttl_secs: None,
            max_traces: 2,
            record_timeout_secs: 60,
        });
        let oldest = register_trace(&mut store, "target/debug/probe-oldest", 100);
        let newer = register_trace(&mut store, "target/debug/probe-newer", 110);
        let newest = register_trace(&mut store, "target/debug/probe-newest", 120);

        let report = store.prune(10_000).expect("prune traces");

        assert_eq!(report.expired, []);
        assert_eq!(report.overflow, [oldest.trace_id]);
        assert_eq!(report.remaining, 2);
        let remaining: Vec<TraceId> = store
            .list_traces()
            .expect("list traces")
            .into_iter()
            .map(|trace| trace.trace_id)
            .collect();
        assert_eq!(remaining, [newer.trace_id, newest.trace_id]);
    }

    #[test]
    fn traces_prune_applies_ttl_expiry_without_max_trace_overflow() {
        let root = temp_trace_root("prune-ttl-only");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root,
            ttl_secs: Some(10),
            max_traces: 20,
            record_timeout_secs: 60,
        });
        let expired = register_trace(&mut store, "target/debug/probe-expired", 100);
        let retained = register_trace(&mut store, "target/debug/probe-retained", 115);

        let report = store.prune(111).expect("prune traces");

        assert_eq!(report.expired, [expired.trace_id]);
        assert_eq!(report.overflow, []);
        assert_eq!(report.remaining, 1);
        let remaining: Vec<TraceId> = store
            .list_traces()
            .expect("list traces")
            .into_iter()
            .map(|trace| trace.trace_id)
            .collect();
        assert_eq!(remaining, [retained.trace_id]);
    }

    #[test]
    fn traces_delete_trace_removes_only_selected_contained_trace_and_missing_deleted_or_expired_ids_return_trace_not_found()
     {
        let root = temp_trace_root("delete");
        let mut store = TraceStore::new(policy(root));
        let keep = register_trace(&mut store, "target/debug/probe-keep", 100);
        let delete = register_trace(&mut store, "target/debug/probe-delete", 101);
        let keep_path = PathBuf::from(&keep.path);
        let delete_path = PathBuf::from(&delete.path);
        let delete_id = delete.trace_id;

        store.delete_trace(&delete_id).expect("delete trace");

        let remaining: Vec<TraceId> = store
            .list_traces()
            .expect("list traces")
            .into_iter()
            .map(|trace| trace.trace_id)
            .collect();
        assert_eq!(remaining, [keep.trace_id]);
        assert!(keep_path.exists());
        assert!(!delete_path.exists());
        assert_eq!(
            store
                .delete_trace(&delete_id)
                .expect_err("deleted trace is missing"),
            TraceStoreError::TraceNotFound {
                trace_id: delete_id
            }
        );
    }

    #[test]
    fn traces_prune_removes_expired_traces_and_oldest_traces_above_max_traces() {
        let root = temp_trace_root("prune");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root,
            ttl_secs: Some(10),
            max_traces: 2,
            record_timeout_secs: 60,
        });
        let expired = register_trace(&mut store, "target/debug/probe-expired", 100);
        let overflow = register_trace(&mut store, "target/debug/probe-overflow", 120);
        let keep_a = register_trace(&mut store, "target/debug/probe-keep-a", 130);
        let keep_b = register_trace(&mut store, "target/debug/probe-keep-b", 140);

        let report = store.prune(115).expect("prune traces");

        assert_eq!(report.expired, [expired.trace_id]);
        assert_eq!(report.overflow, [overflow.trace_id]);
        assert_eq!(report.remaining, 2);
        let remaining: Vec<TraceId> = store
            .list_traces()
            .expect("list traces")
            .into_iter()
            .map(|trace| trace.trace_id)
            .collect();
        assert_eq!(remaining, [keep_a.trace_id, keep_b.trace_id]);
    }

    #[test]
    fn traces_missing_deleted_expired_or_invalid_trace_ids_return_stable_error_variants_without_panicking()
     {
        let root = temp_trace_root("stable-errors");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root,
            ttl_secs: Some(1),
            max_traces: 20,
            record_timeout_secs: 60,
        });
        let invalid = TraceId::new("../escaped").expect_err("invalid ids are structured errors");
        assert_eq!(
            invalid,
            TraceStoreError::InvalidTraceId {
                trace_id: "../escaped".to_owned()
            }
        );

        let missing = TraceId::new("missing").expect("valid trace id");
        assert_eq!(
            store
                .delete_trace(&missing)
                .expect_err("missing trace is structured error"),
            TraceStoreError::TraceNotFound {
                trace_id: missing.clone()
            }
        );

        let allocation = store
            .allocate_trace("target/debug/probe", 100)
            .expect("trace allocation");
        let expired_id = allocation.trace_id.clone();
        store
            .register_completed(allocation, completion("target/debug/probe"), 100)
            .expect("register trace");
        store.prune(102).expect("expire trace");
        assert_eq!(
            store
                .delete_trace(&expired_id)
                .expect_err("expired trace is missing"),
            TraceStoreError::TraceNotFound {
                trace_id: expired_id
            }
        );
    }

    #[test]
    fn traces_register_completed_rejects_allocations_with_paths_outside_trace_root() {
        let root = temp_trace_root("escape");
        let outside = temp_trace_root("outside");
        let outside_file = outside.join("do-not-delete");
        fs::write(&outside_file, "keep").expect("create outside sentinel");
        let mut store = TraceStore::new(policy(root));
        let trace_id = TraceId::new("escaped").expect("valid trace id");
        let allocation = AllocatedTrace {
            trace_id: trace_id.clone(),
            path: outside_file.clone(),
        };

        assert_eq!(
            store
                .register_completed(allocation, completion("target/debug/probe"), 100)
                .expect_err("escaped trace path is rejected"),
            TraceStoreError::TracePathEscaped {
                trace_id,
                path: outside_file.clone()
            }
        );
        assert!(outside_file.exists());
    }

    #[test]
    fn traces_register_completed_rejects_root_sibling_nested_and_mismatched_paths() {
        let root = temp_trace_root("shape");
        let sibling = root
            .parent()
            .expect("temp root has parent")
            .join("tower-traces-sibling");
        fs::create_dir_all(&sibling).expect("create sibling trace dir");
        let nested = root.join("parent").join("nested");
        fs::create_dir_all(&nested).expect("create nested trace dir");
        let mismatched = root.join("other-trace");
        fs::create_dir_all(&mismatched).expect("create mismatched trace dir");

        let mut store = TraceStore::new(policy(root.clone()));

        for (trace_id, path) in [
            ("root", root),
            ("sibling", sibling),
            ("nested", nested),
            ("expected-trace", mismatched),
        ] {
            let trace_id = TraceId::new(trace_id).expect("valid trace id");
            let allocation = AllocatedTrace {
                trace_id: trace_id.clone(),
                path: path.clone(),
            };

            assert_eq!(
                store
                    .register_completed(allocation, completion("target/debug/probe"), 100)
                    .expect_err("non-trace directory should be rejected"),
                TraceStoreError::TracePathEscaped { trace_id, path }
            );
        }
    }

    #[test]
    fn traces_delete_trace_rejects_stale_metadata_for_trace_root_before_deletion() {
        let root = temp_trace_root("delete-root");
        let keep_dir = root.join("keep");
        fs::create_dir_all(&keep_dir).expect("create retained trace");
        let keep_file = keep_dir.join("sentinel");
        fs::write(&keep_file, "keep").expect("create retained sentinel");
        let mut store = TraceStore::new(policy(root.clone()));
        let trace_id = TraceId::new("root").expect("valid trace id");
        store
            .traces
            .insert(trace_id.clone(), metadata(trace_id.as_str(), &root, 100));

        assert_eq!(
            store
                .delete_trace(&trace_id)
                .expect_err("trace root metadata is rejected"),
            TraceStoreError::TracePathEscaped {
                trace_id,
                path: root
            }
        );
        assert!(keep_file.exists());
    }

    #[test]
    fn traces_prune_rejects_stale_metadata_for_trace_root_before_deletion() {
        let root = temp_trace_root("prune-root");
        let keep_dir = root.join("keep");
        fs::create_dir_all(&keep_dir).expect("create retained trace");
        let keep_file = keep_dir.join("sentinel");
        fs::write(&keep_file, "keep").expect("create retained sentinel");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root.clone(),
            ttl_secs: Some(1),
            max_traces: 20,
            record_timeout_secs: 60,
        });
        let trace_id = TraceId::new("root").expect("valid trace id");
        let mut metadata = metadata(trace_id.as_str(), &root, 100);
        metadata.expires_unix_secs = Some(101);
        store.traces.insert(trace_id.clone(), metadata);

        assert_eq!(
            store
                .prune(101)
                .expect_err("trace root metadata is rejected"),
            TraceStoreError::TracePathEscaped {
                trace_id,
                path: root
            }
        );
        assert!(keep_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn traces_delete_trace_rejects_symlink_escape_before_deletion() {
        use std::os::unix::fs::symlink;

        let root = temp_trace_root("delete-symlink");
        let outside = temp_trace_root("delete-symlink-outside");
        let outside_file = outside.join("sentinel");
        fs::write(&outside_file, "keep").expect("create outside sentinel");
        let trace_id = TraceId::new("escaped").expect("valid trace id");
        let symlink_path = root.join(trace_id.as_str());
        symlink(&outside, &symlink_path).expect("create symlink trace dir");
        let mut store = TraceStore::new(policy(root));
        store.traces.insert(
            trace_id.clone(),
            metadata(trace_id.as_str(), &symlink_path, 100),
        );

        assert_eq!(
            store
                .delete_trace(&trace_id)
                .expect_err("symlink escape is rejected"),
            TraceStoreError::TracePathEscaped {
                trace_id,
                path: symlink_path
            }
        );
        assert!(outside_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn traces_prune_rejects_symlink_escape_before_deletion() {
        use std::os::unix::fs::symlink;

        let root = temp_trace_root("prune-symlink");
        let outside = temp_trace_root("prune-symlink-outside");
        let outside_file = outside.join("sentinel");
        fs::write(&outside_file, "keep").expect("create outside sentinel");
        let trace_id = TraceId::new("escaped").expect("valid trace id");
        let symlink_path = root.join(trace_id.as_str());
        symlink(&outside, &symlink_path).expect("create symlink trace dir");
        let mut store = TraceStore::new(TracePolicy {
            trace_root: root,
            ttl_secs: Some(1),
            max_traces: 20,
            record_timeout_secs: 60,
        });
        let mut metadata = metadata(trace_id.as_str(), &symlink_path, 100);
        metadata.expires_unix_secs = Some(101);
        store.traces.insert(trace_id.clone(), metadata);

        assert_eq!(
            store.prune(101).expect_err("symlink escape is rejected"),
            TraceStoreError::TracePathEscaped {
                trace_id,
                path: symlink_path
            }
        );
        assert!(outside_file.exists());
    }
}
