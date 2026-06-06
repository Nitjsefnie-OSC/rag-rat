use std::collections::BTreeMap;

use crate::search::lexical::SearchHit;

pub fn reciprocal_rank_fusion(
    mut ranked_lists: Vec<Vec<SearchHit>>,
    limit: usize,
) -> Vec<SearchHit> {
    let mut scores = BTreeMap::<i64, (f64, SearchHit)>::new();
    for hits in &mut ranked_lists {
        for (rank, hit) in hits.drain(..).enumerate() {
            let score = 1.0 / (60.0 + rank as f64 + 1.0);
            scores.entry(hit.chunk_id).and_modify(|entry| entry.0 += score).or_insert((score, hit));
        }
    }
    let mut fused = scores.into_values().collect::<Vec<_>>();
    fused.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    fused
        .into_iter()
        .take(limit)
        .map(|(score, mut hit)| {
            hit.score = score;
            hit
        })
        .collect()
}
