use std::path::Path;

use tree_sitter::Node;

use crate::language::Language;

#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
    pub docs: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserKind {
    Rust,
    TypeScript,
    Tsx,
    Kotlin,
    Markdown,
}

pub fn parser_kind(path: &Path, language: Language) -> ParserKind {
    match language {
        Language::Rust => ParserKind::Rust,
        Language::TypeScript => {
            if path.extension().and_then(|ext| ext.to_str()) == Some("tsx") {
                ParserKind::Tsx
            } else {
                ParserKind::TypeScript
            }
        },
        Language::Kotlin => ParserKind::Kotlin,
        Language::Markdown => ParserKind::Markdown,
    }
}

pub fn parse_symbols(
    path: &Path,
    language: Language,
    text: &str,
) -> anyhow::Result<Vec<ParsedSymbol>> {
    match parser_kind(path, language) {
        ParserKind::Rust => parse_tree_sitter(path, language, text, tree_sitter_rust::language()),
        ParserKind::TypeScript => {
            parse_tree_sitter(path, language, text, tree_sitter_typescript::language_typescript())
        },
        ParserKind::Tsx => {
            parse_tree_sitter(path, language, text, tree_sitter_typescript::language_tsx())
        },
        ParserKind::Kotlin => parse_kotlin(path, text),
        ParserKind::Markdown => Ok(Vec::new()),
    }
}

pub fn parse_error(path: &Path, language: Language, text: &str) -> anyhow::Result<Option<String>> {
    let grammar = match parser_kind(path, language) {
        ParserKind::Rust => tree_sitter_rust::language(),
        ParserKind::TypeScript => tree_sitter_typescript::language_typescript(),
        ParserKind::Tsx => tree_sitter_typescript::language_tsx(),
        ParserKind::Kotlin => tree_sitter_kotlin::language(),
        ParserKind::Markdown => return Ok(None),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let tree =
        parser.parse(text, None).ok_or_else(|| anyhow::anyhow!("tree-sitter parse failed"))?;
    Ok(tree.root_node().has_error().then(|| {
        "tree-sitter parse produced error nodes; partial structural index was retained".to_string()
    }))
}

fn parse_tree_sitter(
    path: &Path,
    language: Language,
    text: &str,
    grammar: tree_sitter::Language,
) -> anyhow::Result<Vec<ParsedSymbol>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let tree =
        parser.parse(text, None).ok_or_else(|| anyhow::anyhow!("tree-sitter parse failed"))?;
    let mut out = Vec::new();
    collect_symbols(path, language, text, tree.root_node(), &mut out);
    out.sort_by_key(|symbol| (symbol.start_byte, symbol.end_byte));
    out.dedup_by_key(|symbol| (symbol.start_byte, symbol.end_byte, symbol.name.clone()));
    Ok(out)
}

fn parse_kotlin(path: &Path, text: &str) -> anyhow::Result<Vec<ParsedSymbol>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_kotlin::language())?;
    let tree =
        parser.parse(text, None).ok_or_else(|| anyhow::anyhow!("tree-sitter parse failed"))?;
    let mut out = Vec::new();
    collect_kotlin_symbols(path, text, tree.root_node(), &mut out);
    out.sort_by_key(|symbol| (symbol.start_byte, symbol.end_byte));
    out.dedup_by_key(|symbol| (symbol.start_byte, symbol.end_byte, symbol.name.clone()));
    Ok(out)
}

fn collect_symbols(
    path: &Path,
    language: Language,
    text: &str,
    node: Node<'_>,
    out: &mut Vec<ParsedSymbol>,
) {
    if node.is_error() || node.is_missing() {
        return;
    }
    if let Some((kind, name_node)) = symbol_node(language, node) {
        let name = node_text(name_node, text).unwrap_or_default();
        if !name.is_empty() {
            out.push(make_symbol(path, text, kind, name, node.start_byte(), node.end_byte()));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbols(path, language, text, child, out);
    }
}

fn collect_kotlin_symbols(
    path: &Path,
    text: &str,
    node: tree_sitter::Node<'_>,
    out: &mut Vec<ParsedSymbol>,
) {
    if node.is_error() || node.is_missing() {
        return;
    }
    if let Some((kind, name)) = kotlin_symbol_node(node, text) {
        out.push(make_symbol(path, text, kind, name, node.start_byte(), node.end_byte()));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_kotlin_symbols(path, text, child, out);
    }
}

fn symbol_node(language: Language, node: Node<'_>) -> Option<(&'static str, Node<'_>)> {
    let kind = node.kind();
    match language {
        Language::Rust => match kind {
            "function_item" => Some(("function", child_name(node)?)),
            "struct_item" => Some(("struct", child_name(node)?)),
            "enum_item" => Some(("enum", child_name(node)?)),
            "trait_item" => Some(("trait", child_name(node)?)),
            "impl_item" => Some(("impl", impl_name(node).unwrap_or(node))),
            "mod_item" => Some(("module", child_name(node)?)),
            "const_item" => Some(("const", child_name(node)?)),
            "static_item" => Some(("static", child_name(node)?)),
            "type_item" => Some(("type", child_name(node)?)),
            "macro_definition" => Some(("macro", child_name(node)?)),
            _ => None,
        },
        Language::TypeScript => match kind {
            "function_declaration" | "method_definition" | "generator_function_declaration" => {
                Some(("function", child_name(node)?))
            },
            "class_declaration" => Some(("class", child_name(node)?)),
            "interface_declaration" => Some(("interface", child_name(node)?)),
            "type_alias_declaration" => Some(("type", child_name(node)?)),
            "lexical_declaration" | "variable_declarator" | "public_field_definition" => {
                Some(("const", child_name(node)?))
            },
            _ => None,
        },
        Language::Kotlin | Language::Markdown => None,
    }
}

fn kotlin_symbol_node(node: tree_sitter::Node<'_>, text: &str) -> Option<(&'static str, String)> {
    let kind = node.kind();
    let symbol_kind = match kind {
        "class_declaration" => "class",
        "object_declaration" => "object",
        "function_declaration" => "function",
        "property_declaration" => "property",
        "companion_object" | "companion_object_declaration" => "object",
        _ => return None,
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "simple_identifier" | "type_identifier") {
            return Some((symbol_kind, child.utf8_text(text.as_bytes()).ok()?.to_string()));
        }
    }
    if kind == "property_declaration" {
        let name = first_descendant_text(node, text, &["simple_identifier"])?;
        return Some((symbol_kind, name));
    }
    matches!(kind, "companion_object" | "companion_object_declaration")
        .then(|| (symbol_kind, "companion".to_string()))
}

fn first_descendant_text(node: Node<'_>, text: &str, kinds: &[&str]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return child.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned);
        }
        if let Some(value) = first_descendant_text(child, text, kinds) {
            return Some(value);
        }
    }
    None
}

fn child_name(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("name").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "identifier" | "type_identifier" | "property_identifier" | "field_identifier"
            )
        })
    })
}

fn impl_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier")
    })
}

fn make_symbol(
    path: &Path,
    text: &str,
    kind: &str,
    name: String,
    start_byte: usize,
    end_byte: usize,
) -> ParsedSymbol {
    let start_line = byte_to_line(text, start_byte);
    let end_line = byte_to_line(text, end_byte);
    ParsedSymbol {
        qualified_name: format!("{}::{name}", path.to_string_lossy().replace('\\', "/")),
        name,
        kind: kind.to_string(),
        start_byte,
        end_byte,
        start_line,
        end_line,
        signature: signature_for(text, start_byte, end_byte),
        docs: docs_before(text, start_byte),
    }
}

fn node_text(node: Node<'_>, text: &str) -> Option<String> {
    node.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned)
}

fn byte_to_line(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn signature_for(text: &str, start_byte: usize, end_byte: usize) -> Option<String> {
    text.get(start_byte..end_byte)?
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

fn docs_before(text: &str, start_byte: usize) -> Option<String> {
    let before = text.get(..start_byte)?;
    let mut docs = Vec::new();
    for line in before.lines().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with("///") {
            docs.push(trimmed.trim_start_matches('/').trim().to_string());
        } else if trimmed.starts_with("*") || trimmed.starts_with("/**") {
            docs.push(trimmed.trim_start_matches('/').trim_start_matches('*').trim().to_string());
        } else if trimmed.is_empty() {
            continue;
        } else {
            break;
        }
    }
    docs.reverse();
    (!docs.is_empty()).then(|| docs.join("\n"))
}
