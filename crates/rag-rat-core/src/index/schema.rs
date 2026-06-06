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
            indexed_at_ms INTEGER NOT NULL
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
            anchor_version INTEGER NOT NULL DEFAULT 1,
            normalized_hash TEXT NOT NULL DEFAULT '',
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

        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
            text,
            content='chunks',
            content_rowid='id',
            tokenize='porter'
        );

        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name ON symbols(qualified_name);
        ",
    )
}

pub fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        DELETE FROM chunk_fts;
        INSERT INTO chunk_fts(rowid, text)
        SELECT id, text FROM chunks;
        ",
    )?;
    Ok(())
}
