use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::index::ai;

const BM25_WEIGHT: f64 = 0.45;
const VECTOR_WEIGHT: f64 = 0.35;
const SYMBOL_WEIGHT: f64 = 0.10;
const GRAPH_WEIGHT: f64 = 0.05;
const GIT_WEIGHT: f64 = 0.03;
const GITHUB_WEIGHT: f64 = 0.02;

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub path: String,
    pub language: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub symbol_path: Option<String>,
    pub score: f64,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_components: Option<ScoreComponents>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScoreComponents {
    pub bm25: f64,
    pub vector: f64,
    pub symbol: f64,
    pub graph: f64,
    pub git: f64,
    pub github: f64,
}

pub fn search(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        ai::embed_query(conn, query)?,
        false,
    )
}

pub fn search_hash_baseline(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        Some(ai::hash_query_embedding(query)?),
        false,
    )
}

pub fn search_explain(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    search_with_query_embedding(
        conn,
        query,
        limit,
        include_generated,
        ai::embed_query(conn, query)?,
        true,
    )
}

fn search_with_query_embedding(
    conn: &Connection,
    query: &str,
    limit: u32,
    include_generated: bool,
    query_embedding: Option<ai::QueryEmbedding>,
    explain: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    let terms = query_terms(query);
    let candidate_limit = i64::from(limit.max(10)).saturating_mul(8);
    let mut ranked = BTreeMap::<i64, RankedHit>::new();

    for (rank, hit) in
        bm25_candidates(conn, query, candidate_limit, include_generated)?.into_iter().enumerate()
    {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| RankedHit::new(hit));
        entry.components.bm25 = BM25_WEIGHT * (1.0 / (rank as f64 + 1.0));
    }

    for (hit, similarity) in
        vector_candidates(conn, query, candidate_limit, include_generated, query_embedding)?
    {
        let entry = ranked.entry(hit.chunk_id).or_insert_with(|| RankedHit::new(hit));
        entry.components.vector = VECTOR_WEIGHT * f64::from(similarity).clamp(0.0, 1.0);
    }

    let mut hits = ranked
        .into_values()
        .map(|mut hit| {
            let boosts = boosts(conn, &hit.hit, &terms)?;
            hit.components.symbol = SYMBOL_WEIGHT * boosts.symbol;
            hit.components.graph = GRAPH_WEIGHT * boosts.graph;
            hit.components.git = GIT_WEIGHT * boosts.git;
            hit.components.github = GITHUB_WEIGHT * boosts.github;
            Ok(hit.finish(explain))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

struct RankedHit {
    hit: SearchHit,
    components: ScoreComponents,
}

impl RankedHit {
    fn new(hit: SearchHit) -> Self {
        Self { hit, components: ScoreComponents::default() }
    }

    fn finish(mut self, explain: bool) -> SearchHit {
        self.hit.score = self.components.bm25
            + self.components.vector
            + self.components.symbol
            + self.components.graph
            + self.components.git
            + self.components.github;
        if explain {
            self.hit.score_components = Some(self.components);
        }
        self.hit
    }
}

fn bm25_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
) -> anyhow::Result<Vec<SearchHit>> {
    let fts_query = fts_query(query);
    if fts_query == "\"\"" {
        return Ok(Vec::new());
    }
    let generated_filter = if include_generated { "1 = 1" } else { "files.generated = 0" };
    let sql = format!(
        "
        SELECT chunks.id, files.path, files.language, files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               bm25(chunk_fts) AS score,
               chunks.text
        FROM chunk_fts
        JOIN chunks ON chunks.id = chunk_fts.rowid
        JOIN files ON files.id = chunks.file_id
        WHERE chunk_fts MATCH ?1
          AND {generated_filter}
        ORDER BY score
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![fts_query, limit], |row| {
        let text: String = row.get(8)?;
        Ok(SearchHit {
            chunk_id: row.get(0)?,
            path: row.get(1)?,
            language: row.get(2)?,
            kind: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            symbol_path: row.get(6)?,
            score: row.get(7)?,
            summary: snippet(&text, query),
            score_components: None,
        })
    })?;

    collect_rows(rows)
}

fn vector_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
    query_embedding: Option<ai::QueryEmbedding>,
) -> anyhow::Result<Vec<(SearchHit, f32)>> {
    let Some(query_embedding) = query_embedding else {
        return Ok(Vec::new());
    };
    let model_version = ai::active_embedding_model_version(conn, &query_embedding.model_id)?;
    let generated_filter = if include_generated { "1 = 1" } else { "files.generated = 0" };
    let sql = format!(
        "
        SELECT chunks.id, files.path, files.language, files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               chunks.text, chunk_embeddings.vector_blob
        FROM chunk_embeddings
        JOIN ai_models ON ai_models.model_id = chunk_embeddings.model_id
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
        JOIN files ON files.id = chunks.file_id
        WHERE chunk_embeddings.model_id = ?1
          AND ai_models.installed = 1
          AND ai_models.disabled = 0
          AND ai_models.status = 'Ready'
          AND ai_models.embedding_dim = ?2
          AND chunk_embeddings.embedding_dim = ai_models.embedding_dim
          AND chunk_embeddings.status = 'Current'
          AND chunk_embeddings.source_text_hash = chunks.text_hash
          AND chunk_embeddings.model_version = ?3
          AND {generated_filter}
        ",
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            query_embedding.model_id,
            i64::try_from(query_embedding.dim).unwrap_or(i64::MAX),
            model_version
        ],
        |row| {
            let text: String = row.get(7)?;
            let blob: Vec<u8> = row.get(8)?;
            Ok((
                SearchHit {
                    chunk_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    kind: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    symbol_path: row.get(6)?,
                    score: 0.0,
                    summary: snippet(&text, query),
                    score_components: None,
                },
                blob,
            ))
        },
    )?;
    let mut hits = Vec::new();
    for row in rows {
        let (hit, blob) = row?;
        let Some(vector) = ai::decode_vector(&blob, query_embedding.dim) else {
            continue;
        };
        let similarity = dot(&query_embedding.vector, &vector);
        if similarity > 0.0 {
            hits.push((hit, similarity));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

#[derive(Debug, Clone, Default)]
struct BoostComponents {
    symbol: f64,
    graph: f64,
    git: f64,
    github: f64,
}

fn boosts(conn: &Connection, hit: &SearchHit, terms: &[String]) -> anyhow::Result<BoostComponents> {
    let historical = historical_boost(conn, &hit.path)?;
    Ok(BoostComponents {
        symbol: symbol_path_boost(hit, terms),
        graph: graph_boost(conn, hit, terms)?,
        git: historical.git,
        github: historical.github,
    })
}

fn symbol_path_boost(hit: &SearchHit, terms: &[String]) -> f64 {
    let path = hit.path.to_ascii_lowercase();
    let symbol = hit.symbol_path.as_deref().unwrap_or_default().to_ascii_lowercase();
    let mut boost: f64 = 0.0;
    for term in terms {
        if !term.is_empty() && symbol.contains(term) {
            boost += 0.50;
        }
        if !term.is_empty() && path.contains(term) {
            boost += 0.20;
        }
    }
    boost.min(1.0)
}

fn graph_boost(conn: &Connection, hit: &SearchHit, terms: &[String]) -> anyhow::Result<f64> {
    let Some(symbol) = hit.symbol_path.as_deref() else {
        return Ok(0.0);
    };
    let direct = conn.query_row(
        "
        SELECT COUNT(*)
        FROM edges
        WHERE from_name = ?1 OR to_name = ?1
        ",
        [symbol],
        |row| row.get::<_, i64>(0),
    )?;
    let mut boost: f64 = if direct > 0 { 0.70 } else { 0.0 };
    for term in terms {
        let like = format!("%{term}%");
        let related = conn.query_row(
            "
            SELECT COUNT(*)
            FROM edges
            WHERE (from_name = ?1 OR to_name = ?1)
              AND (from_name LIKE ?2 OR to_name LIKE ?2)
            ",
            params![symbol, like],
            |row| row.get::<_, i64>(0),
        )?;
        if related > 0 {
            boost += 0.20;
        }
    }
    Ok(boost.min(1.0))
}

#[derive(Debug, Clone, Default)]
struct HistoricalBoost {
    git: f64,
    github: f64,
}

fn historical_boost(conn: &Connection, path: &str) -> anyhow::Result<HistoricalBoost> {
    let git = conn.query_row(
        "SELECT COUNT(*) FROM git_file_changes WHERE path = ?1 LIMIT 1",
        [path],
        |row| row.get::<_, i64>(0),
    )?;
    let github = conn.query_row(
        "SELECT COUNT(*) FROM github_refs WHERE source_path = ?1 LIMIT 1",
        [path],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(HistoricalBoost {
        git: if git > 0 { 1.0 } else { 0.0 },
        github: if github > 0 { 1.0 } else { 0.0 },
    })
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(left, right)| left * right).sum()
}

fn fts_query(query: &str) -> String {
    let terms = query_terms(query)
        .into_iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() { "\"\"".to_string() } else { terms.join(" OR ") }
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn snippet(text: &str, query: &str) -> String {
    let terms = query_terms(query);
    let lines = text.lines().collect::<Vec<_>>();
    let hit = lines.iter().position(|line| {
        let lower = line.to_ascii_lowercase();
        terms.iter().any(|term| lower.contains(term))
    });
    let start = hit.unwrap_or(0).saturating_sub(1);
    let end = (start + 3).min(lines.len());
    lines[start..end].join("\n")
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
