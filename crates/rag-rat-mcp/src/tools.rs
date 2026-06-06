use std::{path::Path, str::FromStr};

use rag_rat_core::{IndexDatabase, language::Language};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TOOL_NAMES: &[&str] = &[
    "semantic_search",
    "symbol_lookup",
    "find_callers",
    "trace_callees",
    "impact_surface",
    "ffi_surface",
    "docs_for_symbol",
    "read_chunk",
    "commit_search",
    "git_history_for_path",
    "git_history_for_symbol",
    "commits_touching_query",
    "git_blame_chunk",
    "papertrail_for_chunk",
    "papertrail_for_symbol",
    "papertrail_for_commit",
    "github_issue_search",
    "github_refs_for_path",
    "rationale_search",
    "local_ai_status",
    "heal_index",
    "github_sync_status",
    "index_status",
];

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    #[serde(default)]
    pub include_generated: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolArgs {
    pub symbol: String,
    pub language: Option<String>,
    #[serde(default = "default_symbol_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolGraphArgs {
    pub symbol: String,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ImpactArgs {
    pub query: String,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LimitArgs {
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadChunkArgs {
    pub chunk_id: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PathHistoryArgs {
    pub path: String,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameChunkArgs {
    pub chunk_id: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PapertrailChunkArgs {
    pub chunk_id: i64,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PapertrailCommitArgs {
    pub commit_hash: String,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HealIndexArgs {
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct EmptyArgs {}

pub fn list_tools() -> Value {
    json!(
        TOOL_NAMES
            .iter()
            .map(|name| json!({
                "name": name,
                "description": description(name),
                "inputSchema": schema(name)
            }))
            .collect::<Vec<_>>()
    )
}

pub fn call_tool(database: &Path, name: &str, arguments: Value) -> anyhow::Result<Value> {
    let db = IndexDatabase::open(database)?;
    let result = match name {
        "semantic_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.search(&args.query, args.limit, args.include_generated)?)
        },
        "symbol_lookup" => {
            let args: SymbolArgs = serde_json::from_value(arguments)?;
            json!(db.symbols(&args.symbol, optional_language(args.language)?, args.limit)?)
        },
        "find_callers" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            json!(db.find_callers(&args.symbol, args.limit)?)
        },
        "trace_callees" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            json!(db.trace_callees(&args.symbol, args.limit)?)
        },
        "impact_surface" => {
            let args: ImpactArgs = serde_json::from_value(arguments)?;
            json!(db.impact_surface(&args.query, args.limit)?)
        },
        "ffi_surface" => {
            let args: LimitArgs = serde_json::from_value(arguments)?;
            json!(db.ffi_surface(args.limit)?)
        },
        "docs_for_symbol" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            json!(db.docs_for_symbol(&args.symbol, args.limit)?)
        },
        "read_chunk" => {
            let args: ReadChunkArgs = serde_json::from_value(arguments)?;
            json!(db.read_chunk(args.chunk_id)?)
        },
        "commit_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.commit_search(&args.query, args.limit)?)
        },
        "git_history_for_path" => {
            let args: PathHistoryArgs = serde_json::from_value(arguments)?;
            json!(db.git_history_for_path(&args.path, args.limit)?)
        },
        "git_history_for_symbol" => {
            let args: SymbolArgs = serde_json::from_value(arguments)?;
            json!(db.git_history_for_symbol(
                &args.symbol,
                optional_language(args.language)?,
                args.limit
            )?)
        },
        "commits_touching_query" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.commits_touching_query(&args.query, args.limit)?)
        },
        "git_blame_chunk" => {
            let args: BlameChunkArgs = serde_json::from_value(arguments)?;
            json!(db.git_blame_chunk(args.chunk_id)?)
        },
        "papertrail_for_chunk" => {
            let args: PapertrailChunkArgs = serde_json::from_value(arguments)?;
            json!(db.papertrail_for_chunk(args.chunk_id, args.limit)?)
        },
        "papertrail_for_symbol" => {
            let args: SymbolArgs = serde_json::from_value(arguments)?;
            json!(db.papertrail_for_symbol(
                &args.symbol,
                optional_language(args.language)?,
                args.limit
            )?)
        },
        "papertrail_for_commit" => {
            let args: PapertrailCommitArgs = serde_json::from_value(arguments)?;
            json!(db.papertrail_for_commit(&args.commit_hash, args.limit)?)
        },
        "github_issue_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.github_issue_search(&args.query, args.limit)?)
        },
        "github_refs_for_path" => {
            let args: PathHistoryArgs = serde_json::from_value(arguments)?;
            json!(db.github_refs_for_path(&args.path, args.limit)?)
        },
        "rationale_search" => {
            let args: SearchArgs = serde_json::from_value(arguments)?;
            json!(db.rationale_search(&args.query, args.limit)?)
        },
        "local_ai_status" => json!(db.local_ai_status()?),
        "heal_index" => {
            let args: HealIndexArgs = serde_json::from_value(arguments)?;
            json!(db.heal_index(args.limit)?)
        },
        "github_sync_status" => json!(db.github_sync_status()?),
        "index_status" => json!(db.status(database)?),
        other => anyhow::bail!("unknown tool `{other}`"),
    };
    Ok(result)
}

pub fn description(name: &str) -> &'static str {
    match name {
        "semantic_search" => {
            "Search indexed source and docs with SQLite BM25 lexical recall; validates stale hits."
        },
        "symbol_lookup" => "Find exact or fuzzy Rust, TypeScript, Kotlin symbols.",
        "find_callers" => "Traverse reverse graph edges for callers when graph data exists.",
        "trace_callees" => "Traverse forward graph edges for callees when graph data exists.",
        "impact_surface" => "Estimate affected source, test, generated binding, and docs surfaces.",
        "ffi_surface" => "Find UniFFI/export/generated-binding/call-site candidates.",
        "docs_for_symbol" => "Find docs chunks related to a symbol.",
        "read_chunk" => "Read current text for one selected chunk ID with anchor validation.",
        "commit_search" => "Search historical git commit subjects and bodies.",
        "git_history_for_path" => "Return historical commits that touched one current path.",
        "git_history_for_symbol" => {
            "Resolve a current symbol, then return historical commits touching its path."
        },
        "commits_touching_query" => {
            "Combine commit-message and current file-change evidence for a query."
        },
        "git_blame_chunk" => "Compute lazy hash-bound git blame summary for one current chunk.",
        "papertrail_for_chunk" => "Return current chunk context plus cached GitHub rationale.",
        "papertrail_for_symbol" => "Return current symbol context plus cached GitHub rationale.",
        "papertrail_for_commit" => "Return cached GitHub rationale related to a historical commit.",
        "github_issue_search" => "Search cached GitHub issue and PR text.",
        "github_refs_for_path" => "List discovered GitHub references for one current path.",
        "rationale_search" => "Search cached GitHub rationale snippets.",
        "local_ai_status" => "Report explicit local AI capability and artifact status.",
        "heal_index" => "Repair stale already-indexed files and refresh SQLite FTS.",
        "github_sync_status" => "Report local GitHub papertrail cache status.",
        "index_status" => {
            "Report SQLite index freshness, git metadata, parser failures, and file counts."
        },
        _ => "Unknown tool.",
    }
}

pub fn schema(name: &str) -> Value {
    match name {
        "semantic_search"
        | "commit_search"
        | "commits_touching_query"
        | "github_issue_search"
        | "rationale_search" => schema_for::<SearchArgs>(),
        "symbol_lookup" | "git_history_for_symbol" | "papertrail_for_symbol" => {
            schema_for::<SymbolArgs>()
        },
        "find_callers" | "trace_callees" | "docs_for_symbol" => schema_for::<SymbolGraphArgs>(),
        "impact_surface" => schema_for::<ImpactArgs>(),
        "ffi_surface" => schema_for::<LimitArgs>(),
        "read_chunk" => schema_for::<ReadChunkArgs>(),
        "git_history_for_path" | "github_refs_for_path" => schema_for::<PathHistoryArgs>(),
        "git_blame_chunk" => schema_for::<BlameChunkArgs>(),
        "papertrail_for_chunk" => schema_for::<PapertrailChunkArgs>(),
        "papertrail_for_commit" => schema_for::<PapertrailCommitArgs>(),
        "heal_index" => schema_for::<HealIndexArgs>(),
        "local_ai_status" | "github_sync_status" | "index_status" => schema_for::<EmptyArgs>(),
        _ => json!({"type": "object"}),
    }
}

fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type": "object"}))
}

fn optional_language(language: Option<String>) -> anyhow::Result<Option<Language>> {
    language.map(|value| Language::from_str(&value)).transpose().map_err(Into::into)
}

fn default_search_limit() -> u32 {
    10
}

fn default_symbol_limit() -> u32 {
    20
}

fn default_graph_limit() -> u32 {
    50
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rag_rat_core::{Config, IndexDatabase, ResolvedTarget, TargetKind, language::Language};
    use serde_json::json;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn list_tools_exposes_complete_typed_schemas() {
        let tools = list_tools();
        let tools = tools.as_array().expect("tools/list shape");
        let names =
            tools.iter().map(|tool| tool["name"].as_str().expect("tool name")).collect::<Vec<_>>();

        for expected in [
            "semantic_search",
            "symbol_lookup",
            "find_callers",
            "trace_callees",
            "impact_surface",
            "ffi_surface",
            "docs_for_symbol",
            "read_chunk",
            "index_status",
            "commit_search",
            "git_history_for_path",
            "git_history_for_symbol",
            "commits_touching_query",
            "git_blame_chunk",
            "papertrail_for_chunk",
            "papertrail_for_symbol",
            "papertrail_for_commit",
            "github_issue_search",
            "github_refs_for_path",
            "rationale_search",
            "local_ai_status",
            "heal_index",
            "github_sync_status",
        ] {
            assert!(names.contains(&expected), "missing MCP tool {expected}");
        }

        assert_schema_requires(tools, "semantic_search", "query");
        assert_schema_requires(tools, "read_chunk", "chunk_id");
        assert_schema_requires(tools, "papertrail_for_commit", "commit_hash");
        assert_schema_has_property(tools, "heal_index", "limit");
        assert_eq!(tool_schema(tools, "local_ai_status")["type"], "object");
    }

    #[test]
    fn mcp_tool_calls_preserve_compatibility_shapes() {
        let (root, config) = mixed_config();
        let db = IndexDatabase::rebuild(&config).unwrap();
        drop(db);

        let search =
            call_tool(&config.database, "semantic_search", json!({"query": "alpha"})).unwrap();
        let hit = search.as_array().unwrap().first().expect("semantic hit");
        for field in ["chunk_id", "path", "start_line", "end_line", "summary", "score"] {
            assert!(hit.get(field).is_some(), "semantic_search missing {field}");
        }
        let chunk_id = hit["chunk_id"].as_i64().unwrap();

        let chunk =
            call_tool(&config.database, "read_chunk", json!({"chunk_id": chunk_id})).unwrap();
        for field in ["chunk_id", "path", "start_line", "end_line", "text"] {
            assert!(chunk.get(field).is_some(), "read_chunk missing {field}");
        }

        let status = call_tool(&config.database, "index_status", json!({})).unwrap();
        assert!(status["database"].as_str().unwrap().ends_with("index.sqlite"));
        assert_eq!(status["fts_fresh"], true);
        assert!(status["local_ai"].is_object());

        let papertrail = call_tool(
            &config.database,
            "papertrail_for_symbol",
            json!({"symbol": "alpha_symbol", "language": "rust"}),
        )
        .unwrap();
        assert!(papertrail["current_source"].is_object());
        assert!(papertrail["github_evidence"].is_array());

        let github_status = call_tool(&config.database, "github_sync_status", json!({})).unwrap();
        assert!(github_status["capability"].is_string());

        let local_ai = call_tool(&config.database, "local_ai_status", json!({})).unwrap();
        assert_eq!(local_ai["embedding"]["state"], "MissingModel");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mcp_read_chunk_and_heal_index_do_not_return_stale_text() {
        let (root, config) = markdown_config("# Title\nalpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        drop(db);

        let search =
            call_tool(&config.database, "semantic_search", json!({"query": "alpha"})).unwrap();
        let chunk_id = search.as_array().unwrap()[0]["chunk_id"].as_i64().unwrap();
        fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

        let chunk =
            call_tool(&config.database, "read_chunk", json!({"chunk_id": chunk_id})).unwrap();
        assert_eq!(chunk["start_line"], 2);
        assert_eq!(chunk["text"], "# Title\nalpha token\n");

        fs::write(root.join("docs/search.md"), "# Changed\nbeta token\n").unwrap();
        let report = call_tool(&config.database, "heal_index", json!({"limit": 10})).unwrap();
        assert_eq!(report["healed_files"], 1);
        assert_eq!(report["fts_fresh"], true);

        let stale =
            call_tool(&config.database, "semantic_search", json!({"query": "alpha"})).unwrap();
        assert!(stale.as_array().unwrap().is_empty());
        let fresh =
            call_tool(&config.database, "semantic_search", json!({"query": "beta"})).unwrap();
        assert_eq!(fresh.as_array().unwrap().len(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    fn assert_schema_requires(tools: &[Value], name: &str, field: &str) {
        let schema = tool_schema(tools, name);
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|value| value == field), "{name} should require {field}");
    }

    fn assert_schema_has_property(tools: &[Value], name: &str, field: &str) {
        let schema = tool_schema(tools, name);
        assert!(schema["properties"].get(field).is_some(), "{name} should define {field}");
    }

    fn tool_schema<'a>(tools: &'a [Value], name: &str) -> &'a Value {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| &tool["inputSchema"])
            .expect("tool schema")
    }

    fn mixed_config() -> (PathBuf, Config) {
        let root = unique_temp_root();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn alpha_symbol() {}\n").unwrap();
        (
            root.clone(),
            Config {
                root: root.clone(),
                database: root.join(".rag-rat/index.sqlite"),
                targets: vec![
                    ResolvedTarget {
                        name: "markdown".to_string(),
                        language: Language::Markdown,
                        directories: vec![PathBuf::from("docs")],
                        include: vec!["**/*.md".to_string()],
                        exclude: Vec::new(),
                        kind: TargetKind::Docs,
                    },
                    ResolvedTarget {
                        name: "rust".to_string(),
                        language: Language::Rust,
                        directories: vec![PathBuf::from("src")],
                        include: vec!["**/*.rs".to_string()],
                        exclude: Vec::new(),
                        kind: TargetKind::Source,
                    },
                ],
            },
        )
    }

    fn markdown_config(text: &str) -> (PathBuf, Config) {
        let root = unique_temp_root();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/search.md"), text).unwrap();
        (
            root.clone(),
            Config {
                root: root.clone(),
                database: root.join(".rag-rat/index.sqlite"),
                targets: vec![ResolvedTarget {
                    name: "markdown".to_string(),
                    language: Language::Markdown,
                    directories: vec![PathBuf::from("docs")],
                    include: vec!["**/*.md".to_string()],
                    exclude: Vec::new(),
                    kind: TargetKind::Docs,
                }],
            },
        )
    }

    fn unique_temp_root() -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rag-rat-mcp-test-{}-{id}", std::process::id()))
    }
}
