use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const LATEST_SCHEMA_VERSION: u32 = 10;
const DIRTY_MIGRATION_ID: &str = "__dirty__";
const MIGRATION_001_ID: &str = "001_sqlite_storage_baseline";
const MIGRATION_001_CHECKSUM: &str = "sha256:rag-rat-sqlite-baseline-v1";
const MIGRATION_001_DESCRIPTION: &str =
    "SQLite storage baseline with FTS, tree-sitter graph edges, git/GitHub, and local AI metadata";
const MIGRATION_002_ID: &str = "002_embedding_vector_metadata";
const MIGRATION_002_CHECKSUM: &str = "sha256:rag-rat-embedding-vector-metadata-v2";
const MIGRATION_002_DESCRIPTION: &str =
    "Add embedding model dimension metadata and per-vector dimensions for hybrid vector search";
const MIGRATION_003_ID: &str = "003_derived_artifact_reconcile_metadata";
const MIGRATION_003_CHECKSUM: &str = "sha256:rag-rat-derived-artifact-reconcile-metadata-v3";
const MIGRATION_003_DESCRIPTION: &str = "Add model version, retry metadata, summaries, and reconcile meta for diff-based derived artifact reconciliation";
const MIGRATION_004_ID: &str = "004_edge_source_target_spans";
const MIGRATION_004_CHECKSUM: &str = "sha256:rag-rat-edge-source-target-spans-v4";
const MIGRATION_004_DESCRIPTION: &str =
    "Add exact source call-site spans and resolved target line spans to graph edges";
const MIGRATION_005_ID: &str = "005_edge_evidence_and_resolution";
const MIGRATION_005_CHECKSUM: &str = "sha256:rag-rat-edge-evidence-resolution-v5";
const MIGRATION_005_DESCRIPTION: &str =
    "Add raw graph edge evidence, receiver hints, qualified targets, and resolution reasons";
const MIGRATION_006_ID: &str = "006_embedding_policy_and_input_hash";
const MIGRATION_006_CHECKSUM: &str = "sha256:rag-rat-embedding-policy-input-hash-v6";
const MIGRATION_006_DESCRIPTION: &str = "Add embedding eligibility policy, priority, bounded input hash, and reconcile throughput metadata";
const MIGRATION_007_ID: &str = "007_logical_symbol_groups";
const MIGRATION_007_CHECKSUM: &str = "sha256:rag-rat-logical-symbol-groups-v7";
const MIGRATION_007_DESCRIPTION: &str =
    "Add logical symbol groups for cfg variants and duplicate definitions";
const MIGRATION_008_ID: &str = "008_commit_addressable_worktrees";
const MIGRATION_008_CHECKSUM: &str = "sha256:rag-rat-commit-addressable-worktrees-v8";
const MIGRATION_008_DESCRIPTION: &str =
    "Add commit_sha and worktree_id to files table for multi-worktree / multi-branch support";
const MIGRATION_009_ID: &str = "009_github_ref_sync_state";
const MIGRATION_009_CHECKSUM: &str = "sha256:rag-rat-github-ref-sync-state-v9";
const MIGRATION_009_DESCRIPTION: &str =
    "Add per-GitHub-ref sync state for resumable papertrail cache updates";
const MIGRATION_010_ID: &str = "010_symbol_facts";
const MIGRATION_010_CHECKSUM: &str = "sha256:rag-rat-symbol-facts-v10";
const MIGRATION_010_DESCRIPTION: &str =
    "Add normalized symbol facts for parsed language metadata such as Rust attributes";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchemaState {
    Missing,
    Compatible,
    Older,
    Newer,
    Dirty,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppliedMigration {
    pub id: String,
    pub applied_at_ms: i64,
    pub checksum: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaStatus {
    pub state: SchemaState,
    pub current_version: u32,
    pub latest_version: u32,
    pub migrations: Vec<AppliedMigration>,
    pub message: String,
}

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_version(
            id TEXT PRIMARY KEY,
            applied_at_ms INTEGER NOT NULL,
            checksum TEXT NOT NULL,
            description TEXT NOT NULL
        );
        ",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES (?1, ?2, ?3, ?4)",
        params![DIRTY_MIGRATION_ID, now_ms(), "", "partial migration in progress"],
    )?;
    let result = apply_baseline(conn);
    if let Err(err) = result {
        let _ = conn.execute(
            "UPDATE schema_version SET description = ?2 WHERE id = ?1",
            params![DIRTY_MIGRATION_ID, format!("partial migration failed: {err}")],
        );
        return Err(err);
    }
    conn.execute("DELETE FROM schema_version WHERE id = ?1", [DIRTY_MIGRATION_ID])?;
    record_migration(conn, MIGRATION_001_ID, MIGRATION_001_CHECKSUM, MIGRATION_001_DESCRIPTION)?;
    apply_embedding_vector_metadata(conn)?;
    record_migration(conn, MIGRATION_002_ID, MIGRATION_002_CHECKSUM, MIGRATION_002_DESCRIPTION)?;
    apply_derived_artifact_reconcile_metadata(conn)?;
    record_migration(conn, MIGRATION_003_ID, MIGRATION_003_CHECKSUM, MIGRATION_003_DESCRIPTION)?;
    apply_edge_source_target_spans(conn)?;
    record_migration(conn, MIGRATION_004_ID, MIGRATION_004_CHECKSUM, MIGRATION_004_DESCRIPTION)?;
    apply_edge_evidence_and_resolution(conn)?;
    record_migration(conn, MIGRATION_005_ID, MIGRATION_005_CHECKSUM, MIGRATION_005_DESCRIPTION)?;
    apply_embedding_policy_and_input_hash(conn)?;
    record_migration(conn, MIGRATION_006_ID, MIGRATION_006_CHECKSUM, MIGRATION_006_DESCRIPTION)?;
    apply_logical_symbol_groups(conn)?;
    record_migration(conn, MIGRATION_007_ID, MIGRATION_007_CHECKSUM, MIGRATION_007_DESCRIPTION)?;
    apply_commit_addressable_worktrees(conn)?;
    record_migration(conn, MIGRATION_008_ID, MIGRATION_008_CHECKSUM, MIGRATION_008_DESCRIPTION)?;
    apply_github_ref_sync(conn)?;
    record_migration(conn, MIGRATION_009_ID, MIGRATION_009_CHECKSUM, MIGRATION_009_DESCRIPTION)?;
    apply_symbol_facts(conn)?;
    record_migration(conn, MIGRATION_010_ID, MIGRATION_010_CHECKSUM, MIGRATION_010_DESCRIPTION)?;
    Ok(())
}

pub fn status(conn: &Connection) -> anyhow::Result<SchemaStatus> {
    if !table_exists(conn, "schema_version")? {
        let has_legacy_tables = table_exists(conn, "files")? || table_exists(conn, "chunks")?;
        return Ok(if has_legacy_tables {
            SchemaStatus {
                state: SchemaState::Older,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "legacy index schema has no schema_version table; run `rag-rat migrate` or rebuild the derived index with `rag-rat index --full`".to_string(),
            }
        } else {
            SchemaStatus {
                state: SchemaState::Missing,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "index schema is not initialized; run `rag-rat migrate` or build the derived index with `rag-rat index --full`".to_string(),
            }
        });
    }

    let migrations = applied_migrations(conn)?;
    if migrations.iter().any(|migration| migration.id == DIRTY_MIGRATION_ID) {
        return Ok(SchemaStatus {
            state: SchemaState::Dirty,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "dirty or partial schema migration detected; rebuild the derived index with `rag-rat index --full`".to_string(),
        });
    }
    if migrations.iter().any(migration_checksum_mismatch) {
        return Ok(SchemaStatus {
            state: SchemaState::Dirty,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "schema migration checksum mismatch; refusing to open, rebuild the derived index with `rag-rat index --full`".to_string(),
        });
    }
    if migrations.iter().any(|migration| !known_migration(&migration.id)) {
        return Ok(SchemaStatus {
            state: SchemaState::Newer,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "index schema was created by a newer rag-rat; refusing to open".to_string(),
        });
    }
    let current_version = known_version(&migrations);
    if current_version < LATEST_SCHEMA_VERSION {
        return Ok(SchemaStatus {
            state: SchemaState::Older,
            current_version,
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "index schema is older than this rag-rat; run `rag-rat migrate` or rebuild the derived index with `rag-rat index --full`".to_string(),
        });
    }
    Ok(SchemaStatus {
        state: SchemaState::Compatible,
        current_version,
        latest_version: LATEST_SCHEMA_VERSION,
        migrations,
        message: "schema is compatible".to_string(),
    })
}

pub fn check_compatible(conn: &Connection) -> anyhow::Result<()> {
    let status = status(conn)?;
    match status.state {
        SchemaState::Compatible => Ok(()),
        SchemaState::Missing => {
            anyhow::bail!(
                "{}",
                "index schema is not initialized; run `rag-rat migrate`, `rag-rat index`, or `rag-rat index --full`"
            )
        },
        SchemaState::Older => anyhow::bail!("{}", status.message),
        SchemaState::Newer => anyhow::bail!("{}", status.message),
        SchemaState::Dirty => anyhow::bail!("{}", status.message),
    }
}

fn apply_baseline(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS index_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS files(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            UNIQUE(path, commit_sha, worktree_id)
        );

        CREATE TABLE IF NOT EXISTS chunks(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            chunk_kind TEXT NOT NULL,
            symbol_path TEXT,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            text TEXT NOT NULL,
            text_hash TEXT NOT NULL,
            source_revision TEXT NOT NULL DEFAULT '',
            anchor_version INTEGER NOT NULL DEFAULT 1,
            normalized_hash TEXT NOT NULL DEFAULT '',
            start_boundary_hash TEXT NOT NULL DEFAULT '',
            end_boundary_hash TEXT NOT NULL DEFAULT '',
            start_context_hash TEXT NOT NULL DEFAULT '',
            end_context_hash TEXT NOT NULL DEFAULT '',
            context_radius INTEGER NOT NULL DEFAULT 2,
            embedding_policy TEXT NOT NULL DEFAULT 'Embed',
            embedding_priority INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            language TEXT NOT NULL,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            signature TEXT,
            docs TEXT,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS logical_symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            language TEXT NOT NULL,
            path TEXT NOT NULL,
            logical_name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            variant_count INTEGER NOT NULL,
            group_reason TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS logical_symbol_members(
            logical_symbol_id INTEGER NOT NULL,
            symbol_id INTEGER NOT NULL,
            cfg_expr TEXT,
            signature_hash TEXT,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            PRIMARY KEY(logical_symbol_id, symbol_id),
            FOREIGN KEY(logical_symbol_id) REFERENCES logical_symbols(id) ON DELETE CASCADE,
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS symbol_facts(
            symbol_id INTEGER NOT NULL,
            fact_kind TEXT NOT NULL,
            fact_value TEXT NOT NULL,
            PRIMARY KEY(symbol_id, fact_kind, fact_value),
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_file_id INTEGER,
            from_symbol_id INTEGER,
            to_symbol_id INTEGER,
            from_name TEXT,
            to_name TEXT NOT NULL,
            source_start_line INTEGER NOT NULL DEFAULT 0,
            source_end_line INTEGER NOT NULL DEFAULT 0,
            source_start_byte INTEGER NOT NULL DEFAULT 0,
            source_end_byte INTEGER NOT NULL DEFAULT 0,
            target_start_line INTEGER,
            target_end_line INTEGER,
            target_qualified_name TEXT,
            evidence TEXT,
            receiver_hint TEXT,
            resolution TEXT NOT NULL DEFAULT 'unresolved',
            edge_kind TEXT NOT NULL,
            confidence TEXT NOT NULL,
            FOREIGN KEY(source_file_id) REFERENCES files(id) ON DELETE CASCADE,
            FOREIGN KEY(from_symbol_id) REFERENCES symbols(id) ON DELETE SET NULL,
            FOREIGN KEY(to_symbol_id) REFERENCES symbols(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS docs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            source_kind TEXT NOT NULL,
            heading_path TEXT
        );

        CREATE TABLE IF NOT EXISTS parser_failures(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            message TEXT NOT NULL
        );

        DROP TABLE IF EXISTS embeddings;
        DROP TABLE IF EXISTS chunk_summaries;

        CREATE TABLE IF NOT EXISTS ai_models(
            model_id TEXT PRIMARY KEY,
            capability TEXT NOT NULL,
            embedding_dim INTEGER,
            runtime TEXT NOT NULL DEFAULT 'local',
            installed INTEGER NOT NULL DEFAULT 0,
            disabled INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'MissingModel',
            installed_at_ms INTEGER,
            last_error TEXT
        );

        CREATE TABLE IF NOT EXISTS chunk_embeddings(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            model_version TEXT NOT NULL DEFAULT 'v1',
            source_text_hash TEXT NOT NULL,
            input_hash TEXT NOT NULL DEFAULT '',
            embedding_text_version TEXT NOT NULL DEFAULT '',
            embedding_policy TEXT NOT NULL DEFAULT 'Embed',
            embedding_priority INTEGER NOT NULL DEFAULT 1,
            input_chars INTEGER NOT NULL DEFAULT 0,
            input_truncated INTEGER NOT NULL DEFAULT 0,
            embedding_dim INTEGER NOT NULL DEFAULT 0,
            vector_blob BLOB NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error_class TEXT,
            next_retry_after_ms INTEGER,
            computed_at_ms INTEGER,
            created_at_ms INTEGER NOT NULL,
            last_error TEXT,
            UNIQUE(chunk_id, model_id),
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS chunk_summaries(
            chunk_id INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            input_hash TEXT NOT NULL,
            text_hash TEXT NOT NULL,
            summary TEXT NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error_class TEXT,
            next_retry_after_ms INTEGER,
            computed_at_ms INTEGER,
            PRIMARY KEY(chunk_id, model_id, prompt_version),
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS reconcile_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS reconcile_attempts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at_ms INTEGER NOT NULL,
            finished_at_ms INTEGER,
            limit_count INTEGER,
            processed_chunks INTEGER NOT NULL DEFAULT 0,
            embeddings_written INTEGER NOT NULL DEFAULT 0,
            blocked_chunks INTEGER NOT NULL DEFAULT 0,
            elapsed_ms INTEGER NOT NULL DEFAULT 0,
            input_chars INTEGER NOT NULL DEFAULT 0,
            batch_size INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL,
            message TEXT
        );

        CREATE TABLE IF NOT EXISTS git_commits(
            hash TEXT PRIMARY KEY,
            author_name TEXT NOT NULL,
            author_email TEXT NOT NULL,
            authored_at_s INTEGER NOT NULL,
            committed_at_s INTEGER NOT NULL,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            changed_file_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS git_file_changes(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            commit_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            additions INTEGER,
            deletions INTEGER,
            change_kind TEXT NOT NULL DEFAULT 'modified',
            FOREIGN KEY(commit_hash) REFERENCES git_commits(hash) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS git_chunk_blame(
            chunk_id INTEGER PRIMARY KEY,
            source_text_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            line_count INTEGER NOT NULL,
            dominant_commit TEXT,
            dominant_commit_lines INTEGER NOT NULL DEFAULT 0,
            newest_commit TEXT,
            newest_commit_time_s INTEGER,
            oldest_commit TEXT,
            oldest_commit_time_s INTEGER,
            commit_counts_json TEXT NOT NULL,
            computed_at_ms INTEGER NOT NULL,
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS github_refs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            ref_kind TEXT NOT NULL DEFAULT 'unknown',
            source_kind TEXT NOT NULL,
            source_path TEXT,
            source_commit TEXT,
            source_text TEXT NOT NULL,
            discovered_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_issues(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            is_pull_request INTEGER NOT NULL DEFAULT 0,
            synced_at_ms INTEGER NOT NULL,
            UNIQUE(owner, repo, number)
        );

        CREATE TABLE IF NOT EXISTS github_comments(
            id INTEGER PRIMARY KEY,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_pull_requests(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            merged_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            UNIQUE(owner, repo, number)
        );

        CREATE TABLE IF NOT EXISTS github_reviews(
            id INTEGER PRIMARY KEY,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT,
            state TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            submitted_at TEXT,
            synced_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_review_comments(
            id INTEGER PRIMARY KEY,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_ref_sync(
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            last_error TEXT,
            PRIMARY KEY(owner, repo, number)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
            text,
            content='chunks',
            content_rowid='id',
            tokenize='porter'
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS commit_fts USING fts5(
            subject,
            body,
            content='git_commits',
            content_rowid='rowid',
            tokenize='porter'
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS github_fts USING fts5(
            owner,
            repo,
            number UNINDEXED,
            item_kind UNINDEXED,
            item_id UNINDEXED,
            url UNINDEXED,
            title,
            body,
            classification,
            tokenize='porter'
        );

        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name ON symbols(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_symbol_facts_kind_value
            ON symbol_facts(fact_kind, fact_value);
        CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name
            ON logical_symbols(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_logical_symbol_members_symbol
            ON logical_symbol_members(symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_from_symbol ON edges(from_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to_symbol ON edges(to_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_path ON git_file_changes(path);
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_commit ON git_file_changes(commit_hash);
        CREATE INDEX IF NOT EXISTS idx_github_refs_path ON github_refs(source_path);
        CREATE INDEX IF NOT EXISTS idx_github_refs_issue ON github_refs(owner, repo, number);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_github_refs_unique
            ON github_refs(owner, repo, number, source_kind, COALESCE(source_path, ''), COALESCE(source_commit, ''), source_text);
        CREATE INDEX IF NOT EXISTS idx_github_review_comments_path ON github_review_comments(path);
        ",
    )?;
    migrate_files(conn)?;
    migrate_chunks(conn)?;
    migrate_edges(conn)?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_edges_from_name ON edges(from_name);
        CREATE INDEX IF NOT EXISTS idx_edges_to_name ON edges(to_name);
        ",
    )?;
    apply_embedding_vector_metadata(conn)?;
    apply_derived_artifact_reconcile_metadata(conn)?;
    apply_edge_source_target_spans(conn)?;
    apply_embedding_policy_and_input_hash(conn)?;
    apply_logical_symbol_groups(conn)?;
    apply_github_ref_sync(conn)?;
    Ok(())
}

pub fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        DELETE FROM chunk_fts;
        INSERT INTO chunk_fts(rowid, text)
        SELECT id, text FROM chunks;
        DELETE FROM commit_fts;
        INSERT INTO commit_fts(rowid, subject, body)
        SELECT rowid, subject, body FROM git_commits;
        ",
    )?;
    Ok(())
}

fn migrate_files(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "files", "indexed_revision", "TEXT NOT NULL DEFAULT ''")?;
    conn.execute("UPDATE files SET indexed_revision = sha256 WHERE indexed_revision = ''", [])?;
    Ok(())
}

fn migrate_chunks(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "chunks", "source_revision", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "anchor_version", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "chunks", "normalized_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "start_boundary_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "end_boundary_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "start_context_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "end_context_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "context_radius", "INTEGER NOT NULL DEFAULT 2")?;
    add_column_if_missing(conn, "chunks", "embedding_policy", "TEXT NOT NULL DEFAULT 'Embed'")?;
    add_column_if_missing(conn, "chunks", "embedding_priority", "INTEGER NOT NULL DEFAULT 1")?;
    conn.execute(
        "
        UPDATE chunks
        SET source_revision = (
            SELECT files.indexed_revision
            FROM files
            WHERE files.id = chunks.file_id
        )
        WHERE source_revision = ''
        ",
        [],
    )?;
    Ok(())
}

fn migrate_edges(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "source_file_id", "INTEGER")?;
    add_column_if_missing(conn, "edges", "from_name", "TEXT")?;
    add_column_if_missing(conn, "edges", "to_name", "TEXT NOT NULL DEFAULT ''")?;
    apply_edge_source_target_spans(conn)?;
    apply_edge_evidence_and_resolution(conn)?;
    conn.execute(
        "
        UPDATE edges
        SET from_name = COALESCE(from_name, (
                SELECT qualified_name FROM symbols WHERE symbols.id = edges.from_symbol_id
            )),
            to_name = CASE
                WHEN to_name != '' THEN to_name
                ELSE COALESCE((SELECT qualified_name FROM symbols WHERE symbols.id = edges.to_symbol_id), '')
            END
        ",
        [],
    )?;
    conn.execute("DELETE FROM edges WHERE to_name = ''", [])?;
    conn.execute(
        "
        UPDATE edges
        SET confidence = 'NameOnly'
        WHERE confidence NOT IN ('Exact', 'Syntactic', 'NameOnly', 'Ambiguous')
        ",
        [],
    )?;
    Ok(())
}

fn apply_edge_source_target_spans(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "source_start_line", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "source_end_line", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "source_start_byte", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "source_end_byte", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "target_start_line", "INTEGER")?;
    add_column_if_missing(conn, "edges", "target_end_line", "INTEGER")?;
    Ok(())
}

fn apply_edge_evidence_and_resolution(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "target_qualified_name", "TEXT")?;
    add_column_if_missing(conn, "edges", "evidence", "TEXT")?;
    add_column_if_missing(conn, "edges", "receiver_hint", "TEXT")?;
    add_column_if_missing(conn, "edges", "resolution", "TEXT NOT NULL DEFAULT 'unresolved'")?;
    conn.execute(
        "
        UPDATE edges
        SET resolution = CASE
            WHEN to_symbol_id IS NOT NULL AND confidence = 'Exact' THEN 'exact'
            WHEN to_symbol_id IS NOT NULL AND confidence = 'Syntactic' THEN 'syntactic'
            WHEN to_symbol_id IS NOT NULL AND confidence = 'Ambiguous' THEN 'ambiguous'
            WHEN to_symbol_id IS NOT NULL THEN 'name_fallback'
            ELSE COALESCE(NULLIF(resolution, ''), 'unresolved')
        END
        ",
        [],
    )?;
    Ok(())
}

fn apply_embedding_vector_metadata(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "ai_models", "embedding_dim", "INTEGER")?;
    add_column_if_missing(conn, "ai_models", "runtime", "TEXT NOT NULL DEFAULT 'local'")?;
    add_column_if_missing(conn, "chunk_embeddings", "embedding_dim", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute(
        "
        UPDATE ai_models
        SET embedding_dim = CASE
                WHEN capability = 'embedding' THEN COALESCE(embedding_dim, 384)
                ELSE embedding_dim
            END,
            runtime = COALESCE(runtime, 'local')
        ",
        [],
    )?;
    Ok(())
}

fn apply_derived_artifact_reconcile_metadata(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "chunk_embeddings", "model_version", "TEXT NOT NULL DEFAULT 'v1'")?;
    add_column_if_missing(conn, "chunk_embeddings", "attempt_count", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "chunk_embeddings", "last_error_class", "TEXT")?;
    add_column_if_missing(conn, "chunk_embeddings", "next_retry_after_ms", "INTEGER")?;
    add_column_if_missing(conn, "chunk_embeddings", "computed_at_ms", "INTEGER")?;
    conn.execute(
        "
        UPDATE chunk_embeddings
        SET model_version = CASE
                WHEN model_id = 'embedding-hash' AND model_version = 'v1' THEN 'hash-v1'
                WHEN model_id = 'fastembed-all-minilm-l6-v2' AND model_version = 'v1'
                    THEN 'fastembed-all-minilm-l6-v2-v1'
                ELSE model_version
            END,
            computed_at_ms = COALESCE(computed_at_ms, created_at_ms),
            attempt_count = CASE
                WHEN attempt_count = 0 AND status IN ('Current', 'Failed', 'Blocked') THEN 1
                ELSE attempt_count
            END,
            last_error_class = CASE
                WHEN last_error IS NOT NULL AND last_error_class IS NULL THEN status
                ELSE last_error_class
            END
        ",
        [],
    )?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS chunk_summaries(
            chunk_id INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            input_hash TEXT NOT NULL,
            text_hash TEXT NOT NULL,
            summary TEXT NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error_class TEXT,
            next_retry_after_ms INTEGER,
            computed_at_ms INTEGER,
            PRIMARY KEY(chunk_id, model_id, prompt_version),
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS reconcile_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn apply_embedding_policy_and_input_hash(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "chunks", "embedding_policy", "TEXT NOT NULL DEFAULT 'Embed'")?;
    add_column_if_missing(conn, "chunks", "embedding_priority", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "chunk_embeddings", "input_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "embedding_text_version",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "embedding_policy",
        "TEXT NOT NULL DEFAULT 'Embed'",
    )?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "embedding_priority",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(conn, "chunk_embeddings", "input_chars", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "input_truncated",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "reconcile_attempts", "elapsed_ms", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "reconcile_attempts", "input_chars", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "reconcile_attempts", "batch_size", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

fn apply_github_ref_sync(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS github_ref_sync(
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            last_error TEXT,
            PRIMARY KEY(owner, repo, number)
        );
        ",
    )?;
    Ok(())
}

fn apply_symbol_facts(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS symbol_facts(
            symbol_id INTEGER NOT NULL,
            fact_kind TEXT NOT NULL,
            fact_value TEXT NOT NULL,
            PRIMARY KEY(symbol_id, fact_kind, fact_value),
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_symbol_facts_kind_value
            ON symbol_facts(fact_kind, fact_value);
        ",
    )?;
    Ok(())
}

fn apply_logical_symbol_groups(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS logical_symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            language TEXT NOT NULL,
            path TEXT NOT NULL,
            logical_name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            variant_count INTEGER NOT NULL,
            group_reason TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS logical_symbol_members(
            logical_symbol_id INTEGER NOT NULL,
            symbol_id INTEGER NOT NULL,
            cfg_expr TEXT,
            signature_hash TEXT,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            PRIMARY KEY(logical_symbol_id, symbol_id),
            FOREIGN KEY(logical_symbol_id) REFERENCES logical_symbols(id) ON DELETE CASCADE,
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name
            ON logical_symbols(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_logical_symbol_members_symbol
            ON logical_symbol_members(symbol_id);
        ",
    )?;
    Ok(())
}

fn applied_migrations(conn: &Connection) -> anyhow::Result<Vec<AppliedMigration>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, applied_at_ms, checksum, description
        FROM schema_version
        ORDER BY applied_at_ms, id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            id: row.get(0)?,
            applied_at_ms: row.get(1)?,
            checksum: row.get(2)?,
            description: row.get(3)?,
        })
    })?;
    let mut migrations = Vec::new();
    for row in rows {
        migrations.push(row?);
    }
    Ok(migrations)
}

fn known_version(migrations: &[AppliedMigration]) -> u32 {
    migrations
        .iter()
        .filter_map(|migration| match migration.id.as_str() {
            MIGRATION_001_ID => Some(1),
            MIGRATION_002_ID => Some(2),
            MIGRATION_003_ID => Some(3),
            MIGRATION_004_ID => Some(4),
            MIGRATION_005_ID => Some(5),
            MIGRATION_006_ID => Some(6),
            MIGRATION_007_ID => Some(7),
            MIGRATION_008_ID => Some(8),
            MIGRATION_009_ID => Some(9),
            MIGRATION_010_ID => Some(10),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn known_migration(id: &str) -> bool {
    matches!(
        id,
        MIGRATION_001_ID
            | MIGRATION_002_ID
            | MIGRATION_003_ID
            | MIGRATION_004_ID
            | MIGRATION_005_ID
            | MIGRATION_006_ID
            | MIGRATION_007_ID
            | MIGRATION_008_ID
            | MIGRATION_009_ID
            | MIGRATION_010_ID
            | DIRTY_MIGRATION_ID
    )
}

fn migration_checksum_mismatch(migration: &AppliedMigration) -> bool {
    match migration.id.as_str() {
        MIGRATION_001_ID => migration.checksum != MIGRATION_001_CHECKSUM,
        MIGRATION_002_ID => migration.checksum != MIGRATION_002_CHECKSUM,
        MIGRATION_003_ID => migration.checksum != MIGRATION_003_CHECKSUM,
        MIGRATION_004_ID => migration.checksum != MIGRATION_004_CHECKSUM,
        MIGRATION_005_ID => migration.checksum != MIGRATION_005_CHECKSUM,
        MIGRATION_006_ID => migration.checksum != MIGRATION_006_CHECKSUM,
        MIGRATION_007_ID => migration.checksum != MIGRATION_007_CHECKSUM,
        MIGRATION_008_ID => migration.checksum != MIGRATION_008_CHECKSUM,
        MIGRATION_009_ID => migration.checksum != MIGRATION_009_CHECKSUM,
        MIGRATION_010_ID => migration.checksum != MIGRATION_010_CHECKSUM,
        _ => false,
    }
}

fn record_migration(
    conn: &Connection,
    id: &str,
    checksum: &str,
    description: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, now_ms(), checksum, description],
    )?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'virtual table') AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }

    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
}

fn apply_commit_addressable_worktrees(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "files", "commit_sha", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "files", "worktree_id", "TEXT NOT NULL DEFAULT ''")?;
    rebuild_files_table_for_commit_scopes(conn)?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )?;
    Ok(())
}

fn rebuild_files_table_for_commit_scopes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys = OFF;

        CREATE TABLE IF NOT EXISTS files_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            UNIQUE(path, commit_sha, worktree_id)
        );

        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, COALESCE(commit_sha, ''), COALESCE(worktree_id, '')
        FROM files;

        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;

        PRAGMA foreign_keys = ON;
        ",
    )
}
