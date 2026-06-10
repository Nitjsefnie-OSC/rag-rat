use super::*;

pub(crate) fn install_fastembed_model(conn: &Connection, model_id: &str) -> anyhow::Result<()> {
    #[cfg(feature = "fastembed")]
    {
        let embedder = FastEmbedEmbedder::new(None)
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

pub(crate) fn fastembed_operational_status(
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
        eligible_embeddings: plan
            .current
            .saturating_add(plan.missing)
            .saturating_add(plan.stale)
            .saturating_add(plan.model_changed)
            .saturating_add(plan.dim_changed)
            .saturating_add(plan.failed_retryable)
            .saturating_add(plan.failed_waiting)
            .saturating_add(plan.blocked),
        skipped_embeddings: plan.skipped_total,
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

pub(crate) fn fastembed_next_command(plan: &EmbeddingReconcilePlan) -> Option<String> {
    if !fastembed_build_feature_enabled() {
        return Some("cargo install rag-rat".to_string());
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

pub(crate) fn fastembed_build_feature_enabled() -> bool {
    cfg!(feature = "fastembed")
}

pub(crate) fn capability_status(
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

pub(crate) fn model(conn: &Connection, model_id: &str) -> anyhow::Result<ModelInfo> {
    Ok(conn.query_row(
        "
        SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, \
         installed_at_ms, last_error
        FROM ai_models WHERE model_id = ?1
        ",
        [model_id],
        model_row,
    )?)
}

pub(crate) fn model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelInfo> {
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
