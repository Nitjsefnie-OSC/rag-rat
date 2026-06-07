use std::{env, path::PathBuf};

use rag_rat_core::{Config, IndexDatabase, index::IndexProgress, search::lexical::SearchHit};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        anyhow::bail!("missing command");
    };
    let config_path = option_value(&args, "--config").unwrap_or_else(|| "rag-rat.toml".to_string());
    let config = Config::load(&config_path)?;

    match command {
        "index" => {
            let db = if has_flag(&args, "--watch") {
                anyhow::bail!(
                    "index --watch is not implemented yet; use --changed, --discover, or --full"
                );
            } else if has_flag(&args, "--full") {
                IndexDatabase::rebuild_with_progress(&config, render_index_progress)?
            } else if has_flag(&args, "--discover") {
                IndexDatabase::index_discover_with_progress(&config, render_index_progress)?
            } else {
                IndexDatabase::index_changed_with_progress(&config, render_index_progress)?
            };
            print_json(&db.status(&config.database)?)?;
        },
        "doctor" => {
            doctor(&config)?;
        },
        "migrate" => {
            migrate(&config, &args)?;
        },
        "query" => {
            let query = positional_after_options(&args).unwrap_or_default();
            if query.is_empty() {
                anyhow::bail!("query command needs a search string");
            }
            let db = IndexDatabase::open(&config.database)?;
            if has_flag(&args, "--explain") {
                print_query_explain(&db.search_explain(&query, 10, false)?);
            } else {
                print_json(&db.search(&query, 10, false)?)?;
            }
        },
        "mcp" => {
            rag_rat_mcp::server::run_stdio(config.database).await?;
        },
        "github" => {
            github(&config, &args)?;
        },
        "models" => {
            models(&config, &args)?;
        },
        "reconcile" => {
            reconcile(&config, &args)?;
        },
        "eval" => {
            eval(&config, &args)?;
        },
        "dump-config" => {
            let targets = config
                .targets
                .iter()
                .map(|target| {
                    serde_json::json!({
                        "name": target.name,
                        "language": target.language.as_str(),
                        "directories": target.directories,
                        "include": target.include,
                        "exclude": target.exclude,
                        "kind": target.kind.as_str(),
                    })
                })
                .collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "root": config.root,
                "database": config.database,
                "targets": targets,
            }))?;
        },
        _ => {
            usage();
            anyhow::bail!("unknown command `{command}`");
        },
    }

    Ok(())
}

fn eval(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let options = rag_rat_core::eval::EvalOptions {
        queries_path: option_value(args, "--queries")
            .map(Into::into)
            .unwrap_or_else(|| default_eval_path(config, "queries.toml")),
        expected_path: option_value(args, "--expected")
            .map(Into::into)
            .unwrap_or_else(|| default_eval_path(config, "expected_hits.toml")),
        update_baseline: has_flag(args, "--update-baseline"),
    };
    let report = rag_rat_core::eval::run(config, &options)?;
    if has_flag(args, "--json") || options.update_baseline {
        print_json(&report)?;
    } else {
        print_eval_summary(&report);
    }
    if !report.pass {
        anyhow::bail!(
            "eval failed: stale_current_source_violations={}, failed_queries={}",
            report.metrics.stale_current_source_violations,
            report.results.iter().filter(|result| !result.passed).count()
        );
    }
    Ok(())
}

fn default_eval_path(config: &Config, file_name: &str) -> PathBuf {
    config.root.join("evals").join(file_name)
}

fn print_eval_summary(report: &rag_rat_core::eval::EvalReport) {
    println!(
        "eval: pass={} queries={} skipped={} mrr@10={:.3} recall@10={:.3} path_hit_rate={:.3} symbol_hit_rate={:.3}",
        report.pass,
        report.queries,
        report.results.iter().filter(|result| result.skipped).count(),
        report.metrics.mrr_at_10,
        report.metrics.recall_at_10,
        report.metrics.path_hit_rate,
        report.metrics.symbol_hit_rate
    );
    println!(
        "eval: stale_current_source_violations={} stale_hit_rate={:.3} latency_p50_ms={:.1} latency_p95_ms={:.1}",
        report.metrics.stale_current_source_violations,
        report.metrics.stale_hit_rate,
        report.metrics.latency_p50_ms,
        report.metrics.latency_p95_ms
    );
    println!(
        "eval: graph_evidence_hit_rate={:.3} impact_hit_rate={:.3} git_evidence_hit_rate={:.3} papertrail_evidence_hit_rate={:.3}",
        report.metrics.graph_evidence_hit_rate,
        report.metrics.impact_hit_rate,
        report.metrics.git_evidence_hit_rate,
        report.metrics.papertrail_evidence_hit_rate
    );
    if let Some(precision) = report.metrics.papertrail_precision_sample {
        println!("eval: papertrail_precision_sample={precision:.3}");
    }
    println!(
        "eval: hash_vector_baseline model={} available={} current_artifacts={} mrr@10={:.3} recall@10={:.3} delta_mrr@10={:+.3} delta_recall@10={:+.3}",
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
            "eval: failed {} missing_paths={:?} missing_symbols={:?} missing_graph_targets={:?} missing_impact_categories={:?} missing_impact_paths={:?} missing_impact_symbols={:?} missing_git_subjects={:?} missing_papertrail_kinds={:?} stale_current_source_violations={}",
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

fn print_query_explain(hits: &[SearchHit]) {
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
        }
        println!("summary:");
        for line in hit.summary.lines() {
            println!("  {line}");
        }
    }
}

fn models(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let db = IndexDatabase::open(&config.database)?;
    match args.get(1).map(String::as_str) {
        Some("list") | None => print_json(&db.list_models()?),
        Some("install") => {
            let Some(model_id) = args.get(2) else {
                anyhow::bail!("models install needs a model id");
            };
            print_json(&db.install_model(model_id)?)
        },
        Some(other) => anyhow::bail!("unknown models subcommand `{other}`"),
    }
}

fn reconcile(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let db = IndexDatabase::open(&config.database)?;
    if has_flag(args, "--plan") {
        let plan = db.reconcile_plan()?;
        if has_flag(args, "--json") {
            print_json(&plan)
        } else {
            print_reconcile_plan(&plan);
            Ok(())
        }?;
        return Ok(());
    }
    let limit = option_value(args, "--limit").map(|value| value.parse()).transpose()?;
    let batch_size = option_value(args, "--batch-size").map(|value| value.parse()).transpose()?;
    let force = has_flag(args, "--force");
    print_json(&db.reconcile_with_progress(limit, batch_size, force, render_reconcile_progress)?)
}

fn print_reconcile_plan(plan: &rag_rat_core::index::ai::ReconcilePlan) {
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
    println!();
    println!("Summaries");
    println!(
        "  {}",
        if plan.summaries.enabled { "enabled" } else { plan.summaries.message.as_str() }
    );
}

fn render_reconcile_progress(progress: rag_rat_core::index::ai::ReconcileProgress) {
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
            let percent =
                processed_chunks.saturating_mul(100).checked_div(total_chunks).unwrap_or(100);
            eprintln!(
                "reconcile: {processed_chunks}/{total_chunks} ({percent:>3}%) written={embeddings_written} blocked={blocked_chunks}"
            );
        },
        rag_rat_core::index::ai::ReconcileProgress::Finished {
            processed_chunks,
            embeddings_written,
            blocked_chunks,
        } => {
            eprintln!(
                "reconcile: complete processed={processed_chunks} written={embeddings_written} blocked={blocked_chunks}"
            );
        },
    }
}

fn migrate(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let status = if has_flag(args, "--check") {
        IndexDatabase::migration_check(&config.database)?
    } else {
        IndexDatabase::migrate(&config.database)?
    };
    print_json(&status)?;
    if has_flag(args, "--check")
        && status.state != rag_rat_core::index::schema::SchemaState::Compatible
    {
        anyhow::bail!("{}", status.message);
    }
    Ok(())
}

fn doctor(config: &Config) -> anyhow::Result<()> {
    let schema = IndexDatabase::migration_check(&config.database)?;
    let (index, discovery, storage) =
        if schema.state == rag_rat_core::index::schema::SchemaState::Compatible {
            let db = IndexDatabase::open(&config.database)?;
            (
                Some(serde_json::to_value(db.status(&config.database)?)?),
                Some(serde_json::to_value(db.discovery_status(config)?)?),
                Some(serde_json::to_value(db.storage_status()?)?),
            )
        } else {
            (None, None, None)
        };
    print_json(&serde_json::json!({
        "config_root": config.root,
        "database": config.database,
        "schema": schema,
        "storage": storage,
        "discovery": discovery,
        "targets": config.targets.iter().map(|target| serde_json::json!({
            "name": target.name,
            "language": target.language.as_str(),
            "directories": target.directories,
            "kind": target.kind.as_str(),
        })).collect::<Vec<_>>(),
        "index": index,
        "mcp": {
            "transport": "stdio",
            "tools": rag_rat_mcp::tools::TOOL_NAMES,
            "source_read_only": true,
            "index_writes": "sqlite_auto_heal"
        }
    }))
}

fn github(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("github command needs a subcommand");
    };
    match subcommand {
        "sync" => {
            let db = IndexDatabase::open(&config.database)?;
            let offline = has_flag(args, "--offline");
            let report = if let Some(issue) = option_value(args, "--issue") {
                db.github_sync_issue(&issue, offline)?
            } else if has_flag(args, "--from-refs") {
                db.github_sync_from_refs(offline)?
            } else {
                anyhow::bail!("github sync needs --from-refs or --issue <owner/repo#number>");
            };
            print_json(&report)
        },
        other => anyhow::bail!("unknown github subcommand `{other}`"),
    }
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn render_index_progress(progress: IndexProgress) {
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
        IndexProgress::IndexingFile { current, total, path, language, kind } => {
            let percent = current.saturating_mul(100).checked_div(total).unwrap_or(100);
            eprintln!(
                "index: {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::RebuildingFts => {
            eprintln!("index: rebuilding SQLite FTS");
        },
        IndexProgress::Finished { files } => {
            eprintln!("index: complete ({files} files)");
        },
    }
}

fn usage() {
    eprintln!(
        "usage: rag-rat <index|doctor|migrate|query|mcp|github|models|reconcile|eval|dump-config> --config <path> [query]\n\
         examples:\n\
         rag-rat index --config rag-rat.toml\n\
         rag-rat index --changed --config rag-rat.toml\n\
         rag-rat index --discover --config rag-rat.toml\n\
         rag-rat index --full --config rag-rat.toml\n\
         rag-rat index --watch --config rag-rat.toml\n\
         rag-rat migrate --check --config rag-rat.toml\n\
         rag-rat github sync --from-refs --config rag-rat.toml\n\
         rag-rat models list --config rag-rat.toml\n\
         rag-rat models install embedding-hash --config rag-rat.toml\n\
         rag-rat models install fastembed-all-minilm-l6-v2 --config rag-rat.toml\n\
         rag-rat reconcile --plan --config rag-rat.toml\n\
         rag-rat reconcile --limit 100 --batch-size 32 --config rag-rat.toml\n\
         rag-rat reconcile --force --limit 100 --batch-size 32 --config rag-rat.toml\n\
         rag-rat eval --config rag-rat.toml\n\
         rag-rat eval --json --config rag-rat.toml\n\
         rag-rat eval --update-baseline --config rag-rat.toml\n\
         rag-rat query --config rag-rat.toml \"semantic recall\"\n\
         rag-rat query --explain --config rag-rat.toml \"runtime shutdown\""
    );
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|window| window[0] == name).map(|window| window[1].clone())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

fn positional_after_options(args: &[String]) -> Option<String> {
    let mut values = Vec::new();
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--config"
            || arg == "--issue"
            || arg == "--limit"
            || arg == "--batch-size"
            || arg == "--queries"
            || arg == "--expected"
        {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--") {
            continue;
        }
        values.push(arg.clone());
    }
    Some(values.join(" ")).filter(|value| !value.is_empty())
}
