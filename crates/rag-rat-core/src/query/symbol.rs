use rusqlite::{Connection, params};
use serde::Serialize;

use crate::language::Language;

#[derive(Debug, Serialize)]
pub struct SymbolHit {
    pub symbol_id: i64,
    pub path: String,
    pub language: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub signature: Option<String>,
    pub docs: Option<String>,
}

pub fn lookup(
    conn: &Connection,
    name: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut sql = "
        SELECT symbols.id, files.path, symbols.language, symbols.name, symbols.qualified_name,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE (symbols.name = ?1 OR symbols.qualified_name LIKE ?2)
    "
    .to_string();
    if language.is_some() {
        sql.push_str(" AND symbols.language = ?3");
    }
    sql.push_str(" ORDER BY CASE WHEN symbols.name = ?1 THEN 0 ELSE 1 END, files.path LIMIT ?");

    let fuzzy = format!("%{name}%");
    let mut stmt = conn.prepare(&sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(SymbolHit {
            symbol_id: row.get(0)?,
            path: row.get(1)?,
            language: row.get(2)?,
            name: row.get(3)?,
            qualified_name: row.get(4)?,
            kind: row.get(5)?,
            start_byte: row.get(6)?,
            end_byte: row.get(7)?,
            signature: row.get(8)?,
            docs: row.get(9)?,
        })
    };
    let rows = if let Some(language) = language {
        stmt.query_map(params![name, fuzzy, language.as_str(), limit], map_row)?
    } else {
        stmt.query_map(params![name, fuzzy, limit], map_row)?
    };

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}
