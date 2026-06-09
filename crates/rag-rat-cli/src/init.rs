use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use dialoguer::{Confirm, MultiSelect, Select};
use rag_rat_core::{
    Config, IndexDatabase,
    config::EmbeddingBackend,
    index::ai::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, MODEL2VEC_MODEL_ID, ReconcileOptions},
    language::Language,
};

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
    backend: EmbeddingBackend,
}

#[derive(Debug, Default)]
struct RepoScan {
    language_counts: BTreeMap<Language, usize>,
    dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
    direct_dir_counts: BTreeMap<Language, BTreeMap<PathBuf, usize>>,
    total_source_bytes: u64,
}

/// Very rough chunk-count estimate from total indexable source bytes (~500 chars per chunk after
/// policy skips). Used only to *recommend* an embedding backend at init time.
fn estimated_chunks(total_source_bytes: u64) -> u64 {
    total_source_bytes / 500
}

/// Recommend an embedding backend by repo scale. The FastEmbed (MiniLM) cold backfill is CPU-bound
/// at ~10-100 chunks/sec, so it's only comfortable for repos that finish in a few minutes; larger
/// repos default to the static Model2Vec backend (orders of magnitude faster, some quality cost).
fn recommend_backend(estimated_chunks: u64) -> EmbeddingBackend {
    if estimated_chunks <= 5_000 {
        EmbeddingBackend::FastEmbed
    } else {
        EmbeddingBackend::Model2Vec
    }
}

fn backend_label(backend: EmbeddingBackend) -> &'static str {
    match backend {
        EmbeddingBackend::FastEmbed => {
            "minilm — MiniLM transformer; best quality, CPU backfill ~10-100 chunks/sec"
        },
        EmbeddingBackend::Model2Vec => {
            "model2vec — static embeddings; ~100-500x faster on CPU, some quality cost"
        },
        EmbeddingBackend::None => "none — BM25 + structure only, no dense vectors",
    }
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    let options = InitOptions::from_args(args)?;
    let _terminal_reset = TerminalResetGuard::install_if_interactive(!options.yes)?;
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
    println!("Detected languages:");
    print_language_summary(scan);
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

    let backend = prompt_backend(scan)?;
    Ok(InitPlan { root_value, languages, bindings, backend })
}

fn prompt_backend(scan: &RepoScan) -> anyhow::Result<EmbeddingBackend> {
    let estimate = estimated_chunks(scan.total_source_bytes);
    let recommended = recommend_backend(estimate);
    println!();
    println!(
        "Embedding backend (≈{estimate} chunks from {} of source):",
        human_bytes(scan.total_source_bytes)
    );
    println!("  recommended: {}", backend_label(recommended));
    let choices = [EmbeddingBackend::FastEmbed, EmbeddingBackend::Model2Vec, EmbeddingBackend::None];
    let default_index = choices.iter().position(|backend| *backend == recommended).unwrap_or(0);
    let items = choices.iter().map(|backend| backend_label(*backend)).collect::<Vec<_>>();
    let selected = Select::new()
        .with_prompt("Select embedding backend")
        .items(&items)
        .default(default_index)
        .interact()?;
    Ok(choices[selected])
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
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
    let backend = recommend_backend(estimated_chunks(scan.total_source_bytes));
    InitPlan { root_value, languages, bindings, backend }
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
    let backend = config.local_ai.embedding.backend;
    let Some(model_id) = backend.model_id() else {
        eprintln!(
            "init: embeddings disabled (model = \"none\") — structural + BM25 search only, no \
             vector backfill"
        );
        return Ok(());
    };
    let install = assume_yes
        || Confirm::new()
            .with_prompt(format!("Install the {} embedding model and reconcile vectors now?", backend.as_str()))
            .default(true)
            .interact()?;
    if !install {
        eprintln!("init: skipped model install and reconcile");
        return Ok(());
    }
    eprintln!("init: installing model {model_id}");
    match db.install_model(model_id) {
        Ok(model) => eprintln!("init: model status {} {}", model.model_id, model.status),
        Err(err) if model_id == FASTEMBED_MODEL_ID || model_id == MODEL2VEC_MODEL_ID => {
            eprintln!("init: {} install failed: {err}", backend.as_str());
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
            intra_threads: config.local_ai.embedding.runtime.ort_threads.map(|n| n as usize),
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
            scan.total_source_bytes +=
                entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
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
    *scan
        .direct_dir_counts
        .entry(language)
        .or_default()
        .entry(relative_parent.to_path_buf())
        .or_default() += 1;
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
            default: default_dir(scan, language, path),
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

fn default_dir(scan: &RepoScan, language: Language, path: &Path) -> bool {
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
        Language::C | Language::Cpp => {
            text == "src"
                || text.ends_with("/src")
                || text == "include"
                || text.ends_with("/include")
                || directly_contains_source(scan, language, path)
        },
        Language::Markdown => text == "docs" || text == ".",
    }
}

fn directly_contains_source(scan: &RepoScan, language: Language, path: &Path) -> bool {
    path != Path::new(".")
        && scan
            .direct_dir_counts
            .get(&language)
            .and_then(|counts| counts.get(path))
            .copied()
            .unwrap_or_default()
            > 0
}

fn path_depth(path: &Path) -> usize {
    if path == Path::new(".") { 0 } else { path.components().count() }
}

fn print_language_summary(scan: &RepoScan) {
    for language in supported_languages() {
        let count = scan.language_counts.get(&language).copied().unwrap_or_default();
        if count > 0 {
            println!("  {}: {count} files", language.as_str());
        }
    }
}

fn render_config(plan: &InitPlan) -> String {
    let mut text = String::new();
    text.push_str("[index]\n");
    text.push_str(&format!("root = {}\n", toml_string(&plan.root_value)));
    text.push_str(&format!("database = {}\n\n", toml_string(DEFAULT_DATABASE)));
    text.push_str("[local_ai.embedding]\n");
    text.push_str(&format!("# {}\n", backend_label(plan.backend)));
    text.push_str(&format!("model = {}\n\n", toml_string(plan.backend.as_str())));
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
    Language::all().to_vec()
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|window| window[0] == name).map(|window| window[1].clone())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

#[cfg(unix)]
struct TerminalResetGuard {
    fd: libc::c_int,
    handlers: Vec<(libc::c_int, libc::sighandler_t)>,
}

#[cfg(unix)]
impl TerminalResetGuard {
    fn install_if_interactive(interactive: bool) -> anyhow::Result<Option<Self>> {
        if !interactive {
            return Ok(None);
        }
        match Self::install() {
            Ok(guard) => Ok(Some(guard)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn install() -> io::Result<Self> {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        c_result(|| unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) })?;
        let termios = unsafe { termios.assume_init() };
        unsafe {
            std::ptr::addr_of_mut!(ORIGINAL_TERMIOS).write(std::mem::MaybeUninit::new(termios));
        }
        TERMINAL_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
        ORIGINAL_TERMIOS_SET.store(true, std::sync::atomic::Ordering::SeqCst);

        let handlers = install_signal_handlers()?;
        Ok(Self { fd: tty.into_raw_fd(), handlers })
    }
}

#[cfg(unix)]
impl Drop for TerminalResetGuard {
    fn drop(&mut self) {
        restore_terminal();
        for (signal, previous) in &self.handlers {
            unsafe {
                libc::signal(*signal, *previous);
            }
        }
        TERMINAL_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
        ORIGINAL_TERMIOS_SET.store(false, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(unix)]
static TERMINAL_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static ORIGINAL_TERMIOS_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(unix)]
static mut ORIGINAL_TERMIOS: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();

#[cfg(unix)]
fn install_signal_handlers() -> io::Result<Vec<(libc::c_int, libc::sighandler_t)>> {
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .map(|signal| {
            let previous = unsafe {
                libc::signal(signal, handle_terminal_signal as *const () as libc::sighandler_t)
            };
            if previous == libc::SIG_ERR {
                Err(io::Error::last_os_error())
            } else {
                Ok((signal, previous))
            }
        })
        .collect()
}

#[cfg(unix)]
extern "C" fn handle_terminal_signal(signal: libc::c_int) {
    restore_terminal();
    let reset = b"\x1b[0m\x1b[?25h\r\n";
    let fd = TERMINAL_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::write(fd, reset.as_ptr().cast(), reset.len());
        }
    }
    unsafe {
        libc::_exit(128 + signal);
    }
}

#[cfg(unix)]
fn restore_terminal() {
    if !ORIGINAL_TERMIOS_SET.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let fd = TERMINAL_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    unsafe {
        libc::tcsetattr(
            fd,
            libc::TCSANOW,
            std::ptr::addr_of!(ORIGINAL_TERMIOS).cast::<libc::termios>(),
        );
    }
}

#[cfg(unix)]
fn c_result<F: FnOnce() -> libc::c_int>(f: F) -> io::Result<()> {
    let status = f();
    if status == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(unix))]
struct TerminalResetGuard;

#[cfg(not(unix))]
impl TerminalResetGuard {
    fn install_if_interactive(_interactive: bool) -> anyhow::Result<Option<Self>> {
        Ok(None)
    }
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
            backend: EmbeddingBackend::Model2Vec,
        };

        let text = render_config(&plan);

        assert!(text.contains("[index]"));
        assert!(text.contains("database = \".rag-rat/index.sqlite\""));
        assert!(text.contains("rust = [\"crates/app/src\"]"));
        assert!(text.contains("typescript = [\"web/src\", \"app/src\"]"));
        assert!(text.contains("[local_ai.embedding]"));
        assert!(text.contains("model = \"model2vec\""));
    }

    #[test]
    fn recommend_backend_scales_with_repo_size() {
        assert_eq!(recommend_backend(estimated_chunks(500_000)), EmbeddingBackend::FastEmbed);
        assert_eq!(recommend_backend(estimated_chunks(50_000_000)), EmbeddingBackend::Model2Vec);
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
            direct_dir_counts: BTreeMap::new(),
            total_source_bytes: 0,
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
    fn c_defaults_include_direct_source_feature_dirs() {
        let scan = RepoScan {
            language_counts: BTreeMap::from([(Language::C, 10)]),
            dir_counts: BTreeMap::from([(
                Language::C,
                BTreeMap::from([
                    (PathBuf::from("."), 10),
                    (PathBuf::from("drivers"), 1),
                    (PathBuf::from("drivers/entropy"), 1),
                    (PathBuf::from("samples"), 9),
                    (PathBuf::from("samples/simple_txrx"), 9),
                    (PathBuf::from("samples/simple_txrx/src"), 9),
                ]),
            )]),
            direct_dir_counts: BTreeMap::from([(
                Language::C,
                BTreeMap::from([
                    (PathBuf::from("drivers/entropy"), 1),
                    (PathBuf::from("samples/simple_txrx/src"), 1),
                ]),
            )]),
            total_source_bytes: 0,
        };

        let defaults = candidate_dirs(&scan, Language::C)
            .into_iter()
            .filter(|candidate| candidate.default)
            .map(|candidate| candidate.path)
            .collect::<Vec<_>>();

        assert!(defaults.contains(&PathBuf::from("drivers/entropy")));
        assert!(defaults.contains(&PathBuf::from("samples/simple_txrx/src")));
        assert!(!defaults.contains(&PathBuf::from("drivers")));
        assert!(!defaults.contains(&PathBuf::from(".")));
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
