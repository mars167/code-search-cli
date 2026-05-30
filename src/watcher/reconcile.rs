use std::{collections::HashMap, fs, path::Path, time::SystemTime};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{lancedb_store, snapshot_store, workspace::FileRecord};
/// Result of a reconcile operation comparing workspace files with the snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResult {
    pub dirty_files: Vec<DirtyFile>,
    pub added_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub stale: bool,
    pub total_files_scanned: usize,
    pub reconciled_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirtyFile {
    pub path: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_hash: Option<String>,
}

/// Run reconcile: compare the current workspace file state against
/// the most recent working snapshot. Returns dirty/added/deleted file lists.
pub fn reconcile(workspace_root: &Path) -> Result<ReconcileResult> {
    let code_search_dir = workspace_root.join(".code-search");
    let working_dir = code_search_dir.join("working");

    // Follow manifest chain to find the actual snapshot
    let snapshot_records: Vec<FileRecord> =
        resolve_snapshot_records(&code_search_dir, &working_dir);

    let snapshot_map: HashMap<String, &FileRecord> = snapshot_records
        .iter()
        .map(|r| (r.path.clone(), r))
        .collect();

    // Walk current workspace files
    let mut current_files: HashMap<String, String> = HashMap::new();

    walk_workspace_for_hashes(workspace_root, &mut current_files)?;

    let mut dirty_files: Vec<DirtyFile> = Vec::new();
    let mut added_files: Vec<String> = Vec::new();
    let mut deleted_files: Vec<String> = Vec::new();

    // Compare current files against snapshot
    for (path, cur_hash) in &current_files {
        if let Some(record) = snapshot_map.get(path) {
            if record.hash != *cur_hash {
                dirty_files.push(DirtyFile {
                    path: path.clone(),
                    reason: "file_hash_mismatch".to_string(),
                    expected_hash: Some(record.hash.clone()),
                    actual_hash: Some(cur_hash.clone()),
                });
            }
        } else {
            added_files.push(path.clone());
        }
    }

    // Find files in snapshot that are no longer on disk
    for (path, _record) in &snapshot_map {
        if !current_files.contains_key(path) {
            let file_path = workspace_root.join(path);
            if !file_path.exists() {
                deleted_files.push(path.clone());
            }
        }
    }

    // Sort for deterministic output
    dirty_files.sort_by(|a, b| a.path.cmp(&b.path));
    added_files.sort();
    deleted_files.sort();

    let total_files = current_files.len();
    let stale = !dirty_files.is_empty() || !added_files.is_empty() || !deleted_files.is_empty();

    Ok(ReconcileResult {
        dirty_files,
        added_files,
        deleted_files,
        stale,
        total_files_scanned: total_files,
        reconciled_at: now_epoch_ms(),
    })
}

/// Walk workspace files, computing blake3 hashes for each file.
/// Skips .git, .code-search, target, node_modules, etc.
pub fn walk_workspace_for_hashes(root: &Path, hashes: &mut HashMap<String, String>) -> Result<()> {
    use ignore::WalkBuilder;

    let mut builder = WalkBuilder::new(root);
    builder.hidden(false).ignore(true).git_ignore(true);

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();

        // Skip directories that should be excluded
        if skip_dir(path) {
            continue;
        }

        if !path.is_file() || super::events::should_skip_path(path) {
            continue;
        }

        // Skip binary files
        if is_probably_binary(path) {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let content = match fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let hash = format!("blake3:{}", blake3::hash(&content).to_hex());
        hashes.insert(rel, hash);
    }

    Ok(())
}

fn skip_dir(path: &Path) -> bool {
    super::events::should_skip_path(path)
}

fn is_probably_binary(path: &Path) -> bool {
    match fs::read(path) {
        Ok(bytes) => bytes.iter().take(8192).any(|byte| *byte == 0),
        Err(_) => true,
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolve the snapshot records by following the manifest chain:
/// 1. Read working/manifest.json to get snapshotKey
/// 2. Read snapshots/<snapshotKey>/files.parquet
/// 3. Fall back to working/files.parquet if available
fn resolve_snapshot_records(code_search_dir: &Path, working_dir: &Path) -> Vec<FileRecord> {
    let manifest_path = working_dir.join("manifest.json");

    // Read snapshot key and id from manifest
    let (snapshot_key, snapshot_id) = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .map(|manifest| {
            let key = manifest
                .get("snapshotKey")
                .or_else(|| manifest.get("snapshot_key"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let id = manifest
                .get("snapshotId")
                .or_else(|| manifest.get("snapshot_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (key, id)
        })
        .unwrap_or((None, None));

    // Try LanceDB first (use snapshot_id, not snapshot_key)
    if let Some(ref id) = snapshot_id {
        let root = code_search_dir.parent().unwrap_or(code_search_dir);
        if lancedb_store::is_available(root) {
            if let Ok(store) = lancedb_store::LanceDbStore::open_or_create(root) {
                if let Ok(records) = store.read_file_records(id) {
                    if !records.is_empty() {
                        return records;
                    }
                }
            }
        }
    }

    // Fallback: try parquet from snapshots/<key>/files.parquet
    if let Some(ref key) = snapshot_key {
        let parquet_path = code_search_dir
            .join("snapshots")
            .join(key)
            .join("files.parquet");
        if parquet_path.exists() {
            return snapshot_store::read_files_parquet(&parquet_path).unwrap_or_default();
        }
    }

    // Last fallback: try working/files.parquet directly
    let working_parquet = working_dir.join("files.parquet");
    if working_parquet.exists() {
        return snapshot_store::read_files_parquet(&working_parquet).unwrap_or_default();
    }

    Vec::new()
}
