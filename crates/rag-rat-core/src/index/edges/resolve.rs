use super::*;

pub(crate) fn resolve_all_edges(conn: &Connection) -> anyhow::Result<()> {
    let symbols = all_symbols(conn)?;
    let index = SymbolIndex::build(&symbols);
    let mut stmt = conn.prepare(
        "SELECT id, source_file_id, to_name, target_qualified_name, edge_kind, confidence, \
         evidence, receiver_hint FROM edges ORDER BY id",
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
pub(crate) fn resolve_symbol<'a>(
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
                [_, ..] if same_logical_symbol(&same_file) =>
                    Some((same_file[0], EdgeConfidence::Syntactic, "logical_variant")),
                _ => None,
            }
        },
        [] => None,
    }
}
pub(crate) fn same_logical_symbol(symbols: &[&IndexedSymbol]) -> bool {
    let Some(first) = symbols.first() else {
        return false;
    };
    symbols.iter().all(|symbol| {
        symbol.qualified_name == first.qualified_name
            && symbol.name == first.name
            && symbol.kind == first.kind
    })
}
pub(crate) fn allow_unqualified_fallback(
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
pub(crate) fn is_external_rust_root(value: &str) -> bool {
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
pub(crate) fn is_common_member_name(value: &str) -> bool {
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
pub(crate) fn preferred_matches<'a>(
    edge_kind: &str,
    matches: &[&'a IndexedSymbol],
) -> Vec<&'a IndexedSymbol> {
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
