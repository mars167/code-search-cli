use std::{
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

pub struct QueryOutput {
    pub results: Value,
    pub index: Value,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub facets: Value,
}

pub struct Page {
    pub results: Value,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub facets: Value,
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

    let source = candidate_file_catalog(workspace, opts)?;
    for file in source.records {
        let matches = matcher
            .as_ref()
            .map(|glob| glob.is_match(&file.path))
            .unwrap_or_else(|| file.path.contains(pattern));
        if matches {
            results.push(json!({
                "path": file.path,
                "language": file.language,
                "size": file.size,
                "hash": file.hash,
                "producer": if source.index["used"].as_bool().unwrap_or(false) { "text_index_file_catalog" } else { "live_file_catalog" },
                "reliability": "source_fact",
                "exact": true
            }));
        }
    }
    paged_query_output(
        results,
        source.index,
        opts,
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
            let metadata = fs::metadata(&entry.path)?;
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
        fs::canonicalize(&path).with_context(|| format!("failed to read {}", request.path))?;
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
    let mut results = Vec::new();
    for file in source.records {
        let path = workspace.abs_path(&file.path);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for mat in regex.find_iter(&content) {
            if refs_mode && !identifier_boundary(&content, mat.start(), mat.end()) {
                continue;
            }
            let range = byte_range_to_line_range(&content, mat.start(), mat.end());
            let (preview, preview_truncated) = preview_line(&content, mat.start());
            results.push(json!({
                "path": file.path,
                "range": range,
                "matchText": mat.as_str(),
                "preview": preview,
                "previewTruncated": preview_truncated,
                "context": context_lines(&content, range["start"]["line"].as_u64().unwrap_or(1) as usize, context),
                "fileHash": file.hash,
                "language": file.language,
                "producer": text_search_producer(refs_mode, source.index["used"].as_bool().unwrap_or(false)),
                "reliability": "source_fact",
                "exact": true
            }));
        }
    }
    paged_query_output(
        results,
        source.index,
        opts,
        &pagination_scope(
            if refs_mode { "refs" } else { "find" },
            json!({ "pattern": pattern, "mode": mode, "context": context }),
            opts,
            &workspace.snapshot_id,
        ),
    )
}

pub fn changed(workspace: &Workspace) -> Result<Value> {
    Ok(serde_json::to_value(&workspace.changed)?)
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
        let metadata = fs::metadata(&entry.path)?;
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
    let canonical = fs::canonicalize(&base)
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
        let entry = entry?;
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
        matches!(component, ".git" | ".code-search")
            || (!no_ignore && matches!(component, "target" | "node_modules" | "dist" | ".next"))
    })
}

fn path_matches_output_filters(workspace: &Workspace, path: &Path, opts: &ScanOptions) -> bool {
    matches_filters(&workspace.rel_path(path), &opts.include, &opts.exclude)
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn paged_query_output(
    results: Vec<Value>,
    index: Value,
    opts: &ScanOptions,
    scope: &str,
) -> Result<QueryOutput> {
    let page = page_results_vec(results, opts, scope)?;
    Ok(QueryOutput {
        results: page.results,
        index,
        truncated: page.truncated,
        next_cursor: page.next_cursor,
        facets: page.facets,
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
        });
    };
    page_results_vec(
        results,
        opts,
        &pagination_scope(kind, args, opts, snapshot_id),
    )
}

fn page_results_vec(mut results: Vec<Value>, opts: &ScanOptions, scope: &str) -> Result<Page> {
    sort_results(&mut results);
    let result_set_hash = value_hash(&Value::Array(results.clone()));
    let scope = format!("{scope}|resultSet:{result_set_hash}");
    let facets = result_facets(&results);
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
    })
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
        "limit": opts.limit
    })
}

fn sort_results(results: &mut [Value]) {
    results.sort_by(|left, right| result_sort_key(left).cmp(&result_sort_key(right)));
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
    records: Vec<FileRecord>,
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
}

impl From<FileRecord> for FileEntry {
    fn from(record: FileRecord) -> Self {
        Self {
            path: record.path,
            language: record.language,
            size: record.size,
            hash: Some(record.hash),
        }
    }
}

impl From<FileCatalogRecord> for FileEntry {
    fn from(record: FileCatalogRecord) -> Self {
        Self {
            path: record.path,
            language: record.language,
            size: record.size,
            hash: None,
        }
    }
}

fn candidate_file_catalog(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<CandidateFileCatalog> {
    if !opts.changed {
        if let Some((records, index)) = index::fresh_file_records(workspace, opts)? {
            return Ok(CandidateFileCatalog {
                records: filter_file_entries(
                    records.into_iter().map(FileEntry::from).collect(),
                    opts,
                ),
                index,
            });
        }
    }

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(CandidateFileCatalog {
        records: workspace
            .scan_catalog(&scan_opts)?
            .into_iter()
            .map(FileEntry::from)
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
        if let Some((records, index)) = index::fresh_text_records(workspace, opts, pattern, mode)? {
            return Ok(CandidateFiles {
                records: filter_records(records, opts),
                index,
            });
        }
    }

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(CandidateFiles {
        records: workspace.scan_files(&scan_opts)?,
        index: live_scan_index_with_summary(workspace, &scan_opts)?,
    })
}

fn live_scan_index_with_summary(workspace: &Workspace, opts: &ScanOptions) -> Result<Value> {
    let mut index = index::live_scan_index_meta("index_missing_or_stale");
    index["scanSummary"] = workspace.scan_summary(opts)?;
    Ok(index)
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

fn text_search_producer(refs_mode: bool, index_used: bool) -> &'static str {
    match (refs_mode, index_used) {
        (true, true) => "text_index_identifier_boundary_search",
        (true, false) => "identifier_boundary_text_search",
        (false, true) => "text_index_live_text_search",
        (false, false) => "live_text_search",
    }
}

fn preview_line(content: &str, byte: usize) -> (String, bool) {
    let start = content[..byte].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let end = content[byte..]
        .find('\n')
        .map(|idx| byte + idx)
        .unwrap_or(content.len());
    truncate_preview(content[start..end].trim_end())
}

fn context_lines(content: &str, line: usize, context: u16) -> Value {
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
            json!({
                "line": start + idx + 1,
                "text": truncate_preview(text).0,
                "truncated": truncate_preview(text).1
            })
        })
        .collect();
    Value::Array(values)
}

fn truncate_preview(text: &str) -> (String, bool) {
    if text.chars().count() <= MAX_PREVIEW_CHARS {
        return (text.to_string(), false);
    }
    let truncated = text.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
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
        if buffer[..read].iter().any(|byte| *byte == 0) {
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
