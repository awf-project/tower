use std::{collections::BTreeMap, sync::Arc};

use extension_protocol::{HostCall, WorkspaceReadFailure, WorkspaceReadResult};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    PortFuture, SymbolSpan, WorkspaceDocument, WorkspacePath, WorkspaceReadError, WorkspaceReadPort,
};

impl WorkspaceReadError {
    pub fn public_message(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid workspace path",
            Self::NotFound => "workspace file not found",
            Self::UnsupportedFileType => "unsupported workspace file type",
            Self::InvalidUtf8 => "workspace file is not valid UTF-8",
            Self::HostUnavailable => "workspace host is unavailable",
        }
    }
}

#[derive(Clone, Default)]
pub struct InMemoryWorkspaceReader {
    files: BTreeMap<String, Result<Vec<u8>, WorkspaceReadFailure>>,
}

impl InMemoryWorkspaceReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(mut self, path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        self.files.insert(path.into(), Ok(content.into()));
        self
    }

    pub fn with_failure(mut self, path: impl Into<String>, failure: WorkspaceReadFailure) -> Self {
        self.files.insert(path.into(), Err(failure));
        self
    }
}

impl WorkspaceReadPort for InMemoryWorkspaceReader {
    fn list_files<'a>(&'a self) -> PortFuture<'a, Result<Vec<WorkspacePath>, WorkspaceReadError>> {
        Box::pin(async move {
            Ok(self
                .files
                .keys()
                .filter(|path| supported_language(path).is_some())
                .filter_map(|path| WorkspacePath::new(path.clone()).ok())
                .collect())
        })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<WorkspaceDocument, WorkspaceReadError>> {
        Box::pin(async move {
            let language = validate_supported_path(path.as_str())?;
            let bytes = self
                .files
                .get(path.as_str())
                .ok_or(WorkspaceReadError::NotFound)?
                .as_ref()
                .map_err(|failure| map_failure(*failure))?;
            document(path, bytes.clone(), language)
        })
    }

    fn symbol_spans<'a>(
        &'a self,
        _path: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<Option<Vec<SymbolSpan>>, WorkspaceReadError>> {
        Box::pin(async { Ok(None) })
    }
}

pub trait WorkspaceHostCallTransport: Send + Sync {
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

impl<F> WorkspaceHostCallTransport for F
where
    F: Fn(&str, Value) -> Result<Value, String> + Send + Sync,
{
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self(method, params)
    }
}

#[derive(Clone)]
pub struct HostCallWorkspaceReadAdapter {
    transport: Arc<dyn WorkspaceHostCallTransport>,
}

impl HostCallWorkspaceReadAdapter {
    pub fn new(transport: Arc<dyn WorkspaceHostCallTransport>) -> Self {
        Self { transport }
    }
}

impl WorkspaceReadPort for HostCallWorkspaceReadAdapter {
    fn list_files<'a>(&'a self) -> PortFuture<'a, Result<Vec<WorkspacePath>, WorkspaceReadError>> {
        Box::pin(async move {
            let params = serde_json::to_value(HostCall::ListFiles)
                .map_err(|_| WorkspaceReadError::HostUnavailable)?;
            let value = self
                .transport
                .call("workspace/listFiles", params)
                .map_err(|_| WorkspaceReadError::HostUnavailable)?;
            let paths: Vec<String> =
                serde_json::from_value(value).map_err(|_| WorkspaceReadError::HostUnavailable)?;
            let mut paths: Vec<_> = paths
                .into_iter()
                .filter(|path| supported_language(path).is_some())
                .filter_map(|path| WorkspacePath::new(path).ok())
                .collect();
            paths.sort();
            paths.dedup();
            Ok(paths)
        })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<WorkspaceDocument, WorkspaceReadError>> {
        Box::pin(async move {
            let language = validate_supported_path(path.as_str())?;
            let params = serde_json::to_value(HostCall::ReadFile {
                path: path.as_str().to_owned(),
                strict: Some(true),
            })
            .map_err(|_| WorkspaceReadError::HostUnavailable)?;
            let value = self
                .transport
                .call("workspace/readFile", params)
                .map_err(|_| WorkspaceReadError::HostUnavailable)?;
            match serde_json::from_value(value).map_err(|_| WorkspaceReadError::HostUnavailable)? {
                WorkspaceReadResult::Ok { content } => {
                    document(path, content.into_bytes(), language)
                }
                WorkspaceReadResult::Error { kind } => Err(map_failure(kind)),
            }
        })
    }

    fn symbol_spans<'a>(
        &'a self,
        _path: &'a WorkspacePath,
    ) -> PortFuture<'a, Result<Option<Vec<SymbolSpan>>, WorkspaceReadError>> {
        Box::pin(async { Ok(None) })
    }
}

fn validate_supported_path(path: &str) -> Result<&'static str, WorkspaceReadError> {
    supported_language(path).ok_or(WorkspaceReadError::UnsupportedFileType)
}

fn supported_language(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1;
    [
        ("rs", "rust"),
        ("go", "go"),
        ("php", "php"),
        ("js", "javascript"),
        ("jsx", "javascript"),
        ("ts", "typescript"),
        ("tsx", "typescript"),
        ("py", "python"),
        ("c", "c"),
        ("h", "c"),
        ("cpp", "cpp"),
        ("hpp", "cpp"),
        ("java", "java"),
        ("kt", "kotlin"),
        ("swift", "swift"),
        ("rb", "ruby"),
        ("sh", "shell"),
        ("toml", "toml"),
        ("json", "json"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("md", "markdown"),
        ("txt", "text"),
    ]
    .into_iter()
    .find_map(|(candidate, language)| {
        extension
            .eq_ignore_ascii_case(candidate)
            .then_some(language)
    })
}

fn document(
    path: &WorkspacePath,
    bytes: Vec<u8>,
    language: &'static str,
) -> Result<WorkspaceDocument, WorkspaceReadError> {
    let source = String::from_utf8(bytes).map_err(|_| WorkspaceReadError::InvalidUtf8)?;
    if source.contains('\0') {
        return Err(WorkspaceReadError::UnsupportedFileType);
    }
    Ok(WorkspaceDocument {
        path: path.clone(),
        content_hash: Sha256::digest(source.as_bytes()).into(),
        source,
        language: language.to_owned(),
        symbol_spans: Vec::new(),
    })
}

fn map_failure(failure: WorkspaceReadFailure) -> WorkspaceReadError {
    match failure {
        WorkspaceReadFailure::InvalidPath => WorkspaceReadError::InvalidPath,
        WorkspaceReadFailure::NotFound => WorkspaceReadError::NotFound,
        WorkspaceReadFailure::UnsupportedFileType => WorkspaceReadError::UnsupportedFileType,
        WorkspaceReadFailure::InvalidUtf8 => WorkspaceReadError::InvalidUtf8,
        WorkspaceReadFailure::HostUnavailable => WorkspaceReadError::HostUnavailable,
    }
}
