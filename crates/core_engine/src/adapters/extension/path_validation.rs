//! Path validation for extension capability callbacks (UN2).
//!
//! Extensions may only request access to workspace-relative paths. Absolute
//! paths, empty paths, and paths containing `..` components are denied. This
//! mirrors the WASM allow-list rules from spec 11c.

/// Validate a path supplied by an extension for `workspace/readFile` or
/// `workspace/requestFormat` capability calls.
///
/// # Rules
///
/// - Must not be empty.
/// - Must not start with `/` (absolute path).
/// - Must not equal `..`.
/// - Must not start with `../` (escapes workspace root).
/// - Must not contain `/../` (escapes workspace root mid-path).
/// - Must not end with `/..` (escapes workspace root at tail).
///
/// # Errors
///
/// Returns an error string suitable for a JSON-RPC error message on rejection.
///
/// # Examples
///
/// ```rust
/// use core_engine::adapters::extension::path_validation::validate_capability_path;
///
/// assert!(validate_capability_path("src/lib.rs").is_ok());
/// assert!(validate_capability_path("").is_err());
/// assert!(validate_capability_path("/etc/passwd").is_err());
/// assert!(validate_capability_path("../secret").is_err());
/// assert!(validate_capability_path("a/../../b").is_err());
/// ```
pub fn validate_capability_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path must not be empty".to_owned());
    }
    if path.starts_with('/') {
        return Err(format!("path must not be absolute: {path:?}"));
    }
    if path == ".." {
        return Err("path must not be '..'".to_owned());
    }
    if path.starts_with("../") {
        return Err(format!("path must not start with '../': {path:?}"));
    }
    if path.contains("/../") {
        return Err(format!("path must not contain '/../': {path:?}"));
    }
    if path.ends_with("/..") {
        return Err(format!("path must not end with '/..': {path:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_capability_path;

    #[test]
    fn valid_relative_paths_are_accepted() {
        assert!(validate_capability_path("src/lib.rs").is_ok());
        assert!(validate_capability_path("README.md").is_ok());
        assert!(validate_capability_path("a/b/c.rs").is_ok());
        assert!(validate_capability_path("a").is_ok());
    }

    #[test]
    fn empty_path_is_rejected() {
        assert!(validate_capability_path("").is_err());
    }

    #[test]
    fn absolute_path_is_rejected() {
        assert!(validate_capability_path("/etc/passwd").is_err());
        assert!(validate_capability_path("/").is_err());
        assert!(validate_capability_path("/src/lib.rs").is_err());
    }

    #[test]
    fn dotdot_traversal_variants_are_rejected() {
        assert!(validate_capability_path("..").is_err());
        assert!(validate_capability_path("../secret").is_err());
        assert!(validate_capability_path("a/../../b").is_err());
        assert!(validate_capability_path("a/b/..").is_err());
        assert!(validate_capability_path("a/../b").is_err());
    }
}
