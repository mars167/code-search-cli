use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::{
    cli::ReplaySnapshot,
    output,
    query::{QueryOptions, QueryService},
    workspace::Workspace,
};

const SAVED_QUERY_KIND: &str = "code_search_saved_query";

pub fn save_from_response(workspace: &Workspace, name: &str, response: &Value) -> Result<Value> {
    let path = query_path(workspace, name)?;
    let query = response
        .get("query")
        .cloned()
        .ok_or_else(|| anyhow!("response is missing query metadata"))?;
    let snapshot_id = response
        .get("snapshot_id")
        .and_then(Value::as_str)
        .unwrap_or(&workspace.snapshot_id)
        .to_string();
    let command = response
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("response is missing command metadata"))?;
    if !is_replayable_command(command) {
        return Err(anyhow!(
            "command cannot be saved for query replay: {command}"
        ));
    }

    let saved = json!({
        "schemaVersion": output::SCHEMA_VERSION,
        "kind": SAVED_QUERY_KIND,
        "name": name,
        "savedAtUnix": unix_timestamp(),
        "command": command,
        "canonicalCommand": response.get("canonicalCommand").cloned().unwrap_or(Value::Null),
        "query": query,
        "scope": response.pointer("/query/scope").cloned().unwrap_or_else(|| json!({})),
        "snapshotId": snapshot_id,
        "requestCursor": response.pointer("/query/scope/cursor").cloned().unwrap_or(Value::Null),
        "nextCursor": response.get("nextCursor").cloned().unwrap_or(Value::Null)
    });

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_vec_pretty(&saved)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(saved_query_meta(workspace, name, &path, &saved, "save"))
}

pub fn replay(workspace: &Workspace, name: &str, mode: &ReplaySnapshot) -> Result<Value> {
    let (path, saved) = load_with_path(workspace, name)?;
    let saved_snapshot = saved
        .get("snapshotId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let snapshot_match = saved_snapshot == workspace.snapshot_id;
    if matches!(mode, ReplaySnapshot::Saved) && !snapshot_match {
        return Err(anyhow!(
            "saved query snapshot mismatch: saved {saved_snapshot}, current {}; rerun with --snapshot current to replay against the current workspace",
            workspace.snapshot_id
        ));
    }

    let service = QueryService::new(&workspace.root)?;
    let opts = query_options_from_saved(&saved, snapshot_match)?;
    let command = saved
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("saved query is missing command"))?;
    let query = saved
        .get("query")
        .ok_or_else(|| anyhow!("saved query is missing query"))?;

    let mut value = match command {
        "find" | "grep" => {
            let default_mode = if command == "grep" {
                "regex"
            } else {
                "literal"
            };
            let mode = query
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or(default_mode);
            let context = query
                .get("context")
                .and_then(Value::as_u64)
                .unwrap_or(opts.context as u64) as u16;
            service.text_search(
                command,
                required_str(query, "pattern")?,
                mode,
                context,
                &opts,
            )?
        }
        "files" | "find-path" => service.files(required_str(query, "pattern")?, &opts)?,
        "glob" => service.glob(required_str(query, "pattern")?, &opts)?,
        "refs" => service.refs(required_str(query, "identifier")?, &opts)?,
        "defs" => service.defs(required_str(query, "identifier")?, &opts)?,
        "symbols" => service.symbols(required_str(query, "query")?, &opts)?,
        "calls" => service.calls(required_str(query, "identifier")?, &opts)?,
        "callers" => service.callers(required_str(query, "identifier")?, &opts)?,
        other => return Err(anyhow!("saved query command is not replayable: {other}")),
    };

    value["savedQuery"] = saved_query_meta(workspace, name, &path, &saved, "replay");
    value["savedQuery"]["snapshotMode"] = Value::String(match mode {
        ReplaySnapshot::Current => "current".to_string(),
        ReplaySnapshot::Saved => "saved".to_string(),
    });
    value["savedQuery"]["snapshotMatch"] = Value::Bool(snapshot_match);
    if !snapshot_match {
        push_warning(
            &mut value,
            "saved_query_snapshot_mismatch",
            format!(
                "saved query snapshot changed: saved {saved_snapshot}, current {}; replay used current workspace state",
                workspace.snapshot_id
            ),
        );
    }
    Ok(value)
}

pub fn show(workspace: &Workspace, name: &str) -> Result<Value> {
    let (_path, saved) = load_with_path(workspace, name)?;
    Ok(saved)
}

pub fn list(workspace: &Workspace) -> Result<Value> {
    let dir = queries_dir(workspace);
    let mut entries = Vec::new();
    if !dir.exists() {
        return Ok(Value::Array(entries));
    }
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(saved) = read_saved_file(&path) else {
            continue;
        };
        let name = saved
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| path.file_stem().and_then(|value| value.to_str()))
            .unwrap_or("");
        entries.push(saved_query_meta(workspace, name, &path, &saved, "list"));
    }
    entries.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(Value::Array(entries))
}

pub fn delete(workspace: &Workspace, name: &str) -> Result<Value> {
    let path = query_path(workspace, name)?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to delete {}", path.display()))?;
    }
    Ok(json!([{
        "name": name,
        "path": workspace.rel_path(&path),
        "deleted": true,
        "producer": "saved_query_store",
        "reliability": "source_fact",
        "exact": true
    }]))
}

fn query_options_from_saved(saved: &Value, snapshot_match: bool) -> Result<QueryOptions> {
    let query = saved
        .get("query")
        .ok_or_else(|| anyhow!("saved query is missing query"))?;
    let scope = saved
        .get("scope")
        .or_else(|| query.get("scope"))
        .unwrap_or(&Value::Null);
    Ok(QueryOptions {
        include: string_array(scope.get("include")),
        exclude: string_array(scope.get("exclude")),
        lang: string_array(scope.get("lang")),
        changed: bool_field(scope, "changed"),
        hidden: bool_field(scope, "hidden"),
        no_ignore: bool_field(scope, "noIgnore"),
        cursor: snapshot_match.then(|| replay_cursor(saved)).flatten(),
        allow_broad: bool_field(scope, "allowBroad"),
        limit: scope.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize,
        context: query.get("context").and_then(Value::as_u64).unwrap_or(0) as u16,
    })
}

fn replay_cursor(saved: &Value) -> Option<String> {
    saved
        .get("nextCursor")
        .and_then(Value::as_str)
        .or_else(|| saved.get("requestCursor").and_then(Value::as_str))
        .map(ToString::to_string)
}

fn required_str<'a>(query: &'a Value, field: &str) -> Result<&'a str> {
    query
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("saved query is missing query.{field}"))
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn bool_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn load_with_path(workspace: &Workspace, name: &str) -> Result<(PathBuf, Value)> {
    let path = query_path(workspace, name)?;
    let saved = read_saved_file(&path)?;
    Ok((path, saved))
}

fn read_saved_file(path: &Path) -> Result<Value> {
    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let saved: Value = serde_json::from_slice(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if saved.get("kind").and_then(Value::as_str) != Some(SAVED_QUERY_KIND) {
        return Err(anyhow!("not a code-search saved query: {}", path.display()));
    }
    Ok(saved)
}

fn saved_query_meta(
    workspace: &Workspace,
    name: &str,
    path: &Path,
    saved: &Value,
    operation: &str,
) -> Value {
    let saved_snapshot = saved.get("snapshotId").cloned().unwrap_or(Value::Null);
    let snapshot_match = saved_snapshot.as_str() == Some(workspace.snapshot_id.as_str());
    json!({
        "name": name,
        "operation": operation,
        "path": workspace.rel_path(path),
        "command": saved.get("command").cloned().unwrap_or(Value::Null),
        "snapshotId": saved_snapshot,
        "currentSnapshotId": workspace.snapshot_id,
        "snapshotMatch": snapshot_match,
        "requestCursor": saved.get("requestCursor").cloned().unwrap_or(Value::Null),
        "nextCursor": saved.get("nextCursor").cloned().unwrap_or(Value::Null)
    })
}

fn push_warning(value: &mut Value, code: &str, message: String) {
    let warning = json!({ "code": code, "message": message });
    if let Some(warnings) = value.get_mut("warnings").and_then(Value::as_array_mut) {
        warnings.push(warning);
    } else {
        value["warnings"] = json!([warning]);
    }
}

fn query_path(workspace: &Workspace, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(queries_dir(workspace).join(format!("{name}.json")))
}

fn queries_dir(workspace: &Workspace) -> PathBuf {
    workspace.root.join(".code-search").join("queries")
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(anyhow!(
            "saved query name must use only letters, numbers, '.', '_' or '-'"
        ));
    }
    Ok(())
}

fn is_replayable_command(command: &str) -> bool {
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
    )
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
