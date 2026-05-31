use std::{fs, path::Path};

use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tree_sitter::{Language, Node, Parser};

use crate::{
    index,
    search::line_range_for_node,
    workspace::{language_for_path, FileRecord, ScanOptions, Workspace},
};

#[derive(Clone, Debug)]
struct Symbol {
    path: String,
    language: String,
    name: String,
    kind: String,
    range: Value,
    body_range: Value,
    producer: String,
    warning: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CallCandidate {
    pub path: String,
    pub language: String,
    pub target: String,
    pub enclosing_symbol: Option<String>,
    pub range: Value,
    pub file_hash: String,
    pub producer: String,
}

pub fn symbols(
    workspace: &Workspace,
    opts: &ScanOptions,
    query: &str,
) -> Result<(Value, Vec<String>)> {
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    for symbol in collect_symbols_prefiltered(workspace, opts, &mut warnings, Some(query))? {
        if symbol.name.contains(query) {
            results.push(symbol_to_json(symbol));
        }
        if opts.limit > 0 && results.len() >= opts.limit {
            break;
        }
    }
    Ok((Value::Array(results), warnings))
}

pub fn defs(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<(Value, Vec<String>)> {
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    for symbol in collect_symbols_prefiltered(workspace, opts, &mut warnings, Some(identifier))? {
        if symbol.name == identifier {
            results.push(symbol_to_json(symbol));
        }
        if opts.limit > 0 && results.len() >= opts.limit {
            break;
        }
    }
    Ok((Value::Array(results), warnings))
}

pub fn calls(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<(Value, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut results = Vec::new();
    for call in collect_calls_prefiltered(workspace, opts, &mut warnings, Some(identifier))? {
        if call.enclosing_symbol.as_deref() == Some(identifier) {
            results.push(call_to_json(call));
        }
        if opts.limit > 0 && results.len() >= opts.limit {
            break;
        }
    }
    Ok((Value::Array(results), warnings))
}

pub fn callers(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<(Value, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut results = Vec::new();
    for call in collect_calls_prefiltered(workspace, opts, &mut warnings, Some(identifier))? {
        if last_identifier(&call.target) == identifier {
            results.push(call_to_json(call));
        }
        if opts.limit > 0 && results.len() >= opts.limit {
            break;
        }
    }
    Ok((Value::Array(results), warnings))
}

fn collect_symbols_prefiltered(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
    needle: Option<&str>,
) -> Result<Vec<Symbol>> {
    let files = parser_candidate_files(workspace, opts, needle)?;
    parse_symbol_files(workspace, &files, warnings)
}

fn parse_symbol_files(
    workspace: &Workspace,
    files: &[FileRecord],
    warnings: &mut Vec<String>,
) -> Result<Vec<Symbol>> {
    let parsed = files
        .par_iter()
        .map(|file| parse_symbols_in_file(workspace, file))
        .collect::<Result<Vec<_>>>()?;
    let mut symbols = Vec::new();
    for (mut file_symbols, mut file_warnings) in parsed {
        symbols.append(&mut file_symbols);
        warnings.append(&mut file_warnings);
    }
    symbols.sort_by(|a, b| a.path.cmp(&b.path).then(a.name.cmp(&b.name)));
    Ok(symbols)
}

pub(crate) fn collect_calls(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
) -> Result<Vec<CallCandidate>> {
    collect_calls_prefiltered(workspace, opts, warnings, None)
}

fn collect_calls_prefiltered(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
    needle: Option<&str>,
) -> Result<Vec<CallCandidate>> {
    let files = parser_candidate_files(workspace, opts, needle)?;
    parse_call_files(workspace, &files, warnings)
}

fn parse_call_files(
    workspace: &Workspace,
    files: &[FileRecord],
    warnings: &mut Vec<String>,
) -> Result<Vec<CallCandidate>> {
    let parsed = files
        .par_iter()
        .map(|file| parse_calls_in_file(workspace, file))
        .collect::<Result<Vec<_>>>()?;
    let mut calls = Vec::new();
    for (mut file_calls, mut file_warnings) in parsed {
        calls.append(&mut file_calls);
        warnings.append(&mut file_warnings);
    }
    calls.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.target.cmp(&b.target))
            .then(a.enclosing_symbol.cmp(&b.enclosing_symbol))
    });
    Ok(calls)
}

fn parser_candidate_files(
    workspace: &Workspace,
    opts: &ScanOptions,
    needle: Option<&str>,
) -> Result<Vec<FileRecord>> {
    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    if let Some(needle) = needle.filter(|value| value.as_bytes().len() >= 3) {
        if let Some((records, _index)) =
            index::fresh_text_records(workspace, &scan_opts, needle, "literal")?
        {
            return Ok(records);
        }
    }
    workspace.scan_files(&scan_opts)
}

fn parse_symbols_in_file(
    workspace: &Workspace,
    file: &FileRecord,
) -> Result<(Vec<Symbol>, Vec<String>)> {
    let path = workspace.abs_path(&file.path);
    let Some(language) = parser_language(&path) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let language_name = language_for_path(&path);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return Ok((Vec::new(), Vec::new())),
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let Some(tree) = parser.parse(&content, None) else {
        return Ok((Vec::new(), vec![format!("parser failed for {}", file.path)]));
    };
    let mut warnings = Vec::new();
    if tree.root_node().has_error() {
        warnings.push(format!("partial parse with syntax errors: {}", file.path));
    }
    let mut symbols = Vec::new();
    walk_symbols(
        tree.root_node(),
        &file.path,
        language_name,
        content.as_bytes(),
        &mut symbols,
    );
    Ok((symbols, warnings))
}

fn parse_calls_in_file(
    workspace: &Workspace,
    file: &FileRecord,
) -> Result<(Vec<CallCandidate>, Vec<String>)> {
    let path = workspace.abs_path(&file.path);
    let Some(language) = parser_language(&path) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let language_name = language_for_path(&path);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return Ok((Vec::new(), Vec::new())),
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let Some(tree) = parser.parse(&content, None) else {
        return Ok((Vec::new(), vec![format!("parser failed for {}", file.path)]));
    };
    let mut warnings = Vec::new();
    if tree.root_node().has_error() {
        warnings.push(format!("partial parse with syntax errors: {}", file.path));
    }
    let mut calls = Vec::new();
    walk_calls(
        tree.root_node(),
        &file.path,
        language_name,
        &file.hash,
        content.as_bytes(),
        &mut calls,
    );
    Ok((calls, warnings))
}

fn walk_symbols(node: Node, path: &str, language: &str, source: &[u8], symbols: &mut Vec<Symbol>) {
    if let Some(kind) = symbol_kind(node.kind()) {
        if let Some(name_node) = node
            .child_by_field_name("name")
            .or_else(|| first_named_child(node))
        {
            if let Ok(name) = name_node.utf8_text(source) {
                let body_node = node.child_by_field_name("body").unwrap_or(node);
                symbols.push(Symbol {
                    path: path.to_string(),
                    language: language.to_string(),
                    name: name.to_string(),
                    kind: kind.to_string(),
                    range: point_range(node),
                    body_range: point_range(body_node),
                    producer: "tree_sitter_parser".to_string(),
                    warning: None,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, path, language, source, symbols);
    }
}

fn walk_calls(
    node: Node,
    path: &str,
    language: &str,
    file_hash: &str,
    source: &[u8],
    calls: &mut Vec<CallCandidate>,
) {
    if is_call_node(node.kind()) {
        if let Some(target_node) = node
            .child_by_field_name("function")
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| first_named_child(node))
        {
            if let Ok(target) = target_node.utf8_text(source) {
                calls.push(CallCandidate {
                    path: path.to_string(),
                    language: language.to_string(),
                    target: target.trim().to_string(),
                    enclosing_symbol: enclosing_symbol(node, source),
                    range: point_range(node),
                    file_hash: file_hash.to_string(),
                    producer: "tree_sitter_call_heuristic".to_string(),
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_calls(child, path, language, file_hash, source, calls);
    }
}

fn parser_language(path: &Path) -> Option<Language> {
    match language_for_path(path) {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        _ => None,
    }
}

fn symbol_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "function_item" | "function_definition" | "function_declaration" | "method_definition" => {
            Some("function")
        }
        "struct_item" => Some("struct"),
        "enum_item" | "enum_declaration" => Some("enum"),
        "trait_item" => Some("trait"),
        "impl_item" => Some("impl"),
        "mod_item" => Some("module"),
        "method_declaration" => Some("function"),
        "constructor_declaration" | "compact_constructor_declaration" => Some("constructor"),
        "interface_declaration" => Some("interface"),
        "record_declaration" => Some("record"),
        "annotation_type_declaration" => Some("annotation"),
        "class_definition" | "class_declaration" => Some("class"),
        "lexical_declaration" => Some("variable"),
        _ => None,
    }
}

fn is_call_node(kind: &str) -> bool {
    matches!(kind, "call_expression" | "call" | "method_invocation")
}

fn first_named_child(node: Node) -> Option<Node> {
    (0..node.named_child_count()).find_map(|idx| node.named_child(idx))
}

fn enclosing_symbol(node: Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(node) = current {
        if symbol_kind(node.kind()).is_some() {
            if let Some(name_node) = node
                .child_by_field_name("name")
                .or_else(|| first_named_child(node))
            {
                return name_node.utf8_text(source).ok().map(ToString::to_string);
            }
        }
        current = node.parent();
    }
    None
}

fn point_range(node: Node) -> Value {
    let start = node.start_position();
    let end = node.end_position();
    line_range_for_node(start.row, start.column, end.row, end.column)
}

fn symbol_to_json(symbol: Symbol) -> Value {
    json!({
        "path": symbol.path,
        "name": symbol.name,
        "kind": symbol.kind,
        "language": symbol.language,
        "range": symbol.range,
        "bodyRange": symbol.body_range,
        "producer": symbol.producer,
        "reliability": "parser_fact",
        "exact": false,
        "warning": symbol.warning
    })
}

fn call_to_json(call: CallCandidate) -> Value {
    json!({
        "path": call.path,
        "target": call.target,
        "enclosingSymbol": call.enclosing_symbol,
        "language": call.language,
        "range": call.range,
        "fileHash": call.file_hash,
        "producer": call.producer,
        "reliability": "inferred_candidate",
        "exact": false,
        "knownBlindSpots": [
            "dynamic dispatch",
            "trait/interface implementations",
            "reflection",
            "macro generated code",
            "framework injection",
            "alias-heavy imports"
        ]
    })
}

pub(crate) fn last_identifier(target: &str) -> &str {
    target
        .rsplit(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .find(|part| !part.is_empty())
        .unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parser_symbols_use_extension_language_even_when_index_record_is_stale() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/main/java/example")).unwrap();
        fs::write(
            dir.path().join("src/main/java/example/Sample.java"),
            "package example;\n\npublic class Sample {}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let file = FileRecord {
            path: "src/main/java/example/Sample.java".to_string(),
            language: "text".to_string(),
            size: 39,
            mtime_ms: 0,
            mode: 0,
            hash: "blake3:test".to_string(),
        };

        let (symbols, warnings) = parse_symbols_in_file(&workspace, &file).unwrap();

        assert!(warnings.is_empty());
        assert!(symbols
            .iter()
            .any(|symbol| symbol.name == "Sample" && symbol.language == "java"));
    }
}
