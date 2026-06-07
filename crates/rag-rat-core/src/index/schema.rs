use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const LATEST_SCHEMA_VERSION: u32 = 1;
const DIRTY_MIGRATION_ID: &str = "__dirty__";
const MIGRATION_001_ID: &str = "001_sqlite_storage_baseline";
const MIGRATION_001_CHECKSUM: &str = "sha256:rag-rat-sqlite-baseline-v1";
const MIGRATION_001_DESCRIPTION: &str =
    "SQLite storage baseline with FTS, tree-sitter graph edges, git/GitHub, and local AI metadata";

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
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES (?1, ?2, ?3, ?4)",
        params![MIGRATION_001_ID, now_ms(), MIGRATION_001_CHECKSUM, MIGRATION_001_DESCRIPTION],
    )?;
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
    if migrations.iter().any(|migration| {
        migration.id == MIGRATION_001_ID && migration.checksum != MIGRATION_001_CHECKSUM
    }) {
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
            path TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT ''
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

        CREATE TABLE IF NOT EXISTS edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_file_id INTEGER,
            from_symbol_id INTEGER,
            to_symbol_id INTEGER,
            from_name TEXT,
            to_name TEXT NOT NULL,
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

        CREATE TABLE IF NOT EXISTS ai_models(
            model_id TEXT PRIMARY KEY,
            capability TEXT NOT NULL,
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
            source_text_hash TEXT NOT NULL,
            vector_blob BLOB NOT NULL,
            status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            last_error TEXT,
            UNIQUE(chunk_id, model_id),
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS chunk_summaries(
            chunk_id INTEGER PRIMARY KEY,
            model_id TEXT NOT NULL,
            source_text_hash TEXT NOT NULL,
            summary TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            last_error TEXT,
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS reconcile_attempts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at_ms INTEGER NOT NULL,
            finished_at_ms INTEGER,
            limit_count INTEGER,
            processed_chunks INTEGER NOT NULL DEFAULT 0,
            embeddings_written INTEGER NOT NULL DEFAULT 0,
            summaries_written INTEGER NOT NULL DEFAULT 0,
            blocked_chunks INTEGER NOT NULL DEFAULT 0,
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
        CREATE INDEX IF NOT EXISTS idx_edges_from_symbol ON edges(from_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to_symbol ON edges(to_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_from_name ON edges(from_name);
        CREATE INDEX IF NOT EXISTS idx_edges_to_name ON edges(to_name);
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
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn known_migration(id: &str) -> bool {
    matches!(id, MIGRATION_001_ID | DIRTY_MIGRATION_ID)
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
