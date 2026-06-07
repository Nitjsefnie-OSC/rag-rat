use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::index::now_ms;

pub const HASH_MODEL_ID: &str = "embedding-hash";
pub const FASTEMBED_MODEL_ID: &str = "fastembed-all-minilm-l6-v2";
pub const FASTEMBED_DISPLAY_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const HASH_EMBEDDING_DIM: usize = 384;
pub const FASTEMBED_EMBEDDING_DIM: usize = 384;
pub const FASTEMBED_MISSING_FEATURE_MESSAGE: &str = "FastEmbed backend requested, but this binary was built without `--features fastembed`.\nRebuild with:\n  cargo install rag-rat --features fastembed";
const ACTIVE_EMBEDDING_MODEL_META: &str = "active_embedding_model";
const ACTIVE_EMBEDDING_MODEL_VERSION_META: &str = "embedding_active_model_version";
const LAST_EMBEDDING_RECONCILE_STARTED_META: &str = "last_embedding_reconcile_started_at_ms";
const LAST_EMBEDDING_RECONCILE_FINISHED_META: &str = "last_embedding_reconcile_finished_at_ms";
const DEFAULT_BATCH_SIZE: usize = 64;
pub const DEFAULT_MAX_EMBEDDING_CHARS: usize = 4_000;
const MIN_EMBEDDING_CHARS: usize = 80;
pub const EMBEDDING_TEXT_VERSION: &str = "embedding-text-v2";
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
            InitOptions::new(EmbeddingModel::AllMiniLML6V2)
                .with_cache_dir(fastembed_cache_dir())
                .with_show_download_progress(true),
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
    pub fastembed: FastEmbedOperationalStatus,
    pub last_reconcile: Option<LastReconcileStatus>,
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
pub struct FastEmbedOperationalStatus {
    pub backend: String,
    pub build_feature_enabled: bool,
    pub model_id: String,
    pub model: String,
    pub dim: usize,
    pub cache: String,
    pub installed: bool,
    pub active: bool,
    pub status: String,
    pub current_embeddings: u64,
    pub stale_embeddings: u64,
    pub missing_embeddings: u64,
    pub failed_embeddings: u64,
    pub failed_retryable_embeddings: u64,
    pub failed_waiting_embeddings: u64,
    pub message: Option<String>,
    pub next: Option<String>,
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
pub struct LastReconcileStatus {
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub batch_size: u64,
    pub processed_chunks: u64,
    pub embeddings_written: u64,
    pub blocked_chunks: u64,
    pub elapsed_ms: u64,
    pub input_chars: u64,
    pub chunks_per_sec: f64,
    pub chars_per_sec: f64,
    pub status: String,
    pub message: Option<String>,
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
    pub skipped_chunks: u64,
    pub failed_chunks: u64,
    pub blocked_chunks: u64,
    pub model_id: String,
    pub model_version: String,
    pub embedding_dim: usize,
    pub batch_size: usize,
    pub max_embedding_chars: usize,
    pub forced: bool,
    pub changed_first: bool,
    pub until_clean: bool,
    pub max_seconds: Option<u64>,
    pub work_reasons: BTreeMap<String, u64>,
    pub skipped_by_policy: BTreeMap<String, u64>,
    pub input_chars: u64,
    pub truncated_inputs: u64,
    pub elapsed_ms: u64,
    pub chunks_per_sec: f64,
    pub chars_per_sec: f64,
    pub avg_chars_per_chunk: f64,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcilePlan {
    pub embeddings: EmbeddingReconcilePlan,
    pub summaries: SummaryReconcilePlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingReconcilePlan {
    pub model_id: String,
    pub model_version: String,
    pub dim: usize,
    pub available: bool,
    pub current: u64,
    pub missing: u64,
    pub stale: u64,
    pub model_changed: u64,
    pub dim_changed: u64,
    pub failed_retryable: u64,
    pub failed_waiting: u64,
    pub blocked: u64,
    pub disabled: u64,
    pub skipped_total: u64,
    pub skipped_by_policy: BTreeMap<String, u64>,
    pub missing_by_priority: BTreeMap<String, u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryReconcilePlan {
    pub enabled: bool,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReconcileReason {
    Missing,
    SourceChanged,
    InputChanged,
    ModelChanged,
    DimChanged,
    RetryAfterFailure,
    Forced,
}

impl ReconcileReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "Missing",
            Self::SourceChanged => "SourceChanged",
            Self::InputChanged => "InputChanged",
            Self::ModelChanged => "ModelChanged",
            Self::DimChanged => "DimChanged",
            Self::RetryAfterFailure => "RetryAfterFailure",
            Self::Forced => "Forced",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    pub limit: Option<u32>,
    pub batch_size: Option<u32>,
    pub force: bool,
    pub until_clean: bool,
    pub changed_first: bool,
    pub max_seconds: Option<u64>,
    pub max_embedding_chars: usize,
}

impl Default for ReconcileOptions {
    fn default() -> Self {
        Self {
            limit: None,
            batch_size: None,
            force: false,
            until_clean: false,
            changed_first: false,
            max_seconds: None,
            max_embedding_chars: DEFAULT_MAX_EMBEDDING_CHARS,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingPolicyDecision {
    pub policy: String,
    pub priority: i64,
    pub eligible: bool,
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
    normalize_embedding_model_versions(conn)?;
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

fn normalize_embedding_model_versions(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "
        UPDATE chunk_embeddings
        SET model_version = CASE model_id
            WHEN 'embedding-hash' THEN 'hash-v1'
            WHEN 'fastembed-all-minilm-l6-v2' THEN 'fastembed-all-minilm-l6-v2-v1'
            ELSE model_version
        END
        WHERE model_version = 'v1'
          AND model_id IN ('embedding-hash', 'fastembed-all-minilm-l6-v2')
        ",
        [],
    )?;
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
    let fastembed = fastembed_operational_status(conn, &active_model_id)?;
    Ok(LocalAiStatus {
        embedding,
        artifacts: ArtifactCounts { current, missing, stale, failed, blocked, disabled: 0 },
        fastembed,
        last_reconcile: last_reconcile_status(conn)?,
    })
}

pub fn reconcile_plan(conn: &Connection) -> anyhow::Result<ReconcilePlan> {
    ensure_model_manifest(conn)?;
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    let model_version = active_embedding_model_version(conn, &model_id)?;
    let dim = usize::try_from(model.embedding_dim.unwrap_or_default()).unwrap_or(0);
    let available = validate_ready_model(&model).is_ok();
    let message = (!available).then(|| model_not_ready_reason(&model));
    Ok(ReconcilePlan {
        embeddings: embedding_reconcile_plan(
            conn,
            &model,
            &model_version,
            dim,
            available,
            message,
        )?,
        summaries: SummaryReconcilePlan {
            enabled: false,
            message: "summaries are not implemented yet".to_string(),
        },
    })
}

fn embedding_reconcile_plan(
    conn: &Connection,
    model: &ModelInfo,
    model_version: &str,
    dim: usize,
    available: bool,
    message: Option<String>,
) -> anyhow::Result<EmbeddingReconcilePlan> {
    let jobs = embedding_job_candidates(conn, &model.model_id, model_version, dim, None, false)?;
    let skipped_by_policy = embedding_policy_skip_summary(conn, DEFAULT_MAX_EMBEDDING_CHARS)?;
    let mut missing_by_priority = BTreeMap::new();
    let mut current = 0_u64;
    let mut missing = 0_u64;
    let mut stale = 0_u64;
    let mut model_changed = 0_u64;
    let mut dim_changed = 0_u64;
    let mut failed_retryable = 0_u64;
    let mut failed_waiting = 0_u64;
    let mut blocked = 0_u64;
    for job in jobs {
        let policy = policy_for_job(&job, DEFAULT_MAX_EMBEDDING_CHARS);
        if !policy.eligible {
            continue;
        }
        let current_artifact = job.embedding_status.as_deref() == Some("Current")
            && job.source_text_hash.as_deref() == Some(job.text_hash.as_str())
            && job.model_version.as_deref() == Some(model_version)
            && job.embedding_dim == Some(i64::try_from(dim).unwrap_or(i64::MAX))
            && job.embedding_text_version.as_deref() == Some(EMBEDDING_TEXT_VERSION)
            && job.input_hash.as_deref().is_some_and(|input_hash| {
                let input = build_embedding_input(&job, DEFAULT_MAX_EMBEDDING_CHARS);
                input_hash == embedding_input_hash(&model.model_id, model_version, &input.text)
            });
        if current_artifact {
            current += 1;
            continue;
        }
        let reason = job.reason(model_version, dim, now_ms(), DEFAULT_MAX_EMBEDDING_CHARS);
        match reason {
            ReconcileReason::Missing => missing += 1,
            ReconcileReason::SourceChanged => stale += 1,
            ReconcileReason::InputChanged => stale += 1,
            ReconcileReason::ModelChanged => model_changed += 1,
            ReconcileReason::DimChanged => dim_changed += 1,
            ReconcileReason::RetryAfterFailure => failed_retryable += 1,
            ReconcileReason::Forced => missing += 1,
        }
        *missing_by_priority.entry(priority_label(policy.priority).to_string()).or_default() += 1;
        if job.embedding_status.as_deref() == Some("Failed")
            && job.next_retry_after_ms.unwrap_or(0) > now_ms()
        {
            failed_waiting += 1;
        }
        if job.embedding_status.as_deref() == Some("Blocked") {
            blocked += 1;
        }
    }
    Ok(EmbeddingReconcilePlan {
        model_id: model.model_id.clone(),
        model_version: model_version.to_string(),
        dim,
        available,
        current,
        missing,
        stale,
        model_changed,
        dim_changed,
        failed_retryable,
        failed_waiting,
        blocked,
        disabled: u64::from(model.disabled),
        skipped_total: skipped_by_policy.values().sum(),
        skipped_by_policy,
        missing_by_priority,
        message,
    })
}

pub fn reconcile(
    conn: &Connection,
    limit: Option<u32>,
    batch_size: Option<u32>,
) -> anyhow::Result<ReconcileReport> {
    reconcile_with_options_progress(
        conn,
        ReconcileOptions { limit, batch_size, ..ReconcileOptions::default() },
        |_| {},
    )
}

pub fn reconcile_with_progress(
    conn: &Connection,
    limit: Option<u32>,
    batch_size: Option<u32>,
    force: bool,
    progress: impl FnMut(ReconcileProgress),
) -> anyhow::Result<ReconcileReport> {
    reconcile_with_options_progress(
        conn,
        ReconcileOptions { limit, batch_size, force, ..ReconcileOptions::default() },
        progress,
    )
}

pub fn reconcile_with_options_progress(
    conn: &Connection,
    options: ReconcileOptions,
    mut progress: impl FnMut(ReconcileProgress),
) -> anyhow::Result<ReconcileReport> {
    ensure_model_manifest(conn)?;
    let active_model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &active_model_id)?;
    let model_version = active_embedding_model_version(conn, &active_model_id)?;
    let embedding_dim = usize::try_from(model.embedding_dim.unwrap_or_default()).unwrap_or(0);
    let batch_size = options
        .batch_size
        .map(usize::try_from)
        .transpose()?
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let max_embedding_chars = options.max_embedding_chars.max(MIN_EMBEDDING_CHARS);
    let started = now_ms();
    set_reconcile_meta(conn, LAST_EMBEDDING_RECONCILE_STARTED_META, &started.to_string())?;
    conn.execute(
        "INSERT INTO reconcile_attempts(started_at_ms, limit_count, status, batch_size) VALUES (?1, ?2, 'Running', ?3)",
        params![
            started,
            options.limit.map(i64::from),
            i64::try_from(batch_size).unwrap_or(i64::MAX)
        ],
    )?;
    let attempt_id = conn.last_insert_rowid();
    let timer = Instant::now();

    let embedder = active_embedder(conn);
    let skipped_by_policy = embedding_policy_skip_summary(conn, max_embedding_chars)?;
    let skipped_chunks = skipped_by_policy.values().sum();
    let mut report = ReconcileReport {
        processed_chunks: 0,
        embeddings_written: 0,
        skipped_chunks,
        failed_chunks: 0,
        blocked_chunks: 0,
        model_id: active_model_id.clone(),
        model_version: model_version.clone(),
        embedding_dim,
        batch_size,
        max_embedding_chars,
        forced: options.force,
        changed_first: options.changed_first,
        until_clean: options.until_clean,
        max_seconds: options.max_seconds,
        work_reasons: BTreeMap::new(),
        skipped_by_policy,
        input_chars: 0,
        truncated_inputs: 0,
        elapsed_ms: 0,
        chunks_per_sec: 0.0,
        chars_per_sec: 0.0,
        avg_chars_per_chunk: 0.0,
        status: "Current".to_string(),
        message: None,
    };

    let embedder = match embedder {
        Ok(embedder) => embedder,
        Err(_) => {
            report.status = "Blocked".to_string();
            report.message = Some(format!(
                "{} model is not ready; run `rag-rat models install {}`",
                active_model_id, active_model_id
            ));
            finish_reconcile_attempt(conn, attempt_id, &report)?;
            progress(ReconcileProgress::Started {
                model_id: active_model_id,
                total_chunks: 0,
                batch_size,
            });
            progress(ReconcileProgress::Finished {
                processed_chunks: 0,
                embeddings_written: 0,
                blocked_chunks: 0,
            });
            return Ok(report);
        },
    };

    let mut progress_total_chunks = estimated_reconcile_jobs(
        conn,
        &active_model_id,
        &model_version,
        embedding_dim,
        &options,
        max_embedding_chars,
    )?;
    progress(ReconcileProgress::Started {
        model_id: active_model_id.clone(),
        total_chunks: progress_total_chunks,
        batch_size,
    });

    let mut remaining = options.limit.map(u64::from);
    loop {
        if remaining == Some(0) {
            break;
        }
        if options.max_seconds.is_some_and(|seconds| timer.elapsed().as_secs() >= seconds) {
            report.status = "Partial".to_string();
            report.message = Some(format!(
                "max_seconds={} reached; rerun reconcile to continue",
                options.max_seconds.unwrap_or_default()
            ));
            break;
        }
        let batch_limit = remaining
            .map(|value| value.min(u64::try_from(batch_size).unwrap_or(u64::MAX)))
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(u32::try_from(batch_size).unwrap_or(u32::MAX));
        let selected = select_reconcile_batch(
            conn,
            &active_model_id,
            &model_version,
            embedding_dim,
            batch_limit,
            &options,
            max_embedding_chars,
        )?;
        if selected.jobs.is_empty() {
            break;
        }
        for job in &selected.jobs {
            *report.work_reasons.entry(job.reason.as_str().to_string()).or_default() += 1;
            report.input_chars = report
                .input_chars
                .saturating_add(u64::try_from(job.input_chars).unwrap_or(u64::MAX));
            if job.input_truncated {
                report.truncated_inputs += 1;
            }
        }
        let jobs_len = selected.jobs.len();
        let mut reused_jobs = Vec::new();
        let mut to_embed_jobs = Vec::new();
        for job in selected.jobs {
            match find_existing_embedding(conn, &active_model_id, &job.input_hash, embedding_dim)? {
                Some(vector) => reused_jobs.push((job, vector)),
                None => to_embed_jobs.push(job),
            }
        }

        if !reused_jobs.is_empty() {
            let (reused_jobs_slice, reused_vectors_slice): (Vec<_>, Vec<_>) =
                reused_jobs.into_iter().unzip();
            write_current_embedding_batch(
                conn,
                embedder.as_ref(),
                &model_version,
                &reused_jobs_slice,
                &reused_vectors_slice,
            )?;
            report.embeddings_written += u64::try_from(reused_jobs_slice.len()).unwrap_or(u64::MAX);
        }

        if !to_embed_jobs.is_empty() {
            let texts =
                to_embed_jobs.iter().map(|chunk| chunk.input_text.clone()).collect::<Vec<_>>();
            match embedder.embed_batch(&texts) {
                Ok(vectors) if vectors.len() == to_embed_jobs.len() => {
                    write_current_embedding_batch(
                        conn,
                        embedder.as_ref(),
                        &model_version,
                        &to_embed_jobs,
                        &vectors,
                    )?;
                    report.embeddings_written +=
                        u64::try_from(to_embed_jobs.len()).unwrap_or(u64::MAX);
                },
                Ok(vectors) => {
                    let error = format!(
                        "embedder {} returned {} vectors for {} texts",
                        embedder.model_id(),
                        vectors.len(),
                        to_embed_jobs.len()
                    );
                    write_failed_embedding_batch(
                        conn,
                        embedder.as_ref(),
                        &model_version,
                        &to_embed_jobs,
                        &error,
                    )?;
                    report.failed_chunks += u64::try_from(to_embed_jobs.len()).unwrap_or(u64::MAX);
                },
                Err(err) => {
                    write_failed_embedding_batch(
                        conn,
                        embedder.as_ref(),
                        &model_version,
                        &to_embed_jobs,
                        &err.to_string(),
                    )?;
                    report.failed_chunks += u64::try_from(to_embed_jobs.len()).unwrap_or(u64::MAX);
                },
            }
        }
        report.processed_chunks = report
            .embeddings_written
            .saturating_add(report.failed_chunks)
            .saturating_add(report.blocked_chunks);
        if let Some(value) = remaining.as_mut() {
            *value = value.saturating_sub(u64::try_from(jobs_len).unwrap_or(0));
        }
        progress_total_chunks = progress_total_chunks.max(report.processed_chunks);
        progress(ReconcileProgress::Batch {
            processed_chunks: report.embeddings_written
                + report.failed_chunks
                + report.blocked_chunks,
            total_chunks: progress_total_chunks,
            embeddings_written: report.embeddings_written,
            blocked_chunks: report.blocked_chunks,
        });
    }
    if report.failed_chunks > 0 {
        report.status = "Failed".to_string();
        report.message =
            Some(format!("{} chunks failed; retry after backoff", report.failed_chunks));
    }
    finalize_reconcile_throughput(&mut report, timer.elapsed().as_millis());

    finish_reconcile_attempt(conn, attempt_id, &report)?;
    progress(ReconcileProgress::Finished {
        processed_chunks: report.processed_chunks,
        embeddings_written: report.embeddings_written,
        blocked_chunks: report.blocked_chunks,
    });
    Ok(report)
}

fn embedding_policy_skip_summary(
    conn: &Connection,
    max_embedding_chars: usize,
) -> anyhow::Result<BTreeMap<String, u64>> {
    let mut skipped_by_policy = BTreeMap::new();
    for chunk in current_chunks(conn, None)? {
        let policy = policy_for_job(&chunk, max_embedding_chars);
        if !policy.eligible {
            *skipped_by_policy.entry(policy.policy).or_default() += 1;
        }
    }
    Ok(skipped_by_policy)
}

fn finish_reconcile_attempt(
    conn: &Connection,
    attempt_id: i64,
    report: &ReconcileReport,
) -> anyhow::Result<()> {
    let finished = now_ms();
    conn.execute(
        "
        UPDATE reconcile_attempts
        SET finished_at_ms = ?2,
            processed_chunks = ?3,
            embeddings_written = ?4,
            blocked_chunks = ?5,
            status = ?6,
            message = ?7,
            elapsed_ms = ?8,
            input_chars = ?9,
            batch_size = ?10
        WHERE id = ?1
        ",
        params![
            attempt_id,
            finished,
            i64::try_from(report.processed_chunks).unwrap_or(i64::MAX),
            i64::try_from(report.embeddings_written).unwrap_or(i64::MAX),
            i64::try_from(report.blocked_chunks).unwrap_or(i64::MAX),
            report.status,
            report.message,
            i64::try_from(report.elapsed_ms).unwrap_or(i64::MAX),
            i64::try_from(report.input_chars).unwrap_or(i64::MAX),
            i64::try_from(report.batch_size).unwrap_or(i64::MAX),
        ],
    )?;
    set_reconcile_meta(conn, LAST_EMBEDDING_RECONCILE_FINISHED_META, &finished.to_string())?;
    Ok(())
}

fn last_reconcile_status(conn: &Connection) -> anyhow::Result<Option<LastReconcileStatus>> {
    conn.query_row(
        "
        SELECT started_at_ms,
               finished_at_ms,
               batch_size,
               processed_chunks,
               embeddings_written,
               blocked_chunks,
               elapsed_ms,
               input_chars,
               status,
               message
        FROM reconcile_attempts
        ORDER BY started_at_ms DESC, id DESC
        LIMIT 1
        ",
        [],
        |row| {
            let elapsed_ms = u64::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
            let input_chars = u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0);
            let embeddings_written = u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0);
            let elapsed_secs = (elapsed_ms as f64 / 1000.0).max(0.001);
            Ok(LastReconcileStatus {
                started_at_ms: row.get(0)?,
                finished_at_ms: row.get(1)?,
                batch_size: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                processed_chunks: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                embeddings_written,
                blocked_chunks: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
                elapsed_ms,
                input_chars,
                chunks_per_sec: embeddings_written as f64 / elapsed_secs,
                chars_per_sec: input_chars as f64 / elapsed_secs,
                status: row.get(8)?,
                message: row.get(9)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn finalize_reconcile_throughput(report: &mut ReconcileReport, elapsed_ms: u128) {
    report.elapsed_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    let elapsed_secs = (report.elapsed_ms as f64 / 1000.0).max(0.001);
    report.chunks_per_sec = report.embeddings_written as f64 / elapsed_secs;
    report.chars_per_sec = report.input_chars as f64 / elapsed_secs;
    report.avg_chars_per_chunk = if report.embeddings_written > 0 {
        report.input_chars as f64 / report.embeddings_written as f64
    } else {
        0.0
    };
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
            params![model_id, FASTEMBED_MISSING_FEATURE_MESSAGE],
        )?;
        anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
    }
}

fn fastembed_operational_status(
    conn: &Connection,
    active_model_id: &str,
) -> anyhow::Result<FastEmbedOperationalStatus> {
    let model = model(conn, FASTEMBED_MODEL_ID)?;
    let model_version = active_embedding_model_version(conn, FASTEMBED_MODEL_ID)?;
    let plan = embedding_reconcile_plan(
        conn,
        &model,
        &model_version,
        FASTEMBED_EMBEDDING_DIM,
        validate_ready_model(&model).is_ok(),
        model.last_error.clone(),
    )?;
    let failed = plan.failed_retryable.saturating_add(plan.failed_waiting);
    Ok(FastEmbedOperationalStatus {
        backend: "fastembed".to_string(),
        build_feature_enabled: fastembed_build_feature_enabled(),
        model_id: FASTEMBED_MODEL_ID.to_string(),
        model: FASTEMBED_DISPLAY_MODEL.to_string(),
        dim: FASTEMBED_EMBEDDING_DIM,
        cache: fastembed_cache_dir().display().to_string(),
        installed: model.installed,
        active: active_model_id == FASTEMBED_MODEL_ID,
        status: model.status,
        current_embeddings: plan.current,
        stale_embeddings: plan
            .stale
            .saturating_add(plan.model_changed)
            .saturating_add(plan.dim_changed),
        missing_embeddings: plan.missing,
        failed_embeddings: failed,
        failed_retryable_embeddings: plan.failed_retryable,
        failed_waiting_embeddings: plan.failed_waiting,
        message: model.last_error,
        next: fastembed_next_command(&plan),
    })
}

fn fastembed_next_command(plan: &EmbeddingReconcilePlan) -> Option<String> {
    if !fastembed_build_feature_enabled() {
        return Some("cargo install rag-rat --features fastembed".to_string());
    }
    if !plan.available {
        return Some(format!("rag-rat models install {}", FASTEMBED_MODEL_ID));
    }
    if plan.missing > 0
        || plan.stale > 0
        || plan.model_changed > 0
        || plan.dim_changed > 0
        || plan.failed_retryable > 0
    {
        return Some("rag-rat reconcile --limit 500".to_string());
    }
    if plan.failed_waiting > 0 {
        return Some("rag-rat reconcile --plan".to_string());
    }
    None
}

fn fastembed_build_feature_enabled() -> bool {
    cfg!(feature = "fastembed")
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

#[derive(Debug, Clone)]
struct CurrentChunk {
    id: i64,
    path: String,
    language: String,
    file_kind: String,
    chunk_kind: String,
    symbol_path: Option<String>,
    text: String,
    text_hash: String,
    embedding_status: Option<String>,
    source_text_hash: Option<String>,
    model_version: Option<String>,
    embedding_dim: Option<i64>,
    input_hash: Option<String>,
    embedding_text_version: Option<String>,
    next_retry_after_ms: Option<i64>,
    reason: ReconcileReason,
}

#[derive(Debug)]
struct PreparedEmbeddingJob {
    id: i64,
    text_hash: String,
    input_text: String,
    input_hash: String,
    input_chars: usize,
    input_truncated: bool,
    policy: String,
    priority: i64,
    reason: ReconcileReason,
}

struct SelectedBatch {
    jobs: Vec<PreparedEmbeddingJob>,
}

fn current_chunks(conn: &Connection, limit: Option<u32>) -> anyhow::Result<Vec<CurrentChunk>> {
    embedding_job_candidates(conn, "", "", 0, limit, false)
}

fn current_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CurrentChunk> {
    Ok(CurrentChunk {
        id: row.get(0)?,
        path: row.get(1)?,
        language: row.get(2)?,
        file_kind: row.get(3)?,
        chunk_kind: row.get(4)?,
        symbol_path: row.get(5)?,
        text: row.get(6)?,
        text_hash: row.get(7)?,
        embedding_status: row.get(8)?,
        source_text_hash: row.get(9)?,
        model_version: row.get(10)?,
        embedding_dim: row.get(11)?,
        input_hash: row.get(12)?,
        embedding_text_version: row.get(13)?,
        next_retry_after_ms: row.get(14)?,
        reason: ReconcileReason::Forced,
    })
}

fn embedding_job_candidates(
    conn: &Connection,
    model_id: &str,
    model_version: &str,
    dim: usize,
    limit: Option<u32>,
    changed_first: bool,
) -> anyhow::Result<Vec<CurrentChunk>> {
    let changed_order = if changed_first {
        "chunks.source_revision DESC,"
    } else {
        "chunks.embedding_priority ASC,"
    };
    let sql = format!(
        "
        SELECT chunks.id,
               files.path,
               files.language,
               files.kind,
               chunks.chunk_kind,
               chunks.symbol_path,
               chunks.text,
               chunks.text_hash,
               chunk_embeddings.status,
               chunk_embeddings.source_text_hash,
               chunk_embeddings.model_version,
               chunk_embeddings.embedding_dim,
               chunk_embeddings.input_hash,
               chunk_embeddings.embedding_text_version,
               chunk_embeddings.next_retry_after_ms
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        LEFT JOIN chunk_embeddings
          ON chunk_embeddings.chunk_id = chunks.id
         AND chunk_embeddings.model_id = ?1
        WHERE chunks.text IS NOT NULL
        ORDER BY
          CASE
            WHEN chunk_embeddings.chunk_id IS NULL THEN 0
            WHEN chunk_embeddings.source_text_hash != chunks.text_hash THEN 1
            WHEN chunk_embeddings.status = 'Failed' THEN 2
            ELSE 3
          END,
          {changed_order}
          chunks.id
        LIMIT ?5
    "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            model_id,
            model_version,
            i64::try_from(dim).unwrap_or(i64::MAX),
            now_ms(),
            limit.map(i64::from).unwrap_or(i64::MAX)
        ],
        current_chunk_row,
    )?;
    let mut chunks = collect_rows(rows)?;
    if !model_id.is_empty() {
        for chunk in &mut chunks {
            chunk.reason = ReconcileReason::Missing;
        }
    }
    Ok(chunks)
}

impl CurrentChunk {
    fn reason(
        &self,
        model_version: &str,
        dim: usize,
        now_ms: i64,
        _max_embedding_chars: usize,
    ) -> ReconcileReason {
        if self.reason == ReconcileReason::Forced {
            return ReconcileReason::Forced;
        }
        if self.embedding_status.is_none() {
            return ReconcileReason::Missing;
        }
        if self.source_text_hash.as_deref() != Some(self.text_hash.as_str()) {
            return ReconcileReason::SourceChanged;
        }
        if self.input_hash.as_deref().is_none_or(str::is_empty) {
            return ReconcileReason::InputChanged;
        }
        if self.model_version.as_deref() != Some(model_version)
            || self.embedding_text_version.as_deref() != Some(EMBEDDING_TEXT_VERSION)
        {
            return ReconcileReason::ModelChanged;
        }
        if self.embedding_dim != Some(i64::try_from(dim).unwrap_or(i64::MAX)) {
            return ReconcileReason::DimChanged;
        }
        if self.embedding_status.as_deref() == Some("Failed")
            && self.next_retry_after_ms.unwrap_or(0) <= now_ms
        {
            return ReconcileReason::RetryAfterFailure;
        }
        ReconcileReason::Missing
    }
}

fn estimated_reconcile_jobs(
    conn: &Connection,
    model_id: &str,
    model_version: &str,
    dim: usize,
    options: &ReconcileOptions,
    max_embedding_chars: usize,
) -> anyhow::Result<u64> {
    let candidates = if options.force {
        current_chunks(conn, options.limit)?
    } else {
        embedding_job_candidates(
            conn,
            model_id,
            model_version,
            dim,
            options.limit,
            options.changed_first,
        )?
    };
    let count = candidates
        .iter()
        .filter(|candidate| {
            policy_for_job(candidate, max_embedding_chars).eligible
                && (options.force
                    || needs_embedding(
                        candidate,
                        model_id,
                        model_version,
                        dim,
                        max_embedding_chars,
                    ))
        })
        .count();
    Ok(u64::try_from(count).unwrap_or(u64::MAX))
}

fn select_reconcile_batch(
    conn: &Connection,
    model_id: &str,
    model_version: &str,
    dim: usize,
    limit: u32,
    options: &ReconcileOptions,
    max_embedding_chars: usize,
) -> anyhow::Result<SelectedBatch> {
    let scan_limit = options.limit.unwrap_or(100_000).max(limit);
    let candidates = if options.force {
        current_chunks(conn, Some(scan_limit))?
    } else {
        embedding_job_candidates(
            conn,
            model_id,
            model_version,
            dim,
            Some(scan_limit),
            options.changed_first,
        )?
    };
    let mut jobs = Vec::new();
    for candidate in candidates {
        let policy = policy_for_job(&candidate, max_embedding_chars);
        if !policy.eligible {
            continue;
        }
        if !options.force
            && !needs_embedding(&candidate, model_id, model_version, dim, max_embedding_chars)
        {
            continue;
        }
        let input = build_embedding_input(&candidate, max_embedding_chars);
        let reason = if options.force {
            ReconcileReason::Forced
        } else {
            candidate.reason(model_version, dim, now_ms(), max_embedding_chars)
        };
        jobs.push(PreparedEmbeddingJob {
            id: candidate.id,
            text_hash: candidate.text_hash,
            input_hash: embedding_input_hash(model_id, model_version, &input.text),
            input_chars: input.chars,
            input_truncated: input.truncated,
            input_text: input.text,
            policy: policy.policy,
            priority: policy.priority,
            reason,
        });
        if jobs.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
    }
    Ok(SelectedBatch { jobs })
}

fn needs_embedding(
    chunk: &CurrentChunk,
    model_id: &str,
    model_version: &str,
    dim: usize,
    max_embedding_chars: usize,
) -> bool {
    let input = build_embedding_input(chunk, max_embedding_chars);
    let expected_input_hash = embedding_input_hash(model_id, model_version, &input.text);
    chunk.embedding_status.as_deref() != Some("Current")
        || chunk.source_text_hash.as_deref() != Some(chunk.text_hash.as_str())
        || chunk.model_version.as_deref() != Some(model_version)
        || chunk.embedding_dim != Some(i64::try_from(dim).unwrap_or(i64::MAX))
        || chunk.input_hash.as_deref() != Some(expected_input_hash.as_str())
        || chunk.embedding_text_version.as_deref() != Some(EMBEDDING_TEXT_VERSION)
}

pub fn embedding_policy_for_chunk(
    path: &Path,
    language: &str,
    file_kind: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
    text: &str,
    max_embedding_chars: usize,
) -> EmbeddingPolicyDecision {
    let path_text = path.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.chars().count() > max_embedding_chars.saturating_mul(4)
        && (file_kind == "generated" || chunk_kind == "generated" || symbol_path.is_none())
    {
        return policy("SkipTooLarge", 9, false);
    }
    if file_kind == "generated" || chunk_kind == "generated" || looks_generated_path(&path_text) {
        return policy("SkipGenerated", 9, false);
    }
    if is_test_fixture_path(&path_text) {
        return policy("SkipTestFixture", 9, false);
    }
    if !matches!(language, "rust" | "typescript" | "kotlin" | "markdown") {
        return policy("SkipLanguageUnsupported", 9, false);
    }
    if trimmed.chars().count() < MIN_EMBEDDING_CHARS {
        return policy("SkipTooSmall", 9, false);
    }
    if is_low_signal_chunk(language, chunk_kind, symbol_path, trimmed) {
        return policy("SkipLowSignal", 9, false);
    }
    policy("Embed", embedding_priority(&path_text, language, chunk_kind, symbol_path), true)
}

fn policy(name: &str, priority: i64, eligible: bool) -> EmbeddingPolicyDecision {
    EmbeddingPolicyDecision { policy: name.to_string(), priority, eligible }
}

fn policy_for_job(chunk: &CurrentChunk, max_embedding_chars: usize) -> EmbeddingPolicyDecision {
    embedding_policy_for_chunk(
        Path::new(&chunk.path),
        &chunk.language,
        &chunk.file_kind,
        &chunk.chunk_kind,
        chunk.symbol_path.as_deref(),
        &chunk.text,
        max_embedding_chars,
    )
}

fn embedding_priority(
    path: &str,
    language: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
) -> i64 {
    if symbol_path.is_some()
        && matches!(chunk_kind, "code")
        && !is_test_path(path)
        && language != "markdown"
    {
        return 0;
    }
    if language == "markdown" {
        return 1;
    }
    if is_test_path(path) {
        return 2;
    }
    1
}

fn priority_label(priority: i64) -> &'static str {
    match priority {
        0 => "source_symbols",
        1 => "source_or_docs",
        2 => "tests",
        3 => "low_signal",
        9 => "skipped",
        _ => "other",
    }
}

fn looks_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/src/generated/")
        || path.contains("/target/")
        || path.ends_with("Cargo.lock")
        || path.ends_with("package-lock.json")
        || path.ends_with("pnpm-lock.yaml")
}

fn is_test_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("__tests__")
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with(".spec.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.tsx")
}

fn is_test_fixture_path(path: &str) -> bool {
    path.contains("/fixtures/")
        || path.contains("/__fixtures__/")
        || path.contains("/testdata/")
        || path.contains("/snapshots/")
        || path.ends_with(".snap")
}

fn is_low_signal_chunk(
    language: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
    text: &str,
) -> bool {
    if language == "markdown" {
        return false;
    }
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with("/*"))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return true;
    }
    if symbol_path.is_none() && chunk_kind == "code" && lines.len() <= 3 {
        return true;
    }
    lines.iter().all(|line| {
        line.starts_with("use ")
            || line.starts_with("pub use ")
            || line.starts_with("import ")
            || line.starts_with("export ")
            || line.starts_with("mod ")
            || line.starts_with("pub mod ")
            || *line == "}"
            || *line == "{"
    })
}

struct EmbeddingInput {
    text: String,
    chars: usize,
    truncated: bool,
}

fn build_embedding_input(chunk: &CurrentChunk, max_chars: usize) -> EmbeddingInput {
    let mut input = String::new();
    input.push_str("path: ");
    input.push_str(&chunk.path);
    input.push('\n');
    input.push_str("language: ");
    input.push_str(&chunk.language);
    input.push('\n');
    input.push_str("kind: ");
    input.push_str(&chunk.chunk_kind);
    input.push('\n');
    if let Some(symbol_path) = &chunk.symbol_path {
        input.push_str("symbol: ");
        input.push_str(symbol_path);
        input.push('\n');
    }
    input.push_str("body:\n");
    let prefix_chars = input.chars().count();
    let budget = max_chars.saturating_sub(prefix_chars).max(MIN_EMBEDDING_CHARS);
    let (body, truncated) = truncate_chars(&chunk.text, budget);
    input.push_str(&body);
    let chars = input.chars().count();
    EmbeddingInput { text: input, chars, truncated }
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

fn embedding_input_hash(model_id: &str, model_version: &str, input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_version.as_bytes());
    hasher.update(b"\0");
    hasher.update(EMBEDDING_TEXT_VERSION.as_bytes());
    hasher.update(b"\0");
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn write_current_embedding_batch(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    batch: &[PreparedEmbeddingJob],
    vectors: &[Vec<f32>],
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for (chunk, vector) in batch.iter().zip(vectors) {
            store_embedding(conn, embedder, model_version, chunk, vector)?;
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)
}

fn write_failed_embedding_batch(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    batch: &[PreparedEmbeddingJob],
    error: &str,
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for chunk in batch {
            store_failed_embedding(conn, embedder, model_version, chunk, error)?;
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
    model_version: &str,
    chunk: &PreparedEmbeddingJob,
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
        INSERT INTO chunk_embeddings(
            chunk_id, model_id, model_version, source_text_hash, input_hash,
            embedding_text_version, embedding_policy, embedding_priority, input_chars,
            input_truncated, embedding_dim, vector_blob,
            status, attempt_count, last_error_class, next_retry_after_ms, computed_at_ms,
            created_at_ms, last_error
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'Current', 1, NULL, NULL, ?13, ?13, NULL)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            model_version = excluded.model_version,
            source_text_hash = excluded.source_text_hash,
            input_hash = excluded.input_hash,
            embedding_text_version = excluded.embedding_text_version,
            embedding_policy = excluded.embedding_policy,
            embedding_priority = excluded.embedding_priority,
            input_chars = excluded.input_chars,
            input_truncated = excluded.input_truncated,
            embedding_dim = excluded.embedding_dim,
            vector_blob = excluded.vector_blob,
            status = excluded.status,
            attempt_count = chunk_embeddings.attempt_count + 1,
            last_error_class = NULL,
            next_retry_after_ms = NULL,
            computed_at_ms = excluded.computed_at_ms,
            created_at_ms = excluded.created_at_ms,
            last_error = NULL
        ",
        params![
            chunk.id,
            embedder.model_id(),
            model_version,
            chunk.text_hash,
            chunk.input_hash,
            EMBEDDING_TEXT_VERSION,
            chunk.policy,
            chunk.priority,
            i64::try_from(chunk.input_chars).unwrap_or(i64::MAX),
            chunk.input_truncated,
            i64::try_from(embedder.dim()).unwrap_or(i64::MAX),
            encode_vector(vector),
            now_ms()
        ],
    )?;
    Ok(())
}

fn store_failed_embedding(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    chunk: &PreparedEmbeddingJob,
    error: &str,
) -> anyhow::Result<()> {
    let retry_at = now_ms().saturating_add(60_000);
    conn.execute(
        "
        INSERT INTO chunk_embeddings(
            chunk_id, model_id, model_version, source_text_hash, input_hash,
            embedding_text_version, embedding_policy, embedding_priority, input_chars,
            input_truncated, embedding_dim, vector_blob,
            status, attempt_count, last_error_class, next_retry_after_ms, computed_at_ms,
            created_at_ms, last_error
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, x'', 'Failed', 1, ?12, ?13, NULL, ?14, ?15)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            model_version = excluded.model_version,
            source_text_hash = excluded.source_text_hash,
            input_hash = excluded.input_hash,
            embedding_text_version = excluded.embedding_text_version,
            embedding_policy = excluded.embedding_policy,
            embedding_priority = excluded.embedding_priority,
            input_chars = excluded.input_chars,
            input_truncated = excluded.input_truncated,
            embedding_dim = excluded.embedding_dim,
            vector_blob = x'',
            status = 'Failed',
            attempt_count = chunk_embeddings.attempt_count + 1,
            last_error_class = excluded.last_error_class,
            next_retry_after_ms = excluded.next_retry_after_ms,
            computed_at_ms = NULL,
            created_at_ms = excluded.created_at_ms,
            last_error = excluded.last_error
        ",
        params![
            chunk.id,
            embedder.model_id(),
            model_version,
            chunk.text_hash,
            chunk.input_hash,
            EMBEDDING_TEXT_VERSION,
            chunk.policy,
            chunk.priority,
            i64::try_from(chunk.input_chars).unwrap_or(i64::MAX),
            chunk.input_truncated,
            i64::try_from(embedder.dim()).unwrap_or(i64::MAX),
            "Transient",
            retry_at,
            now_ms(),
            error
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

pub fn active_embedding_model_version(conn: &Connection, model_id: &str) -> anyhow::Result<String> {
    if let Some(version) = reconcile_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META)? {
        return Ok(version);
    }
    Ok(default_model_version(model_id).to_string())
}

fn default_model_version(model_id: &str) -> &'static str {
    match model_id {
        HASH_MODEL_ID => "hash-v1",
        FASTEMBED_MODEL_ID => "fastembed-all-minilm-l6-v2-v1",
        _ => "v1",
    }
}

pub fn current_embedding_count(conn: &Connection, model_id: &str) -> anyhow::Result<u64> {
    ensure_model_manifest(conn)?;
    let model_version = active_embedding_model_version(conn, model_id)?;
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
          AND chunk_embeddings.model_version = ?2
          AND chunk_embeddings.embedding_text_version = ?3
          AND chunk_embeddings.input_hash != ''
        ",
        params![model_id, model_version, EMBEDDING_TEXT_VERSION],
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
    } else if let Some(last_error) = &model.last_error {
        last_error.clone()
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
        anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
    }
}

pub fn fastembed_cache_dir() -> PathBuf {
    if let Ok(cache) = std::env::var("RAG_RAT_MODEL_CACHE") {
        return PathBuf::from(cache);
    }
    if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(cache).join("rag-rat").join("models");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache").join("rag-rat").join("models");
    }
    PathBuf::from(".rag-rat").join("models")
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
    let model_version = active_embedding_model_version(conn, model_id)?;
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        JOIN ai_models ON ai_models.model_id = {table}.model_id
        WHERE {table}.model_id = ?1
          AND {table}.status = 'Current'
          AND {table}.source_text_hash = chunks.text_hash
          AND {table}.model_version = ?2
          AND {table}.embedding_text_version = ?3
          AND {table}.input_hash != ''
          AND {table}.embedding_dim = ai_models.embedding_dim
    ",
    );
    count_query3(conn, &sql, model_id, &model_version, EMBEDDING_TEXT_VERSION)
}

fn stale_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
) -> anyhow::Result<u64> {
    let model_version = active_embedding_model_version(conn, model_id)?;
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        JOIN ai_models ON ai_models.model_id = {table}.model_id
        WHERE {table}.model_id = ?1
          AND (
            {table}.source_text_hash != chunks.text_hash
            OR {table}.model_version != ?2
            OR {table}.embedding_text_version != ?3
            OR {table}.input_hash = ''
            OR {table}.embedding_dim != ai_models.embedding_dim
            OR {table}.status = 'Stale'
          )
    ",
    );
    count_query3(conn, &sql, model_id, &model_version, EMBEDDING_TEXT_VERSION)
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

fn count_query3(
    conn: &Connection,
    sql: &str,
    model_id: &str,
    left: &str,
    right: &str,
) -> anyhow::Result<u64> {
    let count = conn.query_row(sql, params![model_id, left, right], |row| row.get::<_, i64>(0))?;
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

fn set_reconcile_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO reconcile_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn reconcile_meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM reconcile_meta WHERE key = ?1", [key], |row| row.get(0))
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

fn find_existing_embedding(
    conn: &Connection,
    model_id: &str,
    input_hash: &str,
    dim: usize,
) -> anyhow::Result<Option<Vec<f32>>> {
    let vector: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vector_blob FROM chunk_embeddings
         WHERE model_id = ?1 AND input_hash = ?2 AND status = 'Current' AND embedding_dim = ?3
         LIMIT 1",
            params![model_id, input_hash, i64::try_from(dim).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(blob) = vector { Ok(decode_vector(&blob, dim)) } else { Ok(None) }
}
