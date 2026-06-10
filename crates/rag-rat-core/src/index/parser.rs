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
    pub facts: Vec<ParsedSymbolFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbolFact {
    pub kind: String,
    pub value: String,
}

const NAME_KINDS: &[&str] = &[
    "identifier",
    "type_identifier",
    "property_identifier",
    "field_identifier",
    "simple_identifier",
    "namespace_identifier",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserKind {
    Rust,
    TypeScript,
    Tsx,
    Kotlin,
    C,
    Cpp,
    Markdown,
}

pub fn parser_kind(path: &Path, language: Language) -> ParserKind {
    match language {
        Language::Rust => ParserKind::Rust,
        Language::TypeScript =>
            if path.extension().and_then(|ext| ext.to_str()) == Some("tsx") {
                ParserKind::Tsx
            } else {
                ParserKind::TypeScript
            },
        Language::Kotlin => ParserKind::Kotlin,
        Language::C => ParserKind::C,
        Language::Cpp => ParserKind::Cpp,
        Language::Markdown => ParserKind::Markdown,
    }
}

pub fn parse_symbols(
    path: &Path,
    language: Language,
    text: &str,
) -> anyhow::Result<Vec<ParsedSymbol>> {
    match parser_kind(path, language) {
        ParserKind::Rust =>
            parse_tree_sitter(path, language, text, tree_sitter_rust::LANGUAGE.into()),
        ParserKind::TypeScript => parse_tree_sitter(
            path,
            language,
            text,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        ParserKind::Tsx =>
            parse_tree_sitter(path, language, text, tree_sitter_typescript::LANGUAGE_TSX.into()),
        ParserKind::Kotlin =>
            parse_tree_sitter(path, language, text, tree_sitter_kotlin::LANGUAGE.into()),
        ParserKind::C => parse_tree_sitter(path, language, text, tree_sitter_c::LANGUAGE.into()),
        ParserKind::Cpp =>
            parse_tree_sitter(path, language, text, tree_sitter_cpp::LANGUAGE.into()),
        ParserKind::Markdown => Ok(Vec::new()),
    }
}

pub fn parse_error(path: &Path, language: Language, text: &str) -> anyhow::Result<Option<String>> {
    let grammar = match parser_kind(path, language) {
        ParserKind::Rust => tree_sitter_rust::LANGUAGE.into(),
        ParserKind::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ParserKind::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        ParserKind::Kotlin => tree_sitter_kotlin::LANGUAGE.into(),
        ParserKind::C => tree_sitter_c::LANGUAGE.into(),
        ParserKind::Cpp => tree_sitter_cpp::LANGUAGE.into(),
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
            out.push(make_symbol(path, language, text, node, kind, name));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbols(path, language, text, child, out);
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
            "function_declaration" | "method_definition" | "generator_function_declaration" =>
                Some(("function", child_name(node)?)),
            "class_declaration" => Some(("class", child_name(node)?)),
            "interface_declaration" => Some(("interface", child_name(node)?)),
            "type_alias_declaration" => Some(("type", child_name(node)?)),
            "variable_declarator" | "public_field_definition" => Some(("const", child_name(node)?)),
            _ => None,
        },
        Language::Kotlin => match kind {
            "class_declaration" => Some(("class", child_name(node)?)),
            "object_declaration" => Some(("object", child_name(node)?)),
            "function_declaration" => Some(("function", child_name(node)?)),
            "property_declaration" => Some(("property", kotlin_property_name(node)?)),
            "companion_object" | "companion_object_declaration" =>
                Some(("object", companion_name(node).unwrap_or(node))),
            _ => None,
        },
        Language::C => match kind {
            "function_definition" =>
                Some(("function", function_name(node).or_else(|| child_name(node))?)),
            "declaration" if has_descendant_kind(node, "function_declarator") =>
                Some(("function", function_name(node).or_else(|| child_name(node))?)),
            "struct_specifier" => Some(("struct", child_name(node)?)),
            "union_specifier" => Some(("union", child_name(node)?)),
            "enum_specifier" => Some(("enum", child_name(node)?)),
            "type_definition" => Some(("type", child_name(node)?)),
            "preproc_function_def" => Some(("macro", child_name(node)?)),
            _ => None,
        },
        Language::Cpp => match kind {
            "function_definition" =>
                Some(("function", function_name(node).or_else(|| child_name(node))?)),
            "declaration" if has_descendant_kind(node, "function_declarator") =>
                Some(("function", function_name(node).or_else(|| child_name(node))?)),
            "class_specifier" => Some(("class", child_name(node)?)),
            "struct_specifier" => Some(("struct", child_name(node)?)),
            "union_specifier" => Some(("union", child_name(node)?)),
            "enum_specifier" => Some(("enum", child_name(node)?)),
            "type_definition" | "alias_declaration" => Some(("type", child_name(node)?)),
            "namespace_definition" => Some(("namespace", child_name(node)?)),
            "preproc_function_def" => Some(("macro", child_name(node)?)),
            _ => None,
        },
        Language::Markdown => None,
    }
}

fn child_name(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name);
    }

    let mut cursor = node.walk();
    if let Some(name) =
        node.named_children(&mut cursor).find(|child| NAME_KINDS.contains(&child.kind()))
    {
        return Some(name);
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor).find_map(|child| first_descendant_node(child, NAME_KINDS))
}

fn first_descendant_node<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return Some(child);
        }
        if let Some(value) = first_descendant_node(child, kinds) {
            return Some(value);
        }
    }
    None
}

fn has_descendant_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| child.kind() == kind || has_descendant_kind(child, kind))
}

fn companion_name(node: Node<'_>) -> Option<Node<'_>> {
    for index in 0..node.child_count() {
        let Some(index) = u32::try_from(index).ok() else {
            continue;
        };
        if let Some(child) = node.child(index)
            && child.kind() == "companion"
        {
            return Some(child);
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "simple_identifier" | "type_identifier"))
}

fn kotlin_property_name(node: Node<'_>) -> Option<Node<'_>> {
    child_name(kotlin_variable_declaration(node).unwrap_or(node))
}

fn kotlin_variable_declaration(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find_map(|child| {
        if child.kind() == "variable_declaration" {
            Some(child)
        } else if matches!(child.kind(), "modifiers" | "type_parameters" | "type_constraints") {
            None
        } else {
            kotlin_variable_declaration(child)
        }
    })
}

fn function_name(node: Node<'_>) -> Option<Node<'_>> {
    let declarator = first_descendant_node(node, &["function_declarator"]).unwrap_or(node);
    let name_root = declarator.child_by_field_name("declarator").unwrap_or(declarator);
    if NAME_KINDS.contains(&name_root.kind()) {
        return Some(name_root);
    }
    last_descendant_node(name_root, NAME_KINDS)
}

fn last_descendant_node<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let mut last = None;
    for child in node.named_children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            last = Some(child);
        }
        if let Some(value) = last_descendant_node(child, kinds) {
            last = Some(value);
        }
    }
    last
}

fn impl_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier")
    })
}

fn make_symbol(
    path: &Path,
    language: Language,
    text: &str,
    node: Node<'_>,
    kind: &str,
    name: String,
) -> ParsedSymbol {
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
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
        facts: symbol_facts(language, text, node),
    }
}

fn symbol_facts(language: Language, text: &str, node: Node<'_>) -> Vec<ParsedSymbolFact> {
    if language != Language::Rust {
        return Vec::new();
    }
    let mut facts = Vec::new();
    for attribute in rust_attribute_items(text, node) {
        if rust_attribute_is_uniffi_export(&attribute) {
            facts.push(ParsedSymbolFact {
                kind: "rust_attr".to_string(),
                value: "uniffi_export".to_string(),
            });
        }
    }
    facts.sort_by(|left, right| (&left.kind, &left.value).cmp(&(&right.kind, &right.value)));
    facts.dedup();
    facts
}

fn rust_attribute_items(text: &str, node: Node<'_>) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_item" {
            attributes.push(node_text(child, text).unwrap_or_default());
        }
    }

    let mut preceding = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if previous.kind() != "attribute_item" {
            break;
        }
        preceding.push(node_text(previous, text).unwrap_or_default());
        sibling = previous.prev_named_sibling();
    }
    preceding.reverse();
    preceding.extend(attributes);
    preceding
}

fn rust_attribute_is_uniffi_export(attribute: &str) -> bool {
    attribute.contains("uniffi::export") || attribute.contains("::uniffi::export")
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
        if matches!(trimmed, "/**" | "*/") {
            continue;
        } else if let Some(doc_line) = clean_doc_comment_line(trimmed) {
            docs.push(doc_line);
        } else if trimmed.is_empty() {
            continue;
        } else {
            break;
        }
    }
    docs.reverse();
    (!docs.is_empty()).then(|| docs.join("\n"))
}

fn clean_doc_comment_line(trimmed: &str) -> Option<String> {
    let line = if trimmed.starts_with("///") {
        trimmed.trim_start_matches('/')
    } else if trimmed.starts_with('*') || trimmed.starts_with("/**") {
        trimmed.trim_start_matches('/').trim_start_matches('*').trim_end_matches('/')
    } else {
        return None;
    }
    .trim();

    (!line.is_empty()).then(|| line.to_string())
}
