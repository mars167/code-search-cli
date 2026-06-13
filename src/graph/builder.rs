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
use petgraph::{graph::NodeIndex, visit::EdgeRef};

use crate::{
    lsp::scip_gen,
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

#[derive(Clone)]
struct ScipDefinition {
    symbol_key: String,
    name: String,
    language: String,
    path: String,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

fn enclosing_definition<'a>(
    def_locations: &'a HashMap<String, ScipDefinition>,
    path: &str,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
) -> Option<&'a ScipDefinition> {
    def_locations
        .values()
        .filter(|definition| definition.path == path)
        .filter(|definition| {
            range_contains(
                definition.start_line,
                definition.start_column,
                definition.end_line,
                definition.end_column,
                start_line,
                start_column,
                end_line,
                end_column,
            )
        })
        .max_by_key(|definition| (definition.start_line, definition.start_column))
        .or_else(|| {
            def_locations
                .values()
                .filter(|definition| definition.path == path)
                .filter(|definition| {
                    (definition.start_line, definition.start_column) <= (start_line, start_column)
                })
                .max_by_key(|definition| (definition.start_line, definition.start_column))
        })
}

fn range_contains(
    outer_start_line: u32,
    outer_start_column: u32,
    outer_end_line: u32,
    outer_end_column: u32,
    inner_start_line: u32,
    inner_start_column: u32,
    inner_end_line: u32,
    inner_end_column: u32,
) -> bool {
    (outer_start_line, outer_start_column) <= (inner_start_line, inner_start_column)
        && (outer_end_line, outer_end_column) >= (inner_end_line, inner_end_column)
}

fn edge_exists_at_site(
    backend: &PetgraphBackend,
    caller_idx: NodeIndex,
    callee_idx: NodeIndex,
    file_path: &str,
    call_line: u32,
    call_column: u32,
) -> bool {
    backend
        .graph
        .edges_directed(caller_idx, petgraph::Direction::Outgoing)
        .any(|edge| {
            let meta = edge.weight();
            edge.target() == callee_idx
                && meta.file_path == file_path
                && meta.call_line == call_line
                && meta.call_column == call_column
        })
}

fn build_from_scip(backend: &mut PetgraphBackend, workspace: &Workspace) {
    let db_path = native_db_path(workspace);
    if !db_path.exists() {
        return;
    }
    if !scip::occurrence_db_fresh(&db_path, &workspace.snapshot_id, &workspace.root) {
        return;
    }
    if !scip_gen::generation_manifests_allow_precise_use(workspace).unwrap_or(false) {
        return;
    }
    // Read all symbols with their definitions
    let Ok(symbols) = scip::query_symbols(&db_path, "") else {
        return;
    };

    let mut def_locations: HashMap<String, ScipDefinition> = HashMap::new();

    for sym in &symbols {
        if sym.role == "definition" {
            def_locations.insert(
                sym.symbol_key.clone(),
                ScipDefinition {
                    symbol_key: sym.symbol_key.clone(),
                    name: sym.name.clone(),
                    language: sym.language.clone(),
                    path: sym.path.clone(),
                    start_line: sym.start_line,
                    start_column: sym.start_column,
                    end_line: sym.end_line,
                    end_column: sym.end_column,
                },
            );
        }
    }

    for definition in def_locations.values() {
        backend.ensure_node(GraphNode {
            id: definition.symbol_key.clone(),
            display_name: definition.name.clone(),
            kind: NodeKind::Function,
            language: definition.language.clone(),
            file_path: definition.path.clone(),
            start_line: definition.start_line,
            start_column: definition.start_column,
            end_line: definition.end_line,
            end_column: definition.end_column,
        });
    }

    for sym in def_locations.values() {
        let Ok(refs) = scip::query_refs_by_symbol_key(&db_path, &sym.symbol_key) else {
            continue;
        };
        for r in refs {
            let enclosing = enclosing_definition(
                &def_locations,
                &r.path,
                r.start_line,
                r.start_column,
                r.end_line,
                r.end_column,
            );

            if let Some(caller) = enclosing {
                backend.ensure_node(GraphNode {
                    id: r.symbol_key.clone(),
                    display_name: r.name.clone(),
                    kind: NodeKind::Function,
                    language: r.language.clone(),
                    file_path: r.path.clone(),
                    start_line: r.start_line,
                    start_column: r.start_column,
                    end_line: r.end_line,
                    end_column: r.end_column,
                });

                let (Some(&caller_idx), Some(&callee_idx)) = (
                    backend.node_by_id.get(&caller.symbol_key),
                    backend.node_by_id.get(&r.symbol_key),
                ) else {
                    continue;
                };

                if !edge_exists_at_site(
                    backend,
                    caller_idx,
                    callee_idx,
                    &r.path,
                    r.start_line,
                    r.start_column,
                ) {
                    backend.graph.add_edge(
                        caller_idx,
                        callee_idx,
                        EdgeMetadata {
                            source: EdgeSource::ScipPrecise,
                            level: ReliabilityLevel::InferredCandidate,
                            file_path: r.path.clone(),
                            call_line: r.start_line,
                            call_column: r.start_column,
                            caller_id: caller.symbol_key.clone(),
                            callee_id: r.symbol_key.clone(),
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
        lang: Vec::new(),
        changed: false,
        cursor: None,
        allow_broad: false,
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
            display_name: caller_id.clone(),
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
            display_name: callee_id.clone(),
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

        if !edge_exists_at_site(
            backend, caller_idx, callee_idx, &call.path, call_line, call_col,
        ) {
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
