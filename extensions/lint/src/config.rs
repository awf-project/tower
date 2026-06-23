#![forbid(unsafe_code)]

use core_engine::adapters::config::lint::{LintCommandConfig, ParserFormat, TargetMode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerLintConfig {
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    pub format: ParserFormat,
    pub target: TargetMode,
    pub regex: Option<String>,
    pub source: Option<String>,
}

impl From<&LintCommandConfig> for RunnerLintConfig {
    fn from(config: &LintCommandConfig) -> Self {
        Self {
            command: config.command.clone(),
            args: config.args.clone(),
            extensions: config.extensions.clone(),
            format: config.format,
            target: config.target,
            regex: config.regex.clone(),
            source: config.source.clone(),
        }
    }
}
