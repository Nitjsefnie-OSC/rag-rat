use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{Config, IndexDatabase};

const TOP_K: usize = 10;

#[derive(Debug, Clone, Deserialize)]
pub struct EvalSuite {
    #[serde(default)]
    pub query: Vec<EvalQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedSuite {
    #[serde(default)]
    pub expected: Vec<ExpectedQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalQuery {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub must_include_paths: Vec<String>,
    #[serde(default)]
    pub must_include_symbols: Vec<String>,
    #[serde(default)]
    pub should_include_git_subjects: Vec<String>,
    #[serde(default)]
    pub should_include_papertrail_kinds: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExpectedQuery {
    pub id: String,
    #[serde(default)]
    pub must_include_paths: Vec<String>,
    #[serde(default)]
    pub must_include_symbols: Vec<String>,
    #[serde(default)]
    pub should_include_git_subjects: Vec<String>,
    #[serde(default)]
    pub should_include_papertrail_kinds: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub queries_path: PathBuf,
    pub expected_path: PathBuf,
    pub update_baseline: bool,
}

#[derive(Debug, Serialize)]
pub struct EvalReport {
    pub pass: bool,
    pub queries: usize,
    pub metrics: EvalMetrics,
    pub results: Vec<EvalQueryReport>,
}

#[derive(Debug, Serialize)]
pub struct EvalMetrics {
    pub mrr_at_10: f64,
    pub recall_at_10: f64,
    pub path_hit_rate: f64,
    pub symbol_hit_rate: f64,
    pub stale_hit_rate: f64,
    pub stale_current_source_violations: u64,
    pub current_source_violation_count: u64,
    pub papertrail_precision_sample: Option<f64>,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct EvalQueryReport {
    pub id: String,
    pub text: String,
    pub passed: bool,
    pub reciprocal_rank_at_10: f64,
    pub recall_at_10: f64,
    pub path_hits: Vec<String>,
    pub missing_paths: Vec<String>,
    pub symbol_hits: Vec<String>,
    pub missing_symbols: Vec<String>,
    pub git_subject_hits: Vec<String>,
    pub missing_git_subjects: Vec<String>,
    pub papertrail_kind_hits: Vec<String>,
    pub missing_papertrail_kinds: Vec<String>,
    pub papertrail_precision_sample: Option<f64>,
    pub stale_current_source_violations: u64,
    pub current_source_violations: Vec<CurrentSourceViolation>,
    pub latency_ms: f64,
    pub top_hits: Vec<EvalSearchHit>,
}

#[derive(Debug, Serialize)]
pub struct EvalSearchHit {
    pub rank: usize,
    pub chunk_id: i64,
    pub path: String,
    pub symbol_path: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct CurrentSourceViolation {
    pub chunk_id: i64,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
struct BaselineSuite {
    expected: Vec<ExpectedQuery>,
}

pub fn run(config: &Config, options: &EvalOptions) -> anyhow::Result<EvalReport> {
    let suite = load_queries(&options.queries_path)?;
    let expected = load_expected(&options.expected_path)?;
    let db = IndexDatabase::open(&config.database)?;
    let mut results = Vec::new();
    let mut observed = Vec::new();

    for query in suite.query {
        let expected_query = expected.get(&query.id);
        let merged = merge_expected(query, expected_query);
        let report = evaluate_query(config, &db, &merged)?;
        observed.push(observed_expected(&report));
        results.push(report);
    }

    if options.update_baseline {
        write_baseline(&options.expected_path, observed)?;
    }

    let metrics = aggregate(&results);
    let pass = metrics.stale_current_source_violations == 0 && results.iter().all(|r| r.passed);
    Ok(EvalReport { pass, queries: results.len(), metrics, results })
}

fn load_queries(path: &Path) -> anyhow::Result<EvalSuite> {
    let text = fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("failed to read eval queries {}: {err}", path.display()))?;
    toml::from_str(&text)
        .map_err(|err| anyhow::anyhow!("failed to parse eval queries {}: {err}", path.display()))
}

fn load_expected(path: &Path) -> anyhow::Result<BTreeMap<String, ExpectedQuery>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(path).map_err(|err| {
        anyhow::anyhow!("failed to read eval expected hits {}: {err}", path.display())
    })?;
    let suite: ExpectedSuite = toml::from_str(&text).map_err(|err| {
        anyhow::anyhow!("failed to parse eval expected hits {}: {err}", path.display())
    })?;
    Ok(suite.expected.into_iter().map(|expected| (expected.id.clone(), expected)).collect())
}

fn merge_expected(query: EvalQuery, expected: Option<&ExpectedQuery>) -> EvalQuery {
    let Some(expected) = expected else {
        return query;
    };
    EvalQuery {
        id: query.id,
        text: query.text,
        must_include_paths: union(query.must_include_paths, &expected.must_include_paths),
        must_include_symbols: union(query.must_include_symbols, &expected.must_include_symbols),
        should_include_git_subjects: union(
            query.should_include_git_subjects,
            &expected.should_include_git_subjects,
        ),
        should_include_papertrail_kinds: union(
            query.should_include_papertrail_kinds,
            &expected.should_include_papertrail_kinds,
        ),
    }
}

fn union(mut values: Vec<String>, extra: &[String]) -> Vec<String> {
    let mut seen = values.iter().cloned().collect::<BTreeSet<_>>();
    for value in extra {
        if seen.insert(value.clone()) {
            values.push(value.clone());
        }
    }
    values
}

fn evaluate_query(
    config: &Config,
    db: &IndexDatabase,
    query: &EvalQuery,
) -> anyhow::Result<EvalQueryReport> {
    let started = Instant::now();
    let mut hits = db.search(&query.text, TOP_K as u32, false)?;
    let mut latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut current_source_violations = find_current_source_violations(config, db, &hits);
    if !current_source_violations.is_empty() {
        let retry_started = Instant::now();
        hits = db.search(&query.text, TOP_K as u32, false)?;
        latency_ms += retry_started.elapsed().as_secs_f64() * 1000.0;
        current_source_violations = find_current_source_violations(config, db, &hits);
    }
    let top_hits = top_hits(&hits);

    let path_hits = query
        .must_include_paths
        .iter()
        .filter(|expected| hits.iter().any(|hit| hit.path == **expected))
        .cloned()
        .collect::<Vec<_>>();
    let missing_paths = missing(&query.must_include_paths, &path_hits);
    let symbol_hits = query
        .must_include_symbols
        .iter()
        .filter(|expected| {
            hits.iter()
                .filter_map(|hit| hit.symbol_path.as_deref())
                .any(|symbol| symbol == expected.as_str() || symbol.ends_with(expected.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_symbols = missing(&query.must_include_symbols, &symbol_hits);

    let commit_hits = db.commit_search(&query.text, TOP_K as u32).unwrap_or_default();
    let git_subject_hits = query
        .should_include_git_subjects
        .iter()
        .filter(|expected| {
            let needle = expected.to_ascii_lowercase();
            commit_hits.iter().any(|hit| hit.subject.to_ascii_lowercase().contains(&needle))
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_git_subjects = missing(&query.should_include_git_subjects, &git_subject_hits);

    let papertrail = db.rationale_search(&query.text, TOP_K as u32).unwrap_or_default();
    let papertrail_kind_hits = query
        .should_include_papertrail_kinds
        .iter()
        .filter(|expected| {
            let needle = normalize_kind(expected);
            papertrail.iter().any(|item| normalize_kind(&item.classification) == needle)
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_papertrail_kinds =
        missing(&query.should_include_papertrail_kinds, &papertrail_kind_hits);
    let papertrail_precision_sample = if query.should_include_papertrail_kinds.is_empty() {
        None
    } else if papertrail.is_empty() {
        Some(0.0)
    } else {
        let expected = query
            .should_include_papertrail_kinds
            .iter()
            .map(|kind| normalize_kind(kind))
            .collect::<BTreeSet<_>>();
        let matched = papertrail
            .iter()
            .filter(|item| expected.contains(&normalize_kind(&item.classification)))
            .count();
        Some(matched as f64 / papertrail.len() as f64)
    };

    let stale_current_source_violations =
        u64::try_from(current_source_violations.len()).unwrap_or(u64::MAX);
    let relevant_rank = hits.iter().position(|hit| relevant(hit, query)).map(|rank| rank + 1);
    let reciprocal_rank_at_10 = relevant_rank.map(|rank| 1.0 / rank as f64).unwrap_or(0.0);
    let expected_relevant = query.must_include_paths.len() + query.must_include_symbols.len();
    let found_relevant = path_hits.len() + symbol_hits.len();
    let recall_at_10 =
        if expected_relevant == 0 { 1.0 } else { found_relevant as f64 / expected_relevant as f64 };
    let passed = stale_current_source_violations == 0
        && missing_paths.is_empty()
        && missing_symbols.is_empty()
        && missing_git_subjects.is_empty()
        && missing_papertrail_kinds.is_empty();

    Ok(EvalQueryReport {
        id: query.id.clone(),
        text: query.text.clone(),
        passed,
        reciprocal_rank_at_10,
        recall_at_10,
        path_hits,
        missing_paths,
        symbol_hits,
        missing_symbols,
        git_subject_hits,
        missing_git_subjects,
        papertrail_kind_hits,
        missing_papertrail_kinds,
        papertrail_precision_sample,
        stale_current_source_violations,
        current_source_violations,
        latency_ms,
        top_hits,
    })
}

fn top_hits(hits: &[crate::search::lexical::SearchHit]) -> Vec<EvalSearchHit> {
    hits.iter()
        .enumerate()
        .map(|(index, hit)| EvalSearchHit {
            rank: index + 1,
            chunk_id: hit.chunk_id,
            path: hit.path.clone(),
            symbol_path: hit.symbol_path.clone(),
            start_line: hit.start_line,
            end_line: hit.end_line,
            score: hit.score,
        })
        .collect()
}

fn relevant(hit: &crate::search::lexical::SearchHit, query: &EvalQuery) -> bool {
    query.must_include_paths.iter().any(|path| path == &hit.path)
        || hit.symbol_path.as_deref().is_some_and(|symbol| {
            query
                .must_include_symbols
                .iter()
                .any(|expected| symbol == expected || symbol.ends_with(expected))
        })
}

fn missing(expected: &[String], found: &[String]) -> Vec<String> {
    let found = found.iter().collect::<BTreeSet<_>>();
    expected.iter().filter(|value| !found.contains(value)).cloned().collect()
}

fn find_current_source_violations(
    config: &Config,
    db: &IndexDatabase,
    hits: &[crate::search::lexical::SearchHit],
) -> Vec<CurrentSourceViolation> {
    let mut violations = Vec::new();
    let mut checked = BTreeSet::new();
    for hit in hits {
        if !checked.insert(hit.chunk_id) {
            continue;
        }
        match db.read_chunk(hit.chunk_id) {
            Ok(Some(chunk)) => {
                let source_path = config.root.join(&chunk.path);
                match fs::read_to_string(&source_path) {
                    Ok(source) => {
                        let current = slice_lines(&source, chunk.start_line, chunk.end_line);
                        if current.as_deref() != Some(chunk.text.as_str()) {
                            violations.push(CurrentSourceViolation {
                                chunk_id: hit.chunk_id,
                                path: chunk.path,
                                reason: "read_chunk text differs from current source line span"
                                    .to_string(),
                            });
                        }
                    },
                    Err(err) => violations.push(CurrentSourceViolation {
                        chunk_id: hit.chunk_id,
                        path: chunk.path,
                        reason: format!("current source unreadable: {err}"),
                    }),
                }
            },
            Ok(None) => violations.push(CurrentSourceViolation {
                chunk_id: hit.chunk_id,
                path: hit.path.clone(),
                reason: "search hit chunk is missing".to_string(),
            }),
            Err(err) => violations.push(CurrentSourceViolation {
                chunk_id: hit.chunk_id,
                path: hit.path.clone(),
                reason: format!("read_chunk failed: {err}"),
            }),
        }
    }
    violations
}

fn slice_lines(source: &str, start_line: i64, end_line: i64) -> Option<String> {
    let start = usize::try_from(start_line).ok()?.max(1);
    let end = usize::try_from(end_line).ok()?.max(start);
    let lines = source.lines().collect::<Vec<_>>();
    if start > lines.len() {
        return None;
    }
    let mut text = lines[(start - 1)..end.min(lines.len())].join("\n");
    text.push('\n');
    Some(text)
}

fn normalize_kind(kind: &str) -> String {
    kind.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn aggregate(results: &[EvalQueryReport]) -> EvalMetrics {
    let query_count = results.len().max(1) as f64;
    let total_hits = results.iter().map(|r| r.top_hits.len() as u64).sum::<u64>();
    let stale = results.iter().map(|r| r.stale_current_source_violations).sum::<u64>();
    let papertrail_samples =
        results.iter().filter_map(|r| r.papertrail_precision_sample).collect::<Vec<_>>();
    EvalMetrics {
        mrr_at_10: results.iter().map(|r| r.reciprocal_rank_at_10).sum::<f64>() / query_count,
        recall_at_10: results.iter().map(|r| r.recall_at_10).sum::<f64>() / query_count,
        path_hit_rate: hit_rate(results, |r| r.missing_paths.is_empty()),
        symbol_hit_rate: hit_rate(results, |r| r.missing_symbols.is_empty()),
        stale_hit_rate: if total_hits == 0 { 0.0 } else { stale as f64 / total_hits as f64 },
        stale_current_source_violations: stale,
        current_source_violation_count: stale,
        papertrail_precision_sample: (!papertrail_samples.is_empty())
            .then(|| papertrail_samples.iter().sum::<f64>() / papertrail_samples.len() as f64),
        latency_p50_ms: percentile(results.iter().map(|r| r.latency_ms).collect(), 0.50),
        latency_p95_ms: percentile(results.iter().map(|r| r.latency_ms).collect(), 0.95),
    }
}

fn hit_rate(results: &[EvalQueryReport], predicate: fn(&EvalQueryReport) -> bool) -> f64 {
    if results.is_empty() {
        return 1.0;
    }
    results.iter().filter(|result| predicate(result)).count() as f64 / results.len() as f64
}

fn percentile(mut values: Vec<f64>, percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

fn observed_expected(report: &EvalQueryReport) -> ExpectedQuery {
    let mut paths = report.top_hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
    dedup(&mut paths);
    let mut symbols =
        report.top_hits.iter().filter_map(|hit| hit.symbol_path.clone()).collect::<Vec<_>>();
    dedup(&mut symbols);
    ExpectedQuery {
        id: report.id.clone(),
        must_include_paths: paths,
        must_include_symbols: symbols,
        should_include_git_subjects: report.git_subject_hits.clone(),
        should_include_papertrail_kinds: report.papertrail_kind_hits.clone(),
    }
}

fn dedup(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn write_baseline(path: &Path, expected: Vec<ExpectedQuery>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(&BaselineSuite { expected })?;
    fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Config, IndexDatabase};

    #[test]
    fn eval_suite_reports_search_quality_and_current_source_safety() {
        let root = fixture_root();
        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        IndexDatabase::rebuild(&config).unwrap();
        let report = run(
            &config,
            &EvalOptions {
                queries_path: workspace_root().join("evals/queries.toml"),
                expected_path: workspace_root().join("evals/expected_hits.toml"),
                update_baseline: false,
            },
        )
        .unwrap();
        assert_eq!(report.metrics.stale_current_source_violations, 0);
        assert!(report.metrics.mrr_at_10 > 0.0);
        assert!(report.metrics.recall_at_10 > 0.0);
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
    }

    fn fixture_root() -> PathBuf {
        workspace_root().join("tests/fixtures/held-mini")
    }
}
