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
        "index_status" => {
            "Report SQLite index freshness, git metadata, parser failures, and file counts."
        },
        _ => "Unknown tool.",
    }
}

pub fn schema(name: &str) -> Value {
    match name {
        "semantic_search" => json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "default": 10},
                "include_generated": {"type": "boolean", "default": false}
            },
            "required": ["query"]
        }),
        "symbol_lookup" => json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string"},
                "language": {"type": "string"},
                "limit": {"type": "integer", "default": 20}
            },
            "required": ["symbol"]
        }),
        "find_callers" | "trace_callees" | "docs_for_symbol" => json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string"},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["symbol"]
        }),
        "impact_surface" => json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["query"]
        }),
        "ffi_surface" => json!({
            "type": "object",
            "properties": {"limit": {"type": "integer", "default": 50}}
        }),
        "read_chunk" => json!({
            "type": "object",
            "properties": {"chunk_id": {"type": "integer"}},
            "required": ["chunk_id"]
        }),
        "commit_search" | "commits_touching_query" => json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "default": 10}
            },
            "required": ["query"]
        }),
        "git_history_for_path" => json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["path"]
        }),
        "git_history_for_symbol" => json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string"},
                "language": {"type": "string"},
                "limit": {"type": "integer", "default": 20}
            },
            "required": ["symbol"]
        }),
        "git_blame_chunk" => json!({
            "type": "object",
            "properties": {"chunk_id": {"type": "integer"}},
            "required": ["chunk_id"]
        }),
        "papertrail_for_chunk" => json!({
            "type": "object",
            "properties": {
                "chunk_id": {"type": "integer"},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["chunk_id"]
        }),
        "papertrail_for_symbol" => json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string"},
                "language": {"type": "string"},
                "limit": {"type": "integer", "default": 20}
            },
            "required": ["symbol"]
        }),
        "papertrail_for_commit" => json!({
            "type": "object",
            "properties": {
                "commit_hash": {"type": "string"},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["commit_hash"]
        }),
        "github_issue_search" | "rationale_search" => json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "default": 10}
            },
            "required": ["query"]
        }),
        "github_refs_for_path" => json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["path"]
        }),
        "index_status" => json!({"type": "object", "properties": {}}),
        _ => json!({"type": "object"}),
    }
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
