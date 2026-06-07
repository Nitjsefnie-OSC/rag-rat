use std::{path::Path, str::FromStr};

use rag_rat_core::{
    IndexDatabase,
    language::Language,
    query::{
        graph::{GraphResolutionMode, GraphTraversalOptions},
        graph_meta::GraphMetaMode,
        impact::ImpactSurfaceOptions,
        symbol::SymbolSelector,
    },
    search::lexical::SearchOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TOOL_NAMES: &[&str] = &[
    "semantic_search",
    "symbol_lookup",
    "find_callers",
    "trace_callees",
    "compare_graph_to_text",
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
    #[serde(default)]
    pub explain: bool,
    #[serde(default = "default_true")]
    pub include_git: bool,
    #[serde(default = "default_true")]
    pub include_papertrail: bool,
    #[serde(default = "default_search_graph_mode")]
    pub include_graph: String,
    #[serde(default = "default_search_graph_limit")]
    pub graph_limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolArgs {
    pub symbol: Option<String>,
    pub symbol_path: Option<String>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub language: Option<String>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_symbol_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolGraphArgs {
    pub symbol: Option<String>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub resolution: Option<String>,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default)]
    pub include_references: bool,
    #[serde(default)]
    pub include_unresolved: bool,
    #[serde(default)]
    pub include_macros: bool,
    #[serde(default)]
    pub include_common_methods: bool,
    pub edge_kinds: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CompareGraphTextArgs {
    pub pattern: String,
    pub symbol: Option<String>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub resolution: Option<String>,
    #[serde(default = "default_compare_limit")]
    pub limit: u32,
    #[serde(default = "default_true")]
    pub include_tests: bool,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default)]
    pub include_references: bool,
    #[serde(default)]
    pub include_unresolved: bool,
    #[serde(default)]
    pub include_macros: bool,
    #[serde(default)]
    pub include_common_methods: bool,
    pub edge_kinds: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ImpactArgs {
    pub query: Option<String>,
    pub symbol: Option<String>,
    pub symbol_path: Option<String>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub resolution: Option<String>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    #[serde(default = "default_true")]
    pub include_tests: bool,
    #[serde(default = "default_true")]
    pub include_docs: bool,
    #[serde(default = "default_true")]
    pub include_git: bool,
    #[serde(default = "default_true")]
    pub include_papertrail: bool,
    #[serde(default = "default_true")]
    pub include_text_fallback: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LimitArgs {
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadChunkArgs {
    pub chunk_id: i64,
    #[serde(default = "default_read_chunk_graph_mode")]
    pub include_graph: String,
    #[serde(default = "default_read_chunk_graph_limit")]
    pub graph_limit: u32,
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
            let graph_mode = GraphMetaMode::parse(&args.include_graph)?;
            let options = SearchOptions {
                include_git: args.include_git,
                include_papertrail: args.include_papertrail,
            };
            if args.explain {
                json!(db.search_explain_with_graph_meta_options(
                    &args.query,
                    args.limit,
                    args.include_generated,
                    graph_mode,
                    args.graph_limit,
                    options
                )?)
            } else {
                json!(db.search_with_graph_meta_options(
                    &args.query,
                    args.limit,
                    args.include_generated,
                    graph_mode,
                    args.graph_limit,
                    options
                )?)
            }
        },
        "symbol_lookup" => {
            let args: SymbolArgs = serde_json::from_value(arguments)?;
            json!(db.symbol_candidates(&symbol_selector(args)?)?)
        },
        "find_callers" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            let resolution_mode = GraphResolutionMode::parse(args.resolution.as_deref())?;
            graph_tool(&db, args, resolution_mode, true)?
        },
        "trace_callees" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            let resolution_mode = GraphResolutionMode::parse(args.resolution.as_deref())?;
            graph_tool(&db, args, resolution_mode, false)?
        },
        "compare_graph_to_text" => {
            let args: CompareGraphTextArgs = serde_json::from_value(arguments)?;
            let resolution_mode = GraphResolutionMode::parse(args.resolution.as_deref())?;
            compare_graph_to_text_tool(&db, args, resolution_mode)?
        },
        "impact_surface" => {
            let args: ImpactArgs = serde_json::from_value(arguments)?;
            let resolution_mode = GraphResolutionMode::parse(args.resolution.as_deref())?;
            impact_tool(&db, args, resolution_mode)?
        },
        "ffi_surface" => {
            let args: LimitArgs = serde_json::from_value(arguments)?;
            json!(db.ffi_surface(args.limit)?)
        },
        "docs_for_symbol" => {
            let args: SymbolGraphArgs = serde_json::from_value(arguments)?;
            docs_for_symbol_tool(&db, args)?
        },
        "read_chunk" => {
            let args: ReadChunkArgs = serde_json::from_value(arguments)?;
            json!(db.read_chunk_with_graph(
                args.chunk_id,
                GraphMetaMode::parse(&args.include_graph)?,
                args.graph_limit
            )?)
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
            git_history_for_symbol_tool(&db, args)?
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
            papertrail_for_symbol_tool(&db, args)?
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

fn graph_tool(
    db: &IndexDatabase,
    args: SymbolGraphArgs,
    resolution_mode: GraphResolutionMode,
    reverse: bool,
) -> anyhow::Result<Value> {
    let limit = args.limit;
    let include_references = args.include_references;
    let include_unresolved = args.include_unresolved;
    let include_macros = args.include_macros;
    let include_common_methods = args.include_common_methods;
    let edge_kinds = args.edge_kinds.clone();
    let allow_ambiguous = args.allow_ambiguous;
    let selector = graph_symbol_selector(&args)?;
    let selected = db.select_symbol(&selector)?;
    match selected {
        Ok(Some(symbol)) => {
            let options = GraphTraversalOptions {
                include_references,
                include_unresolved,
                include_macros,
                include_common_methods,
                edge_kinds,
                resolution_mode,
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: args.logical_symbol_id,
            };
            Ok(json!(db.graph_traversal_report(
                if reverse { "find_callers" } else { "trace_callees" },
                &symbol,
                reverse,
                limit,
                &options
            )?))
        },
        Ok(None) if allow_ambiguous => {
            let Some(symbol) = args.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            let options = GraphTraversalOptions {
                include_references,
                include_unresolved,
                include_macros,
                include_common_methods,
                edge_kinds,
                resolution_mode,
                symbol_id: args.symbol_id,
                logical_symbol_id: args.logical_symbol_id,
            };
            let hops = if reverse {
                db.find_callers_with_options(symbol, limit, &options)?
            } else {
                db.trace_callees_with_options(symbol, limit, &options)?
            };
            Ok(json!(hops))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

fn docs_for_symbol_tool(db: &IndexDatabase, args: SymbolGraphArgs) -> anyhow::Result<Value> {
    let selector = graph_symbol_selector(&args)?;
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.docs_for_symbol(&symbol.qualified_name, args.limit)?)),
        Ok(None) if args.allow_ambiguous => {
            let Some(symbol) = args.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            Ok(json!(db.docs_for_symbol(symbol, args.limit)?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

fn compare_graph_to_text_tool(
    db: &IndexDatabase,
    args: CompareGraphTextArgs,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Value> {
    let selector = SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: args.symbol_id,
        symbol_path: args.symbol_path,
        symbol: args.symbol,
        language: None,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    };
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => {
            let options = GraphTraversalOptions {
                include_references: args.include_references,
                include_unresolved: args.include_unresolved,
                include_macros: args.include_macros,
                include_common_methods: args.include_common_methods,
                edge_kinds: args.edge_kinds,
                resolution_mode,
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: args.logical_symbol_id,
            };
            Ok(json!(db.compare_graph_to_text(
                &symbol,
                &args.pattern,
                args.limit,
                &options,
                args.include_tests
            )?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

fn git_history_for_symbol_tool(db: &IndexDatabase, args: SymbolArgs) -> anyhow::Result<Value> {
    let selector = symbol_selector(args)?;
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.git_history_for_symbol(
            &symbol.qualified_name,
            optional_language(Some(symbol.language.clone()))?,
            selector.limit
        )?)),
        Ok(None) if selector.allow_ambiguous => {
            let Some(symbol) = selector.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            Ok(json!(db.git_history_for_symbol(symbol, selector.language, selector.limit)?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

fn papertrail_for_symbol_tool(db: &IndexDatabase, args: SymbolArgs) -> anyhow::Result<Value> {
    let selector = symbol_selector(args)?;
    match db.select_symbol(&selector)? {
        Ok(Some(symbol)) => Ok(json!(db.papertrail_for_selected_symbol(&symbol, selector.limit)?)),
        Ok(None) if selector.allow_ambiguous => {
            let Some(symbol) = selector.symbol.as_deref() else {
                return Ok(Value::Null);
            };
            Ok(json!(db.papertrail_for_symbol(symbol, selector.language, selector.limit)?))
        },
        Ok(None) => Ok(Value::Null),
        Err(disambiguation) => Ok(json!(disambiguation)),
    }
}

fn impact_tool(
    db: &IndexDatabase,
    args: ImpactArgs,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Value> {
    let options = ImpactSurfaceOptions {
        resolution_mode,
        include_tests: args.include_tests,
        include_docs: args.include_docs,
        include_git: args.include_git,
        include_papertrail: args.include_papertrail,
        include_text_fallback: args.include_text_fallback,
    };
    if args.logical_symbol_id.is_some()
        || args.symbol_id.is_some()
        || args.symbol_path.is_some()
        || args.symbol.is_some()
    {
        let selector = SymbolSelector {
            logical_symbol_id: args.logical_symbol_id,
            symbol_id: args.symbol_id,
            symbol_path: args.symbol_path,
            symbol: args.symbol,
            language: None,
            allow_ambiguous: args.allow_ambiguous,
            limit: args.limit,
        };
        return match db.select_symbol(&selector)? {
            Ok(Some(symbol)) => Ok(json!(
                db.impact_surface_report_for_selected_symbol(&symbol, args.limit, &options)?
            )),
            Ok(None) if selector.allow_ambiguous => {
                let Some(symbol) = selector.symbol.as_deref() else {
                    return Ok(Value::Null);
                };
                Ok(json!(db.impact_surface_with_options(symbol, args.limit, resolution_mode)?))
            },
            Ok(None) => Ok(Value::Null),
            Err(disambiguation) => Ok(json!(disambiguation)),
        };
    }
    let Some(query) = args.query.as_deref() else {
        anyhow::bail!("impact_surface requires query, symbol_id, symbol_path, or symbol");
    };
    Ok(json!(db.impact_surface_with_options(query, args.limit, resolution_mode)?))
}

fn symbol_selector(args: SymbolArgs) -> anyhow::Result<SymbolSelector> {
    Ok(SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: args.symbol_id,
        symbol_path: args.symbol_path,
        symbol: args.symbol,
        language: optional_language(args.language)?,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    })
}

fn graph_symbol_selector(args: &SymbolGraphArgs) -> anyhow::Result<SymbolSelector> {
    Ok(SymbolSelector {
        logical_symbol_id: args.logical_symbol_id,
        symbol_id: args.symbol_id,
        symbol_path: args.symbol_path.clone(),
        symbol: args.symbol.clone(),
        language: None,
        allow_ambiguous: args.allow_ambiguous,
        limit: args.limit,
    })
}

pub fn description(name: &str) -> &'static str {
    match name {
        "semantic_search" => {
            "Search indexed source and docs with SQLite BM25 lexical recall; validates stale hits."
        },
        "symbol_lookup" => "Find exact or fuzzy Rust, TypeScript, Kotlin symbols.",
        "find_callers" => "Traverse tree-sitter-derived reverse graph edges for callers.",
        "trace_callees" => "Traverse tree-sitter-derived forward graph edges for callees.",
        "compare_graph_to_text" => {
            "Compare graph caller edges for a symbol against regex text hits in indexed source."
        },
        "impact_surface" => {
            "Graph-backed coding preflight with structural, textual fallback, and papertrail evidence."
        },
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
        "compare_graph_to_text" => schema_for::<CompareGraphTextArgs>(),
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
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .unwrap_or_else(|_| json!({"type": "object"}));
    strip_schema_metadata(&mut schema);
    schema
}

fn strip_schema_metadata(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("title");
            for child in map.values_mut() {
                strip_schema_metadata(child);
            }
        },
        Value::Array(items) => {
            for item in items {
                strip_schema_metadata(item);
            }
        },
        _ => {},
    }
}

fn optional_language(language: Option<String>) -> anyhow::Result<Option<Language>> {
    language.map(|value| Language::from_str(&value)).transpose().map_err(Into::into)
}

fn default_search_limit() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

fn default_search_graph_mode() -> String {
    "compact".to_string()
}

fn default_search_graph_limit() -> u32 {
    3
}

fn default_read_chunk_graph_mode() -> String {
    "full".to_string()
}

fn default_read_chunk_graph_limit() -> u32 {
    20
}

fn default_symbol_limit() -> u32 {
    20
}

fn default_graph_limit() -> u32 {
    50
}

fn default_compare_limit() -> u32 {
    10_000
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
            "compare_graph_to_text",
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
        assert_schema_has_property(tools, "semantic_search", "include_graph");
        assert_schema_has_property(tools, "semantic_search", "graph_limit");
        assert_schema_has_property(tools, "semantic_search", "include_git");
        assert_schema_has_property(tools, "semantic_search", "include_papertrail");
        assert_schema_has_property(tools, "semantic_search", "explain");
        assert_symbol_selector_schema(tools, "symbol_lookup");
        assert_schema_has_property(tools, "find_callers", "include_references");
        assert_schema_has_property(tools, "find_callers", "include_unresolved");
        assert_schema_has_property(tools, "find_callers", "include_macros");
        assert_schema_has_property(tools, "find_callers", "include_common_methods");
        assert_schema_has_property(tools, "find_callers", "edge_kinds");
        assert_schema_has_property(tools, "find_callers", "resolution");
        assert_schema_has_property(tools, "find_callers", "logical_symbol_id");
        assert_symbol_selector_schema(tools, "find_callers");
        assert_schema_has_property(tools, "trace_callees", "include_references");
        assert_schema_has_property(tools, "trace_callees", "include_unresolved");
        assert_schema_has_property(tools, "trace_callees", "include_macros");
        assert_schema_has_property(tools, "trace_callees", "include_common_methods");
        assert_schema_has_property(tools, "trace_callees", "edge_kinds");
        assert_schema_has_property(tools, "trace_callees", "resolution");
        assert_schema_has_property(tools, "trace_callees", "logical_symbol_id");
        assert_symbol_selector_schema(tools, "trace_callees");
        assert_schema_requires(tools, "compare_graph_to_text", "pattern");
        assert_schema_has_property(tools, "compare_graph_to_text", "include_unresolved");
        assert_schema_has_property(tools, "compare_graph_to_text", "include_macros");
        assert_schema_has_property(tools, "compare_graph_to_text", "include_common_methods");
        assert_schema_has_property(tools, "compare_graph_to_text", "include_tests");
        assert_schema_has_property(tools, "compare_graph_to_text", "edge_kinds");
        assert_schema_has_property(tools, "compare_graph_to_text", "resolution");
        assert_schema_has_property(tools, "compare_graph_to_text", "logical_symbol_id");
        assert_symbol_selector_schema(tools, "compare_graph_to_text");
        assert_schema_has_property(tools, "impact_surface", "resolution");
        assert_schema_has_property(tools, "impact_surface", "include_tests");
        assert_schema_has_property(tools, "impact_surface", "include_docs");
        assert_schema_has_property(tools, "impact_surface", "include_git");
        assert_schema_has_property(tools, "impact_surface", "include_papertrail");
        assert_schema_has_property(tools, "impact_surface", "include_text_fallback");
        assert_schema_has_property(tools, "impact_surface", "logical_symbol_id");
        assert_symbol_selector_schema(tools, "impact_surface");
        assert_symbol_selector_schema(tools, "docs_for_symbol");
        assert_symbol_selector_schema(tools, "git_history_for_symbol");
        assert_symbol_selector_schema(tools, "papertrail_for_symbol");
        assert_schema_requires(tools, "read_chunk", "chunk_id");
        assert_schema_has_property(tools, "read_chunk", "include_graph");
        assert_schema_has_property(tools, "read_chunk", "graph_limit");
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

    #[test]
    fn mcp_symbol_id_selection_disambiguates_graph_tools() {
        let root = unique_temp_root();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod one;\npub mod two;\n").unwrap();
        fs::write(
            root.join("src/one.rs"),
            "pub fn shared() {}\npub fn caller_one() {\n    shared();\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/two.rs"),
            "pub fn shared() {}\npub fn caller_two() {\n    shared();\n}\n",
        )
        .unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();
        drop(db);

        let lookup =
            call_tool(&config.database, "symbol_lookup", json!({"symbol": "shared"})).unwrap();
        assert_eq!(lookup["disambiguation_required"], true);
        let candidates = lookup["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| {
            candidate["symbol_id"].is_i64() && candidate["symbol_path"].as_str().is_some()
        }));

        let ambiguous =
            call_tool(&config.database, "find_callers", json!({"symbol": "shared"})).unwrap();
        assert_eq!(ambiguous["disambiguation_required"], true);
        assert_eq!(ambiguous["candidates"].as_array().unwrap().len(), 2);

        let one = candidates
            .iter()
            .find(|candidate| candidate["symbol_path"].as_str().unwrap().contains("one.rs"))
            .unwrap();
        let exact = call_tool(
            &config.database,
            "find_callers",
            json!({
                "symbol_id": one["symbol_id"].as_i64().unwrap(),
                "resolution": "exact",
                "edge_kinds": ["calls_name"]
            }),
        )
        .unwrap();
        assert_eq!(exact["query"]["tool"], "find_callers");
        assert_eq!(exact["query"]["symbol_id"], one["symbol_id"]);
        assert_eq!(exact["query"]["resolution"], "exact");
        assert_eq!(exact["summary"]["returned_count"], 1);
        assert_eq!(exact["summary"]["total_matching_edges"], 1);
        assert_eq!(exact["summary"]["truncated"], false);
        assert_eq!(exact["summary"]["exact_verified"], 1);
        assert_eq!(exact["summary"]["false_positive_risk"], "low");
        assert_eq!(exact["summary"]["completeness_risk"], "low");
        assert_eq!(exact["coverage"]["stale_files"], 0);
        assert!(!exact["coverage"]["parser_coverage_for_paths"].as_array().unwrap().is_empty());
        let exact_results = exact["results"].as_array().unwrap();
        assert_eq!(exact_results.len(), 1, "exact callers: {exact:?}");
        assert_eq!(exact_results[0]["verified_target_symbol"], true);
        assert!(exact_results[0]["from_symbol"].as_str().unwrap().contains("caller"));

        let comparison = call_tool(
            &config.database,
            "compare_graph_to_text",
            json!({
                "symbol_id": one["symbol_id"].as_i64().unwrap(),
                "pattern": "    shared\\(",
                "resolution": "exact",
                "edge_kinds": ["calls_name"]
            }),
        )
        .unwrap();
        assert_eq!(comparison["query"]["symbol_id"], one["symbol_id"]);
        assert_eq!(comparison["summary"]["graph_edges"], 1);
        assert_eq!(comparison["summary"]["graph_hits"], 1);
        assert_eq!(comparison["summary"]["text_hits"], 2);
        assert_eq!(comparison["summary"]["matched"], 1);
        assert_eq!(comparison["summary"]["text_only"], 1);
        assert_eq!(comparison["summary"]["likely_parser_gaps"], 1);
        assert_eq!(comparison["summary"]["graph_only"], 0);
        assert_eq!(comparison["summary"]["complete"], false);
        assert_eq!(comparison["summary"]["recommended_fallback"], "text");
        assert_eq!(comparison["matched_hits"].as_array().unwrap().len(), 1);
        assert_eq!(comparison["text_only_hits"].as_array().unwrap().len(), 1);
        assert_eq!(comparison["likely_parser_gaps"].as_array().unwrap().len(), 1);

        let impact = call_tool(
            &config.database,
            "impact_surface",
            json!({
                "symbol_id": one["symbol_id"].as_i64().unwrap(),
                "resolution": "exact",
                "include_tests": true,
                "include_docs": true,
                "include_git": true,
                "include_papertrail": true,
                "include_text_fallback": true
            }),
        )
        .unwrap();
        assert_eq!(impact["query"]["symbol_id"], one["symbol_id"]);
        assert_eq!(impact["query"]["resolution"], "exact");
        assert!(impact["direct_semantic_callers"].as_array().unwrap().len() == 1);
        assert!(impact["direct_semantic_callees"].as_array().unwrap().is_empty());
        assert!(impact["text_fallback_hits"].is_array());
        assert!(impact["completeness_and_caveats"]["caveats"].as_array().unwrap().iter().any(
            |note| note.as_str().is_some_and(|value| value.contains("tree-sitter/syntactic"))
        ));

        let papertrail = call_tool(
            &config.database,
            "papertrail_for_symbol",
            json!({"symbol_id": one["symbol_id"].as_i64().unwrap()}),
        )
        .unwrap();
        assert!(papertrail["current_source"]["symbol"].as_str().unwrap().contains("shared"));

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

    fn assert_symbol_selector_schema(tools: &[Value], name: &str) {
        for field in ["symbol", "symbol_path", "symbol_id", "logical_symbol_id", "allow_ambiguous"]
        {
            assert_schema_has_property(tools, name, field);
        }
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

    fn rust_config(root: PathBuf) -> Config {
        Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
        }
    }

    fn unique_temp_root() -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rag-rat-mcp-test-{}-{id}", std::process::id()))
    }
}
