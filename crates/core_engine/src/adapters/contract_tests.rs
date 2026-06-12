//! Contract test invocations for the in-memory fakes.
//!
//! These modules instantiate the reusable macros from `test_support` against
//! `InMemoryStorage` and `InMemoryFs`. Future real adapters (specs 04 / 05)
//! will do the same in their own crates, giving uniform behavioral coverage.
//!
//! # TDD notes
//!
//! - RED: the macros were written first (in `test_support`) and caused
//!   compile-errors until the fakes were implemented.
//! - GREEN: `InMemoryStorage` / `InMemoryFs` implementations made them pass.
//! - REFACTOR: the contract logic lives in the macros, not duplicated here.

use crate::adapters::{InMemoryAstIndex, InMemoryFs, InMemoryStorage, XdgAstIndexAdapter};
use crate::{ast_index_contract_tests, filesystem_contract_tests, storage_contract_tests};

// Expand StoragePort contract suite against InMemoryStorage.
storage_contract_tests!(InMemoryStorage::new);

// Expand FileSystemPort contract suite against InMemoryFs.
filesystem_contract_tests!(InMemoryFs::new);

// ── AstIndexPort contract suite ───────────────────────────────────────────────
//
// Both the in-memory fake and the real XDG adapter must pass the same
// behavioural contract (spec 02 dependency-inversion proof).

// In-memory fake — no I/O.
ast_index_contract_tests!(InMemoryAstIndex::new);

// Real adapter wired to a TempDir — exercises the shadow-file path.
//
// Each macro-expanded test calls `make_xdg_adapter()` to get a fresh
// `XdgAstIndexAdapter` pointed at its own temporary directory. The
// `TempDir` is leaked (via `into_path`) so the directory survives for the
// duration of the test; the OS cleans it up on process exit.
//
// The macro expands a module named `ast_index_contract`. To avoid a name
// collision with the `InMemoryAstIndex` invocation above, wrap this one in
// a distinct outer module.
mod xdg {
    use super::*;

    // Decision: `TempDir::keep()` instead of leaking: `keep()` returns the
    // path and drops the guard cleanly (no OS-level descriptor leak). The
    // returned `PathBuf` keeps the directory alive implicitly on Linux because
    // the directory entry still exists; it is cleaned up by the OS only after
    // the process exits (or when the test framework's temp-cleanup runs).
    fn make_xdg_adapter() -> XdgAstIndexAdapter {
        let tmp = tempfile::tempdir().expect("tempdir for XdgAstIndexAdapter contract tests");
        let base = tmp.path().join("ast");
        let _ = tmp.keep();
        XdgAstIndexAdapter::new(base)
    }

    ast_index_contract_tests!(make_xdg_adapter);
}

// ── Dependency-inversion proof (spec U1 / EV1 / TDD step 5) ─────────────────
//
// A minimal domain-side consumer that is wired at compile-time only to trait
// objects. It never imports InMemoryStorage or InMemoryFs directly, proving
// the domain depends solely on abstractions.

mod dependency_inversion_proof {
    use crate::domain::{RelativePath, VirtualFile};
    use crate::ports::{FileSystemPort, PortError, StoragePort};
    use crate::test_support::make_virtual_file;

    /// Simulates a tiny domain service that persists a file through the port
    /// contracts. The concrete types (`InMemoryStorage`, `InMemoryFs`) are
    /// injected from the outside; this function knows nothing about them.
    fn index_file(
        storage: &mut dyn StoragePort,
        fs: &mut dyn FileSystemPort,
        path: RelativePath,
        content: Vec<u8>,
        file: VirtualFile,
    ) -> Result<(), PortError> {
        fs.write(path, content)?;
        storage.put(file)?;
        Ok(())
    }

    #[test]
    fn domain_service_wired_against_fakes_roundtrips() {
        use crate::adapters::{InMemoryFs, InMemoryStorage};

        let mut storage = InMemoryStorage::new();
        let mut fs = InMemoryFs::new();

        let path = RelativePath::new("src/lib.rs");
        let file = make_virtual_file(0, 0, "src/lib.rs");
        let id = file.id;

        // Inject fakes through trait-object references — the function only
        // sees &dyn StoragePort / &dyn FileSystemPort.
        index_file(
            &mut storage,
            &mut fs,
            path.clone(),
            b"pub fn hello() {}".to_vec(),
            file.clone(),
        )
        .unwrap();

        // Verify via the same trait objects.
        let stored: &dyn StoragePort = &storage;
        assert_eq!(stored.get(id).unwrap(), file);

        let vfs: &dyn FileSystemPort = &fs;
        assert_eq!(vfs.read(&path).unwrap(), b"pub fn hello() {}");
    }

    #[test]
    fn no_op_plugin_host_is_callable_through_trait_object() {
        use crate::domain::FileId;
        use crate::ports::{NoOpPluginHost, PluginHostPort};

        let host: &dyn PluginHostPort = &NoOpPluginHost;
        let id = FileId::new_for_testing(0, 0);
        let path = RelativePath::new("src/main.rs");
        // Calling through dyn — must compile and not panic (AC3 / OP1).
        host.on_file_indexed(id, &path);
        host.on_file_changed(id, &path);
    }
}
