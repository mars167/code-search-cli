//! Graph builder: construct the call graph from SCIP occurrence data and
//! tree-sitter heuristics.
//!
//! ## Build strategy
//!
//! 1. **SCIP path** — When a SCIP occurrence DB exists and is fresh, read
//!    symbol/occurrence records to extract function-definition locations and
//!    reference relationships.  Every reference from one function scope to
//!    another is recorded as a CALLS edge with `source: scip_precise`.
//!
//! 2. **Tree-sitter path** — Always run tree-sitter AST traversal as a
//!    supplement.  Call expressions discovered this way carry
//!    `source: tree_sitter_heuristic`.
//!
//! 3. Edges are NOT duplicated: if SCIP already provided a precise edge
//!    for a given (caller, callee, site), the tree-sitter duplicate is
//!    skipped.

use std::collections::HashMap;

use anyhow::Result;
use petgraph::visit::EdgeRef;

use crate::{
    scip,
    scip_index::native_db_path,
    syntax,
    workspace::{ScanOptions, Workspace},
};

use super::{
    schema::{EdgeMetadata, EdgeSource, GraphNode, NodeKind, ReliabilityLevel},
    PetgraphBackend,
};

/// Build the petgraph backend from the workspace.
///
/// This function is called by [`PetgraphBackend::build`].
pub(crate) fn build_petgraph_backend(
    backend: &mut PetgraphBackend,
    workspace: &Workspace,
) -> Result<()> {
    // Reset graphs
    backend.graph = petgraph::graph::DiGraph::new();
    backend.node_by_id = HashMap::new();
    backend.snapshot_id = workspace.snapshot_id.clone();

    // --- Phase 1: register nodes from SCIP symbols ---
    build_from_scip(backend, workspace);

    // --- Phase 2: tree-sitter call edges ---
    build_tree_sitter_edges(backend, workspace);

    Ok(())
}

/// Register function nodes and CALLS edges from the SCIP occurrence DB.
fn build_from_scip(backend: &mut PetgraphBackend, workspace: &Workspace) {
    let db_path = native_db_path(workspace);
    if !db_path.exists() {
        return;
    }
    if !scip::occurrence_db_fresh(&db_path, &workspace.snapshot_id, &workspace.root) {
        return;
    }
    // Read all symbols with their definitions
    let Ok(symbols) = scip::query_symbols(&db_path, "") else {
        return;
    };

    // Map: symbol name -> definition file path
    let mut def_locations: HashMap<String, (String, u32, u32)> = HashMap::new();

    for sym in &symbols {
        if sym.role == "definition" {
            def_locations.insert(
                sym.name.clone(),
                (sym.path.clone(), sym.start_line, sym.start_column),
            );
        }
    }

    // Register function nodes from definitions
    for (name, (file_path, start_line, start_column)) in &def_locations {
        backend.ensure_node(GraphNode {
            id: name.clone(),
            kind: NodeKind::Function,
            language: String::new(),
            file_path: file_path.clone(),
            start_line: *start_line,
            start_column: *start_column,
            end_line: *start_line,
            end_column: start_column.saturating_add(name.len() as u32),
        });
    }

    // Build CALLS edges from references
    // For each symbol, query its references; if a reference site falls within
    // another function's body in the same file, add a CALLS edge.

    for sym in &symbols {
        let Ok(refs) = scip::query_refs(&db_path, &sym.name) else {
            continue;
        };
        for r in refs {
            let callee_name = &r.name;
            // Find enclosing function: the last function defined in the same file
            // whose start_line is <= the reference line.
            let enclosing = def_locations
                .iter()
                .filter(|(_, (path, _, _))| path == &r.path)
                .filter(|(_, (_, line, _))| *line <= r.start_line)
                .max_by_key(|(_, (_, line, _))| *line);

            if let Some((caller_name, _)) = enclosing {
                if caller_name == callee_name {
                    continue; // skip self-references
                }

                // Ensure callee node exists
                backend.ensure_node(GraphNode {
                    id: callee_name.clone(),
                    kind: NodeKind::Function,
                    language: r.language.clone(),
                    file_path: r.path.clone(),
                    start_line: r.start_line,
                    start_column: r.start_column,
                    end_line: r.end_line,
                    end_column: r.end_column,
                });

                let caller_idx = backend.node_by_id[caller_name];
                let callee_idx = backend.node_by_id[callee_name];

                // Avoid duplicate edges
                let edge_exists = backend
                    .graph
                    .edges_directed(caller_idx, petgraph::Direction::Outgoing)
                    .any(|e| e.target() == callee_idx);

                if !edge_exists {
                    backend.graph.add_edge(
                        caller_idx,
                        callee_idx,
                        EdgeMetadata {
                            source: EdgeSource::ScipPrecise,
                            level: ReliabilityLevel::InferredCandidate,
                            file_path: r.path.clone(),
                            call_line: r.start_line,
                            call_column: r.start_column,
                            caller_id: caller_name.clone(),
                            callee_id: callee_name.clone(),
                            language: r.language.clone(),
                            file_hash: r.file_hash.clone(),
                        },
                    );
                }
            }
        }
    }
}

/// Add call edges from tree-sitter AST traversal.
fn build_tree_sitter_edges(backend: &mut PetgraphBackend, workspace: &Workspace) {
    let scan_opts = ScanOptions {
        include: Vec::new(),
        exclude: Vec::new(),
        hidden: false,
        no_ignore: false,
        limit: 0,
    };

    let mut warnings = Vec::new();
    let Ok(tree_calls) = syntax::collect_calls(workspace, &scan_opts, &mut warnings) else {
        return;
    };

    for call in &tree_calls {
        // Register caller function node
        let caller_id = call
            .enclosing_symbol
            .as_deref()
            .unwrap_or("<<unknown>>")
            .to_string();
        if caller_id == "<<unknown>>" {
            continue;
        }

        let call_line = call.range["start"]["line"].as_u64().unwrap_or(0) as u32;
        let call_col = call.range["start"]["column"].as_u64().unwrap_or(0) as u32;

        // Ensure caller node
        backend.ensure_node(GraphNode {
            id: caller_id.clone(),
            kind: NodeKind::Function,
            language: call.language.clone(),
            file_path: call.path.clone(),
            start_line: call_line,
            start_column: call_col,
            end_line: call.range["end"]["line"].as_u64().unwrap_or(0) as u32,
            end_column: call.range["end"]["column"].as_u64().unwrap_or(0) as u32,
        });

        // Ensure callee node (the call target is the identifier being called)
        let callee_id = call.target.clone();
        backend.ensure_node(GraphNode {
            id: callee_id.clone(),
            kind: NodeKind::Function,
            language: call.language.clone(),
            file_path: call.path.clone(),
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        });

        let caller_idx = backend.node_by_id[&caller_id];
        let callee_idx = backend.node_by_id[&callee_id];

        // Skip if an edge already exists (SCIP edges take precedence)
        let edge_exists = backend
            .graph
            .edges_directed(caller_idx, petgraph::Direction::Outgoing)
            .any(|e| e.target() == callee_idx);

        if !edge_exists {
            backend.graph.add_edge(
                caller_idx,
                callee_idx,
                EdgeMetadata {
                    source: EdgeSource::TreeSitterHeuristic,
                    level: ReliabilityLevel::InferredCandidate,
                    file_path: call.path.clone(),
                    call_line,
                    call_column: call_col,
                    caller_id,
                    callee_id,
                    language: call.language.clone(),
                    file_hash: call.file_hash.clone(),
                },
            );
        }
    }
}
