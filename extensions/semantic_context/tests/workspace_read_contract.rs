use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use core_engine::adapters::{
    InMemoryAstIndex,
    extension::{HostDeps, SidecarHostAdapter, host_deps::FsAdapter},
    formatter::NoOpFormatQueue,
};
use extension_protocol::{Capability, HostCall, WorkspaceReadFailure};
use semantic_context_extension::{
    WorkspacePath, WorkspaceReadError, WorkspaceReadPort,
    adapters::workspace::{HostCallWorkspaceReadAdapter, InMemoryWorkspaceReader},
};
use serde_json::Value;

const VALID_PATH: &str = "src/lib.RS";
const INVALID_UTF8_PATH: &str = "src/invalid.rs";
const NUL_PATH: &str = "src/nul.rs";
const IO_PATH: &str = "src/io.rs";
const ESCAPE_PATH: &str = "src/link.rs";

async fn verify_workspace_read_contract(reader: &dyn WorkspaceReadPort) {
    let files = reader.list_files().await.expect("list supported files");
    assert!(files.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(files.iter().any(|path| path.as_str() == VALID_PATH));

    let valid = path(VALID_PATH);
    let document = reader.read_file(&valid).await.expect("read valid UTF-8");
    assert_eq!(document.path, valid);
    assert_eq!(document.source, "fn main() {}\n");
    assert_eq!(document.language, "rust");

    assert_eq!(
        reader.read_file(&path("src/missing.rs")).await,
        Err(WorkspaceReadError::NotFound)
    );
    assert_eq!(
        reader.read_file(&path("src/image.bin")).await,
        Err(WorkspaceReadError::UnsupportedFileType)
    );
    assert_eq!(
        reader.read_file(&path(INVALID_UTF8_PATH)).await,
        Err(WorkspaceReadError::InvalidUtf8)
    );
    assert_eq!(
        reader.read_file(&path(NUL_PATH)).await,
        Err(WorkspaceReadError::UnsupportedFileType)
    );
    assert_eq!(
        reader.read_file(&path(IO_PATH)).await,
        Err(WorkspaceReadError::HostUnavailable)
    );
    let public_message = WorkspaceReadError::HostUnavailable.public_message();
    for private in ["legacy read failed", IO_PATH, "private source sentinel"] {
        assert!(!public_message.contains(private));
    }
    assert_eq!(
        reader.read_file(&path(ESCAPE_PATH)).await,
        Err(WorkspaceReadError::InvalidPath)
    );

    let oversized = reader
        .read_file(&path("src/oversized.txt"))
        .await
        .expect("workspace readers do not impose the chunking size limit");
    assert_eq!(oversized.source.len(), 2_000_000);

    for invalid in [
        "",
        "/absolute.rs",
        "src/../escape.rs",
        "src//main.rs",
        "C:",
        "C:secret.rs",
        "C:/secret.rs",
        "z:secret.rs",
    ] {
        assert_eq!(
            WorkspacePath::new(invalid.to_owned()),
            Err(semantic_context_extension::SemanticDomainError::InvalidWorkspacePath)
        );
    }
}

fn memory_reader() -> InMemoryWorkspaceReader {
    InMemoryWorkspaceReader::new()
        .with_file(VALID_PATH, b"fn main() {}\n".to_vec())
        .with_file(INVALID_UTF8_PATH, vec![0xff])
        .with_file(NUL_PATH, b"hello\0world".to_vec())
        .with_failure(IO_PATH, WorkspaceReadFailure::HostUnavailable)
        .with_failure(ESCAPE_PATH, WorkspaceReadFailure::InvalidPath)
        .with_file("src/oversized.txt", vec![b'x'; 2_000_000])
        .with_file("src/image.bin", b"ignored".to_vec())
}

struct ContractFs {
    files: BTreeMap<String, Result<Vec<u8>, WorkspaceReadFailure>>,
    workspace_root: Option<PathBuf>,
}

#[cfg(unix)]
struct SymlinkSwapFs {
    workspace_root: PathBuf,
    link: PathBuf,
    outside: PathBuf,
}

#[cfg(unix)]
impl FsAdapter for SymlinkSwapFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.read_file_typed(path)
            .map_err(|failure| format!("legacy read failed: {failure:?}"))
    }

    fn read_file_typed(&self, _path: &str) -> Result<Vec<u8>, WorkspaceReadFailure> {
        std::fs::remove_file(&self.link).unwrap();
        std::os::unix::fs::symlink(&self.outside, &self.link).unwrap();
        std::fs::read(&self.link).map_err(|_| WorkspaceReadFailure::HostUnavailable)
    }

    fn list_files(&self) -> Vec<String> {
        vec![ESCAPE_PATH.to_owned()]
    }

    fn workspace_root(&self) -> Option<PathBuf> {
        Some(self.workspace_root.clone())
    }
}

impl FsAdapter for ContractFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.files
            .get(path)
            .cloned()
            .unwrap_or(Err(WorkspaceReadFailure::NotFound))
            .map_err(|failure| format!("legacy read failed: {failure:?}"))
    }

    fn read_file_typed(&self, path: &str) -> Result<Vec<u8>, WorkspaceReadFailure> {
        self.files
            .get(path)
            .cloned()
            .unwrap_or(Err(WorkspaceReadFailure::NotFound))
    }

    fn read_file_confined(&self, path: &str) -> Result<Vec<u8>, WorkspaceReadFailure> {
        if let Some(root) = &self.workspace_root
            && path == ESCAPE_PATH
        {
            let resolved = root
                .join(path)
                .canonicalize()
                .map_err(|_| WorkspaceReadFailure::NotFound)?;
            if !resolved.starts_with(root) {
                return Err(WorkspaceReadFailure::InvalidPath);
            }
        }
        self.read_file_typed(path)
    }

    fn list_files(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    fn workspace_root(&self) -> Option<PathBuf> {
        self.workspace_root.clone()
    }
}

fn host_reader() -> HostCallWorkspaceReadAdapter {
    let mut files = memory_reader_files();
    // The host fixture must not manufacture the expected confinement result:
    // if the real SidecarHostAdapter path check is bypassed, this falls through
    // to ContractFs as NotFound and the shared InvalidPath assertion fails.
    files.remove(ESCAPE_PATH);
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(outside.path().join("outside.rs"), b"private").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("outside.rs"),
        workspace.path().join(ESCAPE_PATH),
    )
    .unwrap();
    let deps = HostDeps {
        fs: Arc::new(ContractFs {
            files,
            workspace_root: Some(workspace.path().to_owned()),
        }),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(
            core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost,
        ),
        push_tx: None,
        agent_memory_storage: None,
        agent_memory_namespace: None,
    };
    let capabilities = vec![Capability::ReadFile, Capability::ListFiles];
    let transport = move |method: &str, params: Value| {
        let _fixture_roots = (&workspace, &outside);
        let frame = SidecarHostAdapter::dispatch_capability_call(
            method,
            Value::from(77),
            params,
            &deps,
            &capabilities,
        );
        let response: Value = serde_json::from_str(frame.trim()).map_err(|_| "malformed")?;
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "host rejected capability call".to_owned())
    };
    HostCallWorkspaceReadAdapter::new(Arc::new(transport))
}

fn memory_reader_files() -> BTreeMap<String, Result<Vec<u8>, WorkspaceReadFailure>> {
    BTreeMap::from([
        (VALID_PATH.to_owned(), Ok(b"fn main() {}\n".to_vec())),
        (INVALID_UTF8_PATH.to_owned(), Ok(vec![0xff])),
        (NUL_PATH.to_owned(), Ok(b"hello\0world".to_vec())),
        (
            IO_PATH.to_owned(),
            Err(WorkspaceReadFailure::HostUnavailable),
        ),
        (
            ESCAPE_PATH.to_owned(),
            Err(WorkspaceReadFailure::InvalidPath),
        ),
        ("src/oversized.txt".to_owned(), Ok(vec![b'x'; 2_000_000])),
        ("src/image.bin".to_owned(), Ok(b"ignored".to_vec())),
    ])
}

#[test]
fn in_memory_workspace_reader_satisfies_shared_contract() {
    block_on(verify_workspace_read_contract(&memory_reader()));
}

#[test]
fn host_call_workspace_reader_satisfies_shared_contract() {
    let reader = host_reader();
    block_on(verify_workspace_read_contract(&reader));

    let deps = HostDeps {
        fs: Arc::new(ContractFs {
            files: memory_reader_files(),
            workspace_root: None,
        }),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(
            core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost,
        ),
        push_tx: None,
        agent_memory_storage: None,
        agent_memory_namespace: None,
    };
    let legacy = serde_json::to_value(HostCall::ReadFile {
        path: VALID_PATH.to_owned(),
        strict: None,
    })
    .unwrap();
    let frame = SidecarHostAdapter::dispatch_capability_call(
        "workspace/readFile",
        Value::from(78),
        legacy,
        &deps,
        &[Capability::ReadFile],
    );
    let response: Value = serde_json::from_str(frame.trim()).unwrap();
    assert_eq!(response["result"], "fn main() {}\n");

    #[cfg(unix)]
    assert_symlink_swap_is_confined();
}

#[cfg(unix)]
fn assert_symlink_swap_is_confined() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(workspace.path().join("src/inside.rs"), b"inside").unwrap();
    let outside_file = outside.path().join("outside.rs");
    std::fs::write(&outside_file, b"outside-secret").unwrap();
    let link = workspace.path().join(ESCAPE_PATH);
    std::os::unix::fs::symlink(workspace.path().join("src/inside.rs"), &link).unwrap();

    let deps = HostDeps {
        fs: Arc::new(SymlinkSwapFs {
            workspace_root: workspace.path().to_owned(),
            link,
            outside: outside_file,
        }),
        ast_index: Arc::new(InMemoryAstIndex::new()),
        format_queue: Arc::new(NoOpFormatQueue),
        apply_edits: Arc::new(
            core_engine::adapters::extension::host_deps::UnsupportedApplyEditsHost,
        ),
        push_tx: None,
        agent_memory_storage: None,
        agent_memory_namespace: None,
    };
    let frame = SidecarHostAdapter::dispatch_capability_call(
        "workspace/readFile",
        Value::from(79),
        serde_json::json!({ "path": ESCAPE_PATH, "strict": true }),
        &deps,
        &[Capability::ReadFile],
    );
    let response: Value = serde_json::from_str(frame.trim()).unwrap();
    assert_eq!(response["result"]["status"], "error");
    assert_eq!(response["result"]["kind"], "invalid_path");
    assert!(!frame.contains("outside-secret"));
}

fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value.to_owned()).unwrap()
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
