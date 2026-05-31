use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::Error;
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::OutputFormat;

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reliability {
    pub level: &'static str,
    pub source: &'static str,
    pub exact: bool,
    pub llm_instruction: &'static str,
}

pub fn source_fact() -> Reliability {
    Reliability {
        level: "source_fact",
        source: "text_path_git_filesystem",
        exact: true,
        llm_instruction: "这些结果是可验证源码事实。修改前仍应使用 code-search read 读取精确范围。",
    }
}

pub fn source_fact_inexact() -> Reliability {
    Reliability {
        level: "source_fact",
        source: "text_path_git_filesystem",
        exact: false,
        llm_instruction:
            "这些结果来自源码文件，但内容被省略或截断。需要使用更小范围的 code-search read 验证。",
    }
}

pub fn parser_fact() -> Reliability {
    Reliability {
        level: "parser_fact",
        source: "tree_sitter_ast",
        exact: false,
        llm_instruction:
            "这些结果是 parser fact，不能等同于 precise semantic reference resolution。",
    }
}

pub fn precise_fact() -> Reliability {
    Reliability {
        level: "precise_fact",
        source: "scip_occurrence_index",
        exact: true,
        llm_instruction: "这些结果来自 precise code intelligence index。修改前仍应使用 code-search read 验证源码范围。",
    }
}

pub fn inferred_candidate() -> Reliability {
    Reliability {
        level: "inferred_candidate",
        source: "tree_sitter_ast_heuristic",
        exact: false,
        llm_instruction:
            "这些结果只能作为候选关系，不是完整调用图。推理前必须用 code-search read 验证每个匹配。",
    }
}

pub fn freshness() -> Reliability {
    Reliability {
        level: "freshness",
        source: "index_manifest_git_status",
        exact: false,
        llm_instruction: "这些结果描述缓存新鲜度和 watcher 状态，不提升代码事实准确性。",
    }
}

pub fn response(
    command: &str,
    canonical_command: &str,
    query: Value,
    snapshot_id: &str,
    reliability: Reliability,
    results: Value,
    warnings: Vec<String>,
) -> Value {
    response_with_index(
        command,
        canonical_command,
        query,
        snapshot_id,
        reliability,
        live_scan_index(),
        results,
        warnings,
    )
}

pub fn response_with_index(
    command: &str,
    canonical_command: &str,
    query: Value,
    snapshot_id: &str,
    reliability: Reliability,
    index: Value,
    results: Value,
    warnings: Vec<String>,
) -> Value {
    let query = normalized_query(query);
    let results = enrich_results(results);
    let suggested_reads = suggested_reads(&results);
    let next_actions = next_actions_from_results(&results);
    let summary = response_summary(&results, &warnings, &index);
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "ok": true,
        "command": command,
        "canonicalCommand": canonical_command,
        "query": query,
        "snapshot_id": snapshot_id,
        "reliability": reliability,
        "index": index,
        "truncated": false,
        "nextCursor": Value::Null,
        "summary": summary,
        "results": results,
        "suggestedReads": suggested_reads,
        "nextActions": next_actions,
        "warnings": structured_warnings(warnings)
    })
}

pub fn with_page_meta(
    mut value: Value,
    truncated: bool,
    next_cursor: Option<String>,
    facets: Value,
) -> Value {
    value["truncated"] = Value::Bool(truncated);
    value["nextCursor"] = next_cursor.map(Value::String).unwrap_or(Value::Null);
    if let Some(summary) = value.get_mut("summary").and_then(Value::as_object_mut) {
        summary.insert("facets".to_string(), facets);
    }
    value
}

pub fn with_summary_field(mut value: Value, field: &str, field_value: Value) -> Value {
    if let Some(summary) = value.get_mut("summary").and_then(Value::as_object_mut) {
        summary.insert(field.to_string(), field_value);
    }
    value
}

fn live_scan_index() -> Value {
    json!({
        "used": false,
        "fresh": false,
        "fallback": true,
        "reason": "live_scan"
    })
}

pub fn error_response(error: Error) -> Value {
    let message = error.to_string();
    error_response_with_code(&stable_code(&message), message)
}

pub fn error_response_with_code(code: &str, message: impl Into<String>) -> Value {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "ok": false,
        "truncated": false,
        "nextCursor": Value::Null,
        "warnings": [],
        "suggestedReads": [],
        "nextActions": [],
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

pub fn emit(format: &OutputFormat, value: &Value) -> io::Result<()> {
    match format {
        OutputFormat::Json => {
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            serde_json::to_writer_pretty(&mut handle, value)?;
            writeln!(handle)?;
        }
        OutputFormat::CompactJson => {
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            serde_json::to_writer_pretty(&mut handle, &compact_value(value))?;
            writeln!(handle)?;
        }
        OutputFormat::Jsonl => {
            let mut handle = io::stdout().lock();
            render_jsonl(value, &mut handle)?;
        }
        OutputFormat::Text => {
            let mut handle = io::stdout().lock();
            render_text(value, &mut handle)?;
        }
    }
    Ok(())
}

fn render_jsonl(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let event = json!({
            "schemaVersion": value.get("schemaVersion").cloned().unwrap_or_else(|| json!(SCHEMA_VERSION)),
            "event": "error",
            "ok": false,
            "truncated": value.get("truncated").cloned().unwrap_or_else(|| json!(false)),
            "nextCursor": value.get("nextCursor").cloned().unwrap_or(Value::Null),
            "warnings": value.get("warnings").cloned().unwrap_or_else(|| json!([])),
            "suggestedReads": value.get("suggestedReads").cloned().unwrap_or_else(|| json!([])),
            "nextActions": value.get("nextActions").cloned().unwrap_or_else(|| json!([])),
            "error": value.get("error").cloned().unwrap_or_else(|| json!({ "code": "error", "message": "unknown error" }))
        });
        serde_json::to_writer(&mut *out, &event)?;
        writeln!(out)?;
        return Ok(());
    }

    let result_count = value
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            for result in results {
                let event = json!({
                    "schemaVersion": value.get("schemaVersion").cloned().unwrap_or_else(|| json!(SCHEMA_VERSION)),
                    "event": "result",
                    "result": result
                });
                serde_json::to_writer(&mut *out, &event)?;
                writeln!(out)?;
            }
            Ok::<usize, io::Error>(results.len())
        })
        .transpose()?
        .unwrap_or(0);

    let mut summary = json!({
        "schemaVersion": value.get("schemaVersion").cloned().unwrap_or_else(|| json!(SCHEMA_VERSION)),
        "event": "summary",
        "ok": true,
        "command": value.get("command").cloned().unwrap_or(Value::Null),
        "canonicalCommand": value.get("canonicalCommand").cloned().unwrap_or(Value::Null),
        "snapshot_id": value.get("snapshot_id").cloned().unwrap_or(Value::Null),
        "truncated": value.get("truncated").cloned().unwrap_or_else(|| json!(false)),
        "nextCursor": value.get("nextCursor").cloned().unwrap_or(Value::Null),
        "resultCount": result_count,
        "warnings": value.get("warnings").cloned().unwrap_or_else(|| json!([])),
        "suggestedReads": value.get("suggestedReads").cloned().unwrap_or_else(|| json!([])),
        "nextActions": value.get("nextActions").cloned().unwrap_or_else(|| json!([]))
    });
    if let Some(summary_value) = value.get("summary") {
        summary["summary"] = summary_value.clone();
    }
    serde_json::to_writer(&mut *out, &summary)?;
    writeln!(out)?;
    Ok(())
}

fn render_text(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        writeln!(
            out,
            "error: {}",
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        )?;
        return Ok(());
    }

    if let Some(results) = value.get("results").and_then(Value::as_array) {
        for result in results {
            if let Some(path) = result.get("path").and_then(Value::as_str) {
                if let Some(range) = result.get("range") {
                    let line = range
                        .pointer("/start/line")
                        .and_then(Value::as_u64)
                        .unwrap_or(1);
                    writeln!(out, "{path}:{line}")?;
                } else {
                    writeln!(out, "{path}")?;
                }
            } else {
                writeln!(out, "{result}")?;
            }
        }
        return Ok(());
    }

    writeln!(out, "{value}")?;
    Ok(())
}

pub fn no_match_exit(results: &Value) -> i32 {
    match results.as_array() {
        Some(values) if values.is_empty() => 2,
        _ => 0,
    }
}

fn normalized_query(query: Value) -> Value {
    match query {
        Value::Object(mut object) => {
            object
                .entry("normalized")
                .or_insert_with(|| Value::Bool(true));
            Value::Object(object)
        }
        other => json!({
            "normalized": true,
            "value": other
        }),
    }
}

fn enrich_results(results: Value) -> Value {
    let Value::Array(values) = results else {
        return results;
    };

    Value::Array(values.into_iter().map(enrich_result).collect())
}

fn enrich_result(result: Value) -> Value {
    let Value::Object(mut object) = result else {
        return result;
    };
    if is_readable_path_result(&object) && !object.contains_key("readCommand") {
        if let Some(path) = object.get("path").and_then(Value::as_str) {
            let target = read_target(path, object.get("range"));
            object.insert(
                "readCommand".to_string(),
                Value::String(read_command_string(None, &target)),
            );
            object.insert(
                "readCommandArgv".to_string(),
                json!(read_argv(None, target)),
            );
        }
    }
    Value::Object(object)
}

pub fn with_workspace_root(mut value: Value, root: &Path) -> Value {
    if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
        for result in results {
            enrich_result_with_root(result, root);
        }
    }
    let suggested_reads = suggested_reads(&value["results"]);
    let next_actions = next_actions_from_results(&value["results"]);
    value["suggestedReads"] = suggested_reads;
    value["nextActions"] = next_actions;
    value
}

fn enrich_result_with_root(result: &mut Value, root: &Path) {
    let Value::Object(object) = result else {
        return;
    };
    if !is_readable_path_result(object) {
        object.remove("readCommand");
        object.remove("readCommandArgv");
        return;
    }
    let Some(path) = object.get("path").and_then(Value::as_str) else {
        return;
    };
    let target = read_target(path, object.get("range"));
    let root = root.to_string_lossy().to_string();
    object.insert(
        "readCommand".to_string(),
        Value::String(read_command_string(Some(&root), &target)),
    );
    object.insert(
        "readCommandArgv".to_string(),
        json!(read_argv(Some(root), target)),
    );
}

fn is_readable_path_result(object: &serde_json::Map<String, Value>) -> bool {
    let Some(path) = object.get("path").and_then(Value::as_str) else {
        return false;
    };
    if path.starts_with('/')
        || path == ".code-search"
        || path.starts_with(".code-search/")
        || path.contains("/.code-search/")
    {
        return false;
    }
    if object.get("indexStatus").and_then(Value::as_str) == Some("D")
        || object.get("worktreeStatus").and_then(Value::as_str) == Some("D")
    {
        return false;
    }
    if object.get("binary").and_then(Value::as_bool) == Some(true)
        || object.get("truncated").and_then(Value::as_bool) == Some(true)
    {
        return false;
    }
    object.get("kind").and_then(Value::as_str) != Some("directory")
}

fn read_target(path: &str, range: Option<&Value>) -> String {
    let Some(range) = range else {
        return path.to_string();
    };
    let start = range
        .pointer("/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let end = range
        .pointer("/end/line")
        .and_then(Value::as_u64)
        .unwrap_or(start);
    if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

fn suggested_reads(results: &Value) -> Value {
    let reads = results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|result| result.get("readCommand").and_then(Value::as_str))
        .take(5)
        .map(|command| Value::String(command.to_string()))
        .collect();
    Value::Array(reads)
}

fn next_actions_from_results(results: &Value) -> Value {
    let actions = results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|result| {
            let command = result.get("readCommand").and_then(Value::as_str)?;
            Some(json!({
                "kind": "read",
                "command": command,
                "argv": result.get("readCommandArgv").cloned().unwrap_or(Value::Null),
                "reason": "verify_source_range_before_edit"
            }))
        })
        .take(5)
        .collect();
    Value::Array(actions)
}

fn read_command_string(root: Option<&str>, target: &str) -> String {
    let read_target = if target.starts_with('-') {
        format!("-- {}", shell_quote(target))
    } else {
        shell_quote(target)
    };
    match root {
        Some(root) => format!(
            "code-search --path {} read {read_target}",
            shell_quote(root)
        ),
        None => format!("code-search read {read_target}"),
    }
}

fn read_argv(root: Option<String>, target: String) -> Vec<String> {
    let mut argv = vec!["code-search".to_string()];
    if let Some(root) = root {
        argv.push("--path".to_string());
        argv.push(root);
    }
    argv.push("read".to_string());
    if target.starts_with('-') {
        argv.push("--".to_string());
    }
    argv.push(target);
    argv
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn compact_value(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
        for result in results {
            compact_result(result);
        }
    }
    value
}

fn compact_result(value: &mut Value) {
    if let Value::Object(object) = value {
        for field in ["preview", "context", "content", "matchText"] {
            object.remove(field);
        }
    }
}

fn structured_warnings(warnings: Vec<String>) -> Value {
    Value::Array(
        warnings
            .into_iter()
            .map(|message| {
                json!({
                    "code": stable_code(&message),
                    "message": message
                })
            })
            .collect(),
    )
}

fn response_summary(results: &Value, warnings: &[String], index: &Value) -> Value {
    let result_count = results.as_array().map(Vec::len).unwrap_or(0);
    let truncated_count = results
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| {
            result.get("truncated").and_then(Value::as_bool) == Some(true)
                || result.get("previewTruncated").and_then(Value::as_bool) == Some(true)
                || result
                    .get("context")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|line| line.get("truncated").and_then(Value::as_bool) == Some(true))
        })
        .count();
    let skipped_count = warnings
        .iter()
        .filter(|warning| {
            matches!(
                warning.as_str(),
                "binary_file_not_displayed" | "unreadable_file_skipped"
            )
        })
        .count();
    let scan_skipped_count = index
        .pointer("/scanSummary/skippedCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let scan_summary = index
        .get("scanSummary")
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "resultCount": result_count,
        "truncatedCount": truncated_count,
        "skippedCount": skipped_count as u64 + scan_skipped_count,
        "scan": scan_summary
    })
}

fn stable_code(message: &str) -> String {
    if message.starts_with("failed to read ") {
        return "read_failed".to_string();
    }
    if message.starts_with("invalid line range: ") {
        return "invalid_line_range".to_string();
    }
    if message.starts_with("path escapes workspace root: ") {
        return "path_escapes_workspace_root".to_string();
    }
    if message == "binary_file_not_displayed" {
        return "binary_file_not_displayed".to_string();
    }
    if message == "large_file_truncated" {
        return "large_file_truncated".to_string();
    }
    if message.starts_with("failed to resolve path ") {
        return "workspace_path_resolve_failed".to_string();
    }
    if message.starts_with("partial parse with syntax errors: ") {
        return "partial_parse_syntax_errors".to_string();
    }
    if message.starts_with("unsupported search mode: ") {
        return "unsupported_search_mode".to_string();
    }
    if message
        .starts_with("refs is identifier-boundary text search unless a precise occurrence index")
    {
        return "refs_identifier_boundary_text_search_unless_a_precise_occurrence_index_is_available"
            .to_string();
    }
    if let Some((prefix, _details)) = message.split_once(':') {
        return slug_code(prefix);
    }

    slug_code(message)
}

fn slug_code(message: &str) -> String {
    let mut code = String::new();
    let mut last_was_sep = false;
    for ch in message.chars() {
        if ch.is_ascii_alphanumeric() {
            code.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep && !code.is_empty() {
            code.push('_');
            last_was_sep = true;
        }
    }
    while code.ends_with('_') {
        code.pop();
    }
    if code.is_empty() {
        "warning".to_string()
    } else {
        code
    }
}
