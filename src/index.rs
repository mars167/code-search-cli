use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    snapshot_store, text_index,
    workspace::{read_staged_blob, staged_tree, tracked_files, FileRecord, ScanOptions, Workspace},
};

const INDEX_SCHEMA_VERSION: u32 = 1;

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
    changed: bool,
    force: bool,
) -> Result<Value> {
    let snapshot_id = if staged {
        format!(
            "staged:{}",
            staged_tree(&workspace.root).unwrap_or_else(|| "unknown".to_string())
        )
    } else {
        workspace.snapshot_id.clone()
    };
    let snapshot_key = snapshot_key(&snapshot_id);

    let root = storage_root(workspace);
    let snapshot_parent = root.join("snapshots");
    let text_parent = root.join("text");
    fs::create_dir_all(&snapshot_parent)?;
    fs::create_dir_all(&text_parent)?;

    let snapshot_target = snapshot_parent.join(&snapshot_key);
    let text_target = text_parent.join(&snapshot_key);
    let snapshot_tmp = snapshot_parent.join(format!("{snapshot_key}.tmp"));
    let text_tmp = text_parent.join(format!("{snapshot_key}.tmp"));

    remove_dir_if_exists(&snapshot_tmp)?;
    remove_dir_if_exists(&text_tmp)?;
    fs::create_dir_all(&snapshot_tmp)?;
    let blobs_dir = snapshot_tmp.join("blobs");
    fs::create_dir_all(&blobs_dir)?;

    let records = if staged {
        staged_records(workspace, Some(&blobs_dir))?
    } else {
        let mut scan_opts = opts.clone();
        scan_opts.limit = 0;
        workspace.scan_files(&scan_opts)?
    };

    let manifest = Manifest {
        schema_version: INDEX_SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        repo_root: workspace.root.to_string_lossy().to_string(),
        snapshot_id: snapshot_id.clone(),
        snapshot_key: snapshot_key.clone(),
        source: if staged { "staged" } else { "working_tree" }.to_string(),
        head: workspace.head.clone(),
        dirty: workspace.dirty,
        file_count: records.len(),
        scan_options: IndexScanOptions::from(opts),
        created_at_epoch_ms: now_ms(),
    };

    write_manifest(&snapshot_tmp.join("manifest.json"), &manifest)?;

    if staged {
        snapshot_store::write_files_parquet(&snapshot_tmp.join("files.parquet"), &records)?;
    } else {
        snapshot_store::build_snapshot(&snapshot_tmp, &records, &workspace.root)?;
    }
    text_index::write(&text_tmp, workspace, &records, !staged)?;

    remove_dir_if_exists(&snapshot_target)?;
    remove_dir_if_exists(&text_target)?;
    fs::rename(&snapshot_tmp, &snapshot_target)?;
    fs::rename(&text_tmp, &text_target)?;

    let active_dir = active_dir(workspace, staged);
    fs::create_dir_all(&active_dir)?;
    write_manifest(&active_dir.join("manifest.json"), &manifest)?;

    Ok(json!({
        "index": {
            "used": true,
            "fresh": true,
            "source": "text_index",
            "snapshotSource": manifest.source,
            "snapshot_id": manifest.snapshot_id,
            "snapshotKey": manifest.snapshot_key,
            "fileCount": manifest.file_count,
            "changedOnly": changed,
            "force": force,
            "path": root,
            "snapshotPath": snapshot_target,
            "textPath": text_target
        }
    }))
}

pub fn status(workspace: &Workspace) -> Result<Value> {
    let root = storage_root(workspace);
    let manifest_path = active_manifest_path(workspace, false);
    if !manifest_path.exists() {
        return Ok(json!({
            "exists": false,
            "fresh": false,
            "path": root,
            "reason": "index_missing"
        }));
    }
    let manifest = read_manifest(&manifest_path)?;
    let snapshot_path = snapshot_dir(workspace, &manifest.snapshot_key);
    let text_path = text_dir(workspace, &manifest.snapshot_key);
    let snap_fresh = snapshot_store::verify_snapshot(&snapshot_path, &workspace.root)?;
    let fresh = snap_fresh.stale_files.is_empty() && snap_fresh.missing_files.is_empty();
    let freshness = json!({
        "freshCount": snap_fresh.fresh_count,
        "staleFiles": snap_fresh.stale_files,
        "missingFiles": snap_fresh.missing_files
    });
    Ok(json!({
        "exists": true,
        "fresh": fresh,
        "path": root,
        "snapshotPath": snapshot_path,
        "textPath": text_path,
        "manifest": manifest,
        "freshness": freshness
    }))
}

pub fn fresh_file_records(
    workspace: &Workspace,
    opts: &ScanOptions,
    path_pattern: Option<&str>,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    let Some((manifest, records, text_path)) = fresh_text_snapshot(workspace, opts)? else {
        return Ok(None);
    };

    let (filtered, prefilter, candidate_count) = match path_pattern {
        Some(pattern) if !pattern.is_empty() => {
            let path_idx = text_path.join("paths.idx");
            match text_index::candidate_path_ids(&path_idx, pattern)? {
                Some(ids) => {
                    let filtered = records
                        .into_iter()
                        .enumerate()
                        .filter_map(|(doc_id, record)| ids.contains(&doc_id).then_some(record))
                        .collect::<Vec<_>>();
                    (filtered, Some("trigram_path"), Some(ids.len()))
                }
                None => (records, Some("skipped"), None),
            }
        }
        _ => (records, None, None),
    };

    let index_meta = text_index_meta(&manifest, text_path, prefilter, candidate_count);
    Ok(Some((filtered, index_meta)))
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

    let grams_path = text_path.join("grams.idx");
    let candidate_ids = text_index::candidate_ids(&grams_path, pattern, mode)?;
    let (prefilter, skipped_reason) = match mode {
        "literal" if pattern.len() < 3 => (Some("skipped"), Some("pattern_too_short")),
        "literal" => match &candidate_ids {
            Some(_) => (Some("trigram_literal"), None),
            None => (Some("skipped"), Some("no_trigrams_extracted")),
        },
        "regex" => {
            let has_literal = has_regex_literal_substring(pattern);
            match (&candidate_ids, has_literal) {
                (Some(_), _) => (Some("trigram_regex"), None),
                (None, false) => (Some("skipped"), Some("no_trigrams_in_regex_pattern")),
                (None, true) => (Some("skipped"), Some("trigram_intersection_empty")),
            }
        }
        _ => (Some("skipped"), Some("unsupported_mode")),
    };

    let filtered = match &candidate_ids {
        Some(ids) => records
            .into_iter()
            .enumerate()
            .filter_map(|(doc_id, record)| ids.contains(&doc_id).then_some(record))
            .collect::<Vec<_>>(),
        None => records,
    };

    let index_meta = if let Some(reason) = skipped_reason {
        let mut value = text_index_meta(
            &manifest,
            text_path,
            prefilter,
            candidate_ids.as_ref().map(|ids| ids.len()),
        );
        value["prefilterSkipReason"] = serde_json::json!(reason);
        value
    } else {
        text_index_meta(
            &manifest,
            text_path,
            prefilter,
            candidate_ids.as_ref().map(|ids| ids.len()),
        )
    };

    Ok(Some((filtered, index_meta)))
}

/// Check if a regex pattern contains at least one literal substring of length >= 3
fn has_regex_literal_substring(pattern: &str) -> bool {
    let mut run = 0usize;
    let mut escape = false;
    for ch in pattern.chars() {
        if escape {
            run += 1;
            escape = false;
            if run >= 3 {
                return true;
            }
            continue;
        }
        match ch {
            '\\' => escape = true,
            '.' | '*' | '+' | '?' | '|' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' => {
                run = 0;
            }
            _ => {
                run += 1;
                if run >= 3 {
                    return true;
                }
            }
        }
    }
    false
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

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
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

fn staged_records(workspace: &Workspace, blobs_dir: Option<&Path>) -> Result<Vec<FileRecord>> {
    let files = tracked_files(&workspace.root)?;
    let mut records = Vec::new();
    for path in files {
        let content = match read_staged_blob(&workspace.root, &path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        if content.iter().take(8192).any(|byte| *byte == 0) {
            continue;
        }
        let hash = format!("blake3:{}", blake3::hash(&content).to_hex());
        let hash_hex = hash.strip_prefix("blake3:").unwrap_or(&hash);
        if let Some(dir) = blobs_dir {
            snapshot_store::write_blob(dir, hash_hex, &content)?;
        }
        records.push(FileRecord {
            language: crate::workspace::language_for_path(Path::new(&path)).to_string(),
            size: content.len() as u64,
            mtime_ms: 0,
            hash,
            path,
        });
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(records)
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    writeln!(file)?;
    Ok(())
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
