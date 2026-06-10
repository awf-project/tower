//! Typed errors for the wasmtime plugin loader (spec 11c).
//!
//! All errors are typed — no `Box<dyn Error>` at the boundary. The caller
//! (registry, CLI) can pattern-match to distinguish load failures from ABI
//! rejections from capability call failures.

/// Errors that can occur when loading or operating a wasm plugin.
///
/// Returned by [`super::WasmtimeHost::load`] and [`super::WasmInstance`]'s
/// [`crate::domain::PluginInstance`] implementation.
#[derive(Debug)]
pub enum PluginLoadError {
    /// The `.wasm` file could not be read or parsed by the wasmtime engine.
    WasmLoad(String),

    /// The guest exported a required symbol with the wrong signature, or a
    /// required export is missing.
    MissingExport(String),

    /// Linker instantiation failed — typically because the guest imports a
    /// host function that is not in the capability allow-list (AC3 / U2).
    ///
    /// This is the expected failure mode for `forbidden_import.wasm`.
    LinkError(String),

    /// `__plugin_init` returned a pointer that points outside the guest's
    /// linear memory, or the declared length overflows the memory bounds.
    ///
    /// This indicates a corrupt or malicious plugin binary.
    InvalidManifestPointer,

    /// The manifest returned by `__plugin_init` could not be deserialised
    /// with postcard.
    ManifestDeserialize(String),

    /// The plugin's `manifest.abi` field does not match the host's
    /// [`plugin_sdk::ABI_VERSION`] (AC4 / UN1).
    ///
    /// `expected` is the host ABI version; `got` is the plugin's declared ABI.
    AbiMismatch { expected: u32, got: u32 },

    /// A wasmtime runtime trap occurred during plugin initialisation.
    InitTrap(String),
}

impl std::fmt::Display for PluginLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WasmLoad(msg) => write!(f, "wasm load error: {msg}"),
            Self::MissingExport(name) => write!(f, "missing plugin export: {name}"),
            Self::LinkError(msg) => write!(f, "capability link error: {msg}"),
            Self::InvalidManifestPointer => {
                write!(f, "plugin manifest pointer is out of guest memory bounds")
            }
            Self::ManifestDeserialize(msg) => {
                write!(f, "manifest deserialisation failed: {msg}")
            }
            Self::AbiMismatch { expected, got } => {
                write!(
                    f,
                    "ABI mismatch: host requires {expected}, plugin declared {got}"
                )
            }
            Self::InitTrap(msg) => write!(f, "plugin init trap: {msg}"),
        }
    }
}

impl std::error::Error for PluginLoadError {}
