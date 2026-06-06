//! Range diff proof API.
//!
//! If CodeTrail is to become Codex's sole code evidence entry point, review
//! and modification stages need staged/unstaged/HEAD/worktree range-level diff
//! proof. This module defines the internal schema and proof alignment contract.
//!
//! Diff evidence must align with query freshness proof: a diff returned for
//! snapshot `S1` must not be interpreted against query evidence from `S2`.


use serde::{Deserialize, Serialize};

use crate::generation_manifest::FreshnessGate;

// ── Diff target ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffTarget {
    Staged,
    Unstaged,
    HeadToWorktree,
    HeadToStaged,
    StagedToWorktree,
}

// ── File diff status ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Binary,
}

// ── Diff hunk ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub old_start_line: u32,
    pub old_line_count: u32,
    pub new_start_line: u32,
    pub new_line_count: u32,
    pub context: String,
}

// ── File diff ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub file_path: String,
    pub status: FileDiffStatus,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<DiffHunk>,
    pub truncated: bool,
    pub caveats: Vec<DiffCaveat>,
}

// ── Diff result ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub schema_version: u32,
    pub target: DiffTarget,
    pub snapshot_id: String,
    pub worktree_dirty: bool,
    pub files: Vec<FileDiff>,
    pub page: DiffPage,
    pub caveats: Vec<DiffCaveat>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffPage {
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub has_more: bool,
}

// ── Diff caveats ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffCaveat {
    pub code: DiffCaveatCode,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffCaveatCode {
    DiffProofTruncated,
    DiffProofPartial,
    BinaryFileNotDisplayed,
    LargeFileTruncated,
    RenameDetected,
    ConflictDetected,
}

// ── Diff options ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DiffOptions {
    pub target: DiffTarget,
    pub max_files: usize,
    pub max_hunks_per_file: usize,
    pub max_context_lines: usize,
    pub paths: Vec<String>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            target: DiffTarget::Unstaged,
            max_files: 500,
            max_hunks_per_file: 100,
            max_context_lines: 3,
            paths: Vec::new(),
        }
    }
}

// ── Freshness alignment ─────────────────────────────────────────────────────

/// Check that diff proof aligns with a given freshness gate.
///
/// If the freshness gate reports stale semantic indices for files touched by
/// the diff, consumers should treat semantic evidence for those files as
/// potentially outdated and prefer diff-level source facts.
pub fn diff_freshness_alignment(diff: &DiffResult, gate: &FreshnessGate) -> Vec<String> {
    let blocked = gate.blocked_root_ids();
    let mut misaligned = Vec::new();
    for file in &diff.files {
        // Simple heuristic: if any root is blocked, the whole workspace may
        // have stale semantic facts. A more precise implementation would map
        // file paths to project roots via ProjectGraph.
        if !blocked.is_empty() {
            misaligned.push(format!(
                "diff file {} may reference stale semantic evidence (blocked roots: {:?})",
                file.file_path, blocked
            ));
        }
    }
    misaligned
}

/// Produce paginated diff results.
pub fn paginate(files: Vec<FileDiff>, offset: usize, limit: usize) -> (Vec<FileDiff>, DiffPage) {
    let total = files.len();
    let start = offset.min(total);
    let end = (offset + limit).min(total);
    let page = DiffPage {
        offset: start,
        limit,
        total,
        has_more: end < total,
    };
    (files[start..end].to_vec(), page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_target_variants_cover_git_states() {
        let targets = [
            DiffTarget::Staged,
            DiffTarget::Unstaged,
            DiffTarget::HeadToWorktree,
            DiffTarget::HeadToStaged,
            DiffTarget::StagedToWorktree,
        ];
        assert_eq!(targets.len(), 5);
    }

    #[test]
    fn file_diff_status_variants_include_binary_and_rename() {
        let statuses = [
            FileDiffStatus::Added,
            FileDiffStatus::Modified,
            FileDiffStatus::Deleted,
            FileDiffStatus::Renamed,
            FileDiffStatus::Binary,
        ];
        assert_eq!(statuses.len(), 5);
    }

    #[test]
    fn paginate_middle_page() {
        let files: Vec<FileDiff> = (0..10)
            .map(|i| FileDiff {
                file_path: format!("file_{i}.rs"),
                status: FileDiffStatus::Modified,
                old_path: None,
                new_path: None,
                hunks: Vec::new(),
                truncated: false,
                caveats: Vec::new(),
            })
            .collect();

        let (page, meta) = paginate(files, 3, 4);
        assert_eq!(page.len(), 4);
        assert_eq!(meta.offset, 3);
        assert_eq!(meta.total, 10);
        assert!(meta.has_more);
    }

    #[test]
    fn paginate_last_page_no_more() {
        let files: Vec<FileDiff> = (0..5)
            .map(|i| FileDiff {
                file_path: format!("file_{i}.rs"),
                status: FileDiffStatus::Modified,
                old_path: None,
                new_path: None,
                hunks: Vec::new(),
                truncated: false,
                caveats: Vec::new(),
            })
            .collect();
        let (_, meta) = paginate(files, 3, 5);
        assert!(!meta.has_more);
    }

    #[test]
    fn diff_caveat_codes_cover_truncation_and_conflict() {
        let codes = [
            DiffCaveatCode::DiffProofTruncated,
            DiffCaveatCode::DiffProofPartial,
            DiffCaveatCode::BinaryFileNotDisplayed,
            DiffCaveatCode::LargeFileTruncated,
            DiffCaveatCode::RenameDetected,
            DiffCaveatCode::ConflictDetected,
        ];
        assert_eq!(codes.len(), 6);
    }

    #[test]
    fn diff_freshness_alignment_detects_blocked_roots() {
        use crate::generation_manifest::{GenerationManifest, ProofHashes};
        use crate::project_graph::{ProjectLanguage, ProjectRoot, ProjectRootKind};

        let root = ProjectRoot {
            id: "go:svc".to_string(),
            path: ".".to_string(),
            language: ProjectLanguage::Go,
            kind: ProjectRootKind::GoModule,
            markers: Vec::new(),
        };
        let hashes = ProofHashes {
            provider_version_hash: "a".to_string(),
            environment_hash: "b".to_string(),
            source_proof_hash: "c".to_string(),
            config_proof_hash: "d".to_string(),
        };
        let mut manifest = crate::generation_manifest::new_manifest(&root, "gopls", &hashes);
        manifest.state = crate::generation_manifest::ManifestState::Stale;
        let gate = FreshnessGate::from_manifests(vec![manifest]);

        let diff = DiffResult {
            schema_version: 1,
            target: DiffTarget::Unstaged,
            snapshot_id: "abc".to_string(),
            worktree_dirty: true,
            files: vec![FileDiff {
                file_path: "pkg/handler.go".to_string(),
                status: FileDiffStatus::Modified,
                old_path: None,
                new_path: None,
                hunks: Vec::new(),
                truncated: false,
                caveats: Vec::new(),
            }],
            page: DiffPage {
                offset: 0,
                limit: 10,
                total: 1,
                has_more: false,
            },
            caveats: Vec::new(),
        };

        let misaligned = diff_freshness_alignment(&diff, &gate);
        assert!(!misaligned.is_empty());
    }
}
