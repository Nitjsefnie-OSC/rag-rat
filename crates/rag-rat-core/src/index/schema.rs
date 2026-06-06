use rusqlite::Connection;

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
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
            from_symbol_id INTEGER,
            to_symbol_id INTEGER,
            edge_kind TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 0.5
        );

        CREATE TABLE IF NOT EXISTS docs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            source_kind TEXT NOT NULL,
            heading_path TEXT
        );

        CREATE TABLE IF NOT EXISTS embeddings(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            vector_blob BLOB NOT NULL,
            text_hash TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS parser_failures(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            message TEXT NOT NULL
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
