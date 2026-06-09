use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use rusqlite::{Connection, params};
use serde::Serialize;
use tree_sitter::Node;

use crate::{
    index::parser::{self, ParserKind},
    language::Language,
};

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
struct EdgeCandidate {
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
struct IndexedSymbol {
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

#[derive(Debug, Clone, Copy)]
struct EdgeSpan {
    start_line: i64,
    end_line: i64,
    start_byte: i64,
    end_byte: i64,
}

#[derive(Debug, Clone, Default)]
struct EdgeContext {
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

pub fn index_file_edges(
    conn: &Connection,
    file_id: i64,
    path: &Path,
    language: Language,
    text: &str,
) -> anyhow::Result<()> {
    if language == Language::Markdown {
        return Ok(());
    }
    let symbols = symbols_for_file(conn, file_id)?;
    let mut candidates = contains_edges(&symbols);
    candidates.extend(syntactic_edges(path, language, text, &symbols)?);
    insert_candidates(conn, file_id, candidates)
}

/// Name-keyed indexes over the symbol set, built once per resolve pass. Edge resolution used to
/// scan the entire `Vec<IndexedSymbol>` (several times) per edge — O(edges × symbols), the single
/// biggest cost in a full rebuild. These maps make each lookup ~O(1). Bucket order mirrors the
/// input `symbols` order, so first-match semantics are preserved exactly.
struct SymbolIndex<'a> {
    /// Exact `qualified_name` match.
    by_qualified: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Short-name fallback (`symbol.name`).
    by_name: HashMap<&'a str, Vec<&'a IndexedSymbol>>,
    /// Candidates for the `qualified_name.ends_with("::{q}")` suffix match, keyed by the last
    /// `::`-segment of the qualified name (a name ending in `::{q}` necessarily shares `q`'s tail).
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

/// The last `::`-separated segment of a qualified name.
fn qn_tail(qualified_name: &str) -> &str {
    qualified_name.rsplit("::").next().unwrap_or(qualified_name)
}

pub fn resolve_all_edges(conn: &Connection) -> anyhow::Result<()> {
    let symbols = all_symbols(conn)?;
    let index = SymbolIndex::build(&symbols);
    let mut stmt = conn.prepare(
        "SELECT id, source_file_id, to_name, target_qualified_name, edge_kind, confidence, evidence, receiver_hint FROM edges ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let rows = rows.collect::<Result<Vec<_>, _>>()?;
    for (
        edge_id,
        source_file_id,
        to_name,
        target_qualified_name,
        edge_kind,
        current_confidence,
        evidence,
        receiver_hint,
    ) in rows
    {
        let resolution = resolve_symbol(
            ResolveSymbolRequest {
                name: &to_name,
                target_qualified_name: target_qualified_name.as_deref(),
                edge_kind: &edge_kind,
                evidence: evidence.as_deref(),
                receiver_hint: receiver_hint.as_deref(),
                source_file_id,
                source_language: index.file_language.get(&source_file_id).copied(),
            },
            &index,
        );
        let Some((to_symbol_id, confidence, reason)) = resolution else {
            let confidence = if current_confidence == EdgeConfidence::Ambiguous.as_str() {
                EdgeConfidence::Ambiguous
            } else {
                EdgeConfidence::NameOnly
            };
            conn.execute(
                "UPDATE edges
                 SET to_symbol_id = NULL,
                     target_start_line = NULL,
                     target_end_line = NULL,
                     confidence = ?2,
                     resolution = 'unresolved'
                 WHERE id = ?1",
                params![edge_id, confidence.as_str()],
            )?;
            continue;
        };
        conn.execute(
            "UPDATE edges
             SET to_symbol_id = ?2,
                 confidence = ?3,
                 target_start_line = ?4,
                 target_end_line = ?5,
                 resolution = ?6
             WHERE id = ?1",
            params![
                edge_id,
                to_symbol_id.id,
                confidence.as_str(),
                to_symbol_id.start_line,
                to_symbol_id.end_line,
                reason,
            ],
        )?;
    }
    Ok(())
}

struct ResolveSymbolRequest<'a> {
    name: &'a str,
    target_qualified_name: Option<&'a str>,
    edge_kind: &'a str,
    evidence: Option<&'a str>,
    receiver_hint: Option<&'a str>,
    source_file_id: i64,
    source_language: Option<&'a str>,
}

fn resolve_symbol<'a>(
    request: ResolveSymbolRequest<'_>,
    index: &SymbolIndex<'a>,
) -> Option<(&'a IndexedSymbol, EdgeConfidence, &'static str)> {
    let kind_matches = |symbol: &IndexedSymbol| {
        request.edge_kind != EdgeKind::UsesMacro.as_str() || symbol.kind == "macro"
    };
    if let Some(qualified) = request.target_qualified_name.filter(|value| !value.is_empty()) {
        // Exact qualified-name match (bucket entries already share `qualified_name == qualified`).
        if let Some(symbol) = index
            .by_qualified
            .get(qualified)
            .into_iter()
            .flatten()
            .copied()
            .find(|symbol| kind_matches(symbol))
        {
            return Some((symbol, EdgeConfidence::Exact, "exact"));
        }
        let suffix = format!("::{qualified}");
        let matches = index
            .by_qn_tail
            .get(qn_tail(qualified))
            .into_iter()
            .flatten()
            .copied()
            .filter(|symbol| kind_matches(symbol) && symbol.qualified_name.ends_with(&suffix))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [symbol] => return Some((*symbol, EdgeConfidence::Syntactic, "qualified_suffix")),
            [_, ..] if same_logical_symbol(&matches) => {
                return Some((matches[0], EdgeConfidence::Syntactic, "logical_variant"));
            },
            [_, ..] => return None,
            [] => {},
        }
        if !allow_unqualified_fallback(
            request.edge_kind,
            qualified,
            request.name,
            request.evidence,
            request.receiver_hint,
            request.source_language,
        ) {
            return None;
        }
    }
    let short = short_name(request.name);
    let matches = index
        .by_name
        .get(short)
        .into_iter()
        .flatten()
        .copied()
        .filter(|symbol| kind_matches(symbol))
        .collect::<Vec<_>>();
    let preferred = preferred_matches(request.edge_kind, &matches);
    let matches = if preferred.is_empty() { matches.as_slice() } else { preferred.as_slice() };
    match matches {
        [symbol] => Some((*symbol, EdgeConfidence::Syntactic, "target_name_fallback")),
        [_, ..] => {
            if same_logical_symbol(matches) {
                return Some((matches[0], EdgeConfidence::Syntactic, "logical_variant"));
            }
            let same_file = matches
                .iter()
                .copied()
                .filter(|symbol| symbol.file_id == request.source_file_id)
                .collect::<Vec<_>>();
            match same_file.as_slice() {
                [symbol] => Some((*symbol, EdgeConfidence::Syntactic, "same_file_name")),
                [_, ..] if same_logical_symbol(&same_file) => {
                    Some((same_file[0], EdgeConfidence::Syntactic, "logical_variant"))
                },
                _ => None,
            }
        },
        [] => None,
    }
}

fn same_logical_symbol(symbols: &[&IndexedSymbol]) -> bool {
    let Some(first) = symbols.first() else {
        return false;
    };
    symbols.iter().all(|symbol| {
        symbol.qualified_name == first.qualified_name
            && symbol.name == first.name
            && symbol.kind == first.kind
    })
}

fn allow_unqualified_fallback(
    edge_kind: &str,
    qualified: &str,
    name: &str,
    evidence: Option<&str>,
    receiver_hint: Option<&str>,
    source_language: Option<&str>,
) -> bool {
    if edge_kind == EdgeKind::UsesMacro.as_str() {
        return false;
    }
    let target = short_name(name);
    let qualifier = qualified
        .rsplit_once("::")
        .map(|(qualifier, _)| qualifier)
        .unwrap_or(qualified)
        .split("::")
        .next()
        .unwrap_or_default();
    if matches!(qualifier, "crate" | "self" | "super") {
        return true;
    }
    if receiver_hint
        .is_some_and(|receiver| looks_like_type_name(receiver) && !is_common_member_name(target))
        && matches!(source_language, Some("rust" | "kotlin"))
    {
        return true;
    }
    if receiver_hint.is_some_and(|receiver| !matches!(receiver, "self" | "Self"))
        && evidence.is_some_and(|value| value.contains('.'))
    {
        return source_language == Some(Language::Kotlin.as_str())
            && !is_common_member_name(target);
    }
    if is_external_rust_root(qualifier) {
        return false;
    }
    if looks_like_type_name(qualifier) && is_common_member_name(target) {
        return false;
    }
    true
}

fn is_external_rust_root(value: &str) -> bool {
    matches!(
        value,
        "std"
            | "core"
            | "alloc"
            | "tokio"
            | "serde"
            | "serde_json"
            | "anyhow"
            | "thiserror"
            | "rusqlite"
            | "tree_sitter"
            | "tracing"
            | "log"
            | "Vec"
            | "String"
            | "Option"
            | "Result"
            | "HashMap"
            | "BTreeMap"
            | "HashSet"
            | "BTreeSet"
    )
}

fn is_common_member_name(value: &str) -> bool {
    matches!(
        value,
        "new"
            | "default"
            | "clone"
            | "to_string"
            | "into"
            | "from"
            | "as_ref"
            | "as_mut"
            | "iter"
            | "map"
            | "collect"
            | "build"
            | "unwrap"
            | "expect"
            | "ok"
            | "err"
    )
}

fn preferred_matches<'a>(edge_kind: &str, matches: &[&'a IndexedSymbol]) -> Vec<&'a IndexedSymbol> {
    let preferred_kinds: &[&str] = match edge_kind {
        "calls_name" => &["function", "method"],
        "constructs" => &["struct", "class", "object"],
        "uses_macro" => &["macro"],
        "implements" => &["trait", "interface"],
        "references_type" => &["struct", "enum", "trait", "type", "class", "interface", "object"],
        _ => &[],
    };
    if preferred_kinds.is_empty() {
        return Vec::new();
    }
    matches
        .iter()
        .copied()
        .filter(|symbol| preferred_kinds.contains(&symbol.kind.as_str()))
        .collect()
}

fn contains_edges(symbols: &[IndexedSymbol]) -> Vec<EdgeCandidate> {
    let mut out = Vec::new();
    for child in symbols {
        let parent = symbols
            .iter()
            .filter(|candidate| {
                candidate.id != child.id
                    && candidate.start_byte <= child.start_byte
                    && candidate.end_byte >= child.end_byte
            })
            .min_by_key(|candidate| candidate.end_byte.saturating_sub(candidate.start_byte));
        if let Some(parent) = parent {
            out.push(EdgeCandidate {
                from_symbol_id: Some(parent.id),
                from_name: Some(parent.qualified_name.clone()),
                to_name: child.qualified_name.clone(),
                target_qualified_name: Some(child.qualified_name.clone()),
                evidence: Some(child.qualified_name.clone()),
                receiver_hint: None,
                source_span: child.span(),
                edge_kind: EdgeKind::Contains,
                confidence: EdgeConfidence::Exact,
            });
        }
    }
    out
}

fn syntactic_edges(
    path: &Path,
    language: Language,
    text: &str,
    symbols: &[IndexedSymbol],
) -> anyhow::Result<Vec<EdgeCandidate>> {
    let grammar = match parser::parser_kind(path, language) {
        ParserKind::Rust => tree_sitter_rust::LANGUAGE.into(),
        ParserKind::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ParserKind::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        ParserKind::Kotlin => tree_sitter_kotlin::LANGUAGE.into(),
        ParserKind::C => tree_sitter_c::LANGUAGE.into(),
        ParserKind::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        ParserKind::Markdown => return Ok(Vec::new()),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let Some(tree) = parser.parse(text, None) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    collect_edges(language, text, tree.root_node(), symbols, path, &mut out);
    Ok(out)
}

fn collect_edges(
    language: Language,
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    if node.is_error() || node.is_missing() {
        return;
    }
    match language {
        Language::Rust => rust_edges(text, node, symbols, path, out),
        Language::TypeScript => typescript_edges(text, node, symbols, path, out),
        Language::Kotlin => kotlin_edges(text, node, symbols, path, out),
        Language::C | Language::Cpp => c_like_edges(text, node, symbols, path, out),
        Language::Markdown => {},
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_edges(language, text, child, symbols, path, out);
    }
}

fn rust_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "use_declaration" => {
            let names = identifiers_under(node, text);
            let is_reexport = node_text(node, text).trim_start().starts_with("pub use ");
            for name in names {
                if !is_rust_path_keyword(&name) {
                    out.push(file_edge(
                        path,
                        node,
                        text,
                        name,
                        EdgeKind::Imports,
                        EdgeConfidence::NameOnly,
                    ));
                }
            }
            if is_reexport {
                for name in identifiers_under(node, text) {
                    if !is_rust_path_keyword(&name) {
                        out.push(file_edge(
                            path,
                            node,
                            text,
                            name,
                            EdgeKind::Exports,
                            EdgeConfidence::NameOnly,
                        ));
                    }
                }
            }
        },
        "mod_item" => {
            if let Some(name) = child_name_text(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" => {
            if let Some(name) = call_target_name(node, text) {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: target_qualified_name(node, text),
                        receiver_hint: scoped_receiver_name(node, text),
                    },
                ));
            }
            // A scoped call receiver is a type reference only when it names a type. By Rust
            // convention types are PascalCase, while module paths (`std::env::…`) and method
            // receivers on locals (`p.as_os_str()`) are snake_case — emitting those as
            // `references_type` produced bogus "types" like `std` and `p`. Gate on an
            // uppercase-leading receiver so `Foo::bar()` still records a type reference.
            if let Some(receiver) = scoped_receiver_name(node, text)
                && receiver.chars().next().is_some_and(char::is_uppercase)
            {
                out.push(symbol_edge(
                    symbols,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "macro_invocation" => {
            if let Some(name) = first_identifier_text(node, text) {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::UsesMacro,
                    EdgeConfidence::NameOnly,
                    EdgeContext::default(),
                ));
            }
        },
        "impl_item" => rust_impl_edges(text, node, symbols, out),
        "type_identifier" | "scoped_type_identifier" | "generic_type" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        _ => {},
    }
}

fn rust_impl_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
) {
    let node_text = node_text(node, text);
    let header = node_text.split('{').next().unwrap_or_default();
    let type_names = header
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|part| !part.is_empty())
        .filter(|part| !matches!(*part, "impl" | "for" | "where"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if node_text.contains(" for ") && type_names.len() >= 2 {
        let trait_name = type_names.first().cloned().unwrap_or_default();
        let type_name = type_names.last().cloned().unwrap_or_default();
        out.push(EdgeCandidate {
            from_symbol_id: containing_symbol(symbols, node.start_byte()).map(|symbol| symbol.id),
            from_name: Some(type_name),
            to_name: trait_name,
            target_qualified_name: None,
            evidence: Some(edge_evidence(node, text)),
            receiver_hint: None,
            source_span: span_for_node(node),
            edge_kind: EdgeKind::Implements,
            confidence: EdgeConfidence::NameOnly,
        });
    } else if let Some(type_name) = type_names.first() {
        out.push(symbol_edge(
            symbols,
            node,
            type_name.clone(),
            EdgeKind::ReferencesType,
            EdgeConfidence::NameOnly,
        ));
    }
}

fn typescript_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "import_statement" => {
            for name in identifiers_under(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "export_statement" => {
            for name in identifiers_under(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Exports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" | "new_expression" => {
            let identifiers =
                identifiers_under(node.child_by_field_name("function").unwrap_or(node), text);
            if let Some(name) = identifiers.last().cloned().or_else(|| call_target_name(node, text))
            {
                let edge_kind = if node.kind() == "new_expression" {
                    EdgeKind::Constructs
                } else {
                    EdgeKind::CallsName
                };
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    edge_kind,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: dotted_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                ));
            }
            if let Some(receiver) = identifiers.first().filter(|_| identifiers.len() > 1).cloned() {
                out.push(symbol_edge(
                    symbols,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name) = first_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "type_identifier" => {
            if let Some(name) = node.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        _ => {},
    }
}

fn kotlin_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "import" | "import_header" | "import_directive" => {
            for name in identifiers_under(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" => {
            let identifiers = identifiers_under(node, text);
            if let Some(name) =
                identifiers.last().cloned().or_else(|| first_identifier_text(node, text))
            {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: dotted_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                ));
            }
            if let Some(receiver) = identifiers.first().filter(|_| identifiers.len() > 1).cloned() {
                out.push(symbol_edge(
                    symbols,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
            if let Some(constructor) =
                identifiers.first().filter(|name| looks_like_type_name(name)).cloned()
            {
                out.push(symbol_edge(
                    symbols,
                    node,
                    constructor.clone(),
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    constructor,
                    EdgeKind::Constructs,
                    EdgeConfidence::NameOnly,
                    EdgeContext::default(),
                ));
            }
        },
        "user_type" | "type_identifier" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "delegation_specifier" | "supertype" | "super_type" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::Implements,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        _ => {},
    }
}

fn c_like_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "preproc_include" => {
            let include = node_text(node, text)
                .trim()
                .trim_start_matches("#include")
                .trim()
                .trim_matches(['<', '>', '"'])
                .to_string();
            if !include.is_empty() {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    include,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" => {
            let identifiers =
                identifiers_under(node.child_by_field_name("function").unwrap_or(node), text);
            if let Some(name) = identifiers.last().cloned().or_else(|| call_target_name(node, text))
            {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: c_like_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                ));
            }
        },
        "type_identifier" | "qualified_identifier" | "namespace_identifier" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        _ => {},
    }
}

fn file_edge(
    path: &Path,
    node: Node<'_>,
    text: &str,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
) -> EdgeCandidate {
    EdgeCandidate {
        from_symbol_id: None,
        from_name: Some(path.to_string_lossy().replace('\\', "/")),
        to_name,
        target_qualified_name: None,
        evidence: Some(edge_evidence(node, text)),
        receiver_hint: None,
        source_span: span_for_node(node),
        edge_kind,
        confidence,
    }
}

fn symbol_edge(
    symbols: &[IndexedSymbol],
    node: Node<'_>,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
) -> EdgeCandidate {
    symbol_edge_with_context(
        symbols,
        node,
        "",
        to_name,
        edge_kind,
        confidence,
        EdgeContext::default(),
    )
}

fn symbol_edge_with_context(
    symbols: &[IndexedSymbol],
    node: Node<'_>,
    text: &str,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
    context: EdgeContext,
) -> EdgeCandidate {
    let byte = node.start_byte();
    let source = containing_symbol(symbols, byte);
    EdgeCandidate {
        from_symbol_id: source.map(|symbol| symbol.id),
        from_name: source.map(|symbol| symbol.qualified_name.clone()),
        to_name,
        target_qualified_name: context.target_qualified_name,
        evidence: (!text.is_empty()).then(|| edge_evidence(node, text)),
        receiver_hint: context.receiver_hint,
        source_span: span_for_node(node),
        edge_kind,
        confidence,
    }
}

fn target_qualified_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = node_text(function, text);
    (value.contains("::") || value.contains('.')).then(|| value.replace('.', "::"))
}

fn dotted_qualified_name(identifiers: &[String]) -> Option<String> {
    (identifiers.len() > 1).then(|| identifiers.join("::"))
}

fn c_like_qualified_name(identifiers: &[String]) -> Option<String> {
    (identifiers.len() > 1).then(|| identifiers.join("::"))
}

fn containing_symbol(symbols: &[IndexedSymbol], byte: usize) -> Option<&IndexedSymbol> {
    let mut matches = symbols
        .iter()
        .filter(|symbol| symbol.start_byte <= byte && symbol.end_byte >= byte)
        .collect::<Vec<_>>();
    matches.sort_by_key(|symbol| symbol.end_byte.saturating_sub(symbol.start_byte));
    let first = matches.first().copied()?;
    if matches!(first.kind.as_str(), "const" | "property" | "static") {
        matches
            .iter()
            .copied()
            .find(|symbol| {
                symbol.id != first.id
                    && !matches!(symbol.kind.as_str(), "const" | "property" | "static")
            })
            .or(Some(first))
    } else {
        Some(first)
    }
}

fn call_target_name(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("function")
        .and_then(|child| last_identifier_text(child, text))
        .map(|name| short_name(&name).to_string())
        .or_else(|| first_identifier_text(node, text))
}

fn scoped_receiver_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = node_text(function, text);
    let separator = if value.contains("::") {
        "::"
    } else if value.contains('.') {
        "."
    } else {
        return None;
    };
    value
        .split(separator)
        .next()
        .map(|name| short_name(name.trim()).to_string())
        .filter(|name| !name.is_empty())
}

fn child_name_text(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| child.utf8_text(text.as_bytes()).ok())
        .map(ToOwned::to_owned)
}

fn first_identifier_text(node: Node<'_>, text: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if is_identifier_kind(child.kind()) {
            return child.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned);
        }
        if let Some(value) = first_identifier_text(child, text) {
            return Some(value);
        }
    }
    None
}

fn last_identifier_text(node: Node<'_>, text: &str) -> Option<String> {
    identifiers_under(node, text).into_iter().last()
}

fn identifiers_under(node: Node<'_>, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_identifiers(node, text, &mut out);
    out
}

fn collect_identifiers(node: Node<'_>, text: &str, out: &mut Vec<String>) {
    if is_identifier_kind(node.kind()) {
        if let Ok(value) = node.utf8_text(text.as_bytes())
            && !value.is_empty()
        {
            out.push(value.to_string());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifiers(child, text, out);
    }
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "simple_identifier"
            | "package_identifier"
            | "namespace_identifier"
    )
}

fn is_rust_path_keyword(value: &str) -> bool {
    matches!(value, "self" | "super" | "crate")
}

fn looks_like_type_name(value: &str) -> bool {
    value.chars().next().is_some_and(char::is_uppercase)
}

fn node_text(node: Node<'_>, text: &str) -> String {
    node.utf8_text(text.as_bytes()).unwrap_or_default().to_string()
}

fn edge_evidence(node: Node<'_>, text: &str) -> String {
    node_text(node, text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn short_name(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(name)
}

fn symbols_for_file(conn: &Connection, file_id: i64) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, symbols.qualified_name, symbols.kind,
               symbols.start_byte, symbols.end_byte,
               COALESCE((
                 SELECT chunks.start_byte
                 FROM chunks
                 WHERE chunks.file_id = symbols.file_id
                   AND symbols.start_byte >= chunks.start_byte
                   AND symbols.start_byte < chunks.end_byte
                 ORDER BY chunks.end_byte - chunks.start_byte ASC
                 LIMIT 1
               ), symbols.start_byte) AS chunk_start_byte,
               COALESCE((
                 SELECT chunks.start_line
                 FROM chunks
                 WHERE chunks.file_id = symbols.file_id
                   AND symbols.start_byte >= chunks.start_byte
                   AND symbols.start_byte < chunks.end_byte
                 ORDER BY chunks.end_byte - chunks.start_byte ASC
                 LIMIT 1
               ), 1) AS chunk_start_line,
               COALESCE((
                 SELECT chunks.text
                 FROM chunks
                 WHERE chunks.file_id = symbols.file_id
                   AND symbols.start_byte >= chunks.start_byte
                   AND symbols.start_byte < chunks.end_byte
                 ORDER BY chunks.end_byte - chunks.start_byte ASC
                 LIMIT 1
               ), '') AS chunk_text
        FROM symbols
        WHERE file_id = ?1
        ORDER BY symbols.start_byte, symbols.end_byte
        ",
    )?;
    let rows = stmt.query_map([file_id], symbol_row)?;
    collect_rows(rows)
}

fn all_symbols(conn: &Connection) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, symbols.qualified_name, symbols.kind,
               symbols.start_byte, symbols.end_byte,
               COALESCE((
                 SELECT chunks.start_byte
                 FROM chunks
                 WHERE chunks.file_id = symbols.file_id
                   AND symbols.start_byte >= chunks.start_byte
                   AND symbols.start_byte < chunks.end_byte
                 ORDER BY chunks.end_byte - chunks.start_byte ASC
                 LIMIT 1
               ), symbols.start_byte) AS chunk_start_byte,
               COALESCE((
                 SELECT chunks.start_line
                 FROM chunks
                 WHERE chunks.file_id = symbols.file_id
                   AND symbols.start_byte >= chunks.start_byte
                   AND symbols.start_byte < chunks.end_byte
                 ORDER BY chunks.end_byte - chunks.start_byte ASC
                 LIMIT 1
               ), 1) AS chunk_start_line,
               COALESCE((
                 SELECT chunks.text
                 FROM chunks
                 WHERE chunks.file_id = symbols.file_id
                   AND symbols.start_byte >= chunks.start_byte
                   AND symbols.start_byte < chunks.end_byte
                 ORDER BY chunks.end_byte - chunks.start_byte ASC
                 LIMIT 1
               ), '') AS chunk_text
        FROM symbols
        ORDER BY symbols.qualified_name
        ",
    )?;
    let rows = stmt.query_map([], symbol_row)?;
    collect_rows(rows)
}

fn symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSymbol> {
    let start_byte = usize::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
    let end_byte = usize::try_from(row.get::<_, i64>(7)?).unwrap_or(0);
    let chunk_start_byte = usize::try_from(row.get::<_, i64>(8)?).unwrap_or(start_byte);
    let chunk_start_line = row.get::<_, i64>(9)?;
    let chunk_text: String = row.get(10)?;
    let start_line = line_for_byte(&chunk_text, chunk_start_byte, chunk_start_line, start_byte);
    let end_line = line_for_byte(&chunk_text, chunk_start_byte, chunk_start_line, end_byte);
    Ok(IndexedSymbol {
        id: row.get(0)?,
        file_id: row.get(1)?,
        language: row.get(2)?,
        name: row.get(3)?,
        qualified_name: row.get(4)?,
        kind: row.get(5)?,
        start_byte,
        end_byte,
        start_line,
        end_line,
    })
}

fn insert_candidates(
    conn: &Connection,
    file_id: i64,
    candidates: Vec<EdgeCandidate>,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        let to_name = candidate.to_name.trim();
        if to_name.is_empty() {
            continue;
        }
        if candidate.from_name.as_deref() == Some(to_name) {
            continue;
        }
        let key = (
            candidate.from_symbol_id,
            candidate.from_name.clone(),
            to_name.to_string(),
            candidate.edge_kind,
            candidate.source_span.start_byte,
            candidate.source_span.end_byte,
        );
        if !seen.insert(key) {
            continue;
        }
        conn.execute(
            "
            INSERT INTO edges(
                source_file_id, from_symbol_id, from_name, to_name,
                target_qualified_name, evidence, receiver_hint,
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                edge_kind, confidence
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ",
            params![
                file_id,
                candidate.from_symbol_id,
                candidate.from_name,
                to_name,
                candidate.target_qualified_name,
                candidate.evidence,
                candidate.receiver_hint,
                candidate.source_span.start_line,
                candidate.source_span.end_line,
                candidate.source_span.start_byte,
                candidate.source_span.end_byte,
                candidate.edge_kind.as_str(),
                candidate.confidence.as_str(),
            ],
        )?;
    }
    Ok(())
}

fn span_for_node(node: Node<'_>) -> EdgeSpan {
    EdgeSpan {
        start_line: i64::try_from(node.start_position().row).unwrap_or(i64::MAX).saturating_add(1),
        end_line: i64::try_from(node.end_position().row).unwrap_or(i64::MAX).saturating_add(1),
        start_byte: i64::try_from(node.start_byte()).unwrap_or(i64::MAX),
        end_byte: i64::try_from(node.end_byte()).unwrap_or(i64::MAX),
    }
}

fn line_for_byte(
    chunk_text: &str,
    chunk_start_byte: usize,
    chunk_start_line: i64,
    absolute_byte: usize,
) -> i64 {
    let relative_byte = absolute_byte.saturating_sub(chunk_start_byte).min(chunk_text.len());
    chunk_start_line
        + i64::try_from(
            chunk_text.as_bytes()[..relative_byte].iter().filter(|ch| **ch == b'\n').count(),
        )
        .unwrap_or(0)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
