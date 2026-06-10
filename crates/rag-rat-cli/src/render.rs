use super::*;

pub(crate) fn print_eval_summary(report: &rag_rat_core::eval::EvalReport) {
    println!(
        "eval: pass={} queries={} skipped={} mrr@10={:.3} recall@10={:.3} path_hit_rate={:.3} \
         symbol_hit_rate={:.3}",
        report.pass,
        report.queries,
        report.results.iter().filter(|result| result.skipped).count(),
        report.metrics.mrr_at_10,
        report.metrics.recall_at_10,
        report.metrics.path_hit_rate,
        report.metrics.symbol_hit_rate
    );
    println!(
        "eval: stale_current_source_violations={} stale_hit_rate={:.3} latency_p50_ms={:.1} \
         latency_p95_ms={:.1}",
        report.metrics.stale_current_source_violations,
        report.metrics.stale_hit_rate,
        report.metrics.latency_p50_ms,
        report.metrics.latency_p95_ms
    );
    println!(
        "eval: graph_evidence_hit_rate={:.3} impact_hit_rate={:.3} git_evidence_hit_rate={:.3} \
         papertrail_evidence_hit_rate={:.3}",
        report.metrics.graph_evidence_hit_rate,
        report.metrics.impact_hit_rate,
        report.metrics.git_evidence_hit_rate,
        report.metrics.papertrail_evidence_hit_rate
    );
    if let Some(precision) = report.metrics.papertrail_precision_sample {
        println!("eval: papertrail_precision_sample={precision:.3}");
    }
    println!(
        "eval: hash_vector_baseline model={} available={} current_artifacts={} mrr@10={:.3} \
         recall@10={:.3} delta_mrr@10={:+.3} delta_recall@10={:+.3}",
        report.hash_vector_baseline.model_id,
        report.hash_vector_baseline.available,
        report.hash_vector_baseline.current_artifacts,
        report.hash_vector_baseline.metrics.mrr_at_10,
        report.hash_vector_baseline.metrics.recall_at_10,
        report.hash_vector_baseline.delta_mrr_at_10,
        report.hash_vector_baseline.delta_recall_at_10
    );
    for result in report.results.iter().filter(|result| !result.passed) {
        println!(
            "eval: failed {} missing_paths={:?} missing_symbols={:?} missing_graph_targets={:?} \
             missing_impact_categories={:?} missing_impact_paths={:?} missing_impact_symbols={:?} \
             missing_git_subjects={:?} missing_papertrail_kinds={:?} \
             stale_current_source_violations={}",
            result.id,
            result.missing_paths,
            result.missing_symbols,
            result.missing_graph_targets,
            result.missing_impact_categories,
            result.missing_impact_paths,
            result.missing_impact_symbols,
            result.missing_git_subjects,
            result.missing_papertrail_kinds,
            result.stale_current_source_violations
        );
    }
    for result in report.results.iter().filter(|result| result.skipped) {
        println!(
            "eval: skipped {} reason={}",
            result.id,
            result.skip_reason.as_deref().unwrap_or("not applicable")
        );
    }
}
pub(crate) fn print_query_explain(hits: &[SearchHit]) {
    for (index, hit) in hits.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!(
            "{}:{}-{} {}",
            hit.path,
            hit.start_line,
            hit.end_line,
            hit.symbol_path.as_deref().unwrap_or("<chunk>")
        );
        println!("score: {:.3}", hit.score);
        if let Some(components) = &hit.score_components {
            println!("  bm25: {:.3}", components.bm25);
            println!("  vector: {:.3}", components.vector);
            println!("  symbol: {:.3}", components.symbol);
            println!("  graph: {:.3}", components.graph);
            println!("  git: {:.3}", components.git);
            println!("  github: {:.3}", components.github);
            if let Some(note) = &components.vector_note {
                println!("  vector_note: {note}");
            }
        }
        println!("summary:");
        for line in hit.summary.lines() {
            println!("  {line}");
        }
    }
}
pub(crate) fn print_reconcile_plan(plan: &rag_rat_core::index::ai::ReconcilePlan) {
    let embeddings = &plan.embeddings;
    println!("Embeddings");
    println!("  model: {}", embeddings.model_id);
    println!("  model_version: {}", embeddings.model_version);
    println!("  dim: {}", embeddings.dim);
    println!("  available: {}", embeddings.available);
    if let Some(message) = &embeddings.message {
        println!("  message: {message}");
    }
    println!("  current: {}", embeddings.current);
    println!("  missing: {}", embeddings.missing);
    println!("  stale: {}", embeddings.stale);
    println!("  model_changed: {}", embeddings.model_changed);
    println!("  dim_changed: {}", embeddings.dim_changed);
    println!("  failed_retryable: {}", embeddings.failed_retryable);
    println!("  failed_waiting: {}", embeddings.failed_waiting);
    println!("  blocked: {}", embeddings.blocked);
    println!("  skipped: {}", embeddings.skipped_total);
    if !embeddings.skipped_by_policy.is_empty() {
        println!("  skipped_by_policy:");
        for (policy, count) in &embeddings.skipped_by_policy {
            println!("    {policy}: {count}");
        }
    }
    if !embeddings.missing_by_priority.is_empty() {
        println!("  missing_by_priority:");
        for (priority, count) in &embeddings.missing_by_priority {
            println!("    {priority}: {count}");
        }
    }
    println!();
    println!("Summaries");
    println!(
        "  {}",
        if plan.summaries.enabled { "enabled" } else { plan.summaries.message.as_str() }
    );
}
pub(crate) fn render_reconcile_progress(progress: rag_rat_core::index::ai::ReconcileProgress) {
    match progress {
        rag_rat_core::index::ai::ReconcileProgress::Started {
            model_id,
            total_chunks,
            batch_size,
        } => {
            eprintln!("reconcile: model={model_id} chunks={total_chunks} batch_size={batch_size}");
        },
        rag_rat_core::index::ai::ReconcileProgress::Batch {
            processed_chunks,
            total_chunks,
            embeddings_written,
            blocked_chunks,
        } => {
            let percent = progress_percent(processed_chunks, total_chunks);
            eprintln!(
                "reconcile: {processed_chunks}/{total_chunks} ({percent:>3}%) \
                 written={embeddings_written} blocked={blocked_chunks}"
            );
        },
        rag_rat_core::index::ai::ReconcileProgress::Finished {
            processed_chunks,
            embeddings_written,
            blocked_chunks,
        } => {
            eprintln!(
                "reconcile: complete processed={processed_chunks} written={embeddings_written} \
                 blocked={blocked_chunks}"
            );
        },
    }
}
pub(crate) fn progress_percent(current: u64, total: u64) -> u64 {
    current.saturating_mul(100).checked_div(total).unwrap_or(100).min(100)
}
pub(crate) fn render_github_sync_progress(
    progress: rag_rat_core::index::github::GitHubSyncProgress,
) {
    match progress.action {
        GitHubSyncAction::Syncing => eprintln!(
            "github sync: {}/{} fetching {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        GitHubSyncAction::Skipped => eprintln!(
            "github sync: {}/{} skip cached {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        GitHubSyncAction::Synced => eprintln!(
            "github sync: {}/{} synced {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        GitHubSyncAction::Failed => eprintln!(
            "github sync: {}/{} failed {}/{}#{}: {}",
            progress.current,
            progress.total,
            progress.owner,
            progress.repo,
            progress.number,
            progress.message.unwrap_or_else(|| "unknown error".to_string())
        ),
        GitHubSyncAction::RebuildingFts => {
            eprintln!("github sync: rebuilding GitHub FTS cache")
        },
    }
}
pub(crate) fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
pub(crate) fn render_index_progress(progress: IndexProgress) {
    match progress {
        IndexProgress::Started { database, mode } => {
            eprintln!("index: {} using {}", mode.label(), database.display());
        },
        IndexProgress::Discovering => {
            eprintln!("index: discovering files");
        },
        IndexProgress::Discovered { files } => {
            eprintln!("index: discovered {files} files");
        },
        IndexProgress::PreparingFile { current, total, path, language, kind } => {
            let percent = progress_percent(
                u64::try_from(current).unwrap_or(u64::MAX),
                u64::try_from(total).unwrap_or(u64::MAX),
            );
            eprintln!(
                "index: preparing {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::IndexingFile { current, total, path, language, kind } => {
            let percent = progress_percent(
                u64::try_from(current).unwrap_or(u64::MAX),
                u64::try_from(total).unwrap_or(u64::MAX),
            );
            eprintln!(
                "index: {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::IndexingGitHistory => {
            eprintln!("index: indexing git history");
        },
        IndexProgress::RebuildingLogicalSymbols => {
            eprintln!("index: rebuilding logical symbols");
        },
        IndexProgress::ResolvingGraph => {
            eprintln!("index: resolving graph edges");
        },
        IndexProgress::SyncingFts => {
            eprintln!("index: syncing SQLite FTS");
        },
        IndexProgress::RebuildingFts => {
            eprintln!("index: rebuilding SQLite FTS");
        },
        IndexProgress::Finished { files } => {
            eprintln!("index: complete ({files} files)");
        },
    }
}
