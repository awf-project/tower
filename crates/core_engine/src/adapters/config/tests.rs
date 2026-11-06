use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::tempdir;

use super::{
    AgentMemoryConfigurationReason, AgentMemoryPersistenceMode, AgentMemoryStartupPolicy,
    ConfigError, load,
};
use crate::adapters::cli::GlobalOpts;
use crate::adapters::daemon::engine::{
    AgentMemoryProviderOutcome, AgentMemoryProviderStep, AgentMemoryRuntime,
};
use crate::adapters::daemon::engine::{build_engine, host_memory_configuration};
use crate::ports::{
    AgentMemoryGetRequest, AgentMemoryGetResult, AgentMemoryNamespace, AgentMemoryScanRequest,
    AgentMemoryScanResult, AgentMemoryStorageError, AgentMemoryStoragePort,
    AgentMemoryTransactionOutcome, AgentMemoryTransactionRequest,
};

fn write_config(root: &Path, body: &str) {
    let directory = root.join(".tower");
    fs::create_dir_all(&directory).expect("create configuration directory");
    fs::write(directory.join("config.toml"), body).expect("write configuration");
}

fn assert_agent_memory_error(
    body: &str,
    expected_field: &'static str,
    expected_reason: AgentMemoryConfigurationReason,
) {
    let workspace = tempdir().expect("create workspace");
    let eager_sidecar_marker = workspace.path().join("eager-sidecar-started");
    let extensions = workspace.path().join("extensions");
    write_capturing_eager_extension(&extensions, &eager_sidecar_marker);
    write_config(workspace.path(), body);
    match load(workspace.path()).expect_err("invalid agent-memory configuration must fail") {
        ConfigError::InvalidAgentMemoryConfiguration { field, reason } => {
            assert_eq!(field, expected_field);
            assert_eq!(reason, expected_reason);
        }
        other => panic!("expected InvalidAgentMemoryConfiguration, got {other:?}"),
    }
    assert!(
        !eager_sidecar_marker.exists(),
        "an invalid configuration must be rejected before eager sidecar startup"
    );
}

#[test]
fn agent_memory_config_rejects_unknown_zero_and_inconsistent_bounds() {
    let defaults = tempdir().expect("create defaults workspace");
    let config = load(defaults.path()).expect("agent-memory defaults must load");
    let limits = config.agent_memory.limits;
    assert_eq!(
        config.agent_memory.storage.startup_policy,
        AgentMemoryStartupPolicy::Fail
    );
    assert_eq!(limits.max_title_bytes, 1_024);
    assert_eq!(limits.max_body_bytes, 65_536);
    assert_eq!(limits.max_metadata_items, 100);
    assert_eq!(limits.max_metadata_string_bytes, 1_024);
    assert_eq!(limits.max_batch_items, 100);
    assert_eq!(limits.default_results, 20);
    assert_eq!(limits.max_results, 100);
    assert_eq!(limits.default_response_bytes, 32_768);
    assert_eq!(limits.max_response_bytes, 131_072);
    assert_eq!(limits.max_query_bytes, 4_096);

    for (field, assignment, reason) in [
        (
            "agent_memory.limits.max_title_bytes",
            "max_title_bytes=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_title_bytes",
            "max_title_bytes=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_body_bytes",
            "max_body_bytes=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_body_bytes",
            "max_body_bytes=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_metadata_items",
            "max_metadata_items=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_metadata_items",
            "max_metadata_items=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_metadata_string_bytes",
            "max_metadata_string_bytes=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_metadata_string_bytes",
            "max_metadata_string_bytes=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_batch_items",
            "max_batch_items=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_batch_items",
            "max_batch_items=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.default_results",
            "default_results=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.default_results",
            "default_results=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_results",
            "max_results=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_results",
            "max_results=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.default_response_bytes",
            "default_response_bytes=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.default_response_bytes",
            "default_response_bytes=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_response_bytes",
            "max_response_bytes=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_response_bytes",
            "max_response_bytes=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
        (
            "agent_memory.limits.max_query_bytes",
            "max_query_bytes=0",
            AgentMemoryConfigurationReason::Zero,
        ),
        (
            "agent_memory.limits.max_query_bytes",
            "max_query_bytes=16777217",
            AgentMemoryConfigurationReason::OutOfRange,
        ),
    ] {
        assert_agent_memory_error(
            &format!("[agent_memory.limits]\n{assignment}\n"),
            field,
            reason,
        );
    }
    for (body, field) in [
        (
            "[agent_memory.limits]\ndefault_results=21\nmax_results=20\n",
            "agent_memory.limits.default_results",
        ),
        (
            "[agent_memory.limits]\nmax_results=101\nmax_batch_items=100\n",
            "agent_memory.limits.max_results",
        ),
        (
            "[agent_memory.limits]\ndefault_response_bytes=9\nmax_response_bytes=8\n",
            "agent_memory.limits.default_response_bytes",
        ),
    ] {
        assert_agent_memory_error(
            body,
            field,
            AgentMemoryConfigurationReason::InconsistentBounds,
        );
    }

    for body in [
        "[agent_memory]\nunknown=true\n",
        "[agent_memory.storage]\nunknown=true\n",
        "[agent_memory.limits]\nunknown=1\n",
        "[agent_memory.privacy]\nunknown=true\n",
        "[[agent_memory.privacy.rules]]\nid=\"rule\"\npattern=\"secret\"\naction=\"reject\"\nunknown=true\n",
        "[agent_memory.provider]\nname=\"local\"\nunknown=true\n",
    ] {
        let workspace = tempdir().expect("create unknown-field workspace");
        let marker = workspace.path().join("eager-sidecar-started");
        write_capturing_eager_extension(&workspace.path().join("extensions"), &marker);
        write_config(workspace.path(), body);
        assert!(
            matches!(load(workspace.path()), Err(ConfigError::Parse { .. })),
            "unknown fields must use the strict parse error: {body}"
        );
        assert!(
            !marker.exists(),
            "strict parsing must precede eager sidecar startup"
        );
    }
}

#[derive(Default)]
struct FailingRuntimeStorage;

impl AgentMemoryStoragePort for FailingRuntimeStorage {
    fn get(
        &self,
        _namespace: &AgentMemoryNamespace,
        _request: AgentMemoryGetRequest,
    ) -> Result<AgentMemoryGetResult, AgentMemoryStorageError> {
        Err(AgentMemoryStorageError::Unavailable)
    }

    fn scan(
        &self,
        _namespace: &AgentMemoryNamespace,
        _request: AgentMemoryScanRequest,
    ) -> Result<AgentMemoryScanResult, AgentMemoryStorageError> {
        Err(AgentMemoryStorageError::Unavailable)
    }

    fn transact(
        &self,
        _namespace: &AgentMemoryNamespace,
        _request: AgentMemoryTransactionRequest,
    ) -> Result<AgentMemoryTransactionOutcome, AgentMemoryStorageError> {
        Err(AgentMemoryStorageError::Unavailable)
    }
}

#[test]
fn agent_memory_config_rejects_invalid_privacy_rules() {
    let cases = [
        (
            "id=\"\"\npattern=\"secret\"\naction=\"reject\"",
            "agent_memory.privacy.rules[0].id",
        ),
        (
            "id=\"rule\"\npattern=\"(\"\naction=\"reject\"",
            "agent_memory.privacy.rules[0].pattern",
        ),
        (
            "id=\"rule\"\npattern=\"secret\"\naction=\"redact\"",
            "agent_memory.privacy.rules[0].replacement",
        ),
        (
            "id=\"rule\"\npattern=\"secret\"\naction=\"reject\"\nreplacement=\"[x]\"",
            "agent_memory.privacy.rules[0].replacement",
        ),
    ];
    for (rule, field) in cases {
        assert_agent_memory_error(
            &format!("[[agent_memory.privacy.rules]]\n{rule}\n"),
            field,
            AgentMemoryConfigurationReason::InvalidPrivacyRule,
        );
    }
    assert_agent_memory_error(
        "[[agent_memory.privacy.rules]]\nid=\"same\"\npattern=\"a\"\naction=\"reject\"\n[[agent_memory.privacy.rules]]\nid=\"same\"\npattern=\"b\"\naction=\"reject\"\n",
        "agent_memory.privacy.rules[1].id",
        AgentMemoryConfigurationReason::InvalidPrivacyRule,
    );
}

#[test]
fn agent_memory_provider_timeout_is_optional_and_finite_when_set() {
    let absent = tempdir().expect("create provider-free workspace");
    let absent_config = load(absent.path())
        .expect("provider may be absent")
        .agent_memory;
    assert!(absent_config.provider.is_none());
    let (storage, init) = host_memory_configuration(absent.path(), &absent_config)
        .expect("provider-free baseline storage starts");
    let runtime = AgentMemoryRuntime::new(storage, init);
    assert_eq!(
        runtime.run_provider_operation::<(), ()>(|_| panic!("absent provider must not be called")),
        AgentMemoryProviderOutcome::NotConfigured
    );

    let defaulted = tempdir().expect("create defaulted-provider workspace");
    write_config(
        defaulted.path(),
        "[agent_memory.provider]\nname=\"local\"\n",
    );
    assert_eq!(
        load(defaulted.path())
            .expect("provider timeout defaults")
            .agent_memory
            .provider
            .expect("provider")
            .timeout_ms,
        5_000
    );

    for timeout_ms in [1_u64, 86_400_000] {
        let workspace = tempdir().expect("create bounded-provider workspace");
        write_config(
            workspace.path(),
            &format!("[agent_memory.provider]\nname=\"local\"\ntimeout_ms={timeout_ms}\n"),
        );
        assert_eq!(
            load(workspace.path())
                .expect("valid timeout bound")
                .agent_memory
                .provider
                .expect("provider")
                .timeout_ms,
            timeout_ms
        );
    }

    for value in ["null", "0", "86400001"] {
        assert_agent_memory_error(
            &format!("[agent_memory.provider]\nname=\"local\"\ntimeout_ms={value}\n"),
            "agent_memory.provider.timeout_ms",
            AgentMemoryConfigurationReason::InvalidProviderTimeout,
        );
    }
    for assignment in ["\"timeout_ms\" = null", "timeout_ms = null # explicit null"] {
        assert_agent_memory_error(
            &format!("[agent_memory.provider]\nname=\"local\"\n{assignment}\n"),
            "agent_memory.provider.timeout_ms",
            AgentMemoryConfigurationReason::InvalidProviderTimeout,
        );
    }
}

#[test]
fn agent_memory_provider_retries_share_one_deadline_and_leave_baseline_available() {
    let workspace = tempdir().expect("create provider workspace");
    write_config(
        workspace.path(),
        "[agent_memory.provider]\nname=\"local\"\ntimeout_ms=5\n",
    );
    let config = load(workspace.path())
        .expect("provider configuration")
        .agent_memory;
    let (storage, init) =
        host_memory_configuration(workspace.path(), &config).expect("start durable baseline");
    let runtime = AgentMemoryRuntime::new(storage, init);
    let started = Instant::now();
    let ticks = [
        started,
        started,
        started + Duration::from_millis(2),
        started + Duration::from_millis(5),
    ];
    let mut tick = 0;
    let mut deadlines = Vec::new();
    let outcome = runtime.run_provider_operation_with_clock(
        || {
            let now = ticks[tick];
            tick += 1;
            now
        },
        &mut |deadline| {
            deadlines.push(deadline);
            AgentMemoryProviderStep::<(), ()>::Retry
        },
    );
    assert_eq!(outcome, AgentMemoryProviderOutcome::TimedOut);
    assert_eq!(deadlines, vec![started + Duration::from_millis(5); 2]);

    let namespace = AgentMemoryNamespace::from_trusted_identity(b"project", b"agent_memory")
        .expect("trusted namespace");
    assert_eq!(
        runtime
            .get(
                &namespace,
                AgentMemoryGetRequest {
                    key: b"baseline".to_vec()
                }
            )
            .expect("provider expiry must not block baseline storage"),
        AgentMemoryGetResult {
            value: None,
            generation: 0,
        }
    );
    assert_eq!(
        runtime.initialization().persistence_mode,
        AgentMemoryPersistenceMode::Durable
    );
}

#[test]
fn agent_memory_runtime_storage_failure_preserves_selected_mode_and_store() {
    let workspace = tempdir().expect("create runtime-failure workspace");
    let config = load(workspace.path())
        .expect("default configuration")
        .agent_memory;
    let (_, init) =
        host_memory_configuration(workspace.path(), &config).expect("select durable mode");
    let schema = workspace.path().join(".tower/agent_memory/schema");
    let before = fs::read(&schema).expect("read compatible schema marker");
    let runtime = AgentMemoryRuntime::new(Arc::new(FailingRuntimeStorage), init);
    let namespace = AgentMemoryNamespace::from_trusted_identity(b"project", b"agent_memory")
        .expect("trusted namespace");

    assert_eq!(
        runtime.get(&namespace, AgentMemoryGetRequest { key: vec![1] }),
        Err(AgentMemoryStorageError::Unavailable)
    );
    assert_eq!(
        runtime.initialization().persistence_mode,
        AgentMemoryPersistenceMode::Durable,
        "runtime failure must not switch to volatile storage"
    );
    assert!(!runtime.initialization().durable_data_unavailable);
    assert_eq!(
        fs::read(schema).expect("runtime failure leaves durable store intact"),
        before
    );
}

#[test]
fn agent_memory_startup_policy_preserves_unavailable_corrupt_and_future_stores() {
    let workspace = tempdir().expect("create durable workspace");
    let config = load(workspace.path())
        .expect("default memory configuration")
        .agent_memory;
    let (_, initialized) = host_memory_configuration(workspace.path(), &config)
        .expect("an empty durable store initializes");
    assert_eq!(
        initialized.persistence_mode,
        AgentMemoryPersistenceMode::Durable
    );
    assert!(!initialized.durable_data_unavailable);

    let unavailable = tempdir().expect("create unavailable-store workspace");
    fs::create_dir_all(unavailable.path().join(".tower")).expect("create tower directory");
    fs::write(
        unavailable.path().join(".tower/agent_memory"),
        b"not a directory",
    )
    .expect("block the durable store root");

    let fail = load(unavailable.path())
        .expect("default policy parses")
        .agent_memory;
    assert!(matches!(
        host_memory_configuration(unavailable.path(), &fail),
        Err(ConfigError::InvalidAgentMemoryConfiguration {
            field: "agent_memory.storage.startup_policy",
            reason: AgentMemoryConfigurationReason::IncompatibleStartupPolicy,
        })
    ));

    for (policy, expected_mode) in [
        (AgentMemoryStartupPolicy::ReadOnly, None),
        (
            AgentMemoryStartupPolicy::InMemory,
            Some(AgentMemoryPersistenceMode::InMemory),
        ),
    ] {
        let mut configured = fail.clone();
        configured.storage.startup_policy = policy;
        match expected_mode {
            None => assert!(matches!(
                host_memory_configuration(unavailable.path(), &configured),
                Err(ConfigError::InvalidAgentMemoryConfiguration {
                    field: "agent_memory.storage.startup_policy",
                    reason: AgentMemoryConfigurationReason::IncompatibleStartupPolicy,
                })
            )),
            Some(mode) => {
                let (_, initialized) = host_memory_configuration(unavailable.path(), &configured)
                    .expect("explicit volatile fallback is allowed");
                assert_eq!(initialized.persistence_mode, mode);
                assert!(initialized.durable_data_unavailable);
            }
        }
        assert_eq!(
            fs::read(unavailable.path().join(".tower/agent_memory")).unwrap(),
            b"not a directory"
        );
    }

    for (name, contents) in [
        ("corrupt", b"not a sled database".as_slice()),
        ("future", b"{\"version\":2}".as_slice()),
    ] {
        let fixture = tempdir().expect("create incompatible-store workspace");
        let root = fixture.path().join(".tower/agent_memory");
        fs::create_dir_all(&root).expect("create durable store fixture root");
        let state = root.join("schema");
        fs::write(&state, contents).expect("write incompatible durable store fixture");

        let config = load(fixture.path())
            .expect("default policy parses")
            .agent_memory;
        assert!(
            matches!(
                host_memory_configuration(fixture.path(), &config),
                Err(ConfigError::InvalidAgentMemoryConfiguration {
                    field: "agent_memory.storage.startup_policy",
                    reason: AgentMemoryConfigurationReason::IncompatibleStartupPolicy,
                })
            ),
            "{name} durable fixture must be rejected under fail policy"
        );

        let mut read_only = config.clone();
        read_only.storage.startup_policy = AgentMemoryStartupPolicy::ReadOnly;
        assert!(
            matches!(
                host_memory_configuration(fixture.path(), &read_only),
                Err(ConfigError::InvalidAgentMemoryConfiguration {
                    field: "agent_memory.storage.startup_policy",
                    reason: AgentMemoryConfigurationReason::IncompatibleStartupPolicy,
                })
            ),
            "{name} durable fixture must be rejected under read-only policy"
        );

        let mut in_memory = config;
        in_memory.storage.startup_policy = AgentMemoryStartupPolicy::InMemory;
        let (_, initialized) = host_memory_configuration(fixture.path(), &in_memory)
            .expect("explicit volatile fallback is allowed");
        assert_eq!(
            initialized.persistence_mode,
            AgentMemoryPersistenceMode::InMemory
        );
        assert!(initialized.durable_data_unavailable);
        assert_eq!(
            fs::read(&state).expect("incompatible fixture remains present"),
            contents,
            "{name} durable fixture must not be overwritten or treated as empty"
        );
    }

    let compatible = tempdir().expect("create compatible-store workspace");
    let config = load(compatible.path())
        .expect("default memory configuration")
        .agent_memory;
    let (_, durable) = host_memory_configuration(compatible.path(), &config)
        .expect("empty durable store initializes consistently");
    assert_eq!(
        durable.persistence_mode,
        AgentMemoryPersistenceMode::Durable
    );
    let mut read_only = config;
    read_only.storage.startup_policy = AgentMemoryStartupPolicy::ReadOnly;
    let (_, initialized) = host_memory_configuration(compatible.path(), &read_only)
        .expect("a compatible consistent store supports read-only startup");
    assert_eq!(
        initialized.persistence_mode,
        AgentMemoryPersistenceMode::ReadOnly
    );
    assert!(!initialized.durable_data_unavailable);
}

#[test]
fn agent_memory_initialize_payload_contains_only_validated_values() {
    let workspace = tempdir().expect("create initialized workspace");
    write_config(
        workspace.path(),
        "[agent_memory.limits]\nmax_title_bytes=64\ndefault_results=2\nmax_results=3\nmax_batch_items=4\ndefault_response_bytes=1024\nmax_response_bytes=2048\n\n[[agent_memory.privacy.rules]]\nid=\"token\"\npattern=\"token-[0-9]+\"\naction=\"redact\"\nreplacement=\"[token]\"\n\n[agent_memory.provider]\nname=\"local\"\n",
    );
    let extensions = workspace.path().join("extensions");
    let marker = workspace.path().join("agent-memory-init.json");
    write_capturing_eager_extension(&extensions, &marker);
    let engine = build_engine(
        &GlobalOpts {
            workspace_dir: Some(workspace.path().to_path_buf()),
            extensions_dir: Some(extensions),
        },
        load(workspace.path()).expect("validated configuration reaches public assembly"),
    )
    .expect("public assembly initializes the eager sidecar");
    drop(engine);

    let initialize = fs::read_to_string(&marker).expect("capturing sidecar received initialize");
    let initialize: serde_json::Value =
        serde_json::from_str(&initialize).expect("captured initialize is JSON-RPC");
    let payload = initialize["params"]["extension_config"].clone();
    let object = payload.as_object().expect("payload object");
    assert_eq!(
        object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "durable_data_unavailable",
            "limits",
            "persistence_mode",
            "privacy",
            "project_key",
            "provider",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(payload["limits"]["max_title_bytes"], 64);
    assert_eq!(payload["limits"]["default_results"], 2);
    assert_eq!(payload["limits"]["max_results"], 3);
    assert_eq!(payload["provider"]["timeout_ms"], 5_000);
    assert_eq!(payload["privacy"]["rules"][0]["replacement"], "[token]");
    assert!(
        payload["project_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
    assert!(!payload.to_string().contains("config.toml"));
    assert!(!payload.to_string().contains("agent_memory"));
}

#[test]
fn agent_memory_daemon_payload_overrides_untrusted_extension_initialization() {
    let workspace = tempdir().expect("create daemon configuration workspace");
    write_config(
        workspace.path(),
        "[extensions.initialize.agent_memory]\nproject_key=\"untrusted\"\n\n[agent_memory.provider]\nname=\"local\"\n",
    );
    let mut config = load(workspace.path()).expect("load validated daemon configuration");
    let (_, init) = host_memory_configuration(workspace.path(), &config.agent_memory)
        .expect("prepare host-owned memory configuration");

    super::super::daemon::engine::install_agent_memory_init(&mut config, &init)
        .expect("compose daemon extension initialization");

    let payload = config
        .extensions
        .initialize
        .get("agent_memory")
        .expect("daemon configuration contains agent-memory initialization");
    assert_ne!(
        payload
            .get("project_key")
            .and_then(serde_json::Value::as_str),
        Some("untrusted")
    );
    assert_eq!(
        payload
            .get("provider")
            .and_then(|provider| provider.get("timeout_ms"))
            .and_then(serde_json::Value::as_u64),
        Some(5_000)
    );
}

#[cfg(unix)]
fn write_capturing_eager_extension(extensions: &Path, marker: &Path) {
    let extension = extensions.join("agent_memory");
    fs::create_dir_all(&extension).expect("create capturing extension directory");
    let script = extension.join("capture-init.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nIFS= read -r initialize\nprintf '%s' \"$initialize\" > '{}'\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"type\":\"Initialized\",\"data\":{{\"tools\":[],\"events\":[],\"capabilities\":[]}}}}}}'\nIFS= read -r _shutdown\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"type\":\"Ack\"}}}}'\n",
            marker.display()
        ),
    )
    .expect("write capturing extension script");
    fs::write(
        extension.join("extension.toml"),
        format!(
            "name = \"agent_memory\"\nversion = \"0.1.0\"\ncommand = [\"/bin/sh\", \"{}\"]\nactivation = \"eager\"\n",
            script.display()
        ),
    )
    .expect("write capturing extension manifest");
}
