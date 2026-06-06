use duckdb::Connection;

pub fn apply(conn: &Connection) -> duckdb::Result<()> {
    conn.execute_batch(
        "
        CREATE SEQUENCE IF NOT EXISTS files_id_seq START 1;
        CREATE SEQUENCE IF NOT EXISTS chunks_id_seq START 1;
        CREATE SEQUENCE IF NOT EXISTS symbols_id_seq START 1;
        CREATE SEQUENCE IF NOT EXISTS edges_id_seq START 1;
        CREATE SEQUENCE IF NOT EXISTS docs_id_seq START 1;
        CREATE SEQUENCE IF NOT EXISTS embeddings_id_seq START 1;
        CREATE SEQUENCE IF NOT EXISTS parser_failures_id_seq START 1;

        CREATE TABLE IF NOT EXISTS index_meta(
            key VARCHAR PRIMARY KEY,
            value VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS files(
            id BIGINT PRIMARY KEY DEFAULT nextval('files_id_seq'),
            path VARCHAR NOT NULL UNIQUE,
            language VARCHAR NOT NULL,
            kind VARCHAR NOT NULL,
            sha256 VARCHAR NOT NULL,
            modified_at_ms BIGINT NOT NULL,
            generated BOOLEAN NOT NULL DEFAULT false,
            indexed_at_ms BIGINT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS chunks(
            id BIGINT PRIMARY KEY DEFAULT nextval('chunks_id_seq'),
            file_id BIGINT NOT NULL,
            chunk_kind VARCHAR NOT NULL,
            symbol_path VARCHAR,
            start_byte BIGINT NOT NULL,
            end_byte BIGINT NOT NULL,
            start_line BIGINT NOT NULL,
            end_line BIGINT NOT NULL,
            text VARCHAR NOT NULL,
            text_hash VARCHAR NOT NULL,
            anchor_version BIGINT NOT NULL DEFAULT 1,
            normalized_hash VARCHAR NOT NULL DEFAULT '',
            start_context_hash VARCHAR NOT NULL DEFAULT '',
            end_context_hash VARCHAR NOT NULL DEFAULT '',
            context_radius BIGINT NOT NULL DEFAULT 2
        );

        CREATE TABLE IF NOT EXISTS symbols(
            id BIGINT PRIMARY KEY DEFAULT nextval('symbols_id_seq'),
            file_id BIGINT NOT NULL,
            language VARCHAR NOT NULL,
            name VARCHAR NOT NULL,
            qualified_name VARCHAR NOT NULL,
            kind VARCHAR NOT NULL,
            start_byte BIGINT NOT NULL,
            end_byte BIGINT NOT NULL,
            signature VARCHAR,
            docs VARCHAR
        );

        CREATE TABLE IF NOT EXISTS edges(
            id BIGINT PRIMARY KEY DEFAULT nextval('edges_id_seq'),
            from_symbol_id BIGINT,
            to_symbol_id BIGINT,
            edge_kind VARCHAR NOT NULL,
            confidence DOUBLE NOT NULL DEFAULT 0.5
        );

        CREATE TABLE IF NOT EXISTS docs(
            id BIGINT PRIMARY KEY DEFAULT nextval('docs_id_seq'),
            chunk_id BIGINT NOT NULL,
            source_kind VARCHAR NOT NULL,
            heading_path VARCHAR
        );

        CREATE TABLE IF NOT EXISTS embeddings(
            id BIGINT PRIMARY KEY DEFAULT nextval('embeddings_id_seq'),
            chunk_id BIGINT NOT NULL,
            model_id VARCHAR NOT NULL,
            vector_blob BLOB NOT NULL,
            text_hash VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS parser_failures(
            id BIGINT PRIMARY KEY DEFAULT nextval('parser_failures_id_seq'),
            path VARCHAR NOT NULL,
            language VARCHAR NOT NULL,
            message VARCHAR NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name ON symbols(qualified_name);
        ",
    )
}

pub fn refresh_fts(conn: &Connection) -> anyhow::Result<()> {
    let _ = conn.execute_batch("LOAD fts;");
    let _ = conn.execute_batch("PRAGMA drop_fts_index('chunks');");
    conn.execute_batch(
        "
        PRAGMA create_fts_index(
            'chunks',
            'id',
            'text',
            stemmer = 'porter',
            stopwords = 'english',
            overwrite = 1
        );
        ",
    )?;
    Ok(())
}
