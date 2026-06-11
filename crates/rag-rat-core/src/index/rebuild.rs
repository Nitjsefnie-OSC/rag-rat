use super::*;

impl IndexDatabase {
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
        // RAM-first bulk build: a full rebuild is one big atomic write, so skip per-commit fsyncs
        // (synchronous=OFF) and give SQLite a large page cache. Restored to NORMAL after the
        // rebuild. Only `rebuild` uses this; incremental indexing and the watcher stay durable.
        //
        // NB: stay in WAL — switching journal_mode needs an EXCLUSIVE database lock, which fails
        // ("database is locked") whenever another connection is open (e.g. the watcher, or a
        // concurrent reader). `synchronous` and `cache_size` are per-connection and safe under
        // concurrency. Also do NOT touch `temp_store` — changing it drops the connection_context
        // overlay temp table created by `set_context` above.
        db.storage.execute_batch(
            "PRAGMA synchronous = OFF;
             PRAGMA cache_size = -262144;",
        )?;
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
            // Edges were resolved and inserted in one in-memory pass inside
            // index_targets_with_progress (full rebuild), so there is no separate resolve_edges phase.
            progress(IndexProgress::ResolvingGraph);
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
        // Restore durable fsync behavior for any later writes on this connection (reconcile, etc.).
        // cache_size is left bumped — harmless for the short remaining lifetime of the connection.
        let _ = db.storage.execute_batch("PRAGMA synchronous = NORMAL;");
        result?;
        Ok(db)
    }

    fn clear_full_rebuild_tables(&self) -> anyhow::Result<()> {
        // Stage the active context's file ids, then cascade-delete them and their derived rows.
        self.storage.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS staged_file_ids(id INTEGER PRIMARY KEY);
            DELETE FROM temp.staged_file_ids;
            INSERT OR IGNORE INTO temp.staged_file_ids(id)
            SELECT id
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
             'worktree_id')
              AND worktree_id != '';
            INSERT OR IGNORE INTO temp.staged_file_ids(id)
            SELECT id
            FROM main.files
            WHERE commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != ''
              AND path NOT IN (
                  SELECT path FROM main.files
                  WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
             'worktree_id')
                    AND worktree_id != ''
              );
            ",
        )?;
        self.delete_staged_files_cascade()?;
        self.storage.execute_batch("DELETE FROM temp.staged_file_ids;")?;
        Ok(())
    }

    /// Cascade-delete every derived row (edges, symbols, chunks, embeddings, FTS, blame, docs,
    /// parser failures) for the file ids staged in `temp.staged_file_ids`, then the files
    /// themselves. The caller is responsible for populating and clearing the temp table.
    /// Shared by full rebuild (active context) and GC (dead, non-live contexts).
    pub(super) fn delete_staged_files_cascade(&self) -> anyhow::Result<()> {
        self.storage.execute_batch(
            "
            UPDATE main.edges
            SET to_symbol_id = NULL,
                target_start_line = NULL,
                target_end_line = NULL,
                resolution = 'unresolved'
            WHERE to_symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.edges
            WHERE source_file_id IN (SELECT id FROM temp.staged_file_ids)
               OR from_symbol_id IN (
                    SELECT symbols.id
                    FROM main.symbols
                    JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
               );

            DELETE FROM main.logical_symbol_members
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.logical_symbols
            WHERE id NOT IN (
                SELECT logical_symbol_id FROM main.logical_symbol_members
            );
            DELETE FROM main.symbol_facts
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.chunk_fts
            WHERE rowid IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_summaries
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_embeddings
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.git_chunk_blame
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.docs
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.parser_failures
            WHERE path IN (
                SELECT path
                FROM main.files
                JOIN temp.staged_file_ids ON staged_file_ids.id = files.id
            );
            DELETE FROM main.symbols
            WHERE file_id IN (SELECT id FROM temp.staged_file_ids);
            DELETE FROM main.chunks
            WHERE file_id IN (SELECT id FROM temp.staged_file_ids);
            DELETE FROM main.files
            WHERE id IN (SELECT id FROM temp.staged_file_ids);
            ",
        )?;
        Ok(())
    }
}
