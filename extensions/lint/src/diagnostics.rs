#![forbid(unsafe_code)]

use core_engine::domain::code_intel::{Diagnostic, Severity};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LintDiagnostic {
    pub path: String,
    pub diagnostic: Diagnostic,
}

pub fn severity_json(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "info",
        Severity::Hint => "hint",
    }
}

#[cfg(test)]
mod tests {
    use super::severity_json;
    use core_engine::domain::code_intel::Severity;

    #[test]
    fn severity_json_uses_shared_mcp_diagnostic_vocabulary() {
        assert_eq!(severity_json(Severity::Error), "error");
        assert_eq!(severity_json(Severity::Warning), "warning");
        assert_eq!(severity_json(Severity::Information), "info");
        assert_eq!(severity_json(Severity::Hint), "hint");
    }
}
