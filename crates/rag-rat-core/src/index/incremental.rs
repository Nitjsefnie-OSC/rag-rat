use super::*;

impl IndexDatabase {
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

    pub(super) fn index_targets_with_progress<F>(
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
}
