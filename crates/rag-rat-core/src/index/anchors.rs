use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct ChunkAnchor {
    pub version: i64,
    pub normalized_hash: String,
    pub start_context_hash: String,
    pub end_context_hash: String,
    pub context_radius: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorStatus {
    Exact,
    Relocated { start_line: usize, end_line: usize, text: String },
    Stale,
}

pub fn anchor_for_text(
    text: &str,
    start_line: usize,
    end_line: usize,
    full_text: &str,
) -> ChunkAnchor {
    let lines = full_text.lines().collect::<Vec<_>>();
    let radius = 2;
    ChunkAnchor {
        version: 1,
        normalized_hash: hash_normalized(text),
        start_context_hash: context_hash(&lines, start_line, radius),
        end_context_hash: context_hash(&lines, end_line, radius),
        context_radius: i64::try_from(radius).unwrap_or(2),
    }
}

pub fn validate(
    stored_text: &str,
    start_line: usize,
    end_line: usize,
    anchor: &ChunkAnchor,
    current_text: &str,
) -> AnchorStatus {
    let Some(current_slice) = slice_lines(current_text, start_line, end_line) else {
        return relocate(stored_text, anchor, current_text).unwrap_or(AnchorStatus::Stale);
    };
    if hash_normalized(&current_slice) == anchor.normalized_hash {
        return AnchorStatus::Exact;
    }
    relocate(stored_text, anchor, current_text).unwrap_or(AnchorStatus::Stale)
}

pub fn hash_normalized(text: &str) -> String {
    let normalized =
        text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>();
    hex_sha256(normalized.join("\n").as_bytes())
}

fn relocate(stored_text: &str, anchor: &ChunkAnchor, current_text: &str) -> Option<AnchorStatus> {
    let wanted_hash = if anchor.normalized_hash.is_empty() {
        hash_normalized(stored_text)
    } else {
        anchor.normalized_hash.clone()
    };
    let wanted_lines = stored_text.lines().count().max(1);
    let lines = current_text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    for start in 1..=lines.len() {
        let end = (start + wanted_lines - 1).min(lines.len());
        let Some(candidate) = slice_lines(current_text, start, end) else {
            continue;
        };
        if hash_normalized(&candidate) == wanted_hash {
            let start_context =
                context_hash(&lines, start, usize::try_from(anchor.context_radius).unwrap_or(2));
            let end_context =
                context_hash(&lines, end, usize::try_from(anchor.context_radius).unwrap_or(2));
            if start_context == anchor.start_context_hash || end_context == anchor.end_context_hash
            {
                return Some(AnchorStatus::Relocated {
                    start_line: start,
                    end_line: end,
                    text: candidate,
                });
            }
        }
    }
    None
}

fn slice_lines(text: &str, start_line: usize, end_line: usize) -> Option<String> {
    if start_line == 0 || end_line < start_line {
        return None;
    }
    let selected = text
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let line_no = idx + 1;
            (line_no >= start_line && line_no <= end_line).then_some(line)
        })
        .collect::<Vec<_>>();
    (!selected.is_empty()).then(|| {
        let mut text = selected.join("\n");
        text.push('\n');
        text
    })
}

fn context_hash(lines: &[&str], line: usize, radius: usize) -> String {
    if lines.is_empty() {
        return hex_sha256(b"");
    }
    let line = line.saturating_sub(1);
    let start = line.saturating_sub(radius);
    let end = (line + radius + 1).min(lines.len());
    let normalized =
        lines[start..end].iter().map(|line| line.trim()).collect::<Vec<_>>().join("\n");
    hex_sha256(normalized.as_bytes())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}
