use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use globset::Glob;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    index,
    workspace::{
        language_for_path, matches_filters, matches_lang, FileCatalogRecord, FileRecord,
        ScanOptions, Workspace,
    },
};

const MAX_FULL_READ_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_CHARS: usize = 240;
const BROAD_SAMPLE_LIMIT: usize = 5;
const SMALL_WORKSPACE_FILE_LIMIT: usize = 20;
const MEDIUM_WORKSPACE_FILE_LIMIT: usize = 200;
const SMALL_WORKSPACE_BYTES: u64 = 512 * 1024;
const MEDIUM_WORKSPACE_BYTES: u64 = 5 * 1024 * 1024;
const MEDIUM_HIT_LIMIT: usize = 50;
const LARGE_HIT_LIMIT: usize = 100;
const MEDIUM_PREVIEW_CHARS: usize = 160;
const LARGE_PREVIEW_CHARS: usize = 96;

pub struct QueryOutput {
    pub results: Value,
    pub index: Value,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub facets: Value,
    pub guard: Option<Value>,
    pub budget: Value,
}

pub struct Page {
    pub results: Value,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub facets: Value,
    pub guard: Option<Value>,
}

#[derive(Clone, Debug)]
struct BroadGuard {
    reason: &'static str,
    suggestion: &'static str,
}

#[derive(Clone, Debug)]
struct OutputBudget {
    tier: &'static str,
    max_results: usize,
    max_preview_chars: usize,
    max_context_lines: u16,
    reason: &'static str,
    repository_files: usize,
    repository_bytes: u64,
    estimated_matches: usize,
}

pub fn files(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    strict_glob: bool,
) -> Result<QueryOutput> {
    let mut results = Vec::new();
    let matcher = if strict_glob {
        Some(Glob::new(pattern)?.compile_matcher())
    } else {
        None
    };
    let guard = broad_guard(if strict_glob { "glob" } else { "files" }, pattern, None);
    let broad_path = guard
        .as_ref()
        .map(|guard| guard.reason == "broad_path_pattern")
        .unwrap_or(false);

    let source = candidate_file_catalog(workspace, opts)?;
    let repository_files = source.records.len();
    let repository_bytes = source.records.iter().map(|file| file.size).sum();
    for file in source.records {
        let matches = broad_path
            || matcher
                .as_ref()
                .map(|glob| glob.is_match(&file.path))
                .unwrap_or_else(|| file.path.contains(pattern));
        if matches {
            results.push(json!({
                "path": file.path,
                "language": file.language,
                "size": file.size,
                "hash": file.hash,
                "producer": file.source.file_catalog_producer(),
                "indexFresh": file.source.index_fresh(),
                "sourceReason": file.source.reason(),
                "reliability": "source_fact",
                "exact": true
            }));
        }
    }
    let budget = output_budget(
        repository_files,
        repository_bytes,
        results.len(),
        opts.limit,
        0,
    );
    paged_query_output(
        results,
        source.index,
        opts,
        guard,
        budget,
        &pagination_scope(
            if strict_glob { "glob" } else { "files" },
            json!({ "pattern": pattern, "strictGlob": strict_glob }),
            opts,
            &workspace.snapshot_id,
        ),
    )
}

pub fn list(
    workspace: &Workspace,
    opts: &ScanOptions,
    dir: Option<&str>,
    recursive: bool,
) -> Result<Value> {
    let rel_dir = dir.unwrap_or(".");
    let base = resolve_workspace_dir(workspace, rel_dir)?;

    let mut results = Vec::new();
    if recursive {
        collect_tree(workspace, opts, &base, 0, None, &mut results)?;
    } else {
        for entry in browse_entries(workspace, opts, &base, Some(1))? {
            if !path_matches_output_filters(workspace, &entry.path, opts) {
                continue;
            }
            let Ok(metadata) = fs::metadata(&entry.path) else {
                continue;
            };
            results.push(json!({
                "path": workspace.rel_path(&entry.path),
                "kind": if metadata.is_dir() { "directory" } else { "file" },
                "size": if metadata.is_file() { metadata.len() } else { 0 },
                "language": if metadata.is_file() { language_for_path(&entry.path) } else { "directory" },
                "producer": "filesystem",
                "reliability": "source_fact",
                "exact": true
            }));
            if opts.limit > 0 && results.len() >= opts.limit {
                break;
            }
        }
    }
    Ok(Value::Array(results))
}

pub fn tree(
    workspace: &Workspace,
    opts: &ScanOptions,
    dir: Option<&str>,
    depth: Option<u8>,
) -> Result<Value> {
    let rel_dir = dir.unwrap_or(".");
    let base = resolve_workspace_dir(workspace, rel_dir)?;
    let mut results = Vec::new();
    collect_tree(
        workspace,
        opts,
        &base,
        0,
        depth.map(usize::from),
        &mut results,
    )?;
    Ok(Value::Array(results))
}

pub fn read(workspace: &Workspace, target: &str) -> Result<Value> {
    let request = ReadTarget::parse(target)?;
    let path = workspace.abs_path(&request.path);
    let canonical_path =
        dunce::canonicalize(&path).with_context(|| format!("failed to read {}", request.path))?;
    if !canonical_path.starts_with(&workspace.root) {
        return Err(anyhow!("path escapes workspace root: {}", request.path));
    }

    let metadata = fs::metadata(&canonical_path)
        .with_context(|| format!("failed to read {}", request.path))?;
    if !metadata.is_file() {
        return Err(anyhow!("failed to read {}", request.path));
    }

    let file_facts = scan_file_facts(&canonical_path, &request.path)?;
    if file_facts.binary {
        return Ok(json!({
            "path": request.path,
            "range": {
                "start": { "line": 1, "column": 1 },
                "end": { "line": 1, "column": 1 }
            },
            "content": "",
            "binary": true,
            "truncated": false,
            "fileHash": file_facts.hash,
            "language": language_for_path(&path),
            "producer": "snapshot_store_live_read",
            "reliability": "source_fact",
            "exact": false,
            "warnings": ["binary_file_not_displayed"]
        }));
    }

    let mut warnings = Vec::new();
    let read_content = if request.has_explicit_range {
        read_line_range(
            &canonical_path,
            &request.path,
            request.start_line.unwrap_or(1),
            request.end_line.unwrap_or(1),
        )?
    } else if metadata.len() as usize > MAX_FULL_READ_BYTES {
        warnings.push("large_file_truncated");
        read_prefix(&canonical_path, &request.path, MAX_FULL_READ_BYTES)?
    } else {
        read_full_text(&canonical_path, &request.path)?
    };

    Ok(json!({
        "path": request.path,
        "range": {
            "start": { "line": read_content.start_line, "column": 1 },
            "end": { "line": read_content.end_line, "column": read_content.end_column }
        },
        "content": read_content.content,
        "binary": false,
        "truncated": read_content.truncated,
        "fileHash": file_facts.hash,
        "language": language_for_path(&path),
        "producer": "snapshot_store_live_read",
        "reliability": "source_fact",
        "exact": !read_content.truncated,
        "warnings": warnings
    }))
}

pub fn find(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    mode: &str,
    context: u16,
    refs_mode: bool,
) -> Result<QueryOutput> {
    let regex = match mode {
        "literal" => Regex::new(&regex::escape(pattern))?,
        "regex" => Regex::new(pattern)?,
        other => return Err(anyhow!("unsupported search mode: {other}")),
    };

    let source = candidate_text_files(workspace, opts, pattern, mode)?;
    let repository_files = source.records.len();
    let repository_bytes = source.records.iter().map(|file| file.record.size).sum();
    let provisional_budget =
        output_budget(repository_files, repository_bytes, 0, opts.limit, context);
    let mut results = Vec::new();
    for file in source.records {
        let path = workspace.abs_path(&file.record.path);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for mat in regex.find_iter(&content) {
            if refs_mode && !identifier_boundary(&content, mat.start(), mat.end()) {
                continue;
            }
            let range = byte_range_to_line_range(&content, mat.start(), mat.end());
            let (preview, preview_truncated) =
                preview_line(&content, mat.start(), provisional_budget.max_preview_chars);
            results.push(json!({
                "path": file.record.path,
                "range": range,
                "matchText": mat.as_str(),
                "preview": preview,
                "previewTruncated": preview_truncated,
                "previewTruncatedReason": if preview_truncated { Value::String("output_budget_preview".to_string()) } else { Value::Null },
                "context": context_lines(&content, range["start"]["line"].as_u64().unwrap_or(1) as usize, context, provisional_budget.max_preview_chars),
                "fileHash": file.record.hash,
                "language": file.record.language,
                "producer": file.source.text_search_producer(refs_mode),
                "indexFresh": file.source.index_fresh(),
                "sourceReason": file.source.reason(),
                "reliability": "source_fact",
                "exact": true
            }));
        }
    }
    let budget = output_budget(
        repository_files,
        repository_bytes,
        results.len(),
        opts.limit,
        context,
    );
    apply_output_budget(&mut results, &budget);
    paged_query_output(
        results,
        source.index,
        opts,
        broad_guard(if refs_mode { "refs" } else { "find" }, pattern, Some(mode)),
        budget,
        &pagination_scope(
            if refs_mode { "refs" } else { "find" },
            json!({ "pattern": pattern, "mode": mode, "context": context }),
            opts,
            &workspace.snapshot_id,
        ),
    )
}

pub(crate) fn annotate_identifier_refs_with_definitions(
    results: Value,
    identifier: &str,
    definitions: &[SymbolRange],
) -> Value {
    let Value::Array(values) = results else {
        return results;
    };
    Value::Array(
        values
            .into_iter()
            .map(|value| annotate_identifier_ref(value, identifier, definitions))
            .collect(),
    )
}

fn annotate_identifier_ref(value: Value, identifier: &str, definitions: &[SymbolRange]) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    let role = if is_definition_hit(&object, definitions) {
        "definition"
    } else {
        "reference_candidate"
    };
    object
        .entry("name".to_string())
        .or_insert_with(|| Value::String(identifier.to_string()));
    object
        .entry("symbolName".to_string())
        .or_insert_with(|| Value::String(identifier.to_string()));
    object
        .entry("kind".to_string())
        .or_insert_with(|| Value::String("unknown".to_string()));
    object
        .entry("language".to_string())
        .or_insert_with(|| Value::String("text".to_string()));
    object.entry("container".to_string()).or_insert(Value::Null);
    object.insert("role".to_string(), Value::String(role.to_string()));
    object
        .entry("fallbackReason".to_string())
        .or_insert_with(|| Value::String("precise_scip_index_unavailable".to_string()));
    Value::Object(object)
}

fn is_definition_hit(object: &serde_json::Map<String, Value>, definitions: &[SymbolRange]) -> bool {
    let Some(path) = object.get("path").and_then(Value::as_str) else {
        return false;
    };
    let Some(range) = object.get("range") else {
        return false;
    };
    definitions
        .iter()
        .any(|definition| definition.contains(path, range))
}

#[derive(Clone, Debug)]
pub(crate) struct SymbolRange {
    path: String,
    start: (u64, u64),
    end: (u64, u64),
}

impl SymbolRange {
    fn contains(&self, path: &str, range: &Value) -> bool {
        if self.path != path {
            return false;
        }
        let Some(candidate) = symbol_range(path, range) else {
            return false;
        };
        self.start <= candidate.start && candidate.end <= self.end
    }
}

pub(crate) fn symbol_range(path: &str, range: &Value) -> Option<SymbolRange> {
    Some(SymbolRange {
        path: path.to_string(),
        start: (
            range.pointer("/start/line")?.as_u64()?,
            range.pointer("/start/column")?.as_u64()?,
        ),
        end: (
            range.pointer("/end/line")?.as_u64()?,
            range.pointer("/end/column")?.as_u64()?,
        ),
    })
}

pub fn changed(workspace: &Workspace) -> Result<Value> {
    Ok(serde_json::to_value(&workspace.changed)?)
}

pub fn changed_summary(workspace: &Workspace) -> Value {
    let staged_count = workspace.changed.iter().filter(|item| item.staged).count();
    let unstaged_count = workspace
        .changed
        .iter()
        .filter(|item| item.unstaged)
        .count();
    let untracked_count = workspace
        .changed
        .iter()
        .filter(|item| item.untracked)
        .count();
    json!({
        "base": &workspace.head,
        "head": &workspace.head,
        "worktree": &workspace.snapshot_id,
        "dirty": workspace.dirty,
        "stagedCount": staged_count,
        "unstagedCount": unstaged_count,
        "untrackedCount": untracked_count,
        "changedCount": workspace.changed.len()
    })
}

pub fn status(workspace: &Workspace) -> Value {
    json!({
        "root": workspace.root,
        "gitRoot": workspace.git_root,
        "head": workspace.head,
        "dirty": workspace.dirty,
        "stagedCount": workspace.staged_count,
        "worktreeCount": workspace.worktree_count,
        "snapshot_id": workspace.snapshot_id,
        "producer": "git_status_filesystem",
        "reliability": "source_fact",
        "exact": true
    })
}

pub fn line_range_for_node(
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
) -> Value {
    json!({
        "start": { "line": start_row + 1, "column": start_col + 1 },
        "end": { "line": end_row + 1, "column": end_col + 1 }
    })
}

fn collect_tree(
    workspace: &Workspace,
    opts: &ScanOptions,
    base: &Path,
    _level: usize,
    max_depth: Option<usize>,
    results: &mut Vec<Value>,
) -> Result<()> {
    let walker_max_depth = max_depth.map(|depth| depth + 1);
    for entry in browse_entries(workspace, opts, base, walker_max_depth)? {
        if opts.limit > 0 && results.len() >= opts.limit {
            return Ok(());
        }
        let Ok(metadata) = fs::metadata(&entry.path) else {
            continue;
        };
        if path_matches_output_filters(workspace, &entry.path, opts) {
            results.push(json!({
                "path": workspace.rel_path(&entry.path),
                "kind": if metadata.is_dir() { "directory" } else { "file" },
                "depth": entry.depth,
                "size": if metadata.is_file() { metadata.len() } else { 0 },
                "language": if metadata.is_file() { language_for_path(&entry.path) } else { "directory" },
                "producer": "filesystem",
                "reliability": "source_fact",
                "exact": true
            }));
        }
    }
    Ok(())
}

fn resolve_workspace_dir(workspace: &Workspace, rel_dir: &str) -> Result<PathBuf> {
    let base = workspace.abs_path(rel_dir);
    if !base.exists() {
        return Err(anyhow!("directory does not exist: {rel_dir}"));
    }
    let canonical = dunce::canonicalize(&base)
        .with_context(|| format!("failed to resolve path {}", base.display()))?;
    if !canonical.starts_with(&workspace.root) {
        return Err(anyhow!("path escapes workspace root: {rel_dir}"));
    }
    if !canonical.is_dir() {
        return Err(anyhow!("path is not a directory: {rel_dir}"));
    }
    Ok(canonical)
}

struct BrowseEntry {
    path: PathBuf,
    depth: usize,
}

fn browse_entries(
    workspace: &Workspace,
    opts: &ScanOptions,
    base: &Path,
    max_depth: Option<usize>,
) -> Result<Vec<BrowseEntry>> {
    let mut builder = WalkBuilder::new(base);
    builder
        .current_dir(&workspace.root)
        .hidden(!opts.hidden)
        .ignore(!opts.no_ignore)
        .git_ignore(!opts.no_ignore)
        .git_global(!opts.no_ignore)
        .git_exclude(!opts.no_ignore)
        .parents(!opts.no_ignore)
        .max_depth(max_depth)
        .sort_by_file_path(|left, right| left.cmp(right));

    let mut entries = Vec::new();
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.path() == base {
            continue;
        }
        if should_skip_browse_path(&workspace.root, entry.path(), opts.no_ignore) {
            continue;
        }
        entries.push(BrowseEntry {
            path: entry.path().to_path_buf(),
            depth: entry.depth().saturating_sub(1),
        });
    }
    Ok(entries)
}

fn should_skip_browse_path(root: &Path, path: &Path, no_ignore: bool) -> bool {
    rel_path(root, path).split('/').any(|component| {
        matches!(component, ".git" | ".codetrail")
            || (!no_ignore && matches!(component, "target" | "node_modules" | "dist" | ".next"))
    })
}

fn path_matches_output_filters(workspace: &Workspace, path: &Path, opts: &ScanOptions) -> bool {
    matches_filters(&workspace.rel_path(path), &opts.include, &opts.exclude)
}

fn rel_path(root: &Path, path: &Path) -> String {
    crate::path_compat::relative_path(root, path)
}

fn paged_query_output(
    results: Vec<Value>,
    index: Value,
    opts: &ScanOptions,
    guard: Option<BroadGuard>,
    budget: OutputBudget,
    scope: &str,
) -> Result<QueryOutput> {
    let page = page_results_vec(results, opts, guard, scope)?;
    Ok(QueryOutput {
        results: page.results,
        index,
        truncated: page.truncated,
        next_cursor: page.next_cursor,
        facets: page.facets,
        guard: page.guard,
        budget: budget.to_value(),
    })
}

pub fn page_results(
    results: Value,
    opts: &ScanOptions,
    kind: &str,
    args: Value,
    snapshot_id: &str,
) -> Result<Page> {
    let Value::Array(results) = results else {
        return Ok(Page {
            results,
            truncated: false,
            next_cursor: None,
            facets: result_facets(&[]),
            guard: None,
        });
    };
    page_results_vec(
        results,
        opts,
        None,
        &pagination_scope(kind, args, opts, snapshot_id),
    )
}

fn page_results_vec(
    mut results: Vec<Value>,
    opts: &ScanOptions,
    guard: Option<BroadGuard>,
    scope: &str,
) -> Result<Page> {
    sort_results(&mut results);
    let result_set_hash = value_hash(&Value::Array(results.clone()));
    let scope = format!("{scope}|resultSet:{result_set_hash}");
    let facets = result_facets(&results);
    if let Some(guard) = guard {
        if !opts.allow_broad && results.len() > BROAD_SAMPLE_LIMIT {
            return Ok(guarded_page(results, facets, guard));
        }
    }
    let offset = cursor_offset(opts.cursor.as_deref(), &scope)?;
    let total = results.len();
    let limit = opts.limit;
    let end = if limit == 0 {
        total
    } else {
        offset.saturating_add(limit).min(total)
    };
    let page = if offset >= total {
        Vec::new()
    } else {
        results[offset..end].to_vec()
    };
    let truncated = limit > 0 && end < total;
    let next_cursor = truncated.then(|| encode_cursor(end, &scope));
    Ok(Page {
        results: Value::Array(page),
        truncated,
        next_cursor,
        facets,
        guard: None,
    })
}

fn guarded_page(results: Vec<Value>, facets: Value, guard: BroadGuard) -> Page {
    let total = results.len();
    let matched_files = results
        .iter()
        .filter_map(|result| result.get("path").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let sample: Vec<Value> = results.into_iter().take(BROAD_SAMPLE_LIMIT).collect();
    let suppressed = total.saturating_sub(sample.len());
    let guard_value = json!({
        "triggered": true,
        "reason": guard.reason,
        "estimatedMatches": total,
        "matchedFiles": matched_files,
        "sampleLimit": BROAD_SAMPLE_LIMIT,
        "suppressedResults": suppressed,
        "facets": facets,
        "sampleResults": sample,
        "nextActions": [
            {
                "kind": "narrow_query",
                "reason": guard.suggestion
            },
            {
                "kind": "allow_broad",
                "reason": "rerun with --allow-broad and an explicit --limit to page through full results"
            }
        ]
    });
    Page {
        results: guard_value["sampleResults"].clone(),
        truncated: suppressed > 0,
        next_cursor: None,
        facets: guard_value["facets"].clone(),
        guard: Some(guard_value),
    }
}

impl OutputBudget {
    fn to_value(&self) -> Value {
        json!({
            "tier": self.tier,
            "maxResults": self.max_results,
            "maxPreviewChars": self.max_preview_chars,
            "maxContextLines": self.max_context_lines,
            "reason": self.reason,
            "repository": {
                "files": self.repository_files,
                "bytes": self.repository_bytes
            },
            "estimatedMatches": self.estimated_matches
        })
    }
}

fn output_budget(
    repository_files: usize,
    repository_bytes: u64,
    estimated_matches: usize,
    requested_limit: usize,
    requested_context: u16,
) -> OutputBudget {
    let large = repository_files > MEDIUM_WORKSPACE_FILE_LIMIT
        || repository_bytes > MEDIUM_WORKSPACE_BYTES
        || estimated_matches > LARGE_HIT_LIMIT;
    let medium = repository_files > SMALL_WORKSPACE_FILE_LIMIT
        || repository_bytes > SMALL_WORKSPACE_BYTES
        || estimated_matches > MEDIUM_HIT_LIMIT;

    if large {
        return OutputBudget {
            tier: "large",
            max_results: requested_limit,
            max_preview_chars: LARGE_PREVIEW_CHARS,
            max_context_lines: requested_context,
            reason: "large_workspace_or_high_hits",
            repository_files,
            repository_bytes,
            estimated_matches,
        };
    }

    if medium {
        return OutputBudget {
            tier: "medium",
            max_results: requested_limit,
            max_preview_chars: MEDIUM_PREVIEW_CHARS,
            max_context_lines: requested_context,
            reason: "medium_workspace_or_hit_count",
            repository_files,
            repository_bytes,
            estimated_matches,
        };
    }

    OutputBudget {
        tier: "small",
        max_results: requested_limit,
        max_preview_chars: MAX_PREVIEW_CHARS,
        max_context_lines: requested_context,
        reason: "small_workspace_low_hits",
        repository_files,
        repository_bytes,
        estimated_matches,
    }
}

fn apply_output_budget(results: &mut [Value], budget: &OutputBudget) {
    let max_context_result_lines = if budget.max_context_lines == 0 {
        0
    } else {
        usize::from(budget.max_context_lines) * 2 + 1
    };
    for result in results {
        apply_preview_budget(result, budget.max_preview_chars);
        apply_context_budget(result, max_context_result_lines, budget.max_preview_chars);
    }
}

fn apply_preview_budget(result: &mut Value, max_chars: usize) {
    let Some(preview) = result.get("preview").and_then(Value::as_str) else {
        return;
    };
    let (truncated, changed) = truncate_preview(preview, max_chars);
    let existing_truncated = result
        .get("previewTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if changed {
        result["preview"] = Value::String(truncated);
        result["previewTruncated"] = Value::Bool(true);
        result["previewTruncatedReason"] = Value::String("output_budget_preview".to_string());
        mark_result_truncated(result, "output_budget_preview");
    } else if existing_truncated && result.get("previewTruncatedReason").is_none() {
        result["previewTruncatedReason"] = Value::String("output_budget_preview".to_string());
        mark_result_truncated(result, "output_budget_preview");
    }
}

fn apply_context_budget(result: &mut Value, max_lines: usize, max_chars: usize) {
    let match_line = result
        .pointer("/range/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let mut context_truncated = false;
    let mut text_truncated = false;
    let Some(context) = result.get_mut("context").and_then(Value::as_array_mut) else {
        return;
    };

    let original_len = context.len();
    if max_lines == 0 {
        if original_len > 0 {
            context.clear();
            context_truncated = true;
        }
    } else if original_len > max_lines {
        let mut selected = Vec::new();
        if let Some(line) = context
            .iter()
            .find(|line| line.get("line").and_then(Value::as_u64) == Some(match_line))
            .cloned()
        {
            selected.push(line);
        }
        for line in context.iter().cloned() {
            if selected.len() >= max_lines {
                break;
            }
            if line.get("line").and_then(Value::as_u64) != Some(match_line) {
                selected.push(line);
            }
        }
        *context = selected;
        context_truncated = true;
    }

    for line in context {
        if let Some(text) = line.get("text").and_then(Value::as_str) {
            let (truncated, changed) = truncate_preview(text, max_chars);
            if changed {
                line["text"] = Value::String(truncated);
                line["truncated"] = Value::Bool(true);
                line["truncatedReason"] = Value::String("output_budget_preview".to_string());
                text_truncated = true;
            }
        }
    }

    if context_truncated || text_truncated {
        result["contextTruncated"] = Value::Bool(true);
        result["contextTruncatedReason"] = Value::String("output_budget_context".to_string());
        mark_result_truncated(result, "output_budget_context");
    }
}

fn mark_result_truncated(result: &mut Value, reason: &str) {
    result["truncated"] = Value::Bool(true);
    if result.get("truncatedReason").is_none() || result["truncatedReason"].is_null() {
        result["truncatedReason"] = Value::String(reason.to_string());
    }
}

fn broad_guard(kind: &str, pattern: &str, mode: Option<&str>) -> Option<BroadGuard> {
    let trimmed = pattern.trim();
    if matches!(kind, "files" | "glob") && matches!(trimmed, "" | "*" | "**" | "**/*" | ".*") {
        return Some(BroadGuard {
            reason: "broad_path_pattern",
            suggestion: "add --include/--exclude or use a stricter glob/path substring",
        });
    }
    if mode == Some("regex") && matches!(trimmed, ".*" | ".+" | "^.*$" | "^.+$") {
        return Some(BroadGuard {
            reason: "broad_regex_pattern",
            suggestion: "use a more specific regex or add --include/--lang/--changed scope",
        });
    }
    if matches!(kind, "find" | "refs") && is_broad_literal(trimmed) {
        return Some(BroadGuard {
            reason: "broad_literal_pattern",
            suggestion: "search a more specific token or add --include/--lang/--changed scope",
        });
    }
    None
}

fn is_broad_literal(pattern: &str) -> bool {
    matches!(
        pattern,
        "" | "*"
            | "public"
            | "private"
            | "protected"
            | "class"
            | "function"
            | "fn"
            | "let"
            | "const"
            | "var"
            | "if"
            | "for"
            | "while"
            | "return"
            | "import"
            | "export"
    ) || pattern.chars().count() <= 1
}

fn cursor_offset(cursor: Option<&str>, scope: &str) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let Some(rest) = cursor.strip_prefix("v1:") else {
        return Err(anyhow!("invalid cursor: {cursor}"));
    };
    let Some((cursor_scope_hash, offset)) = rest.split_once(':') else {
        return Err(anyhow!("invalid cursor: {cursor}"));
    };
    if cursor_scope_hash != scope_hash(scope) {
        return Err(anyhow!("cursor does not match query scope"));
    }
    offset
        .parse::<usize>()
        .with_context(|| format!("invalid cursor: {cursor}"))
}

fn encode_cursor(offset: usize, scope: &str) -> String {
    format!("v1:{}:{offset}", scope_hash(scope))
}

fn scope_hash(scope: &str) -> String {
    let digest = Sha256::digest(scope.as_bytes());
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn value_hash(value: &Value) -> String {
    let serialized = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(&serialized);
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn pagination_scope(kind: &str, args: Value, opts: &ScanOptions, snapshot_id: &str) -> String {
    json!({
        "kind": kind,
        "args": args,
        "snapshotId": snapshot_id,
        "scope": pagination_scope_value(opts)
    })
    .to_string()
}

pub fn scope_value(opts: &ScanOptions) -> Value {
    json!({
        "include": &opts.include,
        "exclude": &opts.exclude,
        "lang": &opts.lang,
        "changed": opts.changed,
        "hidden": opts.hidden,
        "noIgnore": opts.no_ignore,
        "cursor": &opts.cursor,
        "allowBroad": opts.allow_broad,
        "limit": opts.limit
    })
}

fn pagination_scope_value(opts: &ScanOptions) -> Value {
    json!({
        "include": &opts.include,
        "exclude": &opts.exclude,
        "lang": &opts.lang,
        "changed": opts.changed,
        "hidden": opts.hidden,
        "noIgnore": opts.no_ignore,
        "allowBroad": opts.allow_broad,
        "limit": opts.limit
    })
}

fn sort_results(results: &mut [Value]) {
    results.sort_by_key(result_sort_key);
}

fn result_sort_key(value: &Value) -> (String, u64, u64, String) {
    (
        value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        value
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        value
            .pointer("/range/start/column")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        value
            .get("matchText")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

fn result_facets(results: &[Value]) -> Value {
    json!({
        "language": count_facet(results, |result| result.get("language").and_then(Value::as_str).map(ToString::to_string)),
        "topDir": count_facet(results, |result| {
            result.get("path").and_then(Value::as_str).map(|path| {
                path.split('/').next().filter(|value| !value.is_empty()).unwrap_or(".").to_string()
            })
        }),
        "fileType": count_facet(results, |result| {
            result.get("path").and_then(Value::as_str).map(|path| {
                Path::new(path)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .filter(|ext| !ext.is_empty())
                    .unwrap_or("none")
                    .to_string()
            })
        }),
        "producer": count_facet(results, |result| result.get("producer").and_then(Value::as_str).map(ToString::to_string)),
        "reliability": count_facet(results, |result| result.get("reliability").and_then(Value::as_str).map(ToString::to_string))
    })
}

fn count_facet(results: &[Value], value_for: impl Fn(&Value) -> Option<String>) -> Value {
    let mut counts = std::collections::BTreeMap::<String, u64>::new();
    for result in results {
        if let Some(value) = value_for(result) {
            *counts.entry(value).or_default() += 1;
        }
    }
    Value::Array(
        counts
            .into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect(),
    )
}

struct CandidateFiles {
    records: Vec<TextFileEntry>,
    index: Value,
}

struct CandidateFileCatalog {
    records: Vec<FileEntry>,
    index: Value,
}

struct FileEntry {
    path: String,
    language: String,
    size: u64,
    hash: Option<String>,
    source: CandidateSource,
}

struct TextFileEntry {
    record: FileRecord,
    source: CandidateSource,
}

#[derive(Clone, Copy, Debug)]
enum CandidateSource {
    IndexedFresh,
    IndexedUnverified,
    LiveOverlay,
    LiveScan,
}

impl CandidateSource {
    fn file_catalog_producer(self) -> &'static str {
        match self {
            CandidateSource::IndexedFresh | CandidateSource::IndexedUnverified => {
                "text_index_file_catalog"
            }
            CandidateSource::LiveOverlay | CandidateSource::LiveScan => "live_file_catalog",
        }
    }

    fn text_search_producer(self, refs_mode: bool) -> &'static str {
        match (refs_mode, self) {
            (true, CandidateSource::IndexedFresh | CandidateSource::IndexedUnverified) => {
                "text_index_identifier_boundary_search"
            }
            (true, CandidateSource::LiveOverlay | CandidateSource::LiveScan) => {
                "identifier_boundary_text_search"
            }
            (false, CandidateSource::IndexedFresh | CandidateSource::IndexedUnverified) => {
                "text_index_live_text_search"
            }
            (false, CandidateSource::LiveOverlay | CandidateSource::LiveScan) => "live_text_search",
        }
    }

    fn index_fresh(self) -> bool {
        matches!(self, CandidateSource::IndexedFresh)
    }

    fn reason(self) -> &'static str {
        match self {
            CandidateSource::IndexedFresh => "indexed_fresh",
            CandidateSource::IndexedUnverified => "indexed_unverified",
            CandidateSource::LiveOverlay => "per_file_live_overlay",
            CandidateSource::LiveScan => "live_scan",
        }
    }
}

impl FileEntry {
    fn indexed(record: FileRecord, source: CandidateSource) -> Self {
        Self {
            path: record.path,
            language: record.language,
            size: record.size,
            hash: Some(record.hash),
            source,
        }
    }

    fn live(record: FileCatalogRecord, source: CandidateSource) -> Self {
        Self {
            path: record.path,
            language: record.language,
            size: record.size,
            hash: None,
            source,
        }
    }
}

impl TextFileEntry {
    fn indexed(record: FileRecord, source: CandidateSource) -> Self {
        Self { record, source }
    }

    fn live(record: FileRecord, source: CandidateSource) -> Self {
        Self { record, source }
    }
}

fn indexed_candidate_source(index: &Value) -> CandidateSource {
    if index.get("source").and_then(Value::as_str) == Some("text_index:remote")
        && !index
            .get("remote_verified")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        CandidateSource::IndexedUnverified
    } else {
        CandidateSource::IndexedFresh
    }
}

fn candidate_file_catalog(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<CandidateFileCatalog> {
    if !opts.changed {
        if let Some(indexed) = index::indexed_file_records(workspace, opts)? {
            let indexed_source = indexed_candidate_source(&indexed.index);
            let mut records = filter_file_entries(
                indexed
                    .records
                    .into_iter()
                    .map(|record| FileEntry::indexed(record, indexed_source))
                    .collect(),
                opts,
            );
            if !indexed.overlay_paths.is_empty() || !indexed.missing_paths.is_empty() {
                records.extend(live_file_overlay(
                    workspace,
                    opts,
                    &indexed.indexed_paths,
                    &indexed.overlay_paths,
                )?);
                records.sort_by(|a, b| a.path.cmp(&b.path));
            }
            return Ok(CandidateFileCatalog {
                records,
                index: indexed.index,
            });
        }
    }

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(CandidateFileCatalog {
        records: workspace
            .scan_catalog(&scan_opts)?
            .into_iter()
            .map(|record| FileEntry::live(record, CandidateSource::LiveScan))
            .collect(),
        index: live_scan_index_with_summary(workspace, &scan_opts)?,
    })
}

fn candidate_text_files(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    mode: &str,
) -> Result<CandidateFiles> {
    if !opts.changed {
        if let Some(indexed) = index::indexed_text_records(workspace, opts, pattern, mode)? {
            let indexed_source = indexed_candidate_source(&indexed.index);
            let mut records = filter_records(indexed.records, opts)
                .into_iter()
                .map(|record| TextFileEntry::indexed(record, indexed_source))
                .collect::<Vec<_>>();
            if !indexed.overlay_paths.is_empty() || !indexed.missing_paths.is_empty() {
                records.extend(live_text_overlay(
                    workspace,
                    opts,
                    &indexed.indexed_paths,
                    &indexed.overlay_paths,
                )?);
                records.sort_by(|a, b| a.record.path.cmp(&b.record.path));
            }
            return Ok(CandidateFiles {
                records,
                index: indexed.index,
            });
        }
    }

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(CandidateFiles {
        records: workspace
            .scan_files(&scan_opts)?
            .into_iter()
            .map(|record| TextFileEntry::live(record, CandidateSource::LiveScan))
            .collect(),
        index: live_scan_index_with_summary(workspace, &scan_opts)?,
    })
}

fn live_scan_index_with_summary(workspace: &Workspace, opts: &ScanOptions) -> Result<Value> {
    let mut index = index::live_scan_index_meta("index_missing_or_stale");
    index["scanSummary"] = workspace.scan_summary(opts)?;
    Ok(index)
}

fn live_file_overlay(
    workspace: &Workspace,
    opts: &ScanOptions,
    indexed_paths: &BTreeSet<String>,
    overlay_paths: &BTreeSet<String>,
) -> Result<Vec<FileEntry>> {
    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(workspace
        .scan_catalog(&scan_opts)?
        .into_iter()
        .filter(|record| {
            overlay_paths.contains(&record.path) || !indexed_paths.contains(&record.path)
        })
        .map(|record| FileEntry::live(record, CandidateSource::LiveOverlay))
        .collect())
}

fn live_text_overlay(
    workspace: &Workspace,
    opts: &ScanOptions,
    indexed_paths: &BTreeSet<String>,
    overlay_paths: &BTreeSet<String>,
) -> Result<Vec<TextFileEntry>> {
    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(workspace
        .scan_files(&scan_opts)?
        .into_iter()
        .filter(|record| {
            overlay_paths.contains(&record.path) || !indexed_paths.contains(&record.path)
        })
        .map(|record| TextFileEntry::live(record, CandidateSource::LiveOverlay))
        .collect())
}

fn filter_records(records: Vec<FileRecord>, opts: &ScanOptions) -> Vec<FileRecord> {
    records
        .into_iter()
        .filter(|record| {
            !opts
                .exclude
                .iter()
                .any(|pattern| record.path.contains(pattern))
                && (opts.include.is_empty()
                    || opts
                        .include
                        .iter()
                        .any(|pattern| record.path.contains(pattern)))
                && matches_lang(&record.language, &opts.lang)
        })
        .collect()
}

fn filter_file_entries(records: Vec<FileEntry>, opts: &ScanOptions) -> Vec<FileEntry> {
    records
        .into_iter()
        .filter(|record| {
            !opts
                .exclude
                .iter()
                .any(|pattern| record.path.contains(pattern))
                && (opts.include.is_empty()
                    || opts
                        .include
                        .iter()
                        .any(|pattern| record.path.contains(pattern)))
                && matches_lang(&record.language, &opts.lang)
        })
        .collect()
}

fn preview_line(content: &str, byte: usize, max_chars: usize) -> (String, bool) {
    let start = content[..byte].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let end = content[byte..]
        .find('\n')
        .map(|idx| byte + idx)
        .unwrap_or(content.len());
    truncate_preview(content[start..end].trim_end(), max_chars)
}

fn context_lines(content: &str, line: usize, context: u16, max_chars: usize) -> Value {
    if context == 0 {
        return Value::Array(Vec::new());
    }
    let lines: Vec<&str> = content.lines().collect();
    let context = usize::from(context);
    let start = line.saturating_sub(context + 1);
    let end = (line + context).min(lines.len());
    let values = lines[start..end]
        .iter()
        .enumerate()
        .map(|(idx, text)| {
            let (text, truncated) = truncate_preview(text, max_chars);
            json!({
                "line": start + idx + 1,
                "text": text,
                "truncated": truncated,
                "truncatedReason": if truncated { Value::String("output_budget_preview".to_string()) } else { Value::Null }
            })
        })
        .collect();
    Value::Array(values)
}

fn truncate_preview(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let truncated = text.chars().take(max_chars).collect::<String>();
    (format!("{truncated}..."), true)
}

fn byte_range_to_line_range(content: &str, start: usize, end: usize) -> Value {
    let (start_line, start_col) = line_col(content, start);
    let (end_line, end_col) = line_col(content, end);
    json!({
        "start": { "line": start_line, "column": start_col },
        "end": { "line": end_line, "column": end_col }
    })
}

fn line_col(content: &str, byte: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (idx, ch) in content.char_indices() {
        if idx >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn line_end_column(line: &str) -> usize {
    line.chars().count() + 1
}

fn identifier_boundary(content: &str, start: usize, end: usize) -> bool {
    let before = content[..start].chars().next_back();
    let after = content[end..].chars().next();
    !is_ident_char(before) && !is_ident_char(after)
}

fn is_ident_char(value: Option<char>) -> bool {
    value
        .map(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        .unwrap_or(false)
}

struct ReadTarget {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    has_explicit_range: bool,
}

impl ReadTarget {
    fn parse(target: &str) -> Result<Self> {
        let Some((path, range)) = target.rsplit_once(':') else {
            return Ok(Self {
                path: target.to_string(),
                start_line: None,
                end_line: None,
                has_explicit_range: false,
            });
        };
        if path.is_empty() || !range.chars().all(|ch| ch.is_ascii_digit() || ch == '-') {
            return Ok(Self {
                path: target.to_string(),
                start_line: None,
                end_line: None,
                has_explicit_range: false,
            });
        }
        let (start_line, end_line) = range.split_once('-').map_or_else(
            || {
                let line = parse_line(range)?;
                Ok((line, line))
            },
            |(start, end)| {
                let start = parse_line(start)?;
                let end = parse_line(end)?;
                if start > end {
                    return Err(anyhow!("invalid line range: {start}-{end}"));
                }
                Ok((start, end))
            },
        )?;
        Ok(Self {
            path: path.to_string(),
            start_line: Some(start_line),
            end_line: Some(end_line),
            has_explicit_range: true,
        })
    }
}

fn parse_line(value: &str) -> Result<usize> {
    let line = value
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid line range: {value}"))?;
    if line == 0 {
        return Err(anyhow!("invalid line range: {value}"));
    }
    Ok(line)
}

struct FileFacts {
    hash: String,
    binary: bool,
}

struct ReadContent {
    content: String,
    start_line: usize,
    end_line: usize,
    end_column: usize,
    truncated: bool,
}

fn scan_file_facts(path: &Path, display_path: &str) -> Result<FileFacts> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to read {display_path}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut binary = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {display_path}"))?;
        if read == 0 {
            break;
        }
        if buffer[..read].contains(&0) {
            binary = true;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(FileFacts {
        hash: format!("blake3:{}", hasher.finalize().to_hex()),
        binary,
    })
}

fn read_full_text(path: &Path, display_path: &str) -> Result<ReadContent> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {display_path}"))?;
    let end_line = line_count_for_content(&content);
    let end_column = last_line_end_column(&content);
    Ok(ReadContent {
        content,
        start_line: 1,
        end_line,
        end_column,
        truncated: false,
    })
}

fn read_prefix(path: &Path, display_path: &str, max_bytes: usize) -> Result<ReadContent> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to read {display_path}"))?;
    let mut bytes = Vec::with_capacity(max_bytes);
    file.by_ref()
        .take(max_bytes as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {display_path}"))?;
    while std::str::from_utf8(&bytes).is_err() && !bytes.is_empty() {
        bytes.pop();
    }
    let content =
        String::from_utf8(bytes).with_context(|| format!("failed to read {display_path}"))?;
    let end_line = line_count_for_content(&content);
    let end_column = last_line_end_column(&content);
    Ok(ReadContent {
        content,
        start_line: 1,
        end_line,
        end_column,
        truncated: true,
    })
}

fn read_line_range(
    path: &Path,
    display_path: &str,
    start_line: usize,
    requested_end_line: usize,
) -> Result<ReadContent> {
    let file = fs::File::open(path).with_context(|| format!("failed to read {display_path}"))?;
    let reader = BufReader::new(file);
    let mut selected = Vec::new();
    let mut total_lines = 0;
    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = line.with_context(|| format!("failed to read {display_path}"))?;
        total_lines = line_no;
        if line_no >= start_line && line_no <= requested_end_line {
            selected.push(line);
        }
    }

    if selected.is_empty() && start_line > total_lines && total_lines > 0 {
        return Err(anyhow!(
            "invalid line range: {start_line}-{requested_end_line}"
        ));
    }

    let content = selected.join("\n");
    let end_line = if selected.is_empty() {
        start_line
    } else {
        start_line + selected.len() - 1
    };
    let end_column = selected
        .last()
        .map(|line| line_end_column(line))
        .unwrap_or(1);
    Ok(ReadContent {
        content,
        start_line,
        end_line,
        end_column,
        truncated: false,
    })
}

fn line_count_for_content(content: &str) -> usize {
    let count = content.lines().count();
    if count == 0 {
        1
    } else {
        count
    }
}

fn last_line_end_column(content: &str) -> usize {
    content.lines().last().map(line_end_column).unwrap_or(1)
}
