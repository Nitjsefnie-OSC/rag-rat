use std::path::Path;

use crate::{index::parser, language::Language};

#[test]
fn extracts_rust_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/lib.rs");
    let symbols = parser::parse_symbols(Path::new("src/lib.rs"), Language::Rust, text).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.kind == "function"));
    assert!(symbols.iter().any(|symbol| symbol.kind == "struct"));
}

#[test]
fn extracts_typescript_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/index.ts");
    let symbols =
        parser::parse_symbols(Path::new("src/index.ts"), Language::TypeScript, text).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.kind == "function"));
    assert!(symbols.iter().any(|symbol| symbol.kind == "const" || symbol.kind == "type"));
}

#[test]
fn extracts_kotlin_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/Main.kt");
    let symbols = parser::parse_symbols(Path::new("src/Main.kt"), Language::Kotlin, text).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.kind == "class" || symbol.kind == "object"));
    assert!(symbols.iter().any(|symbol| symbol.kind == "function"));
}
