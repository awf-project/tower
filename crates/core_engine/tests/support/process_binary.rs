use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub(crate) fn selected_binary(selector: &str) -> PathBuf {
    let path = match std::env::var_os(selector) {
        Some(path) => PathBuf::from(path),
        None => match selector {
            "TOWER_PRODUCTION_BIN" => PathBuf::from(env!("CARGO_BIN_EXE_tower")),
            "TOWER_TEST_BIN" => startup_hooks_binary().to_owned(),
            _ => panic!("{selector} must be set"),
        },
    };
    assert!(path.is_absolute(), "{selector} must be absolute");
    assert!(path.is_file(), "{selector} must name a built binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_ne!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o111,
            0,
            "{selector} must name an executable"
        );
    }
    path
}

fn startup_hooks_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("core_engine belongs to the Cargo workspace");
        // Build the current checkout with the opt-in hooks only for tests that
        // need them. A separate target directory preserves Cargo's normal binary.
        let target = workspace.join("target/test-hooks");
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace)
            .args([
                "build",
                "--locked",
                "--offline",
                "-p",
                "core_engine",
                "--bin",
                "tower",
                "--features",
                "startup-test-hooks",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .expect("build startup-hook process fixture from the current checkout");
        assert!(status.success(), "startup-hook fixture build failed");
        target
            .join("debug")
            .join(format!("tower{}", std::env::consts::EXE_SUFFIX))
    })
}
