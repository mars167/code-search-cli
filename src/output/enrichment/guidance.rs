use serde_json::{json, Value};

use super::command::command_action;

pub(in crate::output) fn supports_no_match(command: &str, canonical_command: &str) -> bool {
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

pub(in crate::output) fn attach_no_match(value: &mut Value) {
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
            vec!["codetrail", "grep", term],
            "try the same text as a regex search",
        ));
    }
    if matches!(canonical, "find" | "defs" | "refs" | "symbols") {
        actions.push(command_action(
            "search_paths",
            vec!["codetrail", "files", term],
            "check whether the token appears in paths before widening content search",
        ));
    }
    if matches!(canonical, "defs" | "symbols") {
        actions.push(command_action(
            "try_definitions",
            vec!["codetrail", "defs", term],
            "search definitions before choosing an implementation site",
        ));
    }
    if value.pointer("/index/fallback").and_then(Value::as_bool) == Some(true) {
        actions.push(command_action(
            "update_index",
            vec!["codetrail", "index", "update"],
            "refresh the local index before retrying the query",
        ));
    }
    actions
}

pub(in crate::output) fn attach_ambiguity(value: &mut Value) {
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
            vec!["codetrail", "--include", &prefix, &command, &term],
            "rerun with a path scope that selects one candidate group",
        ));
    }
    if canonical != "defs" {
        actions.push(command_action(
            "inspect_definitions",
            vec!["codetrail", "defs", &term],
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
