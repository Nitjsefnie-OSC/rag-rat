pub mod anchors;
pub mod chunker;
pub mod edges;
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

use crate::{
    config::{Config, TargetKind},
    index::{
        anchors::{AnchorStatus, ChunkAnchor},
        chunker::Chunk,
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
        full_rebuild: bool,
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

#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub database: String,
    pub exists: bool,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
    pub indexed_at_ms: Option<i64>,
    pub fts_synced_at_ms: Option<i64>,
    pub fts_source_revision: Option<String>,
    pub fts_fresh: bool,
    pub file_count_by_language: BTreeMap<String, u64>,
    pub parser_failures: u64,
}

impl IndexDatabase {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut storage = IndexConnection::open(path)?;
        schema::apply(storage.connection())?;
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
        progress(IndexProgress::Started { database: config.database.clone(), full_rebuild: true });
        remove_database_files(&config.database)?;
        let db = Self::open(&config.database)?;
        let result = (|| -> anyhow::Result<()> {
            db.storage.execute_batch("BEGIN TRANSACTION")?;
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.write_git_meta(&config.root)?;
            let indexed = db.index_targets_with_progress(config, &mut progress)?;
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
        if !config.database.exists() {
            return Self::rebuild_with_progress(config, progress);
        }

        progress(IndexProgress::Started { database: config.database.clone(), full_rebuild: false });
        let db = Self::open(&config.database)?;
        let result = (|| -> anyhow::Result<()> {
            db.storage.execute_batch("BEGIN TRANSACTION")?;
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.write_git_meta(&config.root)?;
            let indexed = db.index_changed_files_with_progress(config, &mut progress)?;
            if indexed > 0 {
                db.record_fts_current()?;
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
        progress(IndexProgress::Discovered { files: files.len() });

        for path in changes.deleted {
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

        Ok(files.len())
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

        Ok(IndexStatus {
            database: database.display().to_string(),
            exists: database.exists(),
            git_commit: self.meta("git_commit")?,
            git_dirty: self.meta("git_dirty")?.map(|value| value == "true"),
            indexed_at_ms: self.meta("indexed_at_ms")?.and_then(|value| value.parse::<i64>().ok()),
            fts_synced_at_ms: self
                .meta("fts_synced_at_ms")?
                .and_then(|value| value.parse::<i64>().ok()),
            fts_fresh: fts_source_revision.as_deref() == Some(content_revision.as_str()),
            fts_source_revision,
            file_count_by_language: counts,
            parser_failures: self.parser_failure_count()?,
        })
    }

    pub fn storage_status(&self) -> anyhow::Result<StorageStatus> {
        self.storage.status()
    }

    pub fn search(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
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
            Err(_) => return Ok(Some(chunk)),
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
            AnchorStatus::Exact => Ok(Some(chunk)),
            AnchorStatus::Relocated { start_line, end_line, text } => {
                chunk.start_line = i64::try_from(start_line)?;
                chunk.end_line = i64::try_from(end_line)?;
                chunk.text = text;
                Ok(Some(chunk))
            },
            AnchorStatus::Stale => {
                self.heal_file(Path::new(&chunk.path))?;
                self.record_fts_current()?;
                crate::query::read_chunk(self.storage.connection(), chunk_id)
            },
        }
    }

    pub fn docs_for_symbol(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
        self.search(symbol, limit, true)
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
        self.record_fts_current()
    }

    pub fn record_fts_current(&self) -> anyhow::Result<()> {
        self.set_meta("fts_synced_at_ms", &now_ms().to_string())?;
        let revision = self.content_revision()?;
        self.set_meta("fts_source_revision", &revision)?;
        Ok(())
    }

    pub fn heal_file(&self, path: &Path) -> anyhow::Result<()> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        let row = self.file_row(path)?;
        let full_path = root.join(path);
        let text = fs::read_to_string(&full_path)?;
        self.remove_file(path)?;
        self.index_file(path, row.language, row.kind, file_metadata_ms(&full_path)?, &text)
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
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id",
            params![
                path_string(path),
                language.as_str(),
                kind.as_str(),
                sha256,
                modified_at_ms,
                matches!(kind, TargetKind::Generated),
                now_ms(),
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
        self.insert_chunks(file_id, &chunks, text)?;
        self.insert_symbols(file_id, language, &symbols)?;
        Ok(())
    }

    fn insert_chunks(&self, file_id: i64, chunks: &[Chunk], full_text: &str) -> anyhow::Result<()> {
        for chunk in chunks {
            let anchor =
                anchors::anchor_for_text(&chunk.text, chunk.start_line, chunk.end_line, full_text);
            self.storage.connection().execute(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line, text, text_hash,
                                    anchor_version, normalized_hash, start_context_hash, end_context_hash, context_radius)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
                    anchor.version,
                    anchor.normalized_hash,
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
        for path in stale.into_iter().take(4) {
            self.heal_file(Path::new(&path))?;
        }
        self.record_fts_current()?;
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
            let current = hex_sha256(text.as_bytes());
            let stored = self.storage.connection().query_row(
                "SELECT sha256 FROM files WHERE path = ?1",
                [&hit.path],
                |row| row.get::<_, String>(0),
            )?;
            if current != stored {
                stale.push(hit.path.clone());
            }
        }
        Ok(stale)
    }

    fn chunk_anchor(&self, chunk_id: i64) -> anyhow::Result<ChunkAnchor> {
        Ok(self.storage.connection().query_row(
            "
            SELECT anchor_version, normalized_hash, start_context_hash, end_context_hash, context_radius
            FROM chunks WHERE id = ?1
            ",
            [chunk_id],
            |row| {
                Ok(ChunkAnchor {
                    version: row.get(0)?,
                    normalized_hash: row.get(1)?,
                    start_context_hash: row.get(2)?,
                    end_context_hash: row.get(3)?,
                    context_radius: row.get(4)?,
                })
            },
        )?)
    }

    fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
        let path = path_string(path);
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
        Ok(())
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
struct IndexFile {
    full_path: PathBuf,
    relative_path: PathBuf,
    language: Language,
    kind: TargetKind,
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
    use super::*;
    use crate::config::ResolvedTarget;

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
        assert!(chunk_columns(&db).contains(&"anchor_version".to_string()));
        assert!(chunk_columns(&db).contains(&"normalized_hash".to_string()));

        fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_root() -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("rag-rat-schema-test-{}-{}", std::process::id(), now_ms()));
        root
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
        let mut stmt = db.storage.connection().prepare("PRAGMA table_info(chunks)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
    }
}
