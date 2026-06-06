use duckdb::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct GraphHop {
    pub from_symbol: Option<String>,
    pub to_symbol: Option<String>,
    pub edge_kind: String,
    pub confidence: f64,
}

pub fn traverse(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
) -> anyhow::Result<Vec<GraphHop>> {
    let direction = if reverse {
        ("to_symbols", "from_symbols", "to_symbol_id", "from_symbol_id")
    } else {
        ("from_symbols", "to_symbols", "from_symbol_id", "to_symbol_id")
    };
    let sql = format!(
        "
        SELECT {from_alias}.qualified_name, {to_alias}.qualified_name, edges.edge_kind, edges.confidence
        FROM edges
        LEFT JOIN symbols {from_alias} ON {from_alias}.id = edges.{from_col}
        LEFT JOIN symbols {to_alias} ON {to_alias}.id = edges.{to_col}
        WHERE {from_alias}.name = ?1 OR {from_alias}.qualified_name LIKE ?2
        LIMIT ?3
        ",
        from_alias = direction.0,
        to_alias = direction.1,
        from_col = direction.2,
        to_col = direction.3,
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
