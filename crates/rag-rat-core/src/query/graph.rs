use rusqlite::{Connection, params_from_iter};
use serde::Serialize;

const CALL_EDGE_KINDS: &[&str] = &["calls_name", "constructs"];
const REFERENCE_EDGE_KINDS: &[&str] =
    &["references_type", "imports", "exports", "contains", "implements"];
const OPTIONAL_EDGE_KINDS: &[&str] = &[
    "calls_name",
    "constructs",
    "uses_macro",
    "references_type",
    "imports",
    "exports",
    "contains",
    "implements",
];

#[derive(Debug, Clone, Default)]
pub struct GraphTraversalOptions {
    pub include_references: bool,
    pub edge_kinds: Option<Vec<String>>,
    pub resolution_mode: GraphResolutionMode,
    pub symbol_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GraphTraversalReport {
    pub query: GraphTraversalQuery,
    pub summary: GraphTraversalSummary,
    pub coverage: GraphCoverage,
    pub results: Vec<GraphHop>,
}

#[derive(Debug, Serialize)]
pub struct GraphTraversalQuery {
    pub tool: String,
    pub symbol_id: Option<i64>,
    pub symbol_path: String,
    pub resolution: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GraphTraversalSummary {
    pub returned_count: u64,
    pub total_matching_edges: u64,
    pub truncated: bool,
    pub exact_verified: u64,
    pub syntactic: u64,
    pub name_only: u64,
    pub ambiguous: u64,
    pub unresolved: u64,
    pub false_positive_risk: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GraphCoverage {
    pub indexed_files: u64,
    pub parser_failures: u64,
    pub source_stale_files: u64,
    pub known_index_gaps: Vec<GraphIndexGap>,
    pub parser_coverage_for_paths: Vec<GraphPathCoverage>,
}

#[derive(Debug, Serialize)]
pub struct GraphIndexGap {
    pub path: String,
    pub language: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct GraphPathCoverage {
    pub path: String,
    pub language: String,
    pub parser_status: String,
    pub graph_status: String,
    pub last_indexed_revision: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GraphResolutionMode {
    Exact,
    #[default]
    Syntactic,
    Fuzzy,
}

impl GraphResolutionMode {
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value.unwrap_or("syntactic") {
            "exact" => Ok(Self::Exact),
            "syntactic" => Ok(Self::Syntactic),
            "fuzzy" => Ok(Self::Fuzzy),
            other => anyhow::bail!(
                "unknown graph resolution mode `{other}`; expected exact, syntactic, or fuzzy"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Syntactic => "syntactic",
            Self::Fuzzy => "fuzzy",
        }
    }
}

impl GraphTraversalOptions {
    pub fn callee_edge_kinds(&self) -> anyhow::Result<Vec<String>> {
        if let Some(edge_kinds) = &self.edge_kinds {
            validate_edge_kinds(edge_kinds)?;
            return Ok(edge_kinds.clone());
        }
        let mut edge_kinds =
            CALL_EDGE_KINDS.iter().map(|value| (*value).to_string()).collect::<Vec<_>>();
        if self.include_references {
            edge_kinds.extend(REFERENCE_EDGE_KINDS.iter().map(|value| (*value).to_string()));
        }
        Ok(edge_kinds)
    }

    pub fn caller_edge_kinds(&self) -> anyhow::Result<Vec<String>> {
        self.callee_edge_kinds()
    }
}

#[derive(Debug, Serialize)]
pub struct GraphHop {
    pub from_symbol: Option<String>,
    pub to_symbol: Option<String>,
    pub edge_kind: String,
    pub confidence: String,
    pub edge_confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_qualified_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_hint: Option<String>,
    pub resolution: String,
    pub verified_target_symbol: bool,
    pub shown_by_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
}

#[derive(Debug, Serialize)]
pub struct Callsite {
    pub path: String,
    pub line: i64,
    pub span: [i64; 2],
}

pub fn traverse(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
) -> anyhow::Result<Vec<GraphHop>> {
    traverse_with_options(conn, symbol, reverse, limit, &GraphTraversalOptions::default())
}

pub fn traverse_with_options(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
    options: &GraphTraversalOptions,
) -> anyhow::Result<Vec<GraphHop>> {
    let edge_kinds =
        if reverse { options.caller_edge_kinds()? } else { options.callee_edge_kinds()? };
    let quoted = quoted_placeholders(edge_kinds.len());
    let unique_short_name = unique_symbol_name(conn, short_name(symbol))?;
    let mode = options.resolution_mode;
    let sql = if reverse {
        let predicate = reverse_predicate(mode);
        let tier = reverse_tier(mode);
        format!(
            "
            SELECT COALESCE(from_symbols.qualified_name, edges.from_name),
                   COALESCE(to_symbols.qualified_name, edges.to_name),
                   edges.edge_kind,
                   edges.confidence,
                   edges.to_name,
                   edges.target_qualified_name,
                   edges.evidence,
                   edges.receiver_hint,
                   edges.resolution,
                   edges.to_symbol_id IS NOT NULL,
                   source_files.path,
                   COALESCE(NULLIF(edges.source_start_line, 0), 1),
                   COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), 1),
                   {tier}
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
            ORDER BY 14,
                CASE edges.confidence
                    WHEN 'Exact' THEN 0
                    WHEN 'Syntactic' THEN 1
                    WHEN 'NameOnly' THEN 2
                    ELSE 3
                END,
                edges.edge_kind,
                edges.from_name
            LIMIT ?5
            "
        )
    } else {
        let predicate = forward_source_predicate(mode);
        let target_filter = forward_target_filter(mode);
        format!(
            "
            SELECT COALESCE(from_symbols.qualified_name, edges.from_name),
                   COALESCE(to_symbols.qualified_name, edges.to_name),
                   edges.edge_kind,
                   edges.confidence,
                   edges.to_name,
                   edges.target_qualified_name,
                   edges.evidence,
                   edges.receiver_hint,
                   edges.resolution,
                   edges.to_symbol_id IS NOT NULL,
                   source_files.path,
                   COALESCE(NULLIF(edges.source_start_line, 0), 1),
                   COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), 1),
                   0
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
              AND ({target_filter})
              AND ?4 IN ('true', 'false')
            ORDER BY
                CASE edges.confidence
                    WHEN 'Exact' THEN 0
                    WHEN 'Syntactic' THEN 1
                    WHEN 'NameOnly' THEN 2
                    ELSE 3
                END,
                edges.edge_kind,
                edges.to_name
            LIMIT ?5
            "
        )
    };
    let params = traversal_params(symbol, limit, &edge_kinds, options.symbol_id, unique_short_name);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        let edge_kind: String = row.get(2)?;
        let confidence: String = row.get(3)?;
        let verified_target_symbol = row.get(9)?;
        let resolution =
            resolution_label(mode, row.get::<_, String>(8)?, row.get(13)?, verified_target_symbol);
        let callsite_path: String = row.get(10)?;
        let callsite_start = row.get(11)?;
        let callsite_end = row.get(12)?;
        Ok(GraphHop {
            from_symbol: row.get(0)?,
            to_symbol: row.get(1)?,
            edge_kind: edge_kind.clone(),
            confidence: confidence.clone(),
            edge_confidence: confidence,
            target: row.get(4)?,
            target_qualified_name: row.get(5)?,
            evidence: row.get(6)?,
            receiver_hint: row.get(7)?,
            resolution,
            verified_target_symbol,
            shown_by_default: CALL_EDGE_KINDS.contains(&edge_kind.as_str()),
            callsite: Some(Callsite {
                path: callsite_path,
                line: callsite_start,
                span: [callsite_start, callsite_end],
            }),
        })
    })?;
    let mut hops = Vec::new();
    for row in rows {
        hops.push(row?);
    }
    Ok(hops)
}

pub fn traversal_summary(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
    options: &GraphTraversalOptions,
    returned_count: usize,
) -> anyhow::Result<GraphTraversalSummary> {
    let edge_kinds =
        if reverse { options.caller_edge_kinds()? } else { options.callee_edge_kinds()? };
    let quoted = quoted_placeholders(edge_kinds.len());
    let unique_short_name = unique_symbol_name(conn, short_name(symbol))?;
    let mode = options.resolution_mode;
    let sql = if reverse {
        let predicate = reverse_predicate(mode);
        format!(
            "
            SELECT
                COUNT(*),
                SUM(CASE WHEN edges.to_symbol_id IS NOT NULL THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Syntactic' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'NameOnly' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Ambiguous' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.to_symbol_id IS NULL THEN 1 ELSE 0 END)
            FROM edges
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
            "
        )
    } else {
        let predicate = forward_source_predicate(mode);
        let target_filter = forward_target_filter(mode);
        format!(
            "
            SELECT
                COUNT(*),
                SUM(CASE WHEN edges.to_symbol_id IS NOT NULL THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Syntactic' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'NameOnly' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Ambiguous' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.to_symbol_id IS NULL THEN 1 ELSE 0 END)
            FROM edges
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
              AND ({target_filter})
              AND ?4 IN ('true', 'false')
            "
        )
    };
    let params = traversal_params(symbol, limit, &edge_kinds, options.symbol_id, unique_short_name);
    let mut summary = conn.query_row(&sql, params_from_iter(params), |row| {
        Ok(GraphTraversalSummary {
            returned_count: u64::try_from(returned_count).unwrap_or(u64::MAX),
            total_matching_edges: count_col(row, 0)?,
            truncated: false,
            exact_verified: count_col(row, 1)?,
            syntactic: count_col(row, 2)?,
            name_only: count_col(row, 3)?,
            ambiguous: count_col(row, 4)?,
            unresolved: count_col(row, 5)?,
            false_positive_risk: String::new(),
        })
    })?;
    summary.truncated = summary.total_matching_edges > u64::from(limit);
    summary.false_positive_risk = false_positive_risk(&summary, mode).to_string();
    Ok(summary)
}

fn count_col(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, Option<i64>>(index)?.unwrap_or(0);
    Ok(u64::try_from(value).unwrap_or(0))
}

fn false_positive_risk(summary: &GraphTraversalSummary, mode: GraphResolutionMode) -> &'static str {
    if summary.ambiguous > 0 || mode == GraphResolutionMode::Fuzzy {
        "high"
    } else if summary.name_only > 0
        || summary.unresolved > 0
        || mode == GraphResolutionMode::Syntactic
    {
        "medium"
    } else {
        "low"
    }
}

fn validate_edge_kinds(edge_kinds: &[String]) -> anyhow::Result<()> {
    for edge_kind in edge_kinds {
        if !OPTIONAL_EDGE_KINDS.contains(&edge_kind.as_str()) {
            anyhow::bail!("unknown graph edge kind `{edge_kind}`");
        }
    }
    Ok(())
}

fn traversal_params(
    symbol: &str,
    limit: u32,
    edge_kinds: &[String],
    symbol_id: Option<i64>,
    unique_short_name: bool,
) -> Vec<String> {
    let qualified = symbol.to_string();
    let short = short_name(symbol).to_string();
    let fuzzy_qualified = format!("%::{qualified}");
    let allow_name_fallback = (!is_qualified_symbol(symbol)).to_string();
    let mut params = vec![
        qualified,
        fuzzy_qualified,
        short,
        allow_name_fallback,
        limit.to_string(),
        symbol_id.unwrap_or(-1).to_string(),
        unique_short_name.to_string(),
    ];
    params.extend(edge_kinds.iter().cloned());
    params
}

fn quoted_placeholders(count: usize) -> String {
    (0..count).map(|index| format!("?{}", index + 8)).collect::<Vec<_>>().join(", ")
}

fn reverse_predicate(mode: GraphResolutionMode) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => {
            "edges.to_symbol_id IS NOT NULL
             AND (edges.to_symbol_id = ?6 OR to_symbols.qualified_name = ?1)"
        },
        GraphResolutionMode::Syntactic => {
            "(edges.to_symbol_id = ?6
              OR to_symbols.qualified_name = ?1
              OR (?7 = 'true' AND to_symbols.name = ?3)
              OR edges.target_qualified_name = ?1)"
        },
        GraphResolutionMode::Fuzzy => {
            "to_symbols.name = ?3
             OR to_symbols.qualified_name = ?1
             OR to_symbols.qualified_name LIKE ?2
             OR edges.target_qualified_name = ?1
             OR edges.target_qualified_name LIKE ?2
             OR (?4 = 'true' AND edges.to_name = ?3)"
        },
    }
}

fn reverse_tier(mode: GraphResolutionMode) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => "0",
        GraphResolutionMode::Syntactic => {
            "CASE
                WHEN edges.to_symbol_id IS NOT NULL THEN 0
                WHEN edges.target_qualified_name = ?1 THEN 1
                ELSE 4
             END"
        },
        GraphResolutionMode::Fuzzy => {
            "CASE
                WHEN edges.to_symbol_id IS NOT NULL THEN 0
                WHEN edges.target_qualified_name = ?1 OR edges.target_qualified_name LIKE ?2 THEN 1
                WHEN ?4 = 'true' AND edges.to_name = ?3 THEN 2
                ELSE 4
             END"
        },
    }
}

fn forward_source_predicate(mode: GraphResolutionMode) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => {
            "from_symbols.id IS NOT NULL
             AND (from_symbols.id = ?6 OR from_symbols.qualified_name = ?1)"
        },
        GraphResolutionMode::Syntactic => {
            "from_symbols.id = ?6
             OR from_symbols.qualified_name = ?1
             OR (?7 = 'true' AND from_symbols.name = ?3)
             OR edges.from_name = ?1"
        },
        GraphResolutionMode::Fuzzy => {
            "from_symbols.name = ?3
             OR from_symbols.qualified_name = ?1
             OR from_symbols.qualified_name LIKE ?2
             OR edges.from_name = ?1
             OR edges.from_name LIKE ?2"
        },
    }
}

fn forward_target_filter(mode: GraphResolutionMode) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => "edges.to_symbol_id IS NOT NULL",
        GraphResolutionMode::Syntactic => {
            "edges.to_symbol_id IS NOT NULL OR edges.target_qualified_name IS NOT NULL"
        },
        GraphResolutionMode::Fuzzy => "1 = 1",
    }
}

fn unique_symbol_name(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM symbols WHERE name = ?1", [name], |row| row.get(0))?;
    Ok(count == 1)
}

fn resolution_label(
    mode: GraphResolutionMode,
    stored: String,
    tier: i64,
    verified_target_symbol: bool,
) -> String {
    if mode == GraphResolutionMode::Exact && verified_target_symbol {
        return "exact".to_string();
    }
    if stored != "unresolved" {
        return stored;
    }
    match tier {
        1 => "target_qualified_suffix".to_string(),
        2 => "target_name_fallback".to_string(),
        _ => stored,
    }
}

fn short_name(symbol: &str) -> &str {
    symbol.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(symbol)
}

fn is_qualified_symbol(symbol: &str) -> bool {
    symbol.contains("::")
        || symbol.contains(".rs:")
        || symbol.contains(".ts:")
        || symbol.contains(".tsx:")
        || symbol.contains(".kt:")
        || symbol.contains('/')
}
