use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    scheduler::IndexScheduler,
    snapshot_store, text_index,
    workspace::{FileRecord, ScanOptions, Workspace},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    tool_version: String,
    repo_root: String,
    snapshot_id: String,
    snapshot_key: String,
    source: String,
    head: Option<String>,
    dirty: bool,
    file_count: usize,
    scan_options: IndexScanOptions,
    created_at_epoch_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexScanOptions {
    include: Vec<String>,
    exclude: Vec<String>,
    hidden: bool,
    no_ignore: bool,
}

pub fn build(
    workspace: &Workspace,
    opts: &ScanOptions,
    staged: bool,
    _changed: bool,
    _force: bool,
) -> Result<Value> {
    let scheduler = IndexScheduler::new(&workspace.root);
    scheduler.build_all(opts, staged)
}

pub fn status(workspace: &Workspace) -> Result<Value> {
    let scheduler = IndexScheduler::new(&workspace.root);
    scheduler.status()
}

pub fn update(workspace: &Workspace) -> Result<Value> {
    let scheduler = IndexScheduler::new(&workspace.root);
    scheduler.update()
}

pub fn compact(workspace: &Workspace) -> Result<Value> {
    let scheduler = IndexScheduler::new(&workspace.root);
    scheduler.compact()
}

pub fn fresh_file_records(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    let Some((manifest, records, text_path)) = fresh_text_snapshot(workspace, opts)? else {
        return Ok(None);
    };

    Ok(Some((
        records,
        text_index_meta(&manifest, text_path, None, None),
    )))
}

pub fn fresh_text_records(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    mode: &str,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    let Some((manifest, records, text_path)) = fresh_text_snapshot(workspace, opts)? else {
        return Ok(None);
    };

    let candidate_ids = text_index::candidate_ids(&text_path.join("grams.idx"), pattern, mode)?;
    let filtered = match &candidate_ids {
        Some(ids) => records
            .into_iter()
            .enumerate()
            .filter_map(|(doc_id, record)| ids.contains(&doc_id).then_some(record))
            .collect::<Vec<_>>(),
        None => records,
    };
    let prefilter = candidate_ids.as_ref().map(|_| "trigram");
    let candidate_count = candidate_ids.as_ref().map(|ids| ids.len());

    Ok(Some((
        filtered,
        text_index_meta(&manifest, text_path, prefilter, candidate_count),
    )))
}

impl From<&ScanOptions> for IndexScanOptions {
    fn from(opts: &ScanOptions) -> Self {
        Self {
            include: opts.include.clone(),
            exclude: opts.exclude.clone(),
            hidden: opts.hidden,
            no_ignore: opts.no_ignore,
        }
    }
}

pub fn live_scan_index_meta(reason: &str) -> Value {
    json!({
        "used": false,
        "fresh": false,
        "fallback": true,
        "reason": reason
    })
}

pub fn verify(workspace: &Workspace) -> Result<(Value, i32)> {
    let value = status(workspace)?;
    let fresh = value.get("fresh").and_then(Value::as_bool).unwrap_or(false);
    Ok((value, if fresh { 0 } else { 6 }))
}

pub fn clean(workspace: &Workspace) -> Result<Value> {
    let root = storage_root(workspace);
    let existed = root.exists();
    if existed {
        fs::remove_dir_all(&root)?;
    }
    Ok(json!({
        "cleaned": existed,
        "path": root
    }))
}

pub fn hooks_install(workspace: &Workspace) -> Result<Value> {
    let Some(git_root) = &workspace.git_root else {
        return Err(anyhow!("hooks require a git repository"));
    };
    let hooks_dir = git_root.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir)?;
    let hooks = [
        (
            "pre-commit",
            "#!/bin/sh\ncode-search index build --staged >/dev/null 2>&1 || true\n",
        ),
        (
            "post-commit",
            "#!/bin/sh\ncode-search index build >/dev/null 2>&1 || true\n",
        ),
        (
            "post-checkout",
            "#!/bin/sh\ncode-search index update >/dev/null 2>&1 || true\n",
        ),
        (
            "post-merge",
            "#!/bin/sh\ncode-search index update >/dev/null 2>&1 || true\n",
        ),
        (
            "post-rewrite",
            "#!/bin/sh\ncode-search index update >/dev/null 2>&1 || true\n",
        ),
    ];

    let mut installed = Vec::new();
    for (name, script) in hooks {
        let path = hooks_dir.join(name);
        fs::write(&path, script)?;
        make_executable(&path)?;
        installed.push(json!({ "hook": name, "path": path }));
    }
    Ok(Value::Array(installed))
}

pub fn hooks_uninstall(workspace: &Workspace) -> Result<Value> {
    let Some(git_root) = &workspace.git_root else {
        return Err(anyhow!("hooks require a git repository"));
    };
    let hooks_dir = git_root.join(".git").join("hooks");
    let names = [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "post-merge",
        "post-rewrite",
    ];
    let mut removed = Vec::new();
    for name in names {
        let path = hooks_dir.join(name);
        if path.exists() {
            let content = fs::read_to_string(&path).unwrap_or_default();
            if content.contains("code-search index") {
                fs::remove_file(&path)?;
                removed.push(json!({ "hook": name, "removed": true }));
            } else {
                removed.push(
                    json!({ "hook": name, "removed": false, "reason": "not_owned_by_code_search" }),
                );
            }
        }
    }
    Ok(Value::Array(removed))
}

pub fn hooks_status(workspace: &Workspace) -> Result<Value> {
    let Some(git_root) = &workspace.git_root else {
        return Err(anyhow!("hooks require a git repository"));
    };
    let hooks_dir = git_root.join(".git").join("hooks");
    let names = [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "post-merge",
        "post-rewrite",
    ];
    let values = names
        .iter()
        .map(|name| {
            let path = hooks_dir.join(name);
            let installed = path.exists()
                && fs::read_to_string(&path)
                    .map(|content| content.contains("code-search index"))
                    .unwrap_or(false);
            json!({ "hook": name, "installed": installed, "path": path })
        })
        .collect();
    Ok(Value::Array(values))
}

pub fn watch_status(workspace: &Workspace) -> Value {
    json!({
        "watcher": {
            "running": false,
            "root": workspace.root,
            "snapshot": workspace.snapshot_id,
            "queueLength": 0,
            "stale": false,
            "lastEventAt": null,
            "lastReconcileAt": now_ms(),
            "mode": "status_only",
            "note": "This CLI currently exposes a reconcile/status loop; long-running daemon mode is represented by code-search serve."
        }
    })
}

pub fn serve_status(workspace: &Workspace, no_watch: bool) -> Value {
    json!({
        "service": {
            "running": false,
            "root": workspace.root,
            "snapshot": workspace.snapshot_id,
            "watchEnabled": !no_watch,
            "mode": "cli_query_service",
            "note": "The stable CLI/JSON query layer is available. HTTP/MCP adapters should wrap the same command service once schema compatibility is locked."
        }
    })
}

pub(crate) fn scip_root(workspace: &Workspace) -> PathBuf {
    storage_root(workspace)
        .join("scip")
        .join(snapshot_key(&workspace.snapshot_id))
}

fn storage_root(workspace: &Workspace) -> PathBuf {
    workspace.root.join(".code-search")
}

fn snapshot_dir(workspace: &Workspace, key: &str) -> PathBuf {
    storage_root(workspace).join("snapshots").join(key)
}

fn text_dir(workspace: &Workspace, key: &str) -> PathBuf {
    storage_root(workspace).join("text").join(key)
}

fn active_dir(workspace: &Workspace, staged: bool) -> PathBuf {
    storage_root(workspace).join(if staged { "staged" } else { "working" })
}

fn active_manifest_path(workspace: &Workspace, staged: bool) -> PathBuf {
    active_dir(workspace, staged).join("manifest.json")
}

fn snapshot_key(snapshot_id: &str) -> String {
    snapshot_id
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '_',
        })
        .collect()
}

fn fresh_text_snapshot(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<(Manifest, Vec<FileRecord>, PathBuf)>> {
    let manifest_path = active_manifest_path(workspace, false);
    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest = read_manifest(&manifest_path)?;
    if manifest.source != "working_tree" || manifest.scan_options != IndexScanOptions::from(opts) {
        return Ok(None);
    }

    let snapshot_path = snapshot_dir(workspace, &manifest.snapshot_key);
    let text_path = text_dir(workspace, &manifest.snapshot_key);
    let records = snapshot_store::read_files_parquet(&snapshot_path.join("files.parquet"))?;
    let snap_fresh = snapshot_store::verify_snapshot(&snapshot_path, &workspace.root)?;
    if !snap_fresh.stale_files.is_empty() || !snap_fresh.missing_files.is_empty() {
        return Ok(None);
    }

    Ok(Some((manifest, records, text_path)))
}

fn text_index_meta(
    manifest: &Manifest,
    text_path: PathBuf,
    prefilter: Option<&str>,
    candidate_count: Option<usize>,
) -> Value {
    let mut value = json!({
        "used": true,
        "fresh": true,
        "source": "text_index",
        "snapshotSource": manifest.source,
        "manifestHead": manifest.head,
        "snapshot_id": manifest.snapshot_id,
        "snapshotKey": manifest.snapshot_key,
        "fallback": false,
        "path": text_path
    });
    if let Some(prefilter) = prefilter {
        value["prefilter"] = json!(prefilter);
    }
    if let Some(candidate_count) = candidate_count {
        value["candidateCount"] = json!(candidate_count);
    }
    value
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    Ok(serde_json::from_reader(file)?)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
