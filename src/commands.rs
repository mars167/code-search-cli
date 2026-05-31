use serde_json::{json, Value};

use crate::{
    cli::{Cli, Command, HooksCommand, IndexCommand},
    completions, graph, index, output, scip_index, search, syntax,
    workspace::{ScanOptions, Workspace},
    AppResult,
};

pub fn run(cli: Cli) -> AppResult<i32> {
    let scan_opts = ScanOptions {
        include: cli.include.clone(),
        exclude: cli.exclude.clone(),
        hidden: cli.hidden,
        no_ignore: cli.no_ignore,
        lang: cli.lang.clone(),
        changed: cli.changed,
        cursor: cli.cursor.clone(),
        limit: cli.limit,
    };
    let mut exit_code = 0;

    if let Command::Completions { shell } = &cli.command {
        print!("{}", completions::script(shell));
        return Ok(0);
    }

    let workspace = Workspace::discover(&cli.path)?;
    let scope_warnings = scope_warnings(&workspace, &scan_opts);

    let value = match &cli.command {
        Command::Find { text, mode } => {
            let query_output =
                search::find(&workspace, &scan_opts, text, mode, cli.context, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "find",
                    "find",
                    scoped_query(json!({ "pattern": text, "mode": mode }), &scan_opts),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    query_output.index.clone(),
                    query_output.results.clone(),
                    scope_warnings.clone(),
                ),
                query_output,
            )
        }
        Command::Grep {
            pattern,
            mode,
            context,
        } => {
            let context = context.unwrap_or(cli.context);
            let query_output = search::find(&workspace, &scan_opts, pattern, mode, context, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "grep",
                    "find",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": mode, "context": context }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    query_output.index.clone(),
                    query_output.results.clone(),
                    scope_warnings.clone(),
                ),
                query_output,
            )
        }
        Command::Files { pattern } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "files",
                    "files",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": "path_substring" }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    query_output.index.clone(),
                    query_output.results.clone(),
                    scope_warnings.clone(),
                ),
                query_output,
            )
        }
        Command::FindPath { pattern } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "find-path",
                    "files",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": "path_substring" }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    query_output.index.clone(),
                    query_output.results.clone(),
                    scope_warnings.clone(),
                ),
                query_output,
            )
        }
        Command::Glob { pattern } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, true)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "glob",
                    "files",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": "strict_glob" }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    query_output.index.clone(),
                    query_output.results.clone(),
                    scope_warnings.clone(),
                ),
                query_output,
            )
        }
        Command::List { dir, recursive } => output::response(
            "list",
            "list",
            scoped_query(json!({ "dir": dir, "recursive": recursive }), &scan_opts),
            &workspace.snapshot_id,
            output::source_fact(),
            search::list(&workspace, &scan_opts, dir.as_deref(), *recursive)?,
            Vec::new(),
        ),
        Command::Tree { dir, depth } => output::response(
            "tree",
            "tree",
            scoped_query(json!({ "dir": dir, "depth": depth }), &scan_opts),
            &workspace.snapshot_id,
            output::source_fact(),
            search::tree(&workspace, &scan_opts, dir.as_deref(), *depth)?,
            Vec::new(),
        ),
        Command::Read { target } => {
            let result = search::read(&workspace, target)?;
            let reliability = if result
                .get("exact")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                output::source_fact()
            } else {
                output::source_fact_inexact()
            };
            let warnings = result
                .get("warnings")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect();
            output::response(
                "read",
                "read",
                json!({ "target": target }),
                &workspace.snapshot_id,
                reliability,
                json!([result]),
                warnings,
            )
        }
        Command::Refs { identifier } => {
            if let Some(precise) = scip_index::refs(&workspace, &scan_opts, identifier)? {
                return emit_response(
                    &cli.output,
                    output::response_with_index(
                        "refs",
                        "refs",
                        scoped_query(
                            json!({ "identifier": identifier, "producer": "scip" }),
                            &scan_opts,
                        ),
                        &workspace.snapshot_id,
                        output::precise_fact(),
                        precise.index,
                        precise.results,
                        scope_warnings.clone(),
                    ),
                    &workspace.root,
                );
            }
            let query_output = search::find(
                &workspace,
                &scan_opts,
                identifier,
                "literal",
                cli.context,
                true,
            )?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(output::response_with_index(
                "refs",
                "refs",
                scoped_query(json!({ "identifier": identifier, "mode": "identifier_boundary_text_search" }), &scan_opts),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index.clone(),
                query_output.results.clone(),
                merge_warnings(
                    vec!["refs is identifier-boundary text search unless a precise occurrence index is available".to_string()],
                    scope_warnings.clone(),
                ),
            ), query_output)
        }
        Command::Symbols { query } => {
            if let Some(precise) = scip_index::symbols(&workspace, &scan_opts, query)? {
                let page = search::page_results(
                    precise.results,
                    &scan_opts,
                    "symbols",
                    json!({ "query": query, "producer": "scip" }),
                    &workspace.snapshot_id,
                )?;
                return emit_response(
                    &cli.output,
                    output::with_page_meta(
                        output::response_with_index(
                            "symbols",
                            "symbols",
                            scoped_query(json!({ "query": query, "producer": "scip" }), &scan_opts),
                            &workspace.snapshot_id,
                            output::precise_fact(),
                            precise.index,
                            page.results.clone(),
                            scope_warnings.clone(),
                        ),
                        page.truncated,
                        page.next_cursor,
                        page.facets,
                    ),
                    &workspace.root,
                );
            }
            let (results, warnings) = syntax::symbols(&workspace, &scan_opts, query)?;
            let page = search::page_results(
                results,
                &scan_opts,
                "symbols",
                json!({ "query": query, "producer": "tree_sitter_parser" }),
                &workspace.snapshot_id,
            )?;
            exit_code = output::no_match_exit(&page.results);
            output::with_page_meta(
                output::response(
                    "symbols",
                    "symbols",
                    scoped_query(
                        json!({ "query": query, "producer": "tree_sitter_parser" }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::parser_fact(),
                    page.results.clone(),
                    merge_warnings(warnings, scope_warnings.clone()),
                ),
                page.truncated,
                page.next_cursor,
                page.facets,
            )
        }
        Command::Defs { identifier } => {
            if let Some(precise) = scip_index::defs(&workspace, &scan_opts, identifier)? {
                return emit_response(
                    &cli.output,
                    output::response_with_index(
                        "defs",
                        "defs",
                        scoped_query(
                            json!({ "identifier": identifier, "producer": "scip" }),
                            &scan_opts,
                        ),
                        &workspace.snapshot_id,
                        output::precise_fact(),
                        precise.index,
                        precise.results,
                        scope_warnings.clone(),
                    ),
                    &workspace.root,
                );
            }
            let (results, warnings) = syntax::defs(&workspace, &scan_opts, identifier)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "defs",
                "defs",
                scoped_query(
                    json!({ "identifier": identifier, "producer": "tree_sitter_parser_fallback", "fallbackReason": "precise_scip_index_unavailable" }),
                    &scan_opts,
                ),
                &workspace.snapshot_id,
                output::parser_fact(),
                results,
                merge_warnings(warnings, scope_warnings.clone()),
            )
        }
        Command::Calls { identifier } => {
            // Try graph backend first (if built and fresh)
            let graph_store = graph::GraphStore::open(&workspace).ok();
            if let Some(ref store) = graph_store {
                if store.freshness_check().unwrap_or(false) {
                    let results = store.query_calls(identifier).unwrap_or_default();
                    let index_meta = store.index_meta(true);
                    let warnings: Vec<String> = Vec::new();
                    return emit_response(
                        &cli.output,
                        output::response_with_index(
                            "calls",
                            "calls",
                            json!({ "identifier": identifier, "producer": "graph" }),
                            &workspace.snapshot_id,
                            output::inferred_candidate(),
                            index_meta,
                            json!(results),
                            warnings,
                        ),
                        &workspace.root,
                    );
                }
            }
            // Fall back to tree-sitter
            let (results, warnings) = syntax::calls(&workspace, &scan_opts, identifier)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "calls",
                "calls",
                json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
                &workspace.snapshot_id,
                output::inferred_candidate(),
                results,
                warnings,
            )
        }
        Command::Callers { identifier } => {
            // Try graph backend first (if built and fresh)
            let graph_store = graph::GraphStore::open(&workspace).ok();
            if let Some(ref store) = graph_store {
                if store.freshness_check().unwrap_or(false) {
                    let results = store.query_callers(identifier).unwrap_or_default();
                    let index_meta = store.index_meta(true);
                    let warnings: Vec<String> = Vec::new();
                    return emit_response(
                        &cli.output,
                        output::response_with_index(
                            "callers",
                            "callers",
                            json!({ "identifier": identifier, "producer": "graph" }),
                            &workspace.snapshot_id,
                            output::inferred_candidate(),
                            index_meta,
                            json!(results),
                            warnings,
                        ),
                        &workspace.root,
                    );
                }
            }
            // Fall back to tree-sitter
            let (results, warnings) = syntax::callers(&workspace, &scan_opts, identifier)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "callers",
                "callers",
                json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
                &workspace.snapshot_id,
                output::inferred_candidate(),
                results,
                warnings,
            )
        }
        Command::Changed => output::with_summary_field(
            output::response(
                "changed",
                "changed",
                json!({}),
                &workspace.snapshot_id,
                output::source_fact(),
                search::changed(&workspace)?,
                Vec::new(),
            ),
            "changed",
            search::changed_summary(&workspace),
        ),
        Command::Status => output::response(
            "status",
            "status",
            json!({}),
            &workspace.snapshot_id,
            output::source_fact(),
            json!([search::status(&workspace)]),
            Vec::new(),
        ),
        Command::Mcp => {
            let server = crate::mcp::Server::new(&workspace.root)?;
            server.run()?;
            return Ok(0);
        }
        Command::Watch { once, status } => {
            let mut watcher = crate::watcher::Watcher::start(&workspace.root)?;

            let results = if *once {
                // Run one reconcile pass, detect file changes against snapshot
                let reconcile_result = watcher.run_once()?;
                json!([serde_json::to_value(&reconcile_result)?])
            } else if *status {
                // Show watcher state (initialized but not running long-lived daemon)
                json!([watcher.status()])
            } else {
                // Default: show status with note about daemon mode
                json!([watcher.status()])
            };
            output::response(
                "watch",
                "watch",
                json!({ "once": once, "status": status }),
                &workspace.snapshot_id,
                output::freshness(),
                results,
                if !once && !status {
                    vec!["long-running watcher daemon mode is intentionally not started in non-interactive command execution; use watch --once for reconcile or watch --status for state".to_string()]
                } else {
                    Vec::new()
                },
            )
        }
        Command::Serve { no_watch } => {
            // Show query service status with optional watcher info
            let mut service_value = index::serve_status(&workspace, *no_watch);
            if !no_watch {
                // When watch is enabled, include watcher status
                if let Ok(watcher) = crate::watcher::Watcher::start(&workspace.root) {
                    if let Some(service) = service_value.get_mut("service") {
                        service["watcher"] = watcher.status();
                    }
                }
            }
            output::response(
                "serve",
                "serve",
                json!({ "noWatch": no_watch }),
                &workspace.snapshot_id,
                output::freshness(),
                json!([service_value]),
                vec!["HTTP/MCP adapters are expected to wrap the same CLI query service after JSON schema stabilization".to_string()],
            )
        }
        Command::Index { command } => match command {
            IndexCommand::Build {
                staged,
                changed,
                force,
            } => output::response(
                "index build",
                "index build",
                json!({ "staged": staged, "changed": changed, "force": force }),
                &workspace.snapshot_id,
                output::freshness(),
                json!([index::build(
                    &workspace, &scan_opts, *staged, *changed, *force
                )?]),
                Vec::new(),
            ),
            IndexCommand::Update => output::response(
                "index update",
                "index update",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                json!([index::update(&workspace, &scan_opts)?]),
                Vec::new(),
            ),
            IndexCommand::Status => output::response(
                "index status",
                "index status",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                json!([index::status(&workspace)?]),
                Vec::new(),
            ),
            IndexCommand::Verify => {
                let (result, code) = index::verify(&workspace)?;
                exit_code = code;
                output::response(
                    "index verify",
                    "index verify",
                    json!({}),
                    &workspace.snapshot_id,
                    output::freshness(),
                    json!([result]),
                    Vec::new(),
                )
            }
            IndexCommand::Clean => output::response(
                "index clean",
                "index clean",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                json!([index::clean(&workspace)?]),
                Vec::new(),
            ),
            IndexCommand::ImportScip { path } => {
                let input = std::fs::read(path).unwrap_or_default();
                // Skip leading whitespace/BOM to detect JSON format
                let is_json = {
                    let bytes = &input[..];
                    let pos = bytes
                        .iter()
                        .position(|b| !b.is_ascii_whitespace())
                        .unwrap_or(bytes.len());
                    !bytes[pos..].is_empty() && bytes[pos..][0] == b'{'
                };
                let value = if is_json {
                    // JSON format (compatibility)
                    scip_index::import_scip_json(&workspace, path)?
                } else {
                    // Native SCIP protobuf format
                    scip_index::import_native_scip(&workspace, path)?
                };
                output::response(
                    "index import-scip",
                    "index import-scip",
                    json!({ "path": path }),
                    &workspace.snapshot_id,
                    output::freshness(),
                    json!([value]),
                    Vec::new(),
                )
            }
            IndexCommand::Pack { output } => {
                let value = index::pack(&workspace, output)?;
                output::response(
                    "index pack",
                    "index pack",
                    json!({ "output": output }),
                    &workspace.snapshot_id,
                    output::freshness(),
                    value,
                    Vec::new(),
                )
            }
            IndexCommand::Unpack { path } => {
                let value = index::unpack(&workspace, path)?;
                output::response(
                    "index unpack",
                    "index unpack",
                    json!({ "path": path }),
                    &workspace.snapshot_id,
                    output::freshness(),
                    value,
                    Vec::new(),
                )
            }
        },
        Command::Hooks { command } => match command {
            HooksCommand::Install => output::response(
                "hooks install",
                "hooks install",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                index::hooks_install(&workspace)?,
                Vec::new(),
            ),
            HooksCommand::Uninstall => output::response(
                "hooks uninstall",
                "hooks uninstall",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                index::hooks_uninstall(&workspace)?,
                Vec::new(),
            ),
            HooksCommand::Status => output::response(
                "hooks status",
                "hooks status",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                index::hooks_status(&workspace)?,
                Vec::new(),
            ),
        },
        Command::Completions { .. } => unreachable!("handled before workspace discovery"),
    };

    let value = output::with_workspace_root(value, &workspace.root);
    output::emit(&cli.output, &value)?;
    Ok(exit_code)
}

fn emit_response(
    format: &crate::cli::OutputFormat,
    value: serde_json::Value,
    workspace_root: &std::path::Path,
) -> AppResult<i32> {
    let value = output::with_workspace_root(value, workspace_root);
    let exit_code = output::no_match_exit(&value["results"]);
    output::emit(format, &value)?;
    Ok(exit_code)
}

fn page_response(value: Value, page: search::QueryOutput) -> Value {
    output::with_page_meta(value, page.truncated, page.next_cursor, page.facets)
}

fn scoped_query(mut query: Value, opts: &ScanOptions) -> Value {
    if let Some(object) = query.as_object_mut() {
        object.insert("scope".to_string(), search::scope_value(opts));
    }
    query
}

fn scope_warnings(workspace: &Workspace, opts: &ScanOptions) -> Vec<String> {
    if opts.changed && workspace.changed.is_empty() {
        vec!["changed scope is empty; no full-workspace fallback was used".to_string()]
    } else {
        Vec::new()
    }
}

fn merge_warnings(mut first: Vec<String>, second: Vec<String>) -> Vec<String> {
    first.extend(second);
    first
}
