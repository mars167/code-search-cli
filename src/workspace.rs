use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};
use ignore::WalkBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub hidden: bool,
    pub no_ignore: bool,
    pub lang: Vec<String>,
    pub changed: bool,
    pub cursor: Option<String>,
    pub allow_broad: bool,
    pub limit: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub path: String,
    pub language: String,
    pub size: u64,
    pub mtime_ms: u128,
    #[serde(default)]
    pub mode: u32,
    pub hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCatalogRecord {
    pub path: String,
    pub language: String,
    pub size: u64,
    pub mtime_ms: u128,
    pub mode: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    pub index_status: String,
    pub worktree_status: String,
    pub change_kind: String,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub git_root: Option<PathBuf>,
    pub head: Option<String>,
    pub dirty: bool,
    pub staged_count: usize,
    pub worktree_count: usize,
    pub changed: Vec<ChangedFile>,
    pub snapshot_id: String,
}

impl Workspace {
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let input = path.as_ref();
        let canonical = fs::canonicalize(input)
            .with_context(|| format!("failed to resolve path {}", input.display()))?;
        let git_root = git_output(&canonical, &["rev-parse", "--show-toplevel"])
            .ok()
            .map(PathBuf::from);
        let root = git_root.clone().unwrap_or(canonical);
        let head = git_output(&root, &["rev-parse", "--verify", "HEAD"]).ok();
        let changed = git_status(&root).unwrap_or_default();
        let staged_count = changed
            .iter()
            .filter(|item| item.index_status != " " && item.index_status != "?")
            .count();
        let worktree_count = changed
            .iter()
            .filter(|item| item.worktree_status != " " || item.index_status == "?")
            .count();
        let dirty = staged_count > 0 || worktree_count > 0;
        let snapshot_id = if let Some(head) = &head {
            if dirty {
                format!("worktree:{}", short_hash(head))
            } else {
                format!("commit:{}", short_hash(head))
            }
        } else {
            "worktree:non-git".to_string()
        };

        Ok(Self {
            root,
            git_root,
            head,
            dirty,
            staged_count,
            worktree_count,
            changed,
            snapshot_id,
        })
    }

    pub fn rel_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    pub fn abs_path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn scan_catalog(&self, opts: &ScanOptions) -> Result<Vec<FileCatalogRecord>> {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(!opts.hidden)
            .ignore(!opts.no_ignore)
            .git_ignore(!opts.no_ignore)
            .git_global(!opts.no_ignore)
            .git_exclude(!opts.no_ignore)
            .parents(!opts.no_ignore);

        let mut files = Vec::new();
        for entry in builder.build() {
            let entry = entry?;
            let path = entry.path();
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && should_skip_dir(path, opts.no_ignore) {
                continue;
            }
            if !file_type.is_file() || should_skip_path(path, opts.no_ignore) {
                continue;
            }

            let rel = self.rel_path(path);
            if !matches_filters(&rel, &opts.include, &opts.exclude) {
                continue;
            }
            let language = language_for_path(path).to_string();
            if !matches_lang(&language, &opts.lang) {
                continue;
            }
            if opts.changed && !self.changed.iter().any(|changed| changed.path == rel) {
                continue;
            }
            if is_probably_binary(path) {
                continue;
            }
            let metadata = fs::metadata(path)?;
            files.push(FileCatalogRecord {
                path: rel,
                language,
                size: metadata.len(),
                mtime_ms: mtime_ms(&metadata),
                mode: file_mode(&metadata),
            });
            if opts.limit > 0 && files.len() >= opts.limit {
                break;
            }
        }

        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    pub fn materialize_proofs(&self, catalog: &[FileCatalogRecord]) -> Result<Vec<FileRecord>> {
        let mut records = catalog
            .par_iter()
            .map(|file| -> Result<FileRecord> {
                let path = self.abs_path(&file.path);
                let content =
                    fs::read(&path).with_context(|| format!("failed to read {}", file.path))?;
                Ok(FileRecord {
                    path: file.path.clone(),
                    language: file.language.clone(),
                    size: file.size,
                    mtime_ms: file.mtime_ms,
                    mode: file.mode,
                    hash: format!("blake3:{}", blake3::hash(&content).to_hex()),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        records.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(records)
    }

    pub fn scan_files(&self, opts: &ScanOptions) -> Result<Vec<FileRecord>> {
        let catalog = self.scan_catalog(opts)?;
        self.materialize_proofs(&catalog)
    }

    pub fn scan_summary(&self, opts: &ScanOptions) -> Result<Value> {
        let mut generated_skipped = 0_u64;
        let mut binary_skipped = 0_u64;
        let mut unreadable_skipped = 0_u64;
        let mut total_seen = 0_u64;
        let mut included = 0_u64;

        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(!opts.hidden)
            .ignore(!opts.no_ignore)
            .git_ignore(!opts.no_ignore)
            .git_global(!opts.no_ignore)
            .git_exclude(!opts.no_ignore)
            .parents(!opts.no_ignore);

        for entry in builder.build() {
            let entry = entry?;
            let path = entry.path();
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if should_skip_dir(path, opts.no_ignore) && is_generated_path(path) {
                    generated_skipped += 1;
                }
                continue;
            }
            if !file_type.is_file() || should_skip_path(path, opts.no_ignore) {
                if file_type.is_file() && is_generated_path(path) && !opts.no_ignore {
                    generated_skipped += 1;
                }
                continue;
            }

            total_seen += 1;
            let rel = self.rel_path(path);
            if !matches_filters(&rel, &opts.include, &opts.exclude) {
                continue;
            }
            let language = language_for_path(path).to_string();
            if !matches_lang(&language, &opts.lang) {
                continue;
            }
            if opts.changed && !self.changed.iter().any(|changed| changed.path == rel) {
                continue;
            }
            match probably_binary_result(path) {
                Ok(true) => binary_skipped += 1,
                Ok(false) => included += 1,
                Err(_) => unreadable_skipped += 1,
            }
        }

        Ok(json!({
            "totalSeen": total_seen,
            "includedCount": included,
            "skippedCount": generated_skipped + binary_skipped + unreadable_skipped,
            "skipped": {
                "generated": generated_skipped,
                "binary": binary_skipped,
                "unreadable": unreadable_skipped
            }
        }))
    }
}

pub fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| "failed to run git")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn git_status(root: &Path) -> Result<Vec<ChangedFile>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1"])
        .output()
        .with_context(|| "failed to run git status")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let mut changed = Vec::new();
    for line in output.lines() {
        if line.len() < 3 {
            continue;
        }
        let index_status = line[0..1].to_string();
        let worktree_status = line[1..2].to_string();
        let untracked = index_status == "?";
        let staged = index_status != " " && index_status != "?";
        let unstaged = !untracked && worktree_status != " ";
        let change_kind = if untracked {
            "untracked"
        } else if staged && unstaged {
            "staged_and_unstaged"
        } else if staged {
            "staged"
        } else {
            "unstaged"
        }
        .to_string();
        let path = line[3..]
            .rsplit_once(" -> ")
            .map(|(_, new_path)| new_path)
            .unwrap_or(&line[3..])
            .trim()
            .to_string();
        changed.push(ChangedFile {
            path,
            index_status,
            worktree_status,
            change_kind,
            staged,
            unstaged,
            untracked,
        });
    }
    Ok(changed)
}

pub fn read_staged_blob(root: &Path, path: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(format!(":{path}"))
        .output()
        .with_context(|| "failed to read staged blob")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git show :{} failed: {}",
            path,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

pub fn tracked_files(root: &Path) -> Result<Vec<String>> {
    let output = git_output(root, &["ls-files"])?;
    Ok(output.lines().map(ToString::to_string).collect())
}

pub fn staged_tree(root: &Path) -> Option<String> {
    git_output(root, &["write-tree"]).ok()
}

pub fn language_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "java" => "java",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        _ => "text",
    }
}

fn should_skip_dir(path: &Path, no_ignore: bool) -> bool {
    should_skip_path(path, no_ignore)
}

fn should_skip_path(path: &Path, no_ignore: bool) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        matches!(value.as_ref(), ".git" | ".code-search")
            || (!no_ignore
                && matches!(value.as_ref(), "target" | "node_modules" | "dist" | ".next"))
    })
}

pub fn matches_filters(path: &str, include: &[String], exclude: &[String]) -> bool {
    if exclude.iter().any(|pattern| path.contains(pattern)) {
        return false;
    }
    include.is_empty() || include.iter().any(|pattern| path.contains(pattern))
}

pub fn matches_lang(language: &str, lang: &[String]) -> bool {
    lang.is_empty()
        || lang
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(language))
}

fn is_generated_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        matches!(value.as_ref(), "target" | "node_modules" | "dist" | ".next")
    })
}

fn is_probably_binary(path: &Path) -> bool {
    probably_binary_result(path).unwrap_or(true)
}

fn probably_binary_result(path: &Path) -> Result<bool> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::with_capacity(8192);
    match file.by_ref().take(8192).read_to_end(&mut bytes) {
        Ok(_) => Ok(bytes.iter().any(|byte| *byte == 0)),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn mtime_ms(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(unix)]
pub(crate) fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode()
}

#[cfg(not(unix))]
pub(crate) fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

fn short_hash(value: &str) -> String {
    value.chars().take(12).collect()
}
