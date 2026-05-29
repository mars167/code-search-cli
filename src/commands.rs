use serde_json::json;

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
        limit: cli.limit,
    };
    let mut exit_code = 0;

    if let Command::Completions { shell } = &cli.command {
        print!("{}", completions::script(shell));
        return Ok(0);
    }

    let workspace = Workspace::discover(&cli.path)?;

    let value = match &cli.command {
        Command::Find { text, mode } => {
            let query_output = search::find(&workspace, &scan_opts, text, mode, cli.context, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            output::response_with_index(
                "find",
                "find",
                json!({ "pattern": text, "mode": mode }),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index,
                query_output.results,
                Vec::new(),
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
            output::response_with_index(
                "grep",
                "find",
                json!({ "pattern": pattern, "mode": mode, "context": context }),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index,
                query_output.results,
                Vec::new(),
            )
        }
        Command::Files { pattern } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            output::response_with_index(
                "files",
                "files",
                json!({ "pattern": pattern, "mode": "path_substring_or_glob" }),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index,
                query_output.results,
                Vec::new(),
            )
        }
        Command::FindPath { pattern } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, false)?;
            exit_code = output::no_match_exit(&query_output.results);
            output::response_with_index(
                "find-path",
                "files",
                json!({ "pattern": pattern, "mode": "path_substring_or_glob" }),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index,
                query_output.results,
                Vec::new(),
            )
        }
        Command::Glob { pattern } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, true)?;
            exit_code = output::no_match_exit(&query_output.results);
            output::response_with_index(
                "glob",
                "files",
                json!({ "pattern": pattern, "mode": "strict_glob" }),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index,
                query_output.results,
                Vec::new(),
            )
        }
        Command::List { dir, recursive } => output::response(
            "list",
            "list",
            json!({ "dir": dir, "recursive": recursive }),
            &workspace.snapshot_id,
            output::source_fact(),
            search::list(&workspace, dir.as_deref(), *recursive)?,
            Vec::new(),
        ),
        Command::Tree { dir, depth } => output::response(
            "tree",
            "tree",
            json!({ "dir": dir, "depth": depth }),
            &workspace.snapshot_id,
            output::source_fact(),
            search::tree(&workspace, dir.as_deref(), *depth)?,
            Vec::new(),
        ),
        Command::Read { target } => {
            let result = search::read(&workspace, target)?;
            output::response(
                "read",
                "read",
                json!({ "target": target }),
                &workspace.snapshot_id,
                output::source_fact(),
                json!([result]),
                Vec::new(),
            )
        }
        Command::Refs { identifier } => {
            if let Some(precise) = scip_index::refs(&workspace, &scan_opts, identifier)? {
                return emit_response(
                    &cli.output,
                    output::response_with_index(
                        "refs",
                        "refs",
                        json!({ "identifier": identifier, "producer": "scip" }),
                        &workspace.snapshot_id,
                        output::precise_fact(),
                        precise.index,
                        precise.results,
                        Vec::new(),
                    ),
                );
            }
            let query_output =
                search::find(&workspace, &scan_opts, identifier, "literal", cli.context, true)?;
            exit_code = output::no_match_exit(&query_output.results);
            output::response_with_index(
                "refs",
                "refs",
                json!({ "identifier": identifier, "mode": "identifier_boundary_text_search" }),
                &workspace.snapshot_id,
                output::source_fact(),
                query_output.index,
                query_output.results,
                vec!["refs is identifier-boundary text search unless a precise occurrence index is available".to_string()],
            )
        }
        Command::Symbols { query } => {
            if let Some(precise) = scip_index::symbols(&workspace, &scan_opts, query)? {
                return emit_response(
                    &cli.output,
                    output::response_with_index(
                        "symbols",
                        "symbols",
                        json!({ "query": query, "producer": "scip" }),
                        &workspace.snapshot_id,
                        output::precise_fact(),
                        precise.index,
                        precise.results,
                        Vec::new(),
                    ),
                );
            }
            let (results, warnings) = syntax::symbols(&workspace, &scan_opts, query)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "symbols",
                "symbols",
                json!({ "query": query, "producer": "tree_sitter_parser" }),
                &workspace.snapshot_id,
                output::parser_fact(),
                results,
                warnings,
            )
        }
        Command::Defs { identifier } => {
            if let Some(precise) = scip_index::defs(&workspace, &scan_opts, identifier)? {
                return emit_response(
                    &cli.output,
                    output::response_with_index(
                        "defs",
                        "defs",
                        json!({ "identifier": identifier, "producer": "scip" }),
                        &workspace.snapshot_id,
                        output::precise_fact(),
                        precise.index,
                        precise.results,
                        Vec::new(),
                    ),
                );
            }
            let (results, warnings) = syntax::defs(&workspace, &scan_opts, identifier)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "defs",
                "defs",
                json!({ "identifier": identifier, "producer": "tree_sitter_parser_fallback", "fallbackReason": "precise_scip_index_unavailable" }),
                &workspace.snapshot_id,
                output::parser_fact(),
                results,
                warnings,
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
        Command::Changed => output::response(
            "changed",
            "changed",
            json!({}),
            &workspace.snapshot_id,
            output::source_fact(),
            search::changed(&workspace)?,
            Vec::new(),
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
        Command::Watch { once, status } => {
            let results = if *once {
                json!([index::build(&workspace, &scan_opts, false, true, false)?])
            } else {
                json!([index::watch_status(&workspace)])
            };
            output::response(
                "watch",
                "watch",
                json!({ "once": once, "status": status }),
                &workspace.snapshot_id,
                output::freshness(),
                results,
                if !once && !status {
                    vec!["long-running watcher daemon mode is intentionally not started in non-interactive command execution; use watch --once or serve wrappers".to_string()]
                } else {
                    Vec::new()
                },
            )
        }
        Command::Serve { no_watch } => output::response(
            "serve",
            "serve",
            json!({ "noWatch": no_watch }),
            &workspace.snapshot_id,
            output::freshness(),
            json!([index::serve_status(&workspace, *no_watch)]),
            vec!["HTTP/MCP adapters are expected to wrap the same CLI query service after JSON schema stabilization".to_string()],
        ),
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
                json!([index::build(&workspace, &scan_opts, *staged, *changed, *force)?]),
                Vec::new(),
            ),
            IndexCommand::Update => output::response(
                "index update",
                "index update",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                json!([index::build(&workspace, &scan_opts, false, true, false)?]),
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
                let input = std::fs::read(path)
                    .unwrap_or_default();
                // Skip leading whitespace/BOM to detect JSON format
                let is_json = {
                    let bytes = &input[..];
                    let pos = bytes.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(bytes.len());
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

    output::emit(&cli.output, &value)?;
    Ok(exit_code)
}

fn emit_response(format: &crate::cli::OutputFormat, value: serde_json::Value) -> AppResult<i32> {
    let exit_code = output::no_match_exit(&value["results"]);
    output::emit(format, &value)?;
    Ok(exit_code)
}
