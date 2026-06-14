use std::{
    collections::{BTreeMap, BTreeSet},
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
    graph, lancedb_store, output,
    scan_diagnostics::SkippedFile,
    snapshot_store, text_index,
    workspace::{
        read_staged_blob, staged_tree, tracked_files, FileRecord, MaterializedIndexData,
        ScanOptions, Workspace,
    },
};

const INDEX_SCHEMA_VERSION: u32 = 1;

/// Maximum decompressed size of a single tar entry during `index unpack`.
const MAX_ENTRY_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB
/// Maximum total decompressed size across all tar entries during `index unpack`.
const MAX_TOTAL_DECOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
/// Maximum number of tar entries accepted during `index unpack`.
const MAX_ARCHIVE_ENTRIES: usize = 1_000;

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
    #[serde(default)]
    lang: Vec<String>,
    #[serde(default)]
    changed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkippedLog {
    schema_version: u32,
    tool_version: String,
    snapshot_id: String,
    snapshot_key: String,
    source: String,
    scan_options: IndexScanOptions,
    created_at_epoch_ms: u128,
    count: usize,
    items: Vec<SkippedFile>,
}

#[derive(Clone, Debug)]
pub struct IndexedRecords {
    pub records: Vec<FileRecord>,
    pub index: Value,
    pub indexed_paths: BTreeSet<String>,
    pub overlay_paths: BTreeSet<String>,
    pub stale_paths: BTreeSet<String>,
    pub missing_paths: BTreeSet<String>,
}

struct LocalTextSnapshot {
    manifest: Manifest,
    records: Vec<FileRecord>,
    text_path: PathBuf,
    lancedb: bool,
    freshness: Value,
}

pub fn build(
    workspace: &Workspace,
    opts: &ScanOptions,
    staged: bool,
    changed: bool,
    force: bool,
    semantic_enabled: bool,
    verbose: output::VerboseLogger,
) -> Result<Value> {
    let changed_only = changed || opts.changed;
    let mut effective_opts = opts.clone();
    effective_opts.changed = changed_only;

    let snapshot_id = if staged {
        format!(
            "staged:{}",
            staged_tree(&workspace.root).unwrap_or_else(|| "unknown".to_string())
        )
    } else {
        workspace.snapshot_id.clone()
    };
    let snapshot_key = snapshot_key(&snapshot_id);
    verbose.log(format!(
        "index build: snapshot_id={snapshot_id} staged={staged} changed_only={changed_only} force={force}"
    ));

    let (records, skipped_files, materialized_index_data) = if staged {
        let records = staged_records(workspace, None)?;
        verbose.log(format!("index build: staged records={}", records.len()));
        (records, Vec::new(), Vec::new())
    } else {
        let mut scan_opts = effective_opts.clone();
        scan_opts.limit = 0;
        verbose.log(format!(
            "index build: scanning include={:?} exclude={:?} hidden={} no_ignore={} lang={:?} changed={}",
            scan_opts.include,
            scan_opts.exclude,
            scan_opts.hidden,
            scan_opts.no_ignore,
            scan_opts.lang,
            scan_opts.changed
        ));
        let catalog_scan = workspace.scan_catalog_with_skips(&scan_opts)?;
        verbose.log(format!(
            "index build: catalog files={}",
            catalog_scan.files.len()
        ));
        let materialized = workspace.materialize_proofs_with_skips(&catalog_scan.files)?;
        let mut skipped_files = catalog_scan.skipped;
        skipped_files.extend(materialized.skipped);
        (materialized.records, skipped_files, materialized.index_data)
    };
    verbose.log(format!(
        "index build: materialized proofs={}",
        records.len()
    ));
    verbose.log(format!(
        "index build: skipped files={}",
        skipped_files.len()
    ));
    let skipped_count = skipped_files.len();

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
        scan_options: IndexScanOptions::from(&effective_opts),
        created_at_epoch_ms: now_ms(),
    };

    // Write compat manifest BEFORE LanceDB — if LanceDB fails,
    // we still have a valid manifest.json for legacy fallback
    let active_dir = active_dir(workspace, staged);
    fs::create_dir_all(&active_dir).with_context(|| {
        format!(
            "failed to create index metadata dir {}",
            active_dir.display()
        )
    })?;
    verbose.log(format!(
        "index build: writing manifest dir={}",
        active_dir.display()
    ));
    write_manifest(&active_dir.join("manifest.json"), &manifest)
        .with_context(|| format!("failed to write index manifest in {}", active_dir.display()))?;
    let skipped_log_path = skipped_log_path(workspace, staged);
    write_skipped_log(
        &skipped_log_path,
        SkippedLog {
            schema_version: INDEX_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            snapshot_id: snapshot_id.clone(),
            snapshot_key: snapshot_key.clone(),
            source: manifest.source.clone(),
            scan_options: manifest.scan_options.clone(),
            created_at_epoch_ms: now_ms(),
            count: skipped_files.len(),
            items: skipped_files,
        },
    )
    .with_context(|| {
        format!(
            "failed to write skipped file log in {}",
            active_dir.display()
        )
    })?;

    let materialized_index_data = (!staged).then_some(materialized_index_data.as_slice());
    write_to_lancedb(
        workspace,
        &manifest,
        &records,
        materialized_index_data,
        staged,
        verbose,
    )
    .with_context(|| {
        format!(
            "LanceDB write failed for snapshot {}. Run 'codetrail index build --force' to rebuild.",
            manifest.snapshot_id
        )
    })?;

    let semantic = if !staged && semantic_enabled {
        crate::lsp::generate_best_effort(workspace, &records, verbose).unwrap_or_else(|error| {
            verbose.log(format!("semantic: phase failed (non-fatal): {error}"));
            crate::lsp::SemanticBuildReport {
                attempted: true,
                skipped: false,
                skip_reason: None,
                languages: Vec::new(),
            }
        })
    } else {
        let reason = if staged {
            "staged_build"
        } else {
            "semantic_disabled"
        };
        crate::lsp::SemanticBuildReport::skipped(reason)
    };

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
            "changedOnly": changed_only,
            "force": force,
            "path": root,
            "storageBackend": "lancedb",
            "semantic": crate::lsp::scip_gen::semantic_summary_json(&semantic),
            "skipped": {
                "count": skipped_count,
                "path": skipped_log_path
            }
        }
    }))
}

pub fn update(
    workspace: &Workspace,
    opts: &ScanOptions,
    verbose: output::VerboseLogger,
) -> Result<Value> {
    verbose.log("index update: checking index status");
    let status = status(workspace)?;
    if status
        .get("fresh")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        verbose.log("index update: index already fresh");
        return Ok(json!({
            "updated": false,
            "reason": "index_fresh",
            "index": {
                "used": true,
                "fresh": true,
                "source": "text_index",
                "path": storage_root(workspace),
                "storageBackend": "lancedb"
            },
            "status": status
        }));
    }

    verbose.log("index update: rebuilding stale or missing index");
    let mut value = build(workspace, opts, false, false, false, true, verbose)?;
    value["updated"] = json!(true);
    value["reason"] = json!("index_stale_or_missing");
    Ok(value)
}

pub fn status(workspace: &Workspace) -> Result<Value> {
    let root = storage_root(workspace);
    let semantic_manifests =
        crate::lsp::scip_gen::read_generation_manifests(workspace).unwrap_or_default();
    let manifest_path = active_manifest_path(workspace, false);
    let active_manifest = manifest_path
        .exists()
        .then(|| read_manifest(&manifest_path))
        .transpose()?;

    if lancedb_store::is_available(&workspace.root) {
        if let Ok(store) = lancedb_store::LanceDbStore::open_or_create(&workspace.root) {
            let snapshot_id = active_manifest
                .as_ref()
                .map(|manifest| manifest.snapshot_id.as_str())
                .unwrap_or(&workspace.snapshot_id);
            if let Ok(Some(snapshot)) = store.read_snapshot(snapshot_id) {
                if let Ok(records) = store.read_file_records(snapshot_id) {
                    let indexed_paths = records
                        .iter()
                        .map(|record| record.path.clone())
                        .collect::<BTreeSet<_>>();
                    let manifest = snapshot_row_to_manifest(&snapshot);
                    let mut freshness = freshness(workspace, &records);
                    let added_paths = added_paths_not_indexed(
                        workspace,
                        &scan_options_from_index_scan(&manifest.scan_options),
                        &indexed_paths,
                    )?;
                    attach_added_files(&mut freshness, &added_paths);
                    let fresh = freshness_is_clean(&freshness);
                    return Ok(json!({
                        "exists": true,
                        "fresh": fresh,
                        "path": root,
                        "snapshotPath": lancedb_store::lancedb_root(&workspace.root),
                        "textPath": text_dir(workspace, &snapshot.snapshot_key),
                        "manifest": manifest,
                        "freshness": freshness,
                        "semanticManifests": semantic_manifests,
                    }));
                }
            }
        }
    }

    let Some(manifest) = active_manifest else {
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
    };
    let snapshot_path = snapshot_dir(workspace, &manifest.snapshot_key);
    let text_path = text_dir(workspace, &manifest.snapshot_key);
    let freshness = match snapshot_store::verify_snapshot(&snapshot_path, &workspace.root) {
        Ok(snap_fresh) => snapshot_freshness_json(snap_fresh),
        Err(error) if snapshot_store::is_legacy_parquet_catalog_error(&error) => {
            legacy_snapshot_freshness(&snapshot_path.join("files.parquet"))
        }
        Err(error) => return Err(error),
    };
    let fresh = freshness_is_clean(&freshness);
    let remote = remote_status(workspace)?;
    let mut result = json!({
        "exists": true,
        "fresh": fresh,
        "path": root,
        "snapshotPath": snapshot_path,
        "textPath": text_path,
        "manifest": manifest,
        "freshness": freshness,
        "semanticManifests": semantic_manifests,
    });
    if !remote.as_array().is_some_and(|a| a.is_empty()) {
        result["remote"] = remote;
    }
    Ok(result)
}

pub fn skipped(workspace: &Workspace, staged: bool) -> Result<Value> {
    let path = skipped_log_path(workspace, staged);
    if !path.exists() {
        return Ok(json!({
            "exists": false,
            "path": path,
            "staged": staged,
            "count": 0,
            "items": []
        }));
    }

    let log = read_skipped_log(&path)?;
    Ok(json!({
        "exists": true,
        "path": path,
        "staged": staged,
        "schemaVersion": log.schema_version,
        "toolVersion": log.tool_version,
        "snapshot_id": log.snapshot_id,
        "snapshotKey": log.snapshot_key,
        "source": log.source,
        "scanOptions": log.scan_options,
        "createdAtEpochMs": log.created_at_epoch_ms,
        "count": log.items.len(),
        "items": log.items
    }))
}

pub fn fresh_file_records(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    Ok(indexed_file_records(workspace, opts)?.and_then(|indexed| {
        indexed
            .index
            .get("fresh")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some((indexed.records, indexed.index))
    }))
}

pub fn fresh_text_records(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    mode: &str,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    Ok(
        indexed_text_records(workspace, opts, pattern, mode)?.and_then(|indexed| {
            indexed
                .index
                .get("fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then_some((indexed.records, indexed.index))
        }),
    )
}

pub fn indexed_file_records(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<IndexedRecords>> {
    let Some(local) = local_text_snapshot(workspace, opts)? else {
        return Ok(
            remote_fallback_text_records(workspace, opts, None)?.map(|(records, index)| {
                IndexedRecords {
                    indexed_paths: records.iter().map(|record| record.path.clone()).collect(),
                    records,
                    index,
                    overlay_paths: BTreeSet::new(),
                    stale_paths: BTreeSet::new(),
                    missing_paths: BTreeSet::new(),
                }
            }),
        );
    };

    indexed_records_from_local(workspace, opts, local, None)
}

pub fn indexed_text_records(
    workspace: &Workspace,
    opts: &ScanOptions,
    pattern: &str,
    mode: &str,
) -> Result<Option<IndexedRecords>> {
    let Some(local) = local_text_snapshot(workspace, opts)? else {
        return Ok(
            remote_fallback_text_records(workspace, opts, Some((pattern, mode)))?.map(
                |(records, index)| IndexedRecords {
                    indexed_paths: records.iter().map(|record| record.path.clone()).collect(),
                    records,
                    index,
                    overlay_paths: BTreeSet::new(),
                    stale_paths: BTreeSet::new(),
                    missing_paths: BTreeSet::new(),
                },
            ),
        );
    };

    indexed_records_from_local(workspace, opts, local, Some((pattern, mode)))
}
/// Fall back to remote snapshots when local text index is not fresh or missing.
fn remote_fallback_text_records(
    workspace: &Workspace,
    opts: &ScanOptions,
    filter: Option<(&str, &str)>,
) -> Result<Option<(Vec<FileRecord>, Value)>> {
    let remote_snapshots = discover_remote_snapshots(workspace)?;
    for (snapshot_key, remote_dir) in &remote_snapshots {
        let manifest = match read_manifest(&remote_dir.join("manifest.json")) {
            Ok(manifest) => manifest,
            Err(_) => continue,
        };
        if manifest.scan_options != IndexScanOptions::from(opts) {
            continue;
        }
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

fn lancedb_candidate_ids(
    workspace: &Workspace,
    snapshot_id: &str,
    pattern: &str,
    mode: &str,
) -> Result<Option<std::collections::HashSet<usize>>> {
    let Some(query_grams) = text_index::query_grams(pattern, mode) else {
        return Ok(None);
    };
    let store = lancedb_store::LanceDbStore::open_or_create(&workspace.root)?;
    let postings = store.read_gram_postings(snapshot_id, &query_grams)?;
    Ok(Some(text_index::intersect_postings(
        &query_grams,
        &postings,
    )))
}

impl From<&ScanOptions> for IndexScanOptions {
    fn from(opts: &ScanOptions) -> Self {
        Self {
            include: opts.include.clone(),
            exclude: opts.exclude.clone(),
            hidden: opts.hidden,
            no_ignore: opts.no_ignore,
            lang: opts.lang.clone(),
            changed: opts.changed,
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

    let graph_snapshot_id = value
        .get("manifest")
        .and_then(|manifest| manifest.get("snapshotId"))
        .and_then(Value::as_str)
        .unwrap_or(&workspace.snapshot_id);

    // Graph build is best-effort; only verify it when a persisted graph exists.
    if fresh && graph::graph_index_exists_for_snapshot(workspace, graph_snapshot_id) {
        if let Ok(store) = graph::GraphStore::open_for_snapshot(workspace, graph_snapshot_id) {
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
            "#!/bin/sh\ncodetrail index build --staged >/dev/null 2>&1 || true\n",
        ),
        (
            "post-commit",
            "#!/bin/sh\ncodetrail index build >/dev/null 2>&1 || true\n",
        ),
        (
            "post-checkout",
            "#!/bin/sh\ncodetrail index update >/dev/null 2>&1 || true\n",
        ),
        (
            "post-merge",
            "#!/bin/sh\ncodetrail index update >/dev/null 2>&1 || true\n",
        ),
        (
            "post-rewrite",
            "#!/bin/sh\ncodetrail index update >/dev/null 2>&1 || true\n",
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
            if content.contains("codetrail index") {
                fs::remove_file(&path)?;
                removed.push(json!({ "hook": name, "removed": true }));
            } else {
                removed.push(
                    json!({ "hook": name, "removed": false, "reason": "not_owned_by_codetrail" }),
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
                    .map(|content| content.contains("codetrail index"))
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
    workspace.root.join(".codetrail")
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

fn skipped_log_path(workspace: &Workspace, staged: bool) -> PathBuf {
    active_dir(workspace, staged).join("skipped.json")
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

fn local_text_snapshot(
    workspace: &Workspace,
    opts: &ScanOptions,
) -> Result<Option<LocalTextSnapshot>> {
    let manifest_path = active_manifest_path(workspace, false);
    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest = read_manifest(&manifest_path)?;
    if manifest.source != "working_tree" || manifest.scan_options != IndexScanOptions::from(opts) {
        return Ok(None);
    }

    let text_path = text_dir(workspace, &manifest.snapshot_key);
    if lancedb_store::is_available(&workspace.root) {
        if let Ok(store) = lancedb_store::LanceDbStore::open_or_create(&workspace.root) {
            if let Ok(Some(snapshot)) = store.read_snapshot(&manifest.snapshot_id) {
                if snapshot.source == "working_tree" {
                    if let Ok(scan_opts) =
                        serde_json::from_str::<IndexScanOptions>(&snapshot.scan_options_json)
                    {
                        if scan_opts == IndexScanOptions::from(opts) {
                            if let Ok(records) = store.read_file_records(&manifest.snapshot_id) {
                                return Ok(Some(LocalTextSnapshot {
                                    manifest: snapshot_row_to_manifest(&snapshot),
                                    freshness: freshness(workspace, &records),
                                    records,
                                    text_path,
                                    lancedb: true,
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    let snapshot_path = snapshot_dir(workspace, &manifest.snapshot_key);
    let files_path = snapshot_path.join("files.parquet");
    if !files_path.exists() {
        return Ok(None);
    }
    let records = match snapshot_store::read_files_parquet(&files_path) {
        Ok(records) => records,
        Err(error) if snapshot_store::is_legacy_parquet_catalog_error(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let freshness = match snapshot_store::verify_snapshot(&snapshot_path, &workspace.root) {
        Ok(snap_fresh) => snapshot_freshness_json(snap_fresh),
        Err(error) if snapshot_store::is_legacy_parquet_catalog_error(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(LocalTextSnapshot {
        manifest,
        records,
        text_path,
        lancedb: false,
        freshness,
    }))
}

fn indexed_records_from_local(
    workspace: &Workspace,
    opts: &ScanOptions,
    local: LocalTextSnapshot,
    filter: Option<(&str, &str)>,
) -> Result<Option<IndexedRecords>> {
    let mut freshness = local.freshness;
    let stale_paths = freshness_paths(&freshness, "staleFiles");
    let missing_paths = freshness_paths(&freshness, "missingFiles");
    let indexed_paths = local
        .records
        .iter()
        .map(|record| record.path.clone())
        .collect::<BTreeSet<_>>();
    let added_paths = added_paths_not_indexed(workspace, opts, &indexed_paths)?;
    attach_added_files(&mut freshness, &added_paths);
    let mut overlay_paths = stale_paths.clone();
    overlay_paths.extend(added_paths.iter().cloned());
    let fresh = stale_paths.is_empty() && missing_paths.is_empty() && added_paths.is_empty();

    let mut candidate_ids = None;
    let mut prefilter = None;
    let mut prefilter_reason = None;
    if let Some((pattern, mode)) = filter {
        candidate_ids = if local.lancedb {
            lancedb_candidate_ids(workspace, &local.manifest.snapshot_id, pattern, mode)
                .unwrap_or(None)
        } else {
            text_index::candidate_ids(&local.text_path.join("grams.idx"), pattern, mode)
                .unwrap_or(None)
        };
        if candidate_ids.is_some() {
            prefilter = Some("trigram");
        } else {
            prefilter = Some("none");
            prefilter_reason = Some(prefilter_unavailable_reason(pattern, mode));
        }
    }

    let mut records = Vec::new();
    for (doc_id, record) in local.records.into_iter().enumerate() {
        if candidate_ids
            .as_ref()
            .is_some_and(|ids| !ids.contains(&doc_id))
        {
            continue;
        }
        if stale_paths.contains(&record.path) || missing_paths.contains(&record.path) {
            continue;
        }
        records.push(record);
    }
    let candidate_count = candidate_ids.as_ref().map(|_| records.len());

    let mut index = text_index_meta(
        &local.manifest,
        local.text_path,
        prefilter,
        candidate_count,
        Some(workspace.scan_summary(opts)?),
    );
    if let Some(reason) = prefilter_reason {
        index["prefilterReason"] = json!(reason);
    }
    if !fresh {
        index["fresh"] = json!(false);
        index["fallback"] = json!(true);
        index["reason"] = json!("partial_live_overlay");
        index["freshness"] = freshness;
        index["staleCount"] = json!(stale_paths.len());
        index["missingCount"] = json!(missing_paths.len());
        index["addedCount"] = json!(added_paths.len());
    }

    Ok(Some(IndexedRecords {
        records,
        index,
        indexed_paths,
        overlay_paths,
        stale_paths,
        missing_paths,
    }))
}

fn freshness_is_clean(freshness: &Value) -> bool {
    freshness
        .get("staleFiles")
        .and_then(Value::as_array)
        .map(|items| items.is_empty())
        .unwrap_or(false)
        && freshness
            .get("missingFiles")
            .and_then(Value::as_array)
            .map(|items| items.is_empty())
            .unwrap_or(false)
        && freshness
            .get("addedFiles")
            .and_then(Value::as_array)
            .map(|items| items.is_empty())
            .unwrap_or(true)
}

fn snapshot_freshness_json(freshness: snapshot_store::SnapshotFreshness) -> Value {
    json!({
        "freshCount": freshness.fresh_count,
        "staleFiles": freshness.stale_files,
        "missingFiles": freshness.missing_files
    })
}

fn legacy_snapshot_freshness(files_path: &Path) -> Value {
    json!({
        "freshCount": 0,
        "staleFiles": [{
            "path": files_path.to_string_lossy().to_string(),
            "reason": "legacy_snapshot_format",
            "message": "legacy Parquet file catalog requires rebuilding the index"
        }],
        "missingFiles": []
    })
}

fn freshness_paths(freshness: &Value, field: &str) -> BTreeSet<String> {
    freshness
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("path")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect()
}

fn added_paths_not_indexed(
    workspace: &Workspace,
    opts: &ScanOptions,
    indexed_paths: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut scan_opts = opts.clone();
    scan_opts.limit = 0;
    scan_opts.changed = false;
    Ok(workspace
        .scan_catalog(&scan_opts)?
        .into_iter()
        .map(|record| record.path)
        .filter(|path| !indexed_paths.contains(path))
        .collect())
}

fn scan_options_from_index_scan(scan_options: &IndexScanOptions) -> ScanOptions {
    ScanOptions {
        include: scan_options.include.clone(),
        exclude: scan_options.exclude.clone(),
        hidden: scan_options.hidden,
        no_ignore: scan_options.no_ignore,
        lang: scan_options.lang.clone(),
        changed: false,
        cursor: None,
        allow_broad: false,
        limit: 0,
        ..ScanOptions::default()
    }
}

fn attach_added_files(freshness: &mut Value, added_paths: &BTreeSet<String>) {
    if added_paths.is_empty() {
        return;
    }
    freshness["addedFiles"] = Value::Array(
        added_paths
            .iter()
            .map(|path| json!({ "path": path, "reason": "not_in_index" }))
            .collect(),
    );
}

fn prefilter_unavailable_reason(pattern: &str, mode: &str) -> &'static str {
    if mode != "literal" {
        "regex_prefilter_not_supported"
    } else if pattern.len() < 3 {
        "literal_shorter_than_trigram"
    } else {
        "trigram_prefilter_unavailable"
    }
}

fn text_index_meta(
    manifest: &Manifest,
    text_path: PathBuf,
    prefilter: Option<&str>,
    candidate_count: Option<usize>,
    scan_summary: Option<Value>,
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
    if let Some(scan_summary) = scan_summary {
        value["scanSummary"] = scan_summary;
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
            mode: 0,
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
        // metadata fast path: if size, mtime, and mode match, skip hash
        match fs::metadata(&path) {
            Ok(meta) => {
                let current_mtime = crate::workspace::mtime_ms(&meta);
                let current_size = meta.len();
                let current_mode = crate::workspace::file_mode(&meta);
                let mode_matches = record.mode == 0 || current_mode == record.mode;
                if current_mtime == record.mtime_ms && current_size == record.size && mode_matches {
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
            lang: Vec::new(),
            changed: false,
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
            "no local index exists; run 'codetrail index build' first"
        ));
    }

    let manifest = read_manifest(&active_manifest_path)?;
    let snapshot_dir = snapshot_dir(workspace, &manifest.snapshot_key);
    let text_dir = text_dir(workspace, &manifest.snapshot_key);
    let scip_d = scip_root(workspace);
    let graph_d = graph::graph_dir(workspace);

    let mut entries: Vec<ArchiveEntry> = Vec::new();
    let lancedb_records = lancedb_store::LanceDbStore::open_or_create(&workspace.root)
        .ok()
        .and_then(|store| store.read_file_records(&manifest.snapshot_id).ok());

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
        "scanOptions": manifest.scan_options.clone(),
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
    } else if let Some(records) = &lancedb_records {
        let tmp = pack_temp_dir(workspace, "files")?;
        let parquet_path = tmp.join("files.parquet");
        snapshot_store::write_files_parquet(&parquet_path, records)?;
        entries.push(ArchiveEntry {
            name: "files.parquet".to_string(),
            content: fs::read(parquet_path)?,
        });
        let _ = fs::remove_dir_all(tmp);
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
    let has_docs = entries.iter().any(|entry| entry.name == "text/docs.idx");
    let has_grams = entries.iter().any(|entry| entry.name == "text/grams.idx");
    if let Some(records) = &lancedb_records {
        if !has_docs || !has_grams {
            let tmp = pack_temp_dir(workspace, "text")?;
            if !has_docs {
                let docs_path = tmp.join("docs.idx");
                text_index::write_docs(&docs_path, records)?;
                entries.push(ArchiveEntry {
                    name: "text/docs.idx".to_string(),
                    content: fs::read(docs_path)?,
                });
            }
            if !has_grams {
                let grams_path = tmp.join("grams.idx");
                text_index::write_grams(&grams_path, &workspace.root, records)?;
                entries.push(ArchiveEntry {
                    name: "text/grams.idx".to_string(),
                    content: fs::read(grams_path)?,
                });
            }
            let _ = fs::remove_dir_all(tmp);
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

/// Unpack a .tar.gz archive into `.codetrail/remote/<snapshot_id>/`.
/// NEVER overwrites local snapshots or modifies working/staged directories.
pub fn unpack(workspace: &Workspace, archive_path: &str) -> Result<Value> {
    // Read archive
    let archive_data =
        fs::read(archive_path).with_context(|| format!("failed to read archive {archive_path}"))?;

    // Decompress and parse tar with bounded memory usage.
    // All entries are collected first so that checksums can be validated
    // before any file is written to disk.
    let decoder = GzDecoder::new(&archive_data[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut entries: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let mut total_decompressed: u64 = 0;
    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?.to_string_lossy().to_string();

        if entries.len() >= MAX_ARCHIVE_ENTRIES {
            return Err(anyhow!(
                "archive contains more than {MAX_ARCHIVE_ENTRIES} entries; refusing to unpack"
            ));
        }

        let entry_size = entry.header().size()?;
        if entry_size > MAX_ENTRY_DECOMPRESSED_BYTES {
            return Err(anyhow!(
                "archive entry '{path}' declares size {entry_size} bytes which exceeds the \
                 per-entry limit of {MAX_ENTRY_DECOMPRESSED_BYTES} bytes"
            ));
        }
        total_decompressed = total_decompressed.saturating_add(entry_size);
        if total_decompressed > MAX_TOTAL_DECOMPRESSED_BYTES {
            return Err(anyhow!(
                "archive total decompressed size exceeds the limit of \
                 {MAX_TOTAL_DECOMPRESSED_BYTES} bytes"
            ));
        }

        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;

        // Enforce the per-entry cap on the actual decompressed bytes as well,
        // since the header size field can be untrustworthy in crafted archives.
        if content.len() as u64 > MAX_ENTRY_DECOMPRESSED_BYTES {
            return Err(anyhow!(
                "archive entry '{path}' decompressed to {} bytes which exceeds the per-entry \
                 limit of {MAX_ENTRY_DECOMPRESSED_BYTES} bytes",
                content.len()
            ));
        }

        entries.insert(path, content);
    }

    // Verify checksums
    let checksums_data = entries
        .get("checksums.txt")
        .ok_or_else(|| anyhow!("archive missing checksums.txt"))?;
    let checksums_str = String::from_utf8(checksums_data.clone())
        .with_context(|| "checksums.txt is not valid UTF-8")?;
    let expected_checksums = parse_checksums(&checksums_str)?;
    verify_archive_checksums(&entries, &expected_checksums)?;

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
    let scan_options = pack_manifest
        .get("scanOptions")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_else(|| IndexScanOptions {
            include: Vec::new(),
            exclude: Vec::new(),
            hidden: false,
            no_ignore: false,
            lang: Vec::new(),
            changed: false,
        });

    // Determine remote target directory: .codetrail/remote/<snapshot_key>/
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

    // Extract files to remote dir with path-traversal guard.
    for (path, content) in &entries {
        if path == "checksums.txt" || path == "manifest.json" {
            continue;
        }
        let dest = safe_join(&remote_dir, path)?;
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
        "scanOptions": scan_options.clone(),
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
            scan_options,
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
        "warning": "Remote snapshots live in .codetrail/remote/ and will not override local state"
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

    let snap_fresh = match snapshot_store::verify_snapshot(remote_dir, &workspace.root) {
        Ok(snap_fresh) => snap_fresh,
        Err(error) if snapshot_store::is_legacy_parquet_catalog_error(&error) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
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

fn verify_archive_checksums(
    entries: &std::collections::HashMap<String, Vec<u8>>,
    expected_checksums: &std::collections::HashMap<String, String>,
) -> Result<()> {
    for path in entries.keys() {
        if path == "checksums.txt" {
            continue;
        }
        if !expected_checksums.contains_key(path) {
            return Err(anyhow!(
                "archive entry '{path}' is not listed in checksums.txt"
            ));
        }
    }

    for (path, expected_hash) in expected_checksums {
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

    Ok(())
}

fn pack_temp_dir(workspace: &Workspace, label: &str) -> Result<PathBuf> {
    let dir = storage_root(workspace).join(format!(
        ".pack-tmp-{label}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn remote_root(workspace: &Workspace) -> PathBuf {
    storage_root(workspace).join("remote")
}

fn remote_dir(workspace: &Workspace, snapshot_key: &str) -> PathBuf {
    remote_root(workspace).join(snapshot_key)
}

/// Join `base` and the relative path `rel` from a tar archive entry,
/// rejecting any path that would escape `base`.
///
/// Rejects: empty paths, absolute paths, `..` components, Windows volume
/// prefixes, and any result that does not start with `base`.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(anyhow!("archive entry has an empty path"));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(anyhow!("archive entry has an absolute path: {rel}"));
    }
    for component in rel_path.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(anyhow!("archive entry contains '..' path component: {rel}"));
            }
            std::path::Component::Prefix(_) => {
                return Err(anyhow!(
                    "archive entry contains a Windows volume prefix: {rel}"
                ));
            }
            _ => {}
        }
    }
    let dest = base.join(rel_path);
    // Belt-and-suspenders: verify the joined path is still under base.
    // `starts_with` works on component boundaries, so a prefix that happens
    // to share bytes but not a component separator is correctly rejected.
    if !dest.starts_with(base) {
        return Err(anyhow!(
            "archive entry '{}' would escape the destination directory",
            rel
        ));
    }
    Ok(dest)
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
    materialized_index_data: Option<&[MaterializedIndexData]>,
    staged: bool,
    verbose: output::VerboseLogger,
) -> Result<()> {
    if let Some(index_data) = materialized_index_data {
        anyhow::ensure!(
            index_data.len() == records.len(),
            "materialized index data length {} does not match records length {}",
            index_data.len(),
            records.len()
        );
    }
    verbose.log(format!(
        "index build: opening LanceDB store path={}",
        lancedb_store::lancedb_root(&workspace.root).display()
    ));
    let lancedb = lancedb_store::LanceDbStore::open_or_create(&workspace.root)
        .with_context(|| "failed to open LanceDB store")?;

    verbose.log("index build: ensuring LanceDB tables");
    lancedb
        .ensure_tables()
        .with_context(|| "failed to ensure LanceDB tables")?;

    let scan_options_json = serde_json::to_string(&manifest.scan_options).unwrap_or_default();

    verbose.log(format!(
        "index build: replacing LanceDB rows snapshot_id={}",
        manifest.snapshot_id
    ));
    lancedb
        .delete_snapshot_rows(&manifest.snapshot_id)
        .with_context(|| "failed to replace old LanceDB snapshot rows")?;

    verbose.log("index build: writing LanceDB snapshot");
    lancedb
        .write_snapshot(lancedb_store::SnapshotWrite {
            snapshot_id: &manifest.snapshot_id,
            snapshot_key: &manifest.snapshot_key,
            schema_version: manifest.schema_version,
            tool_version: &manifest.tool_version,
            repo_root: &manifest.repo_root,
            head: manifest.head.as_deref(),
            dirty: manifest.dirty,
            source: &manifest.source,
            scan_options_json: &scan_options_json,
            file_count: manifest.file_count as u32,
            created_at_epoch_ms: manifest.created_at_epoch_ms as u64,
        })
        .with_context(|| "failed to write snapshot to LanceDB")?;

    verbose.log(format!(
        "index build: writing LanceDB file catalog records={}",
        records.len()
    ));
    lancedb
        .write_file_catalog(&manifest.snapshot_id, records)
        .with_context(|| "failed to write file catalog to LanceDB")?;

    verbose.log("index build: writing LanceDB file proofs");
    let precomputed_line_offsets = materialized_index_data.map(|index_data| {
        index_data
            .iter()
            .map(|data| data.line_offsets_json.as_str())
            .collect::<Vec<_>>()
    });
    lancedb
        .write_file_proofs_with_line_offsets(
            &manifest.snapshot_id,
            records,
            precomputed_line_offsets.as_deref(),
            Some(&workspace.root),
        )
        .with_context(|| "failed to write file proofs to LanceDB")?;

    if !staged {
        verbose.log("index build: building gram postings from materialized files");
        let index_data =
            materialized_index_data.context("missing materialized index data for gram postings")?;
        let gram_index = gram_index_from_materialized_files(index_data);
        if !gram_index.is_empty() {
            let posting_count = gram_index.values().map(Vec::len).sum::<usize>();
            verbose.log(format!(
                "index build: writing LanceDB gram postings grams={} postings={}",
                gram_index.len(),
                posting_count
            ));
            lancedb
                .write_gram_postings(&manifest.snapshot_id, &gram_index)
                .with_context(|| "failed to write gram postings to LanceDB")?;
        }
    }

    Ok(())
}

fn gram_index_from_materialized_files(
    index_data: &[MaterializedIndexData],
) -> BTreeMap<[u8; 3], Vec<u32>> {
    let mut gram_index: BTreeMap<[u8; 3], Vec<u32>> = BTreeMap::new();
    for (doc_id, data) in index_data.iter().enumerate() {
        for &gram in &data.unique_grams {
            gram_index.entry(gram).or_default().push(doc_id as u32);
        }
    }
    gram_index
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, manifest)
        .with_context(|| format!("failed to serialize manifest to {}", path.display()))?;
    writeln!(file).with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(())
}

fn write_skipped_log(path: &Path, log: SkippedLog) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, &log)
        .with_context(|| format!("failed to serialize skipped log to {}", path.display()))?;
    writeln!(file).with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(())
}

fn read_skipped_log(path: &Path) -> Result<SkippedLog> {
    let data =
        fs::read(path).with_context(|| format!("failed to read skipped log {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("failed to parse skipped log {}", path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> PathBuf {
        PathBuf::from("/repo/.codetrail/remote/snap1")
    }

    #[test]
    fn safe_join_accepts_normal_relative_paths() {
        assert!(safe_join(&base(), "files.parquet").is_ok());
        assert!(safe_join(&base(), "text/docs.idx").is_ok());
        assert!(safe_join(&base(), "text/grams.idx").is_ok());
        assert!(safe_join(&base(), "scip/occurrences.db").is_ok());
        assert!(safe_join(&base(), "graph/petgraph.bin").is_ok());
    }

    #[test]
    fn safe_join_rejects_parent_dir_component() {
        let err = safe_join(&base(), "../../escape.txt").unwrap_err();
        assert!(err.to_string().contains(".."), "error: {err}");

        let err = safe_join(&base(), "../sibling/file.txt").unwrap_err();
        assert!(err.to_string().contains(".."), "error: {err}");

        let err = safe_join(&base(), "subdir/../../file.txt").unwrap_err();
        assert!(err.to_string().contains(".."), "error: {err}");
    }

    #[test]
    fn safe_join_rejects_absolute_paths() {
        let err = safe_join(&base(), "/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("absolute"), "error: {err}");
    }

    #[test]
    fn safe_join_rejects_empty_path() {
        let err = safe_join(&base(), "").unwrap_err();
        assert!(err.to_string().contains("empty"), "error: {err}");
    }

    #[test]
    fn checksum_validation_rejects_unlisted_archive_entries() {
        let mut entries = std::collections::HashMap::new();
        entries.insert("checksums.txt".to_string(), Vec::new());
        entries.insert("manifest.json".to_string(), b"{}".to_vec());
        entries.insert("text/docs.idx".to_string(), b"unverified".to_vec());

        let mut hasher = Sha256::new();
        hasher.update(b"{}");
        let mut expected = std::collections::HashMap::new();
        expected.insert(
            "manifest.json".to_string(),
            format!("{:x}", hasher.finalize()),
        );

        let err = verify_archive_checksums(&entries, &expected).unwrap_err();
        assert!(
            err.to_string().contains("not listed in checksums.txt"),
            "error: {err}"
        );
    }

    #[test]
    fn safe_join_result_is_under_base() {
        let dest = safe_join(&base(), "text/docs.idx").unwrap();
        assert!(dest.starts_with(&base()));
    }
}
