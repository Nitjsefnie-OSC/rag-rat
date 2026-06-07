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
    time::{SystemTime, UNIX_EPOCH},
};

use gix::{
    bstr::{BString, ByteSlice},
    status::{UntrackedFiles, tree_index},
};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    config::{Config, TargetKind},
    index::{
        ai::{LocalAiStatus, ModelInfo, ReconcileReport},
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
    search::lexical::SearchHit,
    storage::IndexConnection,
    storage::StorageStatus,
};

#[derive(Debug)]
pub struct IndexDatabase {
    storage: IndexConnection,
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
    IndexingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
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
        let mut storage = IndexConnection::open(path)?;
        schema::check_compatible(storage.connection())?;
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(root) = meta_for(storage.connection(), "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        Ok(Self { storage })
    }

    pub fn migrate(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        let storage = IndexConnection::open(path)?;
        let status = schema::status(storage.connection())?;
        match status.state {
            schema::SchemaState::Compatible => return Ok(status),
            schema::SchemaState::Newer | schema::SchemaState::Dirty => {
                anyhow::bail!("{}", status.message);
            },
            schema::SchemaState::Missing | schema::SchemaState::Older => {},
        }
        schema::apply(storage.connection())?;
        ai::ensure_model_manifest(storage.connection())?;
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
        Ok(Self { storage })
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
        remove_database_files(&config.database)?;
        let mut db = Self::create_or_migrate(&config.database)?;
        let result = (|| -> anyhow::Result<()> {
            db.storage.execute_batch("BEGIN TRANSACTION")?;
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            db.write_git_meta(&config.root)?;
            let indexed = db.index_targets_with_progress(config, &mut progress)?;
            db.index_git_history(&config.root)?;
            db.resolve_edges()?;
            progress(IndexProgress::RebuildingFts);
            db.rebuild_fts()?;
            db.set_meta("indexed_at_ms", &now_ms().to_string())?;
            db.storage.execute_batch("COMMIT")?;
            progress(IndexProgress::Finished { files: indexed });
            Ok(())
        })();
        if result.is_err() {
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        result?;
        Ok(db)
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
        if db.indexed_file_count()? == 0 {
            return Self::rebuild_with_progress(config, progress);
        }
        progress(IndexProgress::Started { database: config.database.clone(), mode });
        let result = (|| -> anyhow::Result<()> {
            db.storage.execute_batch("BEGIN TRANSACTION")?;
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            db.write_git_meta(&config.root)?;
            db.index_git_history(&config.root)?;
            let indexed = match mode {
                IndexMode::Changed => db.index_changed_files_with_progress(config, progress)?,
                IndexMode::Discover => db.index_discovered_files_with_progress(config, progress)?,
                IndexMode::Full => unreachable!("full mode is handled by rebuild_with_progress"),
            };
            db.resolve_edges()?;
            if indexed > 0 {
                db.sync_fts()?;
            }
            db.set_meta("indexed_at_ms", &now_ms().to_string())?;
            db.storage.execute_batch("COMMIT")?;
            progress(IndexProgress::Finished { files: indexed });
            Ok(())
        })();
        if result.is_err() {
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
        progress(IndexProgress::Discovered { files: files.len() });

        for (index, file) in files.iter().enumerate() {
            progress(IndexProgress::IndexingFile {
                current: index + 1,
                total: files.len(),
                path: file.relative_path.clone(),
                language: file.language,
                kind: file.kind,
            });
            let text = match fs::read_to_string(&file.full_path) {
                Ok(text) => text,
                Err(err) => {
                    self.insert_parser_failure(
                        &file.relative_path,
                        file.language,
                        &err.to_string(),
                    )?;
                    continue;
                },
            };
            self.index_file(
                &file.relative_path,
                file.language,
                file.kind,
                file_metadata_ms(&file.full_path)?,
                &text,
            )?;
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
        self.apply_incremental_file_plan(plan.files, plan.deleted, progress)
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
            self.remove_file(&path)?;
        }

        for (index, file) in files.iter().enumerate() {
            progress(IndexProgress::IndexingFile {
                current: index + 1,
                total: files.len(),
                path: file.relative_path.clone(),
                language: file.language,
                kind: file.kind,
            });
            let text = match fs::read_to_string(&file.full_path) {
                Ok(text) => text,
                Err(err) => {
                    self.insert_parser_failure(
                        &file.relative_path,
                        file.language,
                        &err.to_string(),
                    )?;
                    continue;
                },
            };
            self.remove_file(&file.relative_path)?;
            self.index_file(
                &file.relative_path,
                file.language,
                file.kind,
                file_metadata_ms(&file.full_path)?,
                &text,
            )?;
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
        self.ensure_fts_fresh()?;
        self.search_with_heal(query, limit, include_generated, true)
    }

    pub fn symbols(
        &self,
        name: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::symbol::SymbolHit>> {
        crate::query::symbol::lookup(self.storage.connection(), name, language, limit)
    }

    pub fn read_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<crate::query::ReadChunk>> {
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
                self.remove_file(Path::new(&path))?;
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

    pub fn docs_for_symbol(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
        self.search(symbol, limit, true)
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
            github::sync_from_refs(self.storage.connection(), root, Some(&client), false)
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
        let Some(symbol) = self.symbols(symbol, language, 1)?.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(github::papertrail_for_symbol(self.storage.connection(), &symbol, limit)?))
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

    pub fn reconcile(&self, limit: Option<u32>) -> anyhow::Result<ReconcileReport> {
        ai::reconcile(self.storage.connection(), limit)
    }

    pub fn heal_index(&self, limit: Option<u32>) -> anyhow::Result<HealIndexReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("heal_index requires source_root metadata; run `rag-rat index` first");
        };
        let indexed_files = self.indexed_files()?;
        let max_files = limit.map(usize::try_from).transpose()?.unwrap_or(usize::MAX);
        let mut report = HealIndexReport {
            checked_files: 0,
            healed_files: 0,
            removed_files: 0,
            skipped_files: 0,
            fts_fresh: false,
            message: None,
        };

        for file in indexed_files.into_iter().take(max_files) {
            report.checked_files += 1;
            let path = Path::new(&file.path);
            let full_path = root.join(path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                self.remove_file(path)?;
                report.removed_files += 1;
                continue;
            };
            let sha256 = hex_sha256(text.as_bytes());
            if sha256 == file.sha256 {
                report.skipped_files += 1;
                continue;
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
        if usize::try_from(report.checked_files).unwrap_or(usize::MAX)
            < self.indexed_file_count()?
        {
            report.message = Some("limit reached; rerun heal_index to continue".to_string());
        }
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

    pub fn trace_callees(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, false, limit)
    }

    pub fn impact_surface(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface(self.storage.connection(), query, limit)
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
        self.remove_file(path)?;
        self.index_file(path, row.language, row.kind, file_metadata_ms(&full_path)?, &text)?;
        self.resolve_edges()
    }

    fn index_file(
        &self,
        path: &Path,
        language: Language,
        kind: TargetKind,
        modified_at_ms: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        if language != Language::Markdown && kind != TargetKind::Generated {
            if text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                // Large source files are intentionally coarse-indexed to keep full-repo indexing
                // responsive. This is not a parser failure.
            } else if let Err(err) = parser::parse_symbols(path, language, text) {
                self.insert_parser_failure(path, language, &err.to_string())?;
            }
        }
        let sha256 = hex_sha256(text.as_bytes());
        let file_id = self.storage.connection().query_row(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, indexed_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
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
        if kind != TargetKind::Generated && text.len() <= chunker::MAX_STRUCTURAL_PARSE_BYTES {
            edges::index_file_edges(self.storage.connection(), file_id, path, language, text)?;
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
        for chunk in chunks {
            let anchor =
                anchors::anchor_for_text(&chunk.text, chunk.start_line, chunk.end_line, full_text);
            self.storage.connection().execute(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line, text, text_hash,
                                    source_revision, anchor_version, normalized_hash, start_boundary_hash, end_boundary_hash,
                                    start_context_hash, end_context_hash, context_radius)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
        }
        Ok(())
    }

    fn write_git_meta(&self, root: &Path) -> anyhow::Result<()> {
        self.set_meta("git_commit", &git_output(root, &["rev-parse", "HEAD"]).unwrap_or_default())?;
        let dirty = !git_output(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        self.set_meta("git_dirty", if dirty { "true" } else { "false" })?;
        Ok(())
    }

    fn index_git_history(&self, root: &Path) -> anyhow::Result<GitHistoryIndexStatus> {
        git_history::index(self.storage.connection(), root)
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
    ) -> anyhow::Result<Vec<SearchHit>> {
        let hits = crate::search::lexical::search(
            self.storage.connection(),
            query,
            limit,
            include_generated,
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
        self.search_with_heal(query, limit, include_generated, false)
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

    fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
        let path = path_string(path);
        self.storage.connection().execute(
            "UPDATE edges
             SET to_symbol_id = NULL,
                 confidence = 'NameOnly'
             WHERE to_symbol_id IN (
                 SELECT symbols.id FROM symbols
                 JOIN files ON files.id = symbols.file_id
                 WHERE files.path = ?1
             )",
            [&path],
        )?;
        self.storage.connection().execute(
            "DELETE FROM edges
             WHERE source_file_id IN (SELECT id FROM files WHERE path = ?1)
                OR from_symbol_id IN (
                    SELECT symbols.id FROM symbols
                    JOIN files ON files.id = symbols.file_id
                    WHERE files.path = ?1
                )",
            [&path],
        )?;
        self.storage
            .connection()
            .execute("DELETE FROM parser_failures WHERE path = ?1", [&path])?;
        self.storage.connection().execute(
            "DELETE FROM chunk_fts
             WHERE rowid IN (
                 SELECT chunks.id FROM chunks
                 JOIN files ON files.id = chunks.file_id
                 WHERE files.path = ?1
             )",
            [&path],
        )?;
        self.storage.connection().execute(
            "DELETE FROM chunks WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
            [&path],
        )?;
        self.storage.connection().execute(
            "DELETE FROM symbols WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
            [&path],
        )?;
        self.storage.connection().execute("DELETE FROM files WHERE path = ?1", [&path])?;
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
        files.push(IndexFile { full_path, relative_path: relative_path.clone(), language, kind });
    }
    Ok(files)
}

fn discovery_plan(conn: &rusqlite::Connection, config: &Config) -> anyhow::Result<DiscoveryPlan> {
    let discovered = collect_index_files(config)?;
    let mut indexed = indexed_file_map(conn)?;
    let mut current_paths = BTreeSet::new();
    let mut files = Vec::new();
    let mut unindexed = Vec::new();
    let mut changed = Vec::new();

    for file in discovered {
        let relative = path_string(&file.relative_path);
        current_paths.insert(file.relative_path.clone());
        let Some(indexed_hash) = indexed.remove(&relative) else {
            unindexed.push(file.clone());
            files.push(file);
            continue;
        };
        let text = fs::read(&file.full_path)?;
        let current_hash = hex_sha256(&text);
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
        discovered_files: current_paths.len(),
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

fn remove_database_files(path: &Path) -> anyhow::Result<()> {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-shm", path.display())),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}.tmp", path.display())),
    ] {
        if candidate.exists() {
            fs::remove_file(candidate)?;
        }
    }
    Ok(())
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
        assert_eq!(table_count(&db, "reconcile_attempts"), 1);
        assert!(file_columns(&db).contains(&"indexed_revision".to_string()));
        assert_eq!(indexed_revision_count(&db), 0);
        assert!(chunk_columns(&db).contains(&"anchor_version".to_string()));
        assert!(chunk_columns(&db).contains(&"normalized_hash".to_string()));
        assert!(chunk_columns(&db).contains(&"start_boundary_hash".to_string()));
        assert!(chunk_columns(&db).contains(&"end_boundary_hash".to_string()));
        assert!(chunk_columns(&db).contains(&"source_revision".to_string()));
        assert_eq!(db.status(&config.database).unwrap().schema.current_version, 2);

        fs::remove_dir_all(root).unwrap();
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
        assert_eq!(table_count(&db, "idx_edges_from_name"), 1);
        assert_eq!(table_count(&db, "idx_edges_to_name"), 1);

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
        assert_eq!(indexed_revision_count(&db), 1);
        assert_eq!(chunk_source_revision_count(&db), 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_requires_explicit_model_install_and_ignores_stale_artifacts() {
        let (root, config) = markdown_config("alpha token\nsecond line\n");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let chunk_id = first_chunk_id(&db);

        let models = db.list_models().unwrap();
        let embedding = models.iter().find(|model| model.model_id == "embedding-small").unwrap();
        assert!(!embedding.installed);
        assert_eq!(embedding.status, "MissingModel");

        let hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].summary, "alpha token\nsecond line");

        let blocked = db.reconcile(Some(1)).unwrap();
        assert_eq!(blocked.processed_chunks, 1);
        assert_eq!(blocked.embeddings_written, 0);
        assert_eq!(blocked.blocked_chunks, 1);
        assert_eq!(blocked.status, "Blocked");

        let status = db.local_ai_status().unwrap();
        assert_eq!(status.embedding.state, "MissingModel");
        assert_eq!(status.embedding.blocked_artifacts, 1);

        db.install_model("embedding-small").unwrap();
        let current = db.reconcile(Some(1)).unwrap();
        assert_eq!(current.embeddings_written, 1);
        assert_eq!(current.status, "Current");
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
        assert_eq!(embedding_bytes, (ai::EMBEDDING_DIM * 4) as i64);

        let hits = db.search("alpha", 10, false).unwrap();
        assert_eq!(hits[0].summary, "alpha token\nsecond line");

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
        let stale_embedding_hits = db.search("alpha", 10, false).unwrap();
        assert!(stale_embedding_hits.is_empty());

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

        let chunk_id = first_chunk_id(&db);
        let papertrail = db.papertrail_for_chunk(chunk_id, 10).unwrap().unwrap();
        assert!(papertrail.current_source.is_some());
        assert!(!papertrail.github_evidence.is_empty());
        assert!(
            papertrail.github_evidence.iter().all(|item| item.evidence_kind == "historical_github")
        );

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
        };

        let db = IndexDatabase::rebuild(&config).unwrap();
        let status = db.status(&config.database).unwrap();
        assert_eq!(status.parser_failures, 1);
        assert_eq!(status.parser_failure_paths[0].path, "src/broken.rs");

        fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_root() -> PathBuf {
        let mut root = std::env::temp_dir();
        let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        root.push(format!("rag-rat-schema-test-{}-{}-{suffix}", std::process::id(), now_ms()));
        root
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
}
