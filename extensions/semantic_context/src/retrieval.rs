use crate::{
    EmbeddingVector, RankedHit, RelatedFile, SemanticDomainError, SemanticSearchRequest,
    SemanticSnapshot, TaskContext,
};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
};

pub fn semantic_search(
    snapshot: &SemanticSnapshot,
    query: &EmbeddingVector,
    request: &SemanticSearchRequest,
) -> Result<Vec<RankedHit>, SemanticDomainError> {
    semantic_search_with_check(snapshot, query, request, || Ok(()))
}

pub fn semantic_search_with_check<E, F>(
    snapshot: &SemanticSnapshot,
    query: &EmbeddingVector,
    request: &SemanticSearchRequest,
    mut check_active: F,
) -> Result<Vec<RankedHit>, E>
where
    E: From<SemanticDomainError>,
    F: FnMut() -> Result<(), E>,
{
    let mut hits = Vec::new();
    let query_tokens = tokens(&request.query_text);
    for indexed in &snapshot.chunks {
        check_active()?;
        if (!request.filters.paths.is_empty()
            && !request.filters.paths.contains(&indexed.chunk.path))
            || (!request.filters.languages.is_empty()
                && !request.filters.languages.contains(&indexed.chunk.language))
        {
            continue;
        }
        let cosine_score = cosine(query, &indexed.embedding).map_err(E::from)?;
        let lexical_score = request.lexical_reranking.then(|| {
            if query_tokens.is_empty() {
                0.0
            } else {
                let snippet = tokens(indexed.chunk.snippet.as_str());
                query_tokens.intersection(&snippet).count() as f32 / query_tokens.len() as f32
            }
        });
        let final_score =
            lexical_score.map_or(cosine_score, |lexical| 0.8 * cosine_score + 0.2 * lexical);
        hits.push(RankedHit {
            chunk: indexed.chunk.clone(),
            rank: 0,
            cosine_score,
            lexical_score,
            final_score,
        });
    }
    check_active()?;
    hits.sort_by(compare_hits);
    check_active()?;
    hits.truncate(request.limit);
    for (index, hit) in hits.iter_mut().enumerate() {
        check_active()?;
        hit.rank = index + 1;
    }
    check_active()?;
    Ok(hits)
}

pub fn related_files(hits: &[RankedHit], limit: usize) -> Vec<RelatedFile> {
    related_files_with_check(hits, limit, || Ok::<_, std::convert::Infallible>(()))
        .expect("infallible related-file aggregation")
}

pub fn related_files_with_check<E>(
    hits: &[RankedHit],
    limit: usize,
    mut check_active: impl FnMut() -> Result<(), E>,
) -> Result<Vec<RelatedFile>, E> {
    let mut best = BTreeMap::new();
    for hit in hits {
        check_active()?;
        best.entry(hit.chunk.path.clone())
            .and_modify(|score: &mut f32| *score = score.max(hit.final_score))
            .or_insert(hit.final_score);
    }
    let mut files: Vec<_> = best
        .into_iter()
        .map(|(path, score)| RelatedFile { path, score })
        .collect();
    files.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    files.truncate(limit);
    check_active()?;
    Ok(files)
}

pub fn context_for_task(hits: &[RankedHit], token_budget: u32, limit: usize) -> TaskContext {
    context_for_task_with_check(hits, token_budget, limit, || {
        Ok::<_, std::convert::Infallible>(())
    })
    .expect("infallible task-context packing")
}

pub fn context_for_task_with_check<E, F>(
    hits: &[RankedHit],
    token_budget: u32,
    limit: usize,
    mut check_active: F,
) -> Result<TaskContext, E>
where
    F: FnMut() -> Result<(), E>,
{
    let mut seen = HashSet::new();
    let mut by_path: BTreeMap<_, Vec<&RankedHit>> = BTreeMap::new();
    for hit in hits {
        check_active()?;
        if seen.insert(hit.chunk.chunk_hash) {
            by_path.entry(hit.chunk.path.clone()).or_default().push(hit);
        }
    }
    for path_hits in by_path.values_mut() {
        check_active()?;
        path_hits.sort_by(|left, right| compare_hits(left, right));
        check_active()?;
    }
    let mut candidates = Vec::new();
    let mut round = 0;
    loop {
        check_active()?;
        let mut current: Vec<_> = by_path
            .values()
            .filter_map(|hits| hits.get(round).copied())
            .collect();
        if current.is_empty() {
            break;
        }
        current.sort_by(|left, right| compare_hits(left, right));
        check_active()?;
        candidates.extend(current);
        round += 1;
    }
    let eligible = candidates.len();
    let mut chunks = Vec::new();
    let mut tokens_used: u32 = 0;
    for hit in candidates.iter().take(limit) {
        check_active()?;
        let snippet_tokens = hit.chunk.snippet.as_str().len().div_ceil(4) as u32;
        let cost = snippet_tokens + if chunks.is_empty() { 0 } else { 2 };
        if tokens_used.saturating_add(cost) <= token_budget {
            tokens_used += cost;
            chunks.push(hit.chunk.clone());
        }
    }
    check_active()?;
    let eligible_unfitted = eligible - chunks.len();
    Ok(TaskContext {
        chunks,
        tokens_used,
        token_budget,
        eligible_unfitted,
        truncated: eligible_unfitted > 0,
    })
}

fn cosine(left: &EmbeddingVector, right: &EmbeddingVector) -> Result<f32, SemanticDomainError> {
    if left.values.len() != right.values.len() {
        return Err(SemanticDomainError::DimensionMismatch {
            expected: left.values.len(),
            actual: right.values.len(),
        });
    }
    let dot: f32 = left
        .values
        .iter()
        .zip(&right.values)
        .map(|(l, r)| l * r)
        .sum();
    let left_norm: f32 = left
        .values
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    let right_norm: f32 = right
        .values
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        return Err(SemanticDomainError::ZeroNormEmbedding);
    }
    let score = dot / (left_norm * right_norm);
    if !score.is_finite() {
        return Err(SemanticDomainError::NonFiniteEmbedding);
    }
    Ok(score)
}

fn tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn compare_hits(left: &RankedHit, right: &RankedHit) -> Ordering {
    right
        .final_score
        .total_cmp(&left.final_score)
        .then_with(|| left.chunk.path.cmp(&right.chunk.path))
        .then_with(|| left.chunk.start_byte.cmp(&right.chunk.start_byte))
        .then_with(|| left.chunk.end_byte.cmp(&right.chunk.end_byte))
        .then_with(|| left.chunk.chunk_hash.cmp(&right.chunk.chunk_hash))
}
