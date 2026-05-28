use std::{fs, path::Path};

use anyhow::{anyhow, Context, Result};
use globset::Glob;
use regex::Regex;
use serde_json::{json, Value};

use crate::{
    index,
    workspace::{language_for_path, FileRecord, ScanOptions, Workspace},
};

pub struct QueryOutput {
    pub results: Value,
    pub index: Value,
}

pub fn files(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    strict_glob: bool,
) -> Result<QueryOutput> {
    let mut results = Vec::new();
    let matcher = if strict_glob || has_glob_meta(pattern) {
        Some(Glob::new(pattern)?.compile_matcher())
    } else {
        None
    };
    // Use path index prefilter when available
    let source = candidate_files(workspace, opts, Some(pattern))?;
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
        if opts.limit > 0 && results.len() >= opts.limit {
            break;
        }
    }
    Ok(QueryOutput {
        results: Value::Array(results),
        index: source.index,
    })
}

pub fn list(workspace: &Workspace, dir: Option<&str>, recursive: bool) -> Result<Value> {
    let rel_dir = dir.unwrap_or(".");
    let base = workspace.abs_path(rel_dir);
    if !base.exists() {
        return Err(anyhow!("directory does not exist: {rel_dir}"));
    }
    if !base.is_dir() {
        return Err(anyhow!("path is not a directory: {rel_dir}"));
    }

    let mut results = Vec::new();
    if recursive {
        collect_tree(workspace, &base, 0, None, &mut results)?;
    } else {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&base)? {
            let entry = entry?;
            entries.push(entry.path());
        }
        entries.sort();
        for path in entries {
            if should_hide(&path) {
                continue;
            }
            let metadata = fs::metadata(&path)?;
            results.push(json!({
                "path": workspace.rel_path(&path),
                "kind": if metadata.is_dir() { "directory" } else { "file" },
                "size": if metadata.is_file() { metadata.len() } else { 0 },
                "language": if metadata.is_file() { language_for_path(&path) } else { "directory" },
                "producer": "filesystem",
                "reliability": "source_fact",
                "exact": true
            }));
        }
    }
    Ok(Value::Array(results))
}

pub fn tree(workspace: &Workspace, dir: Option<&str>, depth: Option<u8>) -> Result<Value> {
    let rel_dir = dir.unwrap_or(".");
    let base = workspace.abs_path(rel_dir);
    if !base.exists() {
        return Err(anyhow!("directory does not exist: {rel_dir}"));
    }
    let mut results = Vec::new();
    collect_tree(workspace, &base, 0, depth.map(usize::from), &mut results)?;
    Ok(Value::Array(results))
}

pub fn read(workspace: &Workspace, target: &str) -> Result<Value> {
    let request = ReadTarget::parse(target);
    let path = workspace.abs_path(&request.path);
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", request.path))?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let start_line = request.start_line.unwrap_or(1).max(1);
    let end_line = request.end_line.unwrap_or(total_lines).min(total_lines);
    if start_line > end_line && total_lines > 0 {
        return Err(anyhow!("invalid line range: {start_line}-{end_line}"));
    }

    let selected = if total_lines == 0 {
        String::new()
    } else {
        lines[(start_line - 1)..end_line].join("\n")
    };
    let hash = format!("blake3:{}", blake3::hash(content.as_bytes()).to_hex());

    Ok(json!({
        "path": request.path,
        "range": {
            "start": { "line": start_line, "column": 1 },
            "end": { "line": end_line.max(start_line), "column": line_end_column(lines.get(end_line.saturating_sub(1)).copied().unwrap_or("")) }
        },
        "content": selected,
        "fileHash": hash,
        "language": language_for_path(&path),
        "producer": "snapshot_store_live_read",
        "reliability": "source_fact",
        "exact": true
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
            results.push(json!({
                "path": file.path,
                "range": range,
                "matchText": mat.as_str(),
                "preview": preview_line(&content, mat.start()),
                "context": context_lines(&content, range["start"]["line"].as_u64().unwrap_or(1) as usize, context),
                "fileHash": file.hash,
                "language": file.language,
                "producer": text_search_producer(refs_mode, source.index["used"].as_bool().unwrap_or(false)),
                "reliability": "source_fact",
                "exact": true
            }));
            if opts.limit > 0 && results.len() >= opts.limit {
                return Ok(QueryOutput {
                    results: Value::Array(results),
                    index: source.index,
                });
            }
        }
    }
    Ok(QueryOutput {
        results: Value::Array(results),
        index: source.index,
    })
}

pub fn changed(workspace: &Workspace) -> Result<Value> {
    let changed = crate::workspace::git_status(&workspace.root).unwrap_or_default();
    Ok(serde_json::to_value(changed)?)
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
    base: &Path,
    level: usize,
    max_depth: Option<usize>,
    results: &mut Vec<Value>,
) -> Result<()> {
    if let Some(max_depth) = max_depth {
        if level > max_depth {
            return Ok(());
        }
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        if should_hide(&entry.path()) {
            continue;
        }
        entries.push(entry.path());
    }
    entries.sort();

    for path in entries {
        let metadata = fs::metadata(&path)?;
        results.push(json!({
            "path": workspace.rel_path(&path),
            "kind": if metadata.is_dir() { "directory" } else { "file" },
            "depth": level,
            "size": if metadata.is_file() { metadata.len() } else { 0 },
            "language": if metadata.is_file() { language_for_path(&path) } else { "directory" },
            "producer": "filesystem",
            "reliability": "source_fact",
            "exact": true
        }));
        if metadata.is_dir() {
            collect_tree(workspace, &path, level + 1, max_depth, results)?;
        }
    }
    Ok(())
}

fn should_hide(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            matches!(
                name,
                ".git" | ".code-search" | "target" | "node_modules" | "dist"
            )
        })
        .unwrap_or(false)
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[') || pattern.contains('{')
}

struct CandidateFiles {
    records: Vec<FileRecord>,
    index: Value,
}

fn candidate_files(
    workspace: &Workspace,
    opts: &ScanOptions,
    path_pattern: Option<&str>,
) -> Result<CandidateFiles> {
    if let Some((records, index)) = index::fresh_file_records(workspace, opts, path_pattern)? {
        return Ok(CandidateFiles {
            records: filter_records(records, opts),
            index,
        });
    }

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(CandidateFiles {
        records: workspace.scan_files(&scan_opts)?,
        index: index::live_scan_index_meta("index_missing_or_stale"),
    })
}

fn candidate_text_files(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    mode: &str,
) -> Result<CandidateFiles> {
    if let Some((records, index)) = index::fresh_text_records(workspace, opts, pattern, mode)? {
        return Ok(CandidateFiles {
            records: filter_records(records, opts),
            index,
        });
    }

    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    Ok(CandidateFiles {
        records: workspace.scan_files(&scan_opts)?,
        index: index::live_scan_index_meta("index_missing_or_stale"),
    })
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

fn preview_line(content: &str, byte: usize) -> String {
    let start = content[..byte].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let end = content[byte..]
        .find('\n')
        .map(|idx| byte + idx)
        .unwrap_or(content.len());
    content[start..end].trim_end().to_string()
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
                "text": text
            })
        })
        .collect();
    Value::Array(values)
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
}

impl ReadTarget {
    fn parse(target: &str) -> Self {
        let Some((path, range)) = target.rsplit_once(':') else {
            return Self {
                path: target.to_string(),
                start_line: None,
                end_line: None,
            };
        };
        if path.is_empty() || !range.chars().all(|ch| ch.is_ascii_digit() || ch == '-') {
            return Self {
                path: target.to_string(),
                start_line: None,
                end_line: None,
            };
        }
        let (start_line, end_line) = range
            .split_once('-')
            .map(|(start, end)| (start.parse().ok(), end.parse().ok()))
            .unwrap_or_else(|| {
                let line = range.parse().ok();
                (line, line)
            });
        Self {
            path: path.to_string(),
            start_line,
            end_line,
        }
    }
}
