use super::*;

pub(crate) fn target_qualified_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = node_text(function, text);
    (value.contains("::") || value.contains('.')).then(|| value.replace('.', "::"))
}
pub(crate) fn dotted_qualified_name(identifiers: &[String]) -> Option<String> {
    (identifiers.len() > 1).then(|| identifiers.join("::"))
}
pub(crate) fn c_like_qualified_name(identifiers: &[String]) -> Option<String> {
    (identifiers.len() > 1).then(|| identifiers.join("::"))
}
pub(crate) fn containing_symbol(symbols: &[IndexedSymbol], byte: usize) -> Option<&IndexedSymbol> {
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
pub(crate) fn call_target_name(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("function")
        .and_then(|child| last_identifier_text(child, text))
        .map(|name| short_name(&name).to_string())
        .or_else(|| first_identifier_text(node, text))
}
pub(crate) fn scoped_receiver_name(node: Node<'_>, text: &str) -> Option<String> {
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
pub(crate) fn child_name_text(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| child.utf8_text(text.as_bytes()).ok())
        .map(ToOwned::to_owned)
}
pub(crate) fn first_identifier_text(node: Node<'_>, text: &str) -> Option<String> {
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
pub(crate) fn last_identifier_text(node: Node<'_>, text: &str) -> Option<String> {
    identifiers_under(node, text).into_iter().last()
}
pub(crate) fn identifiers_under(node: Node<'_>, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_identifiers(node, text, &mut out);
    out
}
pub(crate) fn collect_identifiers(node: Node<'_>, text: &str, out: &mut Vec<String>) {
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
pub(crate) fn is_identifier_kind(kind: &str) -> bool {
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
pub(crate) fn is_rust_path_keyword(value: &str) -> bool {
    matches!(value, "self" | "super" | "crate")
}
pub(crate) fn looks_like_type_name(value: &str) -> bool {
    value.chars().next().is_some_and(char::is_uppercase)
}
pub(crate) fn node_text(node: Node<'_>, text: &str) -> String {
    node.utf8_text(text.as_bytes()).unwrap_or_default().to_string()
}
pub(crate) fn edge_evidence(node: Node<'_>, text: &str) -> String {
    node_text(node, text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}
pub(crate) fn short_name(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(name)
}
pub(crate) fn symbols_for_file(
    conn: &Connection,
    file_id: i64,
) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, \
         symbols.qualified_name, symbols.kind,
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
pub(crate) fn all_symbols(conn: &Connection) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, \
         symbols.qualified_name, symbols.kind,
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
pub(crate) fn symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSymbol> {
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
pub(crate) fn insert_candidates(
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
pub(crate) fn span_for_node(node: Node<'_>) -> EdgeSpan {
    EdgeSpan {
        start_line: i64::try_from(node.start_position().row).unwrap_or(i64::MAX).saturating_add(1),
        end_line: i64::try_from(node.end_position().row).unwrap_or(i64::MAX).saturating_add(1),
        start_byte: i64::try_from(node.start_byte()).unwrap_or(i64::MAX),
        end_byte: i64::try_from(node.end_byte()).unwrap_or(i64::MAX),
    }
}
pub(crate) fn line_for_byte(
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
pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
