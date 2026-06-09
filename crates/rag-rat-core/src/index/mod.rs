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

#[cfg(test)]
mod anchor_tests;
#[cfg(test)]
mod parser_tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    thread::JoinHandle,
    time::{SystemTime, UNIX_EPOCH},
};

use gix::{
    bstr::{BString, ByteSlice},
    status::{UntrackedFiles, tree_index},
};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::{Config, TargetKind},
    index::{
        ai::{LocalAiStatus, ModelInfo, ReconcilePlan, ReconcileReport},
        anchors::{AnchorStatus, ChunkAnchor},
        chunker::Chunk,
        git_history::{
            ChunkBlameSummary, CommitSearchHit, GitHistoryIndexStatus, PathHistoryItem,
            QueryCommitHit, SymbolHistoryItem,
        },
        github::{GitHubEvidence, GitHubStatus, GitHubSyncReport, Papertrail},
        symbols::Symbol,
    },
    language::Language,
    query::graph_meta::{self, GraphMetaMode},
    search::lexical::{SearchHit, SearchOptions},
    storage::IndexConnection,
    storage::StorageStatus,
};

#[derive(Debug)]
pub struct IndexDatabase {
    storage: IndexConnection,
    pub active_commit_sha: String,
    pub active_worktree_id: String,
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

impl IndexDatabase {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_graph_check(path, true)
    }

    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    fn open_with_graph_check(path: &Path, check_graph: bool) -> anyhow::Result<Self> {
        let mut storage = IndexConnection::open(path)?;
        schema::check_compatible(storage.connection())?;
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(root) = meta_for(storage.connection(), "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        let db =
            Self { storage, active_commit_sha: String::new(), active_worktree_id: String::new() };
        if check_graph {
            db.ensure_graph_index_current()?;
        }
        Ok(db)
    }

    pub fn open_config(config: &Config) -> anyhow::Result<Self> {
        let mut db = Self::open_with_graph_check(&config.database, false)?;
        db.storage.set_source_root(config.root.clone());
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        db.ensure_graph_index_current()?;
        Ok(db)
    }

    pub fn migrate(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        Self::migrate_with_fastembed_cache(path, None)
    }

    fn migrate_with_fastembed_cache(
        path: &Path,
        fastembed_cache_dir: Option<&Path>,
    ) -> anyhow::Result<schema::SchemaStatus> {
        let storage = IndexConnection::open(path)?;
        let status = schema::status(storage.connection())?;
        match status.state {
            schema::SchemaState::Newer | schema::SchemaState::Dirty => {
                anyhow::bail!("{}", status.message);
            },
            schema::SchemaState::Compatible => {},
            schema::SchemaState::Missing | schema::SchemaState::Older => {
                schema::apply(storage.connection())?;
            },
        }
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(fastembed_cache_dir) = fastembed_cache_dir {
            ai::recover_cached_fastembed_model_from(storage.connection(), fastembed_cache_dir)?;
        } else {
            ai::recover_cached_fastembed_model(storage.connection())?;
        }
        schema::status(storage.connection())
    }

    pub fn migration_check(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        let storage = IndexConnection::open(path)?;
        schema::status(storage.connection())
    }

    fn create_or_migrate(path: &Path) -> anyhow::Result<Self> {
        let mut storage = IndexConnection::open(path)?;
        schema::apply(storage.connection())?;
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(root) = meta_for(storage.connection(), "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        Ok(Self { storage, active_commit_sha: String::new(), active_worktree_id: String::new() })
    }

    pub fn set_context(&mut self, commit_sha: &str, worktree_id: &str) -> anyhow::Result<()> {
        self.active_commit_sha = commit_sha.to_string();
        self.active_worktree_id = worktree_id.to_string();

        let conn = self.storage.connection();
        conn.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);
        ",
        )?;

        let mut stmt = conn.prepare(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES (?1, ?2)",
        )?;
        stmt.execute(params!["commit_sha", commit_sha])?;
        stmt.execute(params!["worktree_id", worktree_id])?;

        conn.execute_batch("
            DROP VIEW IF EXISTS temp.files;
            CREATE TEMP VIEW temp.files AS
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, indexed_revision, commit_sha, worktree_id
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = 'worktree_id') AND worktree_id != '' AND kind != 'deleted'
            UNION ALL
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, indexed_revision, commit_sha, worktree_id
            FROM main.files
            WHERE commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != ''
              AND path NOT IN (
                  SELECT path FROM main.files 
                  WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = 'worktree_id')
                    AND worktree_id != ''
              );
        ")?;

        Ok(())
    }

    pub fn rebuild(config: &Config) -> anyhow::Result<Self> {
        Self::rebuild_with_progress(config, |_| {})
    }

    pub fn rebuild_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Started {
            database: config.database.clone(),
            mode: IndexMode::Full,
        });
        let mut db = Self::create_or_migrate(&config.database)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        progress(IndexProgress::IndexingGitHistory);
        let mut git_history = Some(spawn_git_history_prepare(&config.root));
        let result = (|| -> anyhow::Result<()> {
            db.storage.execute_batch("BEGIN TRANSACTION")?;
            db.clear_full_rebuild_tables()?;
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            db.write_git_meta(&config.root)?;
            let indexed = db.index_targets_with_progress(config, &mut progress)?;
            db.apply_prepared_git_history(
                &config.root,
                git_history
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("git history preparation was already used"))?,
            )?;
            progress(IndexProgress::RebuildingLogicalSymbols);
            db.rebuild_logical_symbols()?;
            progress(IndexProgress::ResolvingGraph);
            db.resolve_edges()?;
            db.mark_graph_index_current()?;
            progress(IndexProgress::RebuildingFts);
            db.rebuild_fts()?;
            db.set_meta("indexed_at_ms", &now_ms().to_string())?;
            db.storage.execute_batch("COMMIT")?;
            progress(IndexProgress::Finished { files: indexed });
            Ok(())
        })();
        if result.is_err() {
            if let Some(handle) = git_history.take() {
                let _ = join_git_history_prepare(handle);
            }
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        result?;
        Ok(db)
    }

    fn clear_full_rebuild_tables(&self) -> anyhow::Result<()> {
        self.storage.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS full_rebuild_file_ids(id INTEGER PRIMARY KEY);
            DELETE FROM temp.full_rebuild_file_ids;
            INSERT OR IGNORE INTO temp.full_rebuild_file_ids(id)
            SELECT id
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = 'worktree_id')
              AND worktree_id != '';
            INSERT OR IGNORE INTO temp.full_rebuild_file_ids(id)
            SELECT id
            FROM main.files
            WHERE commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != ''
              AND path NOT IN (
                  SELECT path FROM main.files
                  WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = 'worktree_id')
                    AND worktree_id != ''
              );

            UPDATE main.edges
            SET to_symbol_id = NULL,
                target_start_line = NULL,
                target_end_line = NULL,
                resolution = 'unresolved'
            WHERE to_symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = symbols.file_id
            );
            DELETE FROM main.edges
            WHERE source_file_id IN (SELECT id FROM temp.full_rebuild_file_ids)
               OR from_symbol_id IN (
                    SELECT symbols.id
                    FROM main.symbols
                    JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = symbols.file_id
               );

            DELETE FROM main.logical_symbol_members
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = symbols.file_id
            );
            DELETE FROM main.logical_symbols
            WHERE id NOT IN (
                SELECT logical_symbol_id FROM main.logical_symbol_members
            );
            DELETE FROM main.symbol_facts
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = symbols.file_id
            );
            DELETE FROM main.chunk_fts
            WHERE rowid IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_summaries
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_embeddings
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = chunks.file_id
            );
            DELETE FROM main.git_chunk_blame
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = chunks.file_id
            );
            DELETE FROM main.docs
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = chunks.file_id
            );
            DELETE FROM main.parser_failures
            WHERE path IN (
                SELECT path
                FROM main.files
                JOIN temp.full_rebuild_file_ids ON full_rebuild_file_ids.id = files.id
            );
            DELETE FROM main.symbols
            WHERE file_id IN (SELECT id FROM temp.full_rebuild_file_ids);
            DELETE FROM main.chunks
            WHERE file_id IN (SELECT id FROM temp.full_rebuild_file_ids);
            DELETE FROM main.files
            WHERE id IN (SELECT id FROM temp.full_rebuild_file_ids);
            DELETE FROM temp.full_rebuild_file_ids;
            ",
        )?;
        Ok(())
    }

    pub fn index_changed(config: &Config) -> anyhow::Result<Self> {
        Self::index_changed_with_progress(config, |_| {})
    }

    pub fn index_changed_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Changed, &mut progress)
    }

    pub fn index_discover(config: &Config) -> anyhow::Result<Self> {
        Self::index_discover_with_progress(config, |_| {})
    }

    pub fn index_discover_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Discover, &mut progress)
    }

    fn index_incremental_with_progress<F>(
        config: &Config,
        mode: IndexMode,
        progress: &mut F,
    ) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        if !config.database.exists() {
            return Self::rebuild_with_progress(config, progress);
        }
        if Self::migration_check(&config.database)?.state == schema::SchemaState::Missing {
            return Self::rebuild_with_progress(config, progress);
        }

        let mut db = Self::open(&config.database)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        if db.indexed_file_count()? == 0 {
            return Self::rebuild_with_progress(config, progress);
        }
        progress(IndexProgress::Started { database: config.database.clone(), mode });
        progress(IndexProgress::IndexingGitHistory);
        let mut git_history = Some(spawn_git_history_prepare(&config.root));
        let result = (|| -> anyhow::Result<()> {
            db.storage.execute_batch("BEGIN TRANSACTION")?;
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            db.write_git_meta(&config.root)?;
            let indexed = match mode {
                IndexMode::Changed => db.index_changed_files_with_progress(config, progress)?,
                IndexMode::Discover => db.index_discovered_files_with_progress(config, progress)?,
                IndexMode::Full => unreachable!("full mode is handled by rebuild_with_progress"),
            };
            db.apply_prepared_git_history(
                &config.root,
                git_history
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("git history preparation was already used"))?,
            )?;
            if indexed > 0 {
                progress(IndexProgress::RebuildingLogicalSymbols);
                db.rebuild_logical_symbols()?;
                progress(IndexProgress::ResolvingGraph);
                db.resolve_edges()?;
                db.mark_graph_index_current()?;
                progress(IndexProgress::SyncingFts);
                db.sync_fts()?;
            }
            db.set_meta("indexed_at_ms", &now_ms().to_string())?;
            db.storage.execute_batch("COMMIT")?;
            progress(IndexProgress::Finished { files: indexed });
            Ok(())
        })();
        if result.is_err() {
            if let Some(handle) = git_history.take() {
                let _ = join_git_history_prepare(handle);
            }
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        result?;
        Ok(db)
    }

    pub fn index_targets(&self, config: &Config) -> anyhow::Result<()> {
        self.index_targets_with_progress(config, &mut |_| {})?;
        Ok(())
    }

    fn index_targets_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let files = collect_index_files(config)?;
        let changes = git_changed_paths(&config.root).unwrap_or_default();
        let files = self.assign_file_scopes(files, &changes);
        progress(IndexProgress::Discovered { files: files.len() });

        let prepared = prepare_files_with_progress(&files, progress)?;
        for (index, prepared_file) in prepared.iter().enumerate() {
            let current = index + 1;
            if should_report_file_progress(current, files.len()) {
                progress(IndexProgress::IndexingFile {
                    current,
                    total: files.len(),
                    path: prepared_file.file.relative_path.clone(),
                    language: prepared_file.file.language,
                    kind: prepared_file.file.kind,
                });
            }
            self.insert_prepared_file(prepared_file)?;
        }

        Ok(files.len())
    }

    fn index_changed_files_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let changes = git_changed_paths(&config.root)?;
        let files = collect_changed_index_files(config, &changes)?;
        let files = self.assign_file_scopes(files, &changes);
        self.apply_incremental_file_plan(files, changes.deleted, progress)
    }

    fn index_discovered_files_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let plan = discovery_plan(self.storage.connection(), config)?;
        let changes = git_changed_paths(&config.root).unwrap_or_default();
        let files = self.assign_file_scopes(plan.files, &changes);
        self.apply_incremental_file_plan(files, plan.deleted, progress)
    }

    fn assign_file_scopes(
        &self,
        files: Vec<IndexFile>,
        changes: &GitChangedPaths,
    ) -> Vec<IndexFile> {
        let has_base_commit = !self.active_commit_sha.is_empty();
        files
            .into_iter()
            .map(|mut file| {
                if !has_base_commit || changes.changed.contains(&file.relative_path) {
                    file.commit_sha.clear();
                    file.worktree_id.clone_from(&self.active_worktree_id);
                } else {
                    file.commit_sha.clone_from(&self.active_commit_sha);
                    file.worktree_id.clear();
                }
                file
            })
            .collect()
    }

    fn apply_incremental_file_plan<F>(
        &self,
        files: Vec<IndexFile>,
        deleted: BTreeSet<PathBuf>,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovered { files: files.len() });

        let deleted_count = deleted.len();
        for path in deleted {
            self.mark_file_deleted(&path)?;
        }

        let prepared = prepare_files_with_progress(&files, progress)?;
        for (index, prepared_file) in prepared.iter().enumerate() {
            let current = index + 1;
            if should_report_file_progress(current, files.len()) {
                progress(IndexProgress::IndexingFile {
                    current,
                    total: files.len(),
                    path: prepared_file.file.relative_path.clone(),
                    language: prepared_file.file.language,
                    kind: prepared_file.file.kind,
                });
            }
            self.remove_file_in_scope(
                &prepared_file.file.relative_path,
                &prepared_file.file.commit_sha,
                &prepared_file.file.worktree_id,
            )?;
            self.insert_prepared_file(prepared_file)?;
        }

        Ok(files.len() + deleted_count)
    }

    pub fn status(&self, database: &Path) -> anyhow::Result<IndexStatus> {
        let mut counts = BTreeMap::new();
        let mut stmt = self
            .storage
            .connection()
            .prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY language")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        for row in rows {
            let (language, count) = row?;
            counts.insert(language, u64::try_from(count).unwrap_or(0));
        }

        let content_revision = self.content_revision()?;
        let fts_source_revision = self.meta("fts_source_revision")?;
        let fts_dirty = self.fts_dirty()?;

        Ok(IndexStatus {
            database: database.display().to_string(),
            exists: database.exists(),
            schema: schema::status(self.storage.connection())?,
            git_commit: self.meta("git_commit")?,
            git_dirty: self.meta("git_dirty")?.map(|value| value == "true"),
            indexed_at_ms: self.meta("indexed_at_ms")?.and_then(|value| value.parse::<i64>().ok()),
            content_revision: content_revision.clone(),
            fts_synced_at_ms: self
                .meta("fts_synced_at_ms")?
                .and_then(|value| value.parse::<i64>().ok()),
            fts_dirty,
            fts_fresh: !fts_dirty
                && fts_source_revision.as_deref() == Some(content_revision.as_str()),
            fts_source_revision,
            file_count_by_language: counts,
            parser_failures: self.parser_failure_count()?,
            parser_failure_paths: self.parser_failure_paths()?,
            git_history: self.git_history_status()?,
            github: self.github_status()?,
            local_ai: self.local_ai_status()?,
        })
    }

    pub fn storage_status(&self) -> anyhow::Result<StorageStatus> {
        self.storage.status()
    }

    pub fn discovery_status(&self, config: &Config) -> anyhow::Result<DiscoveryStatus> {
        let plan = discovery_plan(self.storage.connection(), config)?;
        let unindexed_source_files =
            plan.unindexed.iter().filter(|file| file.kind == TargetKind::Source).count();
        let unindexed_sample =
            plan.unindexed.iter().take(10).map(|file| path_string(&file.relative_path)).collect();
        let warning = (unindexed_source_files > 0).then(|| {
            format!(
                "{unindexed_source_files} unindexed source files detected. Run `rag-rat index --full` or `rag-rat index --discover`."
            )
        });
        Ok(DiscoveryStatus {
            discovered_files: plan.discovered_files,
            indexed_files: plan.indexed_files,
            unindexed_files: plan.unindexed.len(),
            unindexed_source_files,
            changed_indexed_files: plan.changed.len(),
            removed_indexed_files: plan.deleted.len(),
            unindexed_sample,
            warning,
        })
    }

    pub fn search(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_with_graph_meta(query, limit, include_generated, GraphMetaMode::Compact, 3)
    }

    pub fn search_explain(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_explain_with_graph_meta(
            query,
            limit,
            include_generated,
            GraphMetaMode::Compact,
            3,
        )
    }

    pub fn search_with_graph_meta(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_with_graph_meta_options(
            query,
            limit,
            include_generated,
            graph_mode,
            graph_limit,
            SearchOptions::default(),
        )
    }

    pub fn search_with_graph_meta_options(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        let mut hits =
            self.search_with_heal(query, limit, include_generated, true, false, options)?;
        graph_meta::attach_to_search_hits(
            self.storage.connection(),
            &mut hits,
            graph_mode,
            graph_limit,
        )?;
        Ok(hits)
    }

    pub fn search_explain_with_graph_meta(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_explain_with_graph_meta_options(
            query,
            limit,
            include_generated,
            graph_mode,
            graph_limit,
            SearchOptions::default(),
        )
    }

    pub fn search_explain_with_graph_meta_options(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        let mut hits =
            self.search_with_heal(query, limit, include_generated, true, true, options)?;
        graph_meta::attach_to_search_hits(
            self.storage.connection(),
            &mut hits,
            graph_mode,
            graph_limit,
        )?;
        Ok(hits)
    }

    pub fn symbols(
        &self,
        name: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::symbol::SymbolHit>> {
        crate::query::symbol::lookup(self.storage.connection(), name, language, limit)
    }

    pub fn symbol_candidates(
        &self,
        selector: &crate::query::symbol::SymbolSelector,
    ) -> anyhow::Result<crate::query::symbol::SymbolLookup> {
        crate::query::symbol::lookup_candidates(self.storage.connection(), selector)
    }

    pub fn select_symbol(
        &self,
        selector: &crate::query::symbol::SymbolSelector,
    ) -> anyhow::Result<
        Result<Option<crate::query::symbol::SymbolHit>, crate::query::symbol::SymbolDisambiguation>,
    > {
        crate::query::symbol::select_one(self.storage.connection(), selector)
    }

    pub fn read_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        self.read_chunk_with_graph_and_memories(chunk_id, GraphMetaMode::Full, 20, true)
    }

    pub fn read_chunk_with_graph(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        self.read_chunk_with_graph_and_memories(chunk_id, graph_mode, graph_limit, false)
    }

    pub fn read_chunk_with_graph_and_memories(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        include_memories: bool,
    ) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        let Some(mut chunk) = self.read_chunk_current(chunk_id)? else {
            return Ok(None);
        };
        graph_meta::attach_to_read_chunk(
            self.storage.connection(),
            &mut chunk,
            graph_mode,
            graph_limit,
        )?;
        if include_memories {
            chunk.memories =
                crate::query::memory::memories_for_chunk(self.storage.connection(), chunk_id, 20)?;
        }
        Ok(Some(chunk))
    }

    fn read_chunk_current(&self, chunk_id: i64) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        let Some(mut chunk) = crate::query::read_chunk(self.storage.connection(), chunk_id)? else {
            return Ok(None);
        };
        let Some(root) = self.storage.source_root() else {
            return Ok(Some(chunk));
        };
        let source_path = root.join(&chunk.path);
        let current_text = match fs::read_to_string(&source_path) {
            Ok(text) => text,
            Err(_) => {
                let path = chunk.path.clone();
                self.mark_file_deleted(Path::new(&path))?;
                self.sync_fts()?;
                anyhow::bail!(IndexError::Gone { chunk_id });
            },
        };
        let anchor = self.chunk_anchor(chunk_id)?;
        let status = anchors::validate(
            &chunk.text,
            usize::try_from(chunk.start_line).unwrap_or(1),
            usize::try_from(chunk.end_line).unwrap_or(1),
            &anchor,
            &current_text,
        );
        match status {
            AnchorStatus::Exact => {
                if let Some(text) = anchors::slice_lines(
                    &current_text,
                    usize::try_from(chunk.start_line).unwrap_or(1),
                    usize::try_from(chunk.end_line).unwrap_or(1),
                ) {
                    chunk.text = text;
                }
                Ok(Some(chunk))
            },
            AnchorStatus::Relocated { start_line, end_line, text } => {
                chunk.start_line = i64::try_from(start_line)?;
                chunk.end_line = i64::try_from(end_line)?;
                chunk.text = text;
                Ok(Some(chunk))
            },
            AnchorStatus::Stale => {
                self.heal_file(Path::new(&chunk.path))?;
                self.sync_fts()?;
                let healed = crate::query::read_chunk(self.storage.connection(), chunk_id)?;
                match healed {
                    Some(chunk) => Ok(Some(chunk)),
                    None => anyhow::bail!(IndexError::StaleChunk { chunk_id, path: chunk.path }),
                }
            },
        }
    }

    pub fn search_hash_baseline(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        crate::search::lexical::search_hash_baseline(
            self.storage.connection(),
            query,
            limit,
            include_generated,
        )
    }

    pub fn docs_for_symbol(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
        self.search(symbol, limit, true)
    }

    pub fn docs_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let mut hits = self.local_symbol_context_hits(symbol, limit)?;
        hits.extend(self.search(&symbol.name, limit.saturating_mul(4).max(limit), true)?);
        rank_docs_for_symbol(symbol, &mut hits);
        dedupe_search_hits(&mut hits);
        hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(hits)
    }

    pub fn commit_search(&self, query: &str, limit: u32) -> anyhow::Result<Vec<CommitSearchHit>> {
        git_history::commit_search(self.storage.connection(), query, limit)
    }

    pub fn git_history_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<PathHistoryItem>> {
        git_history::history_for_path(self.storage.connection(), path, limit)
    }

    pub fn git_history_for_symbol(
        &self,
        symbol: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Vec<SymbolHistoryItem>> {
        let symbols = self.symbols(symbol, language, limit)?;
        let per_symbol_limit = limit.max(1);
        let mut out = Vec::new();
        for symbol_hit in symbols {
            for commit in self.git_history_for_path(&symbol_hit.path, per_symbol_limit)? {
                out.push(SymbolHistoryItem {
                    symbol: symbol_hit.name.clone(),
                    qualified_name: symbol_hit.qualified_name.clone(),
                    path: symbol_hit.path.clone(),
                    start_byte: symbol_hit.start_byte,
                    end_byte: symbol_hit.end_byte,
                    commit,
                    evidence_kind: "historical",
                });
                if out.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    pub fn commits_touching_query(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<QueryCommitHit>> {
        let current_hits = self.search(query, limit, true)?;
        git_history::commits_touching_query(self.storage.connection(), query, limit, &current_hits)
    }

    pub fn git_blame_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<ChunkBlameSummary>> {
        let Some(chunk) = self.read_chunk(chunk_id)? else {
            return Ok(None);
        };
        let source_text_hash = git_history::source_text_hash(&chunk.text);
        if let Some(cached) =
            git_history::cached_blame(self.storage.connection(), chunk_id, &source_text_hash)?
        {
            return Ok(Some(cached));
        }
        let Some(root) = self.storage.source_root() else {
            return Ok(Some(ChunkBlameSummary {
                chunk_id,
                path: chunk.path,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                source_text_hash,
                line_count: 0,
                dominant_commit: None,
                dominant_commit_lines: 0,
                newest_commit: None,
                newest_commit_time_s: None,
                oldest_commit: None,
                oldest_commit_time_s: None,
                commit_counts: BTreeMap::new(),
                evidence_kind: "historical",
            }));
        };
        let blame_lines =
            git_history::blame_lines(root, &chunk.path, chunk.start_line, chunk.end_line);
        let mut counts = BTreeMap::<String, i64>::new();
        let mut newest = None::<(String, i64)>;
        let mut oldest = None::<(String, i64)>;
        for line in &blame_lines {
            *counts.entry(line.commit.clone()).or_default() += 1;
            if let Some(time) = line.author_time_s {
                if newest.as_ref().is_none_or(|(_, newest_time)| time > *newest_time) {
                    newest = Some((line.commit.clone(), time));
                }
                if oldest.as_ref().is_none_or(|(_, oldest_time)| time < *oldest_time) {
                    oldest = Some((line.commit.clone(), time));
                }
            }
        }
        let dominant = counts
            .iter()
            .max_by_key(|(commit, count)| (*count, *commit))
            .map(|(commit, count)| (commit.clone(), *count));
        let summary = ChunkBlameSummary {
            chunk_id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            source_text_hash,
            line_count: i64::try_from(blame_lines.len()).unwrap_or(i64::MAX),
            dominant_commit: dominant.as_ref().map(|(commit, _)| commit.clone()),
            dominant_commit_lines: dominant.map(|(_, count)| count).unwrap_or(0),
            newest_commit: newest.as_ref().map(|(commit, _)| commit.clone()),
            newest_commit_time_s: newest.as_ref().map(|(_, time)| *time),
            oldest_commit: oldest.as_ref().map(|(commit, _)| commit.clone()),
            oldest_commit_time_s: oldest.as_ref().map(|(_, time)| *time),
            commit_counts: counts,
            evidence_kind: "historical",
        };
        git_history::store_blame(self.storage.connection(), &summary)?;
        Ok(Some(summary))
    }

    pub fn github_sync_from_refs(&self, offline: bool) -> anyhow::Result<GitHubSyncReport> {
        self.github_sync_from_refs_with_progress(offline, |_| {})
    }

    pub fn github_sync_from_refs_with_progress(
        &self,
        offline: bool,
        progress: impl FnMut(github::GitHubSyncProgress),
    ) -> anyhow::Result<GitHubSyncReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        if offline {
            github::sync_from_refs::<github::GhCliGitHubClient>(
                self.storage.connection(),
                root,
                None,
                true,
            )
        } else {
            let client = github::GhCliGitHubClient;
            github::sync_from_refs_with_progress(
                self.storage.connection(),
                root,
                Some(&client),
                false,
                progress,
            )
        }
    }

    pub fn github_sync_issue(
        &self,
        issue_ref: &str,
        offline: bool,
    ) -> anyhow::Result<GitHubSyncReport> {
        if offline {
            github::sync_issue::<github::GhCliGitHubClient>(
                self.storage.connection(),
                issue_ref,
                None,
                true,
            )
        } else {
            let client = github::GhCliGitHubClient;
            github::sync_issue(self.storage.connection(), issue_ref, Some(&client), false)
        }
    }

    pub fn github_issue_search(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<GitHubEvidence>> {
        github::issue_search(self.storage.connection(), query, limit)
    }

    pub fn rationale_search(&self, query: &str, limit: u32) -> anyhow::Result<Vec<GitHubEvidence>> {
        github::rationale_search(self.storage.connection(), query, limit)
    }

    pub fn github_refs_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<github::GitHubRef>> {
        github::refs_for_path(self.storage.connection(), path, limit)
    }

    pub fn github_sync_status(&self) -> anyhow::Result<GitHubStatus> {
        self.github_status()
    }

    pub fn papertrail_for_chunk(
        &self,
        chunk_id: i64,
        limit: u32,
    ) -> anyhow::Result<Option<Papertrail>> {
        let Some(chunk) = self.read_chunk(chunk_id)? else {
            return Ok(None);
        };
        Ok(Some(github::papertrail_for_chunk(self.storage.connection(), &chunk, limit)?))
    }

    pub fn papertrail_for_symbol(
        &self,
        symbol: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Option<Papertrail>> {
        let Some(symbol) = self.symbols(symbol, language, limit)?.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(github::papertrail_for_symbol(self.storage.connection(), &symbol, limit)?))
    }

    pub fn papertrail_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Papertrail> {
        github::papertrail_for_symbol(self.storage.connection(), symbol, limit)
    }

    pub fn papertrail_for_commit(
        &self,
        commit_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Papertrail> {
        github::papertrail_for_commit(self.storage.connection(), commit_hash, limit)
    }

    pub fn local_ai_status(&self) -> anyhow::Result<LocalAiStatus> {
        ai::status(self.storage.connection())
    }

    pub fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        ai::models(self.storage.connection())
    }

    pub fn install_model(&self, model_id: &str) -> anyhow::Result<ModelInfo> {
        ai::install_model(self.storage.connection(), model_id)
    }

    pub fn reconcile(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile(self.storage.connection(), limit, batch_size)
    }

    pub fn reconcile_plan(&self) -> anyhow::Result<ReconcilePlan> {
        ai::reconcile_plan(self.storage.connection())
    }

    pub fn reconcile_with_progress(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
        force: bool,
        progress: impl FnMut(ai::ReconcileProgress),
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile_with_progress(self.storage.connection(), limit, batch_size, force, progress)
    }

    pub fn reconcile_with_options_progress(
        &self,
        options: ai::ReconcileOptions,
        progress: impl FnMut(ai::ReconcileProgress),
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile_with_options_progress(self.storage.connection(), options, progress)
    }

    pub fn current_embedding_count(&self, model_id: &str) -> anyhow::Result<u64> {
        ai::current_embedding_count(self.storage.connection(), model_id)
    }

    pub fn heal_index(&self, limit: Option<u32>) -> anyhow::Result<HealIndexReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("heal_index requires source_root metadata; run `rag-rat index` first");
        };
        let indexed_files = self.indexed_files()?;
        let max_repairs = limit.map(usize::try_from).transpose()?.unwrap_or(usize::MAX);
        let mut report = HealIndexReport {
            checked_files: 0,
            healed_files: 0,
            removed_files: 0,
            skipped_files: 0,
            fts_fresh: false,
            message: None,
        };

        for file in indexed_files {
            report.checked_files += 1;
            let path = Path::new(&file.path);
            let full_path = root.join(path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                if usize::try_from(report.healed_files + report.removed_files).unwrap_or(usize::MAX)
                    >= max_repairs
                {
                    report.message =
                        Some("limit reached; rerun heal_index to continue".to_string());
                    break;
                }
                self.mark_file_deleted(path)?;
                report.removed_files += 1;
                continue;
            };
            let sha256 = hex_sha256(text.as_bytes());
            if sha256 == file.sha256 {
                report.skipped_files += 1;
                continue;
            }
            if usize::try_from(report.healed_files + report.removed_files).unwrap_or(usize::MAX)
                >= max_repairs
            {
                report.message = Some("limit reached; rerun heal_index to continue".to_string());
                break;
            }
            self.heal_file(path)?;
            report.healed_files += 1;
        }

        if report.healed_files > 0 || report.removed_files > 0 {
            self.sync_fts()?;
        } else {
            self.ensure_fts_fresh()?;
        }
        report.fts_fresh = !self.fts_dirty()?;
        Ok(report)
    }

    pub fn ffi_surface(&self, limit: u32) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::ffi_surface(self.storage.connection(), limit)
    }

    pub fn find_callers(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, true, limit)
    }

    pub fn find_callers_with_options(
        &self,
        symbol: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let options = self.graph_options_with_logical_group(options)?;
        crate::query::graph::traverse_with_options(
            self.storage.connection(),
            symbol,
            true,
            limit,
            &options,
        )
    }

    pub fn trace_callees(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, false, limit)
    }

    pub fn trace_callees_with_options(
        &self,
        symbol: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let options = self.graph_options_with_logical_group(options)?;
        crate::query::graph::traverse_with_options(
            self.storage.connection(),
            symbol,
            false,
            limit,
            &options,
        )
    }

    pub fn graph_traversal_report(
        &self,
        tool: &str,
        symbol: &crate::query::symbol::SymbolHit,
        reverse: bool,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<crate::query::graph::GraphTraversalReport> {
        let options = self.graph_options_with_logical_group(options)?;
        let results = crate::query::graph::traverse_with_options(
            self.storage.connection(),
            &symbol.qualified_name,
            reverse,
            limit,
            &options,
        )?;
        let summary = crate::query::graph::traversal_summary(
            self.storage.connection(),
            &symbol.qualified_name,
            reverse,
            limit,
            &options,
            results.len(),
        )?;
        let (logical_symbol, variants) = self.graph_logical_symbol(options.logical_symbol_id)?;
        let mut paths = BTreeSet::new();
        paths.insert(symbol.path.clone());
        for result in &results {
            if let Some(callsite) = &result.callsite {
                paths.insert(callsite.path.clone());
            }
        }
        let mut coverage = self.graph_coverage(paths)?;
        if summary.unresolved > 0 {
            coverage.known_index_gaps.push(format!(
                "{} unresolved qualified callsites match the requested final segment but are not verified to this symbol",
                summary.unresolved
            ));
        }
        Ok(crate::query::graph::GraphTraversalReport {
            query: crate::query::graph::GraphTraversalQuery {
                tool: tool.to_string(),
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: options.logical_symbol_id,
                symbol_path: symbol.qualified_name.clone(),
                resolution: options.resolution_mode.as_str().to_string(),
            },
            logical_symbol,
            variants,
            summary,
            coverage,
            results,
        })
    }

    pub fn compare_graph_to_text(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        pattern: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
        include_tests: bool,
    ) -> anyhow::Result<crate::query::graph::CompareGraphTextReport> {
        let regex = Regex::new(pattern)?;
        let options = self.graph_options_with_logical_group(options)?;
        let mut graph_edges = crate::query::graph::traverse_with_options(
            self.storage.connection(),
            &symbol.qualified_name,
            true,
            limit,
            &options,
        )?;
        if !include_tests {
            graph_edges.retain(|edge| {
                edge.callsite.as_ref().is_none_or(|callsite| !is_test_like_path(&callsite.path))
            });
        }
        let (logical_symbol, variants) = self.graph_logical_symbol(options.logical_symbol_id)?;
        let text_hits = self.regex_hits(pattern, &regex, include_tests)?;
        let text_by_location = text_hits
            .iter()
            .map(|hit| ((hit.path.clone(), hit.line), hit))
            .collect::<BTreeMap<_, _>>();
        let graph_by_location = graph_edges
            .iter()
            .filter_map(|edge| {
                edge.callsite
                    .as_ref()
                    .map(|callsite| ((callsite.path.clone(), callsite.line), edge))
            })
            .collect::<BTreeMap<_, _>>();

        let mut paths = BTreeSet::new();
        paths.insert(symbol.path.clone());
        for hit in &text_hits {
            paths.insert(hit.path.clone());
        }
        for edge in &graph_edges {
            if let Some(callsite) = &edge.callsite {
                paths.insert(callsite.path.clone());
            }
        }

        let parser_failure_paths = self
            .parser_failure_paths()?
            .into_iter()
            .map(|failure| failure.path)
            .collect::<BTreeSet<_>>();
        let mut matched_hits = Vec::new();
        let mut text_only_hits = Vec::new();
        let mut likely_parser_gaps = Vec::new();
        for hit in &text_hits {
            if let Some(edge) = graph_by_location.get(&(hit.path.clone(), hit.line)) {
                matched_hits.push(crate::query::graph::MatchedGraphTextHit {
                    path: hit.path.clone(),
                    line: hit.line,
                    text: hit.text.clone(),
                    target: edge.target.clone(),
                    edge_kind: edge.edge_kind.clone(),
                    confidence: edge.confidence.clone(),
                    resolution: edge.resolution.clone(),
                });
            } else {
                let gap_kind = classify_text_only_hit(&hit.path, &hit.text, &parser_failure_paths);
                let text_only_hit = crate::query::graph::TextOnlyHit {
                    path: hit.path.clone(),
                    line: hit.line,
                    text: hit.text.clone(),
                    reason: if gap_kind == "parser_call_extraction" || gap_kind == "parser_failure"
                    {
                        "no graph edge extracted"
                    } else {
                        "text mention outside graph-call evidence"
                    }
                    .to_string(),
                    likely_gap: gap_kind.to_string(),
                };
                if is_likely_parser_gap_kind(gap_kind) {
                    likely_parser_gaps.push(text_only_hit.clone());
                }
                text_only_hits.push(text_only_hit);
            }
        }

        let mut graph_only_edges = Vec::new();
        let mut likely_false_positives = Vec::new();
        for edge in &graph_edges {
            let Some(callsite) = &edge.callsite else {
                continue;
            };
            if text_by_location.contains_key(&(callsite.path.clone(), callsite.line)) {
                continue;
            }
            let current_line = self.current_line_text(&callsite.path, callsite.line)?;
            let graph_only = crate::query::graph::GraphOnlyEdge {
                path: callsite.path.clone(),
                line: callsite.line,
                target: edge.target.clone(),
                edge_kind: edge.edge_kind.clone(),
                confidence: edge.confidence.clone(),
                resolution: edge.resolution.clone(),
                evidence: edge.evidence.clone(),
                reason: "graph edge exists but pattern did not match text".to_string(),
                likely_reason: graph_only_reason(edge, current_line.as_deref()),
            };
            if is_likely_false_positive_graph_only(edge, &graph_only) {
                likely_false_positives.push(graph_only.clone());
            }
            graph_only_edges.push(graph_only);
        }
        let complete = likely_parser_gaps.is_empty() && likely_false_positives.is_empty();
        let recommended_fallback =
            recommended_graph_text_fallback(&likely_parser_gaps, &graph_only_edges);
        let pattern_match_mode = compare_pattern_match_mode(pattern, &symbol.name);
        let mut warnings = Vec::new();
        if pattern_match_mode == "substring_identifier" {
            warnings.push(format!(
                "pattern may match identifiers that merely contain `{}`; use an identifier boundary or escaped call suffix for exact text auditing",
                symbol.name
            ));
        }

        Ok(crate::query::graph::CompareGraphTextReport {
            query: crate::query::graph::CompareGraphTextQuery {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: options.logical_symbol_id,
                symbol_path: symbol.qualified_name.clone(),
                pattern: pattern.to_string(),
                resolution: options.resolution_mode.as_str().to_string(),
                include_tests,
            },
            logical_symbol,
            variants,
            summary: crate::query::graph::CompareGraphTextSummary {
                graph_hits: u64::try_from(graph_edges.len()).unwrap_or(u64::MAX),
                graph_edges: u64::try_from(graph_edges.len()).unwrap_or(u64::MAX),
                text_hits: u64::try_from(text_hits.len()).unwrap_or(u64::MAX),
                matched: u64::try_from(matched_hits.len()).unwrap_or(u64::MAX),
                graph_only: u64::try_from(graph_only_edges.len()).unwrap_or(u64::MAX),
                text_only: u64::try_from(text_only_hits.len()).unwrap_or(u64::MAX),
                text_mentions: u64::try_from(text_only_hits.len() - likely_parser_gaps.len())
                    .unwrap_or(u64::MAX),
                likely_parser_gaps: u64::try_from(likely_parser_gaps.len()).unwrap_or(u64::MAX),
                likely_false_positives: u64::try_from(likely_false_positives.len())
                    .unwrap_or(u64::MAX),
                likely_index_gaps: u64::try_from(likely_parser_gaps.len()).unwrap_or(u64::MAX),
                complete,
                recommended_fallback,
                pattern_match_mode,
                warnings,
            },
            coverage: self.graph_coverage(paths)?,
            matched_hits,
            text_only_hits,
            graph_only_edges,
            likely_parser_gaps,
            likely_false_positives,
        })
    }

    fn graph_logical_symbol(
        &self,
        logical_symbol_id: Option<i64>,
    ) -> anyhow::Result<(
        Option<crate::query::graph::LogicalSymbol>,
        Vec<crate::query::graph::LogicalSymbolVariant>,
    )> {
        let Some(logical_symbol_id) = logical_symbol_id else {
            return Ok((None, Vec::new()));
        };
        let Some(logical) = crate::query::symbol::lookup_logical_by_id(
            self.storage.connection(),
            logical_symbol_id,
        )?
        else {
            return Ok((None, Vec::new()));
        };
        let variants = crate::query::symbol::logical_members(
            self.storage.connection(),
            logical.logical_symbol_id,
        )?
        .into_iter()
        .map(|member| crate::query::graph::LogicalSymbolVariant {
            symbol_id: member.symbol_id,
            cfg_expr: member.cfg_expr,
            signature_hash: member.signature_hash,
            start_line: member.start_line,
            end_line: member.end_line,
        })
        .collect::<Vec<_>>();
        Ok((
            Some(crate::query::graph::LogicalSymbol {
                logical_symbol_id: logical.logical_symbol_id,
                qualified_name: logical.qualified_name,
                variant_count: logical.variant_count,
                group_reason: logical.group_reason,
            }),
            variants,
        ))
    }

    fn graph_options_with_logical_group(
        &self,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<crate::query::graph::GraphTraversalOptions> {
        if options.logical_symbol_id.is_some() {
            return Ok(options.clone());
        }
        let Some(symbol_id) = options.symbol_id else {
            return Ok(options.clone());
        };
        let Some(logical) =
            crate::query::symbol::logical_for_symbol_id(self.storage.connection(), symbol_id)?
        else {
            return Ok(options.clone());
        };
        let mut options = options.clone();
        options.logical_symbol_id = Some(logical.logical_symbol_id);
        Ok(options)
    }

    fn local_symbol_context_hits(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let mut stmt = self.storage.connection().prepare(
            "
            SELECT chunks.id, files.path, files.language, files.kind,
                   chunks.start_line, chunks.end_line, chunks.symbol_path, chunks.text
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path = ?1
              AND (
                chunks.symbol_path = ?2
                OR chunks.symbol_path LIKE ?3
                OR chunks.text LIKE ?4
              )
            ORDER BY
              CASE
                WHEN chunks.symbol_path = ?2 THEN 0
                WHEN chunks.symbol_path LIKE ?3 THEN 1
                ELSE 2
              END,
              chunks.start_line
            LIMIT ?5
            ",
        )?;
        let rows = stmt.query_map(
            params![
                symbol.path,
                symbol.qualified_name,
                format!("%{}%", symbol.name),
                format!("%{}%", symbol.name),
                i64::from(limit.max(1)),
            ],
            |row| {
                let text: String = row.get(7)?;
                Ok(SearchHit {
                    chunk_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    kind: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    symbol_path: row.get(6)?,
                    score: 1.0,
                    summary: bounded_summary(&text),
                    graph: None,
                    score_components: None,
                })
            },
        )?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    pub fn impact_surface(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface(self.storage.connection(), query, limit)
    }

    pub fn impact_surface_with_options(
        &self,
        query: &str,
        limit: u32,
        resolution_mode: crate::query::graph::GraphResolutionMode,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface_with_options(
            self.storage.connection(),
            query,
            limit,
            resolution_mode,
        )
    }

    pub fn impact_surface_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        resolution_mode: crate::query::graph::GraphResolutionMode,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface_for_symbol(
            self.storage.connection(),
            symbol,
            limit,
            resolution_mode,
        )
    }

    pub fn impact_surface_report_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        options: &crate::query::impact::ImpactSurfaceOptions,
    ) -> anyhow::Result<crate::query::impact::ImpactSurfaceReport> {
        crate::query::impact::impact_surface_report_for_symbol(
            self.storage.connection(),
            symbol,
            limit,
            options,
        )
    }

    pub fn repo_brief(
        &self,
        options: crate::query::repo_brief::RepoBriefOptions,
    ) -> anyhow::Result<crate::query::repo_brief::RepoBrief> {
        crate::query::repo_brief::repo_brief(self.storage.connection(), options)
    }

    pub fn repo_clusters(
        &self,
        options: crate::query::clusters::RepoClustersOptions,
    ) -> anyhow::Result<crate::query::clusters::RepoClustersReport> {
        crate::query::clusters::repo_clusters(self.storage.connection(), options)
    }

    pub fn memory_create(
        &self,
        request: crate::query::memory::RepoMemoryCreate,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryCreateResult> {
        crate::query::memory::create_memory(self.storage.connection(), request)
    }

    pub fn memory_update(
        &self,
        update: crate::query::memory::RepoMemoryUpdate,
    ) -> anyhow::Result<crate::query::memory::RepoMemory> {
        crate::query::memory::update_memory(self.storage.connection(), update)
    }

    pub fn memory_mark_obsolete(
        &self,
        memory_id: &str,
    ) -> anyhow::Result<crate::query::memory::RepoMemory> {
        crate::query::memory::mark_obsolete(self.storage.connection(), memory_id)
    }

    pub fn memory_search(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memory_search(self.storage.connection(), query, limit)
    }

    pub fn memory_for_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_symbol(self.storage.connection(), symbol, limit)
    }

    pub fn memory_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_path(self.storage.connection(), path, limit)
    }

    pub fn memory_for_edges(
        &self,
        edge_ids: &[i64],
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_edges(self.storage.connection(), edge_ids, limit)
    }

    pub fn memory_evidence_for_symbol_and_edges(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        edge_ids: &[i64],
        limit: u32,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryEvidence> {
        crate::query::memory::memory_evidence_for_symbol_and_edges(
            self.storage.connection(),
            symbol,
            edge_ids,
            limit,
        )
    }

    pub fn memory_for_call_path_hash(
        &self,
        edge_sequence_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_call_path_hash(
            self.storage.connection(),
            edge_sequence_hash,
            limit,
        )
    }

    pub fn memory_validate(
        &self,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryValidationReport> {
        crate::query::memory::validate_memories(self.storage.connection())
    }

    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        schema::rebuild_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    pub fn sync_fts(&self) -> anyhow::Result<()> {
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    fn record_fts_current(&self) -> anyhow::Result<()> {
        self.set_meta("fts_synced_at_ms", &now_ms().to_string())?;
        let revision = self.content_revision()?;
        self.set_meta("fts_source_revision", &revision)?;
        Ok(())
    }

    fn record_content_revision(&self) -> anyhow::Result<String> {
        let revision = self.content_revision()?;
        self.set_meta("content_revision", &revision)?;
        Ok(revision)
    }

    pub fn heal_file(&self, path: &Path) -> anyhow::Result<()> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        let row = self.file_row(path)?;
        let full_path = root.join(path);
        let text = fs::read_to_string(&full_path)?;

        let changes = git_changed_paths(root).unwrap_or_default();
        let is_dirty = changes.changed.contains(path);
        let has_base_commit = !self.active_commit_sha.is_empty();
        let scope = if !has_base_commit || is_dirty {
            FileScope::worktree(self.active_worktree_id.clone())
        } else {
            FileScope::commit(self.active_commit_sha.clone())
        };
        self.remove_file_in_scope(path, &scope.commit_sha, &scope.worktree_id)?;

        self.index_file(
            path,
            row.language,
            row.kind,
            file_metadata_ms(&full_path)?,
            &text,
            &scope,
        )?;
        self.rebuild_logical_symbols()?;
        self.resolve_edges()
    }

    fn index_file(
        &self,
        path: &Path,
        language: Language,
        kind: TargetKind,
        modified_at_ms: i64,
        text: &str,
        scope: &FileScope,
    ) -> anyhow::Result<()> {
        if language != Language::Markdown && kind != TargetKind::Generated {
            if text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                // Large source files are intentionally coarse-indexed to keep full-repo indexing
                // responsive. This is not a parser failure.
            } else if let Some(message) = parser::parse_error(path, language, text)
                .unwrap_or_else(|err| Some(err.to_string()))
            {
                self.insert_parser_failure(path, language, &message)?;
            }
        }
        let sha256 = hex_sha256(text.as_bytes());
        let file_id = self.storage.connection().query_row(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             RETURNING id",
            params![
                path_string(path),
                language.as_str(),
                kind.as_str(),
                sha256,
                modified_at_ms,
                matches!(kind, TargetKind::Generated),
                now_ms(),
                sha256,
                &scope.commit_sha,
                &scope.worktree_id,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        let chunks = if kind == TargetKind::Generated {
            chunker::generated_chunks_for_file(path, text)
        } else {
            chunker::chunks_for_file(path, language, text)
        };
        let symbols =
            if kind == TargetKind::Generated || text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                Vec::new()
            } else {
                symbols::symbols_for_file(path, language, text)
            };
        self.insert_chunks(file_id, &sha256, &chunks, text)?;
        self.insert_symbols(file_id, language, &symbols)?;
        if kind != TargetKind::Generated && text.len() <= edges::MAX_GRAPH_PARSE_BYTES {
            edges::index_file_edges(self.storage.connection(), file_id, path, language, text)?;
        }
        self.mark_fts_dirty()?;
        Ok(())
    }

    fn insert_prepared_file(&self, prepared_file: &PreparedIndexFile) -> anyhow::Result<()> {
        let file = &prepared_file.file;
        let prepared = match &prepared_file.prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                self.insert_parser_failure(&file.relative_path, file.language, &err.to_string())?;
                return Ok(());
            },
        };
        if let Some(message) = &prepared.parser_failure {
            self.insert_parser_failure(&file.relative_path, file.language, message)?;
        }
        let file_id = self.storage.connection().query_row(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             RETURNING id",
            params![
                path_string(&file.relative_path),
                file.language.as_str(),
                file.kind.as_str(),
                prepared.sha256,
                prepared.modified_at_ms,
                matches!(file.kind, TargetKind::Generated),
                now_ms(),
                prepared.sha256,
                file.commit_sha,
                file.worktree_id,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        self.insert_chunks(file_id, &prepared.sha256, &prepared.chunks, &prepared.text)?;
        self.insert_symbols(file_id, file.language, &prepared.symbols)?;
        if file.kind != TargetKind::Generated && prepared.text.len() <= edges::MAX_GRAPH_PARSE_BYTES
        {
            edges::index_file_edges(
                self.storage.connection(),
                file_id,
                &file.relative_path,
                file.language,
                &prepared.text,
            )?;
        }
        self.mark_fts_dirty()?;
        Ok(())
    }

    fn insert_chunks(
        &self,
        file_id: i64,
        source_revision: &str,
        chunks: &[Chunk],
        full_text: &str,
    ) -> anyhow::Result<()> {
        let (path, language, kind) = self.storage.connection().query_row(
            "SELECT path, language, kind FROM main.files WHERE id = ?1",
            [file_id],
            |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            },
        )?;
        for chunk in chunks {
            let anchor =
                anchors::anchor_for_text(&chunk.text, chunk.start_line, chunk.end_line, full_text);
            let embedding_policy = ai::embedding_policy_for_chunk(
                Path::new(&path),
                &language,
                &kind,
                chunk.kind,
                chunk.symbol_path.as_deref(),
                &chunk.text,
                ai::DEFAULT_MAX_EMBEDDING_CHARS,
            );
            self.storage.connection().execute(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line, text, text_hash,
                                    source_revision, anchor_version, normalized_hash, start_boundary_hash, end_boundary_hash,
                                    start_context_hash, end_context_hash, context_radius, embedding_policy, embedding_priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    file_id,
                    chunk.kind,
                    chunk.symbol_path,
                    i64::try_from(chunk.start_byte)?,
                    i64::try_from(chunk.end_byte)?,
                    i64::try_from(chunk.start_line)?,
                    i64::try_from(chunk.end_line)?,
                    chunk.text,
                    hex_sha256(chunk.text.as_bytes()),
                    source_revision,
                    anchor.version,
                    anchor.normalized_hash,
                    anchor.start_boundary_hash,
                    anchor.end_boundary_hash,
                    anchor.start_context_hash,
                    anchor.end_context_hash,
                    anchor.context_radius,
                    embedding_policy.policy,
                    embedding_policy.priority,
                ],
            )?;
            let chunk_id = self.storage.connection().last_insert_rowid();
            self.storage.connection().execute(
                "INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)",
                params![chunk_id, chunk.text],
            )?;
        }
        Ok(())
    }

    fn insert_symbols(
        &self,
        file_id: i64,
        language: Language,
        symbols: &[Symbol],
    ) -> anyhow::Result<()> {
        for symbol in symbols {
            self.storage.connection().execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, end_byte, signature, docs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    file_id,
                    language.as_str(),
                    symbol.name,
                    symbol.qualified_name,
                    symbol.kind,
                    i64::try_from(symbol.start_byte)?,
                    i64::try_from(symbol.end_byte)?,
                    symbol.signature,
                    symbol.docs,
                ],
            )?;
            let symbol_id = self.storage.connection().last_insert_rowid();
            for fact in &symbol.facts {
                self.storage.connection().execute(
                    "INSERT OR IGNORE INTO symbol_facts(symbol_id, fact_kind, fact_value)
                     VALUES (?1, ?2, ?3)",
                    params![symbol_id, fact.kind, fact.value],
                )?;
            }
        }
        Ok(())
    }

    fn write_git_meta(&self, root: &Path) -> anyhow::Result<()> {
        self.set_meta("git_commit", &git_output(root, &["rev-parse", "HEAD"]).unwrap_or_default())?;
        let dirty = !git_output(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        self.set_meta("git_dirty", if dirty { "true" } else { "false" })?;
        Ok(())
    }

    fn apply_prepared_git_history(
        &self,
        root: &Path,
        handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
    ) -> anyhow::Result<GitHistoryIndexStatus> {
        let prepared = join_git_history_prepare(handle)?;
        git_history::apply_prepared(self.storage.connection(), root, prepared)
    }

    fn git_history_status(&self) -> anyhow::Result<GitHistoryIndexStatus> {
        let Some(root) = self.storage.source_root() else {
            return git_history::status(self.storage.connection(), Path::new("."));
        };
        git_history::status(self.storage.connection(), root)
    }

    fn github_status(&self) -> anyhow::Result<GitHubStatus> {
        github::status(self.storage.connection())
    }

    fn mark_fts_dirty(&self) -> anyhow::Result<()> {
        self.set_meta("fts_dirty", "true")
    }

    fn resolve_edges(&self) -> anyhow::Result<()> {
        edges::resolve_all_edges(self.storage.connection())
    }

    fn rebuild_logical_symbols(&self) -> anyhow::Result<()> {
        self.storage.connection().execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS logical_symbols_to_rebuild(id INTEGER PRIMARY KEY);
            DELETE FROM temp.logical_symbols_to_rebuild;
            INSERT OR IGNORE INTO temp.logical_symbols_to_rebuild(id)
            SELECT logical_symbol_members.logical_symbol_id
            FROM main.logical_symbol_members
            JOIN main.symbols ON symbols.id = logical_symbol_members.symbol_id
            JOIN files ON files.id = symbols.file_id;
            DELETE FROM main.logical_symbol_members
            WHERE logical_symbol_id IN (
                SELECT id FROM temp.logical_symbols_to_rebuild
            );
            DELETE FROM main.logical_symbols
            WHERE id IN (
                SELECT id FROM temp.logical_symbols_to_rebuild
            );
            DELETE FROM temp.logical_symbols_to_rebuild;
            ",
        )?;

        let mut stmt = self.storage.connection().prepare(
            "
            SELECT symbols.id, symbols.file_id, files.path, symbols.language, symbols.name,
                   symbols.qualified_name, symbols.kind, symbols.start_byte, symbols.end_byte,
                   symbols.signature,
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
            JOIN files ON files.id = symbols.file_id
            ORDER BY files.path, symbols.language, symbols.qualified_name, symbols.kind,
                     symbols.start_byte, symbols.end_byte
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            let start_byte = usize::try_from(row.get::<_, i64>(7)?).unwrap_or(0);
            let end_byte = usize::try_from(row.get::<_, i64>(8)?).unwrap_or(0);
            let chunk_start_byte = usize::try_from(row.get::<_, i64>(10)?).unwrap_or(start_byte);
            let chunk_start_line = row.get::<_, i64>(11)?;
            let chunk_text: String = row.get(12)?;
            let start_line =
                symbol_line_for_byte(&chunk_text, chunk_start_byte, chunk_start_line, start_byte);
            let end_line =
                symbol_line_for_byte(&chunk_text, chunk_start_byte, chunk_start_line, end_byte);
            Ok(LogicalSymbolMemberRow {
                symbol_id: row.get(0)?,
                path: row.get(2)?,
                language: row.get(3)?,
                name: row.get(4)?,
                qualified_name: row.get(5)?,
                kind: row.get(6)?,
                signature: row.get(9)?,
                start_line,
                end_line,
            })
        })?;
        let mut groups: BTreeMap<LogicalSymbolKey, Vec<LogicalSymbolMemberRow>> = BTreeMap::new();
        for row in rows {
            let row = row?;
            groups.entry(LogicalSymbolKey::from(&row)).or_default().push(row);
        }
        for (key, members) in groups {
            let group_reason = if members.len() > 1 { "cfg_variant" } else { "single" };
            self.storage.connection().execute(
                "
                INSERT INTO logical_symbols(language, path, logical_name, qualified_name, kind, variant_count, group_reason)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
                params![
                    key.language,
                    key.path,
                    key.name,
                    key.qualified_name,
                    key.kind,
                    i64::try_from(members.len()).unwrap_or(i64::MAX),
                    group_reason,
                ],
            )?;
            let logical_symbol_id = self.storage.connection().last_insert_rowid();
            for member in members {
                let signature_hash =
                    member.signature.as_deref().map(|signature| hex_sha256(signature.as_bytes()));
                self.storage.connection().execute(
                    "
                    INSERT INTO logical_symbol_members(
                        logical_symbol_id, symbol_id, cfg_expr, signature_hash, start_line, end_line
                    )
                    VALUES (?1, ?2, NULL, ?3, ?4, ?5)
                    ",
                    params![
                        logical_symbol_id,
                        member.symbol_id,
                        signature_hash,
                        member.start_line,
                        member.end_line,
                    ],
                )?;
            }
        }
        Ok(())
    }

    fn graph_coverage(
        &self,
        paths: BTreeSet<String>,
    ) -> anyhow::Result<crate::query::graph::GraphCoverage> {
        let indexed_files =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        let parser_failure_paths = self.parser_failure_paths()?;
        let parser_failures = u64::try_from(parser_failure_paths.len()).unwrap_or(0);
        let known_index_gaps = parser_failure_paths
            .iter()
            .map(|failure| {
                format!(
                    "{} parser failed for {}: {}",
                    failure.language, failure.path, failure.message
                )
            })
            .collect::<Vec<_>>();
        let mut stale_files = 0_u64;
        let mut parser_coverage_for_paths = Vec::new();
        for path in paths {
            let Some(row) = self.graph_path_row(&path)? else {
                parser_coverage_for_paths.push(crate::query::graph::GraphPathCoverage {
                    path,
                    language: "unknown".to_string(),
                    parser_status: "missing_from_index".to_string(),
                    graph_status: "missing_from_index".to_string(),
                    last_indexed_revision: None,
                });
                continue;
            };
            let stale = self.source_path_is_stale(&path, &row.sha256);
            if stale {
                stale_files += 1;
            }
            let parser_failed = parser_failure_paths.iter().any(|failure| failure.path == path);
            parser_coverage_for_paths.push(crate::query::graph::GraphPathCoverage {
                path,
                language: row.language,
                parser_status: if parser_failed { "failed" } else { "ok" }.to_string(),
                graph_status: if stale {
                    "stale_source"
                } else if parser_failed {
                    "parser_failed"
                } else {
                    "ok"
                }
                .to_string(),
                last_indexed_revision: (!row.indexed_revision.is_empty())
                    .then_some(row.indexed_revision),
            });
        }
        Ok(crate::query::graph::GraphCoverage {
            indexed_files: u64::try_from(indexed_files).unwrap_or(0),
            parser_failures,
            stale_files,
            known_index_gaps,
            parser_coverage_for_paths,
        })
    }

    fn graph_path_row(&self, path: &str) -> anyhow::Result<Option<GraphPathRow>> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, sha256, indexed_revision FROM files WHERE path = ?1",
                [path],
                |row| {
                    Ok(GraphPathRow {
                        language: row.get(0)?,
                        sha256: row.get(1)?,
                        indexed_revision: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn source_path_is_stale(&self, path: &str, indexed_sha256: &str) -> bool {
        let Some(root) = self.storage.source_root() else {
            return false;
        };
        let Ok(bytes) = fs::read(root.join(path)) else {
            return true;
        };
        hex_sha256(&bytes) != indexed_sha256
    }

    fn regex_hits(
        &self,
        pattern: &str,
        regex: &Regex,
        include_tests: bool,
    ) -> anyhow::Result<Vec<crate::query::graph::TextOnlyHit>> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("cannot compare graph to text: source_root is missing from index_meta");
        };
        let mut stmt = self.storage.connection().prepare("SELECT path FROM files ORDER BY path")?;
        let paths =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut hits = Vec::new();
        for path in paths {
            if !include_tests && is_test_like_path(&path) {
                continue;
            }
            let full_path = root.join(&path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    hits.push(crate::query::graph::TextOnlyHit {
                        path: path.clone(),
                        line: i64::try_from(index + 1).unwrap_or(i64::MAX),
                        text: line.trim().to_string(),
                        reason: "text pattern matched".to_string(),
                        likely_gap: pattern.to_string(),
                    });
                }
            }
        }
        Ok(hits)
    }

    fn current_line_text(&self, path: &str, line: i64) -> anyhow::Result<Option<String>> {
        let Some(root) = self.storage.source_root() else {
            return Ok(None);
        };
        let Ok(text) = fs::read_to_string(root.join(path)) else {
            return Ok(None);
        };
        let Some(index) = usize::try_from(line.saturating_sub(1)).ok() else {
            return Ok(None);
        };
        Ok(text.lines().nth(index).map(|line| line.trim().to_string()))
    }

    fn ensure_graph_index_current(&self) -> anyhow::Result<()> {
        if self.meta("graph_index_version")?.as_deref() == Some(GRAPH_INDEX_VERSION) {
            return Ok(());
        }
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| -> anyhow::Result<()> {
            self.storage.connection().execute("DELETE FROM edges", [])?;
            let files = self.graph_reindex_files()?;
            for file in files {
                if file.kind == TargetKind::Generated || file.language == Language::Markdown {
                    continue;
                }
                let full_path = root.join(&file.path);
                let Ok(text) = fs::read_to_string(full_path) else {
                    continue;
                };
                if text.len() > edges::MAX_GRAPH_PARSE_BYTES {
                    continue;
                }
                edges::index_file_edges(
                    self.storage.connection(),
                    file.id,
                    Path::new(&file.path),
                    file.language,
                    &text,
                )?;
            }
            self.resolve_edges()?;
            self.mark_graph_index_current()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
        }
        result?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    fn mark_graph_index_current(&self) -> anyhow::Result<()> {
        self.set_meta("graph_index_version", GRAPH_INDEX_VERSION)
    }

    fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        meta_for(self.storage.connection(), key)
    }

    fn insert_parser_failure(
        &self,
        path: &Path,
        language: Language,
        message: &str,
    ) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO parser_failures(path, language, message) VALUES (?1, ?2, ?3)",
            params![path_string(path), language.as_str(), message],
        )?;
        Ok(())
    }

    fn parser_failure_count(&self) -> anyhow::Result<u64> {
        let count = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM parser_failures",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    fn parser_failure_paths(&self) -> anyhow::Result<Vec<ParserFailure>> {
        let mut stmt = self.storage.connection().prepare(
            "SELECT path, language, message FROM parser_failures ORDER BY path, language, message",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ParserFailure { path: row.get(0)?, language: row.get(1)?, message: row.get(2)? })
        })?;
        let mut failures = Vec::new();
        for row in rows {
            failures.push(row?);
        }
        Ok(failures)
    }

    fn search_with_heal(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        allow_heal: bool,
        explain: bool,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let hits = crate::search::lexical::search_with_options(
            self.storage.connection(),
            query,
            limit,
            include_generated,
            explain,
            options,
        )?;
        if !allow_heal {
            return Ok(hits);
        }
        let stale = self.stale_hit_paths(&hits)?;
        if stale.is_empty() {
            return Ok(hits);
        }
        if stale.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
            anyhow::bail!(IndexError::NeedsReindex {
                stale_files: stale.len(),
                cap: MAX_AUTO_HEAL_FILES_PER_CALL,
            });
        }
        for path in stale {
            self.heal_file(Path::new(&path))?;
        }
        self.sync_fts()?;
        self.search_with_heal(query, limit, include_generated, false, explain, options)
    }

    fn stale_hit_paths(&self, hits: &[SearchHit]) -> anyhow::Result<Vec<String>> {
        let Some(root) = self.storage.source_root() else {
            return Ok(Vec::new());
        };
        let mut stale = Vec::new();
        let mut seen = BTreeSet::new();
        for hit in hits {
            if !seen.insert(hit.path.clone()) {
                continue;
            }
            let source_path = root.join(&hit.path);
            let Ok(text) = fs::read_to_string(source_path) else {
                stale.push(hit.path.clone());
                continue;
            };
            let chunk = crate::query::read_chunk(self.storage.connection(), hit.chunk_id)?;
            let Some(chunk) = chunk else {
                stale.push(hit.path.clone());
                continue;
            };
            let anchor = self.chunk_anchor(hit.chunk_id)?;
            let status = anchors::validate(
                &chunk.text,
                usize::try_from(chunk.start_line).unwrap_or(1),
                usize::try_from(chunk.end_line).unwrap_or(1),
                &anchor,
                &text,
            );
            if !matches!(status, AnchorStatus::Exact) {
                stale.push(hit.path.clone());
            }
        }
        Ok(stale)
    }

    fn chunk_anchor(&self, chunk_id: i64) -> anyhow::Result<ChunkAnchor> {
        Ok(self.storage.connection().query_row(
            "
            SELECT anchor_version, normalized_hash, start_boundary_hash, end_boundary_hash,
                   start_context_hash, end_context_hash, context_radius
            FROM chunks WHERE id = ?1
            ",
            [chunk_id],
            |row| {
                Ok(ChunkAnchor {
                    version: row.get(0)?,
                    normalized_hash: row.get(1)?,
                    start_boundary_hash: row.get(2)?,
                    end_boundary_hash: row.get(3)?,
                    start_context_hash: row.get(4)?,
                    end_context_hash: row.get(5)?,
                    context_radius: row.get(6)?,
                })
            },
        )?)
    }

    fn mark_file_deleted(&self, path: &Path) -> anyhow::Result<()> {
        let path = path_string(path);
        self.remove_file_in_scope(Path::new(&path), "", &self.active_worktree_id)?;
        self.storage.connection().execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, 'unknown', 'deleted', '', 0, 0, ?2, '', '', ?3)
             ON CONFLICT(path, commit_sha, worktree_id) DO UPDATE SET
                kind = 'deleted',
                sha256 = '',
                modified_at_ms = 0,
                indexed_at_ms = excluded.indexed_at_ms",
            params![path, now_ms(), self.active_worktree_id],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    fn remove_file_in_scope(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<()> {
        let path = path_string(path);
        self.storage.connection().execute(
            "UPDATE edges
             SET to_symbol_id = NULL,
                 confidence = 'NameOnly'
             WHERE to_symbol_id IN (
                 SELECT symbols.id FROM symbols
                 JOIN main.files ON main.files.id = symbols.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM edges
             WHERE source_file_id IN (
                    SELECT id FROM main.files
                    WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
                )
                OR from_symbol_id IN (
                    SELECT symbols.id FROM symbols
                    JOIN main.files ON main.files.id = symbols.file_id
                    WHERE main.files.path = ?1
                      AND main.files.commit_sha = ?2
                      AND main.files.worktree_id = ?3
                )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage
            .connection()
            .execute("DELETE FROM parser_failures WHERE path = ?1", [&path])?;
        self.storage.connection().execute(
            "DELETE FROM chunk_fts
             WHERE rowid IN (
                 SELECT chunks.id FROM chunks
                 JOIN main.files ON main.files.id = chunks.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM chunks
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM symbols
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM main.files WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3",
            params![path, commit_sha, worktree_id],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    fn ensure_fts_fresh(&self) -> anyhow::Result<()> {
        let content_revision = self.content_revision()?;
        let fts_source_revision = self.meta("fts_source_revision")?;
        if !self.fts_dirty()? && fts_source_revision.as_deref() == Some(content_revision.as_str()) {
            return Ok(());
        }
        self.rebuild_fts()?;
        let refreshed_revision = self.meta("fts_source_revision")?;
        if refreshed_revision.as_deref() != Some(content_revision.as_str()) {
            anyhow::bail!(
                "FTS freshness invariant failed: content_revision={content_revision}, fts_source_revision={}",
                refreshed_revision.unwrap_or_else(|| "<missing>".to_string())
            );
        }
        Ok(())
    }

    fn fts_dirty(&self) -> anyhow::Result<bool> {
        Ok(self.meta("fts_dirty")?.as_deref() == Some("true"))
    }

    fn file_row(&self, path: &Path) -> anyhow::Result<FileRow> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, kind FROM files WHERE path = ?1",
                [path_string(path)],
                |row| {
                    let language: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    Ok((language, kind))
                },
            )
            .map_err(Into::into)
            .and_then(|(language, kind)| {
                Ok(FileRow { language: language.parse()?, kind: kind.parse()? })
            })
    }

    fn graph_reindex_files(&self) -> anyhow::Result<Vec<GraphReindexFile>> {
        let mut stmt = self
            .storage
            .connection()
            .prepare("SELECT id, path, language, kind FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            let language: String = row.get(2)?;
            let kind: String = row.get(3)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, language, kind))
        })?;
        let mut files = Vec::new();
        for row in rows {
            let (id, path, language, kind) = row?;
            files.push(GraphReindexFile {
                id,
                path,
                language: language.parse()?,
                kind: kind.parse()?,
            });
        }
        Ok(files)
    }

    fn indexed_files(&self) -> anyhow::Result<Vec<IndexedFile>> {
        let mut stmt =
            self.storage.connection().prepare("SELECT path, sha256 FROM files ORDER BY path")?;
        let rows =
            stmt.query_map([], |row| Ok(IndexedFile { path: row.get(0)?, sha256: row.get(1)? }))?;
        let mut files = Vec::new();
        for row in rows {
            files.push(row?);
        }
        Ok(files)
    }

    fn indexed_file_count(&self) -> anyhow::Result<usize> {
        let count =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    fn content_revision(&self) -> anyhow::Result<String> {
        let value = self.storage.connection().query_row(
            "SELECT COALESCE(string_agg(path || ':' || sha256, ',' ORDER BY path), '') FROM files",
            [],
            |row| row.get::<_, String>(0),
        )?;
        Ok(hex_sha256(value.as_bytes()))
    }
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
}

impl LogicalSymbolKey {
    fn from(row: &LogicalSymbolMemberRow) -> Self {
        Self {
            language: row.language.clone(),
            path: row.path.clone(),
            name: row.name.clone(),
            qualified_name: row.qualified_name.clone(),
            kind: row.kind.clone(),
        }
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
        || edge.confidence == "NameOnly"
        || edge.confidence == "Ambiguous"
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

fn target_for_path(config: &Config, relative_path: &Path) -> Option<(Language, TargetKind)> {
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

fn resolve_git_context(root: &Path) -> (String, String) {
    let commit_sha =
        git_output(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_default();
    let worktree_id = root.to_string_lossy().trim_end_matches('/').to_string();
    (commit_sha, worktree_id)
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
mod schema_bootstrap_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::config::ResolvedTarget;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn rebuild_bootstraps_sqlite_schema_for_empty_target_root() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let docs = root.join("docs");
        fs::create_dir_all(&docs).unwrap();

        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            }],
            local_ai: Default::default(),
        };

        let db = IndexDatabase::rebuild(&config).unwrap();
        assert!(config.database.exists());
        assert_eq!(table_count(&db, "files"), 1);
        assert_eq!(table_count(&db, "chunks"), 1);
        assert_eq!(table_count(&db, "symbols"), 1);
        assert_eq!(table_count(&db, "parser_failures"), 1);
        assert_eq!(table_count(&db, "index_meta"), 1);
        assert_eq!(table_count(&db, "chunk_fts"), 1);
        assert_eq!(table_count(&db, "git_commits"), 1);
        assert_eq!(table_count(&db, "git_file_changes"), 1);
        assert_eq!(table_count(&db, "git_chunk_blame"), 1);
        assert_eq!(table_count(&db, "commit_fts"), 1);
        assert_eq!(table_count(&db, "ai_models"), 1);
        assert_eq!(table_count(&db, "chunk_embeddings"), 1);
        assert_eq!(table_count(&db, "chunk_summaries"), 1);
        assert_eq!(table_count(&db, "reconcile_meta"), 1);
        assert_eq!(table_count(&db, "reconcile_attempts"), 1);
        assert!(file_columns(&db).contains(&"indexed_revision".to_string()));
        assert_eq!(indexed_revision_count(&db), 0);
        assert!(chunk_columns(&db).contains(&"anchor_version".to_string()));
        assert!(chunk_columns(&db).contains(&"normalized_hash".to_string()));
        assert!(chunk_columns(&db).contains(&"start_boundary_hash".to_string()));
        assert!(chunk_columns(&db).contains(&"end_boundary_hash".to_string()));
        assert!(chunk_columns(&db).contains(&"source_revision".to_string()));
        let embedding_columns = table_columns(&db, "chunk_embeddings");
        assert!(embedding_columns.contains(&"model_version".to_string()));
        assert!(embedding_columns.contains(&"input_hash".to_string()));
        assert!(embedding_columns.contains(&"embedding_text_version".to_string()));
        assert!(embedding_columns.contains(&"embedding_policy".to_string()));
        assert!(embedding_columns.contains(&"embedding_priority".to_string()));
        assert!(embedding_columns.contains(&"input_chars".to_string()));
        assert!(embedding_columns.contains(&"input_truncated".to_string()));
        assert!(embedding_columns.contains(&"attempt_count".to_string()));
        assert!(embedding_columns.contains(&"next_retry_after_ms".to_string()));
        assert!(embedding_columns.contains(&"computed_at_ms".to_string()));
        let edge_columns = table_columns(&db, "edges");
        assert!(edge_columns.contains(&"source_start_line".to_string()));
        assert!(edge_columns.contains(&"source_end_line".to_string()));
        assert!(edge_columns.contains(&"source_start_byte".to_string()));
        assert!(edge_columns.contains(&"source_end_byte".to_string()));
        assert!(edge_columns.contains(&"target_start_line".to_string()));
        assert!(edge_columns.contains(&"target_end_line".to_string()));
        assert!(edge_columns.contains(&"target_qualified_name".to_string()));
        assert!(edge_columns.contains(&"evidence".to_string()));
        assert!(edge_columns.contains(&"receiver_hint".to_string()));
        assert!(edge_columns.contains(&"resolution".to_string()));
        let logical_columns = table_columns(&db, "logical_symbols");
        assert!(logical_columns.contains(&"qualified_name".to_string()));
        assert!(logical_columns.contains(&"variant_count".to_string()));
        let member_columns = table_columns(&db, "logical_symbol_members");
        assert!(member_columns.contains(&"symbol_id".to_string()));
        assert!(member_columns.contains(&"signature_hash".to_string()));
        let github_ref_sync_columns = table_columns(&db, "github_ref_sync");
        assert!(github_ref_sync_columns.contains(&"status".to_string()));
        assert!(github_ref_sync_columns.contains(&"last_error".to_string()));
        let symbol_fact_columns = table_columns(&db, "symbol_facts");
        assert!(symbol_fact_columns.contains(&"fact_kind".to_string()));
        assert!(symbol_fact_columns.contains(&"fact_value".to_string()));
        assert_eq!(
            db.status(&config.database).unwrap().schema.current_version,
            schema::LATEST_SCHEMA_VERSION
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebuild_reports_file_preparation_progress() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn exported() {}\n").unwrap();

        let config = source_config(root.clone(), Language::Rust);
        let mut events = Vec::new();
        IndexDatabase::rebuild_with_progress(&config, |progress| events.push(progress)).unwrap();

        assert!(
            events.iter().any(|event| matches!(event, IndexProgress::PreparingFile { .. })),
            "missing preparing progress event: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(event, IndexProgress::IndexingFile { .. })),
            "missing indexing progress event: {events:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_progress_reports_first_final_and_decile_boundaries() {
        let reported = (1..=100)
            .filter(|current| should_report_file_progress(*current, 100))
            .collect::<Vec<_>>();
        assert_eq!(reported, vec![1, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
    }

    #[test]
    fn compatible_open_requires_recorded_schema_version() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".rag-rat")).unwrap();
        let database = root.join(".rag-rat/index.sqlite");
        IndexDatabase::migrate(&database).unwrap();
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch("DROP TABLE schema_version;").unwrap();
        drop(conn);

        let status = IndexDatabase::migration_check(&database).unwrap();
        assert_eq!(status.state, schema::SchemaState::Older);
        let err = IndexDatabase::open(&database).unwrap_err().to_string();
        assert!(err.contains("run `rag-rat migrate`"), "{err}");

        let migrated = IndexDatabase::migrate(&database).unwrap();
        assert_eq!(migrated.state, schema::SchemaState::Compatible);
        IndexDatabase::open(&database).unwrap();

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrate_adds_edge_name_columns_before_indexing_them() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".rag-rat")).unwrap();
        let database = root.join(".rag-rat/index.sqlite");
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE files(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                language TEXT NOT NULL,
                kind TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                modified_at_ms INTEGER NOT NULL,
                generated INTEGER NOT NULL DEFAULT 0,
                indexed_at_ms INTEGER NOT NULL
            );
            CREATE TABLE chunks(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                chunk_kind TEXT NOT NULL,
                symbol_path TEXT,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                text TEXT NOT NULL,
                text_hash TEXT NOT NULL
            );
            CREATE TABLE symbols(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                language TEXT NOT NULL,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                signature TEXT,
                docs TEXT
            );
            CREATE TABLE edges(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_symbol_id INTEGER,
                to_symbol_id INTEGER,
                edge_kind TEXT NOT NULL,
                confidence TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        drop(conn);

        let migrated = IndexDatabase::migrate(&database).unwrap();
        assert_eq!(migrated.state, schema::SchemaState::Compatible);
        let db = IndexDatabase::open(&database).unwrap();
        let columns = table_columns(&db, "edges");
        assert!(columns.contains(&"from_name".to_string()));
        assert!(columns.contains(&"to_name".to_string()));
        assert!(columns.contains(&"source_start_line".to_string()));
        assert!(columns.contains(&"source_end_line".to_string()));
        assert!(columns.contains(&"source_start_byte".to_string()));
        assert!(columns.contains(&"source_end_byte".to_string()));
        assert!(columns.contains(&"target_start_line".to_string()));
        assert!(columns.contains(&"target_end_line".to_string()));
        assert_eq!(table_count(&db, "idx_edges_from_name"), 1);
        assert_eq!(table_count(&db, "idx_edges_to_name"), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrate_preserves_github_papertrail_cache() {
        let (root, config) =
            markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        github::sync_from_refs(db.storage.connection(), &root, Some(&MockGitHubClient), false)
            .unwrap();
        assert_eq!(row_count(&db, "github_refs"), 1);
        assert_eq!(row_count(&db, "github_issues"), 1);
        assert_eq!(row_count(&db, "github_comments"), 1);
        assert_eq!(row_count(&db, "github_pull_requests"), 1);
        assert_eq!(row_count(&db, "github_reviews"), 1);
        assert_eq!(row_count(&db, "github_review_comments"), 1);
        assert_eq!(row_count(&db, "github_fts"), 5);
        db.storage
            .connection()
            .execute("DELETE FROM schema_version WHERE id = ?1", ["010_symbol_facts"])
            .unwrap();
        drop(db);

        let migrated = IndexDatabase::migrate(&config.database).unwrap();
        assert_eq!(migrated.state, schema::SchemaState::Compatible);
        let db = IndexDatabase::open(&config.database).unwrap();
        assert_eq!(row_count(&db, "github_refs"), 1);
        assert_eq!(row_count(&db, "github_issues"), 1);
        assert_eq!(row_count(&db, "github_comments"), 1);
        assert_eq!(row_count(&db, "github_pull_requests"), 1);
        assert_eq!(row_count(&db, "github_reviews"), 1);
        assert_eq!(row_count(&db, "github_review_comments"), 1);
        assert_eq!(row_count(&db, "github_fts"), 5);
        let hits = db.github_issue_search("sqlite", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].number, 42);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_rebuild_preserves_github_papertrail_cache() {
        let (root, config) =
            markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        github::sync_from_refs(db.storage.connection(), &root, Some(&MockGitHubClient), false)
            .unwrap();
        assert_eq!(row_count(&db, "github_issues"), 1);
        assert_eq!(row_count(&db, "github_fts"), 5);
        drop(db);

        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_eq!(row_count(&db, "github_refs"), 1);
        assert_eq!(row_count(&db, "github_issues"), 1);
        assert_eq!(row_count(&db, "github_comments"), 1);
        assert_eq!(row_count(&db, "github_pull_requests"), 1);
        assert_eq!(row_count(&db, "github_reviews"), 1);
        assert_eq!(row_count(&db, "github_review_comments"), 1);
        assert_eq!(row_count(&db, "github_ref_sync"), 1);
        assert_eq!(row_count(&db, "github_fts"), 5);
        let hits = db.github_issue_search("sqlite", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].number, 42);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_rebuild_preserves_installed_model_manifest() {
        let (root, config) = markdown_config("alpha token with enough detail for embeddings\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();
        let before = db.local_ai_status().unwrap();
        assert_eq!(before.embedding.model_id, ai::HASH_MODEL_ID);
        assert!(before.embedding.installed);
        drop(db);

        let db = IndexDatabase::rebuild(&config).unwrap();

        let after = db.local_ai_status().unwrap();
        assert_eq!(after.embedding.model_id, ai::HASH_MODEL_ID);
        assert!(after.embedding.installed);
        assert_eq!(after.embedding.state, "Ready");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_rebuild_preserves_other_worktree_contexts() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn current_context() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let other_file_id = db
            .storage
            .connection()
            .query_row(
                "
                INSERT INTO main.files(
                    path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
                    indexed_revision, commit_sha, worktree_id
                )
                VALUES ('src/other.rs', 'rust', 'source', 'other-sha', 0, 0, 1, 'other-sha', '', 'other-worktree')
                RETURNING id
                ",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let other_chunk_id = db
            .storage
            .connection()
            .query_row(
                "
                INSERT INTO main.chunks(
                    file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line,
                    text, text_hash, source_revision, anchor_version, normalized_hash,
                    start_boundary_hash, end_boundary_hash, start_context_hash, end_context_hash,
                    context_radius, embedding_policy, embedding_priority
                )
                VALUES (?1, 'symbol', 'other_context', 0, 12, 1, 1, 'other context', 'other-text',
                    'other-sha', 1, '', '', '', '', '', 2, 'Embed', 1)
                RETURNING id
                ",
                [other_file_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        db.storage
            .connection()
            .execute(
                "
                INSERT INTO main.symbols(
                    file_id, language, name, qualified_name, kind, start_byte, end_byte, signature, docs
                )
                VALUES (?1, 'rust', 'other_context', 'other_context', 'function', 0, 12, NULL, NULL)
                ",
                [other_file_id],
            )
            .unwrap();
        db.storage
            .connection()
            .execute(
                "INSERT INTO main.chunk_fts(rowid, text) VALUES (?1, 'other context')",
                [other_chunk_id],
            )
            .unwrap();
        drop(db);

        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_eq!(
            db.storage
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM main.files WHERE worktree_id = 'other-worktree'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            db.storage
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM main.chunks WHERE file_id = ?1",
                    [other_file_id],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
        assert_eq!(
            db.storage
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM main.symbols WHERE file_id = ?1",
                    [other_file_id],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );
        assert_eq!(
            db.storage
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM main.chunk_fts WHERE rowid = ?1",
                    [other_chunk_id],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compatible_open_refuses_dirty_and_newer_schema() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".rag-rat")).unwrap();
        let database = root.join(".rag-rat/index.sqlite");
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE schema_version(
                id TEXT PRIMARY KEY,
                applied_at_ms INTEGER NOT NULL,
                checksum TEXT NOT NULL,
                description TEXT NOT NULL
            );
            INSERT INTO schema_version(id, applied_at_ms, checksum, description)
            VALUES ('__dirty__', 1, '', 'partial migration in progress');
            ",
        )
        .unwrap();
        drop(conn);

        let dirty = IndexDatabase::migration_check(&database).unwrap();
        assert_eq!(dirty.state, schema::SchemaState::Dirty);
        let err = IndexDatabase::open(&database).unwrap_err().to_string();
        assert!(err.contains("dirty or partial"), "{err}");

        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            DELETE FROM schema_version;
            INSERT INTO schema_version(id, applied_at_ms, checksum, description)
            VALUES ('999_future_schema', 1, 'sha256:future', 'future schema');
            ",
        )
        .unwrap();
        drop(conn);
        let newer = IndexDatabase::migration_check(&database).unwrap();
        assert_eq!(newer.state, schema::SchemaState::Newer);
        let err = IndexDatabase::open(&database).unwrap_err().to_string();
        assert!(err.contains("newer rag-rat"), "{err}");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discover_mode_indexes_new_files_and_removes_deleted_files() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn old_symbol() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.discovery_status(&config).unwrap().unindexed_source_files, 0);

        fs::write(root.join("src/new.rs"), "pub fn new_symbol() {}\n").unwrap();
        fs::remove_file(root.join("src/lib.rs")).unwrap();
        let drift = db.discovery_status(&config).unwrap();
        assert_eq!(drift.unindexed_source_files, 1);
        assert_eq!(drift.removed_indexed_files, 1);
        assert!(drift.warning.as_deref().unwrap().contains("rag-rat index --discover"));

        let db = IndexDatabase::index_discover(&config).unwrap();
        let fresh = db.discovery_status(&config).unwrap();
        assert_eq!(fresh.unindexed_source_files, 0);
        assert_eq!(fresh.removed_indexed_files, 0);
        assert!(fresh.warning.is_none());
        assert_eq!(db.symbols("new_symbol", Some(Language::Rust), 10).unwrap().len(), 1);
        assert!(db.symbols("old_symbol", Some(Language::Rust), 10).unwrap().is_empty());

        let mut events = Vec::new();
        let db = IndexDatabase::index_discover_with_progress(&config, |progress| {
            events.push(progress);
        })
        .unwrap();
        assert!(matches!(events.last(), Some(IndexProgress::Finished { files: 0 })));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                IndexProgress::PreparingFile { .. } | IndexProgress::IndexingFile { .. }
            )),
            "no-op discover should not prepare or index files: {events:?}"
        );
        assert_eq!(db.symbols("new_symbol", Some(Language::Rust), 10).unwrap().len(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn indexing_skips_symlink_loops() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn loop_safe_symbol() {}\n").unwrap();
        std::os::unix::fs::symlink(&root, root.join("src/loop")).unwrap();

        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_eq!(db.symbols("loop_safe_symbol", Some(Language::Rust), 10).unwrap().len(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_git_files_are_indexed_as_worktree_overlay() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let docs = root.join("docs");
        fs::create_dir_all(&docs).unwrap();
        fs::write(docs.join("search.md"), "# Title\nbase token\n").unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["add", "."]);
        run_git(
            &root,
            &[
                "-c",
                "user.name=Rag Rat Test",
                "-c",
                "user.email=rag-rat@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );

        let config = markdown_config_for_root(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.search("base", 10, false).unwrap().len(), 1);

        fs::write(docs.join("search.md"), "# Title\noverlay token\n").unwrap();
        let db = IndexDatabase::index_changed(&config).unwrap();
        let scopes = db
            .storage
            .connection()
            .prepare(
                "
                SELECT commit_sha != '', worktree_id != ''
                FROM main.files
                WHERE path = 'docs/search.md'
                ORDER BY commit_sha != '' DESC, worktree_id != '' DESC
                ",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(scopes, vec![(true, false), (false, true)]);
        assert!(db.search("base", 10, false).unwrap().is_empty());
        let overlay_hits = db.search("overlay", 10, false).unwrap();
        assert_eq!(overlay_hits.len(), 1);
        assert!(overlay_hits[0].summary.contains("overlay token"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebuild_populates_revision_metadata_and_fresh_fts_state() {
        let (root, config) = markdown_config("alpha token");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let status = db.status(&config.database).unwrap();

        assert!(!status.content_revision.is_empty());
        assert_eq!(status.fts_source_revision.as_deref(), Some(status.content_revision.as_str()));
        assert_eq!(
            db.meta("content_revision").unwrap().as_deref(),
            Some(status.content_revision.as_str())
        );
        assert!(!status.fts_dirty);
        assert!(status.fts_fresh);
        assert!(!status.git_history.available);
        assert_eq!(status.git_history.commit_count, 0);
        assert_eq!(status.local_ai.embedding.state, "MissingModel");
        assert_eq!(status.local_ai.fastembed.backend, "fastembed");
        assert_eq!(status.local_ai.fastembed.model, ai::FASTEMBED_DISPLAY_MODEL);
        assert_eq!(status.local_ai.fastembed.dim, ai::FASTEMBED_EMBEDDING_DIM);
        assert!(!status.local_ai.fastembed.cache.is_empty());
        assert_eq!(status.local_ai.fastembed.build_feature_enabled, cfg!(feature = "fastembed"));
        assert_eq!(status.local_ai.artifacts.total_chunks, 1);
        assert_eq!(
            status.local_ai.artifacts.eligible_chunks + status.local_ai.artifacts.skipped_chunks,
            status.local_ai.artifacts.total_chunks
        );
        assert_eq!(
            status.local_ai.fastembed.eligible_embeddings
                + status.local_ai.fastembed.skipped_embeddings,
            status.local_ai.artifacts.total_chunks
        );
        assert_eq!(indexed_revision_count(&db), 1);
        assert_eq!(chunk_source_revision_count(&db), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(feature = "fastembed"))]
    #[test]
    fn fastembed_missing_feature_reports_rebuild_command() {
        let (root, config) = markdown_config("alpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();

        let err = db.install_model(ai::FASTEMBED_MODEL_ID).unwrap_err();
        assert!(err.to_string().contains(ai::FASTEMBED_MISSING_FEATURE_MESSAGE));

        let status = db.local_ai_status().unwrap();
        assert!(!status.fastembed.build_feature_enabled);
        assert_eq!(status.fastembed.status, "MissingRuntime");
        assert_eq!(
            status.fastembed.message.as_deref(),
            Some(ai::FASTEMBED_MISSING_FEATURE_MESSAGE)
        );
        assert_eq!(status.fastembed.next.as_deref(), Some("cargo install rag-rat"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_requires_explicit_model_install_and_ignores_stale_artifacts() {
        let (root, config) = markdown_config(
            "alpha token\nsecond line with enough detail for the semantic embedding policy to keep this chunk\nthird line with runtime context\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();
        let chunk_id = first_chunk_id(&db);

        let models = db.list_models().unwrap();
        let embedding = models.iter().find(|model| model.model_id == ai::HASH_MODEL_ID).unwrap();
        assert!(!embedding.installed);
        assert_eq!(embedding.status, "MissingModel");

        let hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].summary.contains("alpha token"));

        let blocked = db.reconcile(Some(1), Some(8)).unwrap();
        assert_eq!(blocked.processed_chunks, 0);
        assert_eq!(blocked.embeddings_written, 0);
        assert_eq!(blocked.blocked_chunks, 0);
        assert_eq!(blocked.model_id, ai::HASH_MODEL_ID);
        assert_eq!(blocked.batch_size, 8);
        assert_eq!(blocked.status, "Blocked");

        let status = db.local_ai_status().unwrap();
        assert_eq!(status.embedding.state, "MissingModel");
        assert_eq!(status.embedding.blocked_artifacts, 0);

        db.install_model(ai::HASH_MODEL_ID).unwrap();
        let plan = db.reconcile_plan().unwrap();
        assert_eq!(plan.embeddings.missing, 1);
        assert_eq!(plan.embeddings.current, 0);
        let current = db.reconcile(Some(1), Some(8)).unwrap();
        assert_eq!(current.embeddings_written, 1);
        assert_eq!(current.model_id, ai::HASH_MODEL_ID);
        assert_eq!(current.model_version, "hash-v1");
        assert_eq!(current.embedding_dim, ai::HASH_EMBEDDING_DIM);
        assert_eq!(current.status, "Current");
        assert_eq!(current.work_reasons.get("Missing"), Some(&1));
        let noop = db.reconcile(None, Some(8)).unwrap();
        assert_eq!(noop.processed_chunks, 0);
        assert_eq!(noop.embeddings_written, 0);
        let status = db.local_ai_status().unwrap();
        assert_eq!(status.embedding.state, "Ready");
        assert_eq!(status.embedding.current_artifacts, 1);
        let embedding_bytes: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT length(vector_blob) FROM chunk_embeddings WHERE chunk_id = ?1 AND status = 'Current'",
                [chunk_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(embedding_bytes, (ai::HASH_EMBEDDING_DIM * 4) as i64);

        let hits = db.search("alpha", 10, false).unwrap();
        assert!(hits[0].summary.contains("alpha token"));

        db.storage.connection().execute("DELETE FROM chunk_fts", []).unwrap();
        let vector_hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(vector_hits.len(), 1);
        assert_eq!(vector_hits[0].chunk_id, chunk_id);

        db.storage
            .connection()
            .execute(
                "UPDATE chunk_embeddings SET source_text_hash = 'old-hash' WHERE chunk_id = ?1",
                [chunk_id],
            )
            .unwrap();
        let plan = db.reconcile_plan().unwrap();
        assert_eq!(plan.embeddings.current, 0);
        assert_eq!(plan.embeddings.stale, 1);
        let refreshed = db.reconcile(None, Some(8)).unwrap();
        assert_eq!(refreshed.processed_chunks, 1);
        assert_eq!(refreshed.work_reasons.get("SourceChanged"), Some(&1));
        assert_eq!(db.current_embedding_count(ai::HASH_MODEL_ID).unwrap(), 1);
        let stale_embedding_hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(stale_embedding_hits.len(), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "fastembed")]
    #[test]
    fn cached_fastembed_model_recovers_ready_state() {
        let (root, config) = markdown_config("alpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let cache_dir = root.join("models");
        let revision = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
        let repo = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::create_dir_all(repo.join("snapshots").join(revision)).unwrap();
        fs::write(repo.join("refs").join("main"), revision).unwrap();

        ai::recover_cached_fastembed_model_at(db.storage.connection(), &cache_dir).unwrap();

        let models = db.list_models().unwrap();
        let fastembed =
            models.iter().find(|model| model.model_id == ai::FASTEMBED_MODEL_ID).unwrap();
        assert!(fastembed.installed);
        assert_eq!(fastembed.status, "Ready");
        let status = db.local_ai_status().unwrap();
        assert_eq!(status.fastembed.status, "Ready");
        assert!(status.fastembed.active);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "fastembed")]
    #[test]
    fn compatible_migrate_recovers_cached_fastembed_model() {
        let (root, config) = markdown_config("alpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let cache_dir = root.join("models");
        let revision = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
        let repo = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::create_dir_all(repo.join("snapshots").join(revision)).unwrap();
        fs::write(repo.join("refs").join("main"), revision).unwrap();
        db.storage
            .connection()
            .execute(
                "UPDATE ai_models
                 SET installed = 0, status = 'MissingModel', installed_at_ms = NULL
                 WHERE model_id = ?1",
                [ai::FASTEMBED_MODEL_ID],
            )
            .unwrap();

        IndexDatabase::migrate_with_fastembed_cache(&config.database, Some(&cache_dir)).unwrap();

        let db = IndexDatabase::open(&config.database).unwrap();
        let status = db.local_ai_status().unwrap();
        assert_eq!(status.fastembed.status, "Ready");
        assert!(status.fastembed.active);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_without_limit_processes_all_chunks() {
        let (root, config) = markdown_config(
            "# One\nalpha token with enough surrounding detail for embedding eligibility and useful semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding eligibility and useful semantic context\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();

        let report = db.reconcile(None, Some(2)).unwrap();

        assert_eq!(report.processed_chunks, 2);
        assert_eq!(report.embeddings_written, 2);
        assert_eq!(report.batch_size, 2);
        assert_eq!(db.current_embedding_count(ai::HASH_MODEL_ID).unwrap(), 2);
        let second = db.reconcile(None, Some(2)).unwrap();
        assert_eq!(second.processed_chunks, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn force_reconcile_processes_each_chunk_once_and_terminates() {
        // Regression: --force skipped the needs_embedding filter, so select_reconcile_batch
        // never returned an empty batch and the loop re-embedded the active set forever when
        // no --limit/--max-seconds was set. A generous finite limit lets this test terminate
        // either way; the processed/written counts distinguish fixed (==2) from buggy (==50).
        let (root, config) = markdown_config(
            "# One\nalpha token with enough surrounding detail for embedding eligibility and useful semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding eligibility and useful semantic context\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();

        // Two eligible chunks; force with a limit far above the chunk count.
        let report = db.reconcile_with_progress(Some(50), Some(2), true, |_| {}).unwrap();

        assert_eq!(report.embeddings_written, 2, "force re-embedded chunks: {report:?}");
        assert_eq!(report.processed_chunks, 2, "force re-processed chunks: {report:?}");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn force_reconcile_progress_is_honest_and_terminates_without_limit() {
        let (root, config) = markdown_config(
            "# One\nalpha token with enough surrounding detail for embedding eligibility and useful semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding eligibility and useful semantic context\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();

        // No --limit. max_seconds is only a safety net: if the force loop regressed to
        // re-embedding forever it would trip max_seconds and report "Partial" rather than
        // terminating naturally, which this test asserts against (no CI hang on regression).
        let mut events = Vec::new();
        let report = db
            .reconcile_with_options_progress(
                ai::ReconcileOptions {
                    force: true,
                    batch_size: Some(1),
                    max_seconds: Some(30),
                    ..ai::ReconcileOptions::default()
                },
                |event| events.push(event),
            )
            .unwrap();

        assert_eq!(report.status, "Current", "did not terminate naturally: {report:?}");
        assert_eq!(report.processed_chunks, 2);

        let started_total = events.iter().find_map(|event| match event {
            ai::ReconcileProgress::Started { total_chunks, .. } => Some(*total_chunks),
            _ => None,
        });
        assert_eq!(started_total, Some(2), "denominator should equal the eligible set");

        for event in &events {
            if let ai::ReconcileProgress::Batch { processed_chunks, total_chunks, .. } = event {
                assert!(
                    processed_chunks <= total_chunks,
                    "progress exceeded 100%: {processed_chunks}/{total_chunks}",
                );
            }
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_counts_only_active_context_chunks() {
        let (root, config) = markdown_config(
            "# One\nalpha token with enough surrounding detail for embedding eligibility and useful semantic context\n\n# Two\nbeta token with enough surrounding detail for embedding eligibility and useful semantic context\n",
        );
        let mut db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();

        let active = db.local_ai_status().unwrap().artifacts.total_chunks;
        assert!(active > 0, "expected active chunks, got {active}");

        // Point the connection at a context that matches no indexed rows. The active set
        // (temp.files) is now empty, so status must report 0 chunks. Pre-fix the counts ran
        // over main.chunks (every indexed commit) and ignored the active context entirely.
        db.set_context("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "ghost-worktree").unwrap();
        let scoped = db.local_ai_status().unwrap().artifacts;
        assert_eq!(scoped.total_chunks, 0, "status ignored active context scope");
        assert_eq!(scoped.current, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_treats_c_chunks_as_embedding_eligible() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.c"),
            r#"
static int read_sensor_value(int baseline)
{
    int adjusted = baseline + 42;
    return adjusted;
}

int main(void)
{
    int sample = read_sensor_value(7);
    return sample == 49 ? 0 : 1;
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::C);
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();

        let plan = db.reconcile_plan().unwrap();

        assert_eq!(plan.embeddings.skipped_by_policy.get("SkipLanguageUnsupported"), None);
        assert!(plan.embeddings.missing > 0, "plan: {:?}", plan.embeddings);

        let report = db.reconcile(None, Some(8)).unwrap();
        assert!(report.embeddings_written > 0, "report: {report:?}");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_policy_skips_tiny_chunks_before_embedding() {
        let (root, config) = markdown_config("tiny\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();

        let plan = db.reconcile_plan().unwrap();
        assert_eq!(plan.embeddings.missing, 0);
        assert_eq!(plan.embeddings.skipped_by_policy.get("SkipTooSmall"), Some(&1));

        let report = db.reconcile(None, Some(8)).unwrap();
        assert_eq!(report.embeddings_written, 0);
        assert_eq!(report.skipped_by_policy.get("SkipTooSmall"), Some(&1));
        assert_eq!(db.current_embedding_count(ai::HASH_MODEL_ID).unwrap(), 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_plan_reports_policy_skips_for_fastembed_model() {
        let (root, config) = markdown_config("tiny\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.storage
            .connection()
            .execute(
                "UPDATE ai_models
                 SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2
                 WHERE model_id = ?1",
                params![
                    ai::FASTEMBED_MODEL_ID,
                    i64::try_from(ai::FASTEMBED_EMBEDDING_DIM).unwrap()
                ],
            )
            .unwrap();
        db.storage
            .connection()
            .execute(
                "INSERT INTO index_meta(key, value) VALUES ('active_embedding_model', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [ai::FASTEMBED_MODEL_ID],
            )
            .unwrap();

        let plan = db.reconcile_plan().unwrap();

        assert_eq!(plan.embeddings.model_id, ai::FASTEMBED_MODEL_ID);
        assert_eq!(plan.embeddings.missing, 0);
        assert_eq!(plan.embeddings.skipped_by_policy.get("SkipTooSmall"), Some(&1));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(feature = "fastembed"))]
    #[test]
    fn blocked_fastembed_reconcile_still_reports_policy_skips() {
        let (root, config) = markdown_config("tiny\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.storage
            .connection()
            .execute(
                "INSERT INTO index_meta(key, value) VALUES ('active_embedding_model', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [ai::FASTEMBED_MODEL_ID],
            )
            .unwrap();

        let report = db.reconcile(None, Some(8)).unwrap();

        assert_eq!(report.status, "Blocked");
        assert_eq!(report.skipped_by_policy.get("SkipTooSmall"), Some(&1));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_explain_reports_weighted_score_components() {
        let (root, config) = markdown_config(
            "alpha runtime shutdown\nsecond line with enough detail for embedding eligibility and semantic vector scoring\nthird line\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.install_model(ai::HASH_MODEL_ID).unwrap();
        db.reconcile(None, Some(8)).unwrap();

        let hits = db.search_explain("runtime shutdown", 10, false).unwrap();

        assert_eq!(hits.len(), 1);
        let components = hits[0].score_components.as_ref().unwrap();
        let component_sum = components.bm25
            + components.vector
            + components.symbol
            + components.graph
            + components.git
            + components.github;
        assert!((hits[0].score - component_sum).abs() < 0.000_001);
        assert!(components.bm25 > 0.0);
        assert!(components.vector > 0.0);
        assert!(components.vector_note.is_none());
        assert!(components.bm25 <= 0.45);
        assert!(components.vector <= 0.35);
        assert!(components.symbol <= 0.10);
        assert!(components.graph <= 0.05);
        assert!(components.git <= 0.03);
        assert!(components.github <= 0.02);
        assert!(db.search("runtime shutdown", 10, false).unwrap()[0].score_components.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_explain_labels_missing_vector_runtime() {
        let (root, config) = markdown_config(
            "alpha runtime shutdown\nsecond line with enough detail for lexical search without embeddings\nthird line\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();

        let hits = db.search_explain("runtime shutdown", 10, false).unwrap();

        assert_eq!(hits.len(), 1);
        let components = hits[0].score_components.as_ref().unwrap();
        assert!(components.bm25 > 0.0);
        assert_eq!(components.vector, 0.0);
        assert_eq!(
            components.vector_note.as_deref(),
            Some("vector search unavailable: no current embedding model")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_history_indexes_commits_paths_queries_and_blame() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Rag Rat"]);
        run_git(&root, &["config", "user.email", "rag@example.com"]);

        fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Add alpha docs"]);

        fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Refresh beta docs"]);

        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![
                ResolvedTarget {
                    name: "markdown".to_string(),
                    language: Language::Markdown,
                    directories: vec![PathBuf::from("docs")],
                    include: vec!["**/*.md".to_string()],
                    exclude: Vec::new(),
                    kind: TargetKind::Docs,
                },
                ResolvedTarget {
                    name: "rust".to_string(),
                    language: Language::Rust,
                    directories: vec![PathBuf::from("src")],
                    include: vec!["**/*.rs".to_string()],
                    exclude: Vec::new(),
                    kind: TargetKind::Source,
                },
            ],
            local_ai: Default::default(),
        };
        let db = IndexDatabase::rebuild(&config).unwrap();
        let status = db.status(&config.database).unwrap();
        assert!(status.git_history.available);
        assert!(status.git_history.head.is_some());
        assert_eq!(status.git_history.indexed_head, status.git_history.head);
        assert_eq!(status.git_history.commit_count, 2);
        assert_eq!(status.git_history.file_change_count, 3);

        let commit_hits = db.commit_search("beta", 10).unwrap();
        assert_eq!(commit_hits.len(), 1);
        assert_eq!(commit_hits[0].subject, "Refresh beta docs");
        assert_eq!(commit_hits[0].evidence_kind, "historical");
        assert!(commit_hits[0].score > 0.0);

        let path_history = db.git_history_for_path("docs/search.md", 10).unwrap();
        assert_eq!(path_history.len(), 2);
        assert!(path_history.iter().all(|item| item.evidence_kind == "historical"));

        let symbol_history =
            db.git_history_for_symbol("tracked_symbol", Some(Language::Rust), 10).unwrap();
        assert_eq!(symbol_history.len(), 1);
        assert_eq!(symbol_history[0].path, "src/lib.rs");
        assert_eq!(symbol_history[0].evidence_kind, "historical");
        let impact = db.impact_surface("tracked_symbol", 10).unwrap();
        assert!(impact.iter().any(|item| {
            item.category == "Direct structural impact" && item.reason == "exact_symbol_definition"
        }));
        assert!(impact.iter().any(|item| {
            item.category == "Historical/papertrail evidence"
                && item.reason == "git_commit_touched_file"
        }));

        let query_commits = db.commits_touching_query("beta", 10).unwrap();
        let beta_commit =
            query_commits.iter().find(|hit| hit.subject == "Refresh beta docs").unwrap();
        assert!(beta_commit.evidence.iter().any(|value| value == "commit_message"));
        assert!(beta_commit.evidence.iter().any(|value| value == "file_change"));
        assert_eq!(beta_commit.evidence_kind, "historical");

        let chunk_id = first_chunk_id(&db);
        let blame = db.git_blame_chunk(chunk_id).unwrap().unwrap();
        assert_eq!(blame.source_text_hash, hex_sha256("# Title\nbeta token\n".as_bytes()));
        assert_eq!(blame.line_count, 2);
        assert_eq!(blame.commit_counts.values().sum::<i64>(), 2);
        assert!(blame.dominant_commit_lines >= 1);
        assert!(blame.dominant_commit.is_some());
        assert_eq!(blame.evidence_kind, "historical");
        let cached = db.git_blame_chunk(chunk_id).unwrap().unwrap();
        assert_eq!(cached.source_text_hash, blame.source_text_hash);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_rust_graph_edges_from_tree_sitter() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
use crate::worker::Worker;
mod worker;

trait Service {
    fn serve(&self);
}

struct Worker;

impl Service for Worker {
    fn serve(&self) {
        helper();
    }
}

fn helper() {}

fn caller() {
    helper();
    Worker.serve();
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");
        assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
        assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
        let callers = db.find_callers("helper", 10).unwrap();
        assert!(
            callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                    && edge.edge_kind == "calls_name"
            }),
            "helper callers: {callers:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ffi_surface_labels_exported_impl_members_separately() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub struct PhraseRepo;

#[uniffi::export]
impl PhraseRepo {
    pub fn children(&self) {}
    pub fn journal(&self) {}
}

#[cfg_attr(not(target_arch = "wasm32"), uniffi::export(async_runtime = "tokio"))]
impl Runtime {
    pub fn route_search_query(&self) {}
}

pub struct Runtime;

/// Not #[uniffi::export]: this is an internal helper.
pub fn internal_helper() {}

#[cfg_attr(target_arch = "wasm32", ::uniffi::export)]
pub fn exported_fn() {}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let surface = db.ffi_surface(20).unwrap();
        assert!(
            surface.iter().any(|item| {
                item.reason == "rust_uniffi_export"
                    && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("exported_fn"))
            }),
            "direct export should remain direct: {surface:?}"
        );
        assert!(
            surface.iter().any(|item| item.reason == "rust_uniffi_exported_impl"),
            "exported impl/type surface should be explicit: {surface:?}"
        );
        assert!(
            surface.iter().any(|item| {
                item.reason == "rust_uniffi_impl_member"
                    && item
                        .symbol
                        .as_deref()
                        .is_some_and(|symbol| symbol.ends_with("route_search_query"))
            }),
            "cfg_attr exported impl member should be labeled separately: {surface:?}"
        );
        assert!(
            surface.iter().any(|item| {
                item.reason == "rust_uniffi_impl_member"
                    && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("children"))
            }),
            "impl member should be labeled separately: {surface:?}"
        );
        assert!(
            !surface.iter().any(|item| {
                item.reason == "rust_uniffi_export"
                    && item.symbol.as_deref().is_some_and(|symbol| {
                        symbol.ends_with("children") || symbol.ends_with("journal")
                    })
            }),
            "impl members must not be reported as direct exports: {surface:?}"
        );
        assert!(
            !surface.iter().any(|item| {
                item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("internal_helper"))
            }),
            "comment-only UniFFI mentions must not create FFI surface rows: {surface:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_and_read_chunk_attach_bounded_graph_evidence() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n",
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let hits = db.search("helper caller", 10, false).unwrap();
        let helper_hit = hits
            .iter()
            .find(|hit| hit.symbol_path.as_deref().is_some_and(|path| path.ends_with("helper")))
            .expect("helper search hit");
        let helper_graph = helper_hit.graph.as_ref().expect("helper graph evidence");
        assert_eq!(helper_graph.caller_count, 1);
        assert!(helper_graph.top_callers.iter().any(|caller| {
            caller.symbol_path.ends_with("caller")
                && caller.callsite.line == 4
                && caller.callsite.span == [4, 4]
                && caller.confidence == "syntactic"
        }));
        assert!(helper_graph.callers.is_empty(), "search keeps graph compact");

        let caller_hit = hits
            .iter()
            .find(|hit| hit.symbol_path.as_deref().is_some_and(|path| path.ends_with("caller")))
            .expect("caller search hit");
        let caller_graph = caller_hit.graph.as_ref().expect("caller graph evidence");
        assert!(caller_graph.top_callees.iter().any(|callee| {
            callee.target == "helper"
                && callee.callsite.line == 4
                && callee.callsite.span == [4, 4]
                && callee.confidence == "syntactic"
        }));

        let chunk = db.read_chunk(caller_hit.chunk_id).unwrap().expect("caller chunk");
        let full_graph = chunk.graph.as_ref().expect("full read_chunk graph");
        assert!(full_graph.symbol.as_ref().is_some_and(|symbol| symbol.name == "caller"));
        assert!(
            full_graph
                .callees
                .iter()
                .any(|callee| callee.target == "helper" && callee.callsite.line == 4)
        );
        assert!(full_graph.notes.iter().any(|note| note.contains("tree-sitter/syntactic")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_exact_mode_requires_verified_symbol_identity() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n",
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let helper = db.symbols("helper", Some(Language::Rust), 10).unwrap().remove(0);
        let caller = db.symbols("caller", Some(Language::Rust), 10).unwrap().remove(0);

        let bare_exact = db
            .find_callers_with_options(
                "helper",
                10,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(bare_exact.is_empty(), "bare exact lookup should not fall back: {bare_exact:?}");

        let exact_callers = db
            .find_callers_with_options(
                "helper",
                10,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    symbol_id: Some(helper.symbol_id),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            exact_callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                    && edge.verified_target_symbol
            }),
            "exact callers: {exact_callers:?}"
        );
        assert!(exact_callers.iter().all(|edge| edge.verified_target_symbol));

        let exact_callees = db
            .trace_callees_with_options(
                "caller",
                10,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    symbol_id: Some(caller.symbol_id),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            exact_callees.iter().any(|edge| {
                edge.target.as_deref() == Some("helper") && edge.verified_target_symbol
            }),
            "exact callees: {exact_callees:?}"
        );
        assert!(exact_callees.iter().all(|edge| edge.verified_target_symbol));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symbol_lookup_ranks_type_definitions_before_impl_blocks() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
impl Database {
    pub fn open() -> Self {
        Database
    }
}

pub struct Database;
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let hits = db.symbols("Database", Some(Language::Rust), 10).unwrap();
        assert!(hits.len() >= 2, "fixture should expose both impl and struct symbols: {hits:?}");
        assert_eq!(hits[0].kind, "struct", "Database lookup should prefer type definition");
        assert!(
            hits.iter().any(|hit| hit.kind == "impl"),
            "impl Database should still be available after the struct: {hits:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_symbol_exact_mode_covers_duplicate_rust_variants() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_blocking() {}

#[cfg(target_arch = "wasm32")]
pub fn spawn_blocking() {}

pub fn caller() {
    spawn_blocking();
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let lookup = db
            .symbol_candidates(&crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            })
            .unwrap();
        let logical_symbol_id = lookup.candidates[0].logical_symbol_id.expect("logical id");
        assert_eq!(lookup.candidates[0].logical_variant_count, Some(2));
        assert_eq!(lookup.candidates[0].logical_group_reason.as_deref(), Some("cfg_variant"));

        let exact_variant_callers = db
            .find_callers_with_options(
                "spawn_blocking",
                10,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    symbol_id: Some(lookup.candidates[1].symbol_id),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            exact_variant_callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
                    && edge.target.as_deref() == Some("spawn_blocking")
                    && edge.verified_target_symbol
            }),
            "symbol_id exact should include its logical cfg group: {exact_variant_callers:?}"
        );
        assert!(exact_variant_callers.iter().all(|edge| edge.verified_target_symbol));

        let exact_logical = db
            .graph_traversal_report(
                "find_callers",
                &lookup.candidates[0],
                true,
                10,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    symbol_id: Some(lookup.candidates[0].symbol_id),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(exact_logical.query.logical_symbol_id, Some(logical_symbol_id));
        assert_eq!(
            exact_logical.logical_symbol.as_ref().map(|symbol| symbol.variant_count),
            Some(2)
        );
        assert_eq!(exact_logical.variants.len(), 2);
        assert!(exact_logical.results.iter().all(|edge| edge.verified_target_symbol));
        assert!(
            exact_logical.results.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
                    && edge.target.as_deref() == Some("spawn_blocking")
            }),
            "logical exact callers: {exact_logical:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_real_world_rust_graph_patterns() {
        let root = fixture_temp_root("graph-realworld/rust");
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
        assert_edge(&db, "src/lib.rs", "Worker", "exports", "Syntactic");
        assert_edge(&db, "entry", "new", "calls_name", "NameOnly");
        assert_edge(&db, "entry", "Client", "references_type", "Syntactic");
        assert_edge(&db, "drive", "serve", "calls_name", "NameOnly");
        assert_edge(&db, "drive", "GenericRunner", "references_type", "Syntactic");
        assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
        assert_edge(&db, "generic_call", "T", "references_type", "NameOnly");
        assert_edge(&db, "entry", "generated_call", "uses_macro", "NameOnly");
        let syntactic_callers = db.find_callers("serve", 10).unwrap();
        assert!(
            syntactic_callers.is_empty(),
            "syntactic serve callers should avoid receiver/name fallback: {syntactic_callers:?}"
        );
        let callers = db
            .find_callers_with_options(
                "serve",
                10,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Fuzzy,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            callers.iter().any(|edge| {
                edge.edge_kind == "calls_name"
                    && edge.edge_confidence == edge.confidence
                    && edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("drive"))
            }),
            "serve callers: {callers:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_typescript_graph_edges_from_tree_sitter() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/helper.ts"),
            "export function helper() {}\nexport const Card = () => null;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/App.tsx"),
            r#"
import { helper, Card } from "./helper";

export function run() {
  helper();
  return <Card />;
}

export const callRun = () => run();
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::TypeScript);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "run", "helper", "calls_name", "Syntactic");
        assert_edge(&db, "run", "Card", "references_type", "Syntactic");
        assert_edge(&db, "src/App.tsx", "helper", "imports", "Syntactic");
        assert_edge(&db, "src/App.tsx", "run", "exports", "Syntactic");
        let callees = db.trace_callees("callRun", 10).unwrap();
        assert!(
            callees.iter().any(|edge| {
                edge.to_symbol.as_deref().is_some_and(|name| name.ends_with("run"))
                    && edge.confidence == "Syntactic"
            }),
            "callRun callees: {callees:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_c_graph_edges_from_tree_sitter() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/runtime.c"),
            r#"
typedef struct Runtime Runtime;

struct Runtime {
  int state;
};

int helper(Runtime *runtime) {
  return runtime->state;
}

int runtime_open(Runtime *runtime) {
  return helper(runtime);
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::C);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "runtime_open", "helper", "calls_name", "Syntactic");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_c_file_scope_macro_regions_for_search() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("drivers/entropy")).unwrap();
        fs::write(
            root.join("drivers/entropy/entropy.c"),
            r#"
static int entropy_init(const struct device *dev)
{
    ARG_UNUSED(dev);
    return 0;
}

/* Entropy driver APIs structure */
static DEVICE_API(entropy, entropy_cryptoacc_trng_api) = {
    .get_entropy = entropy_cryptoacc_trng_get_entropy,
};

DEVICE_DT_INST_DEFINE(0, entropy_init, NULL, NULL, NULL,
                      PRE_KERNEL_1, CONFIG_ENTROPY_INIT_PRIORITY,
                      &entropy_cryptoacc_trng_api);
"#,
        )
        .unwrap();
        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "c".to_string(),
                language: Language::C,
                directories: vec![PathBuf::from("drivers/entropy")],
                include: vec!["**/*.c".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
        };
        let db = IndexDatabase::rebuild(&config).unwrap();

        let hits = db.search("DEVICE_API", 5, false).unwrap();
        assert!(
            hits.iter().any(|hit| {
                hit.path == "drivers/entropy/entropy.c" && hit.summary.contains("DEVICE_API")
            }),
            "DEVICE_API hits: {hits:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_cpp_graph_edges_from_tree_sitter() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/runtime.cpp"),
            r#"
namespace held {
class Runtime {
public:
  void open();
};

void helper() {}

void Runtime::open() {
  helper();
}
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Cpp);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "open", "helper", "calls_name", "Syntactic");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_real_world_typescript_graph_patterns() {
        let root = fixture_temp_root("graph-realworld/typescript");
        let config = source_config(root.clone(), Language::TypeScript);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "src/lib.tsx", "DefaultWidget", "imports", "Syntactic");
        assert_edge(&db, "src/lib.tsx", "WidgetNS", "imports", "NameOnly");
        assert_edge(&db, "src/lib.tsx", "WidgetProps", "imports", "Syntactic");
        assert_edge(&db, "src/lib.tsx", "ReExportedWidget", "exports", "NameOnly");
        assert_edge(&db, "useWidget", "useMemo", "calls_name", "NameOnly");
        assert_edge(&db, "useWidget", "DefaultWidget", "calls_name", "Syntactic");
        assert_edge(&db, "Shell", "renderWidget", "calls_name", "NameOnly");
        assert_edge(&db, "Shell", "WidgetNS", "references_type", "NameOnly");
        assert_edge(&db, "Shell", "DefaultWidget", "references_type", "Syntactic");
        assert_edge(&db, "DefaultWidget", "WidgetProps", "references_type", "Syntactic");
        let callees = db
            .trace_callees_with_options(
                "Shell",
                10,
                &crate::query::graph::GraphTraversalOptions {
                    include_references: true,
                    edge_kinds: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            callees.iter().any(|edge| {
                edge.edge_kind == "references_type"
                    && edge.edge_confidence == edge.confidence
                    && edge.to_symbol.as_deref().is_some_and(|name| name.ends_with("DefaultWidget"))
            }),
            "Shell callees: {callees:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rust_macro_edges_do_not_resolve_to_same_named_modules() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
mod format;

fn execute_one() {
    let _value = format!("hello");
}
"#,
        )
        .unwrap();
        fs::write(root.join("src/format.rs"), "pub fn helper() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let edge = db
            .storage
            .connection()
            .query_row(
                "
                SELECT edge_kind, to_name, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE edge_kind = 'uses_macro'
                  AND to_name = 'format'
                ",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(edge.0, "uses_macro");
        assert_eq!(edge.1, "format");
        assert_eq!(edge.2, None);
        assert_eq!(edge.3, "NameOnly");
        assert_eq!(edge.4, "unresolved");
        assert!(edge.5.as_deref().is_some_and(|value| value.contains("format!")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_old_graph_policy_rebuilds_stale_macro_edges() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
mod format;

fn execute_one() {
    let _value = format!("hello");
}
"#,
        )
        .unwrap();
        fs::write(root.join("src/format.rs"), "pub fn helper() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.storage
            .connection()
            .execute("UPDATE index_meta SET value = 'old' WHERE key = 'graph_index_version'", [])
            .unwrap();
        db.storage
            .connection()
            .execute(
                "
                UPDATE edges
                SET edge_kind = 'calls_name',
                    to_symbol_id = (SELECT id FROM symbols WHERE name = 'format' LIMIT 1),
                    confidence = 'Syntactic',
                    evidence = NULL,
                    resolution = 'syntactic'
                WHERE to_name = 'format'
                ",
                [],
            )
            .unwrap();
        drop(db);

        let reopened = IndexDatabase::open(&config.database).unwrap();
        let edge = reopened
            .storage
            .connection()
            .query_row(
                "
                SELECT edge_kind, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE to_name = 'format'
                  AND edge_kind = 'uses_macro'
                ",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(edge.0, "uses_macro");
        assert_eq!(edge.1, None);
        assert_eq!(edge.2, "NameOnly");
        assert_eq!(edge.3, "unresolved");
        assert!(edge.4.as_deref().is_some_and(|value| value.contains("format!")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qualified_common_member_calls_do_not_resolve_by_short_name() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub struct AlertsStore;

impl AlertsStore {
    pub fn new() -> Self {
        Self
    }
}

pub fn caller() {
    let _items: Vec<String> = Vec::new();
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let edge = db
            .storage
            .connection()
            .query_row(
                "
                SELECT to_name, target_qualified_name, to_symbol_id, confidence, resolution
                FROM edges
                WHERE from_name LIKE '%caller'
                  AND edge_kind = 'calls_name'
                  AND to_name = 'new'
                ",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(edge.0, "new");
        assert_eq!(edge.1.as_deref(), Some("Vec::new"));
        assert_eq!(edge.2, None);
        assert_eq!(edge.3, "NameOnly");
        assert_eq!(edge.4, "unresolved");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn macro_edges_do_not_resolve_to_same_named_typescript_symbols() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
fn rust_entry() {
    let _payload = json!({"ok": true});
}
"#,
        )
        .unwrap();
        fs::write(root.join("src/preferences.ts"), "export function json() { return {}; }\n")
            .unwrap();
        let mut config = source_config(root.clone(), Language::Rust);
        config.targets.push(ResolvedTarget {
            name: "typescript".to_string(),
            language: Language::TypeScript,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.ts".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        });
        let db = IndexDatabase::rebuild(&config).unwrap();

        let edge = db
            .storage
            .connection()
            .query_row(
                "
                SELECT edge_kind, to_name, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE edge_kind = 'uses_macro'
                  AND to_name = 'json'
                ",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(edge.0, "uses_macro");
        assert_eq!(edge.1, "json");
        assert_eq!(edge.2, None);
        assert_eq!(edge.3, "NameOnly");
        assert_eq!(edge.4, "unresolved");
        assert!(edge.5.as_deref().is_some_and(|value| value.contains("json!")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qualified_crate_helper_callers_use_name_fallback() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub mod task_spawn {
    pub fn spawn_blocking() {}
}

pub fn first() {
    crate::task_spawn::spawn_blocking();
}

pub fn second() {
    task_spawn::spawn_blocking();
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let callers = db.find_callers("spawn_blocking", 10).unwrap();
        assert!(
            callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("first"))
                    && edge.edge_kind == "calls_name"
                    && edge.resolution == "target_name_fallback"
            }),
            "spawn_blocking callers: {callers:?}"
        );
        assert!(
            callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("second"))
                    && edge.edge_kind == "calls_name"
            }),
            "spawn_blocking callers: {callers:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn caller_lookup_does_not_match_related_names_or_chain_evidence() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub mod runtime {
    pub mod task_spawn {
        pub fn spawn() {}
        pub fn spawn_blocking() -> JoinHandle {
            JoinHandle
        }
        pub fn spawn_blocking_handle() {}
        pub fn spawn_blocking_offload() -> JoinHandle {
            JoinHandle
        }
    }
}

pub struct JoinHandle;

impl JoinHandle {
    pub fn map_err(self) {}
}

pub fn direct() {
    crate::runtime::task_spawn::spawn_blocking();
}

pub fn related_handle() {
    crate::runtime::task_spawn::spawn_blocking_handle();
}

pub fn related_offload_chain() {
    crate::runtime::task_spawn::spawn_blocking_offload().map_err();
}

pub fn related_spawn_with_text() {
    crate::runtime::task_spawn::spawn();
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let callers = db.find_callers("spawn_blocking", 20).unwrap();
        assert!(
            callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("direct"))
                    && edge.target.as_deref() == Some("spawn_blocking")
                    && edge.edge_kind == "calls_name"
            }),
            "spawn_blocking callers: {callers:?}"
        );
        assert!(
            callers.iter().all(|edge| {
                !edge.from_symbol.as_deref().is_some_and(|name| {
                    name.ends_with("related_handle")
                        || name.ends_with("related_offload_chain")
                        || name.ends_with("related_spawn_with_text")
                }) && !matches!(
                    edge.target.as_deref(),
                    Some("spawn_blocking_handle" | "spawn_blocking_offload" | "spawn" | "map_err")
                )
            }),
            "caller lookup leaked related names or chain evidence: {callers:?}"
        );

        let qualified_callers = db.find_callers("src/lib.rs::spawn_blocking", 20).unwrap();
        assert!(
            qualified_callers.iter().any(|edge| {
                edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("direct"))
                    && edge.target.as_deref() == Some("spawn_blocking")
                    && edge.edge_kind == "calls_name"
            }),
            "qualified spawn_blocking callers: {qualified_callers:?}"
        );
        assert!(
            qualified_callers.iter().all(|edge| {
                !edge.from_symbol.as_deref().is_some_and(|name| {
                    name.ends_with("related_handle")
                        || name.ends_with("related_offload_chain")
                        || name.ends_with("related_spawn_with_text")
                }) && !matches!(
                    edge.target.as_deref(),
                    Some("spawn_blocking_handle" | "spawn_blocking_offload" | "spawn" | "map_err")
                )
            }),
            "qualified caller lookup leaked related names or chain evidence: {qualified_callers:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_past_the_old_structural_cap_still_contribute_symbols_and_edges() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        let filler =
            (0..700).map(|idx| format!("pub fn filler_{idx}() {{}}\n")).collect::<String>();
        fs::write(
            root.join("src/lib.rs"),
            format!(
                r#"
pub mod task_spawn {{
    pub fn spawn_blocking() {{}}
}}

{filler}

pub fn caller() {{
    crate::task_spawn::spawn_blocking();
}}
"#
            ),
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        assert!(fs::metadata(root.join("src/lib.rs")).unwrap().len() > 10_000);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let symbols = db.symbols("caller", Some(Language::Rust), 10).unwrap();
        assert!(
            symbols.iter().any(|symbol| symbol.name == "caller"),
            "caller symbols: {symbols:?}"
        );
        let callers = db.find_callers("spawn_blocking", 10).unwrap();
        assert!(
            callers.iter().any(|edge| {
                edge.edge_kind == "calls_name"
                    && edge.target.as_deref() == Some("spawn_blocking")
                    && edge.callsite.as_ref().is_some_and(|callsite| callsite.line > 700)
            }),
            "spawn_blocking callers: {callers:?}"
        );
        let impact =
            db.impact_surface("callers of crate::task_spawn::spawn_blocking in src", 10).unwrap();
        assert!(
            impact.iter().any(|item| {
                item.category == "Direct structural impact" && item.reason == "direct_caller"
            }),
            "impact: {impact:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn impact_surface_uses_high_signal_query_symbols_and_call_edges() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub mod runtime {
    pub fn unrelated_runtime_symbol() {}
}

pub mod task_spawn {
    pub fn spawn_blocking<F, T>(f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        f()
    }
}

pub fn caller() {
    crate::task_spawn::spawn_blocking(|| 1);
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let impact = db
            .impact_surface(
                "change runtime task_spawn spawn_blocking wasm inline native blocking pool",
                20,
            )
            .unwrap();
        assert!(
            impact.iter().any(|item| {
                item.category == "Direct structural impact"
                    && item.reason == "direct_caller"
                    && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
            }),
            "spawn_blocking caller should be present: {impact:?}"
        );
        assert!(
            impact.iter().all(|item| {
                !(item.reason == "exact_symbol_definition"
                    && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("runtime")))
            }),
            "broad `runtime` token should not become an exact impact seed: {impact:?}"
        );
        assert!(
            impact.iter().all(|item| {
                !item.evidence.iter().any(|evidence| evidence.contains("references_type"))
                    && item.symbol.as_deref() != Some("Send")
            }),
            "type references should not appear as direct impact: {impact:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn docs_for_symbol_prefers_local_source_context_before_broad_markdown() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/runtime")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            root.join("src/runtime/task_spawn.rs"),
            r#"
pub fn spawn_blocking<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    f()
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("docs/phrase-persistence.md"),
            "# Phrase persistence\nUnrelated notes mention spawn_blocking in passing.\n",
        )
        .unwrap();
        fs::write(
            root.join("docs/task_spawn.md"),
            "# task_spawn\nLocal task_spawn notes explain spawn_blocking.\n",
        )
        .unwrap();
        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![
                ResolvedTarget {
                    name: "rust".to_string(),
                    language: Language::Rust,
                    directories: vec![PathBuf::from("src")],
                    include: vec!["src/".to_string()],
                    exclude: Vec::new(),
                    kind: TargetKind::Source,
                },
                ResolvedTarget {
                    name: "markdown".to_string(),
                    language: Language::Markdown,
                    directories: vec![PathBuf::from("docs")],
                    include: vec!["**/*.md".to_string()],
                    exclude: Vec::new(),
                    kind: TargetKind::Docs,
                },
            ],
            local_ai: Default::default(),
        };
        let db = IndexDatabase::rebuild(&config).unwrap();
        let symbol = db.symbols("spawn_blocking", Some(Language::Rust), 10).unwrap().remove(0);
        let hits = db.docs_for_selected_symbol(&symbol, 10).unwrap();
        assert_eq!(hits[0].path, "src/runtime/task_spawn.rs", "docs hits: {hits:?}");
        let phrase_index = hits.iter().position(|hit| hit.path == "docs/phrase-persistence.md");
        let task_spawn_index = hits.iter().position(|hit| hit.path == "docs/task_spawn.md");
        assert!(
            phrase_index.is_none_or(|phrase| task_spawn_index.is_some_and(|local| local < phrase)),
            "path-local task_spawn docs should outrank unrelated phrase docs: {hits:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_tree_sitter_trees_still_contribute_valid_symbols_and_edges() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub fn helper() {}

pub fn caller() {
    helper();
}

fn broken( {
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let symbols = db.symbols("caller", Some(Language::Rust), 10).unwrap();
        assert!(
            symbols.iter().any(|symbol| symbol.name == "caller"),
            "caller symbols: {symbols:?}"
        );
        assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn receiver_method_calls_do_not_bind_to_same_named_free_functions() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub fn spawn_blocking() {}

pub fn caller(joinset: JoinSet) {
    joinset.spawn_blocking();
}

pub struct JoinSet;
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let edge = db
            .storage
            .connection()
            .query_row(
                "
                SELECT to_name, target_qualified_name, to_symbol_id, confidence, resolution, receiver_hint
                FROM edges
                WHERE from_name LIKE '%caller'
                  AND edge_kind = 'calls_name'
                  AND to_name = 'spawn_blocking'
                ",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(edge.0, "spawn_blocking");
        assert_eq!(edge.1.as_deref(), Some("joinset::spawn_blocking"));
        assert_eq!(edge.2, None);
        assert_eq!(edge.3, "NameOnly");
        assert_eq!(edge.4, "unresolved");
        assert_eq!(edge.5.as_deref(), Some("joinset"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trace_callees_excludes_type_references_by_default() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub struct JoinError;
pub enum Result<T, E> { Ok(T), Err(E) }
pub fn helper() {}

pub fn spawn_blocking<F, T>(f: F) -> Result<T, JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    helper();
    tokio::task::spawn_blocking(f)
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let default_callees = db.trace_callees("spawn_blocking", 20).unwrap();
        assert!(
            default_callees.iter().any(|edge| {
                edge.edge_kind == "calls_name"
                    && edge.target.as_deref() == Some("helper")
                    && edge.verified_target_symbol
            }),
            "default callees: {default_callees:?}"
        );
        assert!(
            default_callees
                .iter()
                .all(|edge| edge.target_qualified_name.as_deref()
                    != Some("tokio::task::spawn_blocking")),
            "default callees leaked unresolved external call: {default_callees:?}"
        );
        assert!(
            default_callees.iter().all(|edge| edge.edge_kind != "references_type"),
            "default callees leaked type refs: {default_callees:?}"
        );
        assert!(
            default_callees.iter().all(|edge| !matches!(
                edge.target.as_deref(),
                Some("F" | "T" | "Send" | "Result" | "JoinError")
            )),
            "default callees leaked generic/type targets: {default_callees:?}"
        );

        let with_refs = db
            .trace_callees_with_options(
                "spawn_blocking",
                20,
                &crate::query::graph::GraphTraversalOptions {
                    include_references: true,
                    edge_kinds: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            with_refs.iter().any(|edge| edge.edge_kind == "references_type"),
            "reference-enabled callees: {with_refs:?}"
        );

        let with_unresolved = db
            .trace_callees_with_options(
                "spawn_blocking",
                20,
                &crate::query::graph::GraphTraversalOptions {
                    include_unresolved: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            with_unresolved
                .iter()
                .any(|edge| edge.target_qualified_name.as_deref()
                    == Some("tokio::task::spawn_blocking")),
            "unresolved-enabled callees: {with_unresolved:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trace_callees_defaults_to_repo_relevant_calls() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub fn repo_helper() {}

pub fn caller(input: Result<String, String>) -> String {
    repo_helper();
    let values: Vec<String> = Vec::new();
    let _ = input.map_err(|error| error.to_string());
    let _ = Some("value").unwrap_or_else(|| "fallback");
    let _ = format!("hello");
    values.get(0).unwrap_or_else(|| "fallback").to_string()
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();

        let default_callees = db.trace_callees("caller", 20).unwrap();
        assert!(
            default_callees.iter().any(|edge| edge.target.as_deref() == Some("repo_helper")),
            "default callees should keep repo-local calls: {default_callees:?}"
        );
        assert!(
            default_callees.iter().all(|edge| {
                edge.edge_kind != "uses_macro"
                    && !matches!(
                        edge.target.as_deref(),
                        Some("new" | "map_err" | "unwrap_or_else" | "to_string" | "format")
                    )
            }),
            "default callees leaked low-signal calls: {default_callees:?}"
        );

        let expanded = db
            .trace_callees_with_options(
                "caller",
                20,
                &crate::query::graph::GraphTraversalOptions {
                    include_unresolved: true,
                    include_macros: true,
                    include_common_methods: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            expanded.iter().any(|edge| edge.edge_kind == "uses_macro"),
            "macro-enabled callees: {expanded:?}"
        );
        assert!(
            expanded.iter().any(|edge| edge.target.as_deref() == Some("unwrap_or_else")),
            "common-method-enabled callees: {expanded:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_kotlin_graph_edges_from_tree_sitter() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/Main.kt"),
            r#"
package dev.cq27.test

import dev.cq27.lib.ExternalThing

interface Syncable

class MainBridge : Syncable {
  suspend fun syncOnce() {
    helper()
    ExternalThing()
  }
}

fun helper() {}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Kotlin);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "syncOnce", "helper", "calls_name", "Syntactic");
        assert_edge(&db, "MainBridge", "Syncable", "implements", "Syntactic");
        assert_edge(&db, "src/Main.kt", "ExternalThing", "imports", "NameOnly");
        let impact = db.impact_surface("helper", 10).unwrap();
        assert!(
            impact.iter().any(|item| {
                item.category == "Direct structural impact" && item.reason == "direct_caller"
            }),
            "impact: {impact:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexes_real_world_kotlin_graph_patterns() {
        let root = fixture_temp_root("graph-realworld/kotlin");
        let config = source_config(root.clone(), Language::Kotlin);
        let db = IndexDatabase::rebuild(&config).unwrap();

        assert_edge(&db, "src/Main.kt", "ExternalFactory", "imports", "NameOnly");
        assert_edge(&db, "Worker", "companion", "contains", "Exact");
        assert_edge(&db, "companion", "create", "contains", "Exact");
        assert_edge(&db, "syncOnce", "create", "calls_name", "Syntactic");
        assert_edge(&db, "syncOnce", "Worker", "references_type", "Syntactic");
        assert_edge(&db, "syncOnce", "run", "calls_name", "Syntactic");
        assert_edge(&db, "syncOnce", "SingletonRunner", "references_type", "Syntactic");
        assert_edge(&db, "syncOnce", "ExternalFactory", "calls_name", "NameOnly");
        assert_edge(&db, "syncOnce", "ExternalFactory", "references_type", "NameOnly");
        assert_edge(&db, "syncOnce", "cleaned", "calls_name", "Syntactic");
        let callers = db.find_callers("cleaned", 10).unwrap();
        assert!(
            callers.iter().any(|edge| {
                edge.edge_kind == "calls_name"
                    && edge.edge_confidence == edge.confidence
                    && edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("syncOnce"))
            }),
            "cleaned callers: {callers:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kotlin_caller_lookup_respects_qualified_receivers_for_common_method_names() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/Main.kt"),
            r#"
package dev.cq27.test

object WatchProposalBuilder {
  fun build(): String = "proposal"
}

class AndroidDialogBuilder {
  fun build(): String = "dialog"
}

fun actualCaller() {
  WatchProposalBuilder.build()
}

fun unrelatedBuilderCalls(dialog: AndroidDialogBuilder) {
  dialog.build()
  AndroidDialogBuilder().build()
}
"#,
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Kotlin);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let target = db
            .symbols("build", Some(Language::Kotlin), 10)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.qualified_name.contains("WatchProposalBuilder"))
            .expect("WatchProposalBuilder.build symbol");
        let callers = db
            .find_callers_with_options(
                "build",
                20,
                &crate::query::graph::GraphTraversalOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    symbol_id: Some(target.symbol_id),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            callers
                .iter()
                .filter(|edge| edge
                    .from_symbol
                    .as_deref()
                    .is_some_and(|name| name.ends_with("actualCaller")))
                .count(),
            1,
            "actual caller should be present once: {callers:?}"
        );
        assert!(
            callers.iter().all(|edge| edge
                .from_symbol
                .as_deref()
                .is_none_or(|name| !name.ends_with("unrelatedBuilderCalls"))),
            "unrelated builder calls should not resolve to WatchProposalBuilder.build: {callers:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn github_sync_caches_papertrail_and_rationale_without_query_time_crawling() {
        let (root, config) =
            markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let mock = MockGitHubClient;

        let offline =
            github::sync_from_refs::<MockGitHubClient>(db.storage.connection(), &root, None, true)
                .unwrap();
        assert!(offline.offline);
        assert_eq!(offline.discovered_refs, 1);
        assert_eq!(offline.synced_items, 0);

        let report =
            github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false).unwrap();
        assert!(!report.offline);
        assert_eq!(report.discovered_refs, 1);
        assert_eq!(report.synced_items, 5);
        assert_eq!(report.status.issues, 1);
        assert_eq!(report.status.comments, 1);
        assert_eq!(report.status.pulls, 1);
        assert_eq!(report.status.reviews, 1);
        assert_eq!(report.status.review_comments, 1);

        let issue_hits = db.github_issue_search("sqlite", 10).unwrap();
        assert_eq!(issue_hits.len(), 1);
        assert_eq!(issue_hits[0].classification, "decision");
        assert_eq!(issue_hits[0].evidence_kind, "historical_github");

        let refs = db.github_refs_for_path("docs/search.md", 10).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].source_kind, "file");

        let rationale = db.rationale_search("risk", 10).unwrap();
        assert!(rationale.iter().any(|item| item.classification == "risk"));
        let issue_ref_rationale = db.rationale_search("Fixes #42", 10).unwrap();
        assert_eq!(issue_ref_rationale.first().map(|item| item.number), Some(42));
        assert_eq!(
            issue_ref_rationale.first().map(|item| item.evidence_kind),
            Some("literal_github_ref")
        );
        assert_eq!(issue_ref_rationale.first().map(|item| item.score), Some(1.0));
        assert!(
            issue_ref_rationale.iter().any(|item| item.number == 42),
            "issue ref rationale should use structured GitHub refs: {issue_ref_rationale:?}"
        );

        let chunk_id = first_chunk_id(&db);
        let papertrail = db.papertrail_for_chunk(chunk_id, 10).unwrap().unwrap();
        assert!(papertrail.current_source.is_some());
        assert!(!papertrail.github_evidence.is_empty());
        assert!(papertrail.github_evidence.iter().all(|item| {
            matches!(item.evidence_kind, "historical_github" | "literal_github_ref")
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn papertrail_for_commit_prefers_commit_sourced_github_refs() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Rag Rat"]);
        run_git(&root, &["config", "user.email", "rag@example.com"]);
        fs::write(root.join("docs/search.md"), "# Decision\nalpha\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Fix search rationale", "-m", "Fixes #42"]);

        let config = markdown_config_for_root(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();
        let commit = db
            .storage
            .connection()
            .query_row("SELECT hash FROM git_commits LIMIT 1", [], |row| row.get::<_, String>(0))
            .unwrap();
        let mock = MockGitHubClient;
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false).unwrap();

        let papertrail = db.papertrail_for_commit(&commit[..7], 10).unwrap();
        assert_eq!(papertrail.github_evidence.first().map(|item| item.number), Some(42));
        assert_eq!(
            papertrail.github_evidence.first().map(|item| item.evidence_kind),
            Some("literal_github_ref")
        );
        assert!(
            papertrail.fallback_github_evidence.is_empty(),
            "structured commit refs should suppress noisy fallback evidence: {papertrail:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn papertrail_for_symbol_dedupes_duplicate_file_refs() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "// First rationale (#42)\n// Second rationale (#42)\npub fn tracked_symbol() {}\n",
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let mock = MockGitHubClient;
        github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false).unwrap();
        let papertrail = db
            .papertrail_for_symbol("tracked_symbol", Some(Language::Rust), 10)
            .unwrap()
            .expect("tracked symbol papertrail");

        assert_eq!(
            papertrail
                .github_evidence
                .iter()
                .filter(|item| item.number == 42 && item.item_kind == "issue")
                .count(),
            1,
            "duplicate #42 refs in one file should collapse to one issue evidence row: {papertrail:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn github_sync_keeps_partial_cache_and_skips_synced_refs_after_404() {
        let (root, config) = markdown_config(
            "# Decision\nRefs cq27-dev/rag-rat#42 and cq27-dev/rag-rat#404\nwe will keep sqlite\n",
        );
        let db = IndexDatabase::rebuild(&config).unwrap();
        let mock = PartiallyFailingGitHubClient;

        let report =
            github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false).unwrap();
        assert_eq!(report.discovered_refs, 2);
        assert_eq!(report.synced_items, 5);
        assert_eq!(report.failed_refs, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].number, 404);
        assert_eq!(report.errors[0].status, "not_found");

        let issue_hits = db.github_issue_search("sqlite", 10).unwrap();
        assert_eq!(issue_hits.len(), 1);
        assert_eq!(issue_hits[0].number, 42);

        let second =
            github::sync_from_refs(db.storage.connection(), &root, Some(&mock), false).unwrap();
        assert_eq!(second.synced_items, 0);
        assert_eq!(second.skipped_refs, 2);
        assert_eq!(second.failed_refs, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_recovers_when_fts_is_marked_dirty() {
        let (root, config) = markdown_config("alpha token");
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.mark_fts_dirty().unwrap();

        let dirty = db.status(&config.database).unwrap();
        assert!(dirty.fts_dirty);
        assert!(!dirty.fts_fresh);

        let hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].summary, "alpha token");
        let fresh = db.status(&config.database).unwrap();
        assert!(!fresh.fts_dirty);
        assert!(fresh.fts_fresh);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_chunk_relocates_small_line_drift_to_current_text() {
        let (root, config) = markdown_config("# Title\nalpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let chunk_id = first_chunk_id(&db);
        fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

        let chunk = db.read_chunk(chunk_id).unwrap().unwrap();
        assert_eq!(chunk.start_line, 2);
        assert_eq!(chunk.end_line, 3);
        assert_eq!(chunk.text, "# Title\nalpha token\n");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_chunk_large_drift_reindexes_and_reports_stale_chunk() {
        let (root, config) = markdown_config("# Title\nalpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let chunk_id = first_chunk_id(&db);
        fs::write(root.join("docs/search.md"), "# Replacement\nbeta token\n").unwrap();

        let err = db.read_chunk(chunk_id).unwrap_err().to_string();
        assert!(err.contains("StaleChunk"), "{err}");
        let hits = db.search("beta", 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(db.search("alpha", 10, false).unwrap().is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_retries_after_healing_stale_hit() {
        let (root, config) = markdown_config("# Title\nalpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();

        let hits = db.search("alpha", 10, false).unwrap();
        assert!(hits.is_empty());
        let beta_hits = db.search("beta", 10, false).unwrap();
        assert_eq!(beta_hits.len(), 1);
        assert!(beta_hits[0].summary.contains("beta"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_heals_relocated_hits_before_returning_line_spans() {
        let (root, config) = markdown_config("# Title\nalpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

        let hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_line, 2);
        assert_eq!(hits[0].end_line, 3);
        assert!(hits[0].summary.contains("alpha"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_chunk_deleted_source_reports_gone() {
        let (root, config) = markdown_config("# Title\nalpha token\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let chunk_id = first_chunk_id(&db);
        fs::remove_file(root.join("docs/search.md")).unwrap();

        let err = db.read_chunk(chunk_id).unwrap_err().to_string();
        assert!(err.contains("Gone"), "{err}");
        assert!(db.search("alpha", 10, false).unwrap().is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_returns_needs_reindex_when_heal_cap_is_exceeded() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let docs = root.join("docs");
        fs::create_dir_all(&docs).unwrap();
        for index in 0..=MAX_AUTO_HEAL_FILES_PER_CALL {
            fs::write(docs.join(format!("doc-{index}.md")), "common stale token\n").unwrap();
        }
        let config = markdown_config_for_root(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();
        for index in 0..=MAX_AUTO_HEAL_FILES_PER_CALL {
            fs::write(docs.join(format!("doc-{index}.md")), "fresh replacement token\n").unwrap();
        }

        let err = db.search("common", 20, false).unwrap_err().to_string();
        assert!(err.contains("needs_reindex"), "{err}");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn heal_index_limit_does_not_warn_when_only_fresh_files_are_skipped() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let docs = root.join("docs");
        fs::create_dir_all(&docs).unwrap();
        fs::write(docs.join("one.md"), "one fresh token\n").unwrap();
        fs::write(docs.join("two.md"), "two fresh token\n").unwrap();
        let config = markdown_config_for_root(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let report = db.heal_index(Some(1)).unwrap();

        assert_eq!(report.healed_files, 0);
        assert_eq!(report.removed_files, 0);
        assert_eq!(report.skipped_files, 2);
        assert_eq!(report.message, None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_recovers_when_fts_revision_is_stale() {
        let (root, config) = markdown_config("alpha token");
        let db = IndexDatabase::rebuild(&config).unwrap();
        db.set_meta("fts_source_revision", "stale").unwrap();

        let stale = db.status(&config.database).unwrap();
        assert!(!stale.fts_dirty);
        assert!(!stale.fts_fresh);

        let hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        let fresh = db.status(&config.database).unwrap();
        assert_eq!(fresh.fts_source_revision.as_deref(), Some(fresh.content_revision.as_str()));
        assert!(fresh.fts_fresh);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parser_failures_report_paths() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("broken.rs"), "pub fn broken(").unwrap();
        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
        };

        let db = IndexDatabase::rebuild(&config).unwrap();
        let status = db.status(&config.database).unwrap();
        assert_eq!(status.parser_failures, 1);
        assert_eq!(status.parser_failure_paths[0].path, "src/broken.rs");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repo_memory_bound_to_logical_symbol_surfaces_in_symbol_chunk_and_impact() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "#[cfg(unix)]\npub fn cfg_helper() {}\n#[cfg(windows)]\npub fn cfg_helper() {}\n",
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let symbol = db
            .select_symbol(&crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("cfg_helper".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            })
            .unwrap()
            .unwrap()
            .expect("selected symbol");
        let logical_symbol_id = symbol.logical_symbol_id.expect("logical symbol id");

        let created = db
            .memory_create(crate::query::memory::RepoMemoryCreate {
                kind: "Invariant".to_string(),
                title: "Treat cfg helper variants as one logical helper".to_string(),
                body: "Caller and impact analysis should use the logical symbol, not one cfg body variant."
                    .to_string(),
                confidence: "high".to_string(),
                created_by: Some("test-agent".to_string()),
                source: Some("agent".to_string()),
                tags: vec!["cfg".to_string(), "graph".to_string()],
                bind: crate::query::memory::RepoMemoryBindTarget {
                    logical_symbol_id: Some(logical_symbol_id),
                    symbol_id: None,
                    chunk_id: None,
                    edge_id: None,
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                },
            })
            .unwrap();
        assert!(!created.duplicate);
        assert_eq!(created.memory.bindings[0].binding_kind, "logical_symbol");

        let memories = db.memory_for_symbol(&symbol, 10).unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].kind, "Invariant");
        let chunk_id = memories[0].bindings[0].chunk_id.expect("bound chunk");
        let chunk = db.read_chunk(chunk_id).unwrap().expect("memory chunk");
        assert_eq!(chunk.memories.len(), 1);
        assert_eq!(chunk.memories[0].memory_id, created.memory.memory_id);

        let impact = db
            .impact_surface_report_for_selected_symbol(
                &symbol,
                10,
                &crate::query::impact::ImpactSurfaceOptions::default(),
            )
            .unwrap();
        assert_eq!(impact.repo_memories.direct.len(), 1);
        assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);
        assert_eq!(impact.completeness_and_caveats.memory_status.stale, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repo_memory_validate_marks_changed_or_missing_anchors_non_current() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn anchored_memory() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let symbol = db
            .select_symbol(&crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("anchored_memory".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: false,
                limit: 10,
            })
            .unwrap()
            .unwrap()
            .expect("selected symbol");
        let chunk_id = db
            .storage
            .connection()
            .query_row(
                "
                SELECT chunks.id
                FROM chunks
                JOIN files ON files.id = chunks.file_id
                WHERE files.path = ?1 AND chunks.symbol_path = ?2
                LIMIT 1
                ",
                params![symbol.path, symbol.qualified_name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let created = db
            .memory_create(crate::query::memory::RepoMemoryCreate {
                kind: "Risk".to_string(),
                title: "Anchor must become stale when source hash changes".to_string(),
                body: "Validation should separate stale memories from current repo evidence."
                    .to_string(),
                confidence: "medium".to_string(),
                created_by: Some("test-agent".to_string()),
                source: Some("agent".to_string()),
                tags: Vec::new(),
                bind: crate::query::memory::RepoMemoryBindTarget {
                    logical_symbol_id: None,
                    symbol_id: None,
                    chunk_id: Some(chunk_id),
                    edge_id: None,
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                },
            })
            .unwrap();

        db.storage
            .connection()
            .execute("UPDATE chunks SET text_hash = 'changed' WHERE id = ?1", [chunk_id])
            .unwrap();
        let report = db.memory_validate().unwrap();
        assert_eq!(report.stale, 1);
        let stale = db.memory_for_symbol(&symbol, 10).unwrap();
        assert_eq!(stale[0].memory_id, created.memory.memory_id);
        assert_eq!(stale[0].bindings[0].anchor_status, "stale");

        db.storage.connection().execute("DELETE FROM chunks WHERE id = ?1", [chunk_id]).unwrap();
        let report = db.memory_validate().unwrap();
        assert_eq!(report.gone, 1);
        let gone = db.memory_for_symbol(&symbol, 10).unwrap();
        assert_eq!(gone[0].bindings[0].anchor_status, "gone");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repo_memory_bound_to_edge_surfaces_when_impact_crosses_call_path() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn target_edge() {}\npub fn caller_edge() {\n    target_edge();\n}\n",
        )
        .unwrap();
        let config = source_config(root.clone(), Language::Rust);
        let db = IndexDatabase::rebuild(&config).unwrap();
        let target = db
            .select_symbol(&crate::query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("target_edge".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: false,
                limit: 10,
            })
            .unwrap()
            .unwrap()
            .expect("selected target");
        let graph_options = crate::query::graph::GraphTraversalOptions {
            resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(target.symbol_id),
            logical_symbol_id: target.logical_symbol_id,
            ..Default::default()
        };
        let callers =
            db.graph_traversal_report("find_callers", &target, true, 10, &graph_options).unwrap();
        let edge_id = callers.results[0].edge_id;

        let edge_memory = db
            .memory_create(crate::query::memory::RepoMemoryCreate {
                kind: "Risk".to_string(),
                title: "caller_edge to target_edge must stay synchronous".to_string(),
                body: "This specific call path is used to prove edge-bound memories surface when impact crosses the edge."
                    .to_string(),
                confidence: "high".to_string(),
                created_by: Some("test-agent".to_string()),
                source: Some("agent".to_string()),
                tags: vec!["edge".to_string()],
                bind: crate::query::memory::RepoMemoryBindTarget {
                    logical_symbol_id: None,
                    symbol_id: None,
                    chunk_id: None,
                    edge_id: Some(edge_id),
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: None,
                    end_logical_symbol_id: None,
                    edge_sequence_hash: None,
                    path_summary: None,
                },
            })
            .unwrap();
        assert_eq!(edge_memory.memory.bindings[0].binding_kind, "edge");
        assert_eq!(edge_memory.memory.bindings[0].edge_id, Some(edge_id));

        let impact = db
            .impact_surface_report_for_selected_symbol(
                &target,
                10,
                &crate::query::impact::ImpactSurfaceOptions {
                    resolution_mode: crate::query::graph::GraphResolutionMode::Exact,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(impact.repo_memories.direct.is_empty());
        assert_eq!(impact.repo_memories.path_crossed.len(), 1);
        assert_eq!(impact.repo_memories.path_crossed[0].memory_id, edge_memory.memory.memory_id);
        assert_eq!(impact.completeness_and_caveats.memory_status.active, 1);

        let call_path_memory = db
            .memory_create(crate::query::memory::RepoMemoryCreate {
                kind: "TestExpectation".to_string(),
                title: "caller_edge path hash recall".to_string(),
                body: "Call-path memories are addressable by a deterministic edge sequence hash."
                    .to_string(),
                confidence: "medium".to_string(),
                created_by: Some("test-agent".to_string()),
                source: Some("agent".to_string()),
                tags: vec!["call-path".to_string()],
                bind: crate::query::memory::RepoMemoryBindTarget {
                    logical_symbol_id: None,
                    symbol_id: None,
                    chunk_id: None,
                    edge_id: None,
                    path: None,
                    start_line: None,
                    end_line: None,
                    commit_hash: None,
                    github_owner: None,
                    github_repo: None,
                    github_number: None,
                    start_logical_symbol_id: target.logical_symbol_id,
                    end_logical_symbol_id: target.logical_symbol_id,
                    edge_sequence_hash: Some("edge-sequence-test-hash".to_string()),
                    path_summary: Some("caller_edge -> target_edge".to_string()),
                },
            })
            .unwrap();
        let call_path = db.memory_for_call_path_hash("edge-sequence-test-hash", 10).unwrap();
        assert_eq!(call_path.len(), 1);
        assert_eq!(call_path[0].memory_id, call_path_memory.memory.memory_id);
        assert_eq!(call_path[0].call_paths[0].path_summary, "caller_edge -> target_edge");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repo_brief_ranks_churn_and_god_module_candidates() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Rag Rat"]);
        run_git(&root, &["config", "user.email", "rag@example.com"]);

        fs::write(root.join("src/stable.rs"), "pub fn stable() -> i32 { 1 }\n").unwrap();
        fs::write(root.join("src/hot.rs"), hot_module_text(0)).unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Add initial modules"]);

        for revision in 1..=3 {
            fs::write(root.join("src/hot.rs"), hot_module_text(revision)).unwrap();
            run_git(&root, &["add", "src/hot.rs"]);
            run_git(&root, &["commit", "-m", "Iterate hot module"]);
        }

        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
        };
        let db = IndexDatabase::rebuild(&config).unwrap();

        let churn = db
            .repo_brief(crate::query::repo_brief::RepoBriefOptions {
                mode: crate::query::repo_brief::RepoBriefMode::Churn,
                limit: 1,
                include_generated: false,
                include_memories: true,
            })
            .unwrap();
        assert_eq!(churn.candidates[0].path, "src/hot.rs");
        assert_eq!(churn.candidates[0].category, "recent_churn_hotspot");
        assert!(churn.candidates[0].score <= 1.0);
        assert!(churn.candidates[0].metrics.commit_touch_count >= 4);
        assert!(churn.candidates[0].why.iter().any(|reason| reason.contains("churn")));

        let god_modules = db
            .repo_brief(crate::query::repo_brief::RepoBriefOptions {
                mode: crate::query::repo_brief::RepoBriefMode::GodModules,
                limit: 1,
                include_generated: false,
                include_memories: true,
            })
            .unwrap();
        assert_eq!(god_modules.candidates[0].path, "src/hot.rs");
        assert!(god_modules.candidates[0].score <= 1.0);
        assert!(god_modules.candidates[0].metrics.symbol_count >= 30);
        assert!(!god_modules.candidates[0].split_hints.is_empty());
        assert!(
            god_modules.candidates[0].next_tools.iter().any(|tool| tool.tool == "impact_surface")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repo_clusters_groups_cotouched_files() {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/sync")).unwrap();
        fs::create_dir_all(root.join("src/ui")).unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Rag Rat"]);
        run_git(&root, &["config", "user.email", "rag@example.com"]);

        fs::write(root.join("src/sync/actor.rs"), "pub fn sync_actor() -> i32 { 1 }\n").unwrap();
        fs::write(root.join("src/sync/msg.rs"), "pub fn sync_msg() -> i32 { 2 }\n").unwrap();
        fs::write(root.join("src/ui/app.rs"), "pub fn ui_app() -> i32 { 3 }\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Add modules"]);

        for revision in 1..=2 {
            fs::write(
                root.join("src/sync/actor.rs"),
                format!("pub fn sync_actor() -> i32 {{ {revision} }}\n"),
            )
            .unwrap();
            fs::write(
                root.join("src/sync/msg.rs"),
                format!("pub fn sync_msg() -> i32 {{ {} }}\n", revision + 10),
            )
            .unwrap();
            run_git(&root, &["add", "src/sync/actor.rs", "src/sync/msg.rs"]);
            run_git(&root, &["commit", "-m", "Iterate sync modules"]);
        }

        let config = Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
        };
        let db = IndexDatabase::rebuild(&config).unwrap();

        let clusters = db
            .repo_clusters(crate::query::clusters::RepoClustersOptions {
                limit: 5,
                include_generated: false,
                include_memories: true,
                min_cluster_size: 2,
            })
            .unwrap();

        let sync_cluster = clusters
            .clusters
            .iter()
            .find(|cluster| cluster.name == "src/sync")
            .expect("sync cluster");
        assert!(sync_cluster.representative_paths.contains(&"src/sync/actor.rs".to_string()));
        assert!(sync_cluster.representative_paths.contains(&"src/sync/msg.rs".to_string()));
        assert!(sync_cluster.metrics.co_touch_edges >= 2);

        fs::remove_dir_all(root).unwrap();
    }

    fn hot_module_text(revision: usize) -> String {
        let mut text = String::new();
        text.push_str("pub fn entry() -> i32 {\n");
        for i in 0..32 {
            text.push_str(&format!("    helper_{i}() +\n"));
        }
        text.push_str(&format!("    {revision}\n}}\n"));
        for i in 0..32 {
            text.push_str(&format!("pub fn helper_{i}() -> i32 {{ {i} }}\n"));
        }
        text
    }

    fn unique_temp_root() -> PathBuf {
        let mut root = std::env::temp_dir();
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        root.push(format!("rag-rat-schema-test-{}-{}-{suffix}", std::process::id(), now_ms()));
        root
    }

    fn fixture_temp_root(fixture: &str) -> PathBuf {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let fixture_root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(fixture);
        copy_fixture_dir(&fixture_root, &root);
        root
    }

    fn copy_fixture_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let from_path = entry.path();
            let to_path = to.join(entry.file_name());
            if from_path.is_dir() {
                copy_fixture_dir(&from_path, &to_path);
            } else {
                fs::copy(&from_path, &to_path).unwrap();
            }
        }
    }

    fn markdown_config(text: &str) -> (PathBuf, Config) {
        let root = unique_temp_root();
        let _ = fs::remove_dir_all(&root);
        let docs = root.join("docs");
        fs::create_dir_all(&docs).unwrap();
        fs::write(docs.join("search.md"), text).unwrap();
        let config = markdown_config_for_root(root.clone());
        (root, config)
    }

    fn markdown_config_for_root(root: PathBuf) -> Config {
        Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            }],
            local_ai: Default::default(),
        }
    }

    fn source_config(root: PathBuf, language: Language) -> Config {
        Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: language.as_str().to_string(),
                language,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
        }
    }

    fn assert_edge(db: &IndexDatabase, from: &str, to: &str, edge_kind: &str, confidence: &str) {
        let count = db
            .storage
            .connection()
            .query_row(
                "
                SELECT COUNT(*)
                FROM edges
                WHERE edge_kind = ?1
                  AND confidence = ?2
                  AND COALESCE(from_name, '') LIKE ?3
                  AND to_name LIKE ?4
                ",
                params![edge_kind, confidence, format!("%{from}%"), format!("%{to}%")],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert!(count > 0, "missing edge {from} -[{edge_kind}/{confidence}]-> {to}");
    }

    fn table_count(db: &IndexDatabase, table: &str) -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [table], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn row_count(db: &IndexDatabase, table: &str) -> i64 {
        db.storage
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
    }

    fn chunk_columns(db: &IndexDatabase) -> Vec<String> {
        table_columns(db, "chunks")
    }

    fn file_columns(db: &IndexDatabase) -> Vec<String> {
        table_columns(db, "files")
    }

    fn table_columns(db: &IndexDatabase, table: &str) -> Vec<String> {
        let mut stmt =
            db.storage.connection().prepare(&format!("PRAGMA table_info({table})")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
    }

    fn indexed_revision_count(db: &IndexDatabase) -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM files WHERE indexed_revision != ''", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn chunk_source_revision_count(db: &IndexDatabase) -> i64 {
        db.storage
            .connection()
            .query_row("SELECT COUNT(*) FROM chunks WHERE source_revision != ''", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn first_chunk_id(db: &IndexDatabase) -> i64 {
        db.storage
            .connection()
            .query_row("SELECT id FROM chunks ORDER BY id LIMIT 1", [], |row| row.get(0))
            .unwrap()
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    struct MockGitHubClient;

    impl github::GitHubClient for MockGitHubClient {
        fn issue(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<github::GitHubIssue> {
            Ok(github::GitHubIssue {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                html_url: format!("https://github.com/{owner}/{repo}/issues/{number}"),
                state: "open".to_string(),
                title: "Decision: keep sqlite".to_string(),
                body: "We decided sqlite is required for binary size.".to_string(),
                author: Some("octo".to_string()),
                created_at: Some("2026-01-01T00:00:00Z".to_string()),
                updated_at: Some("2026-01-02T00:00:00Z".to_string()),
                is_pull_request: true,
            })
        }

        fn issue_comments(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Vec<github::GitHubComment>> {
            Ok(vec![github::GitHubComment {
                id: 4201,
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                html_url: format!("https://github.com/{owner}/{repo}/issues/{number}#comment-1"),
                body: "Rejected alternative: duckdb was too large.".to_string(),
                author: Some("octo".to_string()),
                created_at: Some("2026-01-01T01:00:00Z".to_string()),
                updated_at: Some("2026-01-01T01:00:00Z".to_string()),
            }])
        }

        fn pull(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Option<github::GitHubPullRequest>> {
            Ok(Some(github::GitHubPullRequest {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                html_url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
                state: "open".to_string(),
                title: "Use sqlite".to_string(),
                body: "Constraint: normal queries must use cache only.".to_string(),
                author: Some("octo".to_string()),
                created_at: Some("2026-01-01T00:00:00Z".to_string()),
                updated_at: Some("2026-01-02T00:00:00Z".to_string()),
                merged_at: None,
            }))
        }

        fn pull_reviews(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Vec<github::GitHubReview>> {
            Ok(vec![github::GitHubReview {
                id: 4202,
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                html_url: Some(format!("https://github.com/{owner}/{repo}/pull/{number}#review")),
                state: "COMMENTED".to_string(),
                body: "Risk: live crawling during search would be surprising.".to_string(),
                author: Some("reviewer".to_string()),
                submitted_at: Some("2026-01-01T02:00:00Z".to_string()),
            }])
        }

        fn pull_review_comments(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Vec<github::GitHubReviewComment>> {
            Ok(vec![github::GitHubReviewComment {
                id: 4203,
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                path: Some("docs/search.md".to_string()),
                html_url: format!("https://github.com/{owner}/{repo}/pull/{number}#discussion"),
                body: "No longer use obsolete duckdb rationale.".to_string(),
                author: Some("reviewer".to_string()),
                created_at: Some("2026-01-01T03:00:00Z".to_string()),
                updated_at: Some("2026-01-01T03:00:00Z".to_string()),
            }])
        }
    }

    struct PartiallyFailingGitHubClient;

    impl github::GitHubClient for PartiallyFailingGitHubClient {
        fn issue(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<github::GitHubIssue> {
            if number == 404 {
                anyhow::bail!("gh: Not Found (HTTP 404)");
            }
            MockGitHubClient.issue(owner, repo, number)
        }

        fn issue_comments(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Vec<github::GitHubComment>> {
            MockGitHubClient.issue_comments(owner, repo, number)
        }

        fn pull(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Option<github::GitHubPullRequest>> {
            MockGitHubClient.pull(owner, repo, number)
        }

        fn pull_reviews(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Vec<github::GitHubReview>> {
            MockGitHubClient.pull_reviews(owner, repo, number)
        }

        fn pull_review_comments(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
        ) -> anyhow::Result<Vec<github::GitHubReviewComment>> {
            MockGitHubClient.pull_review_comments(owner, repo, number)
        }
    }
}
