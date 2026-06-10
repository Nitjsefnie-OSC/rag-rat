use super::*;

impl IndexDatabase {
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
        let text = match fs::read_to_string(&full_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File deleted on disk since indexing — drop it from the index rather than
                // hard-erroring the whole search. The caller (search_with_heal) re-runs
                // search without it. Mirrors read_chunk_current's deletion handling.
                self.mark_file_deleted(path)?;
                return Ok(());
            },
            Err(e) => return Err(e.into()),
        };

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
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id)
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

    pub(super) fn insert_prepared_file(
        &self,
        prepared_file: &PreparedIndexFile,
    ) -> anyhow::Result<()> {
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
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id)
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
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, \
                 start_line, end_line, text, text_hash,
                                    source_revision, anchor_version, normalized_hash, \
                 start_boundary_hash, end_boundary_hash,
                                    start_context_hash, end_context_hash, context_radius, \
                 embedding_policy, embedding_priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19)",
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
            self.storage
                .connection()
                .execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", params![
                    chunk_id, chunk.text
                ])?;
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
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, \
                 end_byte, signature, docs)
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

    pub(super) fn write_git_meta(&self, root: &Path) -> anyhow::Result<()> {
        self.set_meta("git_commit", &git_output(root, &["rev-parse", "HEAD"]).unwrap_or_default())?;
        let dirty = !git_output(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        self.set_meta("git_dirty", if dirty { "true" } else { "false" })?;
        Ok(())
    }

    pub(super) fn apply_prepared_git_history(
        &self,
        root: &Path,
        handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
    ) -> anyhow::Result<GitHistoryIndexStatus> {
        let prepared = join_git_history_prepare(handle)?;
        git_history::apply_prepared(self.storage.connection(), root, prepared)
    }

    pub(super) fn git_history_status(&self) -> anyhow::Result<GitHistoryIndexStatus> {
        let Some(root) = self.storage.source_root() else {
            return git_history::status(self.storage.connection(), Path::new("."));
        };
        git_history::status(self.storage.connection(), root)
    }

    pub(super) fn github_status(&self) -> anyhow::Result<GitHubStatus> {
        github::status(self.storage.connection())
    }

    pub(super) fn mark_fts_dirty(&self) -> anyhow::Result<()> {
        self.set_meta("fts_dirty", "true")
    }

    pub(super) fn resolve_edges(&self) -> anyhow::Result<()> {
        edges::resolve_all_edges(self.storage.connection())
    }

    pub(super) fn rebuild_logical_symbols(&self) -> anyhow::Result<()> {
        // The insert below re-derives the COMPLETE logical-symbol table from all current symbols,
        // so clear it entirely first. A member-join "rebuild set" misses logical_symbols whose
        // members were cascade-deleted with their symbols (clear_full_rebuild_tables deletes
        // files → symbols → logical_symbol_members via FK, but logical_symbols has no such FK).
        // Those orphans would then collide with the deterministic stable id on re-insert.
        self.storage.connection().execute_batch(
            "
            DELETE FROM main.logical_symbol_members;
            DELETE FROM main.logical_symbols;
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
            let logical_symbol_id = key.stable_id();
            self.storage.connection().execute(
                "
                INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name, \
                 kind, variant_count, group_reason)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ",
                params![
                    logical_symbol_id,
                    key.language,
                    key.path,
                    key.name,
                    key.qualified_name,
                    key.kind,
                    i64::try_from(members.len()).unwrap_or(i64::MAX),
                    group_reason,
                ],
            )?;
            for member in members {
                let signature_hash =
                    member.signature.as_deref().map(|signature| hex_sha256(signature.as_bytes()));
                self.storage.connection().execute(
                    "
                    INSERT INTO logical_symbol_members(
                        logical_symbol_id, symbol_id, cfg_expr, signature_hash, start_line, \
                     end_line
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

    pub(super) fn graph_coverage(
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

    pub(super) fn regex_hits(
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

    pub(super) fn current_line_text(
        &self,
        path: &str,
        line: i64,
    ) -> anyhow::Result<Option<String>> {
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

    pub(super) fn ensure_graph_index_current(&self) -> anyhow::Result<()> {
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

    pub(super) fn mark_graph_index_current(&self) -> anyhow::Result<()> {
        self.set_meta("graph_index_version", GRAPH_INDEX_VERSION)
    }

    pub(super) fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub(super) fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
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

    pub(super) fn parser_failure_count(&self) -> anyhow::Result<u64> {
        let count = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM parser_failures",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub(super) fn parser_failure_paths(&self) -> anyhow::Result<Vec<ParserFailure>> {
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

    pub(super) fn search_with_heal(
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

    pub(super) fn chunk_anchor(&self, chunk_id: i64) -> anyhow::Result<ChunkAnchor> {
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

    pub(super) fn mark_file_deleted(&self, path: &Path) -> anyhow::Result<()> {
        let path = path_string(path);
        self.remove_file_in_scope(Path::new(&path), "", &self.active_worktree_id)?;
        self.storage.connection().execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id)
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

    pub(super) fn remove_file_in_scope(
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

    pub(super) fn ensure_fts_fresh(&self) -> anyhow::Result<()> {
        let content_revision = self.content_revision()?;
        let fts_source_revision = self.meta("fts_source_revision")?;
        if !self.fts_dirty()? && fts_source_revision.as_deref() == Some(content_revision.as_str()) {
            return Ok(());
        }
        self.rebuild_fts()?;
        let refreshed_revision = self.meta("fts_source_revision")?;
        if refreshed_revision.as_deref() != Some(content_revision.as_str()) {
            anyhow::bail!(
                "FTS freshness invariant failed: content_revision={content_revision}, \
                 fts_source_revision={}",
                refreshed_revision.unwrap_or_else(|| "<missing>".to_string())
            );
        }
        Ok(())
    }

    pub(super) fn fts_dirty(&self) -> anyhow::Result<bool> {
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

    pub(super) fn indexed_files(&self) -> anyhow::Result<Vec<IndexedFile>> {
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

    pub(super) fn indexed_file_count(&self) -> anyhow::Result<usize> {
        let count =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub(super) fn content_revision(&self) -> anyhow::Result<String> {
        let value = self.storage.connection().query_row(
            "SELECT COALESCE(string_agg(path || ':' || sha256, ',' ORDER BY path), '') FROM files",
            [],
            |row| row.get::<_, String>(0),
        )?;
        Ok(hex_sha256(value.as_bytes()))
    }
}
