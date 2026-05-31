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
    let mut warnings = warnings;
    let no_match_supported = supports_no_match(command, canonical_command);
    if no_match_supported && results.as_array().is_some_and(Vec::is_empty) {
        warnings.push("no_match: query returned zero results; absence is not proven".to_string());
    }
    let suggested_reads = suggested_reads(&results);
    let next_actions = next_actions_from_results(&results);
    let summary = response_summary(&results, &warnings, &index);
    let mut value = json!({
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
    });
    if no_match_supported {
        attach_no_match(&mut value);
    }
    attach_ambiguity(&mut value);
    value
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

pub fn with_guard(mut value: Value, guard: Option<Value>) -> Value {
    let Some(guard) = guard else {
        return value;
    };
    if guard.get("triggered").and_then(Value::as_bool) == Some(true) {
        if let Some(warnings) = value.get_mut("warnings").and_then(Value::as_array_mut) {
            let reason = guard
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("broad_query");
            warnings.push(json!({
                "code": "broad_query_guard_triggered",
                "message": format!("broad query guard triggered: {reason}")
            }));
        }
    }
    value["guard"] = guard;
    value
}

pub fn with_budget(mut value: Value, budget: Value) -> Value {
    value["budget"] = budget;
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
        "budget": value.get("budget").cloned().unwrap_or(Value::Null),
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

    if value.pointer("/guard/triggered").and_then(Value::as_bool) == Some(true) {
        let reason = value
            .pointer("/guard/reason")
            .and_then(Value::as_str)
            .unwrap_or("broad_query");
        let suppressed = value
            .pointer("/guard/suppressedResults")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        writeln!(
            out,
            "warning: broad query guard triggered ({reason}); suppressed {suppressed} results"
        )?;
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
    if let Some(actions) = value.get_mut("nextActions").and_then(Value::as_array_mut) {
        let root = root.to_string_lossy().to_string();
        for action in actions {
            enrich_action_with_root(action, &root);
        }
    }
    let suggested_reads = suggested_reads(&value["results"]);
    let mut next_actions = non_read_next_actions(&value["nextActions"]);
    next_actions.extend(
        next_actions_from_results(&value["results"])
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    value["suggestedReads"] = suggested_reads;
    value["nextActions"] = Value::Array(next_actions);
    value
}

fn enrich_action_with_root(action: &mut Value, root: &str) {
    if action.get("kind").and_then(Value::as_str) == Some("read") {
        return;
    }
    let Some(argv) = action.get("argv").and_then(Value::as_array) else {
        return;
    };
    let mut argv: Vec<String> = argv
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();
    if argv.first().map(String::as_str) != Some("code-search") || has_path_arg(&argv) {
        return;
    }
    argv.insert(1, "--path".to_string());
    argv.insert(2, root.to_string());
    action["argv"] = json!(argv);
    action["command"] = Value::String(command_string_from_argv(&action["argv"]));
}

fn has_path_arg(argv: &[String]) -> bool {
    argv.iter()
        .any(|arg| arg == "--path" || arg.starts_with("--path="))
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
    let full_content_truncated = object.contains_key("content")
        && object.get("truncated").and_then(Value::as_bool) == Some(true);
    if object.get("binary").and_then(Value::as_bool) == Some(true) || full_content_truncated {
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

fn non_read_next_actions(actions: &Value) -> Vec<Value> {
    actions
        .as_array()
        .into_iter()
        .flatten()
        .filter(|action| action.get("kind").and_then(Value::as_str) != Some("read"))
        .cloned()
        .collect()
}

fn supports_no_match(command: &str, canonical_command: &str) -> bool {
    matches!(
        command,
        "find"
            | "grep"
            | "files"
            | "find-path"
            | "glob"
            | "refs"
            | "defs"
            | "symbols"
            | "calls"
            | "callers"
    ) || matches!(
        canonical_command,
        "find" | "files" | "refs" | "defs" | "symbols" | "calls" | "callers"
    )
}

fn attach_no_match(value: &mut Value) {
    if value
        .get("results")
        .and_then(Value::as_array)
        .is_none_or(|results| !results.is_empty())
    {
        return;
    }
    let query = value.get("query").cloned().unwrap_or_else(|| json!({}));
    value["noMatch"] = json!({
        "reason": "no_results",
        "command": value.get("command").cloned().unwrap_or(Value::Null),
        "canonicalCommand": value.get("canonicalCommand").cloned().unwrap_or(Value::Null),
        "query": query,
        "scope": value.pointer("/query/scope").cloned().unwrap_or_else(|| json!({})),
        "index": {
            "used": value.pointer("/index/used").cloned().unwrap_or(Value::Null),
            "fresh": value.pointer("/index/fresh").cloned().unwrap_or(Value::Null),
            "fallback": value.pointer("/index/fallback").cloned().unwrap_or(Value::Null)
        }
    });
    append_next_actions(value, no_match_next_actions(value));
}

fn no_match_next_actions(value: &Value) -> Vec<Value> {
    let command = value.get("command").and_then(Value::as_str).unwrap_or("");
    let canonical = value
        .get("canonicalCommand")
        .and_then(Value::as_str)
        .unwrap_or(command);
    let query = value.get("query").unwrap_or(&Value::Null);
    let Some(term) = query_term(query) else {
        return Vec::new();
    };

    let mut actions = Vec::new();
    if canonical == "find" && query.get("mode").and_then(Value::as_str) == Some("literal") {
        actions.push(command_action(
            "try_regex",
            vec!["code-search", "grep", term],
            "try the same text as a regex search",
        ));
    }
    if matches!(canonical, "find" | "defs" | "refs" | "symbols") {
        actions.push(command_action(
            "search_paths",
            vec!["code-search", "files", term],
            "check whether the token appears in paths before widening content search",
        ));
    }
    if matches!(canonical, "defs" | "symbols") {
        actions.push(command_action(
            "try_definitions",
            vec!["code-search", "defs", term],
            "search definitions before choosing an implementation site",
        ));
    }
    if value.pointer("/index/fallback").and_then(Value::as_bool) == Some(true) {
        actions.push(command_action(
            "update_index",
            vec!["code-search", "index", "update"],
            "refresh the local index before retrying the query",
        ));
    }
    actions
}

fn attach_ambiguity(value: &mut Value) {
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let canonical = value
        .get("canonicalCommand")
        .and_then(Value::as_str)
        .unwrap_or(&command)
        .to_string();
    if !matches!(canonical.as_str(), "defs" | "symbols") {
        return;
    }
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return;
    };
    let results = results.clone();
    if results.len() < 2 {
        return;
    }
    let query = value.get("query").unwrap_or(&Value::Null);
    let Some(term) = query_term(query).map(ToString::to_string) else {
        return;
    };
    let same_name = results
        .iter()
        .filter(|result| result.get("name").and_then(Value::as_str) == Some(term.as_str()))
        .count();
    if same_name < 2 {
        return;
    }

    value["ambiguity"] = json!({
        "triggered": true,
        "reason": "multiple_symbol_candidates",
        "candidateCount": same_name,
        "groups": {
            "language": group_counts(&results, "language"),
            "kind": group_counts(&results, "kind"),
            "topDir": top_dir_counts(&results),
            "container": group_counts(&results, "container")
        }
    });
    push_structured_warning(
        value,
        "ambiguous_results",
        "multiple symbol candidates matched; narrow by path, language, kind, or inspect definitions",
    );

    let mut actions = Vec::new();
    if let Some(prefix) = results.iter().find_map(result_parent_dir) {
        actions.push(command_action(
            "narrow_scope",
            vec!["code-search", "--include", &prefix, &command, &term],
            "rerun with a path scope that selects one candidate group",
        ));
    }
    if canonical != "defs" {
        actions.push(command_action(
            "inspect_definitions",
            vec!["code-search", "defs", &term],
            "inspect definitions before choosing a candidate",
        ));
    }
    append_next_actions(value, actions);
}

fn append_next_actions(value: &mut Value, actions: Vec<Value>) {
    if actions.is_empty() {
        return;
    }
    if let Some(existing) = value.get_mut("nextActions").and_then(Value::as_array_mut) {
        existing.extend(actions);
    } else {
        value["nextActions"] = Value::Array(actions);
    }
}

fn push_structured_warning(value: &mut Value, code: &str, message: &str) {
    let warning = json!({ "code": code, "message": message });
    if let Some(warnings) = value.get_mut("warnings").and_then(Value::as_array_mut) {
        warnings.push(warning);
    } else {
        value["warnings"] = json!([warning]);
    }
}

fn query_term(query: &Value) -> Option<&str> {
    query
        .get("pattern")
        .or_else(|| query.get("identifier"))
        .or_else(|| query.get("query"))
        .and_then(Value::as_str)
}

fn command_action(kind: &str, argv: Vec<&str>, reason: &str) -> Value {
    let argv = argv
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let command = command_string_from_parts(&argv);
    json!({
        "kind": kind,
        "command": command,
        "argv": argv,
        "reason": reason
    })
}

fn command_string_from_argv(argv: &Value) -> String {
    let parts = argv
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    command_string_from_parts(&parts)
}

fn command_string_from_parts(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn group_counts(results: &[Value], field: &str) -> Value {
    let mut counts = std::collections::BTreeMap::<String, u64>::new();
    for result in results {
        if let Some(value) = result.get(field).and_then(Value::as_str) {
            *counts.entry(value.to_string()).or_default() += 1;
        }
    }
    Value::Array(
        counts
            .into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect(),
    )
}

fn top_dir_counts(results: &[Value]) -> Value {
    let mut counts = std::collections::BTreeMap::<String, u64>::new();
    for result in results {
        if let Some(path) = result.get("path").and_then(Value::as_str) {
            let top_dir = path.split('/').next().unwrap_or(path);
            *counts.entry(top_dir.to_string()).or_default() += 1;
        }
    }
    Value::Array(
        counts
            .into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect(),
    )
}

fn result_parent_dir(result: &Value) -> Option<String> {
    let path = result.get("path").and_then(Value::as_str)?;
    path.rsplit_once('/').map(|(dir, _)| dir.to_string())
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
    if message.starts_with("no_match: ") {
        return "no_match".to_string();
    }
    if message.starts_with("precise_scip_index_unavailable") {
        return "precise_scip_index_unavailable".to_string();
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
