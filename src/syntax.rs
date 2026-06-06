use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::Result;
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};
use tree_sitter::{Language, Node, Parser};

use crate::{
    index,
    project_graph::discover_project_graph,
    search::{line_range_for_node, symbol_range, SymbolRange},
    workspace::{language_for_path, FileRecord, ScanOptions, Workspace},
};

pub(crate) const MAX_CANDIDATES_PER_FILE: usize = 2_000;
pub(crate) const MAX_CANDIDATES_PER_ROOT: usize = 50_000;
pub(crate) const MAX_CANDIDATES_PER_QUERY: usize = 1_000;

const PARSER_PRODUCER: &str = "tree_sitter_parser";
const CALL_PRODUCER: &str = "tree_sitter_call_heuristic";
const PARSER_FACT: &str = "parser_fact";
const INFERRED_CANDIDATE: &str = "inferred_candidate";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateBudget {
    pub max_per_file: usize,
    pub max_per_root: usize,
    pub max_per_query: usize,
}

impl Default for CandidateBudget {
    fn default() -> Self {
        Self {
            max_per_file: MAX_CANDIDATES_PER_FILE,
            max_per_root: MAX_CANDIDATES_PER_ROOT,
            max_per_query: MAX_CANDIDATES_PER_QUERY,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanguageCandidateMatrix {
    pub language: &'static str,
    pub extracted_kinds: &'static [&'static str],
    pub known_blind_spots: &'static [&'static str],
}

pub(crate) fn candidate_language_matrix() -> &'static [LanguageCandidateMatrix] {
    &LANGUAGE_CANDIDATE_MATRIX
}

static LANGUAGE_CANDIDATE_MATRIX: [LanguageCandidateMatrix; 6] = [
    LanguageCandidateMatrix {
        language: "go",
        extracted_kinds: &["definition", "method", "type", "call", "import"],
        known_blind_spots: &[
            "interface dispatch",
            "embedded method promotion",
            "build tags and generated files",
            "reflection and cgo",
        ],
    },
    LanguageCandidateMatrix {
        language: "rust",
        extracted_kinds: &["definition", "method", "type", "call", "import"],
        known_blind_spots: &[
            "macro generated definitions and calls",
            "trait method dispatch",
            "cfg-gated code",
            "re-export and glob import binding",
            "generic struct type names (e.g. RegexBuilder<T>)",
            "impl block item extraction for non-trait methods",
            "proc-macro generated symbols",
        ],
    },
    LanguageCandidateMatrix {
        language: "java",
        extracted_kinds: &["definition", "method", "class", "type", "call", "import"],
        known_blind_spots: &[
            "overload resolution",
            "interface dispatch and inheritance",
            "reflection and framework injection",
            "annotation generated code",
        ],
    },
    LanguageCandidateMatrix {
        language: "typescript",
        extracted_kinds: &["definition", "method", "class", "type", "call", "import"],
        known_blind_spots: &[
            "dynamic property calls",
            "type-only imports and path aliases",
            "decorator and framework injection",
            "JSX generated calls",
            "non-exported module members (module.exports / exports.xxx)",
            "bundled or minified code without source maps",
            "CommonJS require() patterns",
        ],
    },
    LanguageCandidateMatrix {
        language: "javascript",
        extracted_kinds: &["definition", "method", "class", "call", "import"],
        known_blind_spots: &[
            "dynamic property calls",
            "CommonJS aliasing",
            "prototype mutation",
            "framework injection",
        ],
    },
    LanguageCandidateMatrix {
        language: "python",
        extracted_kinds: &["definition", "method", "class", "call", "import"],
        known_blind_spots: &[
            "dynamic attribute calls",
            "decorator generated bindings",
            "import alias rebinding",
            "metaclass generated members",
        ],
    },
];

#[derive(Clone, Debug)]
struct Symbol {
    path: String,
    language: String,
    root_id: String,
    name: String,
    kind: String,
    candidate_kind: String,
    range: Value,
    name_range: Value,
    body_range: Value,
    enclosing_symbol: Option<String>,
    body_hash: String,
    file_hash: String,
    producer: String,
    layer: String,
    known_blind_spots: Vec<&'static str>,
    warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CallCandidate {
    pub path: String,
    pub language: String,
    pub root_id: String,
    pub target: String,
    pub enclosing_symbol: Option<String>,
    pub range: Value,
    pub body_hash: Option<String>,
    pub file_hash: String,
    pub producer: String,
    pub layer: String,
    pub known_blind_spots: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TreeSitterCandidate {
    pub path: String,
    pub language: String,
    pub root_id: String,
    pub name: Option<String>,
    pub target: Option<String>,
    pub kind: String,
    pub symbol_kind: Option<String>,
    pub range: Value,
    pub name_range: Option<Value>,
    pub body_range: Option<Value>,
    pub call_range: Option<Value>,
    pub enclosing_symbol: Option<String>,
    pub body_hash: Option<String>,
    pub file_hash: String,
    pub producer: String,
    pub reliability: String,
    pub layer: String,
    pub known_blind_spots: Vec<&'static str>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateReuseState {
    pub path: String,
    pub file_hash: String,
    pub body_hashes: BTreeSet<String>,
    pub call_body_hashes: BTreeSet<String>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateReuseDecision {
    pub file_unchanged: bool,
    pub reuse_all_candidates: bool,
    pub reusable_body_hashes: BTreeSet<String>,
}

#[allow(dead_code)]
impl CandidateReuseState {
    pub(crate) fn from_candidates(path: &str, candidates: &[TreeSitterCandidate]) -> Option<Self> {
        let mut file_hash = None;
        let mut body_hashes = BTreeSet::new();
        let mut call_body_hashes = BTreeSet::new();
        for candidate in candidates
            .iter()
            .filter(|candidate| candidate.path.as_str() == path)
        {
            file_hash.get_or_insert_with(|| candidate.file_hash.clone());
            if let Some(body_hash) = &candidate.body_hash {
                if candidate.kind == "call" {
                    call_body_hashes.insert(body_hash.clone());
                } else {
                    body_hashes.insert(body_hash.clone());
                }
            }
        }
        Some(Self {
            path: path.to_string(),
            file_hash: file_hash?,
            body_hashes,
            call_body_hashes,
        })
    }

    pub(crate) fn reuse_decision(&self, newer: &Self) -> CandidateReuseDecision {
        let file_unchanged = self.file_hash == newer.file_hash;
        let reusable_body_hashes = self
            .body_hashes
            .intersection(&newer.body_hashes)
            .cloned()
            .collect();
        CandidateReuseDecision {
            file_unchanged,
            reuse_all_candidates: file_unchanged,
            reusable_body_hashes,
        }
    }
}

#[allow(dead_code)]
impl CandidateReuseDecision {
    pub(crate) fn can_reuse_calls_for_body(&self, body_hash: &str) -> bool {
        self.reuse_all_candidates || self.reusable_body_hashes.contains(body_hash)
    }
}

pub fn symbols(
    workspace: &Workspace,
    opts: &ScanOptions,
    query: &str,
) -> Result<(Value, Vec<String>)> {
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    let budget = CandidateBudget::default();
    let (query_limit, budget_limited) = query_result_limit(opts, budget);
    for symbol in collect_symbols_prefiltered(workspace, opts, &mut warnings, Some(query))? {
        if symbol.name.contains(query) {
            if results.len() >= query_limit {
                if budget_limited {
                    push_query_budget_warning(&mut warnings, "symbols", budget.max_per_query);
                }
                break;
            }
            results.push(symbol_to_json(symbol));
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
    let budget = CandidateBudget::default();
    let (query_limit, budget_limited) = query_result_limit(opts, budget);
    for symbol in collect_symbols_prefiltered(workspace, opts, &mut warnings, Some(identifier))? {
        if symbol.name == identifier {
            if results.len() >= query_limit {
                if budget_limited {
                    push_query_budget_warning(&mut warnings, "defs", budget.max_per_query);
                }
                break;
            }
            results.push(symbol_to_json(symbol));
        }
    }
    Ok((Value::Array(results), warnings))
}

pub(crate) fn definition_ranges(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<Vec<SymbolRange>> {
    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    let mut warnings = Vec::new();
    let ranges =
        collect_symbols_prefiltered(workspace, &scan_opts, &mut warnings, Some(identifier))?
            .into_iter()
            .filter(|symbol| symbol.name == identifier)
            .filter_map(|symbol| symbol_range(&symbol.path, &symbol.name_range))
            .collect();
    Ok(ranges)
}

pub fn calls(
    workspace: &Workspace,
    opts: &ScanOptions,
    identifier: &str,
) -> Result<(Value, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut results = Vec::new();
    let budget = CandidateBudget::default();
    let (query_limit, budget_limited) = query_result_limit(opts, budget);
    for call in collect_calls_prefiltered(workspace, opts, &mut warnings, Some(identifier))? {
        if call.enclosing_symbol.as_deref() == Some(identifier) {
            if results.len() >= query_limit {
                if budget_limited {
                    push_query_budget_warning(&mut warnings, "calls", budget.max_per_query);
                }
                break;
            }
            results.push(call_to_json(call));
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
    let budget = CandidateBudget::default();
    let (query_limit, budget_limited) = query_result_limit(opts, budget);
    for call in collect_calls_prefiltered(workspace, opts, &mut warnings, Some(identifier))? {
        if last_identifier(&call.target) == identifier {
            if results.len() >= query_limit {
                if budget_limited {
                    push_query_budget_warning(&mut warnings, "callers", budget.max_per_query);
                }
                break;
            }
            results.push(call_to_json(call));
        }
    }
    Ok((Value::Array(results), warnings))
}

fn query_result_limit(opts: &ScanOptions, budget: CandidateBudget) -> (usize, bool) {
    if opts.limit > 0 && opts.limit <= budget.max_per_query {
        (opts.limit, false)
    } else {
        (budget.max_per_query, true)
    }
}

fn push_query_budget_warning(warnings: &mut Vec<String>, query: &str, max: usize) {
    let message = format!(
        "tree_sitter_candidate_budget_exceeded: query {query} exceeded max returned candidates ({max})"
    );
    if !warnings.iter().any(|warning| warning == &message) {
        warnings.push(message);
    }
}

fn collect_symbols_prefiltered(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
    needle: Option<&str>,
) -> Result<Vec<Symbol>> {
    let candidates = collect_candidates_prefiltered(workspace, opts, warnings, needle)?;
    let mut symbols = candidates
        .into_iter()
        .filter(|candidate| is_symbol_candidate(&candidate))
        .filter_map(symbol_from_candidate)
        .collect::<Vec<_>>();
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
    let candidates = collect_candidates_prefiltered(workspace, opts, warnings, needle)?;
    let mut calls = candidates
        .into_iter()
        .filter(|candidate| candidate.kind == "call")
        .filter_map(call_from_candidate)
        .collect::<Vec<_>>();
    calls.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.target.cmp(&b.target))
            .then(a.enclosing_symbol.cmp(&b.enclosing_symbol))
    });
    Ok(calls)
}

#[allow(dead_code)]
pub(crate) fn collect_candidates(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
) -> Result<Vec<TreeSitterCandidate>> {
    collect_candidates_with_budget(workspace, opts, warnings, CandidateBudget::default())
}

#[allow(dead_code)]
pub(crate) fn collect_candidates_with_budget(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
    budget: CandidateBudget,
) -> Result<Vec<TreeSitterCandidate>> {
    collect_candidates_prefiltered_with_budget(workspace, opts, warnings, None, budget)
}

fn collect_candidates_prefiltered(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
    needle: Option<&str>,
) -> Result<Vec<TreeSitterCandidate>> {
    collect_candidates_prefiltered_with_budget(
        workspace,
        opts,
        warnings,
        needle,
        CandidateBudget::default(),
    )
}

fn collect_candidates_prefiltered_with_budget(
    workspace: &Workspace,
    opts: &ScanOptions,
    warnings: &mut Vec<String>,
    needle: Option<&str>,
    budget: CandidateBudget,
) -> Result<Vec<TreeSitterCandidate>> {
    let files = parser_candidate_files(workspace, opts, needle)?;
    parse_candidate_files(workspace, &files, warnings, budget)
}

fn parse_candidate_files(
    workspace: &Workspace,
    files: &[FileRecord],
    warnings: &mut Vec<String>,
    budget: CandidateBudget,
) -> Result<Vec<TreeSitterCandidate>> {
    let root_ids = root_ids_by_path(workspace);
    let parsed = files
        .par_iter()
        .map(|file| parse_candidates_in_file(workspace, file, budget, &root_ids))
        .collect::<Result<Vec<_>>>()?;
    let mut candidates = Vec::new();
    for (mut file_candidates, mut file_warnings) in parsed {
        candidates.append(&mut file_candidates);
        warnings.append(&mut file_warnings);
    }
    candidates.sort_by(|a, b| {
        a.root_id
            .cmp(&b.root_id)
            .then(a.path.cmp(&b.path))
            .then(a.kind.cmp(&b.kind))
            .then(a.name.cmp(&b.name))
            .then(a.target.cmp(&b.target))
    });
    Ok(enforce_root_budget(candidates, warnings, budget))
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

#[cfg(test)]
fn parse_symbols_in_file(
    workspace: &Workspace,
    file: &FileRecord,
) -> Result<(Vec<Symbol>, Vec<String>)> {
    let root_ids = root_ids_by_path(workspace);
    let (candidates, warnings) =
        parse_candidates_in_file(workspace, file, CandidateBudget::default(), &root_ids)?;
    Ok((
        candidates
            .into_iter()
            .filter(|candidate| is_symbol_candidate(candidate))
            .filter_map(symbol_from_candidate)
            .collect(),
        warnings,
    ))
}

fn parse_candidates_in_file(
    workspace: &Workspace,
    file: &FileRecord,
    budget: CandidateBudget,
    root_ids: &BTreeMap<String, String>,
) -> Result<(Vec<TreeSitterCandidate>, Vec<String>)> {
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
    let mut candidates = Vec::new();
    let root_id = root_id_for_file(&file.path, language_name, root_ids);
    let mut truncated = false;
    let mut context = CandidateWalkContext {
        path: &file.path,
        language: language_name,
        root_id: &root_id,
        file_hash: &file.hash,
        source: content.as_bytes(),
        max_per_file: budget.max_per_file,
        truncated: &mut truncated,
        candidates: &mut candidates,
    };
    walk_candidates(tree.root_node(), &mut context);
    if truncated {
        warnings.push(format!(
            "tree_sitter_candidate_budget_exceeded: file {} exceeded max candidates per file ({})",
            file.path, budget.max_per_file
        ));
    }
    Ok((candidates, warnings))
}

struct CandidateWalkContext<'a, 'b> {
    path: &'a str,
    language: &'a str,
    root_id: &'a str,
    file_hash: &'a str,
    source: &'a [u8],
    max_per_file: usize,
    truncated: &'b mut bool,
    candidates: &'b mut Vec<TreeSitterCandidate>,
}

fn walk_candidates(node: Node, context: &mut CandidateWalkContext) {
    if *context.truncated {
        return;
    }
    if let Some(candidate) = symbol_candidate(node, context) {
        push_candidate(candidate, context);
        if *context.truncated {
            return;
        }
    }
    if let Some(candidate) = import_candidate(node, context) {
        push_candidate(candidate, context);
        if *context.truncated {
            return;
        }
    }
    if let Some(candidate) = call_candidate(node, context) {
        push_candidate(candidate, context);
        if *context.truncated {
            return;
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if *context.truncated {
            break;
        }
        walk_candidates(child, context);
    }
}

fn push_candidate(candidate: TreeSitterCandidate, context: &mut CandidateWalkContext) {
    if context.candidates.len() < context.max_per_file {
        context.candidates.push(candidate);
    } else {
        *context.truncated = true;
    }
}

fn symbol_candidate(node: Node, context: &CandidateWalkContext) -> Option<TreeSitterCandidate> {
    let symbol_kind = symbol_kind(node.kind())?;
    let name_node = candidate_name_node(node)?;
    let name = name_node.utf8_text(context.source).ok()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let body_node = node.child_by_field_name("body").unwrap_or(node);
    let candidate_kind = candidate_kind_for_symbol(node, context.language);
    let reliability = PARSER_FACT.to_string();
    Some(TreeSitterCandidate {
        path: context.path.to_string(),
        language: context.language.to_string(),
        root_id: context.root_id.to_string(),
        name: Some(name),
        target: None,
        kind: candidate_kind.to_string(),
        symbol_kind: Some(symbol_kind.to_string()),
        range: point_range(node),
        name_range: Some(point_range(name_node)),
        body_range: Some(point_range(body_node)),
        call_range: None,
        enclosing_symbol: enclosing_symbol_name(node, context.source),
        body_hash: Some(hash_node(body_node, context.source)),
        file_hash: context.file_hash.to_string(),
        producer: PARSER_PRODUCER.to_string(),
        reliability: reliability.clone(),
        layer: reliability,
        known_blind_spots: known_blind_spots(context.language).to_vec(),
    })
}

fn import_candidate(node: Node, context: &CandidateWalkContext) -> Option<TreeSitterCandidate> {
    if !is_import_node(node.kind()) {
        return None;
    }
    let name = import_name(node, context.language, context.source)?;
    if name.is_empty() {
        return None;
    }
    let reliability = PARSER_FACT.to_string();
    Some(TreeSitterCandidate {
        path: context.path.to_string(),
        language: context.language.to_string(),
        root_id: context.root_id.to_string(),
        name: Some(name),
        target: None,
        kind: "import".to_string(),
        symbol_kind: Some("import".to_string()),
        range: point_range(node),
        name_range: None,
        body_range: None,
        call_range: None,
        enclosing_symbol: enclosing_symbol_name(node, context.source),
        body_hash: None,
        file_hash: context.file_hash.to_string(),
        producer: PARSER_PRODUCER.to_string(),
        reliability: reliability.clone(),
        layer: reliability,
        known_blind_spots: known_blind_spots(context.language).to_vec(),
    })
}

fn call_candidate(node: Node, context: &CandidateWalkContext) -> Option<TreeSitterCandidate> {
    if !is_call_node(node.kind()) {
        return None;
    }
    let target_node = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("name"))
        .or_else(|| first_named_child(node))?;
    let target = target_node
        .utf8_text(context.source)
        .ok()?
        .trim()
        .to_string();
    if target.is_empty() {
        return None;
    }
    let enclosing = enclosing_symbol_details(node, context.source);
    let reliability = INFERRED_CANDIDATE.to_string();
    Some(TreeSitterCandidate {
        path: context.path.to_string(),
        language: context.language.to_string(),
        root_id: context.root_id.to_string(),
        name: None,
        target: Some(target),
        kind: "call".to_string(),
        symbol_kind: None,
        range: point_range(node),
        name_range: None,
        body_range: None,
        call_range: Some(point_range(node)),
        enclosing_symbol: enclosing.as_ref().map(|details| details.name.clone()),
        body_hash: enclosing.map(|details| details.body_hash),
        file_hash: context.file_hash.to_string(),
        producer: CALL_PRODUCER.to_string(),
        reliability: reliability.clone(),
        layer: reliability,
        known_blind_spots: known_blind_spots(context.language).to_vec(),
    })
}

#[derive(Clone, Debug)]
struct EnclosingSymbolDetails {
    name: String,
    body_hash: String,
}

fn enclosing_symbol_details(node: Node, source: &[u8]) -> Option<EnclosingSymbolDetails> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_enclosing_symbol_node(parent.kind()) {
            let name_node = candidate_name_node(parent)?;
            let body_node = parent.child_by_field_name("body").unwrap_or(parent);
            return Some(EnclosingSymbolDetails {
                name: name_node.utf8_text(source).ok()?.to_string(),
                body_hash: hash_node(body_node, source),
            });
        }
        current = parent.parent();
    }
    None
}

fn enclosing_symbol_name(node: Node, source: &[u8]) -> Option<String> {
    enclosing_symbol_details(node, source).map(|details| details.name)
}

fn is_symbol_candidate(candidate: &TreeSitterCandidate) -> bool {
    candidate.reliability == PARSER_FACT && candidate.kind != "import"
}

fn symbol_from_candidate(candidate: TreeSitterCandidate) -> Option<Symbol> {
    Some(Symbol {
        path: candidate.path,
        language: candidate.language,
        root_id: candidate.root_id,
        name: candidate.name?,
        kind: candidate.symbol_kind?,
        candidate_kind: candidate.kind,
        range: candidate.range,
        name_range: candidate.name_range?,
        body_range: candidate.body_range?,
        enclosing_symbol: candidate.enclosing_symbol,
        body_hash: candidate.body_hash?,
        file_hash: candidate.file_hash,
        producer: candidate.producer,
        layer: candidate.layer,
        known_blind_spots: candidate.known_blind_spots,
        warning: None,
    })
}

fn call_from_candidate(candidate: TreeSitterCandidate) -> Option<CallCandidate> {
    Some(CallCandidate {
        path: candidate.path,
        language: candidate.language,
        root_id: candidate.root_id,
        target: candidate.target?,
        enclosing_symbol: candidate.enclosing_symbol,
        range: candidate.call_range.unwrap_or(candidate.range),
        body_hash: candidate.body_hash,
        file_hash: candidate.file_hash,
        producer: candidate.producer,
        layer: candidate.layer,
        known_blind_spots: candidate.known_blind_spots,
    })
}

fn enforce_root_budget(
    candidates: Vec<TreeSitterCandidate>,
    warnings: &mut Vec<String>,
    budget: CandidateBudget,
) -> Vec<TreeSitterCandidate> {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut warned_roots = BTreeSet::<String>::new();
    let mut retained = Vec::new();
    for candidate in candidates {
        let count = counts.entry(candidate.root_id.clone()).or_default();
        if *count < budget.max_per_root {
            *count += 1;
            retained.push(candidate);
        } else if warned_roots.insert(candidate.root_id.clone()) {
            warnings.push(format!(
                "tree_sitter_candidate_budget_exceeded: root {} exceeded max candidates per root ({})",
                candidate.root_id, budget.max_per_root
            ));
        }
    }
    retained
}

fn root_ids_by_path(workspace: &Workspace) -> BTreeMap<String, String> {
    let Ok(graph) = discover_project_graph(&workspace.root) else {
        return BTreeMap::new();
    };
    let mut root_ids = BTreeMap::new();
    for owner in graph.source_owners {
        root_ids.insert(owner.path, owner.root_id);
    }
    for generated in graph.generated_sources {
        root_ids.insert(generated.path, generated.owner_root_id);
    }
    root_ids
}

fn root_id_for_file(path: &str, language: &str, root_ids: &BTreeMap<String, String>) -> String {
    root_ids
        .get(path)
        .cloned()
        .unwrap_or_else(|| fallback_root_id(language))
}

fn fallback_root_id(language: &str) -> String {
    let root_language = match language {
        "javascript" => "typescript",
        other => other,
    };
    format!("{root_language}:.")
}

fn parser_language(path: &Path) -> Option<Language> {
    match language_for_path(path) {
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
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
        "function_item" | "function_definition" | "function_declaration" => Some("function"),
        "method_definition" | "method_declaration" => Some("function"),
        "method_elem" => Some("function"),
        "struct_item" | "type_declaration" => Some("struct"),
        "enum_item" | "enum_declaration" => Some("enum"),
        "trait_item" => Some("trait"),
        "mod_item" => Some("module"),
        "constructor_declaration" | "compact_constructor_declaration" => Some("constructor"),
        "interface_declaration" => Some("interface"),
        "record_declaration" => Some("record"),
        "annotation_type_declaration" => Some("annotation"),
        "class_definition" | "class_declaration" => Some("class"),
        "lexical_declaration" | "var_declaration" | "const_declaration" => Some("variable"),
        _ => None,
    }
}

fn is_enclosing_symbol_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "method_definition"
            | "method_declaration"
            | "method_elem"
            | "constructor_declaration"
            | "compact_constructor_declaration"
    )
}

fn candidate_kind_for_symbol(node: Node, language: &str) -> &'static str {
    match node.kind() {
        "method_definition"
        | "method_declaration"
        | "constructor_declaration"
        | "compact_constructor_declaration"
        | "method_elem" => "method",
        "class_definition" | "class_declaration" => "class",
        "struct_item"
        | "enum_item"
        | "enum_declaration"
        | "trait_item"
        | "interface_declaration"
        | "record_declaration"
        | "annotation_type_declaration"
        | "type_declaration" => "type",
        "function_item" if language == "rust" && has_parent_kind(node, "impl_item") => "method",
        "function_definition" if has_parent_kind(node, "class_definition") => "method",
        _ => "definition",
    }
}

fn is_call_node(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression" | "call" | "method_invocation" | "function_call_expression"
    )
}

fn is_import_node(kind: &str) -> bool {
    matches!(
        kind,
        "use_declaration"
            | "import_statement"
            | "import_declaration"
            | "import_from_statement"
            | "import_require_clause"
    )
}

fn first_named_child(node: Node) -> Option<Node> {
    (0..node.named_child_count()).find_map(|idx| node.named_child(idx))
}

fn candidate_name_node(node: Node) -> Option<Node> {
    node.child_by_field_name("name")
        .or_else(|| child_name_node(node))
        .or_else(|| first_named_child(node))
}

fn child_name_node(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = child.child_by_field_name("name") {
            return Some(found);
        }
    }
    None
}

fn has_parent_kind(node: Node, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(node) = current {
        if node.kind() == kind {
            return true;
        }
        current = node.parent();
    }
    false
}

fn import_name(node: Node, language: &str, source: &[u8]) -> Option<String> {
    let raw = node.utf8_text(source).ok()?.trim();
    let value = match language {
        "rust" => raw
            .strip_prefix("use ")
            .unwrap_or(raw)
            .trim_end_matches(';')
            .trim()
            .to_string(),
        "java" => raw
            .strip_prefix("import ")
            .unwrap_or(raw)
            .trim_start_matches("static ")
            .trim_end_matches(';')
            .trim()
            .to_string(),
        "go" => raw
            .strip_prefix("import ")
            .unwrap_or(raw)
            .trim_end_matches(';')
            .trim()
            .to_string(),
        "typescript" | "javascript" => raw
            .strip_prefix("import ")
            .unwrap_or(raw)
            .trim_end_matches(';')
            .trim()
            .to_string(),
        "python" => raw
            .strip_prefix("from ")
            .or_else(|| raw.strip_prefix("import "))
            .unwrap_or(raw)
            .trim()
            .to_string(),
        _ => raw.to_string(),
    };
    Some(value)
}

fn known_blind_spots(language: &str) -> &'static [&'static str] {
    candidate_language_matrix()
        .iter()
        .find(|entry| entry.language == language)
        .map(|entry| entry.known_blind_spots)
        .unwrap_or(&["cross-file binding", "type inference", "dynamic dispatch"])
}

fn hash_node(node: Node, source: &[u8]) -> String {
    hash_bytes(&source[node.start_byte()..node.end_byte()])
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
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
        "symbolName": symbol.name,
        "kind": symbol.kind,
        "candidateKind": symbol.candidate_kind,
        "language": symbol.language,
        "rootId": symbol.root_id,
        "container": Value::Null,
        "enclosingSymbol": symbol.enclosing_symbol,
        "role": "definition",
        "range": symbol.range,
        "bodyRange": symbol.body_range,
        "bodyHash": symbol.body_hash,
        "fileHash": symbol.file_hash,
        "producer": symbol.producer,
        "reliability": PARSER_FACT,
        "layer": symbol.layer,
        "exact": false,
        "fallbackReason": "precise_scip_index_unavailable",
        "knownBlindSpots": symbol.known_blind_spots,
        "warning": symbol.warning
    })
}

fn call_to_json(call: CallCandidate) -> Value {
    json!({
        "path": call.path,
        "target": call.target,
        "kind": "call",
        "enclosingSymbol": call.enclosing_symbol,
        "language": call.language,
        "rootId": call.root_id,
        "range": call.range,
        "bodyHash": call.body_hash,
        "fileHash": call.file_hash,
        "producer": call.producer,
        "reliability": INFERRED_CANDIDATE,
        "layer": call.layer,
        "exact": false,
        "knownBlindSpots": call.known_blind_spots
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

    fn scan_all() -> ScanOptions {
        ScanOptions {
            include: vec![],
            exclude: vec![],
            hidden: false,
            no_ignore: false,
            lang: vec![],
            changed: false,
            cursor: None,
            allow_broad: true,
            limit: 0,
        }
    }

    #[test]
    fn candidate_language_matrix_lists_supported_targets_and_blind_spots() {
        let matrix = candidate_language_matrix();
        let rust = matrix
            .iter()
            .find(|entry| entry.language == "rust")
            .expect("rust matrix entry");
        assert!(rust.extracted_kinds.contains(&"definition"));
        assert!(rust.extracted_kinds.contains(&"method"));
        assert!(rust.extracted_kinds.contains(&"call"));
        assert!(rust.extracted_kinds.contains(&"import"));
        assert!(rust
            .known_blind_spots
            .contains(&"macro generated definitions and calls"));

        let go = matrix
            .iter()
            .find(|entry| entry.language == "go")
            .expect("go matrix entry");
        assert!(go.extracted_kinds.contains(&"type"));
        assert!(go.extracted_kinds.contains(&"import"));

        let javascript = matrix
            .iter()
            .find(|entry| entry.language == "javascript")
            .expect("javascript matrix entry");
        assert!(javascript.extracted_kinds.contains(&"class"));
        assert!(javascript
            .known_blind_spots
            .contains(&"dynamic property calls"));
    }

    #[test]
    fn candidates_cover_schema_for_definitions_calls_and_imports() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "use crate::beta;\n\nstruct Widget;\n\nfn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let mut warnings = Vec::new();

        let candidates = collect_candidates(&workspace, &scan_all(), &mut warnings).unwrap();

        assert!(warnings.is_empty());
        let alpha = candidates
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some("alpha"))
            .expect("alpha definition candidate");
        assert_eq!(alpha.path, "src/lib.rs");
        assert_eq!(alpha.language, "rust");
        assert_eq!(alpha.root_id, "rust:.");
        assert_eq!(alpha.kind, "definition");
        assert_eq!(alpha.reliability, "parser_fact");
        assert_eq!(alpha.layer, "parser_fact");
        assert_eq!(alpha.producer, "tree_sitter_parser");
        assert!(alpha.file_hash.starts_with("blake3:"));
        assert!(alpha.body_hash.as_deref().unwrap().starts_with("blake3:"));
        assert!(alpha.body_range.is_some());
        assert!(alpha
            .known_blind_spots
            .contains(&"macro generated definitions and calls"));

        let import = candidates
            .iter()
            .find(|candidate| candidate.kind == "import")
            .expect("import candidate");
        assert_eq!(import.name.as_deref(), Some("crate::beta"));
        assert_eq!(import.reliability, "parser_fact");
        assert_eq!(import.layer, "parser_fact");
        assert!(import.body_hash.is_none());

        let call = candidates
            .iter()
            .find(|candidate| candidate.target.as_deref() == Some("beta"))
            .expect("beta call candidate");
        assert_eq!(call.kind, "call");
        assert_eq!(call.enclosing_symbol.as_deref(), Some("alpha"));
        assert_eq!(call.reliability, "inferred_candidate");
        assert_eq!(call.layer, "inferred_candidate");
        assert_eq!(call.body_hash, alpha.body_hash);
        assert!(call.body_range.is_none());
        assert!(call
            .known_blind_spots
            .contains(&"macro generated definitions and calls"));
    }

    #[test]
    fn go_candidates_extract_type_method_import_and_call() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/sample\n").unwrap();
        fs::write(
            dir.path().join("main.go"),
            "package main\n\nimport \"fmt\"\n\ntype Widget struct{}\n\nfunc Alpha() {\n    fmt.Println(\"x\")\n    Beta()\n}\n\nfunc (Widget) Run() {\n    Beta()\n}\n\nfunc Beta() {}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let mut warnings = Vec::new();

        let candidates = collect_candidates(&workspace, &scan_all(), &mut warnings).unwrap();

        assert!(warnings.is_empty());
        assert!(candidates.iter().any(|candidate| {
            candidate.language == "go"
                && candidate.root_id == "go:."
                && candidate.kind == "type"
                && candidate.name.as_deref() == Some("Widget")
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.language == "go"
                && candidate.kind == "method"
                && candidate.name.as_deref() == Some("Run")
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.language == "go"
                && candidate.kind == "import"
                && candidate.name.as_deref() == Some("\"fmt\"")
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.language == "go"
                && candidate.kind == "call"
                && candidate.target.as_deref() == Some("Beta")
                && candidate.enclosing_symbol.as_deref() == Some("Alpha")
                && candidate.body_hash.is_some()
        }));
    }

    #[test]
    fn rust_impl_blocks_do_not_emit_fake_defs_or_sibling_enclosing_symbols() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "struct Widget;\n\nimpl Widget {\n    fn run(&self) {\n        self.helper();\n    }\n\n    fn helper(&self) {}\n}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let mut warnings = Vec::new();

        let candidates = collect_candidates(&workspace, &scan_all(), &mut warnings).unwrap();

        assert!(warnings.is_empty());
        assert!(!candidates.iter().any(|candidate| {
            candidate.kind == "definition"
                && candidate.symbol_kind.as_deref() == Some("impl")
                && candidate.name.as_deref() == Some("run")
        }));

        let (defs_json, defs_warnings) = defs(&workspace, &scan_all(), "run").unwrap();
        assert!(defs_warnings.is_empty());
        let defs = defs_json.as_array().unwrap();
        assert_eq!(defs.len(), 1, "{defs_json}");
        assert_eq!(defs[0]["name"], "run");
        assert_eq!(defs[0]["kind"], "function");
        assert_eq!(defs[0]["candidateKind"], "method");

        let (callers_json, callers_warnings) = callers(&workspace, &scan_all(), "helper").unwrap();
        assert!(callers_warnings.is_empty());
        let helper_calls = callers_json.as_array().unwrap();
        assert_eq!(helper_calls.len(), 1, "{callers_json}");
        assert_eq!(helper_calls[0]["enclosingSymbol"], "run");
    }

    #[test]
    fn candidate_budget_caps_per_file_and_root_with_caveats() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/extra.rs"),
            "fn five() {}\nfn six() {}\nfn seven() {}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let budget = CandidateBudget {
            max_per_file: 2,
            max_per_root: 3,
            max_per_query: 100,
        };
        let mut warnings = Vec::new();

        let candidates =
            collect_candidates_with_budget(&workspace, &scan_all(), &mut warnings, budget).unwrap();

        assert_eq!(candidates.len(), 3);
        assert!(warnings.iter().any(|warning| {
            warning.starts_with("tree_sitter_candidate_budget_exceeded: file src/lib.rs")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.starts_with("tree_sitter_candidate_budget_exceeded: root rust:.")
        }));
    }

    #[test]
    fn query_budget_caps_parser_symbols_with_caveat() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut source = String::new();
        for idx in 0..(MAX_CANDIDATES_PER_QUERY + 5) {
            source.push_str(&format!("fn needle_{idx}() {{}}\n"));
        }
        fs::write(dir.path().join("src/lib.rs"), source).unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let mut opts = scan_all();
        opts.limit = 0;

        let (results, warnings) = symbols(&workspace, &opts, "needle_").unwrap();

        assert_eq!(results.as_array().unwrap().len(), MAX_CANDIDATES_PER_QUERY);
        assert!(warnings.iter().any(|warning| {
            warning.starts_with("tree_sitter_candidate_budget_exceeded: query symbols")
        }));
    }

    #[test]
    fn body_hash_reuse_survives_file_hash_changes_outside_body() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let mut first_warnings = Vec::new();
        let first = collect_candidates(&workspace, &scan_all(), &mut first_warnings).unwrap();
        let first_state = CandidateReuseState::from_candidates("src/lib.rs", &first).unwrap();
        let first_alpha_hash = first
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some("alpha"))
            .and_then(|candidate| candidate.body_hash.clone())
            .unwrap();

        fs::write(
            dir.path().join("src/lib.rs"),
            "// changed header\nfn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
        )
        .unwrap();
        let workspace = Workspace::discover(dir.path()).unwrap();
        let mut second_warnings = Vec::new();
        let second = collect_candidates(&workspace, &scan_all(), &mut second_warnings).unwrap();
        let second_state = CandidateReuseState::from_candidates("src/lib.rs", &second).unwrap();
        let decision = first_state.reuse_decision(&second_state);

        assert!(!decision.file_unchanged);
        assert!(decision.reusable_body_hashes.contains(&first_alpha_hash));
        assert!(decision.can_reuse_calls_for_body(&first_alpha_hash));
        let second_alpha_hash = second
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some("alpha"))
            .and_then(|candidate| candidate.body_hash.clone())
            .unwrap();
        assert_eq!(first_alpha_hash, second_alpha_hash);
    }
}
