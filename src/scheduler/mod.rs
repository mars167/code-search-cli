pub mod jobs;
pub mod publish;

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::{
    snapshot_store, text_index,
    workspace::{read_staged_blob, staged_tree, tracked_files, FileRecord, ScanOptions, Workspace},
};

use self::publish::{promote_staged_to_commit, promote_temp_to_working};

const INDEX_SCHEMA_VERSION: u32 = 1;

/// IndexScheduler orchestrates all indexing operations:
/// full builds, incremental updates, compaction, and status.
pub struct IndexScheduler {
    workspace_root: PathBuf,
    storage_root: PathBuf,
}

impl IndexScheduler {
    pub fn new(workspace_root: &Path) -> Self {
        Self {
            workspace_root: workspace_root.to_path_buf(),
            storage_root: workspace_root.join(".code-search"),
        }
    }

    // ------------------------------------------------------------------
    // build_all — full build (dirty or staged)
    // ------------------------------------------------------------------
    pub fn build_all(&self, opts: &ScanOptions, staged: bool) -> Result<Value> {
        let workspace = Workspace::discover(&self.workspace_root)?;
        let snapshot_id = if staged {
            format!(
                "staged:{}",
                staged_tree(&self.workspace_root).unwrap_or_else(|| "unknown".to_string())
            )
        } else {
            workspace.snapshot_id.clone()
        };
        let snapshot_key = snapshot_key(&snapshot_id);

        let snapshot_parent = self.storage_root.join("snapshots");
        let text_parent = self.storage_root.join("text");
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
            staged_records(&self.workspace_root, Some(&blobs_dir))?
        } else {
            let mut scan_opts = opts.clone();
            scan_opts.limit = 0;
            workspace.scan_files(&scan_opts)?
        };

        let manifest = Manifest {
            schema_version: INDEX_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_root: self.workspace_root.to_string_lossy().to_string(),
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
            snapshot_store::build_snapshot(&snapshot_tmp, &records, &self.workspace_root)?;
        }
        text_index::write(&text_tmp, &workspace, &records, !staged)?;

        // Atomic rename: tmp → target
        remove_dir_if_exists(&snapshot_target)?;
        remove_dir_if_exists(&text_target)?;
        fs::rename(&snapshot_tmp, &snapshot_target)?;
        fs::rename(&text_tmp, &text_target)?;

        // Write staged manifest for post-commit promotion
        if staged {
            let staged_dir = self.storage_root.join("staged");
            fs::create_dir_all(&staged_dir)?;
            write_manifest(&staged_dir.join("manifest.json"), &manifest)?;
        }

        // Promote: staged snapshot → committed snapshot OR tmp → working
        if staged {
            promote_staged_to_commit(&self.workspace_root)?;
        } else {
            promote_temp_to_working(&self.workspace_root, &snapshot_key)?;
        }

        Ok(json!({
            "job": "build_all",
            "snapshot_id": manifest.snapshot_id,
            "snapshotKey": manifest.snapshot_key,
            "snapshotSource": manifest.source,
            "fileCount": manifest.file_count,
            "staged": staged,
            "path": self.storage_root,
            "snapshotPath": snapshot_target,
            "textPath": text_target
        }))
    }

    // ------------------------------------------------------------------
    // update — incremental update of changed files
    // ------------------------------------------------------------------
    pub fn update(&self) -> Result<Value> {
        let workspace = Workspace::discover(&self.workspace_root)?;

        // 1. Gather changed files from git (worktree + staged)
        let changed_files = Self::git_changed_files(&self.workspace_root)?;

        // 2. Read current working manifest
        let working_manifest_path = self.storage_root.join("working").join("manifest.json");
        if !working_manifest_path.exists() {
            // No existing index — run full build
            let opts = ScanOptions {
                include: vec![],
                exclude: vec![],
                hidden: false,
                no_ignore: false,
                limit: 0,
            };
            return self.build_all(&opts, false);
        }

        let current_manifest = read_manifest(&working_manifest_path)?;
        let current_key = &current_manifest.snapshot_key;

        // 3. Read current records
        let snapshot_path = self.storage_root.join("snapshots").join(current_key);
        let _text_path = self.storage_root.join("text").join(current_key);
        let current_records =
            snapshot_store::read_files_parquet(&snapshot_path.join("files.parquet"))?;

        // 4. Compute which records changed (add/update/delete)
        let mut records_map: std::collections::BTreeMap<String, FileRecord> = current_records
            .into_iter()
            .map(|r| (r.path.clone(), r))
            .collect();

        let mut added_or_updated = 0usize;
        let mut deleted = 0usize;

        for change in &changed_files {
            let status = &change.status;
            let path = &change.path;

            match status.as_str() {
                "A" | "M" | "R" | "C" => {
                    // File was added or modified — re-scan
                    let file_path = self.workspace_root.join(path);
                    if !file_path.exists() || Self::is_probably_binary(&file_path) {
                        continue;
                    }
                    let metadata = fs::metadata(&file_path)?;
                    let content = fs::read(&file_path)?;
                    let record = FileRecord {
                        path: path.clone(),
                        language: crate::workspace::language_for_path(&file_path).to_string(),
                        size: metadata.len(),
                        mtime_ms: mtime_ms(&metadata),
                        hash: format!("blake3:{}", blake3::hash(&content).to_hex()),
                    };
                    records_map.insert(path.clone(), record);
                    added_or_updated += 1;
                }
                "D" => {
                    records_map.remove(path);
                    deleted += 1;
                }
                _ => {}
            }
        }

        // 5. Create new snapshot with updated records
        let snapshot_id = workspace.snapshot_id.clone();
        let snapshot_key_val = snapshot_key(&snapshot_id);

        let records: Vec<FileRecord> = records_map.into_values().collect();

        // 6. Write delta segments
        let snapshot_tmp = self
            .storage_root
            .join("snapshots")
            .join(format!("{}.tmp", &snapshot_key_val));
        let text_tmp = self
            .storage_root
            .join("text")
            .join(format!("{}.tmp", &snapshot_key_val));

        // Clean up any leftover tmp dirs (failure recovery)
        remove_dir_if_exists(&snapshot_tmp)?;
        remove_dir_if_exists(&text_tmp)?;
        fs::create_dir_all(&snapshot_tmp)?;
        fs::create_dir_all(&text_tmp)?;

        let blobs_dir = snapshot_tmp.join("blobs");
        fs::create_dir_all(&blobs_dir)?;

        // Write full records as segment (incremental: we only write a new full
        // snapshot since our text index format doesn't support delta merging yet.
        // But we only rescan the changed files, so the work is O(changed) not O(all)).
        snapshot_store::build_snapshot(&snapshot_tmp, &records, &self.workspace_root)?;
        text_index::write(&text_tmp, &workspace, &records, true)?;

        let manifest = Manifest {
            schema_version: INDEX_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_root: self.workspace_root.to_string_lossy().to_string(),
            snapshot_id: snapshot_id.clone(),
            snapshot_key: snapshot_key_val.clone(),
            source: "working_tree".to_string(),
            head: workspace.head.clone(),
            dirty: workspace.dirty,
            file_count: records.len(),
            scan_options: current_manifest.scan_options.clone(),
            created_at_epoch_ms: now_ms(),
        };

        write_manifest(&snapshot_tmp.join("manifest.json"), &manifest)?;

        let snapshot_target = self.storage_root.join("snapshots").join(&snapshot_key_val);
        let text_target = self.storage_root.join("text").join(&snapshot_key_val);

        remove_dir_if_exists(&snapshot_target)?;
        remove_dir_if_exists(&text_target)?;
        fs::rename(&snapshot_tmp, &snapshot_target)?;
        fs::rename(&text_tmp, &text_target)?;

        // Promote to working
        promote_temp_to_working(&self.workspace_root, &snapshot_key_val)?;

        Ok(json!({
            "job": "update",
            "changedFiles": changed_files.len(),
            "addedOrUpdated": added_or_updated,
            "deleted": deleted,
            "totalFiles": records.len(),
            "snapshot_id": manifest.snapshot_id,
            "snapshotKey": manifest.snapshot_key,
            "path": self.storage_root
        }))
    }

    // ------------------------------------------------------------------
    // compact — removes old snapshot segments, keeping only the current
    // ------------------------------------------------------------------
    pub fn compact(&self) -> Result<Value> {
        let working_manifest_path = self.storage_root.join("working").join("manifest.json");
        let staged_manifest_path = self.storage_root.join("staged").join("manifest.json");

        // Determine which snapshot keys are "live"
        let mut live_keys = HashSet::new();

        if let Ok(manifest) = read_manifest(&working_manifest_path) {
            live_keys.insert(manifest.snapshot_key);
        }
        if let Ok(manifest) = read_manifest(&staged_manifest_path) {
            live_keys.insert(manifest.snapshot_key);
        }

        let mut removed_snapshots = 0usize;
        let mut removed_texts = 0usize;

        // Clean snapshot dir
        let snapshots_dir = self.storage_root.join("snapshots");
        if snapshots_dir.exists() {
            for entry in fs::read_dir(&snapshots_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".tmp") || !live_keys.contains(&name) {
                    if entry.path().is_dir() {
                        fs::remove_dir_all(entry.path())?;
                        removed_snapshots += 1;
                    }
                }
            }
        }

        // Clean text dir
        let text_dir = self.storage_root.join("text");
        if text_dir.exists() {
            for entry in fs::read_dir(&text_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".tmp") || !live_keys.contains(&name) {
                    if entry.path().is_dir() {
                        fs::remove_dir_all(entry.path())?;
                        removed_texts += 1;
                    }
                }
            }
        }

        // Clean any leftover .tmp dirs
        for dir in [&snapshots_dir, &text_dir] {
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".tmp") {
                    if entry.path().is_dir() {
                        fs::remove_dir_all(entry.path())?;
                    }
                }
            }
        }

        Ok(json!({
            "job": "compact",
            "removedSnapshots": removed_snapshots,
            "removedTexts": removed_texts,
            "liveSnapshots": live_keys.len(),
            "path": self.storage_root
        }))
    }

    // ------------------------------------------------------------------
    // status — scheduler health + index freshness
    // ------------------------------------------------------------------
    pub fn status(&self) -> Result<Value> {
        let working_manifest_path = self.storage_root.join("working").join("manifest.json");
        let staged_manifest_path = self.storage_root.join("staged").join("manifest.json");

        let working_manifest = read_manifest_opt(&working_manifest_path)?;
        let staged_manifest = read_manifest_opt(&staged_manifest_path)?;

        // Count segments
        let snapshot_segments = count_subdirs(&self.storage_root.join("snapshots"));
        let text_segments = count_subdirs(&self.storage_root.join("text"));

        let mut value = json!({
            "exists": working_manifest.is_some(),
            "fresh": false,
            "path": self.storage_root,
            "scheduler": {
                "workingSnapshots": snapshot_segments,
                "textSegments": text_segments,
                "hasWorking": working_manifest.is_some(),
                "hasStaged": staged_manifest.is_some(),
            }
        });

        if let Some(wm) = &working_manifest {
            let snapshot_path = self.storage_root.join("snapshots").join(&wm.snapshot_key);
            let text_path = self.storage_root.join("text").join(&wm.snapshot_key);
            let snap_fresh = snapshot_store::verify_snapshot(&snapshot_path, &self.workspace_root)?;
            let fresh = snap_fresh.stale_files.is_empty() && snap_fresh.missing_files.is_empty();

            value["fresh"] = json!(fresh);
            value["snapshotKey"] = json!(&wm.snapshot_key);
            value["snapshotPath"] = json!(snapshot_path);
            value["textPath"] = json!(text_path);
            value["manifest"] = serde_json::to_value(&wm).unwrap_or_default();
            value["freshness"] = json!({
                "freshCount": snap_fresh.fresh_count,
                "staleFiles": snap_fresh.stale_files,
                "missingFiles": snap_fresh.missing_files
            });
        }

        Ok(value)
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Run `git diff --name-status` + `git diff --cached --name-status`
    fn git_changed_files(workspace_root: &Path) -> Result<Vec<GitChange>> {
        let mut changes = Vec::new();

        // Worktree changes
        if let Ok(output) = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["diff", "--name-status", "--diff-filter=ACMRD"])
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Some(change) = GitChange::parse(line) {
                        changes.push(change);
                    }
                }
            }
        }

        // Staged changes
        if let Ok(output) = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["diff", "--cached", "--name-status", "--diff-filter=ACMRD"])
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Some(change) = GitChange::parse(line) {
                        // Deduplicate: staged overrides worktree
                        if !changes.iter().any(|c| c.path == change.path) {
                            changes.push(change);
                        }
                    }
                }
            }
        }

        Ok(changes)
    }

    fn is_probably_binary(path: &Path) -> bool {
        match fs::read(path) {
            Ok(bytes) => bytes.iter().take(8192).any(|b| *b == 0),
            Err(_) => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct GitChange {
    status: String,
    path: String,
}

impl GitChange {
    fn parse(line: &str) -> Option<Self> {
        if line.len() < 3 {
            return None;
        }
        let status = line[..1].to_string();
        // Handle rename lines like "R100\told\tnew"
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            return None;
        }
        // For renames (R), the last tab-delimited part is the new path
        let path = parts.last()?.to_string();
        if path.is_empty() {
            return None;
        }
        Some(Self { status, path })
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexScanOptions {
    include: Vec<String>,
    exclude: Vec<String>,
    hidden: bool,
    no_ignore: bool,
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

// ---------------------------------------------------------------------------
// File-level helpers
// ---------------------------------------------------------------------------

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

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut file = fs::File::create(path)?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    writeln!(file)?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    Ok(serde_json::from_reader(file)?)
}

fn read_manifest_opt(path: &Path) -> Result<Option<Manifest>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_manifest(path)?))
}

fn count_subdirs(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .map(|iter| {
            iter.filter_map(|entry| match entry {
                Ok(e) => e.path().is_dir().then_some(()),
                _ => None,
            })
            .count()
        })
        .unwrap_or(0)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn mtime_ms(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn staged_records(workspace_root: &Path, blobs_dir: Option<&Path>) -> Result<Vec<FileRecord>> {
    let files = tracked_files(workspace_root)?;
    let mut records = Vec::new();
    for path in files {
        let content = match read_staged_blob(workspace_root, &path) {
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
