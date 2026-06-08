use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::query::graph::{self, GraphHop, GraphResolutionMode, GraphTraversalOptions};
use crate::query::symbol::SymbolHit;

#[derive(Debug, Serialize)]
pub struct ImpactItem {
    pub path: String,
    pub language: String,
    pub kind: String,
    pub symbol: Option<String>,
    pub category: String,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactSurfaceOptions {
    pub resolution_mode: GraphResolutionMode,
    pub include_tests: bool,
    pub include_docs: bool,
    pub include_git: bool,
    pub include_papertrail: bool,
    pub include_text_fallback: bool,
}

#[derive(Debug, Serialize)]
pub struct ImpactSurfaceReport {
    pub query: ImpactSurfaceQuery,
    pub direct_semantic_callers: Vec<GraphHop>,
    pub direct_semantic_callees: Vec<GraphHop>,
    pub import_export_dependents: Vec<ImpactItem>,
    pub tests_touching_symbol_path: Vec<ImpactItem>,
    pub docs_mentioning_symbol_path: Vec<ImpactItem>,
    pub text_fallback_hits: Vec<ImpactItem>,
    pub recent_commits_touching_symbol_path: Vec<ImpactItem>,
    pub github_rationale_issues_prs: Vec<ImpactItem>,
    pub completeness_and_caveats: ImpactCompleteness,
}

#[derive(Debug, Serialize)]
pub struct ImpactSurfaceQuery {
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub query: Option<String>,
    pub resolution: String,
    pub include_tests: bool,
    pub include_docs: bool,
    pub include_git: bool,
    pub include_papertrail: bool,
    pub include_text_fallback: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct ImpactCompleteness {
    pub exact_graph_callers: u64,
    pub graph_callees: u64,
    pub text_fallback_hits: u64,
    pub parser_failures: u64,
    pub stale_files: u64,
    pub caveats: Vec<String>,
}

impl Default for ImpactSurfaceOptions {
    fn default() -> Self {
        Self {
            resolution_mode: GraphResolutionMode::Syntactic,
            include_tests: true,
            include_docs: true,
            include_git: true,
            include_papertrail: true,
            include_text_fallback: true,
        }
    }
}

pub fn impact_surface(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    impact_surface_with_options(conn, query, limit, GraphResolutionMode::Syntactic)
}

pub fn impact_surface_report_for_symbol(
    conn: &Connection,
    symbol: &SymbolHit,
    limit: u32,
    options: &ImpactSurfaceOptions,
) -> anyhow::Result<ImpactSurfaceReport> {
    let graph_options = GraphTraversalOptions {
        resolution_mode: options.resolution_mode,
        symbol_id: Some(symbol.symbol_id),
        logical_symbol_id: symbol.logical_symbol_id,
        ..Default::default()
    };
    let direct_semantic_callers =
        graph::traverse_with_options(conn, &symbol.qualified_name, true, limit, &graph_options)?;
    let direct_semantic_callees =
        graph::traverse_with_options(conn, &symbol.qualified_name, false, limit, &graph_options)?;
    let names = vec![symbol.name.clone(), symbol.qualified_name.clone()];
    let import_export_dependents =
        import_export_items(conn, symbol.symbol_id, &symbol.qualified_name, &names, limit)?;
    let tests_touching_symbol_path =
        if options.include_tests { test_items(conn, symbol, &names, limit)? } else { Vec::new() };
    let docs_mentioning_symbol_path =
        if options.include_docs { docs_items(conn, symbol, &names, limit)? } else { Vec::new() };
    let text_fallback_hits = if options.include_text_fallback {
        text_fallback_items(conn, symbol, &names, limit)?
    } else {
        Vec::new()
    };
    let recent_commits_touching_symbol_path = if options.include_git {
        git_commit_items(conn, std::slice::from_ref(&symbol.path), limit)?
    } else {
        Vec::new()
    };
    let github_rationale_issues_prs = if options.include_papertrail {
        let mut items = github_ref_items(conn, std::slice::from_ref(&symbol.path), limit)?;
        items.extend(github_rationale_items(conn, &symbol.qualified_name, limit)?);
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        items
    } else {
        Vec::new()
    };
    let mut caveats = vec![
        "Graph evidence is tree-sitter/syntactic, not compiler-grade name resolution.".to_string(),
    ];
    if options.resolution_mode == GraphResolutionMode::Exact
        && direct_semantic_callers.is_empty()
        && !text_fallback_hits.is_empty()
    {
        caveats.push(format!(
            "No exact graph callers found. Text search found {} symbol/path hits. This likely indicates graph extraction or resolution gaps.",
            text_fallback_hits.len()
        ));
    }
    Ok(ImpactSurfaceReport {
        query: ImpactSurfaceQuery {
            symbol_id: Some(symbol.symbol_id),
            symbol_path: Some(symbol.qualified_name.clone()),
            query: None,
            resolution: options.resolution_mode.as_str().to_string(),
            include_tests: options.include_tests,
            include_docs: options.include_docs,
            include_git: options.include_git,
            include_papertrail: options.include_papertrail,
            include_text_fallback: options.include_text_fallback,
        },
        completeness_and_caveats: ImpactCompleteness {
            exact_graph_callers: u64::try_from(direct_semantic_callers.len()).unwrap_or(u64::MAX),
            graph_callees: u64::try_from(direct_semantic_callees.len()).unwrap_or(u64::MAX),
            text_fallback_hits: u64::try_from(text_fallback_hits.len()).unwrap_or(u64::MAX),
            parser_failures: parser_failure_count(conn)?,
            stale_files: 0,
            caveats,
        },
        direct_semantic_callers,
        direct_semantic_callees,
        import_export_dependents,
        tests_touching_symbol_path,
        docs_mentioning_symbol_path,
        text_fallback_hits,
        recent_commits_touching_symbol_path,
        github_rationale_issues_prs,
    })
}

pub fn impact_surface_with_options(
    conn: &Connection,
    query: &str,
    limit: u32,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Vec<ImpactItem>> {
    impact_surface_from_targets(conn, query, None, limit, resolution_mode)
}

pub fn impact_surface_for_symbol(
    conn: &Connection,
    symbol: &SymbolHit,
    limit: u32,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Vec<ImpactItem>> {
    let target = SymbolTarget {
        id: symbol.symbol_id,
        file_id: symbol.file_id,
        path: symbol.path.clone(),
        language: symbol.language.clone(),
        file_kind: symbol.file_kind.clone(),
        name: symbol.name.clone(),
        qualified_name: symbol.qualified_name.clone(),
    };
    impact_surface_from_targets(
        conn,
        &symbol.qualified_name,
        Some(vec![target]),
        limit,
        resolution_mode,
    )
}

fn impact_surface_from_targets(
    conn: &Connection,
    query: &str,
    selected_targets: Option<Vec<SymbolTarget>>,
    limit: u32,
    resolution_mode: GraphResolutionMode,
) -> anyhow::Result<Vec<ImpactItem>> {
    let max_items = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut surface = ImpactSurface::default();
    let targets = match selected_targets {
        Some(targets) => targets,
        None => exact_symbols(conn, query)?,
    };
    let target_names = target_names(query, &targets);

    for symbol in &targets {
        surface.push(
            ImpactCategory::DirectStructural,
            FileSymbol {
                path: symbol.path.clone(),
                language: symbol.language.clone(),
                kind: symbol.file_kind.clone(),
                symbol: Some(symbol.qualified_name.clone()),
            },
            "exact_symbol_definition",
            format!("defined as {}", symbol.qualified_name),
        );
    }

    graph_neighbors(conn, &targets, &target_names, true, resolution_mode, &mut surface)?;
    graph_neighbors(conn, &targets, &target_names, false, resolution_mode, &mut surface)?;
    import_export_dependents(conn, &targets, &target_names, &mut surface)?;
    same_file_siblings(conn, &targets, &mut surface)?;

    if surface.len() < max_items {
        let remaining = max_items.saturating_sub(surface.len());
        textual_fallback(conn, query, &mut surface, remaining)?;
    }

    let current_paths = surface.current_paths();
    historical_evidence(conn, &current_paths, query, &mut surface, max_items)?;

    Ok(surface.into_items(max_items))
}

pub fn ffi_surface(conn: &Connection, limit: u32) -> anyhow::Result<Vec<ImpactItem>> {
    let mut stmt = conn.prepare(
        "
        WITH rust_exports AS (
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   symbols.qualified_name AS symbol,
                   CASE
                       WHEN symbols.kind = 'impl' THEN 'rust_uniffi_exported_impl'
                       ELSE 'rust_uniffi_export'
                   END AS reason
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            JOIN symbol_facts
              ON symbol_facts.symbol_id = symbols.id
             AND symbol_facts.fact_kind = 'rust_attr'
             AND symbol_facts.fact_value = 'uniffi_export'
            WHERE files.language = 'rust'
              AND symbols.kind IN ('function', 'method', 'impl', 'struct', 'enum', 'trait')
        ),
        rust_exported_impl_members AS (
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   members.qualified_name AS symbol,
                   'rust_uniffi_impl_member' AS reason
            FROM symbols AS impls
            JOIN files ON files.id = impls.file_id
            JOIN symbol_facts
              ON symbol_facts.symbol_id = impls.id
             AND symbol_facts.fact_kind = 'rust_attr'
             AND symbol_facts.fact_value = 'uniffi_export'
            JOIN symbols AS members
              ON members.file_id = impls.file_id
             AND members.start_byte > impls.start_byte
             AND members.end_byte < impls.end_byte
             AND members.kind IN ('function', 'method')
            WHERE files.language = 'rust'
              AND impls.kind = 'impl'
        ),
        binding_refs AS (
            SELECT DISTINCT
                   files.path AS path,
                   files.language AS language,
                   files.kind AS kind,
                   chunks.symbol_path AS symbol,
                   CASE
                       WHEN chunks.text LIKE '%NativeHeldCore%'
                         OR chunks.text LIKE '%uniffi_held_core_%'
                         OR chunks.text LIKE '%ffi_held_core_%'
                           THEN 'native_binding_reference'
                       ELSE 'generated_binding_artifact'
                   END AS reason
            FROM files
            JOIN chunks ON chunks.file_id = files.id
            WHERE chunks.text LIKE '%NativeHeldCore%'
               OR chunks.text LIKE '%uniffi_held_core_%'
               OR chunks.text LIKE '%ffi_held_core_%'
               OR files.path LIKE '%/src/generated/%'
               OR files.path LIKE '%/generated/%'
               OR files.path LIKE '%generated-manifest.json'
        )
        SELECT path, language, kind, symbol, reason FROM rust_exports
        UNION
        SELECT path, language, kind, symbol, reason FROM rust_exported_impl_members
        UNION
        SELECT path, language, kind, symbol, reason FROM binding_refs
        ORDER BY reason, kind DESC, path
        LIMIT ?1
        ",
    )?;
    rows_to_items(stmt.query_map([limit], |row| {
        let reason: String = row.get(4)?;
        Ok(ImpactItem {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            symbol: row.get(3)?,
            category: ImpactCategory::ProbableTextual.as_str().to_string(),
            reason: reason.clone(),
            evidence: ffi_surface_evidence(&reason),
        })
    })?)
}

fn ffi_surface_evidence(reason: &str) -> Vec<String> {
    let mut evidence = vec![format!("ffi_surface evidence class: {reason}")];
    match reason {
        "rust_uniffi_impl_member" => {
            evidence.push(
                "member symbol is inside a chunk containing an exported UniFFI impl".to_string(),
            );
            evidence.push(
                "this row is not claiming the member itself has a #[uniffi::export] attribute"
                    .to_string(),
            );
        },
        "rust_uniffi_exported_impl" => {
            evidence.push(
                "exported impl/type surface; member rows are reported separately when symbols are available"
                    .to_string(),
            );
        },
        _ => {},
    }
    evidence
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ImpactCategory {
    DirectStructural,
    ProbableTextual,
    HistoricalPapertrail,
}

impl ImpactCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::DirectStructural => "Direct structural impact",
            Self::ProbableTextual => "Probable textual impact",
            Self::HistoricalPapertrail => "Historical/papertrail evidence",
        }
    }
}

#[derive(Debug, Clone)]
struct FileSymbol {
    path: String,
    language: String,
    kind: String,
    symbol: Option<String>,
}

#[derive(Debug, Clone)]
struct SymbolTarget {
    id: i64,
    file_id: i64,
    path: String,
    language: String,
    file_kind: String,
    name: String,
    qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImpactKey {
    category: &'static str,
    path: String,
    symbol: Option<String>,
    reason: String,
}

#[derive(Default)]
struct ImpactSurface {
    items: BTreeMap<ImpactKey, ImpactItem>,
}

impl ImpactSurface {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn push(
        &mut self,
        category: ImpactCategory,
        file_symbol: FileSymbol,
        reason: impl Into<String>,
        evidence: impl Into<String>,
    ) {
        let reason = reason.into();
        let key = ImpactKey {
            category: category.as_str(),
            path: file_symbol.path.clone(),
            symbol: file_symbol.symbol.clone(),
            reason: reason.clone(),
        };
        let item = self.items.entry(key).or_insert_with(|| ImpactItem {
            path: file_symbol.path,
            language: file_symbol.language,
            kind: file_symbol.kind,
            symbol: file_symbol.symbol,
            category: category.as_str().to_string(),
            reason,
            evidence: Vec::new(),
        });
        let evidence = evidence.into();
        if !item.evidence.iter().any(|value| value == &evidence) {
            item.evidence.push(evidence);
        }
    }

    fn current_paths(&self) -> Vec<String> {
        let mut paths = BTreeSet::new();
        for item in self.items.values() {
            if item.category != ImpactCategory::HistoricalPapertrail.as_str() {
                paths.insert(item.path.clone());
            }
        }
        paths.into_iter().collect()
    }

    fn into_items(self, limit: usize) -> Vec<ImpactItem> {
        let mut items = self.items.into_values().collect::<Vec<_>>();
        items.sort_by_key(|item| {
            (
                category_rank(&item.category),
                reason_rank(&item.reason),
                item.path.clone(),
                item.symbol.clone().unwrap_or_default(),
            )
        });
        items.truncate(limit);
        items
    }
}

fn category_rank(category: &str) -> u8 {
    match category {
        "Direct structural impact" => 0,
        "Probable textual impact" => 1,
        "Historical/papertrail evidence" => 2,
        _ => 3,
    }
}

fn reason_rank(reason: &str) -> u8 {
    match reason {
        "exact_symbol_definition" => 0,
        "direct_caller" => 1,
        "direct_callee" => 2,
        "import_export_dependent" => 3,
        "same_file_sibling" => 4,
        "textual_fallback" => 5,
        "git_commit_touched_file" => 6,
        "github_papertrail" => 7,
        _ => 8,
    }
}

fn exact_symbols(conn: &Connection, query: &str) -> anyhow::Result<Vec<SymbolTarget>> {
    let candidates = symbol_query_candidates(query);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, files.path, files.language, files.kind,
               symbols.name, symbols.qualified_name
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.name = ?1 OR symbols.qualified_name = ?1
        ORDER BY files.kind, files.path, symbols.start_byte
        ",
    )?;
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    let multi_candidate_query = candidates.len() > 1;
    for candidate in candidates {
        let qualified_candidate = is_qualified_symbol(candidate);
        if multi_candidate_query && !qualified_candidate && !is_high_signal_query_token(candidate) {
            continue;
        }
        let rows = stmt.query_map([candidate], |row| {
            Ok(SymbolTarget {
                id: row.get(0)?,
                file_id: row.get(1)?,
                path: row.get(2)?,
                language: row.get(3)?,
                file_kind: row.get(4)?,
                name: row.get(5)?,
                qualified_name: row.get(6)?,
            })
        })?;
        let rows = collect_rows(rows)?;
        if !qualified_candidate && !is_high_signal_symbol_candidate(&rows) {
            continue;
        }
        for row in rows {
            if seen.insert(row.id) {
                targets.push(row);
            }
        }
    }
    Ok(targets)
}

fn is_high_signal_query_token(value: &str) -> bool {
    value.contains('_') || value.chars().next().is_some_and(char::is_uppercase)
}

fn is_high_signal_symbol_candidate(rows: &[SymbolTarget]) -> bool {
    match rows {
        [] => false,
        [_] => true,
        [first, ..] if rows.len() <= 4 => {
            rows.iter().all(|row| row.path == first.path && row.name == first.name)
        },
        _ => false,
    }
}

fn target_names(query: &str, targets: &[SymbolTarget]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for candidate in symbol_query_candidates(query) {
        names.insert(candidate.to_string());
        names.insert(short_symbol_name(candidate).to_string());
    }
    for target in targets {
        names.insert(target.name.clone());
        names.insert(target.qualified_name.clone());
    }
    names.into_iter().collect()
}

fn symbol_query_candidates(query: &str) -> Vec<&str> {
    query
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|ch: char| {
                !(ch.is_alphanumeric() || matches!(ch, '_' | ':' | '/' | '.' | '-'))
            })
        })
        .filter(|token| !token.is_empty())
        .filter(|token| token.contains("::") || is_non_stopword_identifier(token))
        .collect()
}

fn is_non_stopword_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let is_identifier = (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    is_identifier
        && !matches!(
            value,
            "of" | "in"
                | "to"
                | "from"
                | "for"
                | "and"
                | "or"
                | "the"
                | "callers"
                | "callee"
                | "callees"
                | "caller"
                | "impact"
                | "symbol"
        )
}

fn short_symbol_name(value: &str) -> &str {
    value.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(value)
}

fn is_qualified_symbol(value: &str) -> bool {
    value.contains("::") || value.contains('/')
}

fn graph_neighbors(
    conn: &Connection,
    targets: &[SymbolTarget],
    target_names: &[String],
    reverse: bool,
    resolution_mode: GraphResolutionMode,
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    let reason = if reverse { "direct_caller" } else { "direct_callee" };
    let source_path_col = if reverse {
        "COALESCE(source_files.path, from_files.path)"
    } else {
        "COALESCE(to_files.path, source_files.path)"
    };
    let source_language_col = if reverse {
        "COALESCE(source_files.language, from_files.language)"
    } else {
        "COALESCE(to_files.language, source_files.language)"
    };
    let source_kind_col = if reverse {
        "COALESCE(source_files.kind, from_files.kind)"
    } else {
        "COALESCE(to_files.kind, source_files.kind)"
    };
    let source_symbol_col = if reverse {
        "COALESCE(from_symbols.qualified_name, edges.from_name)"
    } else {
        "COALESCE(to_symbols.qualified_name, edges.to_name)"
    };
    let predicate = impact_graph_predicate(reverse, resolution_mode);
    let sql = format!(
        "
        SELECT {source_path_col}, {source_language_col}, {source_kind_col},
               {source_symbol_col}, edges.edge_kind, edges.confidence
        FROM edges
        LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
        LEFT JOIN files from_files ON from_files.id = from_symbols.file_id
        LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
        LEFT JOIN files to_files ON to_files.id = to_symbols.file_id
        LEFT JOIN files source_files ON source_files.id = edges.source_file_id
        WHERE edges.edge_kind IN ('calls_name', 'constructs', 'implements')
          AND ({predicate})
          AND {source_path_col} IS NOT NULL
        ORDER BY
            CASE edges.confidence
                WHEN 'Exact' THEN 0
                WHEN 'Syntactic' THEN 1
                WHEN 'NameOnly' THEN 2
                ELSE 3
            END,
            edges.edge_kind,
            {source_path_col},
            {source_symbol_col}
        ",
    );
    let mut stmt = conn.prepare(&sql)?;
    for target in targets {
        let rows = stmt.query_map(params![target.id, target.qualified_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (path, language, kind, symbol, edge_kind, confidence) = row?;
            surface.push(
                ImpactCategory::DirectStructural,
                FileSymbol { path, language, kind, symbol },
                reason,
                format!("{edge_kind} edge to {} ({confidence})", target.qualified_name),
            );
        }
    }
    for name in target_names {
        if resolution_mode != GraphResolutionMode::Fuzzy && !is_qualified_symbol(name) {
            continue;
        }
        let rows = stmt.query_map(params![Option::<i64>::None, name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (path, language, kind, symbol, edge_kind, confidence) = row?;
            surface.push(
                ImpactCategory::DirectStructural,
                FileSymbol { path, language, kind, symbol },
                reason,
                format!("{edge_kind} edge matching {name} ({confidence})"),
            );
        }
    }
    Ok(())
}

fn impact_graph_predicate(reverse: bool, mode: GraphResolutionMode) -> &'static str {
    match (reverse, mode) {
        (true, GraphResolutionMode::Exact) => "edges.to_symbol_id = ?1",
        (false, GraphResolutionMode::Exact) => {
            "edges.from_symbol_id = ?1 AND edges.to_symbol_id IS NOT NULL"
        },
        (true, GraphResolutionMode::Syntactic) => {
            "edges.to_symbol_id = ?1 OR edges.target_qualified_name = ?2"
        },
        (false, GraphResolutionMode::Syntactic) => {
            "(edges.from_symbol_id = ?1 OR edges.from_name = ?2)
             AND (edges.to_symbol_id IS NOT NULL OR edges.target_qualified_name IS NOT NULL)"
        },
        (true, GraphResolutionMode::Fuzzy) => "edges.to_symbol_id = ?1 OR edges.to_name = ?2",
        (false, GraphResolutionMode::Fuzzy) => "edges.from_symbol_id = ?1 OR edges.from_name = ?2",
    }
}

fn import_export_dependents(
    conn: &Connection,
    targets: &[SymbolTarget],
    target_names: &[String],
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, edges.from_name,
               edges.edge_kind, edges.confidence
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.edge_kind IN ('imports', 'exports')
          AND (edges.to_symbol_id = ?1 OR edges.to_name = ?2)
        ORDER BY files.kind, files.path, edges.edge_kind
        ",
    )?;
    for target in targets {
        let rows = stmt.query_map(params![target.id, target.qualified_name], import_export_row)?;
        push_import_export_rows(rows, target.qualified_name.as_str(), surface)?;
    }
    for name in target_names {
        let rows = stmt.query_map(params![Option::<i64>::None, name], import_export_row)?;
        push_import_export_rows(rows, name, surface)?;
    }
    Ok(())
}

fn import_export_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, Option<String>, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
}

fn push_import_export_rows(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(
            &rusqlite::Row<'_>,
        )
            -> rusqlite::Result<(String, String, String, Option<String>, String, String)>,
    >,
    target: &str,
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    for row in rows {
        let (path, language, kind, symbol, edge_kind, confidence) = row?;
        surface.push(
            ImpactCategory::DirectStructural,
            FileSymbol { path, language, kind, symbol },
            "import_export_dependent",
            format!("{edge_kind} edge matching {target} ({confidence})"),
        );
    }
    Ok(())
}

fn same_file_siblings(
    conn: &Connection,
    targets: &[SymbolTarget],
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, symbols.qualified_name
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.file_id = ?1 AND symbols.id != ?2
        ORDER BY symbols.start_byte
        LIMIT 20
        ",
    )?;
    for target in targets {
        let rows = stmt.query_map(params![target.file_id, target.id], |row| {
            Ok(FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: row.get(3)?,
            })
        })?;
        for row in rows {
            surface.push(
                ImpactCategory::DirectStructural,
                row?,
                "same_file_sibling",
                format!("shares file with {}", target.qualified_name),
            );
        }
    }
    Ok(())
}

fn textual_fallback(
    conn: &Connection,
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    if limit == 0 {
        return Ok(());
    }
    let like = format!("%{query}%");
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT files.path, files.language, files.kind, symbols.qualified_name,
               CASE
                   WHEN files.path LIKE ?1 THEN 'path LIKE fallback'
                   WHEN symbols.name LIKE ?1 OR symbols.qualified_name LIKE ?1 THEN 'symbol LIKE fallback'
                   ELSE 'chunk text LIKE fallback'
               END
        FROM files
        LEFT JOIN symbols ON symbols.file_id = files.id
        LEFT JOIN chunks ON chunks.file_id = files.id
        WHERE files.path LIKE ?1
           OR symbols.name LIKE ?1
           OR symbols.qualified_name LIKE ?1
           OR chunks.text LIKE ?1
        ORDER BY files.kind, files.path, symbols.qualified_name
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![like, i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
        Ok((
            FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: row.get(3)?,
            },
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (file_symbol, evidence) = row?;
        surface.push(ImpactCategory::ProbableTextual, file_symbol, "textual_fallback", evidence);
    }
    Ok(())
}

fn import_export_items(
    conn: &Connection,
    symbol_id: i64,
    qualified_name: &str,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, edges.from_name,
               edges.edge_kind, edges.confidence
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.edge_kind IN ('imports', 'exports')
          AND (edges.to_symbol_id = ?1 OR edges.to_name = ?2)
        ORDER BY files.kind, files.path, edges.edge_kind
        LIMIT ?3
        ",
    )?;
    for name in std::iter::once(qualified_name).chain(names.iter().map(String::as_str)) {
        let rows = stmt.query_map(params![symbol_id, name, i64::from(limit)], |row| {
            impact_item_row(row, "Import/export dependents", "import_export_dependent")
        })?;
        items.extend(rows_to_items(rows)?);
        if items.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
    }
    dedupe_items(&mut items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

fn test_items(
    conn: &Connection,
    symbol: &SymbolHit,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for name in names_for_like(symbol, names) {
        items.extend(section_like_items(
            conn,
            &name,
            "Tests touching this symbol/path",
            "test_mentions_symbol_or_path",
            "
            files.kind = 'source'
            AND (
                files.path LIKE '%test%'
                OR files.path LIKE '%spec%'
                OR chunks.text LIKE '%#[cfg(test)]%'
                OR chunks.text LIKE '%describe(%'
                OR chunks.text LIKE '%it(%'
                OR chunks.text LIKE '%test(%'
            )
            ",
            limit,
        )?);
    }
    dedupe_items(&mut items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

fn docs_items(
    conn: &Connection,
    symbol: &SymbolHit,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for name in names_for_like(symbol, names) {
        items.extend(section_like_items(
            conn,
            &name,
            "Docs mentioning symbol/path",
            "docs_mentions_symbol_or_path",
            "files.kind = 'docs'",
            limit,
        )?);
    }
    dedupe_items(&mut items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

fn text_fallback_items(
    conn: &Connection,
    symbol: &SymbolHit,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for name in names_for_like(symbol, names) {
        items.extend(section_like_items(
            conn,
            &name,
            "Text fallback hits",
            "text_fallback",
            "1 = 1",
            limit,
        )?);
    }
    dedupe_items(&mut items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

fn names_for_like(symbol: &SymbolHit, names: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    out.insert(symbol.name.clone());
    out.insert(symbol.qualified_name.clone());
    out.insert(symbol.path.clone());
    for name in names {
        out.insert(name.clone());
    }
    out.into_iter().collect()
}

fn section_like_items(
    conn: &Connection,
    needle: &str,
    category: &str,
    reason: &str,
    filter: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let like = format!("%{needle}%");
    let sql = format!(
        "
        SELECT DISTINCT files.path, files.language, files.kind, symbols.qualified_name,
               CASE
                   WHEN files.path LIKE ?1 THEN 'path match'
                   WHEN symbols.name LIKE ?1 OR symbols.qualified_name LIKE ?1 THEN 'symbol match'
                   ELSE 'chunk text match'
               END
        FROM files
        LEFT JOIN symbols ON symbols.file_id = files.id
        LEFT JOIN chunks ON chunks.file_id = files.id
        WHERE ({filter})
          AND (
              files.path LIKE ?1
              OR symbols.name LIKE ?1
              OR symbols.qualified_name LIKE ?1
              OR chunks.text LIKE ?1
          )
        ORDER BY files.kind, files.path, symbols.qualified_name
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![like, i64::from(limit)], |row| {
        Ok(ImpactItem {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            symbol: row.get(3)?,
            category: category.to_string(),
            reason: reason.to_string(),
            evidence: vec![format!("{} for `{needle}`", row.get::<_, String>(4)?)],
        })
    })?;
    rows_to_items(rows)
}

fn git_commit_items(
    conn: &Connection,
    paths: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut surface = ImpactSurface::default();
    git_commits_for_paths(conn, paths, &mut surface, usize::try_from(limit).unwrap_or(usize::MAX))?;
    Ok(surface.into_items(usize::try_from(limit).unwrap_or(usize::MAX)))
}

fn github_ref_items(
    conn: &Connection,
    paths: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut surface = ImpactSurface::default();
    github_refs_for_paths(conn, paths, &mut surface, usize::try_from(limit).unwrap_or(usize::MAX))?;
    Ok(surface.into_items(usize::try_from(limit).unwrap_or(usize::MAX)))
}

fn github_rationale_items(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut surface = ImpactSurface::default();
    github_rationale_for_query(
        conn,
        query,
        &mut surface,
        usize::try_from(limit).unwrap_or(usize::MAX),
    )?;
    Ok(surface.into_items(usize::try_from(limit).unwrap_or(usize::MAX)))
}

fn impact_item_row(
    row: &rusqlite::Row<'_>,
    category: &'static str,
    reason: &'static str,
) -> rusqlite::Result<ImpactItem> {
    Ok(ImpactItem {
        path: row.get(0)?,
        language: row.get(1)?,
        kind: row.get(2)?,
        symbol: row.get(3)?,
        category: category.to_string(),
        reason: reason.to_string(),
        evidence: vec![format!("{} edge ({})", row.get::<_, String>(4)?, row.get::<_, String>(5)?)],
    })
}

fn dedupe_items(items: &mut Vec<ImpactItem>) {
    let mut seen = BTreeSet::new();
    items.retain(|item| {
        seen.insert((
            item.category.clone(),
            item.path.clone(),
            item.symbol.clone(),
            item.reason.clone(),
        ))
    });
}

fn parser_failure_count(conn: &Connection) -> anyhow::Result<u64> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM parser_failures", [], |row| row.get(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn historical_evidence(
    conn: &Connection,
    paths: &[String],
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    if paths.is_empty() || surface.len() >= limit {
        return Ok(());
    }
    git_commits_for_paths(conn, paths, surface, limit.saturating_sub(surface.len()))?;
    if surface.len() >= limit {
        return Ok(());
    }
    github_refs_for_paths(conn, paths, surface, limit.saturating_sub(surface.len()))?;
    if surface.len() >= limit {
        return Ok(());
    }
    github_rationale_for_query(conn, query, surface, limit.saturating_sub(surface.len()))?;
    Ok(())
}

fn git_commits_for_paths(
    conn: &Connection,
    paths: &[String],
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let mut remaining = limit;
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind,
               git_commits.hash, git_commits.subject, git_commits.authored_at_s
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
        LEFT JOIN files ON files.path = git_file_changes.path
        WHERE git_file_changes.path = ?1
        ORDER BY git_commits.authored_at_s DESC, git_commits.hash
        LIMIT ?2
        ",
    )?;
    for path in paths {
        if remaining == 0 {
            break;
        }
        let file = file_for_path(conn, path)?;
        let rows =
            stmt.query_map(params![path, i64::try_from(remaining).unwrap_or(i64::MAX)], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?;
        for row in rows {
            let (row_path, language, kind, hash, subject, authored_at_s) = row?;
            let file_symbol = FileSymbol {
                path: row_path.unwrap_or_else(|| file.path.clone()),
                language: language.unwrap_or_else(|| file.language.clone()),
                kind: kind.unwrap_or_else(|| file.kind.clone()),
                symbol: None,
            };
            surface.push(
                ImpactCategory::HistoricalPapertrail,
                file_symbol,
                "git_commit_touched_file",
                format!("{} touched {path} at {authored_at_s}: {subject}", short_hash(&hash)),
            );
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

fn github_refs_for_paths(
    conn: &Connection,
    paths: &[String],
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let mut remaining = limit;
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number, ref_kind, source_kind, source_text
        FROM github_refs
        WHERE source_path = ?1
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    for path in paths {
        if remaining == 0 {
            break;
        }
        let file = file_for_path(conn, path)?;
        let rows =
            stmt.query_map(params![path, i64::try_from(remaining).unwrap_or(i64::MAX)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
        for row in rows {
            let (owner, repo, number, ref_kind, source_kind, source_text) = row?;
            surface.push(
                ImpactCategory::HistoricalPapertrail,
                file.clone(),
                "github_papertrail",
                format!("{owner}/{repo}#{number} {ref_kind}/{source_kind}: {source_text}"),
            );
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

fn github_rationale_for_query(
    conn: &Connection,
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let fts_query = fts_escape(query);
    if fts_query.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "
        SELECT url, title, classification
        FROM github_fts
        WHERE github_fts MATCH ?1
        ORDER BY rank
        LIMIT ?2
        ",
    )?;
    let rows = stmt
        .query_map(params![fts_query, i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
    for row in rows {
        let (url, title, classification) = row?;
        surface.push(
            ImpactCategory::HistoricalPapertrail,
            FileSymbol {
                path: "(github papertrail)".to_string(),
                language: "github".to_string(),
                kind: "papertrail".to_string(),
                symbol: None,
            },
            "github_papertrail",
            format!("{classification}: {title} ({url})"),
        );
    }
    Ok(())
}

fn file_for_path(conn: &Connection, path: &str) -> anyhow::Result<FileSymbol> {
    let row = conn
        .query_row("SELECT path, language, kind FROM files WHERE path = ?1", [path], |row| {
            Ok(FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: None,
            })
        })
        .optional()?;
    Ok(row.unwrap_or_else(|| FileSymbol {
        path: path.to_string(),
        language: "unknown".to_string(),
        kind: "historical".to_string(),
        symbol: None,
    }))
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn fts_escape(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn rows_to_items(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<ImpactItem>>,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
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
