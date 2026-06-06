use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::Deserialize;
use thiserror::Error;

use crate::language::{Language, LanguageError};

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub database: PathBuf,
    pub targets: Vec<ResolvedTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub name: String,
    pub language: Language,
    pub directories: Vec<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub kind: TargetKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Source,
    Generated,
    Docs,
    Tests,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Generated => "generated",
            Self::Docs => "docs",
            Self::Tests => "tests",
        }
    }
}

impl FromStr for TargetKind {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "source" => Ok(Self::Source),
            "generated" => Ok(Self::Generated),
            "docs" => Ok(Self::Docs),
            "tests" | "test" => Ok(Self::Tests),
            other => Err(ConfigError::UnknownTargetKind(other.to_string())),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let root = config_dir.join(raw.index.root.unwrap_or_else(|| ".".to_string()));
        let root = normalize_existing_dir(&root)?;
        let database =
            root.join(raw.index.database.unwrap_or_else(|| ".rag-rat/index.sqlite".to_string()));
        let targets = resolve_targets(&root, raw.target_bindings, raw.target)?;

        Ok(Self { root, database, targets })
    }
}

fn resolve_targets(
    root: &Path,
    simple: BTreeMap<String, Vec<String>>,
    expanded: Vec<RawTarget>,
) -> Result<Vec<ResolvedTarget>, ConfigError> {
    let mut names = BTreeSet::new();
    let mut targets = Vec::new();

    for (language_name, directories) in simple {
        let language = Language::from_str(&language_name)?;
        let kind =
            if language == Language::Markdown { TargetKind::Docs } else { TargetKind::Source };
        let name = language.as_str().to_string();
        push_target(
            root,
            &mut names,
            &mut targets,
            ResolvedTarget {
                include: language
                    .simple_extensions()
                    .iter()
                    .map(|ext| format!("**/*.{ext}"))
                    .collect(),
                exclude: Vec::new(),
                name,
                language,
                directories: directories.into_iter().map(PathBuf::from).collect(),
                kind,
            },
        )?;
    }

    for target in expanded {
        let language = Language::from_str(&target.language)?;
        let kind = target
            .kind
            .as_deref()
            .map(TargetKind::from_str)
            .transpose()?
            .unwrap_or(TargetKind::Source);
        push_target(
            root,
            &mut names,
            &mut targets,
            ResolvedTarget {
                name: target.name,
                language,
                directories: target.directories.into_iter().map(PathBuf::from).collect(),
                include: target.include.unwrap_or_else(|| {
                    language.simple_extensions().iter().map(|ext| format!("**/*.{ext}")).collect()
                }),
                exclude: target.exclude.unwrap_or_default(),
                kind,
            },
        )?;
    }

    Ok(targets)
}

fn push_target(
    root: &Path,
    names: &mut BTreeSet<String>,
    targets: &mut Vec<ResolvedTarget>,
    target: ResolvedTarget,
) -> Result<(), ConfigError> {
    if !names.insert(target.name.clone()) {
        return Err(ConfigError::DuplicateTarget(target.name));
    }
    for directory in &target.directories {
        let full_path = root.join(directory);
        if !full_path.is_dir() {
            return Err(ConfigError::MissingDirectory(directory.clone()));
        }
    }
    targets.push(target);
    Ok(())
}

fn normalize_existing_dir(path: &Path) -> Result<PathBuf, ConfigError> {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let canonical = absolute.canonicalize()?;
    if !canonical.is_dir() {
        return Err(ConfigError::MissingDirectory(canonical));
    }
    Ok(canonical)
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    index: RawIndex,
    #[serde(default)]
    target_bindings: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "target")]
    target: Vec<RawTarget>,
}

#[derive(Debug, Default, Deserialize)]
struct RawIndex {
    root: Option<String>,
    database: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    name: String,
    language: String,
    directories: Vec<String>,
    kind: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Language(#[from] LanguageError),
    #[error("unknown target kind `{0}`")]
    UnknownTargetKind(String),
    #[error("duplicate target name `{0}`")]
    DuplicateTarget(String),
    #[error("configured directory does not exist: {0}")]
    MissingDirectory(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_and_expanded_targets() {
        let root = std::env::current_dir().unwrap();
        let simple = BTreeMap::from([("rust".to_string(), vec![".".to_string()])]);
        let expanded = vec![RawTarget {
            name: "generated-ts".to_string(),
            language: "typescript".to_string(),
            directories: vec![".".to_string()],
            kind: Some("generated".to_string()),
            include: Some(vec!["**/*.ts".to_string()]),
            exclude: Some(vec!["**/*.map".to_string()]),
        }];

        let targets = resolve_targets(&root, simple, expanded).unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].language, Language::Rust);
        assert_eq!(targets[1].kind, TargetKind::Generated);
    }

    #[test]
    fn rejects_unknown_language() {
        let root = std::env::current_dir().unwrap();
        let simple = BTreeMap::from([("python".to_string(), vec![".".to_string()])]);

        let err = resolve_targets(&root, simple, Vec::new()).unwrap_err();

        assert!(err.to_string().contains("unknown language"));
    }
}
