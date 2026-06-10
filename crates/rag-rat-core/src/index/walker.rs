use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ResolvedTarget;

pub fn walk_target(root: &Path, target: &ResolvedTarget) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for directory in &target.directories {
        walk_dir(root, &root.join(directory), target, &mut files)?;
    }
    Ok(files.into_iter().collect())
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    target: &ResolvedTarget,
    files: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if should_skip_name(&file_name) {
            continue;
        }
        if file_type.is_dir() {
            walk_dir(root, &path, target, files)?;
        } else if file_type.is_file() && is_target_file(root, &path, target) {
            files.insert(path);
        }
    }
    Ok(())
}

fn should_skip_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".rag-rat"
            | ".omx"
            | ".omc"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "coverage"
    )
}

fn is_target_file(root: &Path, path: &Path, target: &ResolvedTarget) -> bool {
    let Some(language) = crate::language::Language::from_path(path) else {
        return false;
    };
    if language != target.language {
        return false;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    let relative = relative.to_string_lossy().replace('\\', "/");
    if target.exclude.iter().any(|pattern| matches_simple_pattern(&relative, pattern)) {
        return false;
    }
    target.include.iter().any(|pattern| matches_simple_pattern(&relative, pattern))
}

fn matches_simple_pattern(path: &str, pattern: &str) -> bool {
    if let Some(extension) = pattern.strip_prefix("**/*.") {
        return path.ends_with(&format!(".{extension}"));
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }
    path == pattern || path.contains(pattern.trim_matches('*'))
}
