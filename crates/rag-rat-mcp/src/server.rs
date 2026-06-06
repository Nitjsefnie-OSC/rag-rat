use std::path::PathBuf;

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde_json::{Value, json};

use crate::tools::{
    EmptyArgs, ImpactArgs, LimitArgs, ReadChunkArgs, SearchArgs, SymbolArgs, SymbolGraphArgs,
};

#[derive(Clone)]
pub struct RagRatService {
    database: PathBuf,
}

impl RagRatService {
    pub fn new(database: PathBuf) -> Self {
        Self { database }
    }

    fn call(&self, name: &str, value: Value) -> Result<CallToolResult, ErrorData> {
        let value = crate::tools::call_tool(&self.database, name, value)
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
        description = "Search indexed source and docs with DuckDB BM25 lexical recall; validates stale hits."
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
        description = "Traverse reverse graph edges for callers when graph data exists."
    )]
    fn find_callers(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("find_callers", json!(args))
    }

    #[tool(
        name = "trace_callees",
        description = "Traverse forward graph edges for callees when graph data exists."
    )]
    fn trace_callees(
        &self,
        Parameters(args): Parameters<SymbolGraphArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.call("trace_callees", json!(args))
    }

    #[tool(
        name = "impact_surface",
        description = "Estimate affected source, test, generated binding, and docs surfaces."
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
        name = "index_status",
        description = "Report DuckDB index freshness, git metadata, parser failures, and file counts."
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
            .with_instructions("Read-only-source repo intelligence. Index and auto-heal writes are confined to the configured DuckDB database.")
    }
}

pub async fn run_stdio(database: PathBuf) -> anyhow::Result<()> {
    let service = RagRatService::new(database).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
