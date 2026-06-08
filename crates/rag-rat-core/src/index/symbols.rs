use std::path::Path;

use crate::{index::parser, language::Language};

#[derive(Debug, Clone)]
pub struct SymbolFact {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub signature: Option<String>,
    pub docs: Option<String>,
    pub facts: Vec<SymbolFact>,
}

pub fn symbols_for_file(path: &Path, language: Language, text: &str) -> Vec<Symbol> {
    parser::parse_symbols(path, language, text)
        .unwrap_or_default()
        .into_iter()
        .map(|symbol| Symbol {
            name: symbol.name,
            qualified_name: symbol.qualified_name,
            kind: symbol.kind,
            start_byte: symbol.start_byte,
            end_byte: symbol.end_byte,
            signature: symbol.signature,
            docs: symbol.docs,
            facts: symbol
                .facts
                .into_iter()
                .map(|fact| SymbolFact { kind: fact.kind, value: fact.value })
                .collect(),
        })
        .collect()
}
