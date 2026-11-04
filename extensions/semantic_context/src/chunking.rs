use crate::{
    ChunkingConfig, ChunkingFingerprint, SemanticChunk, SemanticChunkInput, SemanticDomainError,
    SymbolSpan, WorkspaceDocument,
};

pub fn chunk_document(
    document: &WorkspaceDocument,
    config: &ChunkingConfig,
) -> Result<Vec<SemanticChunk>, SemanticDomainError> {
    if document.source.len() > config.max_file_bytes {
        return Err(SemanticDomainError::DocumentTooLarge {
            actual_bytes: document.source.len(),
            max_file_bytes: config.max_file_bytes,
        });
    }
    if document.source.is_empty() {
        return Ok(Vec::new());
    }

    let fingerprint = ChunkingFingerprint {
        algorithm_version: config.algorithm_version.clone(),
        target_bytes: config.target_bytes as u64,
        overlap_bytes: config.overlap_bytes as u64,
        max_file_bytes: config.max_file_bytes as u64,
    };
    let reliable_symbols = document.symbol_spans.iter().all(|span| {
        span.start_byte < span.end_byte
            && span.end_byte <= document.source.len()
            && document.source.is_char_boundary(span.start_byte)
            && document.source.is_char_boundary(span.end_byte)
    });
    let spans = if !document.symbol_spans.is_empty() && reliable_symbols {
        document
            .symbol_spans
            .iter()
            .copied()
            .map(|span| (span.start_byte, span.end_byte, Some(span)))
            .collect()
    } else {
        heuristic_spans(&document.source, config.target_bytes, config.overlap_bytes)
    };

    spans
        .into_iter()
        .map(|(start_byte, end_byte, symbol)| {
            SemanticChunk::new(SemanticChunkInput {
                path: document.path.clone(),
                source: &document.source,
                start_byte,
                end_byte,
                language: document.language.clone(),
                symbol,
                chunking_fingerprint: fingerprint.clone(),
                expected_chunk_hash: None,
            })
        })
        .collect()
}

fn heuristic_spans(
    source: &str,
    target: usize,
    overlap: usize,
) -> Vec<(usize, usize, Option<SymbolSpan>)> {
    let target = target.max(1);
    let mut spans = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let mut hard_end = (start + target).min(source.len());
        while hard_end > start && !source.is_char_boundary(hard_end) {
            hard_end -= 1;
        }
        if hard_end == start {
            hard_end = source[start..]
                .char_indices()
                .nth(1)
                .map_or(source.len(), |(offset, _)| start + offset);
        }
        let mut end = if hard_end == source.len() {
            hard_end
        } else {
            source[start..hard_end]
                .rfind('\n')
                .map_or(hard_end, |offset| start + offset + 1)
        };
        while end > start && !source.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = hard_end;
            while end < source.len() && !source.is_char_boundary(end) {
                end += 1;
            }
        }
        spans.push((start, end, None));
        if end == source.len() {
            break;
        }
        let desired = end.saturating_sub(overlap).max(start + 1);
        start = desired;
        while start < end && !source.is_char_boundary(start) {
            start += 1;
        }
    }
    spans
}
