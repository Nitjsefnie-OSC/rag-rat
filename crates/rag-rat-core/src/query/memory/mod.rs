mod api;
mod resolve;
mod validate;
use std::collections::BTreeSet;

pub use api::memory_evidence_for_symbol;
pub(crate) use api::*;
pub(crate) use resolve::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub(crate) use validate::*;

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemory {
    pub memory_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub confidence: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub source: String,
    // Internal anchoring/dedup mechanics — never actionable for a reader, so kept off the wire.
    #[serde(skip_serializing)]
    pub source_text_hash: Option<String>,
    #[serde(skip_serializing)]
    pub input_hash: Option<String>,
    #[serde(skip_serializing)]
    pub memory_version: String,
    pub bindings: Vec<RepoMemoryBinding>,
    pub call_paths: Vec<RepoMemoryCallPath>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryBinding {
    pub memory_id: String,
    pub binding_kind: String,
    pub binding_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_symbol_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_hash: Option<String>,
    pub anchor_status: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryCallPath {
    pub memory_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_logical_symbol_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_logical_symbol_id: Option<i64>,
    pub edge_sequence_hash: String,
    pub path_summary: String,
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
    pub edge_id: Option<i64>,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub commit_hash: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
    pub start_logical_symbol_id: Option<i64>,
    pub end_logical_symbol_id: Option<i64>,
    pub edge_sequence_hash: Option<String>,
    pub path_summary: Option<String>,
    /// Directory anchor: a repo-root-relative directory path, or `""` for the repo root.
    /// Normalized on resolve (trim, drop leading `./`, strip trailing `/`).
    pub dir: Option<String>,
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
    pub path_crossed: Vec<RepoMemory>,
    pub stale: Vec<RepoMemory>,
}

#[derive(Debug)]
pub(crate) struct ResolvedBinding {
    binding_kind: String,
    binding_id: String,
    path: Option<String>,
    start_line: Option<i64>,
    end_line: Option<i64>,
    logical_symbol_id: Option<i64>,
    symbol_id: Option<i64>,
    chunk_id: Option<i64>,
    edge_id: Option<i64>,
    commit_hash: Option<String>,
    github_owner: Option<String>,
    github_repo: Option<String>,
    github_number: Option<i64>,
    symbol_kind: Option<String>,
    signature_hash: Option<String>,
    call_path: Option<ResolvedCallPath>,
    source_text_hash: Option<String>,
    anchor_status: String,
}

#[derive(Debug)]
struct ResolvedCallPath {
    start_logical_symbol_id: Option<i64>,
    end_logical_symbol_id: Option<i64>,
    edge_sequence_hash: String,
    path_summary: String,
}

#[derive(Debug)]
pub(crate) struct ChunkAnchor {
    chunk_id: i64,
    path: String,
    start_line: i64,
    end_line: i64,
    symbol_path: Option<String>,
    text_hash: String,
    symbol_id: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct EdgeAnchor {
    edge_id: i64,
    fingerprint: String,
    path: String,
    start_line: i64,
    end_line: i64,
    source_hash: String,
}

pub(crate) struct EdgeFingerprintParts<'a> {
    path: &'a str,
    start_line: i64,
    end_line: i64,
    from_name: Option<&'a str>,
    to_name: Option<&'a str>,
    edge_kind: &'a str,
    target_qualified_name: Option<&'a str>,
    receiver_hint: Option<&'a str>,
}
