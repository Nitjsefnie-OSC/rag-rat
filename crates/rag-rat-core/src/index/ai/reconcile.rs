use super::*;

pub(crate) fn ensure_model_manifest(conn: &Connection) -> anyhow::Result<()> {
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
    upsert_model(
        conn,
        MODEL2VEC_MODEL_ID,
        "embedding",
        Some(MODEL2VEC_EMBEDDING_DIM),
        "model2vec",
        false,
    )?;
    normalize_embedding_model_versions(conn)?;
    Ok(())
}

pub(crate) fn remove_legacy_models(conn: &Connection) -> anyhow::Result<()> {
    for model_id in LEGACY_MODEL_IDS {
        conn.execute("DELETE FROM chunk_embeddings WHERE model_id = ?1", params![model_id])?;
        conn.execute("DELETE FROM ai_models WHERE model_id = ?1", params![model_id])?;
        conn.execute("DELETE FROM index_meta WHERE key = ?1 AND value = ?2", params![
            ACTIVE_EMBEDDING_MODEL_META,
            model_id
        ])?;
    }
    Ok(())
}

pub(crate) fn normalize_embedding_model_versions(conn: &Connection) -> anyhow::Result<()> {
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

pub(crate) fn recover_cached_fastembed_model(conn: &Connection) -> anyhow::Result<()> {
    recover_cached_fastembed_model_from(conn, &fastembed_cache_dir())
}

pub(crate) fn recover_cached_fastembed_model_from(
    conn: &Connection,
    cache_dir: &Path,
) -> anyhow::Result<()> {
    #[cfg(feature = "fastembed")]
    {
        recover_cached_fastembed_model_at(conn, cache_dir)?;
    }
    #[cfg(not(feature = "fastembed"))]
    {
        let _ = (conn, cache_dir);
    }
    Ok(())
}

#[cfg(feature = "fastembed")]
pub(crate) fn recover_cached_fastembed_model_at(
    conn: &Connection,
    cache_dir: &Path,
) -> anyhow::Result<()> {
    if !fastembed_cache_ready(cache_dir) {
        return Ok(());
    }
    let fastembed = model(conn, FASTEMBED_MODEL_ID)?;
    if !fastembed.installed || fastembed.status != "Ready" {
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                 embedding_dim = ?3, runtime = 'fastembed', last_error = NULL
             WHERE model_id = ?1",
            params![
                FASTEMBED_MODEL_ID,
                now_ms(),
                i64::try_from(FASTEMBED_EMBEDDING_DIM).unwrap_or(i64::MAX)
            ],
        )?;
    }
    if active_embedding_model_is_missing(conn)? {
        set_meta(conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID)?;
    }
    Ok(())
}

#[cfg(feature = "fastembed")]
pub(crate) fn active_embedding_model_is_missing(conn: &Connection) -> anyhow::Result<bool> {
    let Some(active_model_id) = meta(conn, ACTIVE_EMBEDDING_MODEL_META)? else {
        return Ok(true);
    };
    let active = conn
        .query_row(
            "
            SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, \
             installed_at_ms, last_error
            FROM ai_models WHERE model_id = ?1
            ",
            [active_model_id],
            model_row,
        )
        .optional()?;
    Ok(match active {
        Some(active) => validate_ready_model(&active).is_err(),
        None => true,
    })
}

#[cfg(feature = "fastembed")]
pub(crate) fn fastembed_cache_ready(cache_dir: &Path) -> bool {
    let repo = cache_dir.join(FASTEMBED_HF_CACHE_REPO_DIR);
    let Ok(revision) = std::fs::read_to_string(repo.join("refs").join("main")) else {
        return false;
    };
    let revision = revision.trim();
    !revision.is_empty() && repo.join("snapshots").join(revision).is_dir()
}

pub(crate) fn install_model(conn: &Connection, model_id: &str) -> anyhow::Result<ModelInfo> {
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
        MODEL2VEC_MODEL_ID => {
            install_model2vec_model(conn, model_id)?;
            set_meta(conn, ACTIVE_EMBEDDING_MODEL_META, model_id)?;
        },
        other => anyhow::bail!("unknown local AI model `{other}`"),
    }
    model(conn, model_id)
}

pub(crate) fn models(conn: &Connection) -> anyhow::Result<Vec<ModelInfo>> {
    ensure_model_manifest(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, \
         installed_at_ms, last_error
        FROM ai_models
        ORDER BY capability, model_id
        ",
    )?;
    let rows = stmt.query_map([], model_row)?;
    collect_rows(rows)
}

pub(crate) fn status(conn: &Connection) -> anyhow::Result<LocalAiStatus> {
    ensure_model_manifest(conn)?;
    let total_chunks = chunk_count(conn)?;
    let active_model_id = active_embedding_model_id(conn)?;
    let embedding = capability_status(conn, "embedding", &active_model_id, total_chunks)?;
    let fastembed = fastembed_operational_status(conn, &active_model_id)?;
    let current = embedding.current_artifacts;
    let stale = embedding.stale_artifacts;
    let failed = embedding.failed_artifacts;
    let blocked = embedding.blocked_artifacts;
    let missing = total_chunks.saturating_sub(current + stale + failed + blocked);
    let skipped_chunks = fastembed.skipped_embeddings;
    let eligible_chunks = total_chunks.saturating_sub(skipped_chunks);
    Ok(LocalAiStatus {
        embedding,
        artifacts: ArtifactCounts {
            total_chunks,
            eligible_chunks,
            skipped_chunks,
            current,
            missing,
            stale,
            failed,
            blocked,
            disabled: 0,
        },
        fastembed,
        last_reconcile: last_reconcile_status(conn)?,
    })
}

pub(crate) fn reconcile_plan(conn: &Connection) -> anyhow::Result<ReconcilePlan> {
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

pub(crate) fn embedding_reconcile_plan(
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

pub(crate) fn reconcile(
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

pub(crate) fn reconcile_with_progress(
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

pub(crate) fn reconcile_with_options_progress(
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
        "INSERT INTO reconcile_attempts(started_at_ms, limit_count, status, batch_size) VALUES \
         (?1, ?2, 'Running', ?3)",
        params![
            started,
            options.limit.map(i64::from),
            i64::try_from(batch_size).unwrap_or(i64::MAX)
        ],
    )?;
    let attempt_id = conn.last_insert_rowid();
    let timer = Instant::now();

    let embedder = active_embedder(conn, options.intra_threads);
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

    let scan = EmbeddingScan {
        model_id: &active_model_id,
        model_version: &model_version,
        dim: embedding_dim,
        max_embedding_chars,
    };
    let mut progress_total_chunks = estimated_reconcile_jobs(conn, &scan, &options)?;
    progress(ReconcileProgress::Started {
        model_id: active_model_id.clone(),
        total_chunks: progress_total_chunks,
        batch_size,
    });

    // Ordered candidate ids fetched ONCE (ids only, need-first). The loop walks them with a cursor
    // and loads text per batch, so each chunk's text is read at most once — see
    // `embedding_candidate_ids`. The processed set guards against a chunk being revisited (e.g.
    // under --force, whose ordering does not reflect embedding state).
    let candidate_ids = embedding_candidate_ids(
        conn,
        if options.force { "" } else { scan.model_id },
        options.changed_first,
    )?;
    let mut cursor = 0usize;
    let mut processed_ids: HashSet<i64> = HashSet::new();
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
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(batch_size);
        // Pull the next batch of unprocessed candidate ids.
        let mut batch_ids = Vec::with_capacity(batch_limit);
        while cursor < candidate_ids.len() && batch_ids.len() < batch_limit {
            let id = candidate_ids[cursor];
            cursor += 1;
            if !processed_ids.contains(&id) {
                batch_ids.push(id);
            }
        }
        if batch_ids.is_empty() {
            break; // candidate list exhausted
        }
        let selected = select_reconcile_batch(conn, &scan, &batch_ids, &options)?;
        if selected.jobs.is_empty() {
            // Every id in this batch was filtered (ineligible/already current); keep walking the
            // rest of the candidate list rather than stopping.
            continue;
        }
        for job in &selected.jobs {
            processed_ids.insert(job.id);
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

pub(crate) fn embedding_policy_skip_summary(
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

pub(crate) fn finish_reconcile_attempt(
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

pub(crate) fn last_reconcile_status(
    conn: &Connection,
) -> anyhow::Result<Option<LastReconcileStatus>> {
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

pub(crate) fn finalize_reconcile_throughput(report: &mut ReconcileReport, elapsed_ms: u128) {
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

pub(crate) fn upsert_model(
    conn: &Connection,
    model_id: &str,
    capability: &str,
    embedding_dim: Option<usize>,
    runtime: &str,
    installed_by_default: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, \
         status, installed_at_ms)
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

pub(crate) fn install_model2vec_model(conn: &Connection, model_id: &str) -> anyhow::Result<()> {
    #[cfg(feature = "model2vec")]
    {
        let embedder = Model2VecEmbedder::new()?;
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                 embedding_dim = ?3, runtime = 'model2vec', last_error = NULL
             WHERE model_id = ?1",
            params![model_id, now_ms(), i64::try_from(embedder.dim()).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
    #[cfg(not(feature = "model2vec"))]
    {
        conn.execute(
            "UPDATE ai_models
             SET installed = 0, disabled = 0, status = 'MissingRuntime', last_error = ?2
             WHERE model_id = ?1",
            params![model_id, MODEL2VEC_MISSING_FEATURE_MESSAGE],
        )?;
        anyhow::bail!("{}", MODEL2VEC_MISSING_FEATURE_MESSAGE)
    }
}
