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

use crate::scan_diagnostics::SkippedFile;

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

#[derive(Clone, Debug)]
pub struct CatalogScan {
    pub files: Vec<FileCatalogRecord>,
    pub skipped: Vec<SkippedFile>,
}

#[derive(Clone, Debug)]
pub struct MaterializedProofs {
    pub records: Vec<FileRecord>,
    pub skipped: Vec<SkippedFile>,
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
        let input = crate::path_compat::native_path(path.as_ref());
        let canonical = dunce::canonicalize(&input)
            .with_context(|| format!("failed to resolve path {}", input.display()))?;
        let git_root = git_output(&canonical, &["rev-parse", "--show-toplevel"])
            .ok()
            .map(PathBuf::from)
            .map(|path| {
                dunce::canonicalize(&path)
                    .with_context(|| format!("failed to resolve git root {}", path.display()))
            })
            .transpose()?;
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
        crate::path_compat::relative_path(&self.root, path)
    }

    pub fn abs_path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn scan_catalog(&self, opts: &ScanOptions) -> Result<Vec<FileCatalogRecord>> {
        Ok(self.scan_catalog_with_skips(opts)?.files)
    }

    pub fn scan_catalog_with_skips(&self, opts: &ScanOptions) -> Result<CatalogScan> {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(!opts.hidden)
            .ignore(!opts.no_ignore)
            .git_ignore(!opts.no_ignore)
            .git_global(!opts.no_ignore)
            .git_exclude(!opts.no_ignore)
            .parents(!opts.no_ignore);

        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for entry in builder.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    skipped.push(SkippedFile::with_message(
                        "<unknown>",
                        "walk",
                        "walk_error",
                        error.to_string(),
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && should_skip_dir(path, opts.no_ignore) {
                if !opts.no_ignore && is_generated_path(path) {
                    skipped.push(SkippedFile::new(
                        self.rel_path(path),
                        "catalog",
                        "generated",
                    ));
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if should_skip_path(path, opts.no_ignore) {
                if !opts.no_ignore && is_generated_path(path) {
                    skipped.push(SkippedFile::new(
                        self.rel_path(path),
                        "catalog",
                        "generated",
                    ));
                }
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
            let metadata = match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => continue,
                Err(error) => {
                    skipped.push(SkippedFile::with_message(
                        rel,
                        "catalog",
                        "metadata_error",
                        error.to_string(),
                    ));
                    continue;
                }
            };
            match probably_binary_result(path) {
                Ok(true) => {
                    skipped.push(SkippedFile::new(rel, "catalog", "binary"));
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    skipped.push(SkippedFile::with_message(
                        rel,
                        "catalog",
                        "read_error",
                        error.to_string(),
                    ));
                    continue;
                }
            }
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
        skipped.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
        Ok(CatalogScan { files, skipped })
    }

    pub fn materialize_proofs(&self, catalog: &[FileCatalogRecord]) -> Result<Vec<FileRecord>> {
        Ok(self.materialize_proofs_with_skips(catalog)?.records)
    }

    pub fn materialize_proofs_with_skips(
        &self,
        catalog: &[FileCatalogRecord],
    ) -> Result<MaterializedProofs> {
        let outcomes = catalog
            .par_iter()
            .map(|file| {
                let path = self.abs_path(&file.path);
                file_record_for_catalog(file, || fs::read(&path))
            })
            .collect::<Vec<_>>();
        let mut records = Vec::new();
        let mut skipped = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok(record) => records.push(record),
                Err(skip) => skipped.push(skip),
            }
        }
        records.sort_by(|a, b| a.path.cmp(&b.path));
        skipped.sort_by(|a, b| a.path.cmp(&b.path).then(a.stage.cmp(&b.stage)));
        Ok(MaterializedProofs { records, skipped })
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
            let Ok(entry) = entry else {
                unreadable_skipped += 1;
                continue;
            };
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
        if let Some(file) = parse_git_status_line(line) {
            changed.push(file);
        }
    }
    Ok(changed)
}

fn parse_git_status_line(line: &str) -> Option<ChangedFile> {
    if line.len() < 3 {
        return None;
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
    let path = crate::path_compat::normalize_separators(&path);
    Some(ChangedFile {
        path,
        index_status,
        worktree_status,
        change_kind,
        staged,
        unstaged,
        untracked,
    })
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
        "go" => "go",
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
        matches!(value.as_ref(), ".git" | ".codetrail")
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

#[cfg(test)]
fn file_metadata_for_catalog(
    read_metadata: impl FnOnce() -> std::io::Result<fs::Metadata>,
) -> Option<fs::Metadata> {
    match read_metadata() {
        Ok(metadata) if metadata.is_file() => Some(metadata),
        Ok(_) | Err(_) => None,
    }
}

fn file_record_for_catalog(
    file: &FileCatalogRecord,
    read_file: impl FnOnce() -> std::io::Result<Vec<u8>>,
) -> std::result::Result<FileRecord, SkippedFile> {
    let content = read_file().map_err(|error| {
        SkippedFile::with_message(
            file.path.clone(),
            "materialize",
            "read_error",
            error.to_string(),
        )
    })?;
    Ok(FileRecord {
        path: file.path.clone(),
        language: file.language.clone(),
        size: file.size,
        mtime_ms: file.mtime_ms,
        mode: file.mode,
        hash: format!("blake3:{}", blake3::hash(&content).to_hex()),
    })
}

fn probably_binary_result(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(8192);
    match file.by_ref().take(8192).read_to_end(&mut bytes) {
        Ok(_) => Ok(bytes.contains(&0)),
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

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn catalog_metadata_lookup_errors_skip_the_entry() {
        // Given: Windows reserved device paths such as `nul` can be visible to
        // the walker but fail when statted with OS error 1.
        // When: catalog metadata lookup sees that OS error.
        let metadata = file_metadata_for_catalog(|| Err(io::Error::from_raw_os_error(1)));

        // Then: the caller can skip the entry instead of aborting indexing.
        assert!(metadata.is_none());
    }

    #[test]
    fn catalog_read_errors_skip_the_materialized_record() {
        let file = FileCatalogRecord {
            path: "nul".to_string(),
            language: "text".to_string(),
            size: 0,
            mtime_ms: 0,
            mode: 0,
        };

        let record = file_record_for_catalog(&file, || Err(io::Error::from_raw_os_error(1)));

        let skipped = record.unwrap_err();
        assert_eq!(skipped.path, "nul");
        assert_eq!(skipped.stage, "materialize");
        assert_eq!(skipped.reason, "read_error");
    }

    #[test]
    fn parse_git_status_line_normalizes_windows_separators() {
        let changed = parse_git_status_line(r" M src\main.rs").unwrap();

        assert_eq!(changed.path, "src/main.rs");
        assert_eq!(changed.change_kind, "unstaged");
    }

    #[test]
    fn parse_git_status_line_uses_renamed_destination_with_normalized_separators() {
        let changed = parse_git_status_line(r"R  old\main.rs -> new\main.rs").unwrap();

        assert_eq!(changed.path, "new/main.rs");
        assert_eq!(changed.change_kind, "staged");
    }
}
