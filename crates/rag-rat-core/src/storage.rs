use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StorageStatus {
    pub backend: &'static str,
    pub sqlite_version: String,
    pub fts5_available: bool,
}

#[derive(Debug)]
pub struct IndexConnection {
    conn: Connection,
    database_path: PathBuf,
    source_root: Option<PathBuf>,
}

impl IndexConnection {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let storage = Self { conn, database_path: path.to_path_buf(), source_root: None };
        storage.setup()?;
        Ok(storage)
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn source_root(&self) -> Option<&Path> {
        self.source_root.as_deref()
    }

    pub fn set_source_root(&mut self, source_root: PathBuf) {
        self.source_root = Some(source_root);
    }

    pub fn execute_batch(&self, sql: &str) -> anyhow::Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    pub fn status(&self) -> anyhow::Result<StorageStatus> {
        let sqlite_version =
            self.conn.query_row("SELECT sqlite_version()", [], |row| row.get::<_, String>(0))?;
        Ok(StorageStatus {
            backend: "sqlite",
            sqlite_version,
            fts5_available: self.fts5_available(),
        })
    }

    fn setup(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            ",
        )?;
        Ok(())
    }

    fn fts5_available(&self) -> bool {
        self.conn
            .execute_batch(
                "
                CREATE VIRTUAL TABLE temp.rag_rat_fts_probe USING fts5(text);
                DROP TABLE temp.rag_rat_fts_probe;
                ",
            )
            .is_ok()
    }
}
