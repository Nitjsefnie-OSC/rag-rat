use super::*;

pub(crate) fn parser_failure_count(conn: &Connection) -> anyhow::Result<u64> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM parser_failures", [], |row| row.get(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub(crate) fn historical_evidence(
    conn: &Connection,
    paths: &[String],
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    if paths.is_empty() || surface.len() >= limit {
        return Ok(());
    }
    git_commits_for_paths(conn, paths, surface, limit.saturating_sub(surface.len()))?;
    if surface.len() >= limit {
        return Ok(());
    }
    github_refs_for_paths(conn, paths, surface, limit.saturating_sub(surface.len()))?;
    if surface.len() >= limit {
        return Ok(());
    }
    github_rationale_for_query(conn, query, surface, limit.saturating_sub(surface.len()))?;
    Ok(())
}

pub(crate) fn git_commits_for_paths(
    conn: &Connection,
    paths: &[String],
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let mut remaining = limit;
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind,
               git_commits.hash, git_commits.subject, git_commits.authored_at_s
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
        LEFT JOIN files ON files.path = git_file_changes.path
        WHERE git_file_changes.path = ?1
        ORDER BY git_commits.authored_at_s DESC, git_commits.hash
        LIMIT ?2
        ",
    )?;
    for path in paths {
        if remaining == 0 {
            break;
        }
        let file = file_for_path(conn, path)?;
        let rows =
            stmt.query_map(params![path, i64::try_from(remaining).unwrap_or(i64::MAX)], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?;
        for row in rows {
            let (row_path, language, kind, hash, subject, authored_at_s) = row?;
            let file_symbol = FileSymbol {
                path: row_path.unwrap_or_else(|| file.path.clone()),
                language: language.unwrap_or_else(|| file.language.clone()),
                kind: kind.unwrap_or_else(|| file.kind.clone()),
                symbol: None,
            };
            surface.push(
                ImpactCategory::HistoricalPapertrail,
                file_symbol,
                "git_commit_touched_file",
                format!("{} touched {path} at {authored_at_s}: {subject}", short_hash(&hash)),
            );
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

pub(crate) fn github_refs_for_paths(
    conn: &Connection,
    paths: &[String],
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let mut remaining = limit;
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number, ref_kind, source_kind, source_text
        FROM github_refs
        WHERE source_path = ?1
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    for path in paths {
        if remaining == 0 {
            break;
        }
        let file = file_for_path(conn, path)?;
        let rows =
            stmt.query_map(params![path, i64::try_from(remaining).unwrap_or(i64::MAX)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
        for row in rows {
            let (owner, repo, number, ref_kind, source_kind, source_text) = row?;
            surface.push(
                ImpactCategory::HistoricalPapertrail,
                file.clone(),
                "github_papertrail",
                format!("{owner}/{repo}#{number} {ref_kind}/{source_kind}: {source_text}"),
            );
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

pub(crate) fn github_rationale_for_query(
    conn: &Connection,
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let fts_query = fts_escape(query);
    if fts_query.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "
        SELECT url, title, classification
        FROM github_fts
        WHERE github_fts MATCH ?1
        ORDER BY rank
        LIMIT ?2
        ",
    )?;
    let rows = stmt
        .query_map(params![fts_query, i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
    for row in rows {
        let (url, title, classification) = row?;
        surface.push(
            ImpactCategory::HistoricalPapertrail,
            FileSymbol {
                path: "(github papertrail)".to_string(),
                language: "github".to_string(),
                kind: "papertrail".to_string(),
                symbol: None,
            },
            "github_papertrail",
            format!("{classification}: {title} ({url})"),
        );
    }
    Ok(())
}

pub(crate) fn file_for_path(conn: &Connection, path: &str) -> anyhow::Result<FileSymbol> {
    let row = conn
        .query_row("SELECT path, language, kind FROM files WHERE path = ?1", [path], |row| {
            Ok(FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: None,
            })
        })
        .optional()?;
    Ok(row.unwrap_or_else(|| FileSymbol {
        path: path.to_string(),
        language: "unknown".to_string(),
        kind: "historical".to_string(),
        symbol: None,
    }))
}

pub(crate) fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

pub(crate) fn fts_escape(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub(crate) fn rows_to_items(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<ImpactItem>>,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
