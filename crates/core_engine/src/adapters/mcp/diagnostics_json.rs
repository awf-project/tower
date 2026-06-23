#![forbid(unsafe_code)]

use serde_json::{Map, Value, json};

use crate::domain::code_intel::{Diagnostic, Severity};

pub struct DiagnosticJson<'a> {
    pub path: Option<&'a str>,
    pub diagnostic: &'a Diagnostic,
}

pub fn diagnostics_response_json(supported: bool, diagnostics: &[DiagnosticJson<'_>]) -> Value {
    let diagnostics = diagnostics
        .iter()
        .map(|diagnostic_json| {
            let diagnostic = diagnostic_json.diagnostic;
            let mut object = Map::from_iter([
                ("line".to_owned(), json!(diagnostic.range.start.line)),
                (
                    "character".to_owned(),
                    json!(diagnostic.range.start.character),
                ),
                ("endLine".to_owned(), json!(diagnostic.range.end.line)),
                (
                    "endCharacter".to_owned(),
                    json!(diagnostic.range.end.character),
                ),
                (
                    "severity".to_owned(),
                    json!(severity_json(&diagnostic.severity)),
                ),
                ("message".to_owned(), json!(diagnostic.message)),
            ]);

            if let Some(path) = diagnostic_json.path {
                object.insert("path".to_owned(), json!(path));
            }
            if let Some(code) = &diagnostic.code {
                object.insert("code".to_owned(), json!(code));
            }
            if let Some(source) = &diagnostic.source {
                object.insert("source".to_owned(), json!(source));
            }

            Value::Object(object)
        })
        .collect::<Vec<_>>();

    json!({
        "supported": supported,
        "diagnostics": diagnostics,
    })
}

pub fn unsupported_diagnostics_json() -> Value {
    diagnostics_response_json(false, &[])
}

pub fn severity_json(severity: &Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "info",
        Severity::Hint => "hint",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        DiagnosticJson, diagnostics_response_json, severity_json, unsupported_diagnostics_json,
    };
    use crate::domain::code_intel::{Diagnostic, Position, Range, Severity};

    fn diagnostic(severity: Severity, code: Option<&str>, source: Option<&str>) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 3,
                    character: 7,
                },
                end: Position {
                    line: 3,
                    character: 12,
                },
            },
            severity,
            message: "expected diagnostic".to_owned(),
            source: source.map(str::to_owned),
            code: code.map(str::to_owned),
        }
    }

    #[test]
    fn diagnostics_json_rs_provides_the_shared_public_types_and_functions() {
        let diagnostic = diagnostic(Severity::Warning, None, None);
        let diagnostics = [DiagnosticJson {
            path: None,
            diagnostic: &diagnostic,
        }];

        assert_eq!(severity_json(&Severity::Warning), "warning");
        assert_eq!(
            diagnostics_response_json(true, &diagnostics)["supported"],
            true
        );
        assert_eq!(unsupported_diagnostics_json()["supported"], false);
    }

    #[test]
    fn diagnostic_json_carries_optional_path_and_diagnostic_reference() {
        let diagnostic = diagnostic(Severity::Error, Some("E0001"), Some("rustc"));
        let diagnostic_json = DiagnosticJson {
            path: Some("src/main.rs"),
            diagnostic: &diagnostic,
        };

        assert_eq!(diagnostic_json.path, Some("src/main.rs"));
        assert_eq!(diagnostic_json.diagnostic.message, "expected diagnostic");
    }

    #[test]
    fn diagnostics_response_json_emits_supported_boolean_and_diagnostics_array_for_supported_results()
     {
        let diagnostic = diagnostic(Severity::Error, None, None);
        let diagnostics = [DiagnosticJson {
            path: None,
            diagnostic: &diagnostic,
        }];

        let value = diagnostics_response_json(true, &diagnostics);

        assert_eq!(value["supported"], true);
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn diagnostics_response_json_emits_supported_boolean_and_diagnostics_array_for_unsupported_results()
     {
        let value = diagnostics_response_json(false, &[]);

        assert_eq!(value, json!({ "supported": false, "diagnostics": [] }));
    }

    #[test]
    fn diagnostic_objects_always_include_range_severity_and_message() {
        let diagnostic = diagnostic(Severity::Error, None, None);
        let diagnostics = [DiagnosticJson {
            path: None,
            diagnostic: &diagnostic,
        }];

        let value = diagnostics_response_json(true, &diagnostics);
        let object = &value["diagnostics"][0];

        assert_eq!(object["line"], 3);
        assert_eq!(object["character"], 7);
        assert_eq!(object["endLine"], 3);
        assert_eq!(object["endCharacter"], 12);
        assert_eq!(object["severity"], "error");
        assert_eq!(object["message"], "expected diagnostic");
    }

    #[test]
    fn diagnostic_objects_include_path_code_and_source_only_when_present() {
        let with_optional_fields = diagnostic(Severity::Warning, Some("W0001"), Some("clippy"));
        let without_optional_fields = diagnostic(Severity::Hint, None, None);
        let diagnostics = [
            DiagnosticJson {
                path: Some("src/lib.rs"),
                diagnostic: &with_optional_fields,
            },
            DiagnosticJson {
                path: None,
                diagnostic: &without_optional_fields,
            },
        ];

        let value = diagnostics_response_json(true, &diagnostics);
        let first = value["diagnostics"][0].as_object().unwrap();
        let second = value["diagnostics"][1].as_object().unwrap();

        assert_eq!(first["path"], "src/lib.rs");
        assert_eq!(first["code"], "W0001");
        assert_eq!(first["source"], "clippy");
        assert!(!second.contains_key("path"));
        assert!(!second.contains_key("code"));
        assert!(!second.contains_key("source"));
    }

    #[test]
    fn severity_json_returns_exactly_error_warning_info_or_hint() {
        assert_eq!(severity_json(&Severity::Error), "error");
        assert_eq!(severity_json(&Severity::Warning), "warning");
        assert_eq!(severity_json(&Severity::Information), "info");
        assert_eq!(severity_json(&Severity::Hint), "hint");
    }

    #[test]
    fn severity_information_serializes_as_info_not_information() {
        let diagnostic = diagnostic(Severity::Information, None, None);
        let diagnostics = [DiagnosticJson {
            path: None,
            diagnostic: &diagnostic,
        }];

        let value = diagnostics_response_json(true, &diagnostics);

        assert_eq!(value["diagnostics"][0]["severity"], "info");
    }

    #[test]
    fn unsupported_diagnostics_responses_serialize_as_successful_empty_results() {
        assert_eq!(
            unsupported_diagnostics_json(),
            json!({ "supported": false, "diagnostics": [] })
        );
    }
}
