use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::index::now_ms;

pub const HASH_MODEL_ID: &str = "embedding-hash";
pub const FASTEMBED_MODEL_ID: &str = "fastembed-all-minilm-l6-v2";
pub const HASH_EMBEDDING_DIM: usize = 384;
pub const FASTEMBED_EMBEDDING_DIM: usize = 384;
const ACTIVE_EMBEDDING_MODEL_META: &str = "active_embedding_model";
const DEFAULT_BATCH_SIZE: usize = 32;
const LEGACY_MODEL_IDS: &[&str] = &["embedding-small"];

pub trait Embedder {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        HASH_MODEL_ID
    }

    fn dim(&self) -> usize {
        HASH_EMBEDDING_DIM
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| hash_embed_text(text, HASH_EMBEDDING_DIM)).collect())
    }
}

#[cfg(test)]
pub struct MockEmbedder {
    model_id: String,
    dim: usize,
}

#[cfg(test)]
impl MockEmbedder {
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self { model_id: model_id.into(), dim }
    }
}

#[cfg(test)]
impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| hash_embed_text(text, self.dim)).collect())
    }
}

#[cfg(feature = "fastembed")]
pub struct FastEmbedEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "fastembed")]
impl FastEmbedEmbedder {
    pub fn new() -> anyhow::Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(true),
        )?;
        Ok(Self { model: std::sync::Mutex::new(model) })
    }
}

#[cfg(feature = "fastembed")]
impl Embedder for FastEmbedEmbedder {
    fn model_id(&self) -> &str {
        FASTEMBED_MODEL_ID
    }

    fn dim(&self) -> usize {
        FASTEMBED_EMBEDDING_DIM
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let documents = texts.iter().map(String::as_str).collect::<Vec<_>>();
        let mut model =
            self.model.lock().map_err(|_| anyhow::anyhow!("fastembed model lock poisoned"))?;
        Ok(model.embed(documents, None)?)
    }
}

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
    pub model_id: String,
    pub embedding_dim: usize,
    pub batch_size: usize,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub enum ReconcileProgress {
    Started { model_id: String, total_chunks: u64, batch_size: usize },
    Batch { processed_chunks: u64, total_chunks: u64, embeddings_written: u64, blocked_chunks: u64 },
    Finished { processed_chunks: u64, embeddings_written: u64, blocked_chunks: u64 },
}

pub fn ensure_model_manifest(conn: &Connection) -> anyhow::Result<()> {
    remove_legacy_models(conn)?;
    upsert_model(conn, HASH_MODEL_ID, "embedding", Some(HASH_EMBEDDING_DIM), "hash", false)?;
    upsert_model(
        conn,
        FASTEMBED_MODEL_ID,
        "embedding",
        Some(FASTEMBED_EMBEDDING_DIM),
        "fastembed",
        false,
    )?;
    Ok(())
}

fn remove_legacy_models(conn: &Connection) -> anyhow::Result<()> {
    for model_id in LEGACY_MODEL_IDS {
        conn.execute("DELETE FROM chunk_embeddings WHERE model_id = ?1", params![model_id])?;
        conn.execute("DELETE FROM ai_models WHERE model_id = ?1", params![model_id])?;
        conn.execute(
            "DELETE FROM index_meta WHERE key = ?1 AND value = ?2",
            params![ACTIVE_EMBEDDING_MODEL_META, model_id],
        )?;
    }
    Ok(())
}

pub fn install_model(conn: &Connection, model_id: &str) -> anyhow::Result<ModelInfo> {
    ensure_model_manifest(conn)?;
    match model_id {
        HASH_MODEL_ID => {
            conn.execute(
                "UPDATE ai_models
                 SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                     embedding_dim = ?3, runtime = 'hash', last_error = NULL
                 WHERE model_id = ?1",
                params![model_id, now_ms(), i64::try_from(HASH_EMBEDDING_DIM).unwrap_or(i64::MAX)],
            )?;
            set_meta(conn, ACTIVE_EMBEDDING_MODEL_META, model_id)?;
        },
        FASTEMBED_MODEL_ID => {
            install_fastembed_model(conn, model_id)?;
            set_meta(conn, ACTIVE_EMBEDDING_MODEL_META, model_id)?;
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
    let active_model_id = active_embedding_model_id(conn)?;
    let embedding = capability_status(conn, "embedding", &active_model_id, total_chunks)?;
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

pub fn reconcile(
    conn: &Connection,
    limit: Option<u32>,
    batch_size: Option<u32>,
) -> anyhow::Result<ReconcileReport> {
    reconcile_with_progress(conn, limit, batch_size, |_| {})
}

pub fn reconcile_with_progress(
    conn: &Connection,
    limit: Option<u32>,
    batch_size: Option<u32>,
    mut progress: impl FnMut(ReconcileProgress),
) -> anyhow::Result<ReconcileReport> {
    ensure_model_manifest(conn)?;
    let active_model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &active_model_id)?;
    let batch_size = batch_size
        .map(usize::try_from)
        .transpose()?
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let started = now_ms();
    conn.execute(
        "INSERT INTO reconcile_attempts(started_at_ms, limit_count, status) VALUES (?1, ?2, 'Running')",
        params![started, limit.map(i64::from)],
    )?;
    let attempt_id = conn.last_insert_rowid();

    let embedder = active_embedder(conn);
    let chunks = current_chunks(conn, limit)?;
    let mut report = ReconcileReport {
        processed_chunks: 0,
        embeddings_written: 0,
        blocked_chunks: 0,
        model_id: active_model_id.clone(),
        embedding_dim: usize::try_from(model.embedding_dim.unwrap_or_default()).unwrap_or(0),
        batch_size,
        status: "Current".to_string(),
        message: None,
    };

    report.processed_chunks = u64::try_from(chunks.len()).unwrap_or(u64::MAX);
    progress(ReconcileProgress::Started {
        model_id: active_model_id.clone(),
        total_chunks: report.processed_chunks,
        batch_size,
    });
    if let Ok(embedder) = embedder {
        for batch in chunks.chunks(batch_size) {
            let texts = batch.iter().map(|chunk| chunk.text.clone()).collect::<Vec<_>>();
            let vectors = embedder.embed_batch(&texts)?;
            if vectors.len() != batch.len() {
                anyhow::bail!(
                    "embedder {} returned {} vectors for {} texts",
                    embedder.model_id(),
                    vectors.len(),
                    batch.len()
                );
            }
            write_current_embedding_batch(conn, embedder.as_ref(), batch, &vectors)?;
            report.embeddings_written += u64::try_from(batch.len()).unwrap_or(u64::MAX);
            progress(ReconcileProgress::Batch {
                processed_chunks: report.embeddings_written + report.blocked_chunks,
                total_chunks: report.processed_chunks,
                embeddings_written: report.embeddings_written,
                blocked_chunks: report.blocked_chunks,
            });
        }
    } else {
        let reason = model_not_ready_reason(&model);
        for batch in chunks.chunks(batch_size) {
            write_blocked_embedding_batch(
                conn,
                &active_model_id,
                model.embedding_dim,
                batch,
                &reason,
            )?;
            report.blocked_chunks += u64::try_from(batch.len()).unwrap_or(u64::MAX);
            progress(ReconcileProgress::Batch {
                processed_chunks: report.embeddings_written + report.blocked_chunks,
                total_chunks: report.processed_chunks,
                embeddings_written: report.embeddings_written,
                blocked_chunks: report.blocked_chunks,
            });
        }
    };

    if report.blocked_chunks > 0 {
        report.status = "Blocked".to_string();
        report.message = Some(format!(
            "{} model is not ready; run `rag-rat models install {}`",
            active_model_id, active_model_id
        ));
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
    progress(ReconcileProgress::Finished {
        processed_chunks: report.processed_chunks,
        embeddings_written: report.embeddings_written,
        blocked_chunks: report.blocked_chunks,
    });
    Ok(report)
}

fn upsert_model(
    conn: &Connection,
    model_id: &str,
    capability: &str,
    embedding_dim: Option<usize>,
    runtime: &str,
    installed_by_default: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, status, installed_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)
        ON CONFLICT(model_id) DO NOTHING
        ",
        params![
            model_id,
            capability,
            embedding_dim.map(|dim| i64::try_from(dim).unwrap_or(i64::MAX)),
            runtime,
            installed_by_default,
            if installed_by_default { "Ready" } else { "MissingModel" },
            installed_by_default.then(now_ms),
        ],
    )?;
    Ok(())
}

fn install_fastembed_model(conn: &Connection, model_id: &str) -> anyhow::Result<()> {
    #[cfg(feature = "fastembed")]
    {
        let embedder = FastEmbedEmbedder::new()
            .map_err(|err| anyhow::anyhow!("failed to initialize fastembed model: {err}"))?;
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                 embedding_dim = ?3, runtime = 'fastembed', last_error = NULL
             WHERE model_id = ?1",
            params![model_id, now_ms(), i64::try_from(embedder.dim()).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
    #[cfg(not(feature = "fastembed"))]
    {
        conn.execute(
            "UPDATE ai_models
             SET installed = 0, disabled = 0, status = 'MissingRuntime', last_error = ?2
             WHERE model_id = ?1",
            params![
                model_id,
                "fastembed backend is not compiled; rebuild with --features fastembed"
            ],
        )?;
        anyhow::bail!(
            "fastembed backend is not compiled; rebuild rag-rat with --features fastembed"
        )
    }
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

#[derive(Debug)]
struct CurrentChunk {
    id: i64,
    text: String,
    text_hash: String,
}

fn current_chunks(conn: &Connection, limit: Option<u32>) -> anyhow::Result<Vec<CurrentChunk>> {
    let rows = if let Some(limit) = limit {
        let mut stmt =
            conn.prepare("SELECT id, text, text_hash FROM chunks ORDER BY id LIMIT ?1")?;
        let rows = stmt.query_map(params![i64::from(limit)], current_chunk_row)?;
        collect_rows(rows)?
    } else {
        let mut stmt = conn.prepare("SELECT id, text, text_hash FROM chunks ORDER BY id")?;
        let rows = stmt.query_map([], current_chunk_row)?;
        collect_rows(rows)?
    };
    Ok(rows)
}

fn current_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CurrentChunk> {
    Ok(CurrentChunk { id: row.get(0)?, text: row.get(1)?, text_hash: row.get(2)? })
}

fn write_current_embedding_batch(
    conn: &Connection,
    embedder: &dyn Embedder,
    batch: &[CurrentChunk],
    vectors: &[Vec<f32>],
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for (chunk, vector) in batch.iter().zip(vectors) {
            store_embedding(conn, embedder, chunk, vector)?;
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)
}

fn write_blocked_embedding_batch(
    conn: &Connection,
    model_id: &str,
    embedding_dim: Option<i64>,
    batch: &[CurrentChunk],
    reason: &str,
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for chunk in batch {
            store_blocked_embedding(conn, model_id, embedding_dim, chunk, reason)?;
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)
}

fn finish_batch_transaction(conn: &Connection, result: anyhow::Result<()>) -> anyhow::Result<()> {
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        },
    }
}

fn store_embedding(
    conn: &Connection,
    embedder: &dyn Embedder,
    chunk: &CurrentChunk,
    vector: &[f32],
) -> anyhow::Result<()> {
    if vector.len() != embedder.dim() {
        anyhow::bail!(
            "embedding dimension mismatch for {}: got {}, expected {}",
            embedder.model_id(),
            vector.len(),
            embedder.dim()
        );
    }
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
            chunk.id,
            embedder.model_id(),
            chunk.text_hash,
            i64::try_from(embedder.dim()).unwrap_or(i64::MAX),
            encode_vector(vector),
            now_ms()
        ],
    )?;
    Ok(())
}

fn store_blocked_embedding(
    conn: &Connection,
    model_id: &str,
    embedding_dim: Option<i64>,
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
            model_id,
            chunk.text_hash,
            embedding_dim.unwrap_or(0),
            now_ms(),
            reason
        ],
    )?;
    Ok(())
}

pub struct QueryEmbedding {
    pub model_id: String,
    pub dim: usize,
    pub vector: Vec<f32>,
}

pub fn embed_query(conn: &Connection, query: &str) -> anyhow::Result<Option<QueryEmbedding>> {
    ensure_model_manifest(conn)?;
    let Ok(embedder) = active_embedder(conn) else {
        return Ok(None);
    };
    embed_query_with(&*embedder, query).map(Some)
}

pub fn hash_query_embedding(query: &str) -> anyhow::Result<QueryEmbedding> {
    embed_query_with(&HashEmbedder, query)
}

fn embed_query_with(embedder: &dyn Embedder, query: &str) -> anyhow::Result<QueryEmbedding> {
    let texts = vec![query.to_string()];
    let mut vectors = embedder.embed_batch(&texts)?;
    let Some(vector) = vectors.pop() else {
        anyhow::bail!("embedder {} returned no query vector", embedder.model_id());
    };
    if vector.len() != embedder.dim() {
        anyhow::bail!(
            "embedder {} returned query dimension {}, expected {}",
            embedder.model_id(),
            vector.len(),
            embedder.dim()
        );
    }
    Ok(QueryEmbedding { model_id: embedder.model_id().to_string(), dim: embedder.dim(), vector })
}

pub fn active_embedding_model_id(conn: &Connection) -> anyhow::Result<String> {
    ensure_model_manifest(conn)?;
    if let Some(model_id) = meta(conn, ACTIVE_EMBEDDING_MODEL_META)? {
        return Ok(model_id);
    }
    Ok(HASH_MODEL_ID.to_string())
}

pub fn current_embedding_count(conn: &Connection, model_id: &str) -> anyhow::Result<u64> {
    ensure_model_manifest(conn)?;
    let count: i64 = conn.query_row(
        "
        SELECT COUNT(*)
        FROM chunk_embeddings
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
        JOIN ai_models ON ai_models.model_id = chunk_embeddings.model_id
        WHERE chunk_embeddings.model_id = ?1
          AND ai_models.installed = 1
          AND ai_models.disabled = 0
          AND ai_models.status = 'Ready'
          AND chunk_embeddings.embedding_dim = ai_models.embedding_dim
          AND chunk_embeddings.status = 'Current'
          AND chunk_embeddings.source_text_hash = chunks.text_hash
        ",
        params![model_id],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn active_embedder(conn: &Connection) -> anyhow::Result<Box<dyn Embedder>> {
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    validate_ready_model(&model)?;
    match model.model_id.as_str() {
        HASH_MODEL_ID => Ok(Box::new(HashEmbedder)),
        FASTEMBED_MODEL_ID => fastembed_embedder(),
        other => anyhow::bail!("unknown active embedding model `{other}`"),
    }
}

fn validate_ready_model(model: &ModelInfo) -> anyhow::Result<()> {
    if model.disabled {
        anyhow::bail!("model {} is disabled", model.model_id);
    }
    if !model.installed || model.status != "Ready" {
        anyhow::bail!("{}", model_not_ready_reason(model));
    }
    let expected_dim = expected_dim(&model.model_id)
        .ok_or_else(|| anyhow::anyhow!("unknown embedding model `{}`", model.model_id))?;
    if model.embedding_dim != Some(i64::try_from(expected_dim).unwrap_or(i64::MAX)) {
        anyhow::bail!(
            "model {} dimension mismatch: manifest has {:?}, expected {}",
            model.model_id,
            model.embedding_dim,
            expected_dim
        );
    }
    Ok(())
}

fn model_not_ready_reason(model: &ModelInfo) -> String {
    if model.disabled {
        "Disabled".to_string()
    } else if !model.installed {
        "MissingModel".to_string()
    } else {
        model.status.clone()
    }
}

fn expected_dim(model_id: &str) -> Option<usize> {
    match model_id {
        HASH_MODEL_ID => Some(HASH_EMBEDDING_DIM),
        FASTEMBED_MODEL_ID => Some(FASTEMBED_EMBEDDING_DIM),
        _ => None,
    }
}

fn fastembed_embedder() -> anyhow::Result<Box<dyn Embedder>> {
    #[cfg(feature = "fastembed")]
    {
        Ok(Box::new(FastEmbedEmbedder::new()?))
    }
    #[cfg(not(feature = "fastembed"))]
    {
        anyhow::bail!(
            "fastembed backend is not compiled; rebuild rag-rat with --features fastembed"
        )
    }
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

fn hash_embed_text(text: &str, dim: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; dim];
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

fn set_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
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
