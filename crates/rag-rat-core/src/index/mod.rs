pub mod ai;
pub mod anchors;
pub mod chunker;
pub mod edges;
pub mod git_history;
pub mod github;
pub mod parser;
pub mod schema;
pub mod symbols;
pub mod walker;

mod incremental;
mod internals;
mod lifecycle;
mod query_api;
mod rebuild;

pub(crate) use lifecycle::install_scope_view;

#[cfg(test)]
mod anchor_tests;
#[cfg(test)]
mod parser_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use gix::bstr::{BString, ByteSlice};
use gix::status::{UntrackedFiles, tree_index};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::{Config, TargetKind};
use crate::index::ai::{LocalAiStatus, ModelInfo, ReconcilePlan, ReconcileReport};
use crate::index::anchors::{AnchorStatus, ChunkAnchor};
use crate::index::chunker::Chunk;
use crate::index::git_history::{
    ChunkBlameSummary, CommitSearchHit, GitHistoryIndexStatus, PathHistoryItem, QueryCommitHit,
    SymbolHistoryItem,
};
use crate::index::github::{GitHubEvidence, GitHubStatus, GitHubSyncReport, Papertrail};
use crate::index::symbols::Symbol;
use crate::language::Language;
use crate::query::graph_meta::{self, GraphMetaMode};
use crate::search::lexical::{SearchHit, SearchOptions};
use crate::storage::{IndexConnection, StorageStatus};

#[derive(Debug)]
pub struct IndexDatabase {
    storage: IndexConnection,
    pub active_commit_sha: String,
    pub active_worktree_id: String,
    /// Injected GitHub repo context. Resolved from `gh` only in `open_config` (real usage);
    /// `rebuild`/`open` leave it offline, and tests set it explicitly — so the library never
    /// shells out to `gh` during tests (#60).
    github: github::GitHubContext,
}

#[derive(Debug, Clone)]
pub enum IndexProgress {
    Started {
        database: PathBuf,
        mode: IndexMode,
    },
    Discovering,
    Discovered {
        files: usize,
    },
    PreparingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
    IndexingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
    IndexingGitHistory,
    RebuildingLogicalSymbols,
    ResolvingGraph,
    SyncingFts,
    RebuildingFts,
    Finished {
        files: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Changed,
    Discover,
    Full,
}

impl IndexMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Changed => "changed files",
            Self::Discover => "discovery",
            Self::Full => "full rebuild",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub database: String,
    pub exists: bool,
    pub schema: schema::SchemaStatus,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
    pub indexed_at_ms: Option<i64>,
    pub content_revision: String,
    pub fts_synced_at_ms: Option<i64>,
    pub fts_source_revision: Option<String>,
    pub fts_dirty: bool,
    pub fts_fresh: bool,
    pub file_count_by_language: BTreeMap<String, u64>,
    pub parser_failures: u64,
    pub parser_failure_paths: Vec<ParserFailure>,
    pub git_history: GitHistoryIndexStatus,
    pub github: GitHubStatus,
    pub local_ai: LocalAiStatus,
    pub anchor_health: AnchorHealth,
}

/// READ-only counts of active repo-memory bindings grouped by `anchor_status`.
/// Computed by a single GROUP BY query; does not run `memory_validate` or write anything.
#[derive(Debug, Default, Serialize)]
pub struct AnchorHealth {
    pub current: u64,
    pub relocated: u64,
    pub stale: u64,
    pub gone: u64,
}

#[derive(Debug, Serialize)]
pub struct HealIndexReport {
    pub checked_files: u64,
    pub healed_files: u64,
    pub removed_files: u64,
    pub skipped_files: u64,
    pub fts_fresh: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub files_pruned: u64,
    pub chunks_pruned: u64,
    pub files_remaining: u64,
    pub chunks_remaining: u64,
    /// True when no live context could be determined and pruning was skipped (nothing deleted).
    pub skipped: bool,
}

#[derive(Debug, Serialize)]
pub struct ParserFailure {
    pub path: String,
    pub language: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryStatus {
    pub discovered_files: usize,
    pub indexed_files: usize,
    pub unindexed_files: usize,
    pub unindexed_source_files: usize,
    pub changed_indexed_files: usize,
    pub removed_indexed_files: usize,
    pub unindexed_sample: Vec<String>,
    pub warning: Option<String>,
}

const MAX_AUTO_HEAL_FILES_PER_CALL: usize = 4;
const GRAPH_INDEX_VERSION: &str = "6";

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("Gone: indexed chunk {chunk_id} no longer exists")]
    Gone { chunk_id: i64 },
    #[error("StaleChunk: chunk {chunk_id} in {path} could not be relocated after reindex")]
    StaleChunk { chunk_id: i64, path: String },
    #[error("needs_reindex: {stale_files} stale files exceeds automatic heal cap {cap}")]
    NeedsReindex { stale_files: usize, cap: usize },
}

#[derive(Debug)]
struct FileRow {
    language: Language,
    kind: TargetKind,
}

#[derive(Debug)]
struct GraphReindexFile {
    id: i64,
    path: String,
    language: Language,
    kind: TargetKind,
}

#[derive(Debug)]
struct GraphPathRow {
    language: String,
    sha256: String,
    indexed_revision: String,
}

fn rank_docs_for_symbol(symbol: &crate::query::symbol::SymbolHit, hits: &mut [SearchHit]) {
    let source_module = module_stem(&symbol.path);
    let symbol_name = symbol.name.to_ascii_lowercase();
    let qualified_name = symbol.qualified_name.to_ascii_lowercase();
    hits.sort_by(|a, b| {
        let a_rank = docs_locality_rank(symbol, &source_module, &symbol_name, &qualified_name, a);
        let b_rank = docs_locality_rank(symbol, &source_module, &symbol_name, &qualified_name, b);
        a_rank
            .cmp(&b_rank)
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    for (idx, hit) in hits.iter_mut().enumerate() {
        hit.score = (10_000usize.saturating_sub(idx)) as f64;
    }
}

fn docs_locality_rank(
    symbol: &crate::query::symbol::SymbolHit,
    source_module: &str,
    symbol_name: &str,
    qualified_name: &str,
    hit: &SearchHit,
) -> u8 {
    let path = hit.path.to_ascii_lowercase();
    let summary = hit.summary.to_ascii_lowercase();
    let hit_symbol = hit.symbol_path.as_deref().unwrap_or_default().to_ascii_lowercase();
    if hit.path == symbol.path && hit_symbol == symbol.qualified_name.to_ascii_lowercase() {
        return 0;
    }
    if hit.path == symbol.path {
        return 1;
    }
    if !source_module.is_empty()
        && path.contains(source_module)
        && (summary.contains(symbol_name) || hit_symbol.contains(symbol_name))
    {
        return 2;
    }
    if summary.contains(qualified_name) || hit_symbol.contains(qualified_name) {
        return 3;
    }
    if summary.contains(symbol_name) || hit_symbol.contains(symbol_name) {
        return 4;
    }
    if !source_module.is_empty() && path.contains(source_module) {
        return 5;
    }
    9
}

fn module_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn dedupe_search_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = BTreeSet::new();
    hits.retain(|hit| seen.insert(hit.chunk_id));
}

fn bounded_summary(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LogicalSymbolKey {
    language: String,
    path: String,
    name: String,
    qualified_name: String,
    kind: String,
    // Signature is part of the identity so that two distinct same-named symbols in one file (e.g.
    // `new` on two different impls — same `qualified_name`, different signatures) do NOT collapse
    // into one logical symbol. Genuine cfg variants share a signature, so they still group.
    signature: Option<String>,
}

impl LogicalSymbolKey {
    fn from(row: &LogicalSymbolMemberRow) -> Self {
        Self {
            language: row.language.clone(),
            path: row.path.clone(),
            name: row.name.clone(),
            qualified_name: row.qualified_name.clone(),
            kind: row.kind.clone(),
            signature: row.signature.clone(),
        }
    }

    /// Deterministic logical-symbol id derived from the key, so it is **stable across reindex**
    /// (the table is fully rebuilt each pass; an autoincrement rowid would churn the id every
    /// time, breaking any cached id or logical-symbol-bound memory). A 63-bit truncation of the
    /// key's SHA-256 — collisions are astronomically unlikely across a repo's symbols, and a
    /// collision would surface as a loud primary-key error on rebuild rather than silent merging.
    fn stable_id(&self) -> i64 {
        let canonical = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.language,
            self.path,
            self.name,
            self.qualified_name,
            self.kind,
            self.signature.as_deref().unwrap_or(""),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) >> 1) as i64
    }
}

#[derive(Debug, Clone)]
struct LogicalSymbolMemberRow {
    symbol_id: i64,
    path: String,
    language: String,
    name: String,
    qualified_name: String,
    kind: String,
    signature: Option<String>,
    start_line: i64,
    end_line: i64,
}

fn symbol_line_for_byte(
    text: &str,
    chunk_start_byte: usize,
    chunk_start_line: i64,
    byte: usize,
) -> i64 {
    if byte <= chunk_start_byte {
        return chunk_start_line.max(1);
    }
    let local = byte.saturating_sub(chunk_start_byte).min(text.len());
    chunk_start_line
        + i64::try_from(text[..local].bytes().filter(|byte| *byte == b'\n').count()).unwrap_or(0)
}

fn graph_only_reason(edge: &crate::query::graph::GraphHop, current_line: Option<&str>) -> String {
    let Some(line) = current_line else {
        return "missing_current_source_line".to_string();
    };
    if edge
        .target_qualified_name
        .as_deref()
        .is_some_and(|qualified| !qualified.is_empty() && line.contains(qualified))
    {
        return "qualified_call_pattern_mismatch".to_string();
    }
    if edge.target.as_deref().is_some_and(|target| !target.is_empty() && line.contains(target)) {
        return "imported_or_unqualified_call".to_string();
    }
    if edge
        .evidence
        .as_deref()
        .is_some_and(|evidence| !evidence.is_empty() && line.contains(evidence.trim()))
    {
        return "regex_too_narrow".to_string();
    }
    "stale_or_overbroad_graph_edge".to_string()
}

fn is_likely_false_positive_graph_only(
    edge: &crate::query::graph::GraphHop,
    graph_only: &crate::query::graph::GraphOnlyEdge,
) -> bool {
    if graph_only.likely_reason == "stale_or_overbroad_graph_edge" {
        return true;
    }
    edge.resolution == "target_name_fallback"
        || edge.confidence == "name_only"
        || edge.confidence == "ambiguous"
        || !edge.verified_target_symbol
}

fn classify_text_only_hit(
    path: &str,
    text: &str,
    parser_failure_paths: &BTreeSet<String>,
) -> &'static str {
    if parser_failure_paths.contains(path) {
        return "parser_failure";
    }
    if is_generated_path(path) {
        return "generated_text_mention";
    }
    let trimmed = text.trim_start();
    if is_comment_like_text(trimmed) {
        return "comment_text_mention";
    }
    if is_import_or_declaration_text(trimmed) {
        return "declaration_text_mention";
    }
    if is_test_like_path(path) && is_test_scaffolding_text(trimmed) {
        return "test_scaffolding_text_mention";
    }
    "parser_call_extraction"
}

fn is_likely_parser_gap_kind(kind: &str) -> bool {
    matches!(kind, "parser_call_extraction" | "parser_failure")
}

fn is_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/generated-web/")
        || path.ends_with(".d.ts")
        || path.ends_with("_bg.wasm.d.ts")
}

fn is_comment_like_text(text: &str) -> bool {
    text.starts_with("//")
        || text.starts_with("/*")
        || text.starts_with('*')
        || text.starts_with("*/")
        || text.starts_with("#")
}

fn is_import_or_declaration_text(text: &str) -> bool {
    text.starts_with("import ")
        || text.starts_with("export type ")
        || text.starts_with("export interface ")
        || text.starts_with("type ")
        || text.starts_with("interface ")
        || text.starts_with("declare ")
}

fn is_test_scaffolding_text(text: &str) -> bool {
    text.contains(".mock")
        || text.contains("jest.")
        || text.contains("jest<")
        || text.contains("expect(")
        || text.contains("toHaveBeen")
        || text.contains("describe(")
        || text.contains("it(")
        || text.contains("test(")
}

fn recommended_graph_text_fallback(
    parser_gaps: &[crate::query::graph::TextOnlyHit],
    graph_only_edges: &[crate::query::graph::GraphOnlyEdge],
) -> String {
    match (parser_gaps.is_empty(), graph_only_edges.is_empty()) {
        (false, false) => "both",
        (false, true) => "text",
        (true, false) => "graph",
        (true, true) => "none",
    }
    .to_string()
}

fn compare_pattern_match_mode(pattern: &str, symbol_name: &str) -> String {
    if symbol_name.is_empty() {
        return "regex".to_string();
    }
    let escaped_call = format!("{symbol_name}\\(");
    let plain_call = format!("{symbol_name}(");
    if pattern.contains("\\b")
        || pattern.contains("\\W")
        || pattern.contains("[^")
        || pattern.contains(&escaped_call)
        || pattern.contains(&plain_call)
    {
        return "identifier_or_call".to_string();
    }
    if pattern.contains(symbol_name) {
        return "substring_identifier".to_string();
    }
    "regex".to_string()
}

fn is_test_like_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.ends_with("_test.rs")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.tsx")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".spec.tsx")
}

#[derive(Debug)]
struct IndexedFile {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct IndexFile {
    full_path: PathBuf,
    relative_path: PathBuf,
    language: Language,
    kind: TargetKind,
    commit_sha: String,
    worktree_id: String,
}

#[derive(Debug, Clone)]
struct FileScope {
    commit_sha: String,
    worktree_id: String,
}

impl FileScope {
    fn commit(commit_sha: String) -> Self {
        Self { commit_sha, worktree_id: String::new() }
    }

    fn worktree(worktree_id: String) -> Self {
        Self { commit_sha: String::new(), worktree_id }
    }
}

#[derive(Debug)]
struct PreparedIndexFile {
    file: IndexFile,
    prepared: anyhow::Result<PreparedIndexContent>,
}

#[derive(Debug)]
struct PreparedIndexContent {
    modified_at_ms: i64,
    text: String,
    sha256: String,
    chunks: Vec<Chunk>,
    symbols: Vec<Symbol>,
    parser_failure: Option<String>,
}

#[derive(Debug)]
struct DiscoveryPlan {
    files: Vec<IndexFile>,
    deleted: BTreeSet<PathBuf>,
    unindexed: Vec<IndexFile>,
    changed: Vec<PathBuf>,
    discovered_files: usize,
    indexed_files: usize,
}

#[derive(Debug, Default)]
struct GitChangedPaths {
    changed: BTreeSet<PathBuf>,
    deleted: BTreeSet<PathBuf>,
}

fn collect_index_files(config: &Config) -> anyhow::Result<Vec<IndexFile>> {
    let mut targets = config.targets.iter().collect::<Vec<_>>();
    targets.sort_by_key(|target| match target.kind {
        TargetKind::Generated => 0,
        TargetKind::Tests => 1,
        TargetKind::Docs => 2,
        TargetKind::Source => 3,
    });
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();

    for target in targets {
        for file in walker::walk_target(&config.root, target)? {
            let relative_path = file.strip_prefix(&config.root)?.to_path_buf();
            if !seen.insert(relative_path.clone()) {
                continue;
            }
            files.push(IndexFile {
                full_path: file,
                relative_path,
                language: target.language,
                kind: target.kind,
                commit_sha: String::new(),
                worktree_id: String::new(),
            });
        }
    }

    Ok(files)
}

fn collect_changed_index_files(
    config: &Config,
    changes: &GitChangedPaths,
) -> anyhow::Result<Vec<IndexFile>> {
    let mut files = Vec::new();
    for relative_path in &changes.changed {
        let full_path = config.root.join(relative_path);
        if !full_path.is_file() {
            continue;
        }
        let Some((language, kind)) = target_for_path(config, relative_path) else {
            continue;
        };
        files.push(IndexFile {
            full_path,
            relative_path: relative_path.clone(),
            language,
            kind,
            commit_sha: String::new(),
            worktree_id: String::new(),
        });
    }
    Ok(files)
}

fn spawn_git_history_prepare(
    root: &Path,
) -> JoinHandle<anyhow::Result<git_history::PreparedGitHistory>> {
    let root = root.to_path_buf();
    thread::spawn(move || git_history::prepare(&root))
}

fn join_git_history_prepare(
    handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
) -> anyhow::Result<git_history::PreparedGitHistory> {
    handle.join().map_err(|_| anyhow::anyhow!("git history preparation panicked"))?
}

fn prepare_index_file(file: &IndexFile) -> PreparedIndexFile {
    PreparedIndexFile { file: file.clone(), prepared: prepare_index_content(file) }
}

fn prepare_files_with_progress<F>(
    files: &[IndexFile],
    progress: &mut F,
) -> anyhow::Result<Vec<PreparedIndexFile>>
where
    F: FnMut(IndexProgress),
{
    #[derive(Debug)]
    struct PreparedProgress {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    }

    let total = files.len();
    let prepared = thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let completed = AtomicUsize::new(0);
        let handle = scope.spawn(move || {
            files
                .par_iter()
                .map(|file| {
                    let prepared = prepare_index_file(file);
                    let current = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if should_report_file_progress(current, total) {
                        let _ = tx.send(PreparedProgress {
                            current,
                            total,
                            path: file.relative_path.clone(),
                            language: file.language,
                            kind: file.kind,
                        });
                    }
                    prepared
                })
                .collect::<Vec<_>>()
        });

        for event in rx {
            progress(IndexProgress::PreparingFile {
                current: event.current,
                total: event.total,
                path: event.path,
                language: event.language,
                kind: event.kind,
            });
        }

        handle.join().map_err(|_| anyhow::anyhow!("parallel file preparation panicked"))
    })?;
    Ok(prepared)
}

fn should_report_file_progress(current: usize, total: usize) -> bool {
    if total == 0 {
        return false;
    }
    current == 1
        || current == total
        || current.saturating_mul(10) / total
            != current.saturating_sub(1).saturating_mul(10) / total
}

fn prepare_index_content(file: &IndexFile) -> anyhow::Result<PreparedIndexContent> {
    let text = fs::read_to_string(&file.full_path)?;
    let modified_at_ms = file_metadata_ms(&file.full_path)?;
    let sha256 = hex_sha256(text.as_bytes());
    let parser_failure =
        if file.language != Language::Markdown && file.kind != TargetKind::Generated {
            if text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                None
            } else {
                parser::parse_error(&file.relative_path, file.language, &text)
                    .unwrap_or_else(|err| Some(err.to_string()))
            }
        } else {
            None
        };
    let chunks = if file.kind == TargetKind::Generated {
        chunker::generated_chunks_for_file(&file.relative_path, &text)
    } else {
        chunker::chunks_for_file(&file.relative_path, file.language, &text)
    };
    let symbols =
        if file.kind == TargetKind::Generated || text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
            Vec::new()
        } else {
            symbols::symbols_for_file(&file.relative_path, file.language, &text)
        };
    Ok(PreparedIndexContent { modified_at_ms, text, sha256, chunks, symbols, parser_failure })
}

fn discovery_plan(conn: &rusqlite::Connection, config: &Config) -> anyhow::Result<DiscoveryPlan> {
    let discovered = collect_index_files(config)?;
    let mut indexed = indexed_file_map(conn)?;
    let mut current_paths = BTreeSet::new();
    let mut files = Vec::new();
    let mut unindexed = Vec::new();
    let mut changed = Vec::new();
    let discovered_files = discovered.len();
    let hashed = discovered
        .par_iter()
        .map(|file| -> anyhow::Result<(IndexFile, String)> {
            let text = fs::read(&file.full_path)?;
            Ok((file.clone(), hex_sha256(&text)))
        })
        .collect::<Vec<_>>();

    for hashed_file in hashed {
        let (file, current_hash) = hashed_file?;
        let relative = path_string(&file.relative_path);
        current_paths.insert(file.relative_path.clone());
        let Some(indexed_hash) = indexed.remove(&relative) else {
            unindexed.push(file.clone());
            files.push(file);
            continue;
        };
        if current_hash != indexed_hash {
            changed.push(file.relative_path.clone());
            files.push(file);
        }
    }

    let deleted = indexed
        .into_keys()
        .map(PathBuf::from)
        .filter(|path| !current_paths.contains(path))
        .collect::<BTreeSet<_>>();

    Ok(DiscoveryPlan {
        discovered_files,
        indexed_files: current_paths
            .len()
            .saturating_add(deleted.len())
            .saturating_sub(unindexed.len()),
        files,
        deleted,
        unindexed,
        changed,
    })
}

fn indexed_file_map(conn: &rusqlite::Connection) -> anyhow::Result<BTreeMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, sha256 FROM files ORDER BY path")?;
    let rows =
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    let mut files = BTreeMap::new();
    for row in rows {
        let (path, sha256) = row?;
        files.insert(path, sha256);
    }
    Ok(files)
}

pub(crate) fn target_for_path(
    config: &Config,
    relative_path: &Path,
) -> Option<(Language, TargetKind)> {
    let relative = path_string(relative_path);
    let language = Language::from_path(relative_path)?;
    let mut targets = config.targets.iter().collect::<Vec<_>>();
    targets.sort_by_key(|target| match target.kind {
        TargetKind::Generated => 0,
        TargetKind::Tests => 1,
        TargetKind::Docs => 2,
        TargetKind::Source => 3,
    });
    targets.into_iter().find_map(|target| {
        if target.language != language {
            return None;
        }
        if !target.directories.iter().any(|directory| {
            directory.as_os_str().is_empty()
                || directory == Path::new(".")
                || relative_path.starts_with(directory)
        }) {
            return None;
        }
        if target.exclude.iter().any(|pattern| matches_simple_pattern(&relative, pattern)) {
            return None;
        }
        if !target.include.iter().any(|pattern| matches_simple_pattern(&relative, pattern)) {
            return None;
        }
        Some((target.language, target.kind))
    })
}

fn git_changed_paths(root: &Path) -> anyhow::Result<GitChangedPaths> {
    let repo = gix::discover(root)?;
    let worktree_root = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("git repository has no worktree"))?
        .to_path_buf();
    let pathspec = config_root_pathspec(&worktree_root, root);
    let mut paths = GitChangedPaths::default();

    for item in repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        .tree_index_track_renames(tree_index::TrackRenames::Disabled)
        .into_iter([pathspec])?
    {
        let item = item?;
        let Some(path) = repo_relative_path_to_config_path(&worktree_root, root, item.location())
        else {
            continue;
        };
        if root.join(&path).exists() {
            if !paths.deleted.contains(&path) {
                paths.changed.insert(path);
            }
        } else {
            paths.changed.remove(&path);
            paths.deleted.insert(path);
        }
    }

    Ok(paths)
}

fn repo_relative_path_to_config_path(
    worktree_root: &Path,
    config_root: &Path,
    repo_relative_path: &gix::bstr::BStr,
) -> Option<PathBuf> {
    let path = PathBuf::from(repo_relative_path.to_str_lossy().as_ref());
    worktree_root.join(path).strip_prefix(config_root).ok().map(Path::to_path_buf)
}

fn config_root_pathspec(worktree_root: &Path, config_root: &Path) -> BString {
    let relative = config_root.strip_prefix(worktree_root).unwrap_or_else(|_| Path::new(""));
    let relative = path_string(relative);
    if relative.is_empty() || relative == "." {
        BString::from("*")
    } else {
        BString::from(format!("{relative}/**"))
    }
}

fn matches_simple_pattern(path: &str, pattern: &str) -> bool {
    if let Some(extension) = pattern.strip_prefix("**/*.") {
        return path.ends_with(&format!(".{extension}"));
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }
    path == pattern || path.contains(pattern.trim_matches('*'))
}

fn meta_for(conn: &rusqlite::Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(root).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn resolve_git_context(root: &Path) -> (String, String) {
    let commit_sha =
        git_output(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default();
    let worktree_id = root.to_string_lossy().trim_end_matches('/').to_string();
    (commit_sha, worktree_id)
}

/// The live (commit_sha, worktree_id) keys across every worktree that shares this repo, from
/// `git worktree list --porcelain`. Each worktree contributes its HEAD commit (for clean rows)
/// and its path (for dirty/overlay rows). Returns empty vecs outside a git worktree.
fn live_worktree_contexts(root: &Path) -> (Vec<String>, Vec<String>) {
    let mut commits = Vec::new();
    let mut worktrees = Vec::new();
    let Some(output) = git_output(root, &["worktree", "list", "--porcelain"]) else {
        return (commits, worktrees);
    };
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.push(path.trim().trim_end_matches('/').to_string());
        } else if let Some(sha) = line.strip_prefix("HEAD ") {
            commits.push(sha.trim().to_string());
        }
    }
    (commits, worktrees)
}

fn table_row_count(conn: &rusqlite::Connection, table: &str) -> anyhow::Result<u64> {
    // `table` is always an internal string literal, never user input.
    let count = conn
        .query_row(&format!("SELECT COUNT(*) FROM main.{table}"), [], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn file_metadata_ms(path: &Path) -> anyhow::Result<i64> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(duration_ms(modified.duration_since(UNIX_EPOCH)?))
}

fn now_ms() -> i64 {
    duration_ms(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn duration_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod schema_bootstrap_tests;
