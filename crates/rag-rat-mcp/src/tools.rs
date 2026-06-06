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
