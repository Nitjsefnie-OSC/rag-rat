use rayon::prelude::*;
use rusqlite::{Connection, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::index::now_ms;

pub const EMBEDDING_MODEL_ID: &str = "embedding-small";
pub const EMBEDDING_DIM: usize = 384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ArtifactStatus {
    Current,
    Missing,
    Stale,
    Failed,
    Blocked,
    Disabled,
}

impl ArtifactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::Missing => "Missing",
            Self::Stale => "Stale",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
            Self::Disabled => "Disabled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalAiStatus {
    pub embedding: CapabilityStatus,
    pub artifacts: ArtifactCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityStatus {
    pub capability: String,
    pub model_id: String,
    pub state: String,
    pub installed: bool,
    pub disabled: bool,
    pub current_artifacts: u64,
    pub stale_artifacts: u64,
    pub failed_artifacts: u64,
    pub blocked_artifacts: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactCounts {
    pub current: u64,
    pub missing: u64,
    pub stale: u64,
    pub failed: u64,
    pub blocked: u64,
    pub disabled: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub capability: String,
    pub embedding_dim: Option<i64>,
    pub runtime: String,
    pub installed: bool,
    pub disabled: bool,
    pub status: String,
    pub installed_at_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub processed_chunks: u64,
    pub embeddings_written: u64,
    pub blocked_chunks: u64,
    pub status: String,
    pub message: Option<String>,
}

pub fn ensure_model_manifest(conn: &Connection) -> anyhow::Result<()> {
    upsert_model(conn, EMBEDDING_MODEL_ID, "embedding", Some(EMBEDDING_DIM), false)?;
    Ok(())
}

pub fn install_model(conn: &Connection, model_id: &str) -> anyhow::Result<ModelInfo> {
    ensure_model_manifest(conn)?;
    match model_id {
        EMBEDDING_MODEL_ID => {
            conn.execute(
                "UPDATE ai_models
                 SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2, last_error = NULL
                 WHERE model_id = ?1",
                params![model_id, now_ms()],
            )?;
        },
        other => anyhow::bail!("unknown local AI model `{other}`"),
    }
    model(conn, model_id)
}

pub fn models(conn: &Connection) -> anyhow::Result<Vec<ModelInfo>> {
    ensure_model_manifest(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, installed_at_ms, last_error
        FROM ai_models
        ORDER BY capability, model_id
        ",
    )?;
    let rows = stmt.query_map([], model_row)?;
    collect_rows(rows)
}

pub fn status(conn: &Connection) -> anyhow::Result<LocalAiStatus> {
    ensure_model_manifest(conn)?;
    let total_chunks = chunk_count(conn)?;
    let embedding = capability_status(conn, "embedding", EMBEDDING_MODEL_ID, total_chunks)?;
    let current = embedding.current_artifacts;
    let stale = embedding.stale_artifacts;
    let failed = embedding.failed_artifacts;
    let blocked = embedding.blocked_artifacts;
    let missing = total_chunks.saturating_sub(current + stale + failed + blocked);
    Ok(LocalAiStatus {
        embedding,
        artifacts: ArtifactCounts { current, missing, stale, failed, blocked, disabled: 0 },
    })
}

pub fn reconcile(conn: &Connection, limit: Option<u32>) -> anyhow::Result<ReconcileReport> {
    ensure_model_manifest(conn)?;
    let started = now_ms();
    conn.execute(
        "INSERT INTO reconcile_attempts(started_at_ms, limit_count, status) VALUES (?1, ?2, 'Running')",
        params![started, limit.map(i64::from)],
    )?;
    let attempt_id = conn.last_insert_rowid();

    let embedding_ready = model_ready(conn, EMBEDDING_MODEL_ID)?;
    let chunks = current_chunks(conn, limit)?;
    let mut report = ReconcileReport {
        processed_chunks: 0,
        embeddings_written: 0,
        blocked_chunks: 0,
        status: "Current".to_string(),
        message: None,
    };

    report.processed_chunks = u64::try_from(chunks.len()).unwrap_or(u64::MAX);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = if embedding_ready {
        let embeddings = chunks.par_iter().map(PreparedEmbedding::from_chunk).collect::<Vec<_>>();
        for embedding in &embeddings {
            store_prepared_embedding(conn, embedding)?;
        }
        report.embeddings_written = u64::try_from(embeddings.len()).unwrap_or(u64::MAX);
        Ok(())
    } else {
        for chunk in &chunks {
            store_blocked_embedding(conn, chunk, "MissingModel")?;
        }
        report.blocked_chunks = u64::try_from(chunks.len()).unwrap_or(u64::MAX);
        Ok(())
    };
    if let Err(err) = write_result {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(err);
    }
    conn.execute_batch("COMMIT")?;

    if !embedding_ready {
        report.status = "Blocked".to_string();
        report.message = Some(
            "embedding-small model is missing; run `rag-rat models install embedding-small`"
                .to_string(),
        );
    }

    conn.execute(
        "
        UPDATE reconcile_attempts
        SET finished_at_ms = ?2,
            processed_chunks = ?3,
            embeddings_written = ?4,
            blocked_chunks = ?5,
            status = ?6,
            message = ?7
        WHERE id = ?1
        ",
        params![
            attempt_id,
            now_ms(),
            i64::try_from(report.processed_chunks).unwrap_or(i64::MAX),
            i64::try_from(report.embeddings_written).unwrap_or(i64::MAX),
            i64::try_from(report.blocked_chunks).unwrap_or(i64::MAX),
            report.status,
            report.message,
        ],
    )?;
    Ok(report)
}

fn upsert_model(
    conn: &Connection,
    model_id: &str,
    capability: &str,
    embedding_dim: Option<usize>,
    installed_by_default: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, status, installed_at_ms)
        VALUES (?1, ?2, ?3, 'local', ?4, 0, ?5, ?6)
        ON CONFLICT(model_id) DO NOTHING
        ",
        params![
            model_id,
            capability,
            embedding_dim.map(|dim| i64::try_from(dim).unwrap_or(i64::MAX)),
            installed_by_default,
            if installed_by_default { "Ready" } else { "MissingModel" },
            installed_by_default.then(now_ms),
        ],
    )?;
    Ok(())
}

fn capability_status(
    conn: &Connection,
    capability: &str,
    model_id: &str,
    total_chunks: u64,
) -> anyhow::Result<CapabilityStatus> {
    let model = model(conn, model_id)?;
    let current = current_artifact_count(conn, capability, model_id)?;
    let stale = stale_artifact_count(conn, capability, model_id)?;
    let failed = status_artifact_count(conn, capability, model_id, ArtifactStatus::Failed)?;
    let blocked = status_artifact_count(conn, capability, model_id, ArtifactStatus::Blocked)?;
    let state = if model.disabled {
        "Disabled"
    } else if total_chunks == 0 {
        "IndexEmpty"
    } else if !model.installed {
        "MissingModel"
    } else if failed > 0 {
        "Failed"
    } else {
        "Ready"
    };
    Ok(CapabilityStatus {
        capability: capability.to_string(),
        model_id: model_id.to_string(),
        state: state.to_string(),
        installed: model.installed,
        disabled: model.disabled,
        current_artifacts: current,
        stale_artifacts: stale,
        failed_artifacts: failed,
        blocked_artifacts: blocked,
        message: model.last_error,
    })
}

fn model(conn: &Connection, model_id: &str) -> anyhow::Result<ModelInfo> {
    Ok(conn.query_row(
        "
        SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, installed_at_ms, last_error
        FROM ai_models WHERE model_id = ?1
        ",
        [model_id],
        model_row,
    )?)
}

fn model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelInfo> {
    Ok(ModelInfo {
        model_id: row.get(0)?,
        capability: row.get(1)?,
        embedding_dim: row.get(2)?,
        runtime: row.get(3)?,
        installed: row.get::<_, bool>(4)?,
        disabled: row.get::<_, bool>(5)?,
        status: row.get(6)?,
        installed_at_ms: row.get(7)?,
        last_error: row.get(8)?,
    })
}

fn model_ready(conn: &Connection, model_id: &str) -> anyhow::Result<bool> {
    let model = model(conn, model_id)?;
    Ok(model.installed && !model.disabled && model.status == "Ready")
}

#[derive(Debug)]
struct CurrentChunk {
    id: i64,
    text: String,
    text_hash: String,
}

struct PreparedEmbedding {
    chunk_id: i64,
    text_hash: String,
    vector_blob: Vec<u8>,
}

impl PreparedEmbedding {
    fn from_chunk(chunk: &CurrentChunk) -> Self {
        Self {
            chunk_id: chunk.id,
            text_hash: chunk.text_hash.clone(),
            vector_blob: encode_vector(&embed_text(&chunk.text)),
        }
    }
}

fn current_chunks(conn: &Connection, limit: Option<u32>) -> anyhow::Result<Vec<CurrentChunk>> {
    let sql = if limit.is_some() {
        "SELECT id, text, text_hash FROM chunks ORDER BY id LIMIT ?1"
    } else {
        "SELECT id, text, text_hash FROM chunks ORDER BY id LIMIT -1"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([limit.map(i64::from).unwrap_or(-1)], |row| {
        Ok(CurrentChunk { id: row.get(0)?, text: row.get(1)?, text_hash: row.get(2)? })
    })?;
    collect_rows(rows)
}

fn store_prepared_embedding(
    conn: &Connection,
    embedding: &PreparedEmbedding,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO chunk_embeddings(chunk_id, model_id, source_text_hash, embedding_dim, vector_blob, status, created_at_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, ?5, 'Current', ?6, NULL)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            source_text_hash = excluded.source_text_hash,
            embedding_dim = excluded.embedding_dim,
            vector_blob = excluded.vector_blob,
            status = excluded.status,
            created_at_ms = excluded.created_at_ms,
            last_error = NULL
        ",
        params![
            embedding.chunk_id,
            EMBEDDING_MODEL_ID,
            embedding.text_hash,
            i64::try_from(EMBEDDING_DIM).unwrap_or(i64::MAX),
            embedding.vector_blob,
            now_ms()
        ],
    )?;
    Ok(())
}

fn store_blocked_embedding(
    conn: &Connection,
    chunk: &CurrentChunk,
    reason: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO chunk_embeddings(chunk_id, model_id, source_text_hash, embedding_dim, vector_blob, status, created_at_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, x'', 'Blocked', ?5, ?6)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            source_text_hash = excluded.source_text_hash,
            embedding_dim = excluded.embedding_dim,
            vector_blob = x'',
            status = 'Blocked',
            created_at_ms = excluded.created_at_ms,
            last_error = excluded.last_error
        ",
        params![
            chunk.id,
            EMBEDDING_MODEL_ID,
            chunk.text_hash,
            i64::try_from(EMBEDDING_DIM).unwrap_or(i64::MAX),
            now_ms(),
            reason
        ],
    )?;
    Ok(())
}

pub fn embed_query(conn: &Connection, query: &str) -> anyhow::Result<Option<Vec<f32>>> {
    ensure_model_manifest(conn)?;
    let model = model(conn, EMBEDDING_MODEL_ID)?;
    if !model.installed
        || model.disabled
        || model.status != "Ready"
        || model.embedding_dim != Some(i64::try_from(EMBEDDING_DIM).unwrap_or(i64::MAX))
    {
        return Ok(None);
    }
    Ok(Some(embed_text(query)))
}

pub fn decode_vector(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
    if blob.len() != dim.checked_mul(4)? {
        return None;
    }
    let mut out = Vec::with_capacity(dim);
    for bytes in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes(bytes.try_into().ok()?));
    }
    Some(out)
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn embed_text(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; EMBEDDING_DIM];
    let tokens = tokens(text);
    for token in &tokens {
        add_feature(&mut vector, token, 1.0);
    }
    for pair in tokens.windows(2) {
        add_feature(&mut vector, &format!("{}::{}", pair[0], pair[1]), 0.6);
    }
    normalize(&mut vector);
    vector
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|part| !part.is_empty())
        .flat_map(split_identifier)
        .filter(|part| part.len() > 1)
        .collect()
}

fn split_identifier(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for ch in value.chars() {
        if ch == '_' || ch == '-' {
            if !current.is_empty() {
                parts.push(current.to_ascii_lowercase());
                current.clear();
            }
            previous_lower = false;
            continue;
        }
        if previous_lower && ch.is_uppercase() && !current.is_empty() {
            parts.push(current.to_ascii_lowercase());
            current.clear();
        }
        previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current.to_ascii_lowercase());
    }
    parts
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let digest = Sha256::digest(feature.as_bytes());
    let index = u16::from_le_bytes([digest[0], digest[1]]) as usize % vector.len();
    let sign = if digest[2] & 1 == 0 { 1.0 } else { -1.0 };
    vector[index] += sign * weight;
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

fn chunk_count(conn: &Connection) -> anyhow::Result<u64> {
    let count = conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn current_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
) -> anyhow::Result<u64> {
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        WHERE {table}.model_id = ?1
          AND {table}.status = 'Current'
          AND {table}.source_text_hash = chunks.text_hash
    ",
    );
    count_query(conn, &sql, model_id)
}

fn stale_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
) -> anyhow::Result<u64> {
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        WHERE {table}.model_id = ?1
          AND {table}.source_text_hash != chunks.text_hash
    ",
    );
    count_query(conn, &sql, model_id)
}

fn status_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
    status: ArtifactStatus,
) -> anyhow::Result<u64> {
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        WHERE model_id = ?1 AND status = ?2
    ",
    );
    let count =
        conn.query_row(&sql, params![model_id, status.as_str()], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn count_query(conn: &Connection, sql: &str, model_id: &str) -> anyhow::Result<u64> {
    let count = conn.query_row(sql, [model_id], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn artifact_table_sql(_capability: &str, template: &str) -> String {
    let table = "chunk_embeddings";
    template.replace("{table}", table)
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
