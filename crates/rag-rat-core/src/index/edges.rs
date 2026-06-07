use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use rusqlite::{Connection, params};
use serde::Serialize;
use tree_sitter::Node;

use crate::{
    index::parser::{self, ParserKind},
    language::Language,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum EdgeKind {
    Imports,
    Exports,
    CallsName,
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
    source_span: EdgeSpan,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
}

#[derive(Debug, Clone)]
struct IndexedSymbol {
    id: i64,
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
    if symbols.is_empty() {
        return Ok(());
    }
    let mut candidates = contains_edges(&symbols);
    candidates.extend(syntactic_edges(path, language, text, &symbols)?);
    insert_candidates(conn, file_id, candidates)
}

pub fn resolve_all_edges(conn: &Connection) -> anyhow::Result<()> {
    let symbols = all_symbols(conn)?;
    let mut by_name: BTreeMap<String, Vec<&IndexedSymbol>> = BTreeMap::new();
    let mut by_qualified: BTreeMap<String, &IndexedSymbol> = BTreeMap::new();
    for symbol in &symbols {
        by_name.entry(symbol.name.clone()).or_default().push(symbol);
        by_qualified.insert(symbol.qualified_name.clone(), symbol);
    }

    let mut stmt =
        conn.prepare("SELECT id, to_name, edge_kind, confidence FROM edges ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let rows = rows.collect::<Result<Vec<_>, _>>()?;
    for (edge_id, to_name, edge_kind, current_confidence) in rows {
        let resolution = resolve_symbol(&to_name, &edge_kind, &by_name, &by_qualified);
        let Some((to_symbol_id, confidence)) = resolution else {
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
                     confidence = ?2
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
                 target_end_line = ?5
             WHERE id = ?1",
            params![
                edge_id,
                to_symbol_id.id,
                confidence.as_str(),
                to_symbol_id.start_line,
                to_symbol_id.end_line
            ],
        )?;
    }
    Ok(())
}

fn resolve_symbol<'a>(
    name: &str,
    edge_kind: &str,
    by_name: &BTreeMap<String, Vec<&'a IndexedSymbol>>,
    by_qualified: &BTreeMap<String, &'a IndexedSymbol>,
) -> Option<(&'a IndexedSymbol, EdgeConfidence)> {
    if let Some(symbol) = by_qualified.get(name) {
        return Some((symbol, EdgeConfidence::Exact));
    }
    let short = short_name(name);
    let matches = by_name.get(short)?;
    let preferred = preferred_matches(edge_kind, matches);
    let matches = if preferred.is_empty() { matches.as_slice() } else { preferred.as_slice() };
    match matches {
        [symbol] => Some((symbol, EdgeConfidence::Syntactic)),
        [symbol, ..] => Some((symbol, EdgeConfidence::Ambiguous)),
        [] => None,
    }
}

fn preferred_matches<'a>(edge_kind: &str, matches: &[&'a IndexedSymbol]) -> Vec<&'a IndexedSymbol> {
    let preferred_kinds: &[&str] = match edge_kind {
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
        ParserKind::Rust => tree_sitter_rust::language(),
        ParserKind::TypeScript => tree_sitter_typescript::language_typescript(),
        ParserKind::Tsx => tree_sitter_typescript::language_tsx(),
        ParserKind::Kotlin => tree_sitter_kotlin::language(),
        ParserKind::Markdown => return Ok(Vec::new()),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let Some(tree) = parser.parse(text, None) else {
        return Ok(Vec::new());
    };
    if tree.root_node().has_error() {
        return Ok(Vec::new());
    }
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
    match language {
        Language::Rust => rust_edges(text, node, symbols, path, out),
        Language::TypeScript => typescript_edges(text, node, symbols, path, out),
        Language::Kotlin => kotlin_edges(text, node, symbols, path, out),
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
                out.push(file_edge(path, node, name, EdgeKind::Imports, EdgeConfidence::NameOnly));
            }
        },
        "call_expression" => {
            if let Some(name) = call_target_name(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                ));
            }
            if let Some(receiver) = scoped_receiver_name(node, text) {
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
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::Ambiguous,
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
                out.push(file_edge(path, node, name, EdgeKind::Imports, EdgeConfidence::NameOnly));
            }
        },
        "export_statement" => {
            for name in identifiers_under(node, text) {
                out.push(file_edge(path, node, name, EdgeKind::Exports, EdgeConfidence::NameOnly));
            }
        },
        "call_expression" | "new_expression" => {
            let identifiers =
                identifiers_under(node.child_by_field_name("function").unwrap_or(node), text);
            if let Some(name) = identifiers.last().cloned().or_else(|| call_target_name(node, text))
            {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
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
        "import_header" | "import_directive" => {
            for name in identifiers_under(node, text) {
                out.push(file_edge(path, node, name, EdgeKind::Imports, EdgeConfidence::NameOnly));
            }
        },
        "call_expression" => {
            let identifiers = identifiers_under(node, text);
            if let Some(name) =
                identifiers.last().cloned().or_else(|| first_identifier_text(node, text))
            {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
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
                    constructor,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
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

fn file_edge(
    path: &Path,
    node: Node<'_>,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
) -> EdgeCandidate {
    EdgeCandidate {
        from_symbol_id: None,
        from_name: Some(path.to_string_lossy().replace('\\', "/")),
        to_name,
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
    let byte = node.start_byte();
    let source = containing_symbol(symbols, byte);
    EdgeCandidate {
        from_symbol_id: source.map(|symbol| symbol.id),
        from_name: source.map(|symbol| symbol.qualified_name.clone()),
        to_name,
        source_span: span_for_node(node),
        edge_kind,
        confidence,
    }
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
        if let Ok(value) = node.utf8_text(text.as_bytes()) {
            if !value.is_empty() {
                out.push(value.to_string());
            }
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

fn short_name(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(name)
}

fn symbols_for_file(conn: &Connection, file_id: i64) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.name, symbols.qualified_name, symbols.kind,
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
        SELECT symbols.id, symbols.name, symbols.qualified_name, symbols.kind,
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
    let start_byte = usize::try_from(row.get::<_, i64>(4)?).unwrap_or(0);
    let end_byte = usize::try_from(row.get::<_, i64>(5)?).unwrap_or(0);
    let chunk_start_byte = usize::try_from(row.get::<_, i64>(6)?).unwrap_or(start_byte);
    let chunk_start_line = row.get::<_, i64>(7)?;
    let chunk_text: String = row.get(8)?;
    let start_line = line_for_byte(&chunk_text, chunk_start_byte, chunk_start_line, start_byte);
    let end_line = line_for_byte(&chunk_text, chunk_start_byte, chunk_start_line, end_byte);
    Ok(IndexedSymbol {
        id: row.get(0)?,
        name: row.get(1)?,
        qualified_name: row.get(2)?,
        kind: row.get(3)?,
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
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                edge_kind, confidence
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                file_id,
                candidate.from_symbol_id,
                candidate.from_name,
                to_name,
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
