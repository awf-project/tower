#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

use core_engine::adapters::config::lint::ParserFormat;
use core_engine::domain::code_intel::{Diagnostic, Position, Range, Severity};
use regex::Regex;
use serde_json::Value;

use crate::diagnostics::LintDiagnostic;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParserError {
    InvalidJson,
    InvalidRegex,
    MissingCaptureGroup(&'static str),
    UnsafePath(String),
    NoDiagnostics,
}

pub fn parse_linter_output(
    format: ParserFormat,
    input: &str,
    workspace_root: &Path,
    regex: Option<&str>,
    source_override: Option<&str>,
) -> Result<Vec<LintDiagnostic>, ParserError> {
    match format {
        ParserFormat::RustcJson => parse_rustc_json(input, workspace_root, source_override),
        ParserFormat::EslintJson => parse_eslint_json(input, workspace_root, source_override),
        ParserFormat::GenericRegex => parse_generic_regex(
            input,
            workspace_root,
            regex.ok_or(ParserError::MissingCaptureGroup("regex"))?,
            source_override,
        ),
    }
}

pub fn parse_rustc_json(
    input: &str,
    workspace_root: &Path,
    source_override: Option<&str>,
) -> Result<Vec<LintDiagnostic>, ParserError> {
    let mut diagnostics = Vec::new();

    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let value: Value = serde_json::from_str(line).map_err(|_| ParserError::InvalidJson)?;
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }

        let Some(message) = value.get("message") else {
            continue;
        };
        let Some(span) = select_rustc_span(message.get("spans")) else {
            continue;
        };
        let Some(file_name) = span.get("file_name").and_then(Value::as_str) else {
            continue;
        };
        let Some(message_text) = message.get("message").and_then(Value::as_str) else {
            continue;
        };

        let Some(start_line) = one_based_u32(span.get("line_start")) else {
            continue;
        };
        let Some(start_col) = one_based_u32(span.get("column_start")) else {
            continue;
        };
        let end_line = one_based_u32(span.get("line_end")).unwrap_or(start_line);
        let end_col = one_based_u32(span.get("column_end")).unwrap_or(start_col);
        let code = message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let source = source_override
            .unwrap_or_else(|| rustc_default_source(code.as_deref()))
            .to_owned();

        diagnostics.push(LintDiagnostic {
            path: normalize_workspace_path(file_name, workspace_root)?,
            diagnostic: Diagnostic {
                range: range(start_line, start_col, end_line, end_col),
                severity: rustc_severity(message.get("level").and_then(Value::as_str)),
                message: message_text.to_owned(),
                source: Some(source),
                code,
            },
        });
    }

    finish(diagnostics)
}

pub fn parse_eslint_json(
    input: &str,
    workspace_root: &Path,
    source_override: Option<&str>,
) -> Result<Vec<LintDiagnostic>, ParserError> {
    let value: Value = serde_json::from_str(input).map_err(|_| ParserError::InvalidJson)?;
    let file_results: Vec<&Value> = match &value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![&value],
        _ => return Err(ParserError::NoDiagnostics),
    };
    let mut diagnostics = Vec::new();

    for file_result in file_results {
        let Some(file_path) = file_result.get("filePath").and_then(Value::as_str) else {
            continue;
        };
        let Some(messages) = file_result.get("messages").and_then(Value::as_array) else {
            continue;
        };
        let path = normalize_workspace_path(file_path, workspace_root)?;

        for message in messages {
            let Some(message_text) = message.get("message").and_then(Value::as_str) else {
                continue;
            };
            let Some(start_line) = one_based_u32(message.get("line")) else {
                continue;
            };
            let Some(start_col) = one_based_u32(message.get("column")) else {
                continue;
            };
            let end_line = one_based_u32(message.get("endLine")).unwrap_or(start_line);
            let end_col = one_based_u32(message.get("endColumn")).unwrap_or(start_col);

            diagnostics.push(LintDiagnostic {
                path: path.clone(),
                diagnostic: Diagnostic {
                    range: range(start_line, start_col, end_line, end_col),
                    severity: eslint_severity(message.get("severity").and_then(Value::as_i64)),
                    message: message_text.to_owned(),
                    source: Some(source_override.unwrap_or("eslint").to_owned()),
                    code: message
                        .get("ruleId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            });
        }
    }

    finish(diagnostics)
}

pub fn parse_generic_regex(
    input: &str,
    workspace_root: &Path,
    regex: &str,
    source_override: Option<&str>,
) -> Result<Vec<LintDiagnostic>, ParserError> {
    let regex = Regex::new(regex).map_err(|_| ParserError::InvalidRegex)?;
    require_capture(&regex, "file")?;
    require_capture(&regex, "line")?;
    require_capture(&regex, "col")?;
    require_capture(&regex, "message")?;

    let mut diagnostics = Vec::new();
    for captures in input.lines().flat_map(|line| regex.captures_iter(line)) {
        let Some(file) = captures.name("file").map(|capture| capture.as_str()) else {
            continue;
        };
        let Some(start_line) = captures
            .name("line")
            .and_then(|capture| parse_one_based(capture.as_str()))
        else {
            continue;
        };
        let Some(start_col) = captures
            .name("col")
            .and_then(|capture| parse_one_based(capture.as_str()))
        else {
            continue;
        };
        let Some(message) = captures.name("message").map(|capture| capture.as_str()) else {
            continue;
        };
        let end_line = captures
            .name("endLine")
            .and_then(|capture| parse_one_based(capture.as_str()))
            .unwrap_or(start_line);
        let end_col = captures
            .name("endCol")
            .and_then(|capture| parse_one_based(capture.as_str()))
            .unwrap_or(start_col);

        diagnostics.push(LintDiagnostic {
            path: normalize_workspace_path(file, workspace_root)?,
            diagnostic: Diagnostic {
                range: range(start_line, start_col, end_line, end_col),
                severity: generic_severity(
                    captures.name("severity").map(|capture| capture.as_str()),
                ),
                message: message.to_owned(),
                source: source_override.map(str::to_owned).or_else(|| {
                    captures
                        .name("source")
                        .map(|capture| capture.as_str().to_owned())
                }),
                code: captures
                    .name("code")
                    .map(|capture| capture.as_str().to_owned()),
            },
        });
    }

    finish(diagnostics)
}

fn select_rustc_span(spans: Option<&Value>) -> Option<&Value> {
    let spans = spans?.as_array()?;
    spans
        .iter()
        .find(|span| {
            span.get("is_primary").and_then(Value::as_bool) == Some(true)
                && span.get("file_name").and_then(Value::as_str).is_some()
        })
        .or_else(|| {
            spans
                .iter()
                .find(|span| span.get("file_name").and_then(Value::as_str).is_some())
        })
}

fn rustc_default_source(code: Option<&str>) -> &'static str {
    if code.is_some_and(|code| code.starts_with("clippy::")) {
        "clippy"
    } else {
        "rustc"
    }
}

fn rustc_severity(level: Option<&str>) -> Severity {
    match level {
        Some("error") => Severity::Error,
        Some("warning") => Severity::Warning,
        Some("note" | "help") => Severity::Information,
        _ => Severity::Hint,
    }
}

fn eslint_severity(severity: Option<i64>) -> Severity {
    match severity {
        Some(2) => Severity::Error,
        Some(1) => Severity::Warning,
        _ => Severity::Hint,
    }
}

fn generic_severity(severity: Option<&str>) -> Severity {
    match severity.map(str::to_ascii_lowercase).as_deref() {
        Some("error" | "err") => Severity::Error,
        Some("warning" | "warn") => Severity::Warning,
        Some("info" | "information" | "note" | "help") => Severity::Information,
        Some("hint") => Severity::Hint,
        _ => Severity::Error,
    }
}

fn one_based_u32(value: Option<&Value>) -> Option<u32> {
    value?.as_u64().and_then(|value| {
        if value == 0 || value > u64::from(u32::MAX) {
            None
        } else {
            Some(value as u32)
        }
    })
}

fn parse_one_based(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|value| *value > 0)
}

fn range(start_line: u32, start_col: u32, end_line: u32, end_col: u32) -> Range {
    Range {
        start: position(start_line, start_col),
        end: position(end_line, end_col),
    }
}

fn position(line: u32, character: u32) -> Position {
    Position {
        line: line - 1,
        character: character - 1,
    }
}

fn require_capture(regex: &Regex, name: &'static str) -> Result<(), ParserError> {
    if regex.capture_names().any(|capture| capture == Some(name)) {
        Ok(())
    } else {
        Err(ParserError::MissingCaptureGroup(name))
    }
}

fn normalize_workspace_path(path: &str, workspace_root: &Path) -> Result<String, ParserError> {
    if path.is_empty() {
        return Err(ParserError::UnsafePath(path.to_owned()));
    }
    if !workspace_root.is_absolute() && Path::new(path).is_absolute() {
        return Err(ParserError::UnsafePath(path.to_owned()));
    }

    let root = normalize_path(workspace_root);
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_child_path(&root, path)
            .ok_or_else(|| ParserError::UnsafePath(path.to_string_lossy().into_owned()))?
    };
    let relative = absolute
        .strip_prefix(&root)
        .map_err(|_| ParserError::UnsafePath(path.to_string_lossy().into_owned()))?;

    if relative.as_os_str().is_empty() {
        return Err(ParserError::UnsafePath(path.to_string_lossy().into_owned()));
    }

    Ok(relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn normalize_child_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut normalized = root.to_path_buf();
    let root_depth = normalized.components().count();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.components().count() <= root_depth {
                    return None;
                }
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(normalized)
}

fn finish(mut diagnostics: Vec<LintDiagnostic>) -> Result<Vec<LintDiagnostic>, ParserError> {
    if diagnostics.is_empty() {
        return Err(ParserError::NoDiagnostics);
    }

    diagnostics.sort_by(|left, right| {
        (
            left.path.as_str(),
            left.diagnostic.range.start.line,
            left.diagnostic.range.start.character,
            left.diagnostic.message.as_str(),
        )
            .cmp(&(
                right.path.as_str(),
                right.diagnostic.range.start.line,
                right.diagnostic.range.start.character,
                right.diagnostic.message.as_str(),
            ))
    });
    Ok(diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_engine::adapters::config::lint::ParserFormat;
    use core_engine::domain::code_intel::{Position, Range, Severity};

    fn workspace_root() -> &'static Path {
        Path::new("/workspace")
    }

    fn assert_position(position: Position, line: u32, character: u32) {
        assert_eq!(position.line, line);
        assert_eq!(position.character, character);
    }

    fn assert_range(
        range: Range,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) {
        assert_position(range.start, start_line, start_character);
        assert_position(range.end, end_line, end_character);
    }

    #[test]
    fn diagnostics_rs_defines_lint_diagnostic_with_path_and_shared_diagnostic() {
        let lint_diagnostic = LintDiagnostic {
            path: "src/lib.rs".to_owned(),
            diagnostic: core_engine::domain::code_intel::Diagnostic {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 2,
                    },
                    end: Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Severity::Warning,
                message: "unused variable".to_owned(),
                source: Some("clippy".to_owned()),
                code: Some("clippy::unused_variables".to_owned()),
            },
        };

        assert_eq!(lint_diagnostic.path, "src/lib.rs");
        assert_eq!(lint_diagnostic.diagnostic.severity, Severity::Warning);
    }

    #[test]
    fn parse_linter_output_dispatches_from_parser_format_rustc_json() {
        let input = r#"{"reason":"compiler-message","message":{"message":"unused import","level":"warning","code":{"code":"clippy::unused_imports"},"spans":[{"file_name":"/workspace/src/lib.rs","is_primary":true,"line_start":3,"column_start":5,"line_end":3,"column_end":11}]}}"#;

        let diagnostics = parse_linter_output(
            ParserFormat::RustcJson,
            input,
            workspace_root(),
            None,
            Some("cargo-clippy"),
        );

        let diagnostics = diagnostics.assert_ok();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "src/lib.rs");
        assert_eq!(
            diagnostics[0].diagnostic.source.as_deref(),
            Some("cargo-clippy")
        );
    }

    #[test]
    fn parse_linter_output_dispatches_from_parser_format_eslint_json() {
        let input = r#"[{"filePath":"/workspace/web/app.js","messages":[{"ruleId":"no-alert","severity":2,"message":"Unexpected alert.","line":4,"column":9}]}]"#;

        let diagnostics = parse_linter_output(
            ParserFormat::EslintJson,
            input,
            workspace_root(),
            None,
            None,
        );

        let diagnostics = diagnostics.assert_ok();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "web/app.js");
        assert_eq!(diagnostics[0].diagnostic.severity, Severity::Error);
    }

    #[test]
    fn parse_linter_output_dispatches_from_parser_format_generic_regex() {
        let input = "src/main.rs:7:13: warning W001: suspicious call";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<severity>\w+) (?P<code>\w+): (?P<message>.+)";

        let diagnostics = parse_linter_output(
            ParserFormat::GenericRegex,
            input,
            workspace_root(),
            Some(regex),
            Some("custom-lint"),
        );

        let diagnostics = diagnostics.assert_ok();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "src/main.rs");
        assert_eq!(
            diagnostics[0].diagnostic.source.as_deref(),
            Some("custom-lint")
        );
    }

    #[test]
    fn parse_linter_output_returns_parser_errors_from_dispatched_format() {
        let error = parse_linter_output(
            ParserFormat::RustcJson,
            "{not json",
            workspace_root(),
            None,
            None,
        )
        .assert_err();

        assert_eq!(error, ParserError::InvalidJson);
    }

    #[test]
    fn parse_rustc_json_accepts_newline_delimited_compiler_message_objects_and_uses_primary_span() {
        let input = r#"{"reason":"build-script-executed"}
{"reason":"compiler-message","message":{"message":"cannot find value `foo`","level":"error","code":{"code":"E0425"},"spans":[{"file_name":"/workspace/src/main.rs","is_primary":false,"line_start":9,"column_start":1,"line_end":9,"column_end":4},{"file_name":"/workspace/src/main.rs","is_primary":true,"line_start":10,"column_start":15,"line_end":10,"column_end":18}]}}"#;

        let diagnostics = parse_rustc_json(input, workspace_root(), None).assert_ok();

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "src/main.rs");
        assert_eq!(diagnostics[0].diagnostic.message, "cannot find value `foo`");
        assert_eq!(diagnostics[0].diagnostic.severity, Severity::Error);
        assert_eq!(diagnostics[0].diagnostic.source.as_deref(), Some("rustc"));
        assert_eq!(diagnostics[0].diagnostic.code.as_deref(), Some("E0425"));
        assert_range(diagnostics[0].diagnostic.range, 9, 14, 9, 17);
    }

    #[test]
    fn parse_rustc_json_uses_first_span_with_file_name_when_no_primary_span_exists() {
        let input = r#"{"reason":"compiler-message","message":{"message":"borrowed value does not live long enough","level":"error","code":{"code":"E0597"},"spans":[{"is_primary":false,"line_start":1,"column_start":1,"line_end":1,"column_end":1},{"file_name":"/workspace/src/fallback.rs","is_primary":false,"line_start":12,"column_start":9,"line_end":12,"column_end":15},{"file_name":"/workspace/src/later.rs","is_primary":false,"line_start":20,"column_start":1,"line_end":20,"column_end":2}]}}"#;

        let diagnostics = parse_rustc_json(input, workspace_root(), None).assert_ok();

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "src/fallback.rs");
        assert_eq!(
            diagnostics[0].diagnostic.message,
            "borrowed value does not live long enough"
        );
        assert_eq!(diagnostics[0].diagnostic.severity, Severity::Error);
        assert_eq!(diagnostics[0].diagnostic.source.as_deref(), Some("rustc"));
        assert_eq!(diagnostics[0].diagnostic.code.as_deref(), Some("E0597"));
        assert_range(diagnostics[0].diagnostic.range, 11, 8, 11, 14);
    }

    #[test]
    fn parse_rustc_json_maps_levels_and_defaults_clippy_source_from_clippy_code() {
        let input = r#"{"reason":"compiler-message","message":{"message":"lint warning","level":"warning","code":{"code":"clippy::pedantic"},"spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":2,"column_start":3,"line_end":2,"column_end":4}]}}
{"reason":"compiler-message","message":{"message":"try this","level":"help","spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":3,"column_start":1,"line_end":3,"column_end":1}]}}
{"reason":"compiler-message","message":{"message":"internal detail","level":"unknown","spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":4,"column_start":1,"line_end":4,"column_end":1}]}}"#;

        let diagnostics = parse_rustc_json(input, workspace_root(), None).assert_ok();

        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostics[0].diagnostic.source.as_deref(), Some("clippy"));
        assert_eq!(diagnostics[1].diagnostic.severity, Severity::Information);
        assert_eq!(diagnostics[2].diagnostic.severity, Severity::Hint);
    }

    #[test]
    fn parse_eslint_json_accepts_array_and_single_file_result_shapes() {
        let array_input = r#"[{"filePath":"/workspace/web/app.js","messages":[{"ruleId":"no-alert","severity":2,"message":"Unexpected alert.","line":4,"column":9,"endLine":4,"endColumn":14}]}]"#;
        let object_input = r#"{"filePath":"/workspace/web/util.js","messages":[{"ruleId":"eqeqeq","severity":1,"message":"Expected ===.","line":8,"column":3}]}"#;

        let array_diagnostics = parse_eslint_json(array_input, workspace_root(), None).assert_ok();
        let object_diagnostics =
            parse_eslint_json(object_input, workspace_root(), Some("biome")).assert_ok();

        assert_eq!(array_diagnostics[0].path, "web/app.js");
        assert_eq!(array_diagnostics[0].diagnostic.severity, Severity::Error);
        assert_eq!(
            array_diagnostics[0].diagnostic.source.as_deref(),
            Some("eslint")
        );
        assert_eq!(
            array_diagnostics[0].diagnostic.code.as_deref(),
            Some("no-alert")
        );
        assert_range(array_diagnostics[0].diagnostic.range, 3, 8, 3, 13);

        assert_eq!(object_diagnostics[0].path, "web/util.js");
        assert_eq!(object_diagnostics[0].diagnostic.severity, Severity::Warning);
        assert_eq!(
            object_diagnostics[0].diagnostic.source.as_deref(),
            Some("biome")
        );
        assert_eq!(
            object_diagnostics[0].diagnostic.code.as_deref(),
            Some("eqeqeq")
        );
    }

    #[test]
    fn parse_eslint_json_maps_zero_missing_and_other_severity_to_hint() {
        let input = r#"[{"filePath":"web/app.js","messages":[{"severity":0,"message":"off rule","line":1,"column":1},{"message":"missing severity","line":2,"column":1},{"severity":99,"message":"unknown severity","line":3,"column":1}]}]"#;

        let diagnostics = parse_eslint_json(input, workspace_root(), None).assert_ok();

        assert_eq!(diagnostics.len(), 3);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.diagnostic.severity == Severity::Hint)
        );
    }

    #[test]
    fn parse_generic_regex_requires_named_captures_and_preserves_optional_fields() {
        let input = "src/main.rs:12:4-13:8: info R123 custom source: message text";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+)-(?P<endLine>\d+):(?P<endCol>\d+): (?P<severity>\w+) (?P<code>\w+) (?P<source>[^:]+): (?P<message>.+)";

        let diagnostics = parse_generic_regex(input, workspace_root(), regex, None).assert_ok();

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "src/main.rs");
        assert_eq!(diagnostics[0].diagnostic.severity, Severity::Information);
        assert_eq!(diagnostics[0].diagnostic.code.as_deref(), Some("R123"));
        assert_eq!(
            diagnostics[0].diagnostic.source.as_deref(),
            Some("custom source")
        );
        assert_eq!(diagnostics[0].diagnostic.message, "message text");
        assert_range(diagnostics[0].diagnostic.range, 11, 3, 12, 7);
    }

    #[test]
    fn parse_generic_regex_maps_severity_case_insensitively_and_defaults_missing_or_unknown_to_error()
     {
        let input = "a.rs:1:1: err broken\nb.rs:2:1: WARN risky\nc.rs:3:1: note detail\nd.rs:4:1: hint idea\ne.rs:5:1: strange fallback";
        let regex =
            r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<severity>\w+) (?P<message>.+)";

        let diagnostics = parse_generic_regex(input, workspace_root(), regex, None).assert_ok();

        assert_eq!(diagnostics[0].diagnostic.severity, Severity::Error);
        assert_eq!(diagnostics[1].diagnostic.severity, Severity::Warning);
        assert_eq!(diagnostics[2].diagnostic.severity, Severity::Information);
        assert_eq!(diagnostics[3].diagnostic.severity, Severity::Hint);
        assert_eq!(diagnostics[4].diagnostic.severity, Severity::Error);
    }

    #[test]
    fn all_parsers_convert_one_based_positions_and_fallback_missing_end_position_code_and_source() {
        let rustc_input = r#"{"reason":"compiler-message","message":{"message":"note text","level":"note","spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":5,"column_start":7}]}}"#;
        let eslint_input =
            r#"{"filePath":"web/app.js","messages":[{"message":"no rule","line":6,"column":8}]}"#;
        let generic_input = "scripts/check.sh:7:9: shell issue";
        let generic_regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        let rustc = parse_rustc_json(rustc_input, workspace_root(), None).assert_ok();
        let eslint = parse_eslint_json(eslint_input, workspace_root(), None).assert_ok();
        let generic =
            parse_generic_regex(generic_input, workspace_root(), generic_regex, None).assert_ok();

        assert_range(rustc[0].diagnostic.range, 4, 6, 4, 6);
        assert_eq!(rustc[0].diagnostic.code, None);
        assert_eq!(rustc[0].diagnostic.source.as_deref(), Some("rustc"));

        assert_range(eslint[0].diagnostic.range, 5, 7, 5, 7);
        assert_eq!(eslint[0].diagnostic.code, None);
        assert_eq!(eslint[0].diagnostic.source.as_deref(), Some("eslint"));

        assert_range(generic[0].diagnostic.range, 6, 8, 6, 8);
        assert_eq!(generic[0].diagnostic.code, None);
        assert_eq!(generic[0].diagnostic.source, None);
    }

    #[test]
    fn parsers_normalize_absolute_and_relative_paths_against_workspace_root_without_environment() {
        let input = "/workspace/src/b.rs:2:1: second\nsrc/a.rs:1:1: first";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        let diagnostics = parse_generic_regex(input, workspace_root(), regex, None).assert_ok();

        assert_eq!(diagnostics[0].path, "src/a.rs");
        assert_eq!(diagnostics[1].path, "src/b.rs");
    }

    #[test]
    fn paths_outside_workspace_root_return_parser_error_unsafe_path() {
        let input = "/tmp/outside.rs:1:1: escaped workspace";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        let error = parse_generic_regex(input, workspace_root(), regex, None).assert_err();

        assert_eq!(error, ParserError::UnsafePath("/tmp/outside.rs".to_owned()));
    }

    #[test]
    fn relative_paths_that_escape_relative_workspace_root_return_unsafe_path() {
        let input = "../outside.rs:1:1: escaped relative workspace root";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        let error = parse_generic_regex(input, Path::new("."), regex, None).assert_err();

        assert_eq!(error, ParserError::UnsafePath("../outside.rs".to_owned()));
    }

    #[test]
    fn absolute_paths_with_relative_workspace_root_return_unsafe_path() {
        let input = "/tmp/outside.rs:1:1: escaped relative workspace root";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        let error = parse_generic_regex(input, Path::new("."), regex, None).assert_err();

        assert_eq!(error, ParserError::UnsafePath("/tmp/outside.rs".to_owned()));
    }

    #[test]
    fn malformed_json_invalid_regex_missing_captures_and_no_diagnostics_return_stable_parser_errors()
     {
        let missing_file_capture = r"(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        assert_eq!(
            parse_rustc_json("{not json", workspace_root(), None).assert_err(),
            ParserError::InvalidJson
        );
        assert_eq!(
            parse_generic_regex("src/lib.rs:1:1: msg", workspace_root(), "(", None).assert_err(),
            ParserError::InvalidRegex
        );
        assert_eq!(
            parse_generic_regex("1:1: msg", workspace_root(), missing_file_capture, None)
                .assert_err(),
            ParserError::MissingCaptureGroup("file")
        );
        assert_eq!(
            parse_eslint_json("[]", workspace_root(), None).assert_err(),
            ParserError::NoDiagnostics
        );
    }

    #[test]
    fn diagnostics_from_parser_are_sorted_by_path_line_character_then_message() {
        let input = "src/b.rs:1:1: later path\nsrc/a.rs:2:1: later line\nsrc/a.rs:1:2: later character\nsrc/a.rs:1:1: z message\nsrc/a.rs:1:1: a message";
        let regex = r"(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<message>.+)";

        let diagnostics = parse_generic_regex(input, workspace_root(), regex, None).assert_ok();

        let order: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                (
                    diagnostic.path.as_str(),
                    diagnostic.diagnostic.range.start.line,
                    diagnostic.diagnostic.range.start.character,
                    diagnostic.diagnostic.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                ("src/a.rs", 0, 0, "a message"),
                ("src/a.rs", 0, 0, "z message"),
                ("src/a.rs", 0, 1, "later character"),
                ("src/a.rs", 1, 0, "later line"),
                ("src/b.rs", 0, 0, "later path"),
            ]
        );
    }

    trait ParserResultExt<T> {
        fn assert_ok(self) -> T;
        fn assert_err(self) -> ParserError;
    }

    impl<T> ParserResultExt<T> for Result<T, ParserError> {
        fn assert_ok(self) -> T {
            match self {
                Ok(value) => value,
                Err(error) => panic!("expected Ok(_), got Err({error:?})"),
            }
        }

        fn assert_err(self) -> ParserError {
            match self {
                Ok(_) => panic!("expected Err(_), got Ok(_)"),
                Err(error) => error,
            }
        }
    }
}
