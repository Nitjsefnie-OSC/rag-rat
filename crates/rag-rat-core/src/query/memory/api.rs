use super::*;

pub(crate) fn create_memory(
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
pub(crate) fn update_memory(
    conn: &Connection,
    update: RepoMemoryUpdate,
) -> anyhow::Result<RepoMemory> {
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
pub(crate) fn mark_obsolete(conn: &Connection, memory_id: &str) -> anyhow::Result<RepoMemory> {
    update_memory(conn, RepoMemoryUpdate {
        memory_id: memory_id.to_string(),
        kind: None,
        title: None,
        body: None,
        confidence: None,
        status: Some("obsolete".to_string()),
        tags: None,
    })
}
pub(crate) fn memory_by_id(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Option<RepoMemory>> {
    let Some(mut memory) = conn
        .query_row(
            "
            SELECT id AS memory_id,
                   kind AS kind,
                   title AS title,
                   body AS body,
                   confidence AS confidence,
                   status AS status,
                   created_by AS created_by,
                   created_at_ms AS created_at_ms,
                   updated_at_ms AS updated_at_ms,
                   source AS source,
                   source_text_hash AS source_text_hash,
                   input_hash AS input_hash,
                   memory_version AS memory_version
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
pub(crate) fn memories_for_chunk(
    conn: &Connection,
    chunk_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
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
        stmt.query_map(params![chunk_id, i64::from(limit)], |row| {
            row.get::<_, String>("memory_id")
        })?,
    )
}
pub(crate) fn memories_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale')
          AND repo_memory_bindings.path = ?1
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?2
        ",
    )?;
    ids_to_memories(
        conn,
        stmt.query_map(params![path, i64::from(limit)], |row| row.get("memory_id"))?,
    )
}
pub(crate) fn memories_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let chunk_ids = chunk_ids_for_symbol(conn, symbol)?;
    let mut candidate_ids = BTreeSet::new();
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale')
          AND (
              repo_memory_bindings.logical_symbol_id = ?1
              OR repo_memory_bindings.symbol_id = ?2
              OR repo_memory_bindings.binding_id = ?3
              OR (
                  repo_memory_bindings.binding_kind = 'path'
                  AND repo_memory_bindings.path = ?4
              )
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
        |row| row.get::<_, String>("memory_id"),
    )?;
    for row in rows {
        candidate_ids.insert(row?);
    }
    if !chunk_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", chunk_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "
            SELECT DISTINCT repo_memories.id AS memory_id
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
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
            row.get::<_, String>("memory_id")
        })?;
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
    memories.sort_by_key(|memory| std::cmp::Reverse(memory.updated_at_ms));
    Ok(memories)
}
pub fn memory_evidence_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<RepoMemoryEvidence> {
    let (direct, stale) = split_active_stale(memories_for_symbol(conn, symbol, limit)?);
    Ok(RepoMemoryEvidence { direct, path_crossed: Vec::new(), stale })
}
pub(crate) fn memory_evidence_for_symbol_and_edges(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    edge_ids: &[i64],
    limit: u32,
) -> anyhow::Result<RepoMemoryEvidence> {
    let (direct, mut stale) = split_active_stale(memories_for_symbol(conn, symbol, limit)?);
    let (path_crossed, crossed_stale) =
        split_active_stale(memories_for_edges(conn, edge_ids, limit)?);
    stale.extend(crossed_stale);
    Ok(RepoMemoryEvidence { direct, path_crossed, stale })
}
pub(crate) fn memories_for_edges(
    conn: &Connection,
    edge_ids: &[i64],
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    if edge_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut unique_edge_ids = edge_ids.to_vec();
    unique_edge_ids.sort_unstable();
    unique_edge_ids.dedup();
    let placeholders =
        std::iter::repeat_n("?", unique_edge_ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale')
          AND repo_memory_bindings.edge_id IN ({placeholders})
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?
        "
    );
    let mut values =
        unique_edge_ids.iter().map(|id| rusqlite::types::Value::Integer(*id)).collect::<Vec<_>>();
    values.push(rusqlite::types::Value::Integer(i64::from(limit)));
    let mut stmt = conn.prepare(&sql)?;
    ids_to_memories(
        conn,
        stmt.query_map(rusqlite::params_from_iter(values), |row| row.get("memory_id"))?,
    )
}
pub(crate) fn memories_for_call_path_hash(
    conn: &Connection,
    edge_sequence_hash: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_call_paths ON repo_memory_call_paths.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale')
          AND repo_memory_call_paths.edge_sequence_hash = ?1
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?2
        ",
    )?;
    ids_to_memories(
        conn,
        stmt.query_map(params![edge_sequence_hash, i64::from(limit)], |row| row.get("memory_id"))?,
    )
}
pub(crate) fn memory_search(
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
    ids_to_memories(
        conn,
        stmt.query_map(params![query, i64::from(limit)], |row| row.get("memory_id"))?,
    )
}
pub(crate) fn validate_memories(conn: &Connection) -> anyhow::Result<RepoMemoryValidationReport> {
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
                edge_id = ?7,
                path = ?8,
                start_line = ?9,
                end_line = ?10
            WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?11
            ",
            params![
                binding.memory_id,
                binding.binding_kind,
                status,
                binding.logical_symbol_id,
                binding.symbol_id,
                binding.chunk_id,
                binding.edge_id,
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
