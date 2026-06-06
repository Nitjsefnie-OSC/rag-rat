//! Graph edge enrichment hooks.
//!
//! The first indexer writes the schema and symbol table that MCP graph tools use.
//! Precise language call graphs can be added here later using Tree-sitter,
//! rust-analyzer, rustdoc JSON, or TypeScript/Kotlin compiler metadata.

#[derive(Debug, Clone, Copy)]
pub enum EdgeKind {
    Defines,
    Imports,
    Calls,
    Exports,
    FfiExposes,
    TestCovers,
    DocDescribes,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Defines => "defines",
            Self::Imports => "imports",
            Self::Calls => "calls",
            Self::Exports => "exports",
            Self::FfiExposes => "ffi_exposes",
            Self::TestCovers => "test_covers",
            Self::DocDescribes => "doc_describes",
        }
    }
}
