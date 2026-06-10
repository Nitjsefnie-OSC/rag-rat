use super::*;

pub(crate) fn store_ref(conn: &Connection, reference: &GitHubRef) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR IGNORE INTO github_refs(
            owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text, \
         discovered_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ",
        params![
            reference.owner,
            reference.repo,
            reference.number,
            reference.ref_kind,
            reference.source_kind,
            reference.source_path,
            reference.source_commit,
            reference.source_text,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_issue(conn: &Connection, issue: &GitHubIssue) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, author, \
         created_at, updated_at, is_pull_request, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(owner, repo, number) DO UPDATE SET
            html_url = excluded.html_url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, is_pull_request = excluded.is_pull_request,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            issue.owner,
            issue.repo,
            issue.number,
            issue.html_url,
            issue.state,
            issue.title,
            issue.body,
            issue.author,
            issue.created_at,
            issue.updated_at,
            issue.is_pull_request,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_comment(conn: &Connection, comment: &GitHubComment) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_comments(id, owner, repo, number, html_url, body, author, \
         created_at, updated_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ",
        params![
            comment.id,
            comment.owner,
            comment.repo,
            comment.number,
            comment.html_url,
            comment.body,
            comment.author,
            comment.created_at,
            comment.updated_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_pull(conn: &Connection, pull: &GitHubPullRequest) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body, \
         author, created_at, updated_at, merged_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(owner, repo, number) DO UPDATE SET
            html_url = excluded.html_url, state = excluded.state, title = excluded.title,
            body = excluded.body, author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, merged_at = excluded.merged_at,
            synced_at_ms = excluded.synced_at_ms
        ",
        params![
            pull.owner,
            pull.repo,
            pull.number,
            pull.html_url,
            pull.state,
            pull.title,
            pull.body,
            pull.author,
            pull.created_at,
            pull.updated_at,
            pull.merged_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_review(conn: &Connection, review: &GitHubReview) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_reviews(id, owner, repo, number, html_url, state, body, \
         author, submitted_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ",
        params![
            review.id,
            review.owner,
            review.repo,
            review.number,
            review.html_url,
            review.state,
            review.body,
            review.author,
            review.submitted_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn store_review_comment(
    conn: &Connection,
    comment: &GitHubReviewComment,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_review_comments(id, owner, repo, number, path, html_url, \
         body, author, created_at, updated_at, synced_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ",
        params![
            comment.id,
            comment.owner,
            comment.repo,
            comment.number,
            comment.path,
            comment.html_url,
            comment.body,
            comment.author,
            comment.created_at,
            comment.updated_at,
            now_ms(),
        ],
    )?;
    Ok(())
}
pub(crate) fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM github_fts", [])?;
    insert_issue_fts(conn)?;
    insert_comment_fts(conn)?;
    insert_pull_fts(conn)?;
    insert_review_fts(conn)?;
    insert_review_comment_fts(conn)?;
    Ok(())
}
pub(crate) fn insert_issue_fts(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt =
        conn.prepare("SELECT id, owner, repo, number, html_url, title, body FROM github_issues")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, title, body) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind: "issue",
            item_id: &id.to_string(),
            url: &url,
            title: &title,
            body: &body,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_comment_fts(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt =
        conn.prepare("SELECT id, owner, repo, number, html_url, body FROM github_comments")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, body) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind: "comment",
            item_id: &id.to_string(),
            url: &url,
            title: "",
            body: &body,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_pull_fts(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, owner, repo, number, html_url, title, body FROM github_pull_requests",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, title, body) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind: "pull",
            item_id: &id.to_string(),
            url: &url,
            title: &title,
            body: &body,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_review_fts(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, owner, repo, number, COALESCE(html_url, ''), body FROM github_reviews",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, body) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind: "review",
            item_id: &id.to_string(),
            url: &url,
            title: "",
            body: &body,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_review_comment_fts(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, owner, repo, number, html_url, COALESCE(path, ''), body FROM \
         github_review_comments",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    for row in rows {
        let (id, owner, repo, number, url, path, body) = row?;
        insert_fts(conn, FtsRow {
            owner: &owner,
            repo: &repo,
            number,
            kind: "review_comment",
            item_id: &id.to_string(),
            url: &url,
            title: &path,
            body: &body,
        })?;
    }
    Ok(())
}
pub(crate) fn insert_fts(conn: &Connection, row: FtsRow<'_>) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            row.owner,
            row.repo,
            row.number,
            row.kind,
            row.item_id,
            row.url,
            row.title,
            row.body,
            classify_text(&format!("{}\n{}", row.title, row.body))
        ],
    )?;
    Ok(())
}
