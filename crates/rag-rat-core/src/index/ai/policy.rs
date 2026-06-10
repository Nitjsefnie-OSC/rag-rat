use super::*;

pub(crate) fn needs_embedding(
    chunk: &CurrentChunk,
    model_id: &str,
    model_version: &str,
    dim: usize,
    max_embedding_chars: usize,
) -> bool {
    let input = build_embedding_input(chunk, max_embedding_chars);
    let expected_input_hash = embedding_input_hash(model_id, model_version, &input.text);
    chunk.embedding_status.as_deref() != Some("Current")
        || chunk.source_text_hash.as_deref() != Some(chunk.text_hash.as_str())
        || chunk.model_version.as_deref() != Some(model_version)
        || chunk.embedding_dim != Some(i64::try_from(dim).unwrap_or(i64::MAX))
        || chunk.input_hash.as_deref() != Some(expected_input_hash.as_str())
        || chunk.embedding_text_version.as_deref() != Some(EMBEDDING_TEXT_VERSION)
}

pub(crate) fn embedding_policy_for_chunk(
    path: &Path,
    language: &str,
    file_kind: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
    text: &str,
    max_embedding_chars: usize,
) -> EmbeddingPolicyDecision {
    let path_text = path.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.chars().count() > max_embedding_chars.saturating_mul(4)
        && (file_kind == "generated" || chunk_kind == "generated" || symbol_path.is_none())
    {
        return policy("SkipTooLarge", 9, false);
    }
    if file_kind == "generated" || chunk_kind == "generated" || looks_generated_path(&path_text) {
        return policy("SkipGenerated", 9, false);
    }
    if is_test_fixture_path(&path_text) {
        return policy("SkipTestFixture", 9, false);
    }
    let Ok(language_kind) = language.parse::<Language>() else {
        return policy("SkipLanguageUnsupported", 9, false);
    };
    if !language_kind.supports_embeddings() {
        return policy("SkipLanguageUnsupported", 9, false);
    }
    if trimmed.chars().count() < MIN_EMBEDDING_CHARS {
        return policy("SkipTooSmall", 9, false);
    }
    if is_low_signal_chunk(language, chunk_kind, symbol_path, trimmed) {
        return policy("SkipLowSignal", 9, false);
    }
    policy("Embed", embedding_priority(&path_text, language, chunk_kind, symbol_path), true)
}

pub(crate) fn policy(name: &str, priority: i64, eligible: bool) -> EmbeddingPolicyDecision {
    EmbeddingPolicyDecision { policy: name.to_string(), priority, eligible }
}

pub(crate) fn policy_for_job(
    chunk: &CurrentChunk,
    max_embedding_chars: usize,
) -> EmbeddingPolicyDecision {
    embedding_policy_for_chunk(
        Path::new(&chunk.path),
        &chunk.language,
        &chunk.file_kind,
        &chunk.chunk_kind,
        chunk.symbol_path.as_deref(),
        &chunk.text,
        max_embedding_chars,
    )
}

pub(crate) fn embedding_priority(
    path: &str,
    language: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
) -> i64 {
    if symbol_path.is_some()
        && matches!(chunk_kind, "code")
        && !is_test_path(path)
        && language != "markdown"
    {
        return 0;
    }
    if language == "markdown" {
        return 1;
    }
    if is_test_path(path) {
        return 2;
    }
    1
}

pub(crate) fn priority_label(priority: i64) -> &'static str {
    match priority {
        0 => "source_symbols",
        1 => "source_or_docs",
        2 => "tests",
        3 => "low_signal",
        9 => "skipped",
        _ => "other",
    }
}

pub(crate) fn looks_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/src/generated/")
        || path.contains("/target/")
        || path.ends_with("Cargo.lock")
        || path.ends_with("package-lock.json")
        || path.ends_with("pnpm-lock.yaml")
}

pub(crate) fn is_test_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("__tests__")
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with(".spec.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.tsx")
}

pub(crate) fn is_test_fixture_path(path: &str) -> bool {
    path.contains("/fixtures/")
        || path.contains("/__fixtures__/")
        || path.contains("/testdata/")
        || path.contains("/snapshots/")
        || path.ends_with(".snap")
}

pub(crate) fn is_low_signal_chunk(
    language: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
    text: &str,
) -> bool {
    if language == "markdown" {
        return false;
    }
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with("/*"))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return true;
    }
    if symbol_path.is_none() && chunk_kind == "code" && lines.len() <= 3 {
        return true;
    }
    lines.iter().all(|line| {
        line.starts_with("use ")
            || line.starts_with("pub use ")
            || line.starts_with("import ")
            || line.starts_with("export ")
            || line.starts_with("mod ")
            || line.starts_with("pub mod ")
            || *line == "}"
            || *line == "{"
    })
}
