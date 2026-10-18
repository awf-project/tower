use std::path::{Component, Path, PathBuf};

use ignore::gitignore::GitignoreBuilder;
use ignore::{Match, Walk, WalkBuilder};

use crate::domain::RelativePath;
use crate::domain::mutation::is_tmp_artifact;
use crate::ports::{
    PathExclusionDecision, PathExclusionError, PathExclusionPort, PathExclusionReason,
};

pub const TOWERIGNORE_FILE_NAME: &str = ".towerignore";

pub struct WorkspacePathExclusion {
    root: PathBuf,
}

impl WorkspacePathExclusion {
    pub fn new(root: PathBuf) -> Result<Self, PathExclusionError> {
        let root = root
            .canonicalize()
            .map_err(|_| PathExclusionError::PolicyEvaluation)?;
        if !root.is_dir() {
            return Err(PathExclusionError::PolicyEvaluation);
        }
        Ok(Self { root })
    }

    pub(crate) fn walker(root: &Path) -> Walk {
        WalkBuilder::new(root)
            .follow_links(false)
            .require_git(false)
            .git_ignore(false)
            .git_exclude(false)
            .git_global(false)
            .add_custom_ignore_filename(TOWERIGNORE_FILE_NAME)
            .hidden(true)
            .build()
    }

    fn validate_containment(&self, path: &RelativePath) -> Result<PathBuf, PathExclusionError> {
        let relative = Path::new(path.as_str());
        for component in relative.components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(PathExclusionError::OutsideWorkspace);
                }
            }
        }

        let candidate = self.root.join(relative);
        let mut existing = candidate.as_path();
        while !existing.exists() {
            existing = existing
                .parent()
                .ok_or(PathExclusionError::OutsideWorkspace)?;
        }
        let canonical_existing = existing
            .canonicalize()
            .map_err(|_| PathExclusionError::PolicyEvaluation)?;
        if !canonical_existing.starts_with(&self.root) {
            return Err(PathExclusionError::SymlinkEscape);
        }
        Ok(candidate)
    }

    fn ignore_match(
        &self,
        candidate: &Path,
        is_dir: bool,
    ) -> Result<Match<()>, PathExclusionError> {
        let relative_parent = candidate
            .parent()
            .and_then(|parent| parent.strip_prefix(&self.root).ok())
            .ok_or(PathExclusionError::OutsideWorkspace)?;

        let mut directory = self.root.clone();
        let mut result = Match::None;
        result = match_ignore_file(&directory, candidate, is_dir, result)?;
        for component in relative_parent.components() {
            directory.push(component);
            result = match_ignore_file(&directory, candidate, is_dir, result)?;
        }
        Ok(result)
    }
}

fn match_ignore_file(
    directory: &Path,
    candidate: &Path,
    is_dir: bool,
    previous: Match<()>,
) -> Result<Match<()>, PathExclusionError> {
    let ignore_file = directory.join(TOWERIGNORE_FILE_NAME);
    if !ignore_file.is_file() {
        return Ok(previous);
    }
    let mut builder = GitignoreBuilder::new(directory);
    if builder.add(ignore_file).is_some() {
        return Err(PathExclusionError::PolicyEvaluation);
    }
    let matcher = builder
        .build()
        .map_err(|_| PathExclusionError::PolicyEvaluation)?;
    Ok(
        match matcher.matched_path_or_any_parents(candidate, is_dir) {
            Match::None => previous,
            matched => matched.map(|_| ()),
        },
    )
}

impl PathExclusionPort for WorkspacePathExclusion {
    fn classify(&self, path: &RelativePath) -> Result<PathExclusionDecision, PathExclusionError> {
        let candidate = self.validate_containment(path)?;
        let components: Vec<_> = path.as_str().split('/').collect();

        if components.contains(&".git") {
            return Ok(PathExclusionDecision::Excluded(
                PathExclusionReason::GitMetadata,
            ));
        }
        if is_tmp_artifact(path) {
            let reason = if path.as_str().ends_with(".tmp_write") {
                PathExclusionReason::AtomicWriteArtifact
            } else {
                PathExclusionReason::FormatterTemporary
            };
            return Ok(PathExclusionDecision::Excluded(reason));
        }
        if components
            .iter()
            .any(|component| component.starts_with('.') && *component != ".")
        {
            return Ok(PathExclusionDecision::Excluded(PathExclusionReason::Hidden));
        }
        let relative_parent = candidate
            .parent()
            .and_then(|parent| parent.strip_prefix(&self.root).ok())
            .ok_or(PathExclusionError::OutsideWorkspace)?;
        let mut ancestor = self.root.clone();
        for component in relative_parent.components() {
            ancestor.push(component);
            if self.ignore_match(&ancestor, true)?.is_ignore() {
                return Ok(PathExclusionDecision::Excluded(
                    PathExclusionReason::IgnoreRule,
                ));
            }
        }
        if self.ignore_match(&candidate, false)?.is_ignore() {
            return Ok(PathExclusionDecision::Excluded(
                PathExclusionReason::IgnoreRule,
            ));
        }
        Ok(PathExclusionDecision::Allowed)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::WorkspacePathExclusion;
    use crate::domain::RelativePath;
    use crate::ports::{PathExclusionDecision, PathExclusionError, PathExclusionPort};
    use crate::test_support::{PathExclusionFixture, path_exclusion_contract};

    fn materialize_fixture(root: &Path, fixture: PathExclusionFixture) {
        for directory in fixture.directories {
            fs::create_dir_all(root.join(directory.as_str())).unwrap();
        }
        for (path, content) in fixture.files {
            let path = root.join(path.as_str());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    #[test]
    fn workspace_path_exclusion_is_public_and_implements_the_port() {
        fn assert_port<T: PathExclusionPort>() {}

        assert_port::<crate::adapters::fs::WorkspacePathExclusion>();
        let root = TempDir::new().unwrap();
        let adapter =
            crate::adapters::fs::WorkspacePathExclusion::new(root.path().to_path_buf()).unwrap();
        assert_eq!(
            adapter.classify(&RelativePath::new("src/lib.rs")),
            Ok(PathExclusionDecision::Allowed)
        );
    }

    #[test]
    fn constructor_canonicalizes_the_root_and_errors_do_not_disclose_it() {
        const CAPTURE_LOGS_ENV: &str = "TOWER_PATH_EXCLUSION_CAPTURE_LOGS";
        const ROOT_ENV: &str = "TOWER_PATH_EXCLUSION_ROOT";
        const CONFIGURED_ROOT_ENV: &str = "TOWER_PATH_EXCLUSION_CONFIGURED_ROOT";
        const SYMLINK_TARGET_ENV: &str = "TOWER_PATH_EXCLUSION_SYMLINK_TARGET";

        if std::env::var_os(CAPTURE_LOGS_ENV).is_some() {
            let root = std::path::PathBuf::from(std::env::var_os(ROOT_ENV).unwrap());
            let configured_root =
                std::path::PathBuf::from(std::env::var_os(CONFIGURED_ROOT_ENV).unwrap());
            let adapter = WorkspacePathExclusion::new(configured_root.clone()).unwrap();
            #[cfg(unix)]
            fs::remove_file(configured_root).unwrap();

            assert_eq!(
                adapter.classify(&RelativePath::new("src/lib.rs")),
                Ok(PathExclusionDecision::Allowed)
            );
            assert_eq!(
                adapter.classify(&RelativePath::new("../outside.rs")),
                Err(PathExclusionError::OutsideWorkspace)
            );
            assert_eq!(
                adapter.classify(&RelativePath::new("missing/ancestor/new.rs")),
                Ok(PathExclusionDecision::Allowed)
            );
            #[cfg(unix)]
            assert_eq!(
                adapter.classify(&RelativePath::new("linked/secret.rs")),
                Err(PathExclusionError::SymlinkEscape)
            );

            // Keep the materialized paths alive for the complete adapter exercise.
            let _ = root;
            return;
        }

        let container = TempDir::new().unwrap();
        let root = container.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"fixture").unwrap();

        let symlink_target = container.path().join("external-secret-target");
        fs::create_dir(&symlink_target).unwrap();
        fs::write(symlink_target.join("secret.rs"), b"secret").unwrap();

        #[cfg(unix)]
        let configured_root = {
            use std::os::unix::fs::symlink;

            let alias = container.path().join("workspace-alias");
            symlink(&root, &alias).unwrap();
            alias
        };
        #[cfg(not(unix))]
        let configured_root = root.clone();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&symlink_target, root.join("linked")).unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "adapters::fs::path_exclusion::tests::constructor_canonicalizes_the_root_and_errors_do_not_disclose_it",
                "--nocapture",
            ])
            .env(CAPTURE_LOGS_ENV, "1")
            .env(ROOT_ENV, &root)
            .env(CONFIGURED_ROOT_ENV, &configured_root)
            .env(SYMLINK_TARGET_ENV, &symlink_target)
            .output()
            .unwrap();

        assert!(output.status.success());
        let captured_output = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!captured_output.contains(&fs::canonicalize(&root).unwrap().display().to_string()));
        assert!(!captured_output.contains(&symlink_target.display().to_string()));
    }

    #[test]
    fn workspace_path_exclusion_passes_the_complete_shared_contract() {
        let root = TempDir::new().unwrap();
        path_exclusion_contract(|fixture| {
            materialize_fixture(root.path(), fixture);
            Box::new(WorkspacePathExclusion::new(root.path().to_path_buf()).unwrap())
        });
    }

    #[cfg(unix)]
    #[test]
    fn workspace_path_exclusion_rejects_a_source_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.rs"), b"secret").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        let adapter = WorkspacePathExclusion::new(root.path().to_path_buf()).unwrap();

        assert_eq!(
            adapter.classify(&RelativePath::new("linked/secret.rs")),
            Err(PathExclusionError::SymlinkEscape)
        );
    }
}
