#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::adapters::config::{ProjectId, ProjectIdentity, QueryableProject, QueryableProjects};
use crate::adapters::daemon::client::{ExternalDaemonError, ExternalProjectConnector};
use crate::adapters::mcp::native_tools::{ExternalAccess, NativeToolRegistry};
use crate::adapters::mcp::registry::ToolRegistry;
use crate::adapters::mcp::types::{ToolDesc, ToolError};
use crate::adapters::path_identity::lexical_normalize;
use crate::domain::CancellationSignal;

pub const EXTERNAL_READ_ONLY_BOUNDARY: i32 = -32060;
pub const EXTERNAL_PROJECT_UNAVAILABLE: i32 = -32061;
pub const EXTERNAL_TARGET_FAILURE: i32 = -32062;
pub const EXTERNAL_TARGET_RESPONSE_INVALID: i32 = -32063;
pub const EXTERNAL_QUERY_CANCELLED: i32 = -32064;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ProjectSelector {
    pub id: Option<String>,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ExternalQueryRequest {
    pub project: ProjectSelector,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExternalQueryResult {
    pub project: ProjectIdentity,
    pub result: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProjectListResult {
    pub projects: Vec<QueryableProjectListing>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueryableProjectListing {
    pub id: ProjectId,
    pub name: String,
    pub path: PathBuf,
}

pub struct ProjectQueryRegistry {
    active_workspace: PathBuf,
    active_workspace_canonical: Option<PathBuf>,
    projects: QueryableProjects,
    availability_timeout: Duration,
    connector: Arc<dyn ExternalProjectConnector>,
    event_sink: Arc<dyn ExternalQueryEventSink>,
}

trait ExternalQueryEventSink: Send + Sync {
    fn record_failure(&self, code: &'static str, project: &ProjectIdentity, operation: &str);
}

struct NoOpExternalQueryEventSink;

impl ExternalQueryEventSink for NoOpExternalQueryEventSink {
    fn record_failure(&self, _code: &'static str, _project: &ProjectIdentity, _operation: &str) {}
}

impl ProjectQueryRegistry {
    #[must_use]
    pub fn new(
        active_workspace: PathBuf,
        projects: QueryableProjects,
        availability_timeout: Duration,
        connector: Arc<dyn ExternalProjectConnector>,
    ) -> ProjectQueryRegistry {
        let active_workspace = absolute_lexical_path(&active_workspace);
        let active_workspace_canonical = std::fs::canonicalize(&active_workspace).ok();
        Self {
            active_workspace,
            active_workspace_canonical,
            projects,
            availability_timeout,
            connector,
            event_sink: Arc::new(NoOpExternalQueryEventSink),
        }
    }

    #[cfg(test)]
    fn with_event_sink(mut self, event_sink: Arc<dyn ExternalQueryEventSink>) -> Self {
        self.event_sink = event_sink;
        self
    }

    fn list_projects(&self) -> Result<Value, ToolError> {
        serialize(ProjectListResult {
            projects: self
                .projects
                .iter()
                .map(|project| QueryableProjectListing {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    path: project.configured_path.clone(),
                })
                .collect(),
        })
    }

    fn query_project(
        &self,
        args: Value,
        cancellation: &dyn CancellationSignal,
    ) -> Result<Value, ToolError> {
        let request: ExternalQueryRequest = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;
        if request.tool.is_empty() || !request.arguments.is_object() {
            return Err(ToolError::InvalidArgs(
                "tool must be non-empty and arguments must be an object".to_owned(),
            ));
        }

        let project = self.resolve_project(&request.project)?;
        if NativeToolRegistry::external_access(&request.tool) != Some(ExternalAccess::ReadOnly) {
            return Err(application_error(
                EXTERNAL_READ_ONLY_BOUNDARY,
                "external project operation is not permitted",
                "external_read_only_boundary",
                project,
                &request.tool,
            ));
        }

        let identity = project_identity(project);
        match self.connector.call(
            project,
            &request.tool,
            request.arguments,
            self.availability_timeout,
            cancellation,
        ) {
            Ok(result) => serialize(ExternalQueryResult {
                project: identity.clone(),
                result,
            }),
            Err(error) => {
                let (error, stable_code) = map_external_error(error, project, &request.tool);
                self.event_sink
                    .record_failure(stable_code, &identity, &request.tool);
                Err(error)
            }
        }
    }

    fn resolve_project(&self, selector: &ProjectSelector) -> Result<&QueryableProject, ToolError> {
        let selector_count = usize::from(selector.id.is_some())
            + usize::from(selector.name.is_some())
            + usize::from(selector.path.is_some());
        if selector_count != 1 {
            return Err(ToolError::InvalidArgs(
                "project selector must contain exactly one of id, name, or path".to_owned(),
            ));
        }

        let candidates = if let Some(id) = &selector.id {
            self.projects
                .iter()
                .filter(|project| project.id.as_str() == id)
                .collect()
        } else if let Some(name) = &selector.name {
            self.projects
                .iter()
                .filter(|project| project.name == *name)
                .collect()
        } else {
            self.path_candidates(selector.path.as_deref().expect("path selector is present"))?
        };

        let mut ids = HashSet::new();
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|project| ids.insert(project.id.as_str()))
            .collect();
        let [project] = candidates.as_slice() else {
            return Err(ToolError::InvalidArgs(
                "project selector must resolve to exactly one configured project".to_owned(),
            ));
        };
        if self.is_active_project(project) {
            return Err(ToolError::InvalidArgs(
                "the active workspace cannot be queried as an external project".to_owned(),
            ));
        }
        Ok(project)
    }

    fn path_candidates(&self, selector: &Path) -> Result<Vec<&QueryableProject>, ToolError> {
        let absolute = if selector.is_absolute() {
            selector.to_path_buf()
        } else {
            self.active_workspace.join(selector)
        };
        let lexical = lexical_normalize(&absolute);
        let (canonical, selector_not_found) = match std::fs::canonicalize(&absolute) {
            Ok(path) => (Some(path), false),
            // A missing original spelling may match only the configured
            // spelling or lexical identity. Canonicalizing after collapsing
            // `..` would turn a missing selector into a resolved alias.
            Err(error) if error.kind() == io::ErrorKind::NotFound => (None, true),
            Err(_) => {
                return Err(ToolError::InvalidArgs(
                    "project path selector could not be resolved".to_owned(),
                ));
            }
        };

        if lexical == self.active_workspace
            || canonical.as_ref() == self.active_workspace_canonical.as_ref()
        {
            return Err(ToolError::InvalidArgs(
                "the active workspace cannot be queried as an external project".to_owned(),
            ));
        }

        Ok(self
            .projects
            .iter()
            .filter(|project| {
                let spelling_or_lexical =
                    project.configured_path == selector || project.resolved_path == lexical;
                if selector_not_found {
                    project.canonical_path.is_none() && spelling_or_lexical
                } else {
                    spelling_or_lexical
                        || canonical
                            .as_ref()
                            .zip(project.canonical_path.as_ref())
                            .is_some_and(|(left, right)| left == right)
                }
            })
            .collect())
    }

    fn is_active_project(&self, project: &QueryableProject) -> bool {
        project.resolved_path == self.active_workspace
            || project.canonical_path.as_ref() == self.active_workspace_canonical.as_ref()
    }
}

impl ToolRegistry for ProjectQueryRegistry {
    fn list(&self) -> Vec<ToolDesc> {
        vec![
            ToolDesc {
                name: "tower_list_projects".to_owned(),
                description: "List explicitly configured external projects".to_owned(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
            ToolDesc {
                name: "tower_query_project".to_owned(),
                description: "Run a read-only native tool in one configured external project"
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "project": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "name": {"type": "string"},
                                "path": {"type": "string"}
                            }
                        },
                        "tool": {"type": "string"},
                        "arguments": {"type": "object"}
                    },
                    "required": ["project", "tool", "arguments"]
                }),
            },
        ]
    }

    fn call(
        &mut self,
        name: &str,
        args: Value,
        cancellation: &dyn CancellationSignal,
    ) -> Result<Value, ToolError> {
        match name {
            "tower_list_projects" => self.list_projects(),
            "tower_query_project" => self.query_project(args, cancellation),
            _ => Err(ToolError::NotFound(name.to_owned())),
        }
    }
}

fn serialize(value: impl Serialize) -> Result<Value, ToolError> {
    serde_json::to_value(value).map_err(|error| ToolError::ExecutionFailed(error.to_string()))
}

fn absolute_lexical_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        lexical_normalize(path)
    } else {
        let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        lexical_normalize(&current.join(path))
    }
}

fn project_identity(project: &QueryableProject) -> ProjectIdentity {
    ProjectIdentity {
        id: project.id.clone(),
        name: project.name.clone(),
    }
}

fn application_error(
    code: i32,
    message: &str,
    stable_code: &str,
    project: &QueryableProject,
    operation: &str,
) -> ToolError {
    ToolError::Application {
        code,
        message: message.to_owned(),
        data: Some(json!({
            "code": stable_code,
            "project": project_identity(project),
            "operation": operation,
        })),
    }
}

fn map_external_error(
    error: ExternalDaemonError,
    project: &QueryableProject,
    operation: &str,
) -> (ToolError, &'static str) {
    let (code, message, stable_code) = match error {
        ExternalDaemonError::AvailabilityTimeout { .. } => (
            EXTERNAL_PROJECT_UNAVAILABLE,
            "external project is unavailable",
            "external_project_unavailable",
        ),
        ExternalDaemonError::Handshake { .. }
        | ExternalDaemonError::Initialize { .. }
        | ExternalDaemonError::TargetCall { .. } => (
            EXTERNAL_TARGET_FAILURE,
            "external project operation failed",
            "external_target_failure",
        ),
        ExternalDaemonError::Decode { .. } => (
            EXTERNAL_TARGET_RESPONSE_INVALID,
            "external project response is invalid",
            "external_target_response_invalid",
        ),
        ExternalDaemonError::Cancelled { .. } => (
            EXTERNAL_QUERY_CANCELLED,
            "external project query was cancelled",
            "external_query_cancelled",
        ),
    };
    (
        application_error(code, message, stable_code, project, operation),
        stable_code,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tempfile::{TempDir, tempdir};

    use super::{
        EXTERNAL_PROJECT_UNAVAILABLE, EXTERNAL_QUERY_CANCELLED, EXTERNAL_READ_ONLY_BOUNDARY,
        EXTERNAL_TARGET_FAILURE, EXTERNAL_TARGET_RESPONSE_INVALID, ExternalQueryEventSink,
        ProjectQueryRegistry,
    };
    use crate::adapters::config::{
        ProjectId, ProjectIdentity, QueryableProject, QueryableProjects, load,
    };
    use crate::adapters::daemon::client::{ExternalDaemonError, ExternalProjectConnector};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::mcp::types::ToolError;
    use crate::domain::{CancellationSignal, NeverCancelled};

    #[derive(Clone, Debug, PartialEq)]
    struct RecordedCall {
        project: QueryableProject,
        operation: String,
        arguments: Value,
        availability_timeout: Duration,
    }

    #[derive(Default)]
    struct RecordingState {
        calls: Vec<RecordedCall>,
        responses: VecDeque<Result<Value, ExternalDaemonError>>,
    }

    #[derive(Clone, Default)]
    struct RecordingConnector {
        state: Arc<Mutex<RecordingState>>,
    }

    impl RecordingConnector {
        fn with_responses(
            responses: impl IntoIterator<Item = Result<Value, ExternalDaemonError>>,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(RecordingState {
                    calls: Vec::new(),
                    responses: responses.into_iter().collect(),
                })),
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl ExternalProjectConnector for RecordingConnector {
        fn call(
            &self,
            project: &QueryableProject,
            operation: &str,
            arguments: Value,
            availability_timeout: Duration,
            _cancellation: &dyn CancellationSignal,
        ) -> Result<Value, ExternalDaemonError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(RecordedCall {
                project: project.clone(),
                operation: operation.to_owned(),
                arguments,
                availability_timeout,
            });
            state
                .responses
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"matches": []})))
        }
    }

    #[derive(Clone, Default)]
    struct CapturingEventSink(Arc<Mutex<Vec<String>>>);

    impl ExternalQueryEventSink for CapturingEventSink {
        fn record_failure(&self, code: &'static str, project: &ProjectIdentity, operation: &str) {
            self.0.lock().unwrap().push(format!(
                "code={code} project_id={} project_name={} operation={operation}",
                project.id.as_str(),
                project.name
            ));
        }
    }

    impl CapturingEventSink {
        fn captured(&self) -> String {
            self.0.lock().unwrap().join("\n")
        }
    }

    struct Fixture {
        _temp: TempDir,
        active: PathBuf,
        alpha: PathBuf,
        beta: PathBuf,
        projects: QueryableProjects,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempdir().unwrap();
            let active = temp.path().join("active");
            let alpha = temp.path().join("alpha");
            let beta = temp.path().join("beta");
            fs::create_dir_all(active.join(".tower")).unwrap();
            fs::create_dir_all(&alpha).unwrap();
            fs::create_dir_all(&beta).unwrap();
            fs::write(
                active.join(".tower/config.toml"),
                format!(
                    "[[projects.queryable]]\nid = \"alpha-id\"\nname = \"Alpha\"\npath = \"{}\"\n\
                     [[projects.queryable]]\nid = \"beta-id\"\nname = \"Beta\"\npath = \"{}\"\n",
                    alpha.display(),
                    beta.display()
                ),
            )
            .unwrap();
            let projects = load(&active).unwrap().projects.queryable().clone();
            Self {
                _temp: temp,
                active,
                alpha,
                beta,
                projects,
            }
        }

        fn registry(&self, connector: RecordingConnector) -> ProjectQueryRegistry {
            ProjectQueryRegistry::new(
                self.active.clone(),
                self.projects.clone(),
                Duration::from_secs(7),
                Arc::new(connector),
            )
        }
    }

    fn query(selector: Value, tool: &str, arguments: Value) -> Value {
        json!({"project": selector, "tool": tool, "arguments": arguments})
    }

    fn assert_invalid_args(result: Result<Value, ToolError>) {
        assert!(
            matches!(result, Err(ToolError::InvalidArgs(_))),
            "{result:?}"
        );
    }

    fn assert_application(
        result: Result<Value, ToolError>,
        expected_code: i32,
        expected_message: &str,
        expected_data: Value,
    ) {
        match result.expect_err("runtime failure must be an application error") {
            ToolError::Application {
                code,
                message,
                data,
            } => {
                assert_eq!(code, expected_code);
                assert_eq!(message, expected_message);
                assert_eq!(data, Some(expected_data));
            }
            other => panic!("expected ToolError::Application, got {other:?}"),
        }
    }

    fn identity(id: &str, name: &str) -> ProjectIdentity {
        ProjectIdentity {
            id: ProjectId::new(id.to_owned()).unwrap(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn project_selector_resolves_exactly_one_configured_project() {
        let fixture = Fixture::new();
        let connector = RecordingConnector::default();
        let mut registry = fixture.registry(connector.clone());

        let tools = registry.list();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tower_list_projects", "tower_query_project"]
        );
        assert_eq!(
            tools[0].input_schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            tools[1].input_schema,
            json!({
                "type": "object",
                "properties": {
                    "project": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "name": {"type": "string"},
                            "path": {"type": "string"}
                        }
                    },
                    "tool": {"type": "string"},
                    "arguments": {"type": "object"}
                },
                "required": ["project", "tool", "arguments"]
            })
        );
        assert_eq!(
            registry
                .call("tower_list_projects", json!({}), &NeverCancelled)
                .unwrap(),
            json!({
                "projects": [
                    {"id": "alpha-id", "name": "Alpha", "path": fixture.alpha},
                    {"id": "beta-id", "name": "Beta", "path": fixture.beta}
                ]
            })
        );

        fs::create_dir(fixture.alpha.join("child")).unwrap();
        #[cfg(unix)]
        let path_selector = {
            let alias = fixture._temp.path().join("alpha-alias");
            std::os::unix::fs::symlink(&fixture.alpha, &alias).unwrap();
            alias.join("child/..")
        };
        #[cfg(not(unix))]
        let path_selector = fixture.alpha.join("child/..");

        for selector in [
            json!({"id": "alpha-id"}),
            json!({"name": "Alpha"}),
            json!({"path": fixture.alpha}),
            // Relative selectors are rooted at the active workspace, not the
            // process current directory. `alpha` is a sibling of `active`.
            json!({"path": "../alpha"}),
            json!({"path": path_selector}),
        ] {
            assert_eq!(
                registry
                    .call(
                        "tower_query_project",
                        query(selector, "tower_read_file", json!({"path": "src/lib.rs"})),
                        &NeverCancelled,
                    )
                    .unwrap(),
                json!({"project": {"id": "alpha-id", "name": "Alpha"}, "result": {"matches": []}})
            );
        }

        let calls = connector.calls();
        assert_eq!(calls.len(), 5);
        assert!(
            calls
                .iter()
                .all(|call| call.project.id.as_str() == "alpha-id")
        );
        assert!(
            calls
                .iter()
                .all(|call| call.availability_timeout == Duration::from_secs(7))
        );
    }

    #[test]
    fn project_selector_rejects_empty_multiple_unknown_and_outside_root_values() {
        let fixture = Fixture::new();
        let connector = RecordingConnector::default();
        let mut registry = fixture.registry(connector.clone());

        let invalid_requests = [
            query(json!({}), "tower_read_file", json!({"path": "src/lib.rs"})),
            query(
                json!({"id": "alpha-id", "name": "Alpha"}),
                "tower_read_file",
                json!({"path": "src/lib.rs"}),
            ),
            query(json!({"id": "unknown"}), "tower_read_file", json!({})),
            query(
                json!({"path": fixture._temp.path().join("outside")}),
                "tower_read_file",
                json!({}),
            ),
            query(
                json!({"path": fixture.alpha.join("child")}),
                "tower_read_file",
                json!({}),
            ),
            query(
                json!({"path": fixture.alpha.parent().unwrap()}),
                "tower_read_file",
                json!({}),
            ),
            query(json!({"id": "alpha-id"}), "", json!({})),
            query(json!({"id": "alpha-id"}), "tower_read_file", json!([])),
        ];
        for request in invalid_requests {
            assert_invalid_args(registry.call("tower_query_project", request, &NeverCancelled));
        }

        let active_config = format!(
            "[[projects.queryable]]\nid = \"active-id\"\nname = \"Active\"\npath = \"{}\"\n",
            fixture.active.display()
        );
        fs::write(fixture.active.join(".tower/config.toml"), active_config).unwrap();
        let active_projects = load(&fixture.active).unwrap().projects.queryable().clone();
        let mut active_registry = ProjectQueryRegistry::new(
            fixture.active.clone(),
            active_projects,
            Duration::from_secs(7),
            Arc::new(connector.clone()),
        );
        for selector in [
            json!({"id": "active-id"}),
            json!({"name": "Active"}),
            json!({"path": fixture.active}),
        ] {
            assert_invalid_args(active_registry.call(
                "tower_query_project",
                query(selector, "tower_read_file", json!({})),
                &NeverCancelled,
            ));
        }

        #[cfg(unix)]
        {
            let active_alias = fixture._temp.path().join("active-alias");
            std::os::unix::fs::symlink(&fixture.active, &active_alias).unwrap();
            assert_invalid_args(active_registry.call(
                "tower_query_project",
                query(json!({"path": active_alias}), "tower_read_file", json!({})),
                &NeverCancelled,
            ));
        }

        // The original selector does not exist even though collapsing its
        // trailing `missing/..` components names a configured resolved root.
        // A NotFound selector must not gain a canonical match after lexical
        // normalization.
        assert_invalid_args(registry.call(
            "tower_query_project",
            query(
                json!({"path": fixture.alpha.join("missing/..")} ),
                "tower_read_file",
                json!({}),
            ),
            &NeverCancelled,
        ));

        let unresolved_root = fixture._temp.path().join("unresolved-active");
        fs::create_dir_all(unresolved_root.join(".tower")).unwrap();
        fs::write(
            unresolved_root.join(".tower/config.toml"),
            "[[projects.queryable]]\nid = \"offline\"\nname = \"Offline\"\npath = \"future\"\n",
        )
        .unwrap();
        let unresolved = load(&unresolved_root).unwrap().projects.queryable().clone();
        // The configuration snapshot captured `future` while it was absent.
        // Creating it later must not make that entry resolved for this request.
        fs::create_dir(unresolved_root.join("future")).unwrap();
        let unresolved_connector =
            RecordingConnector::with_responses([Err(ExternalDaemonError::AvailabilityTimeout {
                project: identity("unconfigured-sentinel", "Unconfigured sentinel"),
            })]);
        let mut unresolved_registry = ProjectQueryRegistry::new(
            unresolved_root.clone(),
            unresolved,
            Duration::from_secs(7),
            Arc::new(unresolved_connector.clone()),
        );
        assert_application(
            unresolved_registry.call(
                "tower_query_project",
                query(json!({"path": "future"}), "tower_read_file", json!({})),
                &NeverCancelled,
            ),
            EXTERNAL_PROJECT_UNAVAILABLE,
            "external project is unavailable",
            json!({
                "code": "external_project_unavailable",
                "project": {"id": "offline", "name": "Offline"},
                "operation": "tower_read_file"
            }),
        );
        let unresolved_calls = unresolved_connector.calls();
        assert_eq!(unresolved_calls.len(), 1);
        assert_eq!(unresolved_calls[0].project.id.as_str(), "offline");
        assert_eq!(unresolved_calls[0].project.canonical_path, None);

        #[cfg(unix)]
        {
            let loop_path = fixture._temp.path().join("selector-loop");
            std::os::unix::fs::symlink("selector-loop", &loop_path).unwrap();
            assert_invalid_args(registry.call(
                "tower_query_project",
                query(json!({"path": loop_path}), "tower_read_file", json!({})),
                &NeverCancelled,
            ));
        }

        #[cfg(unix)]
        {
            let ambiguous_root = fixture._temp.path().join("ambiguous-active");
            let canonical_target = fixture._temp.path().join("canonical-target");
            fs::create_dir_all(ambiguous_root.join(".tower")).unwrap();
            fs::create_dir_all(&canonical_target).unwrap();
            fs::write(
                ambiguous_root.join(".tower/config.toml"),
                format!(
                    "[[projects.queryable]]\nid = \"lexical\"\nname = \"Lexical\"\npath = \"future-alias\"\n\
                     [[projects.queryable]]\nid = \"canonical\"\nname = \"Canonical\"\npath = \"{}\"\n",
                    canonical_target.display()
                ),
            )
            .unwrap();
            let ambiguous_projects = load(&ambiguous_root).unwrap().projects.queryable().clone();
            std::os::unix::fs::symlink(&canonical_target, ambiguous_root.join("future-alias"))
                .unwrap();
            let mut ambiguous_registry = ProjectQueryRegistry::new(
                ambiguous_root,
                ambiguous_projects,
                Duration::from_secs(7),
                Arc::new(connector.clone()),
            );
            assert_invalid_args(ambiguous_registry.call(
                "tower_query_project",
                query(
                    json!({"path": "future-alias"}),
                    "tower_read_file",
                    json!({}),
                ),
                &NeverCancelled,
            ));
        }

        assert!(connector.calls().is_empty());
    }

    #[test]
    fn external_query_uses_one_configuration_snapshot() {
        let fixture = Fixture::new();
        let connector = RecordingConnector::with_responses([
            Ok(json!({"content": "old snapshot"})),
            Err(ExternalDaemonError::TargetCall {
                project: identity("unconfigured-sentinel", "Unconfigured sentinel"),
                operation: "wrong-operation-sentinel".to_owned(),
                message: "PRIVATE_RAW_TARGET_SENTINEL".to_owned(),
            }),
        ]);
        let mut registry = fixture.registry(connector.clone());

        fs::write(
            fixture.active.join(".tower/config.toml"),
            format!(
                "[[projects.queryable]]\nid = \"replacement\"\nname = \"Replacement\"\npath = \"{}\"\n",
                fixture.beta.display()
            ),
        )
        .unwrap();
        let reloaded = load(&fixture.active).unwrap();
        assert!(reloaded.projects.queryable().get_by_name("Alpha").is_none());

        assert_eq!(
            registry
                .call(
                    "tower_query_project",
                    query(
                        json!({"id": "alpha-id"}),
                        "tower_read_file",
                        json!({"path": "old.rs"})
                    ),
                    &NeverCancelled,
                )
                .unwrap(),
            json!({
                "project": {"id": "alpha-id", "name": "Alpha"},
                "result": {"content": "old snapshot"}
            })
        );
        assert_application(
            registry.call(
                "tower_query_project",
                query(
                    json!({"id": "alpha-id"}),
                    "tower_read_file",
                    json!({"path": "old.rs"}),
                ),
                &NeverCancelled,
            ),
            EXTERNAL_TARGET_FAILURE,
            "external project operation failed",
            json!({
                "code": "external_target_failure",
                "project": {"id": "alpha-id", "name": "Alpha"},
                "operation": "tower_read_file"
            }),
        );

        let calls = connector.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| call.project.id.as_str() == "alpha-id")
        );
        assert!(
            calls
                .iter()
                .all(|call| call.project.resolved_path == fixture.alpha)
        );
    }

    #[test]
    fn external_query_maps_target_result_with_project_identity() {
        let fixture = Fixture::new();
        let private_path = "/outside/PRIVATE_EXTERNAL_PATH_SENTINEL";
        let private_content = "PRIVATE_TARGET_CONTENT_SENTINEL";
        let connector = RecordingConnector::with_responses([
            Ok(json!({"matches": [{"path": "src/lib.rs", "line": private_content}]})),
            Ok(json!({"matches": []})),
            Ok(json!({"matches": [{"path": "src/lib.rs"}]})),
            Err(ExternalDaemonError::AvailabilityTimeout {
                project: identity("private-id", private_path),
            }),
            Err(ExternalDaemonError::Handshake {
                project: identity("private-id", private_path),
                source: std::io::Error::other(private_content),
            }),
            Err(ExternalDaemonError::Initialize {
                project: identity("private-id", private_path),
                message: private_content.to_owned(),
            }),
            Err(ExternalDaemonError::TargetCall {
                project: identity("private-id", private_path),
                operation: private_content.to_owned(),
                message: private_content.to_owned(),
            }),
            Err(ExternalDaemonError::Decode {
                project: identity("private-id", private_path),
                operation: private_content.to_owned(),
                message: private_content.to_owned(),
            }),
            Err(ExternalDaemonError::Cancelled {
                project: identity("private-id", private_path),
                operation: private_content.to_owned(),
            }),
        ]);
        let mut registry = fixture.registry(connector);
        let captured_events = CapturingEventSink::default();
        registry = registry.with_event_sink(Arc::new(captured_events.clone()));

        for (selector, expected) in [
            (
                json!({"id": "alpha-id"}),
                json!({
                    "project": {"id": "alpha-id", "name": "Alpha"},
                    "result": {"matches": [{"path": "src/lib.rs", "line": private_content}]}
                }),
            ),
            (
                json!({"id": "alpha-id"}),
                json!({"project": {"id": "alpha-id", "name": "Alpha"}, "result": {"matches": []}}),
            ),
            (
                json!({"id": "beta-id"}),
                json!({
                    "project": {"id": "beta-id", "name": "Beta"},
                    "result": {"matches": [{"path": "src/lib.rs"}]}
                }),
            ),
        ] {
            assert_eq!(
                registry
                    .call(
                        "tower_query_project",
                        query(selector, "tower_search_text", json!({"pattern": "needle"})),
                        &NeverCancelled,
                    )
                    .unwrap(),
                expected
            );
        }

        let failures = [
            (
                EXTERNAL_PROJECT_UNAVAILABLE,
                "external project is unavailable",
                "external_project_unavailable",
            ),
            (
                EXTERNAL_TARGET_FAILURE,
                "external project operation failed",
                "external_target_failure",
            ),
            (
                EXTERNAL_TARGET_FAILURE,
                "external project operation failed",
                "external_target_failure",
            ),
            (
                EXTERNAL_TARGET_FAILURE,
                "external project operation failed",
                "external_target_failure",
            ),
            (
                EXTERNAL_TARGET_RESPONSE_INVALID,
                "external project response is invalid",
                "external_target_response_invalid",
            ),
            (
                EXTERNAL_QUERY_CANCELLED,
                "external project query was cancelled",
                "external_query_cancelled",
            ),
        ];
        for (code, message, stable_code) in failures {
            let result = registry.call(
                "tower_query_project",
                query(
                    json!({"id": "alpha-id"}),
                    "tower_search_text",
                    json!({"pattern": "needle"}),
                ),
                &NeverCancelled,
            );
            assert_application(
                result,
                code,
                message,
                json!({
                    "code": stable_code,
                    "project": {"id": "alpha-id", "name": "Alpha"},
                    "operation": "tower_search_text"
                }),
            );
        }

        let captured = captured_events.captured();
        assert!(
            !captured.is_empty(),
            "failure events must cross the production sink seam"
        );
        for sentinel in [private_path, private_content, "private-id"] {
            assert!(
                !captured.contains(sentinel),
                "captured external-query events leaked {sentinel}: {captured}"
            );
        }
        assert!(captured.contains("project_id=alpha-id"));
        assert!(captured.contains("operation=tower_search_text"));
    }

    #[test]
    fn external_mutation_rejection_does_not_connect() {
        let fixture = Fixture::new();
        let connector = RecordingConnector::default();
        let mut registry = fixture.registry(connector.clone());

        for operation in [
            "tower_create_file",
            "tower_create_directory",
            "tower_delete_file",
            "tower_global_replace",
            "tower_edit_range",
            "tower_reindex",
            "tower_rename_path",
            "tower_safe_delete_symbol",
            "tower_query_project",
            "unclassified_extension_tool",
        ] {
            assert_application(
                registry.call(
                    "tower_query_project",
                    query(json!({"id": "alpha-id"}), operation, json!({})),
                    &NeverCancelled,
                ),
                EXTERNAL_READ_ONLY_BOUNDARY,
                "external project operation is not permitted",
                json!({
                    "code": "external_read_only_boundary",
                    "project": {"id": "alpha-id", "name": "Alpha"},
                    "operation": operation
                }),
            );
        }
        assert!(connector.calls().is_empty());
    }
}
