use std::{collections::BTreeSet, path::Path, process::Command};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::index::now_ms;

#[derive(Debug, Clone, Serialize)]
pub struct GitHubStatus {
    pub refs: u64,
    pub issues: u64,
    pub comments: u64,
    pub pulls: u64,
    pub reviews: u64,
    pub review_comments: u64,
    pub last_sync_ms: Option<i64>,
    pub capability: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitHubSyncReport {
    pub offline: bool,
    pub discovered_refs: usize,
    pub skipped_refs: usize,
    pub failed_refs: usize,
    pub synced_items: usize,
    pub errors: Vec<GitHubSyncError>,
    pub status: GitHubStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitHubSyncError {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub status: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct GitHubSyncProgress {
    pub current: usize,
    pub total: usize,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub action: GitHubSyncAction,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubSyncAction {
    Syncing,
    Skipped,
    Synced,
    Failed,
    RebuildingFts,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitHubRef {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub ref_kind: String,
    pub source_kind: String,
    pub source_path: Option<String>,
    pub source_commit: Option<String>,
    pub source_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitHubEvidence {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub item_kind: String,
    pub item_id: String,
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub classification: String,
    pub evidence_kind: &'static str,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Papertrail {
    pub current_source: Option<CurrentSourceEvidence>,
    pub github_evidence: Vec<GitHubEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback_github_evidence: Vec<GitHubEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentSourceEvidence {
    pub chunk_id: Option<i64>,
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub is_pull_request: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubComment {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubPullRequest {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubReview {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub html_url: Option<String>,
    pub state: String,
    pub body: String,
    pub author: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubReviewComment {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub path: Option<String>,
    pub html_url: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

pub trait GitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<GitHubIssue>;
    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubComment>>;
    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<GitHubPullRequest>>;
    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReview>>;
    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>>;
}

pub struct GhCliGitHubClient;

impl GitHubClient for GhCliGitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<GitHubIssue> {
        let value = gh_api_json(&format!("repos/{owner}/{repo}/issues/{number}"))?;
        Ok(issue_from_value(owner, repo, &value))
    }

    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let values = gh_api_paginated(&format!("repos/{owner}/{repo}/issues/{number}/comments"))?;
        Ok(values.iter().map(|value| comment_from_value(owner, repo, number, value)).collect())
    }

    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<GitHubPullRequest>> {
        match gh_api_json(&format!("repos/{owner}/{repo}/pulls/{number}")) {
            Ok(value) => Ok(Some(pull_from_value(owner, repo, number, &value))),
            Err(_) => Ok(None),
        }
    }

    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReview>> {
        let values = gh_api_paginated(&format!("repos/{owner}/{repo}/pulls/{number}/reviews"))?;
        Ok(values.iter().map(|value| review_from_value(owner, repo, number, value)).collect())
    }

    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>> {
        let values = gh_api_paginated(&format!("repos/{owner}/{repo}/pulls/{number}/comments"))?;
        Ok(values
            .iter()
            .map(|value| review_comment_from_value(owner, repo, number, value))
            .collect())
    }
}

pub fn sync_from_refs<C: GitHubClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
) -> anyhow::Result<GitHubSyncReport> {
    sync_from_refs_with_progress(conn, root, client, offline, |_| {})
}

pub fn sync_from_refs_with_progress<C: GitHubClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    mut progress: impl FnMut(GitHubSyncProgress),
) -> anyhow::Result<GitHubSyncReport> {
    let refs = discover_and_store_refs(conn, root)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("github sync requires a client"))?;
        sync_refs(conn, client, refs.iter(), &mut progress)?
    };
    set_meta(conn, "github_last_sync_ms", &now_ms().to_string())?;
    Ok(GitHubSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        errors: sync.errors,
        status: status(conn)?,
    })
}

pub fn sync_issue<C: GitHubClient>(
    conn: &Connection,
    issue_ref: &str,
    client: Option<&C>,
    offline: bool,
) -> anyhow::Result<GitHubSyncReport> {
    let parsed = parse_issue_ref(issue_ref, default_repo().as_deref())
        .ok_or_else(|| anyhow::anyhow!("invalid GitHub issue reference `{issue_ref}`"))?;
    store_ref(
        conn,
        &GitHubRef {
            owner: parsed.owner,
            repo: parsed.repo,
            number: parsed.number,
            ref_kind: "unknown".to_string(),
            source_kind: "manual".to_string(),
            source_path: None,
            source_commit: None,
            source_text: issue_ref.to_string(),
        },
    )?;
    let refs = refs(conn)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("github sync requires a client"))?;
        sync_refs(conn, client, refs.iter().filter(|r| r.number == parsed.number), &mut |_| {})?
    };
    set_meta(conn, "github_last_sync_ms", &now_ms().to_string())?;
    Ok(GitHubSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        errors: sync.errors,
        status: status(conn)?,
    })
}

pub fn status(conn: &Connection) -> anyhow::Result<GitHubStatus> {
    Ok(GitHubStatus {
        refs: count_table(conn, "github_refs")?,
        issues: count_table(conn, "github_issues")?,
        comments: count_table(conn, "github_comments")?,
        pulls: count_table(conn, "github_pull_requests")?,
        reviews: count_table(conn, "github_reviews")?,
        review_comments: count_table(conn, "github_review_comments")?,
        last_sync_ms: meta(conn, "github_last_sync_ms")?.and_then(|value| value.parse().ok()),
        capability: if gh_available() {
            "gh_cli_available".to_string()
        } else {
            "gh_cli_missing".to_string()
        },
    })
}

pub fn issue_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    search_fts(conn, query, Some("issue"), limit)
}

pub fn rationale_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    let mut evidence = Vec::new();
    let default_repo = default_repo();
    for reference in parse_refs(query, default_repo.as_deref()) {
        evidence.extend(evidence_for_issue(
            conn,
            &reference.owner,
            &reference.repo,
            reference.number,
            limit,
        )?);
    }
    evidence.extend(search_fts(conn, query, None, limit)?);
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}

pub fn refs_for_path(conn: &Connection, path: &str, limit: u32) -> anyhow::Result<Vec<GitHubRef>> {
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text
        FROM github_refs
        WHERE source_path = ?1
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![path, i64::from(limit)], ref_row)?;
    collect_rows(rows)
}

pub fn papertrail_for_chunk(
    conn: &Connection,
    chunk: &crate::query::ReadChunk,
    limit: u32,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_path(conn, &chunk.path, limit)?;
    if evidence.is_empty() {
        evidence = rationale_search(conn, &chunk.path, limit)?;
    }
    Ok(Papertrail {
        current_source: Some(CurrentSourceEvidence {
            chunk_id: Some(chunk.chunk_id),
            path: chunk.path.clone(),
            start_line: Some(chunk.start_line),
            end_line: Some(chunk.end_line),
            symbol: chunk.symbol_path.clone(),
        }),
        github_evidence: evidence,
        fallback_github_evidence: Vec::new(),
    })
}

pub fn papertrail_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_path(conn, &symbol.path, limit)?;
    evidence.extend(rationale_search(conn, &symbol.qualified_name, limit)?);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let (start_line, end_line, chunk_id) = current_symbol_span(conn, symbol)?;
    Ok(Papertrail {
        current_source: Some(CurrentSourceEvidence {
            chunk_id,
            path: symbol.path.clone(),
            start_line,
            end_line,
            symbol: Some(symbol.qualified_name.clone()),
        }),
        github_evidence: evidence,
        fallback_github_evidence: Vec::new(),
    })
}

pub fn papertrail_for_commit(
    conn: &Connection,
    commit_hash: &str,
    limit: u32,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_commit_refs(conn, commit_hash, limit)?;
    let mut fallback_evidence = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT path FROM git_file_changes WHERE commit_hash LIKE ?1 ORDER BY path LIMIT ?2",
    )?;
    let commit_like = format!("{commit_hash}%");
    let rows =
        stmt.query_map(params![commit_like, i64::from(limit)], |row| row.get::<_, String>(0))?;
    for row in rows {
        fallback_evidence.extend(evidence_for_path(conn, &row?, limit)?);
    }
    fallback_evidence.extend(rationale_search(conn, commit_hash, limit)?);
    dedupe_evidence(&mut evidence);
    dedupe_evidence(&mut fallback_evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    fallback_evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(Papertrail {
        current_source: None,
        github_evidence: evidence,
        fallback_github_evidence: fallback_evidence,
    })
}

pub fn discover_and_store_refs(conn: &Connection, root: &Path) -> anyhow::Result<Vec<GitHubRef>> {
    let default_repo = default_repo();
    let mut refs = Vec::new();
    discover_commit_refs(conn, default_repo.as_deref(), &mut refs)?;
    discover_file_refs(conn, root, default_repo.as_deref(), &mut refs)?;
    let branch = git_output(root, &["branch", "--show-current"]).unwrap_or_default();
    for parsed in parse_refs(&branch, default_repo.as_deref()) {
        refs.push(GitHubRef {
            owner: parsed.owner,
            repo: parsed.repo,
            number: parsed.number,
            ref_kind: parsed.kind,
            source_kind: "branch".to_string(),
            source_path: None,
            source_commit: None,
            source_text: branch.clone(),
        });
    }
    let mut unique = BTreeSet::new();
    refs.retain(|r| {
        unique.insert((
            r.owner.clone(),
            r.repo.clone(),
            r.number,
            r.source_kind.clone(),
            r.source_path.clone(),
            r.source_commit.clone(),
            r.source_text.clone(),
        ))
    });
    for reference in &refs {
        store_ref(conn, reference)?;
    }
    Ok(refs)
}

#[derive(Default)]
struct SyncRefsReport {
    synced_items: usize,
    skipped_refs: usize,
    failed_refs: usize,
    errors: Vec<GitHubSyncError>,
}

fn sync_refs<'a, C: GitHubClient>(
    conn: &Connection,
    client: &C,
    refs: impl Iterator<Item = &'a GitHubRef>,
    progress: &mut impl FnMut(GitHubSyncProgress),
) -> anyhow::Result<SyncRefsReport> {
    let refs = refs.collect::<Vec<_>>();
    let total = refs
        .iter()
        .map(|reference| (reference.owner.clone(), reference.repo.clone(), reference.number))
        .collect::<BTreeSet<_>>()
        .len();
    let mut report = SyncRefsReport::default();
    let mut seen = BTreeSet::new();
    for reference in refs {
        if !seen.insert((reference.owner.clone(), reference.repo.clone(), reference.number)) {
            continue;
        }
        let current = seen.len();
        if github_ref_synced(conn, reference)? {
            report.skipped_refs += 1;
            progress(sync_progress(reference, current, total, GitHubSyncAction::Skipped, None));
            continue;
        }
        progress(sync_progress(reference, current, total, GitHubSyncAction::Syncing, None));
        match sync_one_ref(conn, client, reference) {
            Ok(items) => {
                report.synced_items += items;
                mark_ref_sync(conn, reference, "synced", None)?;
                progress(sync_progress(reference, current, total, GitHubSyncAction::Synced, None));
            },
            Err(err) => {
                let message = err.to_string();
                let status = if is_not_found_error(&message) { "not_found" } else { "failed" };
                mark_ref_sync(conn, reference, status, Some(&message))?;
                report.failed_refs += 1;
                report.errors.push(GitHubSyncError {
                    owner: reference.owner.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    status: status.to_string(),
                    error: message.clone(),
                });
                progress(sync_progress(
                    reference,
                    current,
                    total,
                    GitHubSyncAction::Failed,
                    Some(message),
                ));
            },
        }
    }
    progress(GitHubSyncProgress {
        current: total,
        total,
        owner: String::new(),
        repo: String::new(),
        number: 0,
        action: GitHubSyncAction::RebuildingFts,
        message: None,
    });
    rebuild_fts(conn)?;
    Ok(report)
}

fn sync_one_ref<C: GitHubClient>(
    conn: &Connection,
    client: &C,
    reference: &GitHubRef,
) -> anyhow::Result<usize> {
    let mut synced = 0;
    let issue = client.issue(&reference.owner, &reference.repo, reference.number)?;
    store_issue(conn, &issue)?;
    synced += 1;
    for comment in client.issue_comments(&reference.owner, &reference.repo, reference.number)? {
        store_comment(conn, &comment)?;
        synced += 1;
    }
    if let Some(pull) = client.pull(&reference.owner, &reference.repo, reference.number)? {
        store_pull(conn, &pull)?;
        synced += 1;
        for review in client.pull_reviews(&reference.owner, &reference.repo, reference.number)? {
            store_review(conn, &review)?;
            synced += 1;
        }
        for comment in
            client.pull_review_comments(&reference.owner, &reference.repo, reference.number)?
        {
            store_review_comment(conn, &comment)?;
            synced += 1;
        }
    }
    Ok(synced)
}

fn sync_progress(
    reference: &GitHubRef,
    current: usize,
    total: usize,
    action: GitHubSyncAction,
    message: Option<String>,
) -> GitHubSyncProgress {
    GitHubSyncProgress {
        current,
        total,
        owner: reference.owner.clone(),
        repo: reference.repo.clone(),
        number: reference.number,
        action,
        message,
    }
}

fn github_ref_synced(conn: &Connection, reference: &GitHubRef) -> anyhow::Result<bool> {
    let status = conn
        .query_row(
            "
            SELECT status
            FROM github_ref_sync
            WHERE owner = ?1 AND repo = ?2 AND number = ?3
            ",
            params![reference.owner, reference.repo, reference.number],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if matches!(status.as_deref(), Some("synced" | "not_found")) {
        return Ok(true);
    }
    let cached_issue = conn.query_row(
        "
        SELECT EXISTS(
            SELECT 1 FROM github_issues
            WHERE owner = ?1 AND repo = ?2 AND number = ?3
        )
        ",
        params![reference.owner, reference.repo, reference.number],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(cached_issue)
}

fn mark_ref_sync(
    conn: &Connection,
    reference: &GitHubRef,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_ref_sync(owner, repo, number, status, synced_at_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(owner, repo, number) DO UPDATE SET
            status = excluded.status,
            synced_at_ms = excluded.synced_at_ms,
            last_error = excluded.last_error
        ",
        params![reference.owner, reference.repo, reference.number, status, now_ms(), error],
    )?;
    Ok(())
}

fn is_not_found_error(message: &str) -> bool {
    message.contains("HTTP 404") || message.to_ascii_lowercase().contains("not found")
}

fn discover_commit_refs(
    conn: &Connection,
    default_repo: Option<&str>,
    out: &mut Vec<GitHubRef>,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT hash, subject, body FROM git_commits")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (hash, subject, body) = row?;
        for text in [subject, body] {
            for parsed in parse_refs(&text, default_repo) {
                out.push(GitHubRef {
                    owner: parsed.owner,
                    repo: parsed.repo,
                    number: parsed.number,
                    ref_kind: parsed.kind,
                    source_kind: "commit".to_string(),
                    source_path: None,
                    source_commit: Some(hash.clone()),
                    source_text: text.clone(),
                });
            }
        }
    }
    Ok(())
}

fn discover_file_refs(
    conn: &Connection,
    root: &Path,
    default_repo: Option<&str>,
    out: &mut Vec<GitHubRef>,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let path = row?;
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        for line in text.lines() {
            for parsed in parse_refs(line, default_repo) {
                out.push(GitHubRef {
                    owner: parsed.owner,
                    repo: parsed.repo,
                    number: parsed.number,
                    ref_kind: parsed.kind,
                    source_kind: "file".to_string(),
                    source_path: Some(path.clone()),
                    source_commit: None,
                    source_text: line.trim().to_string(),
                });
            }
        }
    }
    Ok(())
}

fn store_ref(conn: &Connection, reference: &GitHubRef) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR IGNORE INTO github_refs(
            owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text, discovered_at_ms
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

fn store_issue(conn: &Connection, issue: &GitHubIssue) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_issues(owner, repo, number, html_url, state, title, body, author, created_at, updated_at, is_pull_request, synced_at_ms)
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

fn store_comment(conn: &Connection, comment: &GitHubComment) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_comments(id, owner, repo, number, html_url, body, author, created_at, updated_at, synced_at_ms)
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

fn store_pull(conn: &Connection, pull: &GitHubPullRequest) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_pull_requests(owner, repo, number, html_url, state, title, body, author, created_at, updated_at, merged_at, synced_at_ms)
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

fn store_review(conn: &Connection, review: &GitHubReview) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_reviews(id, owner, repo, number, html_url, state, body, author, submitted_at, synced_at_ms)
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

fn store_review_comment(conn: &Connection, comment: &GitHubReviewComment) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT OR REPLACE INTO github_review_comments(id, owner, repo, number, path, html_url, body, author, created_at, updated_at, synced_at_ms)
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

pub fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM github_fts", [])?;
    insert_issue_fts(conn)?;
    insert_comment_fts(conn)?;
    insert_pull_fts(conn)?;
    insert_review_fts(conn)?;
    insert_review_comment_fts(conn)?;
    Ok(())
}

fn insert_issue_fts(conn: &Connection) -> anyhow::Result<()> {
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
        insert_fts(
            conn,
            FtsRow {
                owner: &owner,
                repo: &repo,
                number,
                kind: "issue",
                item_id: &id.to_string(),
                url: &url,
                title: &title,
                body: &body,
            },
        )?;
    }
    Ok(())
}

fn insert_comment_fts(conn: &Connection) -> anyhow::Result<()> {
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
        insert_fts(
            conn,
            FtsRow {
                owner: &owner,
                repo: &repo,
                number,
                kind: "comment",
                item_id: &id.to_string(),
                url: &url,
                title: "",
                body: &body,
            },
        )?;
    }
    Ok(())
}

fn insert_pull_fts(conn: &Connection) -> anyhow::Result<()> {
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
        insert_fts(
            conn,
            FtsRow {
                owner: &owner,
                repo: &repo,
                number,
                kind: "pull",
                item_id: &id.to_string(),
                url: &url,
                title: &title,
                body: &body,
            },
        )?;
    }
    Ok(())
}

fn insert_review_fts(conn: &Connection) -> anyhow::Result<()> {
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
        insert_fts(
            conn,
            FtsRow {
                owner: &owner,
                repo: &repo,
                number,
                kind: "review",
                item_id: &id.to_string(),
                url: &url,
                title: "",
                body: &body,
            },
        )?;
    }
    Ok(())
}

fn insert_review_comment_fts(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, owner, repo, number, html_url, COALESCE(path, ''), body FROM github_review_comments",
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
        insert_fts(
            conn,
            FtsRow {
                owner: &owner,
                repo: &repo,
                number,
                kind: "review_comment",
                item_id: &id.to_string(),
                url: &url,
                title: &path,
                body: &body,
            },
        )?;
    }
    Ok(())
}

struct FtsRow<'a> {
    owner: &'a str,
    repo: &'a str,
    number: i64,
    kind: &'a str,
    item_id: &'a str,
    url: &'a str,
    title: &'a str,
    body: &'a str,
}

fn insert_fts(conn: &Connection, row: FtsRow<'_>) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, classification)
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

fn evidence_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    let refs = refs_for_path(conn, path, limit)?;
    let mut evidence = Vec::new();
    for reference in refs {
        evidence.extend(evidence_for_issue(
            conn,
            &reference.owner,
            &reference.repo,
            reference.number,
            limit,
        )?);
    }
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}

fn current_symbol_span(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
) -> anyhow::Result<(Option<i64>, Option<i64>, Option<i64>)> {
    let span = conn
        .query_row(
            "
            SELECT chunks.id, chunks.start_line, chunks.end_line
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path = ?1
              AND (chunks.symbol_path = ?2 OR chunks.symbol_path = ?3)
            ORDER BY
              CASE WHEN chunks.symbol_path = ?2 THEN 0 ELSE 1 END,
              chunks.start_line
            LIMIT 1
            ",
            params![symbol.path, symbol.qualified_name, symbol.symbol_path],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        )
        .optional()?;
    Ok(match span {
        Some((chunk_id, start_line, end_line)) => {
            (Some(start_line), Some(end_line), Some(chunk_id))
        },
        None => (None, None, None),
    })
}

fn evidence_for_issue(
    conn: &Connection,
    owner: &str,
    repo: &str,
    number: i64,
    limit: u32,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number, item_kind, item_id, url, title, body, classification, 0.0
        FROM github_fts
        WHERE owner = ?1 AND repo = ?2 AND number = ?3
        LIMIT ?4
        ",
    )?;
    let rows = stmt.query_map(params![owner, repo, number, i64::from(limit)], evidence_row)?;
    let mut evidence = collect_rows(rows)?;
    for item in &mut evidence {
        item.evidence_kind = "literal_github_ref";
        item.score = 1.0;
    }
    Ok(evidence)
}

fn evidence_for_commit_refs(
    conn: &Connection,
    commit_hash: &str,
    limit: u32,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number
        FROM github_refs
        WHERE source_kind = 'commit'
          AND source_commit LIKE ?1
        ORDER BY ref_kind = 'closing' DESC, id DESC
        LIMIT ?2
        ",
    )?;
    let commit_like = format!("{commit_hash}%");
    let refs = stmt.query_map(params![commit_like, i64::from(limit)], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
    })?;
    let mut evidence = Vec::new();
    for reference in refs {
        let (owner, repo, number) = reference?;
        evidence.extend(evidence_for_issue(conn, &owner, &repo, number, limit)?);
    }
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}

fn search_fts(
    conn: &Connection,
    query: &str,
    kind: Option<&str>,
    limit: u32,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    let fts_query = fts_query(query);
    let kind_clause = kind.map(|_| "AND item_kind = ?3").unwrap_or("");
    let sql = format!(
        "
        SELECT owner, repo, number, item_kind, item_id, url, title, body, classification,
               bm25(github_fts) AS score
        FROM github_fts
        WHERE github_fts MATCH ?1
        {kind_clause}
        ORDER BY score
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(kind) = kind {
        stmt.query_map(params![fts_query, i64::from(limit), kind], evidence_row)?
    } else {
        stmt.query_map(params![fts_query, i64::from(limit)], evidence_row)?
    };
    let mut hits = collect_rows(rows)?;
    for (rank, hit) in hits.iter_mut().enumerate() {
        hit.score = positive_rank_score(rank);
    }
    Ok(hits)
}

fn positive_rank_score(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).sqrt()
}

fn dedupe_evidence(evidence: &mut Vec<GitHubEvidence>) {
    let mut seen = BTreeSet::new();
    evidence.retain(|item| {
        seen.insert((
            item.owner.clone(),
            item.repo.clone(),
            item.number,
            item.item_kind.clone(),
            item.item_id.clone(),
        ))
    });
}

fn evidence_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GitHubEvidence> {
    let title: String = row.get(6)?;
    let body: String = row.get(7)?;
    Ok(GitHubEvidence {
        owner: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        item_kind: row.get(3)?,
        item_id: row.get(4)?,
        url: row.get(5)?,
        title,
        snippet: snippet(&body),
        classification: row.get(8)?,
        evidence_kind: "historical_github",
        score: row.get(9)?,
    })
}

fn ref_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GitHubRef> {
    Ok(GitHubRef {
        owner: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        ref_kind: row.get(3)?,
        source_kind: row.get(4)?,
        source_path: row.get(5)?,
        source_commit: row.get(6)?,
        source_text: row.get(7)?,
    })
}

fn refs(conn: &Connection) -> anyhow::Result<Vec<GitHubRef>> {
    let mut stmt = conn.prepare(
        "SELECT owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text FROM github_refs",
    )?;
    let rows = stmt.query_map([], ref_row)?;
    collect_rows(rows)
}

#[derive(Debug, Clone)]
struct ParsedRef {
    owner: String,
    repo: String,
    number: i64,
    kind: String,
}

fn parse_refs(text: &str, default_repo: Option<&str>) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    let tokens = text
        .split(|c: char| c.is_whitespace() || [',', ';', ')', ']', '}'].contains(&c))
        .map(|token| token.trim_matches(|c: char| ['(', '[', '{', '.', ':'].contains(&c)))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let mut previous = "";
    for token in tokens {
        let kind = ref_kind(previous);
        if let Some(parsed) = parse_issue_ref(token, default_repo) {
            refs.push(ParsedRef { kind, ..parsed });
        }
        previous = token;
    }
    refs
}

fn parse_issue_ref(token: &str, default_repo: Option<&str>) -> Option<ParsedRef> {
    if let Some(rest) = token.strip_prefix("https://github.com/") {
        let parts = rest.split('/').collect::<Vec<_>>();
        if parts.len() >= 4 && (parts[2] == "issues" || parts[2] == "pull") {
            return Some(ParsedRef {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number: parts[3].parse().ok()?,
                kind: "url".to_string(),
            });
        }
    }
    if let Some((repo_ref, number)) = token.split_once('#') {
        let parts = repo_ref.split('/').collect::<Vec<_>>();
        if parts.len() == 2 {
            return Some(ParsedRef {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number: number.parse().ok()?,
                kind: "cross_repo".to_string(),
            });
        }
    }
    if let Some(number) = token.strip_prefix("GH-") {
        let (owner, repo) = split_repo(default_repo?)?;
        return Some(ParsedRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: number.parse().ok()?,
            kind: "gh_dash".to_string(),
        });
    }
    if let Some(number) = token.strip_prefix('#') {
        let (owner, repo) = split_repo(default_repo?)?;
        return Some(ParsedRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: number.parse().ok()?,
            kind: "local_number".to_string(),
        });
    }
    None
}

fn ref_kind(previous: &str) -> String {
    let previous = previous.to_ascii_lowercase();
    if ["fixes", "fixed", "closes", "closed", "resolves", "resolved"].contains(&previous.as_str()) {
        "closing".to_string()
    } else if ["refs", "ref", "see", "related"].contains(&previous.as_str()) {
        "reference".to_string()
    } else {
        "unknown".to_string()
    }
}

fn classify_text(text: &str) -> String {
    let text = text.to_ascii_lowercase();
    if text.contains("decided") || text.contains("decision") || text.contains("we will") {
        "decision"
    } else if text.contains("rejected") || text.contains("alternative") || text.contains("instead")
    {
        "rejected_alternative"
    } else if text.contains("must") || text.contains("constraint") || text.contains("required") {
        "constraint"
    } else if text.contains("risk") || text.contains("concern") || text.contains("blocked") {
        "risk"
    } else if text.contains("obsolete") || text.contains("deprecated") || text.contains("no longer")
    {
        "obsolete"
    } else {
        "context"
    }
    .to_string()
}

fn issue_from_value(owner: &str, repo: &str, value: &Value) -> GitHubIssue {
    GitHubIssue {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number: value["number"].as_i64().unwrap_or_default(),
        html_url: string_value(value, "html_url"),
        state: string_value(value, "state"),
        title: string_value(value, "title"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        is_pull_request: value.get("pull_request").is_some(),
    }
}

fn comment_from_value(owner: &str, repo: &str, number: i64, value: &Value) -> GitHubComment {
    GitHubComment {
        id: value["id"].as_i64().unwrap_or_default(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        html_url: string_value(value, "html_url"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
    }
}

fn pull_from_value(owner: &str, repo: &str, number: i64, value: &Value) -> GitHubPullRequest {
    GitHubPullRequest {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        html_url: string_value(value, "html_url"),
        state: string_value(value, "state"),
        title: string_value(value, "title"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        merged_at: value["merged_at"].as_str().map(str::to_string),
    }
}

fn review_from_value(owner: &str, repo: &str, number: i64, value: &Value) -> GitHubReview {
    GitHubReview {
        id: value["id"].as_i64().unwrap_or_default(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        html_url: value["html_url"].as_str().map(str::to_string),
        state: string_value(value, "state"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        submitted_at: value["submitted_at"].as_str().map(str::to_string),
    }
}

fn review_comment_from_value(
    owner: &str,
    repo: &str,
    number: i64,
    value: &Value,
) -> GitHubReviewComment {
    GitHubReviewComment {
        id: value["id"].as_i64().unwrap_or_default(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        path: value["path"].as_str().map(str::to_string),
        html_url: string_value(value, "html_url"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
    }
}

fn gh_api_json(path: &str) -> anyhow::Result<Value> {
    let output = Command::new("gh").args(["api", path]).output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn gh_api_paginated(path: &str) -> anyhow::Result<Vec<Value>> {
    let output = Command::new("gh").args(["api", "--paginate", "--slurp", path]).output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let mut out = Vec::new();
    if let Some(pages) = value.as_array() {
        for page in pages {
            if let Some(items) = page.as_array() {
                out.extend(items.iter().cloned());
            }
        }
    }
    Ok(out)
}

fn default_repo() -> Option<String> {
    let output = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn gh_available() -> bool {
    Command::new("gh").arg("--version").output().is_ok_and(|output| output.status.success())
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(root).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn string_value(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn split_repo(value: &str) -> Option<(&str, &str)> {
    value.split_once('/')
}

fn snippet(text: &str) -> String {
    text.lines().take(3).collect::<Vec<_>>().join("\n")
}

fn fts_query(query: &str) -> String {
    let terms = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() { "\"\"".to_string() } else { terms.join(" OR ") }
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn count_table(conn: &Connection, table: &str) -> anyhow::Result<u64> {
    let count =
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

fn meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
