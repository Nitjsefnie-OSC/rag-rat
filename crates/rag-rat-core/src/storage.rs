use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::Connection;

#[derive(Debug)]
pub struct IndexConnection {
    conn: Connection,
    source_root: Option<PathBuf>,
}

impl IndexConnection {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let storage = Self { conn, source_root: None };
        storage.setup()?;
        Ok(storage)
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
}
