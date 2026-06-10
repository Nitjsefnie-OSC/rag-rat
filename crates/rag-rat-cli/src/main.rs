mod commands;
mod hooks_support;
mod render;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use std::{env, fs};

pub(crate) use commands::*;
pub(crate) use hooks_support::*;
use rag_rat_core::config::EmbeddingRuntimeConfig;
use rag_rat_core::index::IndexProgress;
use rag_rat_core::index::github::GitHubSyncAction;
use rag_rat_core::search::lexical::SearchHit;
use rag_rat_core::{Config, IndexDatabase};
pub(crate) use render::*;

mod claude_hook;
mod claude_settings;
mod init;

fn main() -> anyhow::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        return Ok(());
    };
    if command == "init" {
        return init::run(&args);
    }
    if command == "claude-hook" {
        return claude_hook::run();
    }
    let config_path = option_value(&args, "--config").unwrap_or_else(|| "rag-rat.toml".to_string());
    let config = Config::load(&config_path)?;
    apply_embedding_runtime_env(&config.local_ai.embedding.runtime);

    match command {
        "index" => {
            if has_flag(&args, "--watch") {
                run_watch(config)?;
            } else {
                // Serialize with the background watcher / other writers (busy_timeout backstops
                // any heal on the query path).
                let _lock = rag_rat_core::locks::FileLock::acquire_blocking(
                    &rag_rat_core::locks::write_lock_path(&config.database),
                )?;
                let db = if has_flag(&args, "--full") {
                    IndexDatabase::rebuild_with_progress(&config, render_index_progress)?
                } else if has_flag(&args, "--discover") {
                    IndexDatabase::index_discover_with_progress(&config, render_index_progress)?
                } else {
                    IndexDatabase::index_changed_with_progress(&config, render_index_progress)?
                };
                // Re-anchor repo memories against the freshly indexed symbols/chunks so a moved or
                // renamed binding relocates (or is flagged) instead of silently pointing at a stale
                // row. Memory rows themselves are never deleted by indexing.
                if let Err(err) = db.memory_validate() {
                    eprintln!("warning: repo-memory re-validation failed: {err}");
                }
                // After validate has refreshed anchor_status values, count non-current anchors
                // with a read-only query (doctor_report reads persisted values; no re-validation).
                let doctor_count = db.memory_doctor().map(|entries| entries.len()).unwrap_or(0);
                if doctor_count > 0 {
                    eprintln!(
                        "⚠ {doctor_count} repo memories need re-anchoring — run 'rag-rat memory \
                         doctor'"
                    );
                }
                print_json(&db.status(&config.database)?)?;
            }
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
            let db = IndexDatabase::open_config(&config)?;
            if has_flag(&args, "--explain") {
                print_query_explain(&db.search_explain(&query, 10, false)?);
            } else {
                print_json(&db.search(&query, 10, false)?)?;
            }
        },
        "brief" => {
            let db = IndexDatabase::open_config(&config)?;
            let mode = rag_rat_core::query::repo_brief::RepoBriefMode::parse(
                option_value(&args, "--mode").as_deref(),
            )?;
            let limit = option_value(&args, "--limit")
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(10);
            print_json(&db.repo_brief(rag_rat_core::query::repo_brief::RepoBriefOptions {
                mode,
                limit,
                include_generated: has_flag(&args, "--include-generated"),
                include_memories: !has_flag(&args, "--no-memories"),
            })?)?;
        },
        "clusters" => {
            let db = IndexDatabase::open_config(&config)?;
            let limit = option_value(&args, "--limit")
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(10);
            let min_cluster_size = option_value(&args, "--min-cluster-size")
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(2);
            print_json(&db.repo_clusters(rag_rat_core::query::clusters::RepoClustersOptions {
                limit,
                include_generated: has_flag(&args, "--include-generated"),
                include_memories: !has_flag(&args, "--no-memories"),
                min_cluster_size,
            })?)?;
        },
        "mcp" => {
            tokio::runtime::Runtime::new()?.block_on(rag_rat_mcp::server::run_stdio(config))?;
        },
        "memory" => {
            memory(&config, &args)?;
        },
        "github" => {
            github(&config, &args)?;
        },
        "hooks" => {
            hooks(&config, &args)?;
        },
        "maintenance" => {
            maintenance(&config, &args)?;
        },
        "models" => {
            models(&config, &args)?;
        },
        "reconcile" => {
            reconcile(&config, &args)?;
        },
        "gc" => {
            let db = IndexDatabase::open_config(&config)?;
            print_json(&db.gc()?)?;
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
                "local_ai": {
                    "embedding": {
                        "runtime": {
                            "batch_size": config.local_ai.embedding.runtime.batch_size,
                            "ort_threads": config.local_ai.embedding.runtime.ort_threads,
                            "omp_threads": config.local_ai.embedding.runtime.omp_threads,
                            "max_embedding_chars": config.local_ai.embedding.runtime.max_embedding_chars,
                        }
                    }
                },
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

pub(crate) const MANAGED_HOOKS: &[&str] =
    &["post-checkout", "post-merge", "post-rewrite", "post-commit"];
const HOOK_MARKER: &str = "# Generated by rag-rat.";
const DEFAULT_MAINTENANCE_SECONDS: u64 = 30;

#[derive(Debug)]
pub(crate) struct GitPaths {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
    pub(crate) hooks_dir: PathBuf,
}

fn usage() {
    eprintln!(
        "usage: rag-rat <init|index|doctor|migrate|query|brief|clusters|mcp|memory|github|hooks|claude-hook|maintenance|models|reconcile|gc|eval|dump-config> [--config <path>] [query]\n\
         default config: rag-rat.toml\n\
         examples:\n\
         rag-rat init\n\
         rag-rat init --dry-run\n\
         rag-rat index\n\
         rag-rat index --changed\n\
         rag-rat index --discover\n\
         rag-rat index --full\n\
         rag-rat index --watch\n\
         rag-rat migrate --check\n\
         rag-rat memory doctor\n\
         rag-rat memory doctor --json\n\
         rag-rat memory rebind <memory_id> --symbol <name>\n\
         rag-rat memory rebind <memory_id> --path <path>\n\
         rag-rat memory rebind <memory_id> --chunk <chunk_id>\n\
         rag-rat github sync --from-refs\n\
         rag-rat hooks install\n\
         rag-rat hooks status\n\
         rag-rat hooks install --claude          # Claude Code grep-augment + SessionStart orientation digest\n\
         rag-rat hooks install --claude --global # ~/.claude/settings.json instead of ./.claude\n\
         rag-rat hooks status --claude\n\
         rag-rat hooks uninstall --claude\n\
         rag-rat maintenance --trigger post-checkout --max-seconds 30\n\
         rag-rat models list\n\
         rag-rat models install embedding-hash\n\
         rag-rat models install fastembed-all-minilm-l6-v2\n\
         rag-rat reconcile --plan\n\
         rag-rat reconcile --limit 100 --batch-size 32\n\
         rag-rat reconcile --changed-first --max-seconds 60 --batch-size 64\n\
         rag-rat reconcile --until-clean --batch-size 64\n\
         rag-rat gc\n\
         rag-rat reconcile --force --limit 100 --batch-size 32\n\
         rag-rat eval\n\
         rag-rat eval --json\n\
         rag-rat eval --update-baseline\n\
         rag-rat query \"semantic recall\"\n\
         rag-rat query --explain \"runtime shutdown\"\n\
         rag-rat brief --mode spine\n\
         rag-rat brief --mode churn\n\
         rag-rat brief --mode god_modules\n\
         rag-rat clusters --limit 10"
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
            || arg == "--trigger"
            || arg == "--max-seconds"
            || arg == "--max-embedding-chars"
            || arg == "--old-head"
            || arg == "--new-head"
            || arg == "--branch-checkout"
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

#[cfg(test)]
mod tests {
    use super::progress_percent;

    #[test]
    fn progress_percent_is_capped() {
        assert_eq!(progress_percent(0, 0), 100);
        assert_eq!(progress_percent(50, 100), 50);
        assert_eq!(progress_percent(17_024, 11_998), 100);
    }
}
