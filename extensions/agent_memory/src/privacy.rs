use regex::{NoExpand, Regex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivacyAction {
    Reject,
    Redact { replacement: String },
}

#[derive(Clone, Debug)]
pub struct PrivacyRule {
    id: String,
    pattern: Regex,
    action: PrivacyAction,
}

/// Pure privacy boundary for every textual value before it can be derived,
/// persisted, logged, or sent to a provider.
///
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivacyFilter {
    rules: Vec<PrivacyRule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivacyRuleConfig {
    pub id: String,
    pub pattern: String,
    pub action: PrivacyAction,
}

/// Text owned by a memory operation before any downstream representation is
/// produced. Field names are fixed here rather than accepted from callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivacyProducerInput {
    pub title: String,
    pub body: String,
    pub provenance: PrivacyProvenanceInput,
    pub idempotency_key: String,
    pub query: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivacyProvenanceInput {
    pub branch: Option<String>,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub tags: Vec<String>,
    pub source_agent: Option<String>,
    pub source_client: Option<String>,
    pub commit: Option<String>,
    pub pull_request: Option<String>,
    pub issue: Option<String>,
}

/// Receives only privacy-filtered inputs. Implementations may persist, derive,
/// log, or dispatch to a provider; no callback occurs unless every field has
/// passed the privacy boundary.
pub trait PrivacyProducer {
    fn persist(&mut self, input: &PrivacyProducerInput);
    fn derive(&mut self, input: &PrivacyProducerInput);
    fn log(&mut self, input: &PrivacyProducerInput);
    fn dispatch_provider(&mut self, input: &PrivacyProducerInput);
}

impl PrivacyRule {
    pub fn try_new(
        id: String,
        pattern: String,
        action: PrivacyAction,
    ) -> Result<Self, PrivacyError> {
        if id.is_empty() {
            return Err(PrivacyError::InvalidRule {
                rule_id: id,
                reason: "empty_id",
            });
        }
        let compiled = Regex::new(&pattern).map_err(|_| PrivacyError::InvalidRule {
            rule_id: id.clone(),
            reason: "invalid_pattern",
        })?;
        Ok(Self {
            id,
            pattern: compiled,
            action,
        })
    }
}

impl PartialEq for PrivacyRule {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.pattern.as_str() == other.pattern.as_str()
            && self.action == other.action
    }
}

impl Eq for PrivacyRule {}

impl PrivacyFilter {
    pub fn try_new(rules: Vec<PrivacyRule>) -> Result<Self, PrivacyError> {
        Ok(Self { rules })
    }

    pub fn try_from_configs(configs: Vec<PrivacyRuleConfig>) -> Result<Self, PrivacyError> {
        configs
            .into_iter()
            .map(|config| PrivacyRule::try_new(config.id, config.pattern, config.action))
            .collect::<Result<Vec<_>, _>>()
            .and_then(Self::try_new)
    }

    /// Filters the complete owned input before invoking any downstream
    /// producer. This all-or-nothing ordering prevents partial persistence,
    /// derivation, logging, or provider dispatch on a later field failure.
    pub fn produce(
        &self,
        input: &PrivacyProducerInput,
        limits: &crate::AgentMemoryLimitsConfig,
        producer: &mut dyn PrivacyProducer,
    ) -> Result<(), PrivacyError> {
        let filtered = PrivacyProducerInput {
            title: self.apply("title", &input.title, limits.max_title_bytes)?,
            body: self.apply("body", &input.body, limits.max_body_bytes)?,
            provenance: PrivacyProvenanceInput {
                branch: self.apply_optional(
                    "provenance.branch",
                    input.provenance.branch.as_deref(),
                    limits.max_metadata_string_bytes,
                )?,
                paths: self.apply_all(
                    "provenance.paths",
                    &input.provenance.paths,
                    limits.max_metadata_string_bytes,
                )?,
                symbols: self.apply_all(
                    "provenance.symbols",
                    &input.provenance.symbols,
                    limits.max_metadata_string_bytes,
                )?,
                tags: self.apply_all(
                    "provenance.tags",
                    &input.provenance.tags,
                    limits.max_metadata_string_bytes,
                )?,
                source_agent: self.apply_optional(
                    "provenance.source_agent",
                    input.provenance.source_agent.as_deref(),
                    limits.max_metadata_string_bytes,
                )?,
                source_client: self.apply_optional(
                    "provenance.source_client",
                    input.provenance.source_client.as_deref(),
                    limits.max_metadata_string_bytes,
                )?,
                commit: self.apply_optional(
                    "provenance.commit",
                    input.provenance.commit.as_deref(),
                    limits.max_metadata_string_bytes,
                )?,
                pull_request: self.apply_optional(
                    "provenance.pull_request",
                    input.provenance.pull_request.as_deref(),
                    limits.max_metadata_string_bytes,
                )?,
                issue: self.apply_optional(
                    "provenance.issue",
                    input.provenance.issue.as_deref(),
                    limits.max_metadata_string_bytes,
                )?,
            },
            idempotency_key: self.apply(
                "idempotency_key",
                &input.idempotency_key,
                limits.max_metadata_string_bytes,
            )?,
            query: self.apply("query", &input.query, limits.max_query_bytes)?,
        };

        producer.persist(&filtered);
        producer.derive(&filtered);
        producer.log(&filtered);
        producer.dispatch_provider(&filtered);
        Ok(())
    }

    fn apply_optional(
        &self,
        field: &'static str,
        value: Option<&str>,
        max_bytes: u32,
    ) -> Result<Option<String>, PrivacyError> {
        value
            .map(|value| self.apply(field, value, max_bytes))
            .transpose()
    }

    fn apply_all(
        &self,
        field: &'static str,
        values: &[String],
        max_bytes: u32,
    ) -> Result<Vec<String>, PrivacyError> {
        values
            .iter()
            .map(|value| self.apply(field, value, max_bytes))
            .collect()
    }

    /// Applies the configured rules to one named textual boundary. `max_bytes`
    /// is rechecked after redaction by the completed implementation.
    pub fn apply(
        &self,
        field: &'static str,
        value: &str,
        max_bytes: u32,
    ) -> Result<String, PrivacyError> {
        for rule in &self.rules {
            if matches!(rule.action, PrivacyAction::Reject) && rule.pattern.is_match(value) {
                return Err(PrivacyError::Rejected {
                    field,
                    rule_id: rule.id.clone(),
                });
            }
        }

        let mut filtered = if matches!(field, "title" | "query") {
            value.trim().to_owned()
        } else {
            value.to_owned()
        };
        for rule in &self.rules {
            if let PrivacyAction::Redact { replacement } = &rule.action {
                filtered = rule
                    .pattern
                    .replace_all(&filtered, NoExpand(replacement))
                    .into_owned();
            }
        }

        for rule in &self.rules {
            if rule.pattern.is_match(&filtered) {
                return Err(PrivacyError::RedactionFailed {
                    field,
                    rule_id: rule.id.clone(),
                });
            }
        }
        if (matches!(field, "title" | "query") && filtered.trim().is_empty())
            || filtered.len() > max_bytes as usize
        {
            let rule_id = self
                .rules
                .iter()
                .find(|rule| matches!(rule.action, PrivacyAction::Redact { .. }))
                .map_or_else(|| "output_bounds".to_owned(), |rule| rule.id.clone());
            return Err(PrivacyError::RedactionFailed { field, rule_id });
        }
        Ok(filtered)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivacyError {
    Rejected {
        field: &'static str,
        rule_id: String,
    },
    InvalidRule {
        rule_id: String,
        reason: &'static str,
    },
    RedactionFailed {
        field: &'static str,
        rule_id: String,
    },
}
