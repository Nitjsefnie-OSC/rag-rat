use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    TypeScript,
    Kotlin,
    C,
    Cpp,
    Markdown,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Kotlin => "kotlin",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Markdown => "markdown",
        }
    }

    pub fn simple_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::TypeScript => &["ts", "tsx"],
            Self::Kotlin => &["kt", "kts"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"],
            Self::Markdown => &["md", "markdown"],
        }
    }

    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        [Self::Rust, Self::TypeScript, Self::Kotlin, Self::C, Self::Cpp, Self::Markdown]
            .into_iter()
            .find(|language| language.simple_extensions().contains(&ext))
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Language {
    type Err = LanguageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Ok(Self::Rust),
            "typescript" | "ts" | "tsx" => Ok(Self::TypeScript),
            "kotlin" | "kt" => Ok(Self::Kotlin),
            "c" => Ok(Self::C),
            "cpp" | "c++" | "cc" | "cxx" => Ok(Self::Cpp),
            "markdown" | "md" => Ok(Self::Markdown),
            other => Err(LanguageError::Unknown(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum LanguageError {
    #[error("unknown language `{0}`")]
    Unknown(String),
}
