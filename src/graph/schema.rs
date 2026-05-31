//! Graph schema constants and node/edge types for the property-graph backend.
//!
//! The graph tracks function-level call relationships with reliability metadata
//! on every edge. The schema is designed to be KuzuDB-compatible so that the
//! petgraph backend can be swapped for Kuzu later.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Node types
// ---------------------------------------------------------------------------

/// Kinds of nodes stored in the call graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    /// A named function (or method).
    Function,
    /// A source file.
    File,
    /// A module / namespace container.
    Module,
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeKind::Function => write!(f, "Function"),
            NodeKind::File => write!(f, "File"),
            NodeKind::Module => write!(f, "Module"),
        }
    }
}

/// A node in the call graph.
///
/// Functions are identified by a scoped name (e.g. `my_crate::mod::fn`).
/// Files are identified by their relative path.  Modules are identified
/// by their declaration path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphNode {
    /// Stable identifier — scoped name for functions, relative path for files.
    pub id: String,
    pub kind: NodeKind,
    /// Source language (rust, python, typescript, …).
    pub language: String,
    /// Relative file path where this node is declared (empty for synthetic nodes).
    pub file_path: String,
    /// 1-based start line (0 if unknown).
    pub start_line: u32,
    /// 1-based start column (0 if unknown).
    pub start_column: u32,
    /// 1-based end line.
    pub end_line: u32,
    /// 1-based end column.
    pub end_column: u32,
}

// ---------------------------------------------------------------------------
// Edge types
// ---------------------------------------------------------------------------

/// Provenance / data source for an edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeSource {
    /// Derived from a precise SCIP occurrence index.
    ScipPrecise,
    /// Heuristic from tree-sitter AST traversal.
    TreeSitterHeuristic,
}

impl fmt::Display for EdgeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdgeSource::ScipPrecise => write!(f, "scip_precise"),
            EdgeSource::TreeSitterHeuristic => write!(f, "tree_sitter_heuristic"),
        }
    }
}

/// The reliability level of an edge.
///
/// Even edges from the graph are NOT marked precise — the task definition
/// dictates that `calls`/`callers` always return `inferred_candidate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReliabilityLevel {
    /// Derived from a precise code-intelligence index.
    PreciseFact,
    /// Best-effort inference that must be verified by the consumer.
    InferredCandidate,
}

impl fmt::Display for ReliabilityLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReliabilityLevel::PreciseFact => write!(f, "precise_fact"),
            ReliabilityLevel::InferredCandidate => write!(f, "inferred_candidate"),
        }
    }
}

/// Metadata carried on every graph edge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeMetadata {
    /// Where this relationship was observed.
    pub source: EdgeSource,
    /// Reliability tier.
    pub level: ReliabilityLevel,
    /// The file where the call site lives.
    pub file_path: String,
    /// 1-based line of the call expression.
    pub call_line: u32,
    /// 1-based column of the call expression.
    pub call_column: u32,
    /// The enclosing function (caller) id.
    pub caller_id: String,
    /// The called function (callee) id.
    pub callee_id: String,
    /// Language of the caller file.
    pub language: String,
    /// blake3 hash of the caller file at build time.
    pub file_hash: String,
}

/// Kinds of relationships stored in the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// Function A calls function B.
    Calls,
    /// Reverse of CALLS (function B is called by function A).
    CalledBy,
    /// A function is defined in a file.
    DefinedIn,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// A single call-candidate returned to the user.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallCandidate {
    pub path: String,
    pub language: String,
    pub target: String,
    pub enclosing_symbol: Option<String>,
    pub range: serde_json::Value,
    pub file_hash: String,
    pub producer: String,
    /// Edge provenance.
    pub source: String,
    /// Reliability level.
    pub level: String,
}

// ---------------------------------------------------------------------------
// Serialisable graph representation (for persisting petgraph)
// ---------------------------------------------------------------------------

/// Flat, serialisable representation of the call graph.
///
/// petgraph's built-in serde support can be unreliable across versions, so
/// we store a simple edge list that can be reconstructed on load.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerialisedGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<EdgeMetadata>,
    pub snapshot_id: String,
    pub schema_version: u32,
}

impl SerialisedGraph {
    pub const CURRENT_SCHEMA_VERSION: u32 = 2;
}
