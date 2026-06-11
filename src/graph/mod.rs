//! Property-graph backend for call/caller queries.
//!
//! This module provides a [`GraphBackend`] trait and a concrete
//! [`GraphStore`] implementation using petgraph.  The trait is
//! designed so that a KuzuDB backend can be swapped in later without
//! changing the public API.
//!
//! ## Architecture
//!
//! ```text
//! GraphStore
//!   ├─ petgraph backend (default)
//!   │   ├─ build()        ── build from SCIP + tree-sitter
//!   │   ├─ query_calls()  ── outgoing call edges
//!   │   ├─ query_callers()── incoming call edges
//!   │   └─ freshness_check()
//!   └─ <future Kuzu backend>
//! ```
//!
//! ## Reliability contract
//!
//! **All** results from `query_calls` and `query_callers` MUST carry
//! `reliability: "inferred_candidate"` — even when the edge was derived
//! from precise SCIP data.  This is because call-graph analysis is
//! inherently incomplete (dynamic dispatch, reflection, macros, …).

pub mod builder;
pub mod schema;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use bincode;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde_json::{json, Value};

use crate::{index, workspace::Workspace};

use self::schema::{CallCandidate, EdgeMetadata, GraphNode, SerialisedGraph};

// Re-exports
pub use self::schema::EdgeKind;

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// Abstraction over graph storage backends (petgraph, KuzuDB, …).
pub trait GraphBackend {
    /// Build the graph from source files and (optionally) SCIP data.
    fn build(&mut self, workspace: &Workspace, graph_dir: &Path) -> Result<()>;

    /// Query outgoing call relationships for a given function identifier.
    fn query_calls(&self, identifier: &str) -> Result<Vec<CallCandidate>>;

    /// Query incoming call relationships for a given function identifier.
    fn query_callers(&self, identifier: &str) -> Result<Vec<CallCandidate>>;

    /// Check whether the stored graph is fresh relative to the given snapshot.
    fn freshness_check(&self, snapshot_id: &str) -> Result<bool>;
}

// ---------------------------------------------------------------------------
// GraphStore — lifecycle manager
// ---------------------------------------------------------------------------

/// Manages the graph backend lifecycle: build, load, query, freshness.
pub struct GraphStore {
    backend: Box<dyn GraphBackend>,
    graph_dir: PathBuf,
    snapshot_id: String,
}

impl GraphStore {
    /// Create a new store, loading an existing graph if available.
    pub fn open(workspace: &Workspace) -> Result<Self> {
        Self::open_for_snapshot(workspace, &workspace.snapshot_id)
    }

    pub fn open_for_snapshot(workspace: &Workspace, snapshot_id: &str) -> Result<Self> {
        let graph_dir = graph_dir_for_snapshot(workspace, snapshot_id);

        // Try loading the persisted graph; if missing or stale, start fresh.
        let bin_path = graph_dir.join("petgraph.bin");
        let backend: Box<dyn GraphBackend> = if bin_path.exists() {
            match PetgraphBackend::load_from_disk(&bin_path) {
                Ok(backend) => Box::new(backend),
                Err(_) => Box::new(PetgraphBackend::empty()),
            }
        } else {
            Box::new(PetgraphBackend::empty())
        };

        Ok(Self {
            backend,
            graph_dir,
            snapshot_id: snapshot_id.to_string(),
        })
    }

    /// Build (or rebuild) the graph from workspace sources and any
    /// existing SCIP occurrence data.
    pub fn build(&mut self, workspace: &Workspace) -> Result<()> {
        fs::create_dir_all(&self.graph_dir)?;
        self.backend.build(workspace, &self.graph_dir)?;
        self.snapshot_id = workspace.snapshot_id.clone();
        Ok(())
    }

    /// Query outgoing calls from the given function identifier.
    pub fn query_calls(&self, identifier: &str) -> Result<Vec<CallCandidate>> {
        self.backend.query_calls(identifier)
    }

    /// Query incoming callers for the given function identifier.
    pub fn query_callers(&self, identifier: &str) -> Result<Vec<CallCandidate>> {
        self.backend.query_callers(identifier)
    }

    /// Check whether the persisted graph matches the current snapshot.
    pub fn freshness_check(&self) -> Result<bool> {
        self.backend.freshness_check(&self.snapshot_id)
    }

    /// Index metadata for JSON responses.
    pub fn index_meta(&self, fresh: bool) -> Value {
        json!({
            "used": true,
            "fresh": fresh,
            "source": "petgraph",
            "fallback": false,
            "path": self.graph_dir,
            "snapshot_id": self.snapshot_id,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the graph storage directory for the current workspace snapshot.
pub fn graph_dir(workspace: &Workspace) -> PathBuf {
    graph_dir_for_snapshot(workspace, &workspace.snapshot_id)
}

pub fn graph_dir_for_snapshot(workspace: &Workspace, snapshot_id: &str) -> PathBuf {
    let root = workspace.root.join(".codetrail");
    root.join("graph").join(index::snapshot_key(snapshot_id))
}

pub fn graph_index_exists(workspace: &Workspace) -> bool {
    graph_index_exists_for_snapshot(workspace, &workspace.snapshot_id)
}

pub fn graph_index_exists_for_snapshot(workspace: &Workspace, snapshot_id: &str) -> bool {
    graph_dir_for_snapshot(workspace, snapshot_id)
        .join("petgraph.bin")
        .exists()
}

// ---------------------------------------------------------------------------
// Petgraph Backend implementation
// ---------------------------------------------------------------------------

/// Concrete [`GraphBackend`] backed by petgraph's `DiGraph`.
pub struct PetgraphBackend {
    pub(crate) graph: DiGraph<GraphNode, EdgeMetadata>,
    /// Cache: node-index lookup by scoped identifier.
    pub(crate) node_by_id: std::collections::HashMap<String, petgraph::graph::NodeIndex>,
    pub(crate) snapshot_id: String,
    pub(crate) schema_version: u32,
}

impl PetgraphBackend {
    /// Create an empty graph (no nodes, no edges).
    pub fn empty() -> Self {
        Self {
            graph: DiGraph::new(),
            node_by_id: std::collections::HashMap::new(),
            snapshot_id: String::new(),
            schema_version: SerialisedGraph::CURRENT_SCHEMA_VERSION,
        }
    }

    /// Persist the current graph to `petgraph.bin` in `graph_dir`.
    fn save_to_disk(&self, graph_dir: &Path) -> Result<()> {
        let serialised = SerialisedGraph {
            nodes: self.graph.node_weights().cloned().collect(),
            edges: self
                .graph
                .edge_references()
                .map(|e| e.weight().clone())
                .collect(),
            snapshot_id: self.snapshot_id.clone(),
            schema_version: SerialisedGraph::CURRENT_SCHEMA_VERSION,
        };

        let bin_path = graph_dir.join("petgraph.bin");
        let encoded =
            bincode::serialize(&serialised).with_context(|| "failed to serialise graph")?;
        fs::write(&bin_path, &encoded)
            .with_context(|| format!("failed to write {}", bin_path.display()))?;

        // Also write a human-readable manifest
        let manifest_path = graph_dir.join("manifest.json");
        let mut f = fs::File::create(&manifest_path)?;
        serde_json::to_writer_pretty(
            &mut f,
            &json!({
                "source": "petgraph",
                "snapshot_id": self.snapshot_id,
                "nodeCount": self.graph.node_count(),
                "edgeCount": self.graph.edge_count(),
                "schemaVersion": SerialisedGraph::CURRENT_SCHEMA_VERSION,
            }),
        )?;
        writeln!(f)?;

        Ok(())
    }

    /// Load a previously persisted graph from disk.
    pub fn load_from_disk(bin_path: &Path) -> Result<Self> {
        let data =
            fs::read(bin_path).with_context(|| format!("failed to read {}", bin_path.display()))?;
        let serialised: SerialisedGraph =
            bincode::deserialize(&data).with_context(|| "failed to deserialise graph")?;

        let mut graph = DiGraph::new();
        let mut node_by_id = std::collections::HashMap::new();

        for node in &serialised.nodes {
            let idx = graph.add_node(node.clone());
            node_by_id.insert(node.id.clone(), idx);
        }

        for edge in &serialised.edges {
            let caller_idx = node_by_id.get(&edge.caller_id);
            let callee_idx = node_by_id.get(&edge.callee_id);
            if let (Some(&caller), Some(&callee)) = (caller_idx, callee_idx) {
                graph.add_edge(caller, callee, edge.clone());
            }
        }

        Ok(Self {
            graph,
            node_by_id,
            snapshot_id: serialised.snapshot_id,
            schema_version: serialised.schema_version,
        })
    }

    /// Helper: insert a node if not already present.
    pub(crate) fn ensure_node(&mut self, node: GraphNode) -> petgraph::graph::NodeIndex {
        *self
            .node_by_id
            .entry(node.id.clone())
            .or_insert_with(|| self.graph.add_node(node))
    }
}

impl GraphBackend for PetgraphBackend {
    fn build(&mut self, workspace: &Workspace, graph_dir: &Path) -> Result<()> {
        self.snapshot_id = workspace.snapshot_id.clone();
        self.schema_version = SerialisedGraph::CURRENT_SCHEMA_VERSION;
        builder::build_petgraph_backend(self, workspace)?;
        self.save_to_disk(graph_dir)?;
        Ok(())
    }

    fn query_calls(&self, identifier: &str) -> Result<Vec<CallCandidate>> {
        let mut results: Vec<CallCandidate> = Vec::new();
        for node_idx in self.matching_node_indices(identifier) {
            results.extend(
                self.graph
                    .edges_directed(node_idx, petgraph::Direction::Outgoing)
                    .map(|edge| {
                        let meta = edge.weight();
                        let caller = &self.graph[edge.source()];
                        let callee = &self.graph[edge.target()];
                        edge_to_candidate(meta, caller, callee)
                    }),
            );
        }

        if results.is_empty() {
            return Ok(results);
        }

        results.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then(a.enclosing_symbol.cmp(&b.enclosing_symbol))
                .then(a.target.cmp(&b.target))
        });

        Ok(results)
    }

    fn query_callers(&self, identifier: &str) -> Result<Vec<CallCandidate>> {
        let mut results: Vec<CallCandidate> = Vec::new();
        for node_idx in self.matching_node_indices(identifier) {
            results.extend(
                self.graph
                    .edges_directed(node_idx, petgraph::Direction::Incoming)
                    .map(|edge| {
                        let meta = edge.weight();
                        let caller = &self.graph[edge.source()];
                        let callee = &self.graph[edge.target()];
                        edge_to_caller_candidate(meta, caller, callee)
                    }),
            );
        }

        if results.is_empty() {
            return Ok(results);
        }

        results.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then(a.enclosing_symbol.cmp(&b.enclosing_symbol))
                .then(a.target.cmp(&b.target))
        });

        Ok(results)
    }

    fn freshness_check(&self, snapshot_id: &str) -> Result<bool> {
        Ok(self.snapshot_id == snapshot_id
            && !self.snapshot_id.is_empty()
            && self.schema_version == SerialisedGraph::CURRENT_SCHEMA_VERSION)
    }
}

impl PetgraphBackend {
    fn matching_node_indices(&self, identifier: &str) -> Vec<NodeIndex> {
        if let Some(idx) = self.node_by_id.get(identifier) {
            return vec![*idx];
        }

        let query_is_simple = last_identifier(identifier) == identifier;
        self.graph
            .node_indices()
            .filter(|idx| {
                let node = &self.graph[*idx];
                node.display_name == identifier
                    || (query_is_simple
                        && (last_identifier(&node.display_name) == identifier
                            || last_identifier(&node.id) == identifier))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// JSON conversion helpers
// ---------------------------------------------------------------------------

fn edge_to_candidate(meta: &EdgeMetadata, caller: &GraphNode, callee: &GraphNode) -> CallCandidate {
    CallCandidate {
        path: meta.file_path.clone(),
        language: meta.language.clone(),
        target: node_display_name(callee),
        enclosing_symbol: Some(node_display_name(caller)),
        range: json!({
            "start": { "line": meta.call_line, "column": meta.call_column },
            "end": { "line": meta.call_line, "column": meta.call_column + 1 }
        }),
        file_hash: meta.file_hash.clone(),
        producer: format!("graph:{}", meta.source),
        source: format!("{}", meta.source),
        level: "inferred_candidate".to_string(), // ALWAYS inferred_candidate per spec
    }
}

fn edge_to_caller_candidate(
    meta: &EdgeMetadata,
    caller: &GraphNode,
    callee: &GraphNode,
) -> CallCandidate {
    CallCandidate {
        path: meta.file_path.clone(),
        language: meta.language.clone(),
        target: node_display_name(callee),
        enclosing_symbol: Some(node_display_name(caller)),
        range: json!({
            "start": { "line": meta.call_line, "column": meta.call_column },
            "end": { "line": meta.call_line, "column": meta.call_column + 1 }
        }),
        file_hash: meta.file_hash.clone(),
        producer: format!("graph:{}", meta.source),
        source: format!("{}", meta.source),
        level: "inferred_candidate".to_string(), // ALWAYS inferred_candidate per spec
    }
}

fn node_display_name(node: &GraphNode) -> String {
    if node.display_name.is_empty() {
        node.id.clone()
    } else {
        node.display_name.clone()
    }
}

fn last_identifier(target: &str) -> &str {
    target
        .rsplit(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .find(|part| !part.is_empty())
        .unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::schema::{EdgeSource, GraphNode, NodeKind, ReliabilityLevel};
    use serde_json::json;
    use tempfile::tempdir;

    fn make_test_node(id: &str, kind: NodeKind) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            display_name: id.to_string(),
            kind,
            language: "rust".to_string(),
            file_path: format!("src/{}.rs", id),
            start_line: 1,
            start_column: 0,
            end_line: 10,
            end_column: 0,
        }
    }

    fn make_test_edge(
        caller_id: &str,
        callee_id: &str,
        file_path: &str,
        source: EdgeSource,
    ) -> EdgeMetadata {
        EdgeMetadata {
            source,
            level: ReliabilityLevel::InferredCandidate,
            file_path: file_path.to_string(),
            call_line: 5,
            call_column: 10,
            caller_id: caller_id.to_string(),
            callee_id: callee_id.to_string(),
            language: "rust".to_string(),
            file_hash: "blake3:deadbeef".to_string(),
        }
    }

    #[test]
    fn graph_build_and_query_calls() {
        let mut backend = PetgraphBackend::empty();
        backend.snapshot_id = "test-snap".to_string();

        // Add nodes
        let caller = make_test_node("foo", NodeKind::Function);
        let callee = make_test_node("bar", NodeKind::Function);
        backend.ensure_node(caller.clone());
        backend.ensure_node(callee.clone());

        // Add edge foo -> bar
        let edge = make_test_edge("foo", "bar", "src/main.rs", EdgeSource::TreeSitterHeuristic);
        let caller_idx = *backend.node_by_id.get("foo").unwrap();
        let callee_idx = *backend.node_by_id.get("bar").unwrap();
        backend.graph.add_edge(caller_idx, callee_idx, edge);

        // Query calls from foo
        let calls = backend.query_calls("foo").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].target, "bar");
        assert_eq!(calls[0].level, "inferred_candidate");
        assert_eq!(calls[0].source, "tree_sitter_heuristic");
        assert_eq!(calls[0].enclosing_symbol, Some("foo".to_string()));
        assert_eq!(calls[0].path, "src/main.rs");

        // Query callers of bar
        let callers = backend.query_callers("bar").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].target, "bar");
        assert_eq!(callers[0].level, "inferred_candidate");
        assert_eq!(callers[0].enclosing_symbol, Some("foo".to_string()));
    }

    #[test]
    fn graph_keeps_unique_ids_for_duplicate_display_names() {
        let mut backend = PetgraphBackend::empty();
        let mut caller = make_test_node("scip:crate/a#parse", NodeKind::Function);
        caller.display_name = "parse".to_string();
        let mut callee = make_test_node("scip:crate/b#parse", NodeKind::Function);
        callee.display_name = "parse".to_string();
        backend.ensure_node(caller);
        backend.ensure_node(callee);

        let edge = make_test_edge(
            "scip:crate/a#parse",
            "scip:crate/b#parse",
            "src/lib.rs",
            EdgeSource::ScipPrecise,
        );
        let caller_idx = *backend.node_by_id.get("scip:crate/a#parse").unwrap();
        let callee_idx = *backend.node_by_id.get("scip:crate/b#parse").unwrap();
        backend.graph.add_edge(caller_idx, callee_idx, edge);

        assert_eq!(backend.graph.node_count(), 2);
        let calls = backend.query_calls("scip:crate/a#parse").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].target, "parse");

        let display_calls = backend.query_calls("parse").unwrap();
        assert_eq!(display_calls.len(), 1);
        assert_eq!(display_calls[0].target, "parse");
        assert_eq!(display_calls[0].enclosing_symbol, Some("parse".to_string()));
    }

    #[test]
    fn graph_matches_simple_identifier_against_qualified_call_targets() {
        let mut backend = PetgraphBackend::empty();
        backend.snapshot_id = "test-snap".to_string();

        let caller = make_test_node("run", NodeKind::Function);
        let mut callee = make_test_node("self.helper", NodeKind::Function);
        callee.display_name = "self.helper".to_string();
        backend.ensure_node(caller);
        backend.ensure_node(callee);

        let edge = make_test_edge(
            "run",
            "self.helper",
            "src/lib.rs",
            EdgeSource::TreeSitterHeuristic,
        );
        let caller_idx = *backend.node_by_id.get("run").unwrap();
        let callee_idx = *backend.node_by_id.get("self.helper").unwrap();
        backend.graph.add_edge(caller_idx, callee_idx, edge);

        let callers = backend.query_callers("helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].target, "self.helper");
        assert_eq!(callers[0].enclosing_symbol, Some("run".to_string()));
    }

    #[test]
    fn query_unknown_identifier_returns_empty() {
        let backend = PetgraphBackend::empty();
        assert!(backend.query_calls("nonexistent").unwrap().is_empty());
        assert!(backend.query_callers("nonexistent").unwrap().is_empty());
    }

    #[test]
    fn freshness_check_matches_snapshot() {
        let backend = PetgraphBackend::empty();
        assert!(!backend.freshness_check("test").unwrap());

        let mut backend = PetgraphBackend::empty();
        backend.snapshot_id = "commit:abc123".to_string();
        assert!(backend.freshness_check("commit:abc123").unwrap());
        assert!(!backend.freshness_check("commit:def456").unwrap());
    }

    #[test]
    fn freshness_rejects_stored_graph_with_old_schema_version() {
        let dir = tempdir().unwrap();
        let graph = SerialisedGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            snapshot_id: "commit:abc123".to_string(),
            schema_version: SerialisedGraph::CURRENT_SCHEMA_VERSION.saturating_sub(1),
        };
        let bin_path = dir.path().join("petgraph.bin");
        std::fs::write(&bin_path, bincode::serialize(&graph).unwrap()).unwrap();

        let backend = PetgraphBackend::load_from_disk(&bin_path).unwrap();

        assert!(!backend.freshness_check("commit:abc123").unwrap());
    }

    #[test]
    fn serialisation_roundtrip() {
        let dir = tempdir().unwrap();
        let graph_dir = dir.path();

        let mut backend = PetgraphBackend::empty();
        backend.snapshot_id = "snapshot-1".to_string();

        let caller = make_test_node("alpha", NodeKind::Function);
        let callee = make_test_node("beta", NodeKind::Function);
        backend.ensure_node(caller);
        backend.ensure_node(callee);

        let edge = make_test_edge("alpha", "beta", "src/lib.rs", EdgeSource::ScipPrecise);
        let a_idx = *backend.node_by_id.get("alpha").unwrap();
        let b_idx = *backend.node_by_id.get("beta").unwrap();
        backend.graph.add_edge(a_idx, b_idx, edge);

        // Save
        backend.save_to_disk(graph_dir).unwrap();
        assert!(graph_dir.join("petgraph.bin").exists());
        assert!(graph_dir.join("manifest.json").exists());

        // Load
        let loaded = PetgraphBackend::load_from_disk(&graph_dir.join("petgraph.bin")).unwrap();
        assert_eq!(loaded.graph.node_count(), 2);
        assert_eq!(loaded.graph.edge_count(), 1);
        assert_eq!(loaded.snapshot_id, "snapshot-1");

        // Queries work on loaded graph
        let calls = loaded.query_calls("alpha").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].target, "beta");
        assert_eq!(calls[0].source, "scip_precise");
    }

    #[test]
    fn graph_index_meta_includes_freshness() {
        let backend = PetgraphBackend::empty();
        let store = GraphStore {
            backend: Box::new(backend),
            graph_dir: "/tmp/test".into(),
            snapshot_id: "snap1".to_string(),
        };

        let meta = store.index_meta(true);
        assert_eq!(meta["used"], json!(true));
        assert_eq!(meta["fresh"], json!(true));
        assert_eq!(meta["source"], "petgraph");
        assert_eq!(meta["snapshot_id"], "snap1");

        let stale = store.index_meta(false);
        assert_eq!(stale["fresh"], json!(false));
    }

    #[test]
    fn all_results_are_inferred_candidate() {
        // Verify that even edges from SCIP produce inferred_candidate
        let mut backend = PetgraphBackend::empty();
        let caller = make_test_node("caller", NodeKind::Function);
        let callee = make_test_node("callee", NodeKind::Function);
        backend.ensure_node(caller);
        backend.ensure_node(callee);

        let edge = make_test_edge("caller", "callee", "f.rs", EdgeSource::ScipPrecise);
        let c1 = *backend.node_by_id.get("caller").unwrap();
        let c2 = *backend.node_by_id.get("callee").unwrap();
        backend.graph.add_edge(c1, c2, edge);

        let calls = backend.query_calls("caller").unwrap();
        assert_eq!(calls.len(), 1);
        // CRITICAL: Even SCIP edges must produce inferred_candidate
        assert_eq!(calls[0].level, "inferred_candidate");
        assert_eq!(calls[0].source, "scip_precise");

        let callers = backend.query_callers("callee").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].level, "inferred_candidate");
    }
}
