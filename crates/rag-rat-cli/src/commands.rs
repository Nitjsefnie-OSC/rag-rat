use super::*;

pub(crate) fn eval(config: &Config, args: &[String]) -> anyhow::Result<()> {
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
pub(crate) fn default_eval_path(config: &Config, file_name: &str) -> PathBuf {
    config.root.join("evals").join(file_name)
}
pub(crate) fn models(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let db = IndexDatabase::open_config(config)?;
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
pub(crate) fn reconcile(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let db = IndexDatabase::open_config(config)?;
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
    let batch_size = option_value(args, "--batch-size")
        .map(|value| value.parse())
        .transpose()?
        .or(Some(config.local_ai.embedding.runtime.batch_size));
    let force = has_flag(args, "--force");
    let max_seconds = option_value(args, "--max-seconds").map(|value| value.parse()).transpose()?;
    let max_embedding_chars = option_value(args, "--max-embedding-chars")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(config.local_ai.embedding.runtime.max_embedding_chars);
    let options = rag_rat_core::index::ai::ReconcileOptions {
        limit,
        batch_size,
        force,
        until_clean: has_flag(args, "--until-clean"),
        changed_first: has_flag(args, "--changed-first"),
        max_seconds,
        max_embedding_chars,
        intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
    };
    let report = db.reconcile_with_options_progress(options, render_reconcile_progress)?;
    // After reconciling, surface non-current memory anchors so they don't rot silently.
    // Read-only count from persisted anchor_status; does not call memory_validate.
    let non_current = db.memory_anchor_health().map(|h| h.stale + h.gone).unwrap_or(0);
    if non_current > 0 {
        eprintln!("⚠ {non_current} repo memories need re-anchoring — run 'rag-rat memory doctor'");
    }
    print_json(&report)
}
pub(crate) fn run_watch(config: Config) -> anyhow::Result<()> {
    let Some(_watcher) = rag_rat_core::watch::Watcher::spawn(config.clone()) else {
        anyhow::bail!("watcher is disabled ([watch] enabled = false or RAG_RAT_NO_WATCH set)");
    };
    eprintln!("rag-rat: watching {} for changes (Ctrl-C to stop)", config.root.display());
    // The watcher runs on its own thread; park here. Ctrl-C ends the process and the OS releases
    // the locks; the next session's startup catch-up covers any edit in flight.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
pub(crate) fn apply_embedding_runtime_env(runtime: &EmbeddingRuntimeConfig) {
    // `ort_threads` is applied via fastembed's session `with_intra_threads` (see
    // FastEmbedEmbedder::new), not an env var — ONNX Runtime does not read `ORT_NUM_THREADS`.
    // `omp_threads` IS effective: Microsoft's prebuilt ORT is OpenMP-based and honors
    // `OMP_NUM_THREADS`, so it is the real thread lever for the default binaries.
    set_env_if_absent("OMP_NUM_THREADS", runtime.omp_threads);
}
pub(crate) fn set_env_if_absent(key: &str, value: Option<u32>) {
    let Some(value) = value else {
        return;
    };
    if env::var_os(key).is_some() {
        return;
    }
    // This is called at process startup before rag-rat creates its Tokio runtime or initializes
    // FastEmbed/ONNX. CLI-provided environment variables intentionally take precedence.
    unsafe {
        env::set_var(key, value.to_string());
    }
}
pub(crate) fn migrate(config: &Config, args: &[String]) -> anyhow::Result<()> {
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
pub(crate) fn doctor(config: &Config) -> anyhow::Result<()> {
    let schema = IndexDatabase::migration_check(&config.database)?;
    let (index, discovery, storage) =
        if schema.state == rag_rat_core::index::schema::SchemaState::Compatible {
            let db = IndexDatabase::open_config(config)?;
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
pub(crate) fn memory(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("memory command needs a subcommand (doctor, rebind)");
    };
    match subcommand {
        "doctor" => {
            let db = IndexDatabase::open_config(config)?;
            let entries = db.memory_doctor()?;
            if has_flag(args, "--json") {
                print_json(&entries)?;
                let any_gone = entries.iter().any(|e| e.anchor_status == "gone");
                if any_gone {
                    anyhow::bail!("one or more memories have gone anchors");
                }
                return Ok(());
            }
            if entries.is_empty() {
                eprintln!("All active memory anchors are current.");
                return Ok(());
            }
            let mut any_gone = false;
            for entry in &entries {
                eprintln!("[{}] {} ({})", entry.anchor_status, entry.title, entry.memory_id);
                eprintln!("  binding: {} {}", entry.binding_kind, entry.binding_id);
                if entry.candidates.is_empty() {
                    if entry.anchor_status == "gone" {
                        eprintln!(
                            "  -> code appears deleted; rag-rat memory mark-obsolete {}",
                            entry.memory_id
                        );
                    }
                } else {
                    for candidate in &entry.candidates {
                        eprintln!(
                            "  rag-rat memory rebind {} --symbol {}",
                            entry.memory_id, candidate
                        );
                    }
                }
                if entry.anchor_status == "gone" {
                    any_gone = true;
                }
            }
            if any_gone {
                anyhow::bail!("one or more memories have gone anchors");
            }
            Ok(())
        },
        "rebind" => {
            let Some(memory_id) = args.get(2).cloned() else {
                anyhow::bail!("memory rebind needs a <memory_id>");
            };
            let db = IndexDatabase::open_config(config)?;
            let bind = if let Some(symbol_name) = option_value(args, "--symbol") {
                let selector = rag_rat_core::query::symbol::SymbolSelector {
                    logical_symbol_id: None,
                    symbol_id: None,
                    symbol_path: None,
                    symbol: Some(symbol_name.clone()),
                    language: None,
                    allow_ambiguous: false,
                    limit: 10,
                };
                match db.select_symbol(&selector)? {
                    Ok(Some(hit)) => rag_rat_core::query::memory::RepoMemoryBindTarget {
                        symbol_id: Some(hit.symbol_id),
                        logical_symbol_id: hit.logical_symbol_id,
                        chunk_id: None,
                        edge_id: None,
                        path: None,
                        start_line: None,
                        end_line: None,
                        commit_hash: None,
                        github_owner: None,
                        github_repo: None,
                        github_number: None,
                        start_logical_symbol_id: None,
                        end_logical_symbol_id: None,
                        edge_sequence_hash: None,
                        path_summary: None,
                        dir: None,
                    },
                    Ok(None) => anyhow::bail!("symbol `{symbol_name}` not found"),
                    Err(disambiguation) => anyhow::bail!(
                        "symbol `{symbol_name}` is ambiguous — candidates: {}",
                        disambiguation
                            .candidates
                            .iter()
                            .map(|c| c.qualified_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            } else if let Some(path) = option_value(args, "--path") {
                rag_rat_core::query::memory::RepoMemoryBindTarget {
                    symbol_id: None,
                    logical_symbol_id: None,
                    chunk_id: None,
                    edge_id: None,
                    path: Some(path),
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                    dir: None,
                }
            } else if let Some(chunk_id_str) = option_value(args, "--chunk") {
                let chunk_id: i64 = chunk_id_str
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--chunk value must be an integer"))?;
                rag_rat_core::query::memory::RepoMemoryBindTarget {
                    symbol_id: None,
                    logical_symbol_id: None,
                    chunk_id: Some(chunk_id),
                    edge_id: None,
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                    dir: None,
                }
            } else {
                anyhow::bail!(
                    "memory rebind needs one of --symbol <name>, --path <path>, or --chunk <id>"
                );
            };
            print_json(&db.memory_rebind(&memory_id, bind)?)
        },
        other => anyhow::bail!("unknown memory subcommand `{other}`; use doctor or rebind"),
    }
}
pub(crate) fn github(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("github command needs a subcommand");
    };
    match subcommand {
        "sync" => {
            let db = IndexDatabase::open_config(config)?;
            let offline = has_flag(args, "--offline");
            let report = if let Some(issue) = option_value(args, "--issue") {
                db.github_sync_issue(&issue, offline)?
            } else if has_flag(args, "--from-refs") {
                db.github_sync_from_refs_with_progress(offline, render_github_sync_progress)?
            } else {
                anyhow::bail!("github sync needs --from-refs or --issue <owner/repo#number>");
            };
            print_json(&report)
        },
        other => anyhow::bail!("unknown github subcommand `{other}`"),
    }
}
pub(crate) fn hooks(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let Some(subcommand) = args.get(1).map(String::as_str) else {
        anyhow::bail!("hooks command needs install, uninstall, or status");
    };
    if args.iter().any(|a| a == "--claude") {
        return claude_hooks(config, subcommand, args.iter().any(|a| a == "--global"));
    }
    let git = git_paths(&config.root)?;
    match subcommand {
        "install" => {
            fs::create_dir_all(&git.hooks_dir)?;
            let mut installed = Vec::new();
            for hook in MANAGED_HOOKS {
                install_hook(&git.hooks_dir, hook)?;
                installed.push(*hook);
            }
            print_json(&serde_json::json!({
                "status": "installed",
                "repo_root": git.worktree_root,
                "git_dir": git.git_dir,
                "git_common_dir": git.git_common_dir,
                "hooks_dir": git.hooks_dir,
                "hooks": installed,
            }))
        },
        "uninstall" => {
            let mut removed = Vec::new();
            let mut kept = Vec::new();
            for hook in MANAGED_HOOKS {
                let path = git.hooks_dir.join(hook);
                if !path.exists() {
                    continue;
                }
                if is_rag_rat_hook(&path)? {
                    fs::remove_file(&path)?;
                    removed.push(*hook);
                } else {
                    kept.push(*hook);
                }
            }
            print_json(&serde_json::json!({
                "status": "uninstalled",
                "hooks_dir": git.hooks_dir,
                "removed": removed,
                "kept_unmanaged": kept,
            }))
        },
        "status" => {
            let hooks = MANAGED_HOOKS
                .iter()
                .map(|hook| {
                    let path = git.hooks_dir.join(hook);
                    let managed = is_rag_rat_hook(&path).unwrap_or(false);
                    serde_json::json!({
                        "name": hook,
                        "path": path,
                        "exists": path.exists(),
                        "managed": managed,
                    })
                })
                .collect::<Vec<_>>();
            print_json(&serde_json::json!({
                "repo_root": git.worktree_root,
                "git_dir": git.git_dir,
                "git_common_dir": git.git_common_dir,
                "hooks_dir": git.hooks_dir,
                "hooks": hooks,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}
pub(crate) fn claude_hooks(config: &Config, subcommand: &str, global: bool) -> anyhow::Result<()> {
    let path = claude_settings::settings_path(&config.root, global)?;
    let mut settings = claude_settings::read_settings(&path)?;
    match subcommand {
        "install" => {
            let changed = claude_settings::merge_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_json(&serde_json::json!({
                "status": if changed { "installed" } else { "already_installed" },
                "settings_path": path,
                "matchers": ["Grep", "Bash"],
            }))
        },
        "uninstall" => {
            let changed = claude_settings::remove_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_json(&serde_json::json!({
                "status": if changed { "uninstalled" } else { "not_installed" },
                "settings_path": path,
            }))
        },
        "status" => {
            let (grep, bash) = claude_settings::hook_status(&settings);
            print_json(&serde_json::json!({
                "settings_path": path,
                "grep_matcher_installed": grep,
                "bash_matcher_installed": bash,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}
pub(crate) fn maintenance(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let trigger = option_value(args, "--trigger").unwrap_or_else(|| "manual".to_string());
    let max_seconds = option_value(args, "--max-seconds")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(DEFAULT_MAINTENANCE_SECONDS);
    let branch_checkout = option_value(args, "--branch-checkout");
    let old_head = option_value(args, "--old-head");
    let new_head = option_value(args, "--new-head");
    let started = Instant::now();

    if trigger == "post-checkout" && branch_checkout.as_deref() == Some("0") {
        print_json(&serde_json::json!({
            "trigger": trigger,
            "status": "skipped",
            "reason": "file checkout",
            "old_head": old_head,
            "new_head": new_head,
            "branch_checkout": branch_checkout,
        }))?;
        return Ok(());
    }

    // Serialize with the background watcher (and other writers). The hook backgrounds this command,
    // so blocking here never holds up the git operation; busy_timeout backstops the query-path
    // heal.
    let _lock = rag_rat_core::locks::FileLock::acquire_blocking(
        &rag_rat_core::locks::write_lock_path(&config.database),
    )?;

    let db = IndexDatabase::index_discover_with_progress(config, render_index_progress)?;
    let elapsed = started.elapsed().as_secs();
    let remaining_seconds = max_seconds.saturating_sub(elapsed);
    let reconcile_report = if remaining_seconds > 0 {
        let options = rag_rat_core::index::ai::ReconcileOptions {
            limit: None,
            batch_size: Some(config.local_ai.embedding.runtime.batch_size),
            force: false,
            until_clean: false,
            changed_first: true,
            max_seconds: Some(remaining_seconds),
            max_embedding_chars: config.local_ai.embedding.runtime.max_embedding_chars,
            intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
        };
        Some(db.reconcile_with_options_progress(options, render_reconcile_progress)?)
    } else {
        None
    };
    // Prune index rows for git contexts that are no longer live (worktree-safe; keeps every
    // live worktree's HEAD). Cheap and bounded, so it runs every maintenance pass.
    let gc_report = db.gc().ok();
    // Re-anchor repo memories: post-checkout/merge/rewrite/commit are exactly when files move,
    // rename, or change, so relocate symbol/chunk bindings (or flag them) here rather than
    // leaving stale anchors until a manual memory_validate.
    let memory_validation = db.memory_validate().ok();
    let plan = db.reconcile_plan()?;
    print_json(&serde_json::json!({
        "trigger": trigger,
        "status": "complete",
        "old_head": old_head,
        "new_head": new_head,
        "branch_checkout": branch_checkout,
        "max_seconds": max_seconds,
        "elapsed_seconds": started.elapsed().as_secs_f64(),
        "reconcile": reconcile_report,
        "gc": gc_report,
        "memory_validation": memory_validation,
        "remaining_backlog": {
            "model": plan.embeddings.model_id,
            "current": plan.embeddings.current,
            "missing": plan.embeddings.missing,
            "stale": plan.embeddings.stale,
            "failed_retryable": plan.embeddings.failed_retryable,
            "failed_waiting": plan.embeddings.failed_waiting,
            "blocked": plan.embeddings.blocked,
            "skipped": plan.embeddings.skipped_total,
            "missing_by_priority": plan.embeddings.missing_by_priority,
            "skipped_by_policy": plan.embeddings.skipped_by_policy,
        }
    }))
}
