use std::path::Path;

use crate::{
    index::parser::{self, ParserKind},
    language::Language,
};

#[test]
fn extracts_rust_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/lib.rs");
    let symbols = parser::parse_symbols(Path::new("src/lib.rs"), Language::Rust, text).unwrap();
    assert_symbol(&symbols, "function", "open_database");
    assert_symbol(&symbols, "const", "MAX_OPEN_DATABASES");
    assert_symbol(&symbols, "static", "DEFAULT_DATABASE_NAME");
    assert_symbol(&symbols, "type", "DatabaseId");
    assert_symbol(&symbols, "macro", "database_event");
    assert_symbol(&symbols, "module", "handles");
    assert_symbol(&symbols, "struct", "DatabaseHandle");
    assert_symbol(&symbols, "impl", "DatabaseHandle");
    assert_symbol(&symbols, "function", "id");
    assert_symbol(&symbols, "enum", "DatabaseState");
    assert_symbol(&symbols, "trait", "DatabaseLifecycle");
}

#[test]
fn extracts_rust_uniffi_export_symbol_facts() {
    let text = r#"
#[uniffi::export]
pub fn exported_fn() {}

#[cfg_attr(not(target_arch = "wasm32"), uniffi::export(async_runtime = "tokio"))]
impl Runtime {
    pub fn route_search_query(&self) {}
}

pub struct Runtime;

/// Not #[uniffi::export]: this is an internal helper.
pub fn internal_helper() {}
"#;
    let symbols = parser::parse_symbols(Path::new("src/lib.rs"), Language::Rust, text).unwrap();
    assert_symbol_fact(&symbols, "function", "exported_fn", "rust_attr", "uniffi_export");
    assert_symbol_fact(&symbols, "impl", "Runtime", "rust_attr", "uniffi_export");
    assert_no_symbol_fact(&symbols, "function", "internal_helper", "rust_attr", "uniffi_export");
}

#[test]
fn extracts_typescript_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/index.ts");
    let symbols =
        parser::parse_symbols(Path::new("src/index.ts"), Language::TypeScript, text).unwrap();
    assert_eq!(
        parser::parser_kind(Path::new("src/index.ts"), Language::TypeScript),
        ParserKind::TypeScript
    );
    assert_symbol(&symbols, "function", "openDatabase");
    assert_symbol(&symbols, "type", "BridgeState");
    assert_symbol(&symbols, "interface", "BridgeConfig");
    assert_symbol(&symbols, "class", "BridgeClient");
    assert_symbol(&symbols, "function", "open");
    assert_symbol(&symbols, "const", "bridgeName");
    assert_symbol(&symbols, "const", "useBridge");
    assert_symbol(&symbols, "const", "BridgeBadge");
}

#[test]
fn extracts_tsx_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/App.tsx");
    let symbols =
        parser::parse_symbols(Path::new("src/App.tsx"), Language::TypeScript, text).unwrap();
    assert_eq!(
        parser::parser_kind(Path::new("src/App.tsx"), Language::TypeScript),
        ParserKind::Tsx
    );
    assert_symbol(&symbols, "function", "HeldStatusCard");
    assert_symbol(&symbols, "const", "useHeldStatus");
}

#[test]
fn extracts_kotlin_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/Main.kt");
    let symbols = parser::parse_symbols(Path::new("src/Main.kt"), Language::Kotlin, text).unwrap();
    assert_symbol(&symbols, "class", "MainBridge");
    assert_symbol(&symbols, "property", "bridgeName");
    assert_symbol(&symbols, "function", "openDatabase");
    assert_symbol(&symbols, "function", "syncOnce");
    assert_symbol(&symbols, "object", "companion");
    assert_symbol(&symbols, "property", "DEFAULT_NAME");
    assert_symbol(&symbols, "function", "create");
    assert_symbol(&symbols, "object", "BridgeRegistry");
    assert_symbol(&symbols, "property", "active");
}

#[test]
fn extracts_kotlin_kdoc_without_closing_delimiter_residue() {
    let text = r#"
/**
 * Builds a proposal.
 */
class WatchProposalBuilder {
    /**
     * Builds the current proposal.
     */
    suspend fun build() {}
}
"#;
    let symbols = parser::parse_symbols(Path::new("src/Main.kt"), Language::Kotlin, text).unwrap();
    let class_docs =
        symbols.iter().find(|symbol| symbol.name == "WatchProposalBuilder").unwrap().docs.as_ref();
    assert_eq!(class_docs.map(String::as_str), Some("Builds a proposal."));
    let function_docs = symbols.iter().find(|symbol| symbol.name == "build").unwrap().docs.as_ref();
    assert_eq!(function_docs.map(String::as_str), Some("Builds the current proposal."));
}

#[test]
fn markdown_uses_no_tree_sitter_symbols() {
    assert_eq!(
        parser::parser_kind(Path::new("docs/search.md"), Language::Markdown),
        ParserKind::Markdown
    );
    let symbols =
        parser::parse_symbols(Path::new("docs/search.md"), Language::Markdown, "# Search").unwrap();
    assert!(symbols.is_empty());
}

fn assert_symbol(symbols: &[parser::ParsedSymbol], kind: &str, name: &str) {
    assert!(
        symbols.iter().any(|symbol| symbol.kind == kind && symbol.name == name),
        "missing {kind} {name}; got {:?}",
        symbols.iter().map(|symbol| (&symbol.kind, &symbol.name)).collect::<Vec<_>>()
    );
}

fn assert_symbol_fact(
    symbols: &[parser::ParsedSymbol],
    kind: &str,
    name: &str,
    fact_kind: &str,
    fact_value: &str,
) {
    let symbol = symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind} {name}: {symbols:?}"));
    assert!(
        symbol.facts.iter().any(|fact| fact.kind == fact_kind && fact.value == fact_value),
        "missing fact {fact_kind}={fact_value} on {kind} {name}; got {:?}",
        symbol.facts
    );
}

fn assert_no_symbol_fact(
    symbols: &[parser::ParsedSymbol],
    kind: &str,
    name: &str,
    fact_kind: &str,
    fact_value: &str,
) {
    let symbol = symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind} {name}: {symbols:?}"));
    assert!(
        !symbol.facts.iter().any(|fact| fact.kind == fact_kind && fact.value == fact_value),
        "unexpected fact {fact_kind}={fact_value} on {kind} {name}; got {:?}",
        symbol.facts
    );
}
