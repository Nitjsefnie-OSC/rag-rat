use std::env;

use rag_rat_core::{Config, IndexDatabase, index::IndexProgress};

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
            let db = if has_flag(&args, "--full") {
                IndexDatabase::rebuild_with_progress(&config, render_index_progress)?
            } else {
                IndexDatabase::index_changed_with_progress(&config, render_index_progress)?
            };
            print_json(&db.status(&config.database)?)?;
        },
        "doctor" => {
            doctor(&config)?;
        },
        "query" => {
            let query = positional_after_options(&args).unwrap_or_default();
            if query.is_empty() {
                anyhow::bail!("query command needs a search string");
            }
            let db = IndexDatabase::open(&config.database)?;
            print_json(&db.search(&query, 10, false)?)?;
        },
        "mcp" => {
            rag_rat_mcp::server::run_stdio(config.database).await?;
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

fn doctor(config: &Config) -> anyhow::Result<()> {
    let db = IndexDatabase::open(&config.database)?;
    let status = db.status(&config.database)?;
    print_json(&serde_json::json!({
        "config_root": config.root,
        "database": config.database,
        "targets": config.targets.iter().map(|target| serde_json::json!({
            "name": target.name,
            "language": target.language.as_str(),
            "directories": target.directories,
            "kind": target.kind.as_str(),
        })).collect::<Vec<_>>(),
        "index": status,
        "mcp": {
            "transport": "stdio",
            "tools": rag_rat_mcp::tools::TOOL_NAMES,
            "source_read_only": true,
            "index_writes": "sqlite_auto_heal"
        }
    }))
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn render_index_progress(progress: IndexProgress) {
    match progress {
        IndexProgress::Started { database, full_rebuild } => {
            let mode = if full_rebuild { "full rebuild" } else { "changed files" };
            eprintln!("index: {mode} using {}", database.display());
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
        "usage: rag-rat <index|doctor|query|mcp|dump-config> --config <path> [query]\n\
         examples:\n\
         rag-rat index --config rag-rat.toml\n\
         rag-rat index --full --config rag-rat.toml\n\
         rag-rat query --config rag-rat.toml \"semantic recall\""
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
        if arg == "--config" {
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
