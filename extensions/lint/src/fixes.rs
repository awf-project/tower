#![forbid(unsafe_code)]

use crate::diagnostics::LintDiagnostic;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintFix {
    pub path: String,
    pub edits: Vec<LintByteEdit>,
    pub applicability: FixApplicability,
    pub unsupported: Option<UnsupportedFix>,
    pub diagnostic: LintDiagnostic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintByteEdit {
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixApplicability {
    Safe,
    Unsafe,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedFix {
    pub supported_fix: bool,
    pub reason: UnsupportedFixReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedFixReason {
    NoStructuredFix,
    UnsupportedParser,
}
