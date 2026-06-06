use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::index::now_ms;

const EMBEDDING_MODEL_ID: &str = "embedding-small";
const SUMMARY_MODEL_ID: &str = "summary-basic";

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
    pub summary: CapabilityStatus,
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
    pub summaries_written: u64,
    pub blocked_chunks: u64,
    pub status: String,
    pub message: Option<String>,
}

pub fn ensure_model_manifest(conn: &Connection) -> anyhow::Result<()> {
    upsert_model(conn, EMBEDDING_MODEL_ID, "embedding", false)?;
    upsert_model(conn, SUMMARY_MODEL_ID, "summary", true)?;
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
        SUMMARY_MODEL_ID => {
            conn.execute(
                "UPDATE ai_models
                 SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = COALESCE(installed_at_ms, ?2), last_error = NULL
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
        SELECT model_id, capability, installed, disabled, status, installed_at_ms, last_error
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
    let summary = capability_status(conn, "summary", SUMMARY_MODEL_ID, total_chunks)?;
    let current = embedding.current_artifacts + summary.current_artifacts;
    let stale = embedding.stale_artifacts + summary.stale_artifacts;
    let failed = embedding.failed_artifacts + summary.failed_artifacts;
    let blocked = embedding.blocked_artifacts + summary.blocked_artifacts;
    let missing = total_chunks.saturating_mul(2).saturating_sub(current + stale + failed + blocked);
    Ok(LocalAiStatus {
        embedding,
        summary,
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
    let summary_ready = model_ready(conn, SUMMARY_MODEL_ID)?;
    let chunks = current_chunks(conn, limit)?;
    let mut report = ReconcileReport {
        processed_chunks: 0,
        embeddings_written: 0,
        summaries_written: 0,
        blocked_chunks: 0,
        status: "Current".to_string(),
        message: None,
    };

    for chunk in chunks {
        report.processed_chunks += 1;
        if embedding_ready {
            store_embedding(conn, &chunk)?;
            report.embeddings_written += 1;
        } else {
            store_blocked_embedding(conn, &chunk, "MissingModel")?;
            report.blocked_chunks += 1;
        }
        if summary_ready {
            store_summary(conn, &chunk)?;
            report.summaries_written += 1;
        }
    }

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
            summaries_written = ?5,
            blocked_chunks = ?6,
            status = ?7,
            message = ?8
        WHERE id = ?1
        ",
        params![
            attempt_id,
            now_ms(),
            i64::try_from(report.processed_chunks).unwrap_or(i64::MAX),
            i64::try_from(report.embeddings_written).unwrap_or(i64::MAX),
            i64::try_from(report.summaries_written).unwrap_or(i64::MAX),
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
    installed_by_default: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO ai_models(model_id, capability, installed, disabled, status, installed_at_ms)
        VALUES (?1, ?2, ?3, 0, ?4, ?5)
        ON CONFLICT(model_id) DO NOTHING
        ",
        params![
            model_id,
            capability,
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
        SELECT model_id, capability, installed, disabled, status, installed_at_ms, last_error
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
        installed: row.get::<_, bool>(2)?,
        disabled: row.get::<_, bool>(3)?,
        status: row.get(4)?,
        installed_at_ms: row.get(5)?,
        last_error: row.get(6)?,
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

fn store_embedding(conn: &Connection, chunk: &CurrentChunk) -> anyhow::Result<()> {
    let vector = deterministic_embedding(&chunk.text);
    conn.execute(
        "
        INSERT INTO chunk_embeddings(chunk_id, model_id, source_text_hash, vector_blob, status, created_at_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, 'Current', ?5, NULL)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            source_text_hash = excluded.source_text_hash,
            vector_blob = excluded.vector_blob,
            status = excluded.status,
            created_at_ms = excluded.created_at_ms,
            last_error = NULL
        ",
        params![chunk.id, EMBEDDING_MODEL_ID, chunk.text_hash, vector, now_ms()],
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
        INSERT INTO chunk_embeddings(chunk_id, model_id, source_text_hash, vector_blob, status, created_at_ms, last_error)
        VALUES (?1, ?2, ?3, x'', 'Blocked', ?4, ?5)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            source_text_hash = excluded.source_text_hash,
            vector_blob = x'',
            status = 'Blocked',
            created_at_ms = excluded.created_at_ms,
            last_error = excluded.last_error
        ",
        params![chunk.id, EMBEDDING_MODEL_ID, chunk.text_hash, now_ms(), reason],
    )?;
    Ok(())
}

fn store_summary(conn: &Connection, chunk: &CurrentChunk) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO chunk_summaries(chunk_id, model_id, source_text_hash, summary, status, created_at_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, 'Current', ?5, NULL)
        ON CONFLICT(chunk_id) DO UPDATE SET
            model_id = excluded.model_id,
            source_text_hash = excluded.source_text_hash,
            summary = excluded.summary,
            status = excluded.status,
            created_at_ms = excluded.created_at_ms,
            last_error = NULL
        ",
        params![chunk.id, SUMMARY_MODEL_ID, chunk.text_hash, summarize(&chunk.text), now_ms()],
    )?;
    Ok(())
}

fn deterministic_embedding(text: &str) -> Vec<u8> {
    let digest = Sha256::digest(text.as_bytes());
    digest[..16].to_vec()
}

fn summarize(text: &str) -> String {
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty()).take(3);
    lines.collect::<Vec<_>>().join("\n")
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

fn artifact_table_sql(capability: &str, template: &str) -> String {
    let table = match capability {
        "embedding" => "chunk_embeddings",
        "summary" => "chunk_summaries",
        _ => "chunk_summaries",
    };
    template.replace("{table}", table)
}

pub fn current_summary(
    conn: &Connection,
    chunk_id: i64,
    text_hash: &str,
) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "
            SELECT summary
            FROM chunk_summaries
            WHERE chunk_id = ?1
              AND source_text_hash = ?2
              AND status = 'Current'
            ",
            params![chunk_id, text_hash],
            |row| row.get(0),
        )
        .optional()?)
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
