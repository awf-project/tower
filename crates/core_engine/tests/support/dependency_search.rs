//! Real dependency snapshot with one indexed workspace file for public MCP tests.

use core_engine::adapters::RealFs;
use core_engine::adapters::config::load;
use core_engine::adapters::fs::dependency_source::FsDependencySource;
use core_engine::adapters::mcp::native_tools::EngineState;
use core_engine::domain::RelativePath;
use core_engine::domain::index::InvertedIndex;
use core_engine::domain::token::tokenize;
use core_engine::domain::virtual_file::FileMetadata;
use core_engine::domain::workspace::ProjectWorkspace;
use core_engine::ports::StoragePort;
use std::sync::{Arc, RwLock};

pub fn dependency_search_state_with_storage(
    workspace_root: &std::path::Path,
    storage: Box<dyn StoragePort + Send + Sync>,
) -> Arc<RwLock<EngineState>> {
    let config = load(workspace_root).expect("dependency configuration must parse");
    let source = Arc::new(
        FsDependencySource::new(
            config.dependencies.roots.clone(),
            workspace_root.to_path_buf(),
            config.dependencies.ignore.clone(),
        )
        .expect("dependency source must assemble"),
    );
    let snapshot = Arc::new(
        source
            .build_snapshot(&config.dependencies.limits)
            .expect("initial dependency snapshot must build"),
    );
    let workspace_path = RelativePath::new("workspace.rs");
    let mut workspace = ProjectWorkspace::new();
    let workspace_id = workspace
        .insert(workspace_path.clone(), FileMetadata::default())
        .unwrap();
    let mut index = InvertedIndex::new();
    index.insert(workspace_id, &tokenize(workspace_path.as_str()));
    let state = Arc::new(RwLock::new(EngineState::new(
        workspace,
        index,
        storage,
        Box::new(RealFs::new(workspace_root)),
    )));
    state
        .write()
        .unwrap()
        .configure_dependency_index(source, config.dependencies.limits, snapshot);
    state
}
