mod predicates;
mod traverse;
use std::collections::BTreeSet;

pub(crate) use predicates::*;
use rusqlite::{Connection, params_from_iter};
use serde::Serialize;
pub(crate) use traverse::*;

const CALL_EDGE_KINDS: &[&str] = &["calls_name", "constructs"];
const MACRO_EDGE_KINDS: &[&str] = &["uses_macro"];
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
    pub include_unresolved: bool,
    pub include_macros: bool,
    pub include_common_methods: bool,
    pub edge_kinds: Option<Vec<String>>,
    pub resolution_mode: GraphResolutionMode,
    pub symbol_id: Option<i64>,
    pub logical_symbol_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GraphTraversalReport {
    pub query: GraphTraversalQuery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_symbol: Option<LogicalSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<LogicalSymbolVariant>,
    pub summary: GraphTraversalSummary,
    pub coverage: GraphCoverage,
    pub results: Vec<GraphHop>,
}

#[derive(Debug, Serialize)]
pub struct GraphTraversalQuery {
    pub tool: String,
    pub symbol_id: Option<i64>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_path: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalSymbol {
    pub logical_symbol_id: i64,
    pub qualified_name: String,
    pub variant_count: u64,
    pub group_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalSymbolVariant {
    pub symbol_id: i64,
    pub cfg_expr: Option<String>,
    pub signature_hash: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
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
    pub completeness_risk: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GraphCoverage {
    pub indexed_files: u64,
    pub parser_failures: u64,
    pub stale_files: u64,
    pub known_index_gaps: Vec<String>,
    pub parser_coverage_for_paths: Vec<GraphPathCoverage>,
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
        if self.include_macros {
            edge_kinds.extend(MACRO_EDGE_KINDS.iter().map(|value| (*value).to_string()));
        }
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
pub struct CompareGraphTextReport {
    pub query: CompareGraphTextQuery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_symbol: Option<LogicalSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<LogicalSymbolVariant>,
    pub summary: CompareGraphTextSummary,
    pub coverage: GraphCoverage,
    pub matched_hits: Vec<MatchedGraphTextHit>,
    pub text_only_hits: Vec<TextOnlyHit>,
    pub graph_only_edges: Vec<GraphOnlyEdge>,
    pub likely_parser_gaps: Vec<TextOnlyHit>,
    pub likely_false_positives: Vec<GraphOnlyEdge>,
}

#[derive(Debug, Serialize)]
pub struct CompareGraphTextQuery {
    pub symbol_id: Option<i64>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_path: String,
    pub pattern: String,
    pub resolution: String,
    pub include_tests: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct CompareGraphTextSummary {
    pub graph_hits: u64,
    pub graph_edges: u64,
    pub text_hits: u64,
    pub matched: u64,
    pub graph_only: u64,
    pub text_only: u64,
    pub text_mentions: u64,
    pub likely_parser_gaps: u64,
    pub likely_false_positives: u64,
    pub likely_index_gaps: u64,
    pub complete: bool,
    pub recommended_fallback: String,
    pub pattern_match_mode: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MatchedGraphTextHit {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub target: Option<String>,
    pub edge_kind: String,
    pub confidence: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextOnlyHit {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub reason: String,
    pub likely_gap: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphOnlyEdge {
    pub path: String,
    pub line: i64,
    pub target: Option<String>,
    pub edge_kind: String,
    pub confidence: String,
    pub resolution: String,
    pub evidence: Option<String>,
    pub reason: String,
    pub likely_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphHop {
    pub edge_id: i64,
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

#[derive(Debug, Clone, Serialize)]
pub struct Callsite {
    pub path: String,
    pub line: i64,
    pub span: [i64; 2],
}
