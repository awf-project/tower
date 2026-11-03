#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::ConfigError;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub commands: Vec<AllowedCommand>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_output_max_bytes")]
    pub stdout_max_bytes: u64,
    #[serde(default = "default_output_max_bytes")]
    pub stderr_max_bytes: u64,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            commands: Vec::new(),
            timeout_secs: default_timeout_secs(),
            stdout_max_bytes: default_output_max_bytes(),
            stderr_max_bytes: default_output_max_bytes(),
        }
    }
}

impl ShellConfig {
    pub fn enabled_policy(&self) -> Option<&Self> {
        self.enabled.then_some(self)
    }

    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.enabled && self.commands.is_empty() {
            return Err(invalid(
                "shell.commands",
                ShellConfigurationReason::EmptyCommands,
            ));
        }

        let mut names = HashSet::with_capacity(self.commands.len());
        for command in &self.commands {
            if !valid_command_name(&command.name) {
                return Err(invalid(
                    "shell.commands.name",
                    ShellConfigurationReason::InvalidName,
                ));
            }
            if !names.insert(command.name.as_str()) {
                return Err(invalid(
                    "shell.commands.name",
                    ShellConfigurationReason::DuplicateName,
                ));
            }
            if command.argv.first().is_none_or(String::is_empty) {
                return Err(invalid(
                    "shell.commands.argv",
                    ShellConfigurationReason::EmptyArgv,
                ));
            }
            if command.argv.iter().any(|argument| argument.contains('\0')) {
                return Err(invalid(
                    "shell.commands.argv",
                    ShellConfigurationReason::InvalidArgv,
                ));
            }
        }

        let timeout = Duration::from_secs(self.timeout_secs);
        if !(1..=86_400).contains(&self.timeout_secs)
            || Instant::now().checked_add(timeout).is_none()
        {
            return Err(invalid(
                "shell.timeout_secs",
                ShellConfigurationReason::InvalidTimeout,
            ));
        }
        validate_output_limit("shell.stdout_max_bytes", self.stdout_max_bytes)?;
        validate_output_limit("shell.stderr_max_bytes", self.stderr_max_bytes)
    }
}

pub(crate) fn valid_command_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_output_limit(field: &'static str, value: u64) -> Result<(), ConfigError> {
    if !(1..=16_777_216).contains(&value) || usize::try_from(value).is_err() {
        return Err(invalid(field, ShellConfigurationReason::InvalidOutputLimit));
    }
    Ok(())
}

fn invalid(field: &'static str, reason: ShellConfigurationReason) -> ConfigError {
    ConfigError::InvalidShellConfiguration { field, reason }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedCommand {
    pub name: String,
    pub argv: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellConfigurationReason {
    EmptyCommands,
    InvalidName,
    DuplicateName,
    EmptyArgv,
    InvalidArgv,
    InvalidTimeout,
    InvalidOutputLimit,
}

const fn default_timeout_secs() -> u64 {
    30
}

const fn default_output_max_bytes() -> u64 {
    65_536
}
