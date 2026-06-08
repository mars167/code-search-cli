use std::path::Path;

use serde_json::{json, Value};

use super::caveats::{caveat_metadata, stable_code};

mod command;
mod guidance;

use command::{command_string_from_argv, shell_quote};
pub(super) use guidance::{attach_ambiguity, attach_no_match, supports_no_match};

pub fn no_match_exit(results: &Value) -> i32 {
    match results.as_array() {
        Some(values) if values.is_empty() => 2,
        _ => 0,
    }
}

pub(super) fn normalized_query(query: Value) -> Value {
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

pub(super) fn enrich_results(results: Value) -> Value {
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
    if argv.first().map(String::as_str) != Some("codetrail") || has_path_arg(&argv) {
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
        || path == ".codetrail"
        || path.starts_with(".codetrail/")
        || path.contains("/.codetrail/")
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

pub(super) fn suggested_reads(results: &Value) -> Value {
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

pub(super) fn next_actions_from_results(results: &Value) -> Value {
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

fn read_command_string(root: Option<&str>, target: &str) -> String {
    let read_target = if target.starts_with('-') {
        format!("-- {}", shell_quote(target))
    } else {
        shell_quote(target)
    };
    match root {
        Some(root) => format!("codetrail --path {} read {read_target}", shell_quote(root)),
        None => format!("codetrail read {read_target}"),
    }
}

fn read_argv(root: Option<String>, target: String) -> Vec<String> {
    let mut argv = vec!["codetrail".to_string()];
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

pub(super) fn structured_warnings(warnings: Vec<String>) -> Value {
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

pub(super) fn response_summary(results: &Value, warnings: &[String], index: &Value) -> Value {
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
