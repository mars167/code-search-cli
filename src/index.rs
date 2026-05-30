use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    graph, lancedb_store, snapshot_store, text_index,
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

    let records = if staged {
        staged_records(workspace, None)?
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

    write_to_lancedb(workspace, &manifest, &records, staged)?;

    // Build call graph (best-effort; non-fatal on failure)
    if !staged {
        let _ =
            crate::graph::GraphStore::open(workspace).and_then(|mut store| store.build(workspace));
    }

    let root = storage_root(workspace);
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
            "storageBackend": "lancedb"
        }
    }))
}

pub fn status(workspace: &Workspace) -> Result<Value> {
    let root = storage_root(workspace);

    if lancedb_store::is_available(&workspace.root) {
        if let Ok(store) = lancedb_store::LanceDbStore::open_or_create(&workspace.root) {
            if let Ok(Some(snapshot)) = store.read_snapshot(&workspace.snapshot_id) {
                if let Ok(records) = store.read_file_records(&workspace.snapshot_id) {
                    let freshness = freshness(workspace, &records);
                    let fresh = freshness
                        .get("staleFiles")
                        .and_then(Value::as_array)
                        .map(|items| items.is_empty())
                        .unwrap_or(false)
                        && freshness
                            .get("missingFiles")
                            .and_then(Value::as_array)
                            .map(|items| items.is_empty())
                            .unwrap_or(false);
                    return Ok(json!({
                        "exists": true,
                        "fresh": fresh,
                        "path": root,
                        "snapshotPath": lancedb_store::lancedb_root(&workspace.root),
                        "textPath": text_dir(workspace, &snapshot.snapshot_key),
                        "manifest": snapshot_row_to_manifest(&snapshot),
                        "freshness": freshness
                    }));
                }
            }
        }
    }

    let manifest_path = active_manifest_path(workspace, false);
    if !manifest_path.exists() {
        let remote = remote_status(workspace)?;
        let mut result = json!({
            "exists": false,
            "fresh": false,
            "path": root,
            "reason": "index_missing"
        });
        if !remote.as_array().is_some_and(|a| a.is_empty()) {
            result["remote"] = remote;
        }
        return Ok(result);
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
    let remote = remote_status(workspace)?;
    let mut result = json!({
        "exists": true,
        "fresh": fresh,
        "path": root,
        "snapshotPath": snapshot_path,
        "textPath": text_path,
        "manifest": manifest,
        "freshness": freshness
    });
    if !remote.as_array().is_some_and(|a| a.is_empty()) {
        result["remote"] = remote;
    }
    Ok(result)
}

pub fn fresh_file_records(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    // Try local fresh snapshot first
    let Some((manifest, records, text_path)) = fresh_text_snapshot(workspace, opts)? else {
        // Fall back to remote snapshots
        return remote_fallback_text_records(workspace, None);
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
    // Try local fresh snapshot first
    let Some((manifest, records, text_path)) = fresh_text_snapshot(workspace, opts)? else {
        // Fall back to remote snapshots with trigram prefilter
        return remote_fallback_text_records(workspace, Some((pattern, mode)));
    };

    let candidate_ids =
        text_index::candidate_ids(&text_path.join("grams.idx"), pattern, mode).unwrap_or(None);
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
/// Fall back to remote snapshots when local text index is not fresh or missing.
fn remote_fallback_text_records(
    workspace: &Workspace,
    filter: Option<(&str, &str)>,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    let remote_snapshots = discover_remote_snapshots(workspace)?;
    for (snapshot_key, remote_dir) in &remote_snapshots {
        let text_dir = remote_text_dir(workspace, snapshot_key);
        let docs_path = text_dir.join("docs.idx");
        if !docs_path.exists() {
            continue;
        }

        let records = match text_index::read_docs(&docs_path) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Apply trigram prefilter if requested
        let filtered = if let Some((pattern, mode)) = filter {
            let grams_path = text_dir.join("grams.idx");
            let candidate_ids = text_index::candidate_ids(&grams_path, pattern, mode)?;
            match candidate_ids {
                Some(ids) => records
                    .into_iter()
                    .enumerate()
                    .filter_map(|(doc_id, record)| ids.contains(&doc_id).then_some(record))
                    .collect::<Vec<_>>(),
                None => records,
            }
        } else {
            records
        };

        let verified = remote_snapshot_matches_local(workspace, remote_dir).unwrap_or(false);

        let index_meta = json!({
            "used": true,
            "fresh": verified,
            "source": "text_index:remote",
            "fallback": false,
            "remote_verified": verified,
            "remote_snapshot_key": snapshot_key,
            "snapshot_id": snapshot_key,
            "snapshotKey": snapshot_key,
            "path": text_dir,
        });

        return Ok(Some((filtered, index_meta)));
    }

    Ok(None)
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
    let mut value = status(workspace)?;
    let mut fresh = value.get("fresh").and_then(Value::as_bool).unwrap_or(false);

    // Also verify graph freshness
    if fresh {
        if let Ok(store) = graph::GraphStore::open(workspace) {
            let graph_fresh = store.freshness_check().unwrap_or(false);
            value["graphFresh"] = json!(graph_fresh);
            if !graph_fresh {
                fresh = false;
            }
        } else {
            value["graphFresh"] = json!(false);
            fresh = false;
        }
    }

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
    // Try to initialize watcher and get its status
    let watcher_status = match crate::watcher::Watcher::start(&workspace.root) {
        Ok(watcher) => watcher.status(),
        Err(_) => {
            json!({
                "running": false,
                "root": workspace.root,
                "snapshot": workspace.snapshot_id,
                "queueLength": 0,
                "stale": false,
                "lastEventAt": null,
                "lastReconcileAt": null,
                "mode": "status_only",
                "note": "Failed to initialize watcher"
            })
        }
    };

    json!({
        "watcher": watcher_status
    })
}

pub fn serve_status(workspace: &Workspace, no_watch: bool) -> Value {
    let query_service = json!({
        "running": false,
        "root": workspace.root,
        "snapshot": workspace.snapshot_id,
        "watchEnabled": !no_watch,
        "mode": "cli_query_service",
        "note": "The stable CLI/JSON query layer is available. HTTP/MCP adapters should wrap the same command service once schema compatibility is locked."
    });

    json!({
        "service": query_service
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

pub(crate) fn snapshot_key(snapshot_id: &str) -> String {
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
    if lancedb_store::is_available(&workspace.root) {
        if let Ok(store) = lancedb_store::LanceDbStore::open_or_create(&workspace.root) {
            if let Ok(Some(snapshot)) = store.read_snapshot(&workspace.snapshot_id) {
                if snapshot.source == "working_tree" {
                    if let Ok(scan_opts) =
                        serde_json::from_str::<IndexScanOptions>(&snapshot.scan_options_json)
                    {
                        if scan_opts == IndexScanOptions::from(opts) {
                            if let Ok(records) = store.read_file_records(&workspace.snapshot_id) {
                                let freshness = freshness(workspace, &records);
                                let fresh = freshness
                                    .get("staleFiles")
                                    .and_then(Value::as_array)
                                    .map(|items| items.is_empty())
                                    .unwrap_or(false)
                                    && freshness
                                        .get("missingFiles")
                                        .and_then(Value::as_array)
                                        .map(|items| items.is_empty())
                                        .unwrap_or(false);
                                if fresh {
                                    let manifest = snapshot_row_to_manifest(&snapshot);
                                    let text_path = text_dir(workspace, &snapshot.snapshot_key);
                                    return Ok(Some((manifest, records, text_path)));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

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

fn read_manifest(path: &Path) -> Result<Manifest> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    Ok(serde_json::from_reader(file)?)
}

#[allow(non_snake_case)]
fn freshness(workspace: &Workspace, records: &[FileRecord]) -> Value {
    let mut fresh = Vec::new();
    let mut staleFiles = Vec::new();
    let mut missingFiles = Vec::new();
    for record in records {
        let path = workspace.abs_path(&record.path);
        if !path.exists() {
            missingFiles.push(json!({ "path": record.path, "reason": "missing" }));
            continue;
        }
        // metadata fast path: if mtime and size match, skip hash
        match fs::metadata(&path) {
            Ok(meta) => {
                let current_mtime = crate::workspace::mtime_ms(&meta);
                let current_size = meta.len();
                if current_mtime == record.mtime_ms && current_size == record.size {
                    fresh.push(record.path.clone());
                    continue;
                }
                // metadata changed, verify with hash
                match fs::read(&path) {
                    Ok(content) => {
                        let hash = format!("blake3:{}", blake3::hash(&content).to_hex());
                        if hash == record.hash {
                            fresh.push(record.path.clone());
                        } else {
                            staleFiles.push(json!({
                                "path": record.path,
                                "reason": "file_hash_mismatch",
                                "expected": record.hash,
                                "actual": hash
                            }));
                        }
                    }
                    Err(error) => staleFiles.push(json!({
                        "path": record.path,
                        "reason": "read_error",
                        "message": error.to_string()
                    })),
                }
            }
            Err(error) => staleFiles.push(json!({
                "path": record.path,
                "reason": "read_error",
                "message": error.to_string()
            })),
        }
    }
    json!({
        "freshCount": fresh.len(),
        "staleFiles": staleFiles,
        "missingFiles": missingFiles
    })
}

fn snapshot_row_to_manifest(s: &lancedb_store::SnapShotRow) -> Manifest {
    let scan_options: IndexScanOptions =
        serde_json::from_str(&s.scan_options_json).unwrap_or_else(|_| IndexScanOptions {
            include: Vec::new(),
            exclude: Vec::new(),
            hidden: false,
            no_ignore: false,
        });
    Manifest {
        schema_version: s.schema_version,
        tool_version: s.tool_version.clone(),
        repo_root: s.repo_root.clone(),
        snapshot_id: s.snapshot_id.clone(),
        snapshot_key: s.snapshot_key.clone(),
        source: s.source.clone(),
        head: s.head.clone(),
        dirty: s.dirty,
        file_count: s.file_count as usize,
        scan_options,
        created_at_epoch_ms: s.created_at_epoch_ms as u128,
    }
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Remote pack / unpack
// ---------------------------------------------------------------------------

/// Pack the current snapshot into a portable .tar.gz archive.
pub fn pack(workspace: &Workspace, output_path: &str) -> Result<Value> {
    let active_manifest_path = active_manifest_path(workspace, false);
    if !active_manifest_path.exists() {
        return Err(anyhow!(
            "no local index exists; run 'code-search index build' first"
        ));
    }

    let manifest = read_manifest(&active_manifest_path)?;
    let snapshot_dir = snapshot_dir(workspace, &manifest.snapshot_key);
    let text_dir = text_dir(workspace, &manifest.snapshot_key);
    let scip_d = scip_root(workspace);
    let graph_d = graph::graph_dir(workspace);

    let mut entries: Vec<ArchiveEntry> = Vec::new();

    // Top-level pack manifest
    let pack_manifest = json!({
        "schemaVersion": INDEX_SCHEMA_VERSION,
        "snapshot_id": manifest.snapshot_id,
        "snapshotKey": manifest.snapshot_key,
        "timestamp": now_ms(),
        "source": "packed_remote",
        "toolVersion": env!("CARGO_PKG_VERSION"),
        "originalRepoRoot": manifest.repo_root,
        "head": manifest.head,
        "dirty": manifest.dirty,
        "fileCount": manifest.file_count,
    });
    let pack_manifest_bytes = serde_json::to_vec_pretty(&pack_manifest)?;
    entries.push(ArchiveEntry {
        name: "manifest.json".to_string(),
        content: pack_manifest_bytes,
    });

    // files.parquet
    let files_parquet = snapshot_dir.join("files.parquet");
    if files_parquet.exists() {
        let content = fs::read(&files_parquet)?;
        entries.push(ArchiveEntry {
            name: "files.parquet".to_string(),
            content,
        });
    }

    // text index segments
    for seg_name in &["docs.idx", "paths.idx", "grams.idx"] {
        let seg_path = text_dir.join(seg_name);
        if seg_path.exists() {
            let rel = format!("text/{seg_name}");
            let content = fs::read(&seg_path)?;
            entries.push(ArchiveEntry { name: rel, content });
        }
    }

    // scip occurrence database
    let scip_db = scip_d.join("occurrences.db");
    if scip_db.exists() {
        let content = fs::read(&scip_db)?;
        entries.push(ArchiveEntry {
            name: "scip/occurrences.db".to_string(),
            content,
        });
    }

    // graph petgraph.bin
    let graph_bin = graph_d.join("petgraph.bin");
    if graph_bin.exists() {
        let content = fs::read(&graph_bin)?;
        entries.push(ArchiveEntry {
            name: "graph/petgraph.bin".to_string(),
            content,
        });
    }

    // Build checksums.txt
    let mut checksums = String::new();
    for entry in &entries {
        let mut hasher = Sha256::new();
        hasher.update(&entry.content);
        let hash = format!("{:x}", hasher.finalize());
        checksums.push_str(&format!("{hash}  {}\n", entry.name));
    }
    entries.push(ArchiveEntry {
        name: "checksums.txt".to_string(),
        content: checksums.as_bytes().to_vec(),
    });

    // Create .tar.gz
    let archive_data = build_tar_gz(&entries)?;

    // Write to output
    if output_path == "-" || output_path.is_empty() {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        std::io::Write::write_all(&mut handle, &archive_data)?;
    } else {
        let output = PathBuf::from(output_path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, &archive_data)?;
    }

    Ok(json!([{
        "packed": true,
        "archiveSize": archive_data.len(),
        "entryCount": entries.len(),
        "snapshot_id": manifest.snapshot_id,
        "source": "packed_remote",
        "output": output_path
    }]))
}

/// Unpack a .tar.gz archive into `.code-search/remote/<snapshot_id>/`.
/// NEVER overwrites local snapshots or modifies working/staged directories.
pub fn unpack(workspace: &Workspace, archive_path: &str) -> Result<Value> {
    // Read archive
    let archive_data =
        fs::read(archive_path).with_context(|| format!("failed to read archive {archive_path}"))?;

    // Decompress and parse tar
    let decoder = GzDecoder::new(&archive_data[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut entries: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?.to_string_lossy().to_string();
        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;
        entries.insert(path, content);
    }

    // Verify checksums
    let checksums_data = entries
        .get("checksums.txt")
        .ok_or_else(|| anyhow!("archive missing checksums.txt"))?;
    let checksums_str = String::from_utf8(checksums_data.clone())
        .with_context(|| "checksums.txt is not valid UTF-8")?;
    let expected_checksums = parse_checksums(&checksums_str)?;

    for (path, expected_hash) in &expected_checksums {
        if path == "checksums.txt" {
            continue;
        }
        let content = entries
            .get(path.as_str())
            .ok_or_else(|| anyhow!("archive missing entry: {path}"))?;
        let mut hasher = Sha256::new();
        hasher.update(content);
        let actual_hash = format!("{:x}", hasher.finalize());
        if &actual_hash != expected_hash {
            return Err(anyhow!(
                "checksum mismatch for '{}': expected {}, got {}",
                path,
                expected_hash,
                actual_hash
            ));
        }
    }

    // Read pack manifest
    let pack_manifest_data = entries
        .get("manifest.json")
        .ok_or_else(|| anyhow!("archive missing manifest.json"))?;
    let pack_manifest: Value = serde_json::from_slice(pack_manifest_data)
        .with_context(|| "failed to parse manifest.json")?;

    let schema_version: u32 = pack_manifest["schemaVersion"].as_u64().unwrap_or(0) as u32;
    if schema_version != INDEX_SCHEMA_VERSION {
        return Err(anyhow!(
            "schema version mismatch: archive has {}, expected {}",
            schema_version,
            INDEX_SCHEMA_VERSION
        ));
    }

    let snapshot_id = pack_manifest["snapshot_id"]
        .as_str()
        .ok_or_else(|| anyhow!("pack manifest missing snapshot_id"))?
        .to_string();
    let snapshot_key = snapshot_key(&snapshot_id);

    // Determine remote target directory: .code-search/remote/<snapshot_key>/
    let remote_dir = remote_dir(workspace, &snapshot_key);

    // CRITICAL: never overwrite if already exists
    if remote_dir.exists() {
        return Err(anyhow!(
            "remote snapshot '{}' already exists at {}; remove it first with 'index clean' if needed",
            snapshot_key,
            remote_dir.display()
        ));
    }

    fs::create_dir_all(&remote_dir)?;

    // Extract files to remote dir
    for (path, content) in &entries {
        if path == "checksums.txt" || path == "manifest.json" {
            continue;
        }
        let dest = remote_dir.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, content)?;
    }

    // Write provenance manifest
    let provenance = json!({
        "schemaVersion": INDEX_SCHEMA_VERSION,
        "source": "remote_unpacked",
        "snapshot_id": snapshot_id,
        "snapshotKey": snapshot_key,
        "unpackedAt": now_ms(),
        "archivePath": archive_path,
        "originalRepoRoot": pack_manifest.get("originalRepoRoot"),
        "originalHead": pack_manifest.get("head"),
        "originalDirty": pack_manifest.get("dirty"),
        "fileCount": pack_manifest.get("fileCount"),
        "toolVersion": pack_manifest.get("toolVersion"),
        "warning": "This is a remote-imported snapshot. It does NOT represent local working/staged state."
    });

    write_manifest(
        &remote_dir.join("manifest.json"),
        &Manifest {
            schema_version: INDEX_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_root: workspace.root.to_string_lossy().to_string(),
            snapshot_id: snapshot_id.clone(),
            snapshot_key: snapshot_key.clone(),
            source: "remote_unpacked".to_string(),
            head: pack_manifest["head"].as_str().map(|s| s.to_string()),
            dirty: pack_manifest["dirty"].as_bool().unwrap_or(false),
            file_count: pack_manifest["fileCount"].as_u64().unwrap_or(0) as usize,
            scan_options: IndexScanOptions {
                include: Vec::new(),
                exclude: Vec::new(),
                hidden: false,
                no_ignore: false,
            },
            created_at_epoch_ms: now_ms(),
        },
    )?;

    // Write provenance json alongside
    let provenance_path = remote_dir.join("provenance.json");
    fs::write(&provenance_path, serde_json::to_vec_pretty(&provenance)?)?;

    Ok(json!([{
        "unpacked": true,
        "remote_snapshot_id": snapshot_id,
        "remote_snapshot_key": snapshot_key,
        "remoteDir": remote_dir,
        "entryCount": entries.len() - 2,
        "source": "remote_unpacked",
        "warning": "Remote snapshots live in .code-search/remote/ and will not override local state"
    }]))
}

/// Discover any remote unpacked snapshots.
pub fn discover_remote_snapshots(workspace: &Workspace) -> Result<Vec<(String, PathBuf)>> {
    let remote_root = remote_root(workspace);
    if !remote_root.exists() {
        return Ok(Vec::new());
    }
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(&remote_root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let dir = entry.path();
            let manifest_path = dir.join("manifest.json");
            if manifest_path.exists() {
                if let Ok(manifest) = read_manifest(&manifest_path) {
                    if manifest.source == "remote_unpacked" {
                        snapshots.push((entry.file_name().to_string_lossy().to_string(), dir));
                    }
                }
            }
        }
    }
    Ok(snapshots)
}

/// Check if a remote snapshot's file records match current local files.
pub fn remote_snapshot_matches_local(workspace: &Workspace, remote_dir: &Path) -> Result<bool> {
    let files_parquet = remote_dir.join("files.parquet");
    if !files_parquet.exists() {
        return Ok(false);
    }

    let snap_fresh = snapshot_store::verify_snapshot(remote_dir, &workspace.root)?;
    Ok(snap_fresh.stale_files.is_empty() && snap_fresh.missing_files.is_empty())
}

/// Get the remote text directory for a given snapshot key.
pub fn remote_text_dir(workspace: &Workspace, snapshot_key: &str) -> PathBuf {
    remote_dir(workspace, snapshot_key).join("text")
}

/// Get the remote scip directory for a given snapshot key.
pub fn remote_scip_dir(workspace: &Workspace, snapshot_key: &str) -> PathBuf {
    remote_dir(workspace, snapshot_key).join("scip")
}

/// Get the remote graph directory for a given snapshot key.
pub fn remote_graph_dir(workspace: &Workspace, snapshot_key: &str) -> PathBuf {
    remote_dir(workspace, snapshot_key).join("graph")
}

// ---------------------------------------------------------------------------
// Internal archive helpers
// ---------------------------------------------------------------------------

struct ArchiveEntry {
    name: String,
    content: Vec<u8>,
}

fn build_tar_gz(entries: &[ArchiveEntry]) -> Result<Vec<u8>> {
    let mut archive_data = Vec::new();
    {
        let encoder = GzEncoder::new(&mut archive_data, Compression::default());
        let mut tar_builder = tar::Builder::new(encoder);

        for entry in entries {
            let mut header = tar::Header::new_gnu();
            header
                .set_path(&entry.name)
                .map_err(|e| anyhow!("invalid path in archive: {e}"))?;
            header.set_size(entry.content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar_builder.append(&header, &entry.content[..])?;
        }

        let encoder = tar_builder.into_inner()?;
        encoder.finish()?;
    }
    Ok(archive_data)
}

fn parse_checksums(checksums_str: &str) -> Result<std::collections::HashMap<String, String>> {
    let mut map = std::collections::HashMap::new();
    for line in checksums_str.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Try double-space separator first (sha256sum format)
        let parts: Vec<&str> = line.splitn(2, "  ").collect();
        if parts.len() == 2 {
            map.insert(parts[1].to_string(), parts[0].to_string());
        } else {
            // Try single space
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                map.insert(parts[1].to_string(), parts[0].to_string());
            }
        }
    }
    Ok(map)
}

fn remote_root(workspace: &Workspace) -> PathBuf {
    storage_root(workspace).join("remote")
}

fn remote_dir(workspace: &Workspace, snapshot_key: &str) -> PathBuf {
    remote_root(workspace).join(snapshot_key)
}

fn remote_status(workspace: &Workspace) -> Result<Value> {
    let remote_snapshots = discover_remote_snapshots(workspace)?;
    let mut snapshots = Vec::new();
    for (snapshot_key, remote_dir) in &remote_snapshots {
        let verified = remote_snapshot_matches_local(workspace, remote_dir).unwrap_or(false);
        let has_text = remote_dir.join("text").join("docs.idx").exists();
        let has_scip = remote_dir.join("scip").join("occurrences.db").exists();
        let has_graph = remote_dir.join("graph").join("petgraph.bin").exists();
        snapshots.push(json!({
            "snapshot_key": snapshot_key,
            "source": "remote_unpacked",
            "remoteVerified": verified,
            "hasTextIndex": has_text,
            "hasScipIndex": has_scip,
            "hasGraph": has_graph,
            "remoteDir": remote_dir,
        }));
    }
    Ok(Value::Array(snapshots))
}

fn write_to_lancedb(
    workspace: &Workspace,
    manifest: &Manifest,
    records: &[FileRecord],
    staged: bool,
) -> Result<()> {
    let lancedb = lancedb_store::LanceDbStore::open_or_create(&workspace.root)
        .with_context(|| "failed to open LanceDB store")?;

    lancedb
        .ensure_tables()
        .with_context(|| "failed to ensure LanceDB tables")?;

    let scan_options_json = serde_json::to_string(&manifest.scan_options).unwrap_or_default();

    lancedb
        .write_snapshot(
            &manifest.snapshot_id,
            &manifest.snapshot_key,
            manifest.schema_version,
            &manifest.tool_version,
            &manifest.repo_root,
            manifest.head.as_deref(),
            manifest.dirty,
            &manifest.source,
            &scan_options_json,
            manifest.file_count as u32,
            manifest.created_at_epoch_ms as u64,
        )
        .with_context(|| "failed to write snapshot to LanceDB")?;

    lancedb
        .write_file_catalog(&manifest.snapshot_id, records)
        .with_context(|| "failed to write file catalog to LanceDB")?;

    lancedb
        .write_file_proofs(&manifest.snapshot_id, records)
        .with_context(|| "failed to write file proofs to LanceDB")?;

    if !staged {
        let mut gram_index: BTreeMap<[u8; 3], Vec<u32>> = BTreeMap::new();
        for (doc_id, record) in records.iter().enumerate() {
            let bytes = match fs::read(workspace.abs_path(&record.path)) {
                Ok(b) => b,
                Err(_) => continue,
            };
            for window in bytes.windows(3) {
                let gram = [window[0], window[1], window[2]];
                gram_index.entry(gram).or_default().push(doc_id as u32);
            }
        }
        if !gram_index.is_empty() {
            lancedb
                .write_gram_postings(&manifest.snapshot_id, &gram_index)
                .with_context(|| "failed to write gram postings to LanceDB")?;
        }
    }

    Ok(())
}
fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    writeln!(file)?;
    Ok(())
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
