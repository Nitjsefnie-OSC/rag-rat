use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct GraphHop {
    pub from_symbol: Option<String>,
    pub to_symbol: Option<String>,
    pub edge_kind: String,
    pub confidence: String,
}

pub fn traverse(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
) -> anyhow::Result<Vec<GraphHop>> {
    let (match_alias, match_name, other_name) = if reverse {
        ("to_symbols", "to_name", "from_name")
    } else {
        ("from_symbols", "from_name", "to_name")
    };
    let sql = format!(
        "
        SELECT COALESCE(from_symbols.qualified_name, edges.from_name),
               COALESCE(to_symbols.qualified_name, edges.to_name),
               edges.edge_kind,
               edges.confidence
        FROM edges
        LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
        LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
        WHERE {match_alias}.name = ?1
           OR {match_alias}.qualified_name LIKE ?2
           OR edges.{match_name} = ?1
           OR edges.{match_name} LIKE ?2
        ORDER BY
            CASE edges.confidence
                WHEN 'Exact' THEN 0
                WHEN 'Syntactic' THEN 1
                WHEN 'NameOnly' THEN 2
                ELSE 3
            END,
            edges.edge_kind,
            edges.{other_name}
        LIMIT ?3
        ",
        match_alias = match_alias,
        match_name = match_name,
        other_name = other_name,
    );
    let fuzzy = format!("%{symbol}%");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map((&symbol, &fuzzy, limit), |row| {
        Ok(GraphHop {
            from_symbol: row.get(0)?,
            to_symbol: row.get(1)?,
            edge_kind: row.get(2)?,
            confidence: row.get(3)?,
        })
    })?;
    let mut hops = Vec::new();
    for row in rows {
        hops.push(row?);
    }
    Ok(hops)
}
