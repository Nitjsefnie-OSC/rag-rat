use rag_rat_core::Config;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, Content, Implementation, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde_json::{Map, Value, json};

use crate::tools::{
    BlameChunkArgs, CompareGraphTextArgs, EmptyArgs, HealIndexArgs, ImpactArgs, LimitArgs,
    PapertrailChunkArgs, PapertrailCommitArgs, PathHistoryArgs, ReadChunkArgs, SearchArgs,
    SymbolArgs, SymbolGraphArgs,
};

#[derive(Clone)]
pub struct RagRatService {
    config: Config,
}

impl RagRatService {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    fn call(&self, name: &str, value: Value) -> Result<CallToolResult, ErrorData> {
        let value = crate::tools::call_tool_for_config(&self.config, name, value)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let text = serde_json::to_string_pretty(&value)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_router]
impl RagRatService {
    #[tool(
        name = "semantic_search",
        description = "Search indexed source and docs with hybrid BM25/vector/structural ranking; optionally explains score components."
    )]
    fn semantic_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("semantic_search", json!(args))
    }

    #[tool(
        name = "symbol_lookup",
        description = "Find exact or fuzzy Rust, TypeScript, Kotlin symbols."
    )]
    fn symbol_lookup(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("symbol_lookup", json!(args))
    }

    #[tool(
        name = "find_callers",
        description = "Traverse tree-sitter-derived reverse graph edges for callers."
    )]
    fn find_callers(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("find_callers", json!(args))
    }

    #[tool(
        name = "trace_callees",
        description = "Traverse tree-sitter-derived forward graph edges for callees."
    )]
    fn trace_callees(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("trace_callees", json!(args))
    }

    #[tool(
        name = "compare_graph_to_text",
        description = "Compare graph caller edges for a symbol against regex text hits in indexed source."
    )]
    fn compare_graph_to_text(
        &self,
        Parameters(args): Parameters<CompareGraphTextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("compare_graph_to_text", json!(args))
    }

    #[tool(
        name = "impact_surface",
        description = "Graph-backed coding preflight with structural, textual fallback, and papertrail evidence."
    )]
    fn impact_surface(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("impact_surface", json!(args))
    }

    #[tool(
        name = "ffi_surface",
        description = "Find UniFFI/export/generated-binding/call-site candidates."
    )]
    fn ffi_surface(
        &self,
        Parameters(args): Parameters<LimitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("ffi_surface", json!(args))
    }

    #[tool(name = "docs_for_symbol", description = "Find docs chunks related to a symbol.")]
    fn docs_for_symbol(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("docs_for_symbol", json!(args))
    }

    #[tool(
        name = "read_chunk",
        description = "Read current text for one selected chunk ID with anchor validation."
    )]
    fn read_chunk(
        &self,
        Parameters(args): Parameters<ReadChunkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("read_chunk", json!(args))
    }

    #[tool(
        name = "commit_search",
        description = "Search historical git commit subjects and bodies."
    )]
    fn commit_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("commit_search", json!(args))
    }

    #[tool(
        name = "git_history_for_path",
        description = "Return historical commits that touched one current path."
    )]
    fn git_history_for_path(
        &self,
        Parameters(args): Parameters<PathHistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("git_history_for_path", json!(args))
    }

    #[tool(
        name = "git_history_for_symbol",
        description = "Resolve a current symbol, then return historical commits touching its path."
    )]
    fn git_history_for_symbol(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("git_history_for_symbol", json!(args))
    }

    #[tool(
        name = "commits_touching_query",
        description = "Combine commit-message and current file-change evidence for a query."
    )]
    fn commits_touching_query(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("commits_touching_query", json!(args))
    }

    #[tool(
        name = "git_blame_chunk",
        description = "Compute lazy hash-bound git blame summary for one current chunk."
    )]
    fn git_blame_chunk(
        &self,
        Parameters(args): Parameters<BlameChunkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("git_blame_chunk", json!(args))
    }

    #[tool(
        name = "papertrail_for_chunk",
        description = "Return current chunk context plus cached GitHub rationale."
    )]
    fn papertrail_for_chunk(
        &self,
        Parameters(args): Parameters<PapertrailChunkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("papertrail_for_chunk", json!(args))
    }

    #[tool(
        name = "papertrail_for_symbol",
        description = "Return current symbol context plus cached GitHub rationale."
    )]
    fn papertrail_for_symbol(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("papertrail_for_symbol", json!(args))
    }

    #[tool(
        name = "papertrail_for_commit",
        description = "Return cached GitHub rationale related to a historical commit."
    )]
    fn papertrail_for_commit(
        &self,
        Parameters(args): Parameters<PapertrailCommitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("papertrail_for_commit", json!(args))
    }

    #[tool(name = "github_issue_search", description = "Search cached GitHub issue and PR text.")]
    fn github_issue_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("github_issue_search", json!(args))
    }

    #[tool(
        name = "github_refs_for_path",
        description = "List discovered GitHub references for one current path."
    )]
    fn github_refs_for_path(
        &self,
        Parameters(args): Parameters<PathHistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("github_refs_for_path", json!(args))
    }

    #[tool(name = "rationale_search", description = "Search cached GitHub rationale snippets.")]
    fn rationale_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("rationale_search", json!(args))
    }

    #[tool(
        name = "local_ai_status",
        description = "Report explicit local AI capability and artifact status."
    )]
    fn local_ai_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("local_ai_status", json!({}))
    }

    #[tool(
        name = "heal_index",
        description = "Repair stale already-indexed files and refresh SQLite FTS."
    )]
    fn heal_index(
        &self,
        Parameters(args): Parameters<HealIndexArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("heal_index", json!(args))
    }

    #[tool(
        name = "github_sync_status",
        description = "Report local GitHub papertrail cache status."
    )]
    fn github_sync_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("github_sync_status", json!({}))
    }

    #[tool(
        name = "index_status",
        description = "Report SQLite index freshness, git metadata, parser failures, and file counts."
    )]
    fn index_status(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("index_status", json!({}))
    }
}

#[tool_handler]
impl ServerHandler for RagRatService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rag-rat", "0.1.0"))
            .with_instructions("Read-only-source repo intelligence. Index and auto-heal writes are confined to the configured SQLite database.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = crate::tools::TOOL_NAMES
            .iter()
            .map(|name| {
                let input_schema = match crate::tools::schema(name) {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                Tool::new((*name).to_string(), crate::tools::description(name), input_schema)
            })
            .collect();
        Ok(ListToolsResult { tools, meta: None, next_cursor: None })
    }
}

pub async fn run_stdio(config: Config) -> anyhow::Result<()> {
    let service = RagRatService::new(config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
