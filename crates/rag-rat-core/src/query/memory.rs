use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemory {
    pub memory_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub confidence: String,
    pub status: String,
    pub created_by: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub source: String,
    pub source_text_hash: Option<String>,
    pub input_hash: Option<String>,
    pub memory_version: String,
    pub bindings: Vec<RepoMemoryBinding>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryBinding {
    pub memory_id: String,
    pub binding_kind: String,
    pub binding_id: String,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub edge_id: Option<i64>,
    pub commit_hash: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
    pub anchor_status: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryCreateResult {
    pub memory: RepoMemory,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoMemoryCreate {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub confidence: String,
    pub created_by: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub bind: RepoMemoryBindTarget,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoMemoryBindTarget {
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub commit_hash: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoMemoryUpdate {
    pub memory_id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub confidence: Option<String>,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryValidationReport {
    pub checked: u64,
    pub current: u64,
    pub relocated: u64,
    pub stale: u64,
    pub gone: u64,
    pub unverified: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryEvidence {
    pub direct: Vec<RepoMemory>,
    pub stale: Vec<RepoMemory>,
}

#[derive(Debug)]
struct ResolvedBinding {
    binding_kind: String,
    binding_id: String,
    path: Option<String>,
    start_line: Option<i64>,
    end_line: Option<i64>,
    logical_symbol_id: Option<i64>,
    symbol_id: Option<i64>,
    chunk_id: Option<i64>,
    commit_hash: Option<String>,
    github_owner: Option<String>,
    github_repo: Option<String>,
    github_number: Option<i64>,
    source_text_hash: Option<String>,
    anchor_status: String,
}

pub fn create_memory(
    conn: &Connection,
    request: RepoMemoryCreate,
) -> anyhow::Result<RepoMemoryCreateResult> {
    validate_kind(&request.kind)?;
    validate_confidence(&request.confidence)?;
    validate_len("title", &request.title, 160)?;
    validate_len("body", &request.body, 4000)?;
    let source = request.source.clone().unwrap_or_else(|| "agent".to_string());
    validate_source(&source)?;
    let binding = resolve_binding(conn, &request.bind)?;
    let input_hash = memory_input_hash(&request.kind, &request.title, &request.body, &request.tags);
    if let Some(existing_id) = duplicate_memory_id(conn, &request.title, &request.body, &binding)? {
        let memory = memory_by_id(conn, &existing_id)?
            .ok_or_else(|| anyhow::anyhow!("duplicate memory `{existing_id}` disappeared"))?;
        return Ok(RepoMemoryCreateResult { memory, duplicate: true });
    }

    let now = now_ms();
    let id = memory_id(now, &input_hash);
    conn.execute(
        "
        INSERT INTO repo_memories(
            id, kind, title, body, confidence, status, created_by, created_at_ms, updated_at_ms,
            source, source_text_hash, input_hash, memory_version
        )
        VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?7, ?8, ?9, ?10, 'v1')
        ",
        params![
            id,
            request.kind,
            request.title,
            request.body,
            request.confidence,
            request.created_by,
            now,
            source,
            binding.source_text_hash,
            input_hash
        ],
    )?;
    insert_binding(conn, &id, &binding, now)?;
    replace_tags(conn, &id, &request.tags)?;
    upsert_memory_fts(conn, &id)?;
    let memory = memory_by_id(conn, &id)?
        .ok_or_else(|| anyhow::anyhow!("created memory `{id}` could not be read back"))?;
    Ok(RepoMemoryCreateResult { memory, duplicate: false })
}

pub fn update_memory(conn: &Connection, update: RepoMemoryUpdate) -> anyhow::Result<RepoMemory> {
    let current = memory_by_id(conn, &update.memory_id)?
        .ok_or_else(|| anyhow::anyhow!("memory `{}` not found", update.memory_id))?;
    if let Some(kind) = update.kind.as_deref() {
        validate_kind(kind)?;
    }
    if let Some(confidence) = update.confidence.as_deref() {
        validate_confidence(confidence)?;
    }
    if let Some(status) = update.status.as_deref() {
        validate_status(status)?;
    }
    if let Some(title) = update.title.as_deref() {
        validate_len("title", title, 160)?;
    }
    if let Some(body) = update.body.as_deref() {
        validate_len("body", body, 4000)?;
    }
    let now = now_ms();
    conn.execute(
        "
        UPDATE repo_memories
        SET kind = ?2,
            title = ?3,
            body = ?4,
            confidence = ?5,
            status = ?6,
            updated_at_ms = ?7
        WHERE id = ?1
        ",
        params![
            update.memory_id,
            update.kind.unwrap_or(current.kind),
            update.title.unwrap_or(current.title),
            update.body.unwrap_or(current.body),
            update.confidence.unwrap_or(current.confidence),
            update.status.unwrap_or(current.status),
            now
        ],
    )?;
    if let Some(tags) = update.tags {
        replace_tags(conn, &update.memory_id, &tags)?;
    }
    upsert_memory_fts(conn, &update.memory_id)?;
    memory_by_id(conn, &update.memory_id)?.ok_or_else(|| {
        anyhow::anyhow!("updated memory `{}` could not be read back", update.memory_id)
    })
}

pub fn mark_obsolete(conn: &Connection, memory_id: &str) -> anyhow::Result<RepoMemory> {
    update_memory(
        conn,
        RepoMemoryUpdate {
            memory_id: memory_id.to_string(),
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: Some("obsolete".to_string()),
            tags: None,
        },
    )
}

pub fn memory_by_id(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<RepoMemory>> {
    let Some(mut memory) = conn
        .query_row(
            "
            SELECT id, kind, title, body, confidence, status, created_by, created_at_ms,
                   updated_at_ms, source, source_text_hash, input_hash, memory_version
            FROM repo_memories
            WHERE id = ?1
            ",
            [memory_id],
            memory_row,
        )
        .optional()?
    else {
        return Ok(None);
    };
    attach_memory_children(conn, &mut memory)?;
    Ok(Some(memory))
}

pub fn memories_for_chunk(
    conn: &Connection,
    chunk_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        LEFT JOIN chunks ON chunks.id = ?1
        LEFT JOIN files ON files.id = chunks.file_id
        WHERE repo_memories.status IN ('active', 'stale')
          AND (
              repo_memory_bindings.chunk_id = ?1
              OR (files.path IS NOT NULL AND repo_memory_bindings.path = files.path)
          )
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?2
        ",
    )?;
    ids_to_memories(
        conn,
        stmt.query_map(params![chunk_id, i64::from(limit)], |row| row.get::<_, String>(0))?,
    )
}

pub fn memories_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale')
          AND repo_memory_bindings.path = ?1
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?2
        ",
    )?;
    ids_to_memories(conn, stmt.query_map(params![path, i64::from(limit)], |row| row.get(0))?)
}

pub fn memories_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let chunk_ids = chunk_ids_for_symbol(conn, symbol)?;
    let mut candidate_ids = BTreeSet::new();
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale')
          AND (
              repo_memory_bindings.logical_symbol_id = ?1
              OR repo_memory_bindings.symbol_id = ?2
              OR repo_memory_bindings.binding_id = ?3
              OR repo_memory_bindings.path = ?4
          )
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?5
        ",
    )?;
    let rows = stmt.query_map(
        params![
            symbol.logical_symbol_id,
            symbol.symbol_id,
            symbol.qualified_name,
            symbol.path,
            i64::from(limit)
        ],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        candidate_ids.insert(row?);
    }
    if !chunk_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", chunk_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "
            SELECT DISTINCT repo_memories.id
            FROM repo_memories
            JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
            WHERE repo_memories.status IN ('active', 'stale')
              AND repo_memory_bindings.chunk_id IN ({placeholders})
            ORDER BY repo_memories.updated_at_ms DESC
            LIMIT ?
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut values =
            chunk_ids.iter().map(|id| rusqlite::types::Value::Integer(*id)).collect::<Vec<_>>();
        values.push(rusqlite::types::Value::Integer(i64::from(limit)));
        let rows =
            stmt.query_map(rusqlite::params_from_iter(values), |row| row.get::<_, String>(0))?;
        for row in rows {
            candidate_ids.insert(row?);
        }
    }
    let mut memories = Vec::new();
    for id in candidate_ids.into_iter().take(usize::try_from(limit).unwrap_or(usize::MAX)) {
        if let Some(memory) = memory_by_id(conn, &id)? {
            memories.push(memory);
        }
    }
    memories.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    Ok(memories)
}

pub fn memory_evidence_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<RepoMemoryEvidence> {
    split_active_stale(memories_for_symbol(conn, symbol, limit)?)
}

pub fn memory_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let query = fts_query(query);
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memory_fts.memory_id
        FROM repo_memory_fts
        JOIN repo_memories ON repo_memories.id = repo_memory_fts.memory_id
        WHERE repo_memory_fts MATCH ?1
          AND repo_memories.status IN ('active', 'stale')
        ORDER BY bm25(repo_memory_fts)
        LIMIT ?2
        ",
    )?;
    ids_to_memories(conn, stmt.query_map(params![query, i64::from(limit)], |row| row.get(0))?)
}

pub fn validate_memories(conn: &Connection) -> anyhow::Result<RepoMemoryValidationReport> {
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, binding_kind, binding_id, path, start_line, end_line,
               logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, github_owner,
               github_repo, github_number, anchor_status, created_at_ms
        FROM repo_memory_bindings
        ",
    )?;
    let rows = stmt.query_map([], binding_row)?;
    let mut report = RepoMemoryValidationReport {
        checked: 0,
        current: 0,
        relocated: 0,
        stale: 0,
        gone: 0,
        unverified: 0,
    };
    for row in rows {
        let mut binding = row?;
        report.checked += 1;
        let status = validate_binding(conn, &mut binding)?;
        conn.execute(
            "
            UPDATE repo_memory_bindings
            SET anchor_status = ?3,
                logical_symbol_id = ?4,
                symbol_id = ?5,
                chunk_id = ?6,
                path = ?7,
                start_line = ?8,
                end_line = ?9
            WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?10
            ",
            params![
                binding.memory_id,
                binding.binding_kind,
                status,
                binding.logical_symbol_id,
                binding.symbol_id,
                binding.chunk_id,
                binding.path,
                binding.start_line,
                binding.end_line,
                binding.binding_id
            ],
        )?;
        match status.as_str() {
            "current" => report.current += 1,
            "relocated" => report.relocated += 1,
            "stale" => report.stale += 1,
            "gone" => report.gone += 1,
            _ => report.unverified += 1,
        }
    }
    Ok(report)
}

fn resolve_binding(
    conn: &Connection,
    bind: &RepoMemoryBindTarget,
) -> anyhow::Result<ResolvedBinding> {
    if let Some(logical_symbol_id) = bind.logical_symbol_id {
        return resolve_logical_symbol_binding(conn, logical_symbol_id);
    }
    if let Some(symbol_id) = bind.symbol_id {
        return resolve_symbol_binding(conn, symbol_id);
    }
    if let Some(chunk_id) = bind.chunk_id {
        return resolve_chunk_binding(conn, chunk_id);
    }
    if let Some(path) = bind.path.as_deref() {
        return resolve_path_binding(conn, path, bind.start_line, bind.end_line);
    }
    if let Some(commit_hash) = bind.commit_hash.as_deref() {
        return Ok(ResolvedBinding {
            binding_kind: "commit".to_string(),
            binding_id: commit_hash.to_string(),
            path: None,
            start_line: None,
            end_line: None,
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            commit_hash: Some(commit_hash.to_string()),
            github_owner: None,
            github_repo: None,
            github_number: None,
            source_text_hash: None,
            anchor_status: "unverified".to_string(),
        });
    }
    if let (Some(owner), Some(repo), Some(number)) =
        (bind.github_owner.as_deref(), bind.github_repo.as_deref(), bind.github_number)
    {
        return Ok(ResolvedBinding {
            binding_kind: "github".to_string(),
            binding_id: format!("{owner}/{repo}#{number}"),
            path: None,
            start_line: None,
            end_line: None,
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            commit_hash: None,
            github_owner: Some(owner.to_string()),
            github_repo: Some(repo.to_string()),
            github_number: Some(number),
            source_text_hash: None,
            anchor_status: "unverified".to_string(),
        });
    }
    anyhow::bail!(
        "memory_create requires logical_symbol_id, symbol_id, chunk_id, path/span, commit_hash, or github ref binding"
    )
}

fn resolve_logical_symbol_binding(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<ResolvedBinding> {
    let logical = crate::query::symbol::lookup_logical_by_id(conn, logical_symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("logical_symbol_id {logical_symbol_id} not found"))?;
    let chunk = chunk_for_logical_symbol(conn, logical_symbol_id)?;
    Ok(ResolvedBinding {
        binding_kind: "logical_symbol".to_string(),
        binding_id: logical.qualified_name,
        path: Some(logical.path),
        start_line: chunk.as_ref().map(|chunk| chunk.start_line),
        end_line: chunk.as_ref().map(|chunk| chunk.end_line),
        logical_symbol_id: Some(logical_symbol_id),
        symbol_id: chunk.as_ref().and_then(|chunk| chunk.symbol_id),
        chunk_id: chunk.as_ref().map(|chunk| chunk.chunk_id),
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        source_text_hash: chunk.map(|chunk| chunk.text_hash),
        anchor_status: "current".to_string(),
    })
}

fn resolve_symbol_binding(conn: &Connection, symbol_id: i64) -> anyhow::Result<ResolvedBinding> {
    let symbol = crate::query::symbol::lookup_by_id(conn, symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("symbol_id {symbol_id} not found"))?;
    let chunk = chunk_for_symbol(conn, symbol_id, &symbol.qualified_name)?;
    Ok(ResolvedBinding {
        binding_kind: "symbol".to_string(),
        binding_id: symbol.qualified_name,
        path: Some(symbol.path),
        start_line: chunk.as_ref().map(|chunk| chunk.start_line),
        end_line: chunk.as_ref().map(|chunk| chunk.end_line),
        logical_symbol_id: symbol.logical_symbol_id,
        symbol_id: Some(symbol_id),
        chunk_id: chunk.as_ref().map(|chunk| chunk.chunk_id),
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        source_text_hash: chunk.map(|chunk| chunk.text_hash),
        anchor_status: "current".to_string(),
    })
}

fn resolve_chunk_binding(conn: &Connection, chunk_id: i64) -> anyhow::Result<ResolvedBinding> {
    let chunk = chunk_by_id(conn, chunk_id)?
        .ok_or_else(|| anyhow::anyhow!("chunk_id {chunk_id} not found"))?;
    let symbol_id = symbol_id_for_chunk(conn, &chunk)?;
    Ok(ResolvedBinding {
        binding_kind: "chunk".to_string(),
        binding_id: chunk_id.to_string(),
        path: Some(chunk.path),
        start_line: Some(chunk.start_line),
        end_line: Some(chunk.end_line),
        logical_symbol_id: symbol_id
            .and_then(|id| logical_symbol_id_for_symbol(conn, id).ok().flatten()),
        symbol_id,
        chunk_id: Some(chunk_id),
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        source_text_hash: Some(chunk.text_hash),
        anchor_status: "current".to_string(),
    })
}

fn resolve_path_binding(
    conn: &Connection,
    path: &str,
    start_line: Option<i64>,
    end_line: Option<i64>,
) -> anyhow::Result<ResolvedBinding> {
    let file_hash = conn
        .query_row(
            "SELECT sha256 FROM files WHERE path = ?1 ORDER BY id DESC LIMIT 1",
            [path],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(ResolvedBinding {
        binding_kind: "path".to_string(),
        binding_id: match (start_line, end_line) {
            (Some(start), Some(end)) => format!("{path}:{start}-{end}"),
            _ => path.to_string(),
        },
        path: Some(path.to_string()),
        start_line,
        end_line,
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        source_text_hash: file_hash,
        anchor_status: "current".to_string(),
    })
}

#[derive(Debug)]
struct ChunkAnchor {
    chunk_id: i64,
    path: String,
    start_line: i64,
    end_line: i64,
    symbol_path: Option<String>,
    text_hash: String,
    symbol_id: Option<i64>,
}

fn chunk_by_id(conn: &Connection, chunk_id: i64) -> anyhow::Result<Option<ChunkAnchor>> {
    conn.query_row(
        "
        SELECT chunks.id, files.path, chunks.start_line, chunks.end_line, chunks.symbol_path,
               chunks.text_hash, NULL
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        WHERE chunks.id = ?1
        ",
        [chunk_id],
        chunk_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}

fn chunk_for_symbol(
    conn: &Connection,
    symbol_id: i64,
    qualified_name: &str,
) -> anyhow::Result<Option<ChunkAnchor>> {
    conn.query_row(
        "
        SELECT chunks.id, files.path, chunks.start_line, chunks.end_line, chunks.symbol_path,
               chunks.text_hash, symbols.id
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN chunks ON chunks.file_id = files.id
            AND (chunks.symbol_path = symbols.qualified_name OR chunks.symbol_path = ?2)
        WHERE symbols.id = ?1
        ORDER BY CASE WHEN chunks.symbol_path = symbols.qualified_name THEN 0 ELSE 1 END,
                 chunks.start_line
        LIMIT 1
        ",
        params![symbol_id, qualified_name],
        chunk_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}

fn chunk_for_logical_symbol(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<ChunkAnchor>> {
    conn.query_row(
        "
        SELECT chunks.id, files.path, chunks.start_line, chunks.end_line, chunks.symbol_path,
               chunks.text_hash, symbols.id
        FROM logical_symbol_members
        JOIN symbols ON symbols.id = logical_symbol_members.symbol_id
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN chunks ON chunks.file_id = files.id
            AND chunks.symbol_path = symbols.qualified_name
        WHERE logical_symbol_members.logical_symbol_id = ?1
        ORDER BY logical_symbol_members.start_line, chunks.start_line
        LIMIT 1
        ",
        [logical_symbol_id],
        chunk_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}

fn chunk_ids_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "
        SELECT chunks.id
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        WHERE files.path = ?1
          AND (chunks.symbol_path = ?2 OR chunks.symbol_path = ?3)
        ",
    )?;
    let rows = stmt
        .query_map(params![symbol.path, symbol.qualified_name, symbol.symbol_path], |row| {
            row.get::<_, i64>(0)
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn symbol_id_for_chunk(conn: &Connection, chunk: &ChunkAnchor) -> anyhow::Result<Option<i64>> {
    let Some(symbol_path) = chunk.symbol_path.as_deref() else {
        return Ok(None);
    };
    conn.query_row(
        "
        SELECT symbols.id
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE files.path = ?1 AND symbols.qualified_name = ?2
        LIMIT 1
        ",
        params![chunk.path, symbol_path],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn logical_symbol_id_for_symbol(conn: &Connection, symbol_id: i64) -> anyhow::Result<Option<i64>> {
    conn.query_row(
        "SELECT logical_symbol_id FROM logical_symbol_members WHERE symbol_id = ?1 LIMIT 1",
        [symbol_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn chunk_anchor_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkAnchor> {
    Ok(ChunkAnchor {
        chunk_id: row.get(0)?,
        path: row.get(1)?,
        start_line: row.get(2)?,
        end_line: row.get(3)?,
        symbol_path: row.get(4)?,
        text_hash: row.get(5)?,
        symbol_id: row.get(6)?,
    })
}

fn insert_binding(
    conn: &Connection,
    memory_id: &str,
    binding: &ResolvedBinding,
    now: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO repo_memory_bindings(
            memory_id, binding_kind, binding_id, path, start_line, end_line, logical_symbol_id,
            symbol_id, chunk_id, edge_id, commit_hash, github_owner, github_repo, github_number,
            anchor_status, created_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11, ?12, ?13, ?14, ?15)
        ",
        params![
            memory_id,
            binding.binding_kind,
            binding.binding_id,
            binding.path,
            binding.start_line,
            binding.end_line,
            binding.logical_symbol_id,
            binding.symbol_id,
            binding.chunk_id,
            binding.commit_hash,
            binding.github_owner,
            binding.github_repo,
            binding.github_number,
            binding.anchor_status,
            now
        ],
    )?;
    Ok(())
}

fn duplicate_memory_id(
    conn: &Connection,
    title: &str,
    body: &str,
    binding: &ResolvedBinding,
) -> anyhow::Result<Option<String>> {
    conn.query_row(
        "
        SELECT repo_memories.id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE lower(repo_memories.title) = lower(?1)
          AND lower(repo_memories.body) = lower(?2)
          AND repo_memory_bindings.binding_kind = ?3
          AND repo_memory_bindings.binding_id = ?4
          AND repo_memories.status != 'obsolete'
        LIMIT 1
        ",
        params![title.trim(), body.trim(), binding.binding_kind, binding.binding_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn replace_tags(conn: &Connection, memory_id: &str, tags: &[String]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM repo_memory_tags WHERE memory_id = ?1", [memory_id])?;
    for tag in tags.iter().map(|tag| tag.trim()).filter(|tag| !tag.is_empty()) {
        validate_len("tag", tag, 64)?;
        conn.execute(
            "INSERT OR IGNORE INTO repo_memory_tags(memory_id, tag) VALUES (?1, ?2)",
            params![memory_id, tag],
        )?;
    }
    Ok(())
}

fn upsert_memory_fts(conn: &Connection, memory_id: &str) -> anyhow::Result<()> {
    conn.execute("DELETE FROM repo_memory_fts WHERE memory_id = ?1", [memory_id])?;
    let tags = tags_for_memory(conn, memory_id)?.join(" ");
    conn.execute(
        "
        INSERT INTO repo_memory_fts(memory_id, title, body, kind, tags)
        SELECT id, title, body, kind, ?2
        FROM repo_memories
        WHERE id = ?1
        ",
        params![memory_id, tags],
    )?;
    Ok(())
}

fn memory_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemory> {
    Ok(RepoMemory {
        memory_id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        confidence: row.get(4)?,
        status: row.get(5)?,
        created_by: row.get(6)?,
        created_at_ms: row.get(7)?,
        updated_at_ms: row.get(8)?,
        source: row.get(9)?,
        source_text_hash: row.get(10)?,
        input_hash: row.get(11)?,
        memory_version: row.get(12)?,
        bindings: Vec::new(),
        tags: Vec::new(),
    })
}

fn binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemoryBinding> {
    Ok(RepoMemoryBinding {
        memory_id: row.get(0)?,
        binding_kind: row.get(1)?,
        binding_id: row.get(2)?,
        path: row.get(3)?,
        start_line: row.get(4)?,
        end_line: row.get(5)?,
        logical_symbol_id: row.get(6)?,
        symbol_id: row.get(7)?,
        chunk_id: row.get(8)?,
        edge_id: row.get(9)?,
        commit_hash: row.get(10)?,
        github_owner: row.get(11)?,
        github_repo: row.get(12)?,
        github_number: row.get(13)?,
        anchor_status: row.get(14)?,
        created_at_ms: row.get(15)?,
    })
}

fn attach_memory_children(conn: &Connection, memory: &mut RepoMemory) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, binding_kind, binding_id, path, start_line, end_line, logical_symbol_id,
               symbol_id, chunk_id, edge_id, commit_hash, github_owner, github_repo,
               github_number, anchor_status, created_at_ms
        FROM repo_memory_bindings
        WHERE memory_id = ?1
        ORDER BY binding_kind, binding_id
        ",
    )?;
    memory.bindings =
        stmt.query_map([&memory.memory_id], binding_row)?.collect::<Result<Vec<_>, _>>()?;
    memory.tags = tags_for_memory(conn, &memory.memory_id)?;
    Ok(())
}

fn tags_for_memory(conn: &Connection, memory_id: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT tag FROM repo_memory_tags WHERE memory_id = ?1 ORDER BY tag")?;
    stmt.query_map([memory_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn ids_to_memories(
    conn: &Connection,
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut memories = Vec::new();
    for row in rows {
        if let Some(memory) = memory_by_id(conn, &row?)? {
            memories.push(memory);
        }
    }
    Ok(memories)
}

fn split_active_stale(memories: Vec<RepoMemory>) -> anyhow::Result<RepoMemoryEvidence> {
    let mut direct = Vec::new();
    let mut stale = Vec::new();
    for memory in memories {
        if memory.status == "stale"
            || memory.bindings.iter().any(|binding| {
                matches!(binding.anchor_status.as_str(), "stale" | "gone" | "unverified")
            })
        {
            stale.push(memory);
        } else {
            direct.push(memory);
        }
    }
    Ok(RepoMemoryEvidence { direct, stale })
}

fn validate_binding(conn: &Connection, binding: &mut RepoMemoryBinding) -> anyhow::Result<String> {
    match binding.binding_kind.as_str() {
        "logical_symbol" => validate_logical_symbol_binding(conn, binding),
        "symbol" => validate_symbol_binding(conn, binding),
        "chunk" => validate_chunk_binding(conn, binding),
        "path" => validate_path_binding(conn, binding),
        "commit" | "github" => Ok("unverified".to_string()),
        _ => Ok("unverified".to_string()),
    }
}

fn validate_logical_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(id) = binding.logical_symbol_id {
        if crate::query::symbol::lookup_logical_by_id(conn, id)?.is_some() {
            return validate_bound_chunk(conn, binding);
        }
    }
    let relocated = conn
        .query_row(
            "
            SELECT id, path
            FROM logical_symbols
            WHERE qualified_name = ?1
            LIMIT 1
            ",
            [&binding.binding_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((id, path)) = relocated else {
        return Ok("gone".to_string());
    };
    binding.logical_symbol_id = Some(id);
    binding.path = Some(path);
    if let Some(chunk) = chunk_for_logical_symbol(conn, id)? {
        binding.symbol_id = chunk.symbol_id;
        binding.chunk_id = Some(chunk.chunk_id);
        binding.start_line = Some(chunk.start_line);
        binding.end_line = Some(chunk.end_line);
    }
    Ok("relocated".to_string())
}

fn validate_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(id) = binding.symbol_id {
        if crate::query::symbol::lookup_by_id(conn, id)?.is_some() {
            return validate_bound_chunk(conn, binding);
        }
    }
    let relocated = conn
        .query_row(
            "
            SELECT symbols.id, files.path
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            WHERE symbols.qualified_name = ?1
            LIMIT 1
            ",
            [&binding.binding_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((id, path)) = relocated else {
        return Ok("gone".to_string());
    };
    binding.symbol_id = Some(id);
    binding.logical_symbol_id = logical_symbol_id_for_symbol(conn, id)?;
    binding.path = Some(path);
    if let Some(chunk) = chunk_for_symbol(conn, id, &binding.binding_id)? {
        binding.chunk_id = Some(chunk.chunk_id);
        binding.start_line = Some(chunk.start_line);
        binding.end_line = Some(chunk.end_line);
    }
    Ok("relocated".to_string())
}

fn validate_chunk_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    validate_bound_chunk(conn, binding)
}

fn validate_bound_chunk(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let Some(chunk_id) = binding.chunk_id else {
        return Ok("unverified".to_string());
    };
    let Some(chunk) = chunk_by_id(conn, chunk_id)? else {
        return Ok("gone".to_string());
    };
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != chunk.text_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}

fn validate_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let Some(path) = binding.path.as_deref() else {
        return Ok("unverified".to_string());
    };
    let current_hash = conn
        .query_row(
            "SELECT sha256 FROM files WHERE path = ?1 ORDER BY id DESC LIMIT 1",
            [path],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(current_hash) = current_hash else {
        return Ok("gone".to_string());
    };
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}

fn source_hash_for_memory(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<String>> {
    conn.query_row("SELECT source_text_hash FROM repo_memories WHERE id = ?1", [memory_id], |row| {
        row.get::<_, Option<String>>(0)
    })
    .optional()
    .map(|value| value.flatten())
    .map_err(Into::into)
}

fn validate_kind(kind: &str) -> anyhow::Result<()> {
    match kind {
        "Invariant"
        | "Decision"
        | "RejectedAlternative"
        | "Risk"
        | "BugPattern"
        | "TestExpectation"
        | "PerformanceNote"
        | "SecurityNote"
        | "FFIBoundary"
        | "PlatformQuirk"
        | "FollowUp"
        | "OpenQuestion"
        | "Obsolete" => Ok(()),
        _ => anyhow::bail!("invalid memory kind `{kind}`"),
    }
}

fn validate_confidence(confidence: &str) -> anyhow::Result<()> {
    match confidence {
        "high" | "medium" | "low" => Ok(()),
        _ => anyhow::bail!("invalid memory confidence `{confidence}`"),
    }
}

fn validate_status(status: &str) -> anyhow::Result<()> {
    match status {
        "active" | "stale" | "obsolete" | "rejected" => Ok(()),
        _ => anyhow::bail!("invalid memory status `{status}`"),
    }
}

fn validate_source(source: &str) -> anyhow::Result<()> {
    match source {
        "agent" | "human" | "imported" | "generated" => Ok(()),
        _ => anyhow::bail!("invalid memory source `{source}`"),
    }
}

fn validate_len(field: &str, value: &str, max: usize) -> anyhow::Result<()> {
    let len = value.trim().chars().count();
    if len == 0 {
        anyhow::bail!("memory {field} must not be empty");
    }
    if len > max {
        anyhow::bail!("memory {field} exceeds {max} characters");
    }
    Ok(())
}

fn memory_id(now: i64, input_hash: &str) -> String {
    let suffix = input_hash.chars().take(12).collect::<String>();
    format!("mem_{now:x}_{suffix}")
}

fn memory_input_hash(kind: &str, title: &str, body: &str, tags: &[String]) -> String {
    let mut normalized_tags = tags.iter().map(|tag| tag.trim()).collect::<Vec<_>>();
    normalized_tags.sort_unstable();
    hex_sha256(
        format!("{kind}\n{}\n{}\n{}", title.trim(), body.trim(), normalized_tags.join(","))
            .as_bytes(),
    )
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| term.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_'))
        .filter(|term| !term.is_empty())
        .map(|term| format!("{term}*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}
