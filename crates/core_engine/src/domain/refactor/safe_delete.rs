use std::collections::HashMap;

use extension_protocol::messages::{
    CleanupCoverage, SafeDeleteBlockingReason, SafeDeleteCleanupEdit, SafeDeleteDeclarationOutcome,
    SafeDeleteDeclarationPlan, SafeDeleteMutationRequest, SafeDeleteMutationStatus,
    SafeDeletePosition, SafeDeleteReference, SafeDeleteRejectionCode, SafeDeleteReport,
    SafeDeleteRequest, SafeDeleteSemanticOutcome, SafeDeleteStatus,
};
use sha2::{Digest, Sha256};

use crate::domain::{DomainError, RelativePath};
use crate::ports::inbound::{FileMutationUseCase, SafeDeleteUseCase};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDeclaration {
    pub plan: SafeDeleteDeclarationPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeDeleteAnalysis {
    pub declaration: ResolvedDeclaration,
    pub references: Vec<SafeDeleteReference>,
    pub live_references: Vec<SafeDeleteReference>,
    pub cleanup_edits: Vec<SafeDeleteCleanupEdit>,
    pub coverage: CleanupCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeDeletePlan {
    pub declaration: ResolvedDeclaration,
    pub references: Vec<SafeDeleteReference>,
    pub edits: Vec<SafeDeleteCleanupEdit>,
    pub affected_files: Vec<RelativePath>,
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeDeleteSnapshot {
    pub path: RelativePath,
    pub content: String,
}

pub struct SafeDeleteService<'mutation> {
    mutation: &'mutation mut dyn FileMutationUseCase,
}

impl<'mutation> SafeDeleteService<'mutation> {
    pub fn new(mutation: &'mutation mut dyn FileMutationUseCase) -> Self {
        Self { mutation }
    }
}

fn blocked(
    reason: SafeDeleteBlockingReason,
    declaration: Option<SafeDeleteDeclarationPlan>,
    references: Vec<SafeDeleteReference>,
    proposed_edits: Vec<SafeDeleteCleanupEdit>,
    mutation_result: Option<extension_protocol::messages::SafeDeleteMutationResult>,
) -> SafeDeleteReport {
    let affected_files = affected_files(&proposed_edits);
    SafeDeleteReport {
        status: SafeDeleteStatus::Blocked,
        blocking_reason: Some(reason),
        declaration,
        references,
        proposed_edits,
        affected_files,
        mutation_result,
    }
}

fn affected_files(edits: &[SafeDeleteCleanupEdit]) -> Vec<String> {
    let mut paths = Vec::new();
    for edit in edits {
        if !paths.contains(&edit.path) {
            paths.push(edit.path.clone());
        }
    }
    paths
}

fn content_hash(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn snapshot_map(
    snapshots: Vec<SafeDeleteSnapshot>,
) -> Result<HashMap<String, String>, SafeDeleteBlockingReason> {
    let mut by_path = HashMap::with_capacity(snapshots.len());
    for snapshot in snapshots {
        if by_path
            .insert(snapshot.path.as_str().to_owned(), snapshot.content)
            .is_some()
        {
            return Err(SafeDeleteBlockingReason::ReferencesUnresolved);
        }
    }
    Ok(by_path)
}

fn snapshot<'a>(
    snapshots: &'a HashMap<String, String>,
    path: &str,
) -> Result<&'a str, SafeDeleteBlockingReason> {
    snapshots
        .get(path)
        .map(String::as_str)
        .ok_or(SafeDeleteBlockingReason::ReferencesUnresolved)
}

fn checked_position(content: &str, position: &SafeDeletePosition) -> Option<usize> {
    let mut line_start = 0;
    let mut current_line = 0_u32;
    while current_line < position.line {
        let newline = content[line_start..].find('\n')?;
        line_start += newline + 1;
        current_line += 1;
    }

    let line_tail = &content[line_start..];
    let line_len = line_tail.find('\n').unwrap_or(line_tail.len());
    let line = line_tail[..line_len]
        .strip_suffix('\r')
        .unwrap_or(&line_tail[..line_len]);

    let mut utf16_offset = 0_u32;
    for (byte_offset, character) in line.char_indices() {
        if utf16_offset == position.character {
            return Some(line_start + byte_offset);
        }
        utf16_offset = utf16_offset.checked_add(character.len_utf16() as u32)?;
        if utf16_offset > position.character {
            return None;
        }
    }
    (utf16_offset == position.character).then_some(line_start + line.len())
}

fn checked_reference_span(
    content: &str,
    reference: &SafeDeleteReference,
) -> Option<(usize, usize)> {
    let start = checked_position(content, &reference.start)?;
    let end = checked_position(content, &reference.end)?;
    (start <= end).then_some((start, end))
}

fn valid_byte_span(content: &str, start: usize, end: usize) -> bool {
    start <= end
        && end <= content.len()
        && content.is_char_boundary(start)
        && content.is_char_boundary(end)
}

fn edits_overlap(edits: &[SafeDeleteCleanupEdit]) -> bool {
    edits.windows(2).any(|pair| {
        pair[0].path == pair[1].path
            && pair[0].start_byte < pair[1].end_byte
            && pair[1].start_byte < pair[0].end_byte
    })
}

fn sort_edits(edits: &mut [SafeDeleteCleanupEdit]) {
    edits.sort_by(|left, right| {
        (
            &left.path,
            left.start_byte,
            left.end_byte,
            &left.replacement,
        )
            .cmp(&(
                &right.path,
                right.start_byte,
                right.end_byte,
                &right.replacement,
            ))
    });
}

impl SafeDeleteUseCase for SafeDeleteService<'_> {
    fn safe_delete(
        &mut self,
        request: SafeDeleteRequest,
        declaration: SafeDeleteDeclarationOutcome,
        semantic: SafeDeleteSemanticOutcome,
        snapshots: Vec<SafeDeleteSnapshot>,
    ) -> Result<SafeDeleteReport, DomainError> {
        let declaration = match declaration {
            SafeDeleteDeclarationOutcome::Planned { plan } => plan,
            SafeDeleteDeclarationOutcome::NotFound => {
                return Ok(blocked(
                    SafeDeleteBlockingReason::NotFound,
                    None,
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
            SafeDeleteDeclarationOutcome::AmbiguousSymbol => {
                return Ok(blocked(
                    SafeDeleteBlockingReason::AmbiguousSymbol,
                    None,
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
            SafeDeleteDeclarationOutcome::InvalidDeclaration => {
                return Ok(blocked(
                    SafeDeleteBlockingReason::InvalidDeclaration,
                    None,
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
        };

        let snapshots = match snapshot_map(snapshots) {
            Ok(snapshots) => snapshots,
            Err(reason) => {
                return Ok(blocked(
                    reason,
                    Some(declaration),
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
        };
        let declaration_content = match snapshot(&snapshots, &declaration.path) {
            Ok(content) => content,
            Err(reason) => {
                return Ok(blocked(
                    reason,
                    Some(declaration),
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
        };
        if content_hash(declaration_content) != declaration.base_hash {
            return Ok(blocked(
                SafeDeleteBlockingReason::CasConflict,
                Some(declaration),
                Vec::new(),
                Vec::new(),
                None,
            ));
        }
        if !valid_byte_span(
            declaration_content,
            declaration.deletion_span.start_byte,
            declaration.deletion_span.end_byte,
        ) || checked_position(declaration_content, &declaration.position).is_none()
        {
            return Ok(blocked(
                SafeDeleteBlockingReason::InvalidCleanup,
                Some(declaration),
                Vec::new(),
                Vec::new(),
                None,
            ));
        }

        let (semantic_plan, outcome_incomplete) = match semantic {
            SafeDeleteSemanticOutcome::Complete { plan } => (plan, false),
            SafeDeleteSemanticOutcome::Incomplete { plan } => (plan, true),
            SafeDeleteSemanticOutcome::CapabilityUnavailable => {
                return Ok(blocked(
                    SafeDeleteBlockingReason::CapabilityUnavailable,
                    Some(declaration),
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
            SafeDeleteSemanticOutcome::InvalidLocation
            | SafeDeleteSemanticOutcome::WorkspaceExternal => {
                return Ok(blocked(
                    SafeDeleteBlockingReason::InvalidCleanup,
                    Some(declaration),
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
            SafeDeleteSemanticOutcome::BackendTimeout
            | SafeDeleteSemanticOutcome::BackendCrashed
            | SafeDeleteSemanticOutcome::MalformedResponse => {
                return Ok(blocked(
                    SafeDeleteBlockingReason::BackendError,
                    Some(declaration),
                    Vec::new(),
                    Vec::new(),
                    None,
                ));
            }
        };
        let incomplete =
            outcome_incomplete || semantic_plan.coverage == CleanupCoverage::Incomplete;

        let mut references = semantic_plan.references;
        references.sort_by_key(|reference| {
            (
                reference.path.clone(),
                reference.start.line,
                reference.start.character,
                reference.end.line,
                reference.end.character,
            )
        });
        let mut edits = semantic_plan.cleanup_edits;
        sort_edits(&mut edits);

        // Hash and snapshot validation is a separate first pass so stale content
        // always wins over any coordinate error in the backend plan.
        for reference in &references {
            let content = match snapshot(&snapshots, &reference.path) {
                Ok(content) => content,
                Err(reason) => {
                    return Ok(blocked(reason, Some(declaration), references, edits, None));
                }
            };
            if content_hash(content) != reference.base_hash {
                return Ok(blocked(
                    SafeDeleteBlockingReason::CasConflict,
                    Some(declaration),
                    references,
                    edits,
                    None,
                ));
            }
        }
        for edit in &edits {
            let content = match snapshot(&snapshots, &edit.path) {
                Ok(content) => content,
                Err(reason) => {
                    return Ok(blocked(reason, Some(declaration), references, edits, None));
                }
            };
            if content_hash(content) != edit.base_hash {
                return Ok(blocked(
                    SafeDeleteBlockingReason::CasConflict,
                    Some(declaration),
                    references,
                    edits,
                    None,
                ));
            }
        }
        if incomplete {
            return Ok(blocked(
                SafeDeleteBlockingReason::CleanupIncomplete,
                Some(declaration),
                references,
                edits,
                None,
            ));
        }

        let mut live_references = Vec::new();
        let mut invalid_reference = false;
        // Finish classification before reporting an invalid location so later
        // resolved usages remain visible to the caller.
        for reference in references {
            let content = snapshots
                .get(&reference.path)
                .expect("reference snapshots were validated before coordinate conversion");
            let Some((start, end)) = checked_reference_span(content, &reference) else {
                invalid_reference = true;
                live_references.push(reference);
                continue;
            };
            let internal = reference.path == declaration.path
                && start >= declaration.deletion_span.start_byte
                && end <= declaration.deletion_span.end_byte;
            let crosses_declaration = reference.path == declaration.path
                && !internal
                && start < declaration.deletion_span.end_byte
                && declaration.deletion_span.start_byte < end;
            invalid_reference |= crosses_declaration;
            if !internal {
                live_references.push(reference);
            }
        }
        if invalid_reference {
            return Ok(blocked(
                SafeDeleteBlockingReason::InvalidCleanup,
                Some(declaration),
                live_references,
                Vec::new(),
                None,
            ));
        }

        for edit in &edits {
            let content = snapshots
                .get(&edit.path)
                .expect("edit snapshots were validated before range validation");
            if !valid_byte_span(content, edit.start_byte, edit.end_byte) {
                return Ok(blocked(
                    SafeDeleteBlockingReason::InvalidCleanup,
                    Some(declaration),
                    live_references,
                    edits,
                    None,
                ));
            }
        }

        if !live_references.is_empty() && edits.is_empty() {
            return Ok(blocked(
                SafeDeleteBlockingReason::LiveReferences,
                Some(declaration),
                live_references,
                Vec::new(),
                None,
            ));
        }

        edits.push(SafeDeleteCleanupEdit {
            path: declaration.path.clone(),
            start_byte: declaration.deletion_span.start_byte,
            end_byte: declaration.deletion_span.end_byte,
            replacement: String::new(),
            base_hash: declaration.base_hash.clone(),
        });
        sort_edits(&mut edits);
        if edits_overlap(&edits) {
            return Ok(blocked(
                SafeDeleteBlockingReason::InvalidCleanup,
                Some(declaration),
                live_references,
                edits,
                None,
            ));
        }

        let mutation_result = self
            .mutation
            .apply_safe_delete_edits(SafeDeleteMutationRequest {
                edits: edits.clone(),
                dry_run: request.dry_run,
            })?;
        let status = match mutation_result.status {
            SafeDeleteMutationStatus::Previewed => SafeDeleteStatus::Previewed,
            SafeDeleteMutationStatus::Applied => SafeDeleteStatus::Applied,
            SafeDeleteMutationStatus::PartialCommit => SafeDeleteStatus::PartialCommit,
            SafeDeleteMutationStatus::Rejected => {
                let reason = match mutation_result.rejection {
                    Some(SafeDeleteRejectionCode::CasConflict) => {
                        SafeDeleteBlockingReason::CasConflict
                    }
                    Some(_) | None => SafeDeleteBlockingReason::InvalidCleanup,
                };
                return Ok(blocked(
                    reason,
                    Some(declaration),
                    live_references,
                    edits,
                    Some(mutation_result),
                ));
            }
        };
        Ok(SafeDeleteReport {
            status,
            blocking_reason: None,
            declaration: Some(declaration),
            references: live_references,
            affected_files: affected_files(&edits),
            proposed_edits: edits,
            mutation_result: Some(mutation_result),
        })
    }
}
