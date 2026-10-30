//! Shared lexical identities for configured projects and runtime selectors.

use std::path::{Path, PathBuf};

/// Normalize an absolute path without filesystem access or symlink resolution.
/// Callers canonicalize the original spelling separately before collapsing `..`.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}
