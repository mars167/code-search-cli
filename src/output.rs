use std::io::{self, Write};

use anyhow::Error;
use serde::Serialize;
use serde_json::{json, Value};

use crate::cli::OutputFormat;

mod caveats;
mod enrichment;
mod jsonl;
mod progress;
mod projection;
mod text;

use caveats::stable_code;
use enrichment::{
    attach_ambiguity, attach_no_match, enrich_results, next_actions_from_results, normalized_query,
    response_summary, structured_warnings, suggested_reads, supports_no_match,
};
use jsonl::render_jsonl;
use projection::public_response;
use text::render_text;

pub use enrichment::{no_match_exit, with_workspace_root};
pub use progress::ProgressIndicator;
pub use projection::public_response_value;

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
            let _ = writeln!(io::stderr(), "codetrail: {}", message.as_ref());
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

pub struct IndexedResponseParts {
    pub index: Value,
    pub results: Value,
    pub warnings: Vec<String>,
}

impl IndexedResponseParts {
    pub fn new(index: Value, results: Value, warnings: Vec<String>) -> Self {
        Self {
            index,
            results,
            warnings,
        }
    }
}

pub fn source_fact() -> Reliability {
    Reliability {
        level: "source_fact",
        source: "text_path_git_filesystem",
        exact: true,
        llm_instruction: "这些结果是可验证源码事实。修改前仍应使用 codetrail read 读取精确范围。",
    }
}

pub fn source_fact_inexact() -> Reliability {
    Reliability {
        level: "source_fact",
        source: "text_path_git_filesystem",
        exact: false,
        llm_instruction:
            "这些结果来自源码文件，但内容被省略或截断。需要使用更小范围的 codetrail read 验证。",
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
        llm_instruction: "这些结果来自 precise code intelligence index。修改前仍应使用 codetrail read 验证源码范围。",
    }
}

pub fn inferred_candidate() -> Reliability {
    Reliability {
        level: "inferred_candidate",
        source: "tree_sitter_ast_heuristic",
        exact: false,
        llm_instruction:
            "这些结果只能作为候选关系，不是完整调用图。推理前必须用 codetrail read 验证每个匹配。",
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
        IndexedResponseParts::new(live_scan_index(), results, warnings),
    )
}

pub fn response_with_index(
    command: &str,
    canonical_command: &str,
    query: Value,
    snapshot_id: &str,
    reliability: Reliability,
    parts: IndexedResponseParts,
) -> Value {
    let IndexedResponseParts {
        index,
        results,
        warnings,
    } = parts;
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
    cfg!(debug_assertions) && std::env::var_os("CODETRAIL_INTERNAL_JSON").is_some()
}
