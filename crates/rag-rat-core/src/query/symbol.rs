use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::language::Language;

#[derive(Debug, Serialize)]
pub struct SymbolHit {
    pub symbol_id: i64,
    pub file_id: i64,
    pub path: String,
    pub file_kind: String,
    pub language: String,
    pub name: String,
    pub qualified_name: String,
    pub symbol_path: String,
    pub kind: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub signature: Option<String>,
    pub docs: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SymbolLookup {
    pub candidates: Vec<SymbolHit>,
    pub disambiguation_required: bool,
}

#[derive(Debug, Clone)]
pub struct SymbolSelector {
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub symbol: Option<String>,
    pub language: Option<Language>,
    pub allow_ambiguous: bool,
    pub limit: u32,
}

#[derive(Debug, Serialize)]
pub struct SymbolDisambiguation {
    pub candidates: Vec<SymbolHit>,
    pub disambiguation_required: bool,
}

pub fn lookup(
    conn: &Connection,
    name: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    lookup_name(conn, name, language, limit)
}

pub fn lookup_candidates(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<SymbolLookup> {
    let candidates = candidates_for_selector(conn, selector)?;
    Ok(SymbolLookup {
        disambiguation_required: needs_disambiguation(&candidates, selector.allow_ambiguous),
        candidates,
    })
}

pub fn select_one(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<Result<Option<SymbolHit>, SymbolDisambiguation>> {
    let mut candidates = candidates_for_selector(conn, selector)?;
    if candidates.is_empty() {
        return Ok(Ok(None));
    }
    if needs_disambiguation(&candidates, selector.allow_ambiguous) {
        return Ok(Err(SymbolDisambiguation { candidates, disambiguation_required: true }));
    }
    Ok(Ok(Some(candidates.remove(0))))
}

pub fn lookup_by_id(conn: &Connection, symbol_id: i64) -> anyhow::Result<Option<SymbolHit>> {
    conn.query_row(
        "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, symbols.qualified_name,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.id = ?1
        ",
        [symbol_id],
        symbol_hit_row,
    )
    .optional()
    .map_err(Into::into)
}

fn candidates_for_selector(
    conn: &Connection,
    selector: &SymbolSelector,
) -> anyhow::Result<Vec<SymbolHit>> {
    if let Some(symbol_id) = selector.symbol_id {
        return Ok(lookup_by_id(conn, symbol_id)?.into_iter().collect());
    }
    if let Some(symbol_path) = selector.symbol_path.as_deref() {
        return lookup_symbol_path(conn, symbol_path, selector.language, selector.limit);
    }
    let Some(symbol) = selector.symbol.as_deref() else {
        anyhow::bail!("one of symbol_id, symbol_path, or symbol is required");
    };
    lookup_name(conn, symbol, selector.language, selector.limit)
}

fn lookup_name(
    conn: &Connection,
    name: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut sql = "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, symbols.qualified_name,
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
    let rows = if let Some(language) = language {
        stmt.query_map(params![name, fuzzy, language.as_str(), limit], symbol_hit_row)?
    } else {
        stmt.query_map(params![name, fuzzy, limit], symbol_hit_row)?
    };

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

fn lookup_symbol_path(
    conn: &Connection,
    symbol_path: &str,
    language: Option<Language>,
    limit: u32,
) -> anyhow::Result<Vec<SymbolHit>> {
    let mut sql = "
        SELECT symbols.id, files.id, files.path, files.kind, symbols.language, symbols.name, symbols.qualified_name,
               symbols.kind, symbols.start_byte, symbols.end_byte, symbols.signature, symbols.docs
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.qualified_name = ?1
    "
    .to_string();
    if language.is_some() {
        sql.push_str(" AND symbols.language = ?2");
    }
    sql.push_str(" ORDER BY files.path, symbols.start_byte LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(language) = language {
        stmt.query_map(params![symbol_path, language.as_str(), limit], symbol_hit_row)?
    } else {
        stmt.query_map(params![symbol_path, limit], symbol_hit_row)?
    };

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

fn needs_disambiguation(candidates: &[SymbolHit], allow_ambiguous: bool) -> bool {
    !allow_ambiguous && candidates.len() > 1
}

fn symbol_hit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolHit> {
    let qualified_name = row.get(6)?;
    Ok(SymbolHit {
        symbol_id: row.get(0)?,
        file_id: row.get(1)?,
        path: row.get(2)?,
        file_kind: row.get(3)?,
        language: row.get(4)?,
        name: row.get(5)?,
        symbol_path: qualified_name,
        qualified_name: row.get(6)?,
        kind: row.get(7)?,
        start_byte: row.get(8)?,
        end_byte: row.get(9)?,
        signature: row.get(10)?,
        docs: row.get(11)?,
    })
}
