use duckdb::{Connection, params};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ImpactItem {
    pub path: String,
    pub language: String,
    pub kind: String,
    pub symbol: Option<String>,
    pub reason: String,
}

pub fn impact_surface(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let like = format!("%{query}%");
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT files.path, files.language, files.kind, symbols.qualified_name
        FROM files
        LEFT JOIN symbols ON symbols.file_id = files.id
        WHERE files.path LIKE ?1 OR symbols.name LIKE ?1 OR symbols.qualified_name LIKE ?1
        ORDER BY files.kind, files.path
        LIMIT ?2
        ",
    )?;
    rows_to_items(stmt.query_map(params![like, limit], |row| {
        Ok(ImpactItem {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            symbol: row.get(3)?,
            reason: "path_or_symbol_match".to_string(),
        })
    })?)
}

pub fn ffi_surface(conn: &Connection, limit: u32) -> anyhow::Result<Vec<ImpactItem>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT files.path, files.language, files.kind, symbols.qualified_name
        FROM files
        LEFT JOIN symbols ON symbols.file_id = files.id
        LEFT JOIN chunks ON chunks.file_id = files.id
        WHERE chunks.text LIKE '%uniffi::export%'
           OR files.path LIKE '%generated%'
           OR symbols.name LIKE '%NativeHeldCore%'
           OR chunks.text LIKE '%NativeHeldCore%'
        ORDER BY files.kind DESC, files.path
        LIMIT ?1
        ",
    )?;
    rows_to_items(stmt.query_map([limit], |row| {
        Ok(ImpactItem {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            symbol: row.get(3)?,
            reason: "ffi_candidate".to_string(),
        })
    })?)
}

fn rows_to_items(
    rows: duckdb::MappedRows<'_, impl FnMut(&duckdb::Row<'_>) -> duckdb::Result<ImpactItem>>,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}
