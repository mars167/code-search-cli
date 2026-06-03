use std::io::IsTerminal;
use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::Error;
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::OutputFormat;

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Clone, Copy, Debug)]
pub struct VerboseLogger {
    level: u8,
}

impl VerboseLogger {
    pub fn new(level: u8) -> Self {
        Self { level }
    }

    pub fn enabled(self) -> bool {
        self.level > 0
    }

    pub fn log(self, message: impl AsRef<str>) {
        if self.enabled() {
            let _ = writeln!(io::stderr(), "code-search: {}", message.as_ref());
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reliability {
    pub level: &'static str,
    pub source: &'static str,
    pub exact: bool,
    pub llm_instruction: &'static str,
}

#[derive(Debug, Serialize)]
struct PublicResponse {
    results: Value,
    page: PublicPage,
    caveats: Vec<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicPage {
    truncated: bool,
    next_cursor: Value,
}

#[derive(Debug, Serialize)]
struct ResultEvent<'a> {
    event: &'static str,
    result: &'a Value,
}

#[derive(Debug, Serialize)]
struct PageEvent {
    event: &'static str,
    page: PublicPage,
    caveats: Vec<Value>,
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
    let mut chain = error.chain();
    let message = chain
        .next()
        .map(ToString::to_string)
        .unwrap_or_else(|| "unknown error".to_string());
    let mut full_message = message.clone();
    for cause in chain {
        full_message.push_str("\ncaused by: ");
        full_message.push_str(&cause.to_string());
    }
    error_response_with_code(&stable_code(&message), full_message)
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
        OutputFormat::Json if internal_json_enabled() => {
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            serde_json::to_writer_pretty(&mut handle, value)?;
            writeln!(handle)?;
        }
        OutputFormat::Json | OutputFormat::CompactJson => {
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            serde_json::to_writer_pretty(&mut handle, &public_response(value))?;
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

fn internal_json_enabled() -> bool {
    cfg!(debug_assertions) && std::env::var_os("CODE_SEARCH_INTERNAL_JSON").is_some()
}

pub struct ProgressIndicator {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    active: bool,
}

impl ProgressIndicator {
    pub fn start(format: &OutputFormat, message: impl Into<String>) -> Self {
        if !should_show_progress(format, io::stderr().is_terminal()) {
            return Self {
                running: Arc::new(AtomicBool::new(false)),
                handle: None,
                active: false,
            };
        }

        let message = message.into();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let frames = ["-", "\\", "|", "/"];
            let mut idx = 0usize;
            while thread_running.load(Ordering::Relaxed) {
                let _ = write!(io::stderr(), "\r{} {}", frames[idx % frames.len()], message);
                let _ = io::stderr().flush();
                idx = idx.wrapping_add(1);
                thread::sleep(Duration::from_millis(120));
            }
            let _ = write!(io::stderr(), "\r{}\r", " ".repeat(message.len() + 4));
            let _ = io::stderr().flush();
        });
        Self {
            running,
            handle: Some(handle),
            active: true,
        }
    }

    pub fn finish(mut self, message: impl AsRef<str>) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.active {
            let message = message.as_ref();
            if !message.is_empty() {
                let _ = writeln!(io::stderr(), "{message}");
            }
        }
    }
}

fn should_show_progress(format: &OutputFormat, stderr_is_terminal: bool) -> bool {
    *format == OutputFormat::Text && stderr_is_terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_indicator_is_enabled_only_for_text_tty_output() {
        assert!(should_show_progress(&OutputFormat::Text, true));
        assert!(!should_show_progress(&OutputFormat::Text, false));
        assert!(!should_show_progress(&OutputFormat::Json, true));
        assert!(!should_show_progress(&OutputFormat::CompactJson, true));
        assert!(!should_show_progress(&OutputFormat::Jsonl, true));
    }
}

fn render_jsonl(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    let public = public_response(value);
    if let Some(results) = public.results.as_array() {
        for result in results {
            let event = ResultEvent {
                event: "result",
                result,
            };
            serde_json::to_writer(&mut *out, &event)?;
            writeln!(out)?;
        }
    }
    let event = PageEvent {
        event: "page",
        page: public.page,
        caveats: public.caveats,
    };
    serde_json::to_writer(&mut *out, &event)?;
    writeln!(out)?;
    Ok(())
}

fn public_response(value: &Value) -> PublicResponse {
    PublicResponse {
        results: public_results(value),
        page: PublicPage {
            truncated: public_page_truncated(value),
            next_cursor: value.get("nextCursor").cloned().unwrap_or(Value::Null),
        },
        caveats: public_caveats(value),
    }
}

pub fn public_response_value(value: &Value) -> Value {
    serde_json::to_value(public_response(value)).unwrap_or_else(|_| {
        json!({
            "results": [],
            "page": {
                "truncated": false,
                "nextCursor": null
            },
            "caveats": [
                {
                    "code": "serialization_error",
                    "message": "failed to serialize public response"
                }
            ]
        })
    })
}

fn public_results(value: &Value) -> Value {
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };
    Value::Array(results.iter().map(public_result).collect())
}

fn public_result(result: &Value) -> Value {
    let Value::Object(object) = result else {
        return result.clone();
    };
    let mut object = object.clone();
    for field in [
        "fileHash",
        "readCommand",
        "readCommandArgv",
        "producer",
        "sourceReason",
        "indexFresh",
        "reliability",
        "exact",
        "knownBlindSpots",
        "fallbackReason",
        "previewTruncatedReason",
    ] {
        object.remove(field);
    }
    sanitize_public_object(&mut object);
    Value::Object(object)
}

fn sanitize_public_object(object: &mut serde_json::Map<String, Value>) {
    for value in object.values_mut() {
        sanitize_public_value(value);
    }
    object.retain(|key, value| keep_public_field(key, value));
}

fn sanitize_public_value(value: &mut Value) {
    match value {
        Value::Object(object) => sanitize_public_object(object),
        Value::Array(values) => {
            for value in values {
                sanitize_public_value(value);
            }
        }
        _ => {}
    }
}

fn keep_public_field(key: &str, value: &Value) -> bool {
    if value.is_null() {
        return false;
    }
    if matches!(key, "context" | "warnings") {
        return !value.as_array().is_some_and(Vec::is_empty);
    }
    if matches!(key, "previewTruncated" | "truncated" | "binary") {
        return value.as_bool().unwrap_or(true);
    }
    if key == "warning" {
        return value.as_str().is_some_and(|warning| !warning.is_empty());
    }
    true
}

fn public_caveats(value: &Value) -> Vec<Value> {
    let mut caveats = Vec::new();
    let mut seen = std::collections::BTreeSet::<String>::new();

    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let code = value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("error");
        let message = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        push_public_caveat_with(&mut caveats, &mut seen, code, message, "error", "error");
    }

    let guard_triggered = value.pointer("/guard/triggered").and_then(Value::as_bool) == Some(true);

    for warning in value
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let code = warning
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("warning");
        let message = warning
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(code);
        if guard_triggered && code == "broad_query_guard_triggered" {
            continue;
        }
        push_public_caveat(&mut caveats, &mut seen, code, message);
    }

    if let Some(level) = value.pointer("/reliability/level").and_then(Value::as_str) {
        match level {
            "parser_fact" => {
                if !seen.contains("precise_scip_index_unavailable") {
                    push_public_caveat(
                        &mut caveats,
                        &mut seen,
                        "parser_fact",
                        "parser fallback result; not semantic reference resolution",
                    );
                }
            }
            "inferred_candidate" => push_public_caveat(
                &mut caveats,
                &mut seen,
                "inferred_candidate",
                "call graph result is an inferred candidate",
            ),
            "source_fact" | "precise_fact" | "freshness" => {}
            other => push_public_caveat(
                &mut caveats,
                &mut seen,
                other,
                "result reliability is not exact",
            ),
        }
    }

    if public_output_truncated(value) {
        push_public_caveat(
            &mut caveats,
            &mut seen,
            "truncated_output",
            "output was truncated; narrow the query or increase limit/context",
        );
    }

    if guard_triggered {
        let message = broad_guard_public_message(value);
        push_public_caveat(&mut caveats, &mut seen, "broad_query_guard", &message);
    }

    caveats
}

fn push_public_caveat(
    caveats: &mut Vec<Value>,
    seen: &mut std::collections::BTreeSet<String>,
    code: &str,
    message: &str,
) {
    let (severity, category) = caveat_metadata(code);
    push_public_caveat_with(caveats, seen, code, message, severity, category);
}

fn push_public_caveat_with(
    caveats: &mut Vec<Value>,
    seen: &mut std::collections::BTreeSet<String>,
    code: &str,
    message: &str,
    severity: &str,
    category: &str,
) {
    if seen.insert(code.to_string()) {
        caveats.push(json!({
            "code": code,
            "message": message,
            "severity": severity,
            "category": category
        }));
    }
}

fn caveat_metadata(code: &str) -> (&'static str, &'static str) {
    match code {
        "precise_scip_index_unavailable"
        | "parser_fact"
        | "refs_identifier_boundary_text_search_unless_a_precise_occurrence_index_is_available"
        | "inferred_candidate" => ("info", "capability"),
        "unknown_tool" | "invalid_mcp_argument" | "unsupported_mcp_scope" | "cli_usage_error" => {
            ("error", "error")
        }
        _ => ("warning", "risk"),
    }
}

fn results_contain_truncation(value: &Value) -> bool {
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|result| {
            result.get("truncated").and_then(Value::as_bool) == Some(true)
                || result.get("previewTruncated").and_then(Value::as_bool) == Some(true)
                || result
                    .get("context")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|line| line.get("truncated").and_then(Value::as_bool) == Some(true))
        })
}

fn public_page_truncated(value: &Value) -> bool {
    if public_output_truncated(value) {
        return true;
    }
    if value.pointer("/guard/triggered").and_then(Value::as_bool) == Some(true) {
        return value
            .pointer("/guard/suppressedResults")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0;
    }
    false
}

fn public_output_truncated(value: &Value) -> bool {
    if results_contain_truncation(value) {
        return true;
    }
    let has_next_cursor = value.get("nextCursor").and_then(Value::as_str).is_some();
    let guard_triggered = value.pointer("/guard/triggered").and_then(Value::as_bool) == Some(true);
    value.get("truncated").and_then(Value::as_bool) == Some(true)
        && !has_next_cursor
        && !guard_triggered
}

fn broad_guard_public_message(value: &Value) -> String {
    let reason = value
        .pointer("/guard/reason")
        .and_then(Value::as_str)
        .unwrap_or("broad_query");
    let suppressed = value
        .pointer("/guard/suppressedResults")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if suppressed > 0 {
        format!(
            "broad query guard triggered: {reason}; showing sample results and suppressing {suppressed}; narrow the query or rerun with --allow-broad and an explicit --limit"
        )
    } else {
        format!(
            "broad query guard triggered: {reason}; narrow the query or rerun with --allow-broad and an explicit --limit"
        )
    }
}

fn render_text(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        let mut lines = message.lines();
        let first = lines.next().unwrap_or("unknown error").trim();
        writeln!(out, "error: {first}")?;
        for line in lines {
            let line = line.trim();
            if line.starts_with("caused by:") {
                writeln!(out, "  {line}")?;
            }
        }
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
        render_text_summary(value, out)?;
        render_text_results(value, out)?;
        return Ok(());
    }

    if value.get("noMatch").is_some() {
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("query");
        writeln!(out, "no matches for {command}")?;
        return Ok(());
    }

    if value
        .pointer("/ambiguity/triggered")
        .and_then(Value::as_bool)
        == Some(true)
    {
        let count = value
            .pointer("/ambiguity/candidateCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        writeln!(out, "ambiguous results: {count} candidates")?;
        render_text_facets(value.pointer("/ambiguity/groups/kind"), out, "kinds")?;
        render_text_facets(value.pointer("/ambiguity/groups/topDir"), out, "top dirs")?;
    }

    render_text_results(value, out)?;
    render_text_caveats(value, out)?;
    Ok(())
}

fn render_text_results(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some(results) = value.get("results").and_then(Value::as_array) {
        let command = value.get("command").and_then(Value::as_str).unwrap_or("");
        if matches!(command, "calls" | "callers") {
            return render_text_graph(value, results, out);
        }
        if command == "read" {
            return render_text_read(results, out);
        }
        if is_status_like(command) {
            return render_text_status_like(command, results, out);
        }
        for result in results {
            render_text_result(result, out)?;
        }
        return Ok(());
    }

    writeln!(out, "{value}")?;
    Ok(())
}

fn render_text_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        let location = format_location(path, result.get("range"));
        if let Some(name) = result
            .get("name")
            .or_else(|| result.get("symbolName"))
            .and_then(Value::as_str)
        {
            let kind = result
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("symbol");
            writeln!(out, "{kind:<12}{name}")?;
            writeln!(out, "  {location}")?;
            return Ok(());
        }
        if let Some(preview) = result.get("preview").and_then(Value::as_str) {
            writeln!(out, "{location}  {}", preview.trim())?;
            return Ok(());
        }
        writeln!(out, "{location}")?;
        return Ok(());
    }

    if let Some(path) = result.get("file").and_then(Value::as_str) {
        writeln!(out, "{path}")?;
        return Ok(());
    }

    writeln!(out, "{}", one_line_json(result))?;
    Ok(())
}

fn render_text_read(results: &[Value], out: &mut dyn Write) -> io::Result<()> {
    for (idx, result) in results.iter().enumerate() {
        if idx > 0 {
            writeln!(out)?;
        }
        let path = result.get("path").and_then(Value::as_str).unwrap_or("read");
        if result.get("binary").and_then(Value::as_bool) == Some(true) {
            writeln!(out, "{path}: binary file not displayed")?;
            continue;
        }
        if let Some(content) = result.get("content").and_then(Value::as_str) {
            write!(out, "{content}")?;
            if !content.ends_with('\n') {
                writeln!(out)?;
            }
        } else {
            writeln!(out, "{}", format_location(path, result.get("range")))?;
        }
    }
    Ok(())
}

fn render_text_graph(value: &Value, results: &[Value], out: &mut dyn Write) -> io::Result<()> {
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("calls");
    let identifier = value
        .pointer("/query/identifier")
        .and_then(Value::as_str)
        .unwrap_or("symbol");
    let title = if command == "callers" {
        format!("Callers of \"{identifier}\" ({})", results.len())
    } else {
        format!("Callees of \"{identifier}\" ({})", results.len())
    };
    writeln!(out, "{title}")?;
    if results.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    for result in results {
        let caller = result
            .get("enclosingSymbol")
            .and_then(Value::as_str)
            .map(display_symbol)
            .unwrap_or_else(|| identifier.to_string());
        let callee = result
            .get("target")
            .and_then(Value::as_str)
            .map(display_symbol)
            .unwrap_or_else(|| identifier.to_string());
        let path = result.get("path").and_then(Value::as_str).unwrap_or("");
        let location = if path.is_empty() {
            String::new()
        } else {
            format_location(path, result.get("range"))
        };
        if location.is_empty() {
            writeln!(out, "{caller} -> {callee}")?;
        } else {
            writeln!(out, "{caller} -> {callee}")?;
            writeln!(out, "  {location}")?;
        }
    }
    Ok(())
}

fn render_text_status_like(
    command: &str,
    results: &[Value],
    out: &mut dyn Write,
) -> io::Result<()> {
    for result in results {
        match command {
            "status" => {
                let root = result.get("root").and_then(Value::as_str).unwrap_or("");
                if !root.is_empty() {
                    writeln!(out, "Workspace: {root}")?;
                }
                if let Some(head) = result.get("head").and_then(Value::as_str) {
                    writeln!(out, "Head: {head}")?;
                }
                let dirty = result
                    .get("dirty")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let staged = result
                    .get("stagedCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let worktree = result
                    .get("worktreeCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                writeln!(out, "Dirty: {dirty} (staged {staged}, worktree {worktree})")?;
            }
            "index status" | "index verify" => render_index_status_result(result, out)?,
            "index build" | "index update" => render_index_build_result(result, out)?,
            "index import-scip" => render_index_import_result(result, out)?,
            "index pack" => render_index_pack_result(result, out)?,
            "index unpack" => render_index_unpack_result(result, out)?,
            "index clean" => render_index_clean_result(result, out)?,
            _ => writeln!(out, "{}", one_line_json(result))?,
        }
    }
    Ok(())
}

fn render_index_status_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let exists = result.get("exists").and_then(Value::as_bool);
    let fresh = result.get("fresh").and_then(Value::as_bool);
    if let Some(exists) = exists {
        writeln!(out, "Index exists: {exists}")?;
    }
    if let Some(fresh) = fresh {
        writeln!(out, "Index fresh: {fresh}")?;
    }
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    if let Some(file_count) = result
        .pointer("/manifest/fileCount")
        .and_then(Value::as_u64)
    {
        writeln!(out, "Files: {file_count}")?;
    }
    if let Some(reason) = result.get("reason").and_then(Value::as_str) {
        writeln!(out, "Reason: {reason}")?;
    }
    Ok(())
}

fn render_index_build_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let index = result.get("index").unwrap_or(result);
    if result.get("updated").and_then(Value::as_bool) == Some(false) {
        writeln!(out, "Index already fresh")?;
    }
    if let Some(file_count) = index.get("fileCount").and_then(Value::as_u64) {
        writeln!(out, "Indexed {file_count} files")?;
    }
    if let Some(storage) = index.get("storageBackend").and_then(Value::as_str) {
        writeln!(out, "Backend: {storage}")?;
    }
    if let Some(path) = index.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    if index.get("fileCount").is_none() {
        writeln!(out, "{}", one_line_json(result))?;
    }
    Ok(())
}

fn render_index_import_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let index = result.get("index").unwrap_or(result);
    let record_count = index
        .get("recordCount")
        .or_else(|| index.get("definitionCount"))
        .and_then(Value::as_u64);
    if let Some(record_count) = record_count {
        writeln!(out, "Imported {record_count} SCIP records")?;
    } else {
        writeln!(out, "Imported SCIP index")?;
    }
    if let Some(source) = index.get("source").and_then(Value::as_str) {
        writeln!(out, "Source: {source}")?;
    }
    if let Some(path) = index.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    Ok(())
}

fn render_index_pack_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let output_path = result
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("archive");
    let entry_count = result
        .get("entryCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let archive_size = result
        .get("archiveSize")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    writeln!(out, "Packed index to {output_path}")?;
    if entry_count > 0 || archive_size > 0 {
        writeln!(out, "Entries: {entry_count}, bytes: {archive_size}")?;
    }
    Ok(())
}

fn render_index_unpack_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    if let Some(snapshot_id) = result.get("remote_snapshot_id").and_then(Value::as_str) {
        writeln!(out, "Unpacked remote snapshot {snapshot_id}")?;
    } else {
        writeln!(out, "Unpacked remote snapshot")?;
    }
    if let Some(remote_dir) = result.get("remoteDir").and_then(Value::as_str) {
        writeln!(out, "Path: {remote_dir}")?;
    }
    if let Some(entry_count) = result.get("entryCount").and_then(Value::as_u64) {
        writeln!(out, "Entries: {entry_count}")?;
    }
    Ok(())
}

fn render_index_clean_result(result: &Value, out: &mut dyn Write) -> io::Result<()> {
    let cleaned = result
        .get("cleaned")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    writeln!(out, "Index cleaned: {cleaned}")?;
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        writeln!(out, "Path: {path}")?;
    }
    Ok(())
}

fn is_status_like(command: &str) -> bool {
    matches!(
        command,
        "status"
            | "index status"
            | "index verify"
            | "index build"
            | "index update"
            | "index import-scip"
            | "index pack"
            | "index unpack"
            | "index clean"
    )
}

fn render_text_caveats(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    let caveats = public_caveats(value);
    let filtered = caveats
        .iter()
        .filter(|caveat| {
            !matches!(
                caveat.get("code").and_then(Value::as_str),
                Some("no_match" | "broad_query_guard_triggered")
            )
        })
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    for caveat in filtered {
        let code = caveat
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("caveat");
        let message = caveat
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(code);
        writeln!(out, "caveat: {code}: {message}")?;
    }
    Ok(())
}

fn format_location(path: &str, range: Option<&Value>) -> String {
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

fn display_symbol(symbol: &str) -> String {
    let symbol = symbol.trim();
    if symbol.contains("::") {
        return symbol.to_string();
    }
    symbol
        .rsplit(['.', '/', '#'])
        .find(|part| !part.is_empty())
        .unwrap_or(symbol)
        .trim_start_matches("function")
        .trim_start_matches('-')
        .to_string()
}

fn one_line_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn render_text_summary(value: &Value, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "summary:")?;
    if let Some(matches) = value
        .pointer("/guard/estimatedMatches")
        .and_then(Value::as_u64)
    {
        writeln!(out, "  estimated matches: {matches}")?;
    }
    if let Some(files) = value.pointer("/guard/matchedFiles").and_then(Value::as_u64) {
        writeln!(out, "  matched files: {files}")?;
    }
    render_text_facets(
        value.pointer("/summary/facets/language"),
        out,
        "top languages",
    )?;
    render_text_facets(value.pointer("/summary/facets/topDir"), out, "top dirs")?;
    Ok(())
}

fn render_text_facets(facets: Option<&Value>, out: &mut dyn Write, label: &str) -> io::Result<()> {
    let Some(values) = facets.and_then(Value::as_array) else {
        return Ok(());
    };
    if values.is_empty() {
        return Ok(());
    }
    let rendered = values
        .iter()
        .take(5)
        .filter_map(|facet| {
            let value = facet.get("value").and_then(Value::as_str)?;
            let count = facet.get("count").and_then(Value::as_u64)?;
            Some(format!("{value}={count}"))
        })
        .collect::<Vec<_>>();
    if !rendered.is_empty() {
        writeln!(out, "  {label}: {}", rendered.join(", "))?;
    }
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

fn structured_warnings(warnings: Vec<String>) -> Value {
    Value::Array(
        warnings
            .into_iter()
            .map(|message| {
                let code = stable_code(&message);
                let (severity, category) = caveat_metadata(&code);
                json!({
                    "code": code,
                    "message": message,
                    "severity": severity,
                    "category": category
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
    if message.starts_with("failed to parse native SCIP index ") {
        return "failed_to_parse_native_scip_index".to_string();
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
