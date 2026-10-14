use super::process_binary::selected_binary;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::tempdir;

type PersistedEntries = Vec<(Vec<u8>, Vec<u8>)>;

#[derive(Debug, PartialEq, Eq)]
struct PersistedState(BTreeMap<String, PersistedEntries>);

fn run_bounded_daemon(binary: &Path, workspace: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(binary);
    command
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run selected tower daemon")
}

fn write_idle_config(workspace: &Path, secret: &str) {
    std::fs::create_dir_all(workspace.join(".tower")).unwrap();
    std::fs::write(
        workspace.join(".tower/config.toml"),
        format!("# {secret}\n[daemon]\nidle_timeout_secs = 1\n"),
    )
    .unwrap();
}

fn install_launch_sentinel(workspace: &Path, sentinel: &Path) {
    let extension = workspace.join("extensions/sentinel");
    std::fs::create_dir_all(&extension).unwrap();
    let script = extension.join("sentinel.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf launched >> '{}'\n", sentinel.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        extension.join("extension.toml"),
        "name = \"sentinel\"\nversion = \"0.1.0\"\ncommand = [\"sentinel.sh\"]\nactivation = \"eager\"\n[capabilities]\nrequired = []\n",
    )
    .unwrap();
}

fn assert_process_output_is_confidential(output: &Output, secret: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty(),
        "daemon startup wrote non-protocol stdout: {stdout}"
    );
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
}

fn assert_reconciliation_metrics_absent(output: &Output) {
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("tower: reconciliation-metrics "),
        "reconciliation telemetry must be disabled: {output:?}"
    );
}

/// Read the `paths` projection itself rather than rebuilding an in-memory
/// workspace. This makes the process fixture detect a partially-published
/// reconciliation transaction.
fn persisted_path_inventory(workspace: &Path) -> BTreeMap<String, (u32, u32)> {
    let db = sled::open(workspace.join(".tower/db")).expect("open persisted Sled database");
    let paths = db.open_tree("paths").expect("open persisted paths tree");
    paths
        .iter()
        .map(|entry| {
            let (path, encoded_id) = entry.expect("read persisted path entry");
            let encoded_id: [u8; 8] = encoded_id
                .as_ref()
                .try_into()
                .expect("persisted FileId must have eight bytes");
            let index = u32::from_be_bytes(encoded_id[..4].try_into().unwrap());
            let generation = u32::from_be_bytes(encoded_id[4..].try_into().unwrap());
            (
                String::from_utf8(path.to_vec()).expect("persisted path must be UTF-8"),
                (index, generation),
            )
        })
        .collect()
}

fn persisted_state(workspace: &Path) -> PersistedState {
    let db = sled::open(workspace.join(".tower/db")).expect("open persisted Sled database");
    PersistedState(
        ["files", "paths", "blobs", "meta", "index"]
            .into_iter()
            .map(|tree_name| {
                let tree = db.open_tree(tree_name).expect("open persisted tree");
                let entries = tree
                    .iter()
                    .map(|entry| {
                        let (key, value) = entry.expect("read persisted tree entry");
                        (key.to_vec(), value.to_vec())
                    })
                    .collect();
                (tree_name.to_owned(), entries)
            })
            .collect(),
    )
}

/// Reusable T008 process contract. T012 can call this helper with the same
/// explicitly provisioned binaries instead of duplicating lifecycle assertions.
pub(crate) fn assert_reconciliation_failure_contract_from_env() {
    assert_reconciliation_failure_contract(
        &selected_binary("TOWER_PRODUCTION_BIN"),
        &selected_binary("TOWER_TEST_BIN"),
    );
}

struct ReconciliationProcessFixture<'a> {
    production: &'a Path,
    test_hooks: &'a Path,
    workspace: tempfile::TempDir,
    unrelated: tempfile::TempDir,
    extensions: tempfile::TempDir,
    selector: PathBuf,
    extensions_dir: PathBuf,
    sentinel: PathBuf,
}

impl<'a> ReconciliationProcessFixture<'a> {
    const SECRET: &'static str = "F009_RECONCILIATION_SECRET_7cbbe8";
    const SOURCE_CONTENT_SENTINEL: &'static str = "F009_SOURCE_CONTENT_SENTINEL_repaired_body";

    fn new(production: &'a Path, test_hooks: &'a Path) -> Self {
        let workspace = tempdir().unwrap();
        let unrelated = tempdir().unwrap();
        let extensions = tempdir().unwrap();
        write_idle_config(workspace.path(), Self::SECRET);
        write_idle_config(unrelated.path(), Self::SECRET);
        std::fs::write(
            workspace.path().join("baseline.rs"),
            "pub fn baseline() {}\n",
        )
        .unwrap();
        let selector = workspace.path().canonicalize().unwrap();
        let extensions_dir = extensions.path().join("extensions");
        let sentinel = extensions.path().join("sidecar-launched");
        Self {
            production,
            test_hooks,
            workspace,
            unrelated,
            extensions,
            selector,
            extensions_dir,
            sentinel,
        }
    }

    fn hook_environment(&self, fail_before_commit: bool, metrics: bool) -> Vec<(&str, &str)> {
        let mut env = vec![
            ("TOWER_TEST_WORKSPACE", self.selector.to_str().unwrap()),
            (
                "TOWER_EXTENSIONS_DIR",
                self.extensions_dir.to_str().unwrap(),
            ),
        ];
        if fail_before_commit {
            env.push(("TOWER_TEST_FAIL_RECONCILIATION", "before_commit"));
        }
        if metrics {
            env.push(("TOWER_TEST_METRICS", "1"));
        }
        env
    }

    fn establish_baseline(&self) -> PersistedState {
        let baseline = run_bounded_daemon(self.production, self.workspace.path(), &[]);
        assert!(baseline.status.success(), "baseline failed: {baseline:?}");
        let prior_good_inventory = BTreeMap::from([("baseline.rs".to_owned(), (0, 0))]);
        assert_eq!(
            persisted_path_inventory(self.workspace.path()),
            prior_good_inventory,
            "baseline must establish the exact persisted inventory used for rollback"
        );
        persisted_state(self.workspace.path())
    }

    fn assert_invalid_selector_is_inert(&self) {
        let secret = Self::SECRET;
        let invalid_selector = self.workspace.path().join("selector-does-not-exist");
        let invalid_selector = invalid_selector.to_str().unwrap();
        let invalid_selector_env = [
            ("TOWER_TEST_WORKSPACE", invalid_selector),
            ("TOWER_TEST_FAIL_RECONCILIATION", "before_commit"),
            ("TOWER_TEST_METRICS", "1"),
        ];
        let invalid_selector_output = run_bounded_daemon(
            self.test_hooks,
            self.workspace.path(),
            &invalid_selector_env,
        );
        assert!(invalid_selector_output.status.success());
        assert_process_output_is_confidential(&invalid_selector_output, secret);
        assert_reconciliation_metrics_absent(&invalid_selector_output);
    }

    fn prepare_missing_file_and_sidecar(&self) {
        let secret = Self::SECRET;
        let source_content_sentinel = Self::SOURCE_CONTENT_SENTINEL;
        std::fs::write(
            self.workspace.path().join("repaired.rs"),
            format!(
                "pub fn repaired() {{ let _ = (\"{secret}\", \"{source_content_sentinel}\"); }}\n"
            ),
        )
        .unwrap();
        install_launch_sentinel(self.extensions.path(), &self.sentinel);
    }

    fn assert_failed_repair_preserves(&self, prior_good_state: &PersistedState) {
        let secret = Self::SECRET;
        let source_content_sentinel = Self::SOURCE_CONTENT_SENTINEL;
        let trigger = self.hook_environment(true, true);
        let failed = run_bounded_daemon(self.test_hooks, self.workspace.path(), &trigger);
        assert!(!failed.status.success());
        assert_process_output_is_confidential(&failed, secret);
        assert_process_output_is_confidential(&failed, source_content_sentinel);
        let failure_stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(failure_stderr.contains("apply_reconciliation_delta"));
        assert!(failure_stderr.contains(&self.workspace.path().display().to_string()));
        assert!(!self.sentinel.exists());
        assert_eq!(
            persisted_state(self.workspace.path()),
            *prior_good_state,
            "injected reconciliation failure must leave every persisted projection intact"
        );
    }

    fn assert_unrelated_workspace_is_unaffected(&self) {
        let secret = Self::SECRET;
        let trigger = self.hook_environment(true, true);
        let unrelated_output = run_bounded_daemon(self.test_hooks, self.unrelated.path(), &trigger);
        assert!(unrelated_output.status.success());
        assert_process_output_is_confidential(&unrelated_output, secret);
        assert_reconciliation_metrics_absent(&unrelated_output);
    }

    fn assert_first_clean_retry_repairs(&self, prior_good_state: &PersistedState) {
        let secret = Self::SECRET;
        let source_content_sentinel = Self::SOURCE_CONTENT_SENTINEL;
        let retry_env = self.hook_environment(false, true);
        let retry = run_bounded_daemon(self.test_hooks, self.workspace.path(), &retry_env);
        assert!(
            retry.status.success(),
            "first clean retry failed: {retry:?}"
        );
        assert_process_output_is_confidential(&retry, secret);
        assert_process_output_is_confidential(&retry, source_content_sentinel);
        let retry_stderr = String::from_utf8_lossy(&retry.stderr);
        assert!(retry_stderr.contains("tower: reconciled index — "));
        assert!(!retry_stderr.contains("tower: reconciled index — 0 added"));
        assert_eq!(
            retry_stderr
                .matches("tower: reconciliation-metrics ")
                .count(),
            1
        );
        assert_eq!(
            persisted_path_inventory(self.workspace.path()),
            BTreeMap::from([
                ("baseline.rs".to_owned(), (0, 0)),
                ("repaired.rs".to_owned(), (1, 0)),
            ]),
            "first clean retry must publish the exact repaired persisted inventory"
        );
        let repaired_state = persisted_state(self.workspace.path());
        assert_repaired_projections(&repaired_state, prior_good_state);
    }

    fn assert_unchanged_restart_has_zero_metrics(&self) {
        let secret = Self::SECRET;
        let retry_env = self.hook_environment(false, true);
        let unchanged = run_bounded_daemon(self.test_hooks, self.workspace.path(), &retry_env);
        assert!(unchanged.status.success());
        assert_process_output_is_confidential(&unchanged, secret);
        assert_eq!(
            String::from_utf8_lossy(&unchanged.stderr)
                .matches("tower: reconciliation-metrics {\"content_reads\":0,\"content_hashes\":0,\"file_record_rewrites\":0}")
                .count(),
            1
        );
    }

    fn assert_metrics_require_opt_in(&self) {
        let secret = Self::SECRET;
        let metrics_disabled_env = self.hook_environment(false, false);
        let metrics_disabled = run_bounded_daemon(
            self.test_hooks,
            self.workspace.path(),
            &metrics_disabled_env,
        );
        assert!(metrics_disabled.status.success());
        assert_process_output_is_confidential(&metrics_disabled, secret);
        assert_reconciliation_metrics_absent(&metrics_disabled);
    }

    fn assert_production_ignores_hooks(&self) {
        let secret = Self::SECRET;
        let trigger = self.hook_environment(true, true);
        let production_output =
            run_bounded_daemon(self.production, self.workspace.path(), &trigger);
        assert!(production_output.status.success());
        assert_process_output_is_confidential(&production_output, secret);
        assert_reconciliation_metrics_absent(&production_output);
    }
}

fn assert_repaired_projections(repaired_state: &PersistedState, prior_good_state: &PersistedState) {
    for projection in ["files", "paths", "meta", "index"] {
        assert_ne!(
            repaired_state.0[projection], prior_good_state.0[projection],
            "clean retry must update the {projection} projection"
        );
    }
    assert_eq!(
        repaired_state.0["blobs"], prior_good_state.0["blobs"],
        "path reconciliation must leave the content-addressed blob projection unchanged"
    );
    assert_eq!(repaired_state.0["files"].len(), 2);
    assert_eq!(repaired_state.0["paths"].len(), 2);
    let search_terms = repaired_state.0["index"]
        .iter()
        .map(|(key, _)| String::from_utf8(key.clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        search_terms,
        [
            "bas", "base", "basel", "baseli", "baselin", "baseline", "rep", "repa", "repai",
            "repair", "repaire", "repaired", "rs",
        ]
    );
    let metadata_keys = repaired_state.0["meta"]
        .iter()
        .map(|(key, _)| String::from_utf8(key.clone()).unwrap())
        .collect::<Vec<_>>();
    assert!(metadata_keys.iter().any(|key| key == "slot_generations"));
    assert!(metadata_keys.iter().any(|key| key == "free_slots"));
}

pub(crate) fn assert_reconciliation_failure_contract(production: &Path, test_hooks: &Path) {
    let fixture = ReconciliationProcessFixture::new(production, test_hooks);
    let prior_good_state = fixture.establish_baseline();
    fixture.assert_invalid_selector_is_inert();
    fixture.prepare_missing_file_and_sidecar();
    fixture.assert_failed_repair_preserves(&prior_good_state);
    fixture.assert_unrelated_workspace_is_unaffected();
    fixture.assert_first_clean_retry_repairs(&prior_good_state);
    fixture.assert_unchanged_restart_has_zero_metrics();
    fixture.assert_metrics_require_opt_in();
    fixture.assert_production_ignores_hooks();
}
