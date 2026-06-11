mod extract;
mod helpers;
mod resolve;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

pub(crate) use extract::*;
pub(crate) use helpers::*;
pub(crate) use resolve::*;
use rusqlite::{Connection, params};
use serde::Serialize;
use tree_sitter::Node;

use crate::index::parser::{self, ParserKind};
use crate::language::Language;

pub const MAX_GRAPH_PARSE_BYTES: usize = 512_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum EdgeKind {
    Imports,
    Exports,
    CallsName,
    Constructs,
    UsesMacro,
    ReferencesType,
    Implements,
    Contains,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Imports => "imports",
            Self::Exports => "exports",
            Self::CallsName => "calls_name",
            Self::Constructs => "constructs",
            Self::UsesMacro => "uses_macro",
            Self::ReferencesType => "references_type",
            Self::Implements => "implements",
            Self::Contains => "contains",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum EdgeConfidence {
    Exact,
    Syntactic,
    NameOnly,
    Ambiguous,
}

impl EdgeConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "Exact",
            Self::Syntactic => "Syntactic",
            Self::NameOnly => "NameOnly",
            Self::Ambiguous => "Ambiguous",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EdgeCandidate {
    from_symbol_id: Option<i64>,
    from_name: Option<String>,
    to_name: String,
    target_qualified_name: Option<String>,
    evidence: Option<String>,
    receiver_hint: Option<String>,
    source_span: EdgeSpan,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexedSymbol {
    id: i64,
    file_id: i64,
    language: String,
    name: String,
    qualified_name: String,
    kind: String,
    start_byte: usize,
    end_byte: usize,
    start_line: i64,
    end_line: i64,
}

impl IndexedSymbol {
    /// Build symbols for the parallel prepare phase, where real DB ids don't exist yet. `id` is the
    /// symbol's index in the prepared `symbols` vec; edge candidates produced from these carry that
    /// local index as `from_symbol_id`, which `insert_prepared_file` remaps to the real DB id once
    /// symbols are inserted. Sorted by (start_byte, end_byte) to match `symbols_for_file`'s ORDER
    /// BY so containing-symbol tie-breaking is identical to the inline path.
    pub(crate) fn local_from_prepared(
        language: Language,
        symbols: &[crate::index::symbols::Symbol],
    ) -> Vec<Self> {
        let mut out = symbols
            .iter()
            .enumerate()
            .map(|(idx, symbol)| IndexedSymbol {
                id: idx as i64,
                file_id: 0,
                language: language.as_str().to_string(),
                name: symbol.name.clone(),
                qualified_name: symbol.qualified_name.clone(),
                kind: symbol.kind.clone(),
                start_byte: symbol.start_byte,
                end_byte: symbol.end_byte,
                start_line: i64::try_from(symbol.start_line).unwrap_or(0),
                end_line: i64::try_from(symbol.end_line).unwrap_or(0),
            })
            .collect::<Vec<_>>();
        out.sort_by_key(|symbol| (symbol.start_byte, symbol.end_byte));
        out
    }
}

impl EdgeCandidate {
    /// Replace a local `from_symbol_id` (an index into the prepared symbols, as produced by
    /// [`IndexedSymbol::local_from_prepared`]) with the real DB id from `db_ids`, indexed by the
    /// prepared symbol's original position. A `None` from_symbol_id (file-level edge) is left
    /// alone.
    pub(crate) fn remap_from_symbol_id(&mut self, db_ids: &[i64]) {
        if let Some(local) = self.from_symbol_id {
            self.from_symbol_id =
                usize::try_from(local).ok().and_then(|index| db_ids.get(index)).copied();
        }
    }
}

impl IndexedSymbol {
    /// Build an `IndexedSymbol` for a symbol that was just inserted (so it carries its real DB id),
    /// for the full-rebuild in-memory edge resolution. Same shape `all_symbols` would return.
    pub(crate) fn from_inserted(
        id: i64,
        file_id: i64,
        language: Language,
        symbol: &crate::index::symbols::Symbol,
    ) -> Self {
        IndexedSymbol {
            id,
            file_id,
            language: language.as_str().to_string(),
            name: symbol.name.clone(),
            qualified_name: symbol.qualified_name.clone(),
            kind: symbol.kind.clone(),
            start_byte: symbol.start_byte,
            end_byte: symbol.end_byte,
            start_line: i64::try_from(symbol.start_line).unwrap_or(0),
            end_line: i64::try_from(symbol.end_line).unwrap_or(0),
        }
    }
}

/// Symbols (with real DB ids) and edge candidates (with their source file id) accumulated across
/// the full-rebuild insert loop, so edges can be resolved in memory and inserted once, fully
/// resolved — instead of inserting them unresolved per file and then resolving with a per-edge
/// UPDATE pass.
#[derive(Default)]
pub(crate) struct FullRebuildGraph {
    pub(crate) symbols: Vec<IndexedSymbol>,
    pub(crate) edges: Vec<(i64, EdgeCandidate)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EdgeSpan {
    start_line: i64,
    end_line: i64,
    start_byte: i64,
    end_byte: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EdgeContext {
    target_qualified_name: Option<String>,
    receiver_hint: Option<String>,
}

impl IndexedSymbol {
    fn span(&self) -> EdgeSpan {
        EdgeSpan {
            start_line: self.start_line,
            end_line: self.end_line,
            start_byte: i64::try_from(self.start_byte).unwrap_or(i64::MAX),
            end_byte: i64::try_from(self.end_byte).unwrap_or(i64::MAX),
        }
    }
}

/// Name-keyed indexes over the symbol set, built once per resolve pass. Edge resolution used to
/// scan the entire `Vec<IndexedSymbol>` (several times) per edge — O(edges × symbols), the single
/// biggest cost in a full rebuild. These maps make each lookup ~O(1). Bucket order mirrors the
/// input `symbols` order, so first-match semantics are preserved exactly.
pub(crate) struct SymbolIndex<'a> {
    /// Exact `qualified_name` match.
    by_qualified: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Short-name fallback (`symbol.name`).
    by_name: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Candidates for the `qualified_name.ends_with("::{q}")` suffix match, keyed by the last
    /// `::`-segment of the qualified name (a name ending in `::{q}` necessarily shares `q`'s
    /// tail).
    by_qn_tail: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Language of each source file (first symbol seen for the file).
    file_language: HashMap<i64, &'a str>,
}

impl<'a> SymbolIndex<'a> {
    fn build(symbols: &'a [IndexedSymbol]) -> Self {
        let mut by_qualified: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let mut by_name: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let mut by_qn_tail: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let mut file_language: HashMap<i64, &str> = HashMap::new();
        for symbol in symbols {
            by_qualified.entry(symbol.qualified_name.as_str()).or_default().push(symbol);
            by_name.entry(symbol.name.as_str()).or_default().push(symbol);
            by_qn_tail.entry(qn_tail(&symbol.qualified_name)).or_default().push(symbol);
            file_language.entry(symbol.file_id).or_insert(symbol.language.as_str());
        }
        Self { by_qualified, by_name, by_qn_tail, file_language }
    }
}

pub(crate) struct ResolveSymbolRequest<'a> {
    name: &'a str,
    target_qualified_name: Option<&'a str>,
    edge_kind: &'a str,
    evidence: Option<&'a str>,
    receiver_hint: Option<&'a str>,
    source_file_id: i64,
    source_language: Option<&'a str>,
}
