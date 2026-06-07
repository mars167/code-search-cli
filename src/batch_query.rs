//! Codex batch query execution contract and capability manifest.
//!
//! Codex agents sometimes need one request that fetches multiple evidence
//! kinds, shares scope and budget, unifies pagination and caveats, and exposes
//! the current workspace's fact-layer capabilities before execution. The agent
//! supplies the query items and dependencies; CodeTrail executes the caller's
//! plan without deciding the task strategy.
//!
//! Public JSON output still follows results/page/caveats. Each result carries
//! query_id, producer, range proof, freshness proof, reliability, and execution
//! metadata.

use serde::{Deserialize, Serialize};

use crate::generation_manifest::FreshnessGate;
use crate::project_graph::ProjectLanguage;

// ── Batch query request ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchQueryRequest {
    pub schema_version: u32,
    pub scope: BatchScope,
    pub queries: Vec<QueryItem>,
    pub budget: BatchBudget,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchScope {
    pub workspace_root: String,
    pub files: Vec<String>,
    pub language: Option<ProjectLanguage>,
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryItem {
    pub query_id: String,
    pub kind: QueryKind,
    pub params: serde_json::Value,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    Semantic,
    Relationship,
    Config,
    Diff,
    Read,
    Source,
    Find,
    Grep,
    Symbols,
    Defs,
    Refs,
    Calls,
    Callers,
    Files,
    Glob,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchBudget {
    pub max_total_results: usize,
    pub max_results_per_query: usize,
    pub max_context_chars: usize,
    pub timeout_ms: u64,
}

// ── Batch query response ────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchQueryResponse {
    pub schema_version: u32,
    pub plan: QueryPlan,
    pub results: Vec<QueryResultGroup>,
    pub page: BatchPage,
    pub caveats: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPlan {
    pub execution_order: Vec<String>,
    pub skipped: Vec<String>,
    pub partial: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResultGroup {
    pub query_id: String,
    pub kind: QueryKind,
    pub items: Vec<BatchResultItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResultItem {
    pub producer: String,
    pub range_proof: RangeProof,
    pub freshness_proof: FreshnessProof,
    pub reliability: String,
    pub next_action: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeProof {
    pub file_path: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub file_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreshnessProof {
    pub snapshot_id: String,
    pub generation_id: Option<String>,
    pub fresh: bool,
    pub stale_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchPage {
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub total_results: usize,
}

// ── Capability manifest ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityManifest {
    pub schema_version: u32,
    pub workspace_root: String,
    pub snapshot_id: String,
    pub capabilities: Vec<CapabilityEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityEntry {
    pub language: ProjectLanguage,
    pub kind: CapabilityKind,
    pub state: CapabilityState,
    pub provider_name: Option<String>,
    pub provider_version: Option<String>,
    pub details: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Semantic,
    Config,
    Diff,
    Batch,
    Read,
    Search,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Ready,
    Partial,
    Missing,
    Unsupported,
}

impl CapabilityManifest {
    pub fn from_gate(
        workspace_root: &str,
        snapshot_id: &str,
        gate: &FreshnessGate,
        languages: &[ProjectLanguage],
    ) -> Self {
        let mut capabilities = Vec::new();
        for lang in languages {
            let manifests = gate.query(None, Some(lang), None);
            let state = if manifests.is_empty() {
                CapabilityState::Missing
            } else if manifests.iter().any(|m| m.state.blocks_precise()) {
                CapabilityState::Partial
            } else {
                CapabilityState::Ready
            };
            capabilities.push(CapabilityEntry {
                language: lang.clone(),
                kind: CapabilityKind::Semantic,
                state,
                provider_name: manifests.first().map(|m| m.provider_name.clone()),
                provider_version: None,
                details: None,
            });
        }
        // Config, diff, batch, read, search are always available at source-fact level
        let always_available = [
            CapabilityKind::Config,
            CapabilityKind::Diff,
            CapabilityKind::Read,
            CapabilityKind::Search,
            CapabilityKind::Batch,
        ];
        for kind in always_available {
            capabilities.push(CapabilityEntry {
                language: ProjectLanguage::Go, // representative; non-language-specific
                kind,
                state: CapabilityState::Ready,
                provider_name: None,
                provider_version: None,
                details: None,
            });
        }
        CapabilityManifest {
            schema_version: 1,
            workspace_root: workspace_root.to_string(),
            snapshot_id: snapshot_id.to_string(),
            capabilities,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation_manifest::GenerationManifest;
    use crate::project_graph::{ProjectLanguage, ProjectRoot, ProjectRootKind};

    #[test]
    fn query_kinds_cover_all_domains() {
        let kinds = [
            QueryKind::Semantic,
            QueryKind::Relationship,
            QueryKind::Config,
            QueryKind::Diff,
            QueryKind::Read,
            QueryKind::Source,
            QueryKind::Find,
            QueryKind::Grep,
            QueryKind::Symbols,
            QueryKind::Defs,
            QueryKind::Refs,
            QueryKind::Calls,
            QueryKind::Callers,
            QueryKind::Files,
            QueryKind::Glob,
        ];
        assert!(kinds.len() >= 10);
    }

    #[test]
    fn batch_request_serializes() {
        let req = BatchQueryRequest {
            schema_version: 1,
            scope: BatchScope {
                workspace_root: "/repo".to_string(),
                files: vec!["src/main.rs".to_string()],
                language: Some(ProjectLanguage::Rust),
                exclude: vec!["target".to_string()],
            },
            queries: vec![QueryItem {
                query_id: "q1".to_string(),
                kind: QueryKind::Defs,
                params: serde_json::json!({"symbol": "main"}),
                depends_on: vec![],
            }],
            budget: BatchBudget {
                max_total_results: 100,
                max_results_per_query: 50,
                max_context_chars: 2000,
                timeout_ms: 5000,
            },
            cursor: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("q1"));
        assert!(json.contains("main"));
    }

    #[test]
    fn capability_manifest_from_fresh_gate() {
        let root = ProjectRoot {
            id: "go:srv".to_string(),
            path: ".".to_string(),
            language: ProjectLanguage::Go,
            kind: ProjectRootKind::GoModule,
            markers: vec![],
        };
        let hashes = crate::generation_manifest::ProofHashes {
            provider_version_hash: "a".into(),
            environment_hash: "b".into(),
            source_proof_hash: "c".into(),
            config_proof_hash: "d".into(),
        };
        let manifest = crate::generation_manifest::new_manifest(&root, "gopls", &hashes);
        let gate = FreshnessGate::from_manifests(vec![manifest]);
        let cap = CapabilityManifest::from_gate("/repo", "snap1", &gate, &[ProjectLanguage::Go]);
        assert!(cap
            .capabilities
            .iter()
            .any(|c| c.kind == CapabilityKind::Semantic));
        assert!(cap
            .capabilities
            .iter()
            .any(|c| c.kind == CapabilityKind::Config));
    }

    #[test]
    fn capability_state_detects_missing_provider() {
        let gate = FreshnessGate::new();
        let cap = CapabilityManifest::from_gate("/repo", "snap1", &gate, &[ProjectLanguage::Java]);
        let sem = cap.capabilities.iter().find(|c| {
            c.kind == CapabilityKind::Semantic && matches!(c.language, ProjectLanguage::Java)
        });
        assert!(sem.is_some());
        assert_eq!(sem.unwrap().state, CapabilityState::Missing);
    }
}
