use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use dialoguer::{Confirm, MultiSelect, Select};
use rag_rat_core::{
    Config, IndexDatabase,
    index::ai::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, ReconcileOptions},
    language::Language,
};
use termtree::Tree;

use crate::{
    apply_embedding_runtime_env, git_paths, render_index_progress, render_reconcile_progress,
};

const CONFIG_FILE: &str = "rag-rat.toml";
const DEFAULT_DATABASE: &str = ".rag-rat/index.sqlite";
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".rag-rat",
    ".direnv",
    ".next",
    ".turbo",
    ".venv",
    "build",
    "dist",
    "node_modules",
    "target",
];

#[derive(Debug, Clone)]
struct InitOptions {
    yes: bool,
    dry_run: bool,
    force: bool,
    config_path: PathBuf,
}

#[derive(Debug, Clone)]
struct InitPlan {
    root_value: String,
    languages: Vec<Language>,
    bindings: BTreeMap<Language, Vec<PathBuf>>,
}

#[derive(Debug, Default)]
struct RepoScan {
    language_counts: BTreeMap<Language, usize>,
    dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
}

#[derive(Debug, Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let options = InitOptions::from_args(args)?;
    let root = env::current_dir()?.canonicalize()?;
    let scan = scan_repo(&root)?;
    let root_value = config_root_value(&root, &options.config_path);
    let plan = if options.yes {
        default_plan(root_value, &scan)
    } else {
        prompt_plan(root, root_value, &scan)?
    };
    let config_text = render_config(&plan);

    if options.dry_run {
        println!("{config_text}");
        return Ok(());
    }

    if options.config_path.exists() && !options.force && !options.yes {
        let overwrite = Confirm::new()
            .with_prompt(format!("Overwrite {}?", options.config_path.display()))
            .default(false)
            .interact()?;
        if !overwrite {
            anyhow::bail!("init cancelled; {} already exists", options.config_path.display());
        }
    }

    if let Some(parent) = options.config_path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(&options.config_path, config_text)?;
    eprintln!("init: wrote {}", options.config_path.display());

    let config = Config::load(&options.config_path)?;
    apply_embedding_runtime_env(&config.local_ai.embedding.runtime);
    let db = setup_index(&config)?;
    setup_model_and_reconcile(&config, &db, options.yes)?;
    offer_mcp_install(&config, &options.config_path, options.yes)?;
    offer_hooks_install(&config, options.yes)?;
    eprintln!("init: complete");
    Ok(())
}

impl InitOptions {
    fn from_args(args: &[String]) -> anyhow::Result<Self> {
        Ok(Self {
            yes: has_flag(args, "--yes") || has_flag(args, "-y"),
            dry_run: has_flag(args, "--dry-run"),
            force: has_flag(args, "--force"),
            config_path: option_value(args, "--config")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(CONFIG_FILE)),
        })
    }
}

fn prompt_plan(root: PathBuf, root_value: String, scan: &RepoScan) -> anyhow::Result<InitPlan> {
    println!("Repository root: {}", root.display());
    println!();
    println!("Detected source tree:");
    print_language_tree(scan)?;
    println!();

    let language_items = supported_languages()
        .iter()
        .map(|language| {
            let count = scan.language_counts.get(language).copied().unwrap_or_default();
            format!("{} ({count} files)", language.as_str())
        })
        .collect::<Vec<_>>();
    let defaults = supported_languages()
        .iter()
        .map(|language| scan.language_counts.get(language).copied().unwrap_or_default() > 0)
        .collect::<Vec<_>>();
    let selected = MultiSelect::new()
        .with_prompt("Select languages to index")
        .items(&language_items)
        .defaults(&defaults)
        .interact()?;
    let languages =
        selected.into_iter().map(|index| supported_languages()[index]).collect::<Vec<_>>();
    if languages.is_empty() {
        anyhow::bail!("init needs at least one selected language");
    }

    let mut bindings = BTreeMap::new();
    for language in &languages {
        let candidates = candidate_dirs(scan, *language);
        if candidates.is_empty() {
            bindings.insert(*language, vec![PathBuf::from(".")]);
            continue;
        }
        println!();
        println!("Candidate paths for {}:", language.as_str());
        print_candidate_tree(&candidates)?;
        let items = candidates
            .iter()
            .map(|candidate| {
                format!("{} ({} files)", display_rel(&candidate.path), candidate.count)
            })
            .collect::<Vec<_>>();
        let defaults = candidates.iter().map(|candidate| candidate.default).collect::<Vec<_>>();
        let selected = MultiSelect::new()
            .with_prompt(format!("Select {} roots", language.as_str()))
            .items(&items)
            .defaults(&defaults)
            .interact()?;
        let dirs: Vec<_> =
            selected.into_iter().map(|index| candidates[index].path.clone()).collect();
        if dirs.is_empty() {
            anyhow::bail!("init needs at least one selected root for {}", language.as_str());
        }
        bindings.insert(*language, dirs);
    }

    Ok(InitPlan { root_value, languages, bindings })
}

fn default_plan(root_value: String, scan: &RepoScan) -> InitPlan {
    let languages = supported_languages()
        .into_iter()
        .filter(|language| scan.language_counts.get(language).copied().unwrap_or_default() > 0)
        .collect::<Vec<_>>();
    let languages = if languages.is_empty() { vec![Language::Rust] } else { languages };
    let bindings = languages
        .iter()
        .map(|language| {
            let dirs = candidate_dirs(scan, *language)
                .into_iter()
                .filter(|candidate| candidate.default)
                .map(|candidate| candidate.path)
                .collect::<Vec<_>>();
            let dirs = if dirs.is_empty() { vec![PathBuf::from(".")] } else { dirs };
            (*language, dirs)
        })
        .collect();
    InitPlan { root_value, languages, bindings }
}

fn setup_index(config: &Config) -> anyhow::Result<IndexDatabase> {
    eprintln!("init: migrating SQLite schema");
    let migration = IndexDatabase::migrate(&config.database)?;
    if migration.state != rag_rat_core::index::schema::SchemaState::Compatible {
        anyhow::bail!("{}", migration.message);
    }
    eprintln!("init: indexing discovered files");
    IndexDatabase::index_discover_with_progress(config, render_index_progress)
}

fn setup_model_and_reconcile(
    config: &Config,
    db: &IndexDatabase,
    assume_yes: bool,
) -> anyhow::Result<()> {
    let install = assume_yes
        || Confirm::new()
            .with_prompt("Install an embedding model and reconcile vectors now?")
            .default(true)
            .interact()?;
    if !install {
        eprintln!("init: skipped model install and reconcile");
        return Ok(());
    }
    let choices = [FASTEMBED_MODEL_ID, HASH_MODEL_ID];
    let model_index = if assume_yes {
        0
    } else {
        Select::new().with_prompt("Select embedding model").items(&choices).default(0).interact()?
    };
    let model_id = choices[model_index];
    eprintln!("init: installing model {model_id}");
    match db.install_model(model_id) {
        Ok(model) => eprintln!("init: model status {} {}", model.model_id, model.status),
        Err(err) if model_id == FASTEMBED_MODEL_ID => {
            eprintln!("init: FastEmbed install failed: {err}");
            eprintln!("init: falling back to {HASH_MODEL_ID}");
            db.install_model(HASH_MODEL_ID)?;
        },
        Err(err) => return Err(err),
    }
    eprintln!("init: reconciling embeddings");
    db.reconcile_with_options_progress(
        ReconcileOptions {
            limit: None,
            batch_size: Some(config.local_ai.embedding.runtime.batch_size),
            force: false,
            until_clean: true,
            changed_first: true,
            max_seconds: None,
            max_embedding_chars: config.local_ai.embedding.runtime.max_embedding_chars,
        },
        render_reconcile_progress,
    )?;
    Ok(())
}

fn offer_mcp_install(config: &Config, config_path: &Path, assume_yes: bool) -> anyhow::Result<()> {
    let absolute_config = absolute_config_path(config, config_path)?;
    if assume_yes
        || Confirm::new()
            .with_prompt("Install rag-rat MCP for Claude Code?")
            .default(false)
            .interact()?
    {
        install_claude_mcp(&absolute_config)?;
    }
    if assume_yes
        || Confirm::new().with_prompt("Install rag-rat MCP for Codex?").default(false).interact()?
    {
        install_codex_mcp(&absolute_config)?;
    }
    Ok(())
}

fn offer_hooks_install(config: &Config, assume_yes: bool) -> anyhow::Result<()> {
    let install = assume_yes
        || Confirm::new()
            .with_prompt("Install rag-rat git maintenance hooks?")
            .default(false)
            .interact()?;
    if !install {
        return Ok(());
    }
    let git = git_paths(&config.root)?;
    fs::create_dir_all(&git.hooks_dir)?;
    for hook in crate::MANAGED_HOOKS {
        crate::install_hook(&git.hooks_dir, hook)?;
    }
    eprintln!("init: installed hooks in {}", git.hooks_dir.display());
    Ok(())
}

fn install_claude_mcp(config_path: &Path) -> anyhow::Result<()> {
    let exe = current_exe_for_mcp()?;
    let status = Command::new("claude")
        .arg("mcp")
        .arg("add")
        .arg("--scope")
        .arg("project")
        .arg("rag-rat")
        .arg("--")
        .arg(&exe)
        .arg("mcp")
        .arg("--config")
        .arg(config_path)
        .status();
    match status {
        Ok(status) if status.success() => eprintln!("init: installed Claude Code MCP server"),
        Ok(status) => eprintln!("init: claude mcp add exited with status {status}"),
        Err(err) => eprintln!("init: could not run claude mcp add: {err}"),
    }
    Ok(())
}

fn install_codex_mcp(config_path: &Path) -> anyhow::Result<()> {
    let exe = current_exe_for_mcp()?;
    let status = Command::new("codex")
        .arg("mcp")
        .arg("add")
        .arg("rag-rat")
        .arg("--")
        .arg(&exe)
        .arg("mcp")
        .arg("--config")
        .arg(config_path)
        .status();
    match status {
        Ok(status) if status.success() => eprintln!("init: installed Codex MCP server"),
        Ok(status) => {
            eprintln!("init: codex mcp add exited with status {status}");
            print_codex_config_snippet(&exe, config_path);
        },
        Err(err) => {
            eprintln!("init: could not run codex mcp add: {err}");
            print_codex_config_snippet(&exe, config_path);
        },
    }
    Ok(())
}

fn current_exe_for_mcp() -> anyhow::Result<PathBuf> {
    env::current_exe().map_err(Into::into)
}

fn print_codex_config_snippet(exe: &Path, config_path: &Path) {
    eprintln!(
        "Add this to ~/.codex/config.toml if your Codex build does not support `codex mcp add`:"
    );
    eprintln!("[mcp_servers.rag-rat]");
    eprintln!("command = {:?}", exe.display().to_string());
    eprintln!("args = [\"mcp\", \"--config\", {:?}]", config_path.display().to_string());
}

fn absolute_config_path(config: &Config, config_path: &Path) -> anyhow::Result<PathBuf> {
    if config_path.is_absolute() {
        Ok(config_path.to_path_buf())
    } else {
        Ok(config.root.join(config_path).canonicalize()?)
    }
}

fn scan_repo(root: &Path) -> anyhow::Result<RepoScan> {
    let mut scan = RepoScan::default();
    scan_dir(root, root, 0, &mut scan)?;
    Ok(scan)
}

fn scan_dir(root: &Path, dir: &Path, depth: usize, scan: &mut RepoScan) -> anyhow::Result<()> {
    if depth > 10 {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_dir(&entry.file_name().to_string_lossy()) {
                continue;
            }
            scan_dir(root, &path, depth + 1, scan)?;
        } else if file_type.is_file()
            && let Some(language) = Language::from_path(&path)
        {
            *scan.language_counts.entry(language).or_default() += 1;
            add_file_to_dir_counts(root, &path, language, scan)?;
        }
    }
    Ok(())
}

fn add_file_to_dir_counts(
    root: &Path,
    path: &Path,
    language: Language,
    scan: &mut RepoScan,
) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or(root);
    let relative_parent = parent.strip_prefix(root).unwrap_or(parent);
    *scan.dir_counts.entry(language).or_default().entry(PathBuf::from(".")).or_default() += 1;
    let mut current = PathBuf::new();
    for component in relative_parent.components() {
        current.push(component.as_os_str());
        *scan.dir_counts.entry(language).or_default().entry(current.clone()).or_default() += 1;
    }
    Ok(())
}

fn should_skip_dir(name: &str) -> bool {
    SKIPPED_DIRS.contains(&name)
}

#[derive(Debug, Clone)]
struct DirCandidate {
    path: PathBuf,
    count: usize,
    default: bool,
}

fn candidate_dirs(scan: &RepoScan, language: Language) -> Vec<DirCandidate> {
    let Some(counts) = scan.dir_counts.get(&language) else {
        return Vec::new();
    };
    let mut candidates = counts
        .iter()
        .filter(|(path, _)| path_depth(path) <= 4)
        .map(|(path, count)| DirCandidate {
            path: path.clone(),
            count: *count,
            default: default_dir(language, path),
        })
        .collect::<Vec<_>>();
    if !candidates.iter().any(|candidate| candidate.default)
        && let Some(best) = candidates.iter_mut().max_by_key(|candidate| candidate.count)
    {
        best.default = true;
    }
    candidates.sort_by(|a, b| {
        b.default
            .cmp(&a.default)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.path.cmp(&b.path))
    });
    candidates.truncate(32);
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates
}

fn default_dir(language: Language, path: &Path) -> bool {
    let text = display_rel(path);
    match language {
        Language::Rust => text == "src" || text.ends_with("/src"),
        Language::TypeScript => text == "src" || text.ends_with("/src") || text.ends_with("/app"),
        Language::Kotlin => {
            text == "src"
                || text.ends_with("/src")
                || text.ends_with("/src/main/java")
                || text.ends_with("/src/main/kotlin")
        },
        Language::Markdown => text == "docs" || text == ".",
    }
}

fn path_depth(path: &Path) -> usize {
    if path == Path::new(".") { 0 } else { path.components().count() }
}

fn print_language_tree(scan: &RepoScan) -> anyhow::Result<()> {
    let candidates = supported_languages()
        .into_iter()
        .flat_map(|language| candidate_dirs(scan, language))
        .collect::<Vec<_>>();
    print_candidate_tree(&candidates)
}

fn print_candidate_tree(candidates: &[DirCandidate]) -> anyhow::Result<()> {
    let mut root = TreeNode::default();
    for candidate in candidates {
        insert_tree_path(&mut root, &candidate.path);
    }
    print!("{}", build_tree(".".to_string(), &root));
    Ok(())
}

fn insert_tree_path(root: &mut TreeNode, path: &Path) {
    if path == Path::new(".") {
        return;
    }
    let mut node = root;
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy().to_string();
        node = node.children.entry(name).or_default();
    }
}

fn build_tree(name: String, node: &TreeNode) -> Tree<String> {
    let mut tree = Tree::new(name);
    for (name, child) in &node.children {
        tree.push(build_tree(name.clone(), child));
    }
    tree
}

fn render_config(plan: &InitPlan) -> String {
    let mut text = String::new();
    text.push_str("[index]\n");
    text.push_str(&format!("root = {}\n", toml_string(&plan.root_value)));
    text.push_str(&format!("database = {}\n\n", toml_string(DEFAULT_DATABASE)));
    text.push_str("[local_ai.embedding.runtime]\n");
    text.push_str("batch_size = 64\n");
    text.push_str("ort_threads = 4\n");
    text.push_str("omp_threads = 1\n");
    text.push_str("max_embedding_chars = 4000\n\n");
    text.push_str("[target_bindings]\n");
    for language in &plan.languages {
        let dirs = plan.bindings.get(language).cloned().unwrap_or_default();
        text.push_str(&format!("{} = [{}]\n", language.as_str(), quoted_paths(&dirs)));
    }
    text
}

fn quoted_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|path| toml_string(&display_rel(path))).collect::<Vec<_>>().join(", ")
}

fn config_root_value(root: &Path, config_path: &Path) -> String {
    let Some(parent) = config_path.parent().filter(|path| !path.as_os_str().is_empty()) else {
        return ".".to_string();
    };
    if config_path.is_absolute() {
        absolute_config_root_value(root, parent)
    } else {
        relative_config_root_value(parent)
    }
}

fn absolute_config_root_value(root: &Path, parent: &Path) -> String {
    if let Ok(relative_parent) = parent.strip_prefix(root) {
        return relative_config_root_value(relative_parent);
    }
    root.display().to_string()
}

fn relative_config_root_value(parent: &Path) -> String {
    let depth = parent.components().filter(normal_component).count();
    if depth == 0 {
        ".".to_string()
    } else {
        std::iter::repeat_n("..", depth).collect::<Vec<_>>().join("/")
    }
}

fn normal_component(component: &std::path::Component<'_>) -> bool {
    matches!(component, std::path::Component::Normal(_))
}

fn toml_string(value: &str) -> String {
    format!("{value:?}")
}

fn display_rel(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.is_empty() { ".".to_string() } else { text }
}

fn supported_languages() -> Vec<Language> {
    vec![Language::Rust, Language::TypeScript, Language::Kotlin, Language::Markdown]
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|window| window[0] == name).map(|window| window[1].clone())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_config_uses_selected_language_bindings() {
        let plan = InitPlan {
            root_value: ".".to_string(),
            languages: vec![Language::Rust, Language::TypeScript],
            bindings: BTreeMap::from([
                (Language::Rust, vec![PathBuf::from("crates/app/src")]),
                (Language::TypeScript, vec![PathBuf::from("web/src"), PathBuf::from("app/src")]),
            ]),
        };

        let text = render_config(&plan);

        assert!(text.contains("[index]"));
        assert!(text.contains("database = \".rag-rat/index.sqlite\""));
        assert!(text.contains("rust = [\"crates/app/src\"]"));
        assert!(text.contains("typescript = [\"web/src\", \"app/src\"]"));
    }

    #[test]
    fn default_plan_selects_detected_src_dirs() {
        let scan = RepoScan {
            language_counts: BTreeMap::from([(Language::Rust, 2), (Language::Markdown, 1)]),
            dir_counts: BTreeMap::from([
                (
                    Language::Rust,
                    BTreeMap::from([(PathBuf::from("."), 2), (PathBuf::from("src"), 2)]),
                ),
                (
                    Language::Markdown,
                    BTreeMap::from([(PathBuf::from("."), 1), (PathBuf::from("docs"), 1)]),
                ),
            ]),
        };

        let plan = default_plan(".".to_string(), &scan);

        assert_eq!(plan.languages, vec![Language::Rust, Language::Markdown]);
        assert_eq!(plan.bindings[&Language::Rust], vec![PathBuf::from("src")]);
        assert_eq!(
            plan.bindings[&Language::Markdown],
            vec![PathBuf::from("."), PathBuf::from("docs")]
        );
    }

    #[test]
    fn nested_config_uses_repo_root_relative_to_config_dir() {
        assert_eq!(config_root_value(Path::new("/repo"), Path::new("profiles/rag-rat.toml")), "..");
        assert_eq!(
            config_root_value(Path::new("/repo"), Path::new("profiles/dev/rag-rat.toml")),
            "../.."
        );
        assert_eq!(config_root_value(Path::new("/repo"), Path::new("rag-rat.toml")), ".");
    }
}
