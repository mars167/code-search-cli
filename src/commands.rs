use serde_json::{json, Value};

use crate::{
    cli::{Cli, Command, HooksCommand, IndexCommand, OutputFormat, QueryCommand},
    completions, graph, index, output,
    query_input::{compatible_input_needs_expansion, InputPlan},
    saved_query, scip_index, search,
    search_pattern::SearchPatternMode,
    syntax,
    workspace::{ScanOptions, Workspace},
    AppResult,
};

pub fn run(cli: Cli) -> AppResult<i32> {
    let verbose = output::VerboseLogger::new(cli.verbose);
    verbose.log(format!("command={}", command_name(&cli.command)));
    verbose.log(format!("path={}", cli.path));

    let scan_opts = ScanOptions {
        dirs: cli.dir.clone(),
        extensions: cli.ext.clone(),
        file_patterns: cli.file_pattern.clone(),
        file_mode: cli.file_mode,
        case_sensitive: cli.case_sensitive,
        input_mode: cli.input_mode,
        include: cli.include.clone(),
        exclude: cli.exclude.clone(),
        hidden: cli.hidden,
        no_ignore: cli.no_ignore,
        lang: cli.lang.clone(),
        changed: cli.changed,
        cursor: cli.cursor.clone(),
        allow_broad: cli.allow_broad,
        limit: cli.limit,
        ..ScanOptions::default()
    };
    let mut exit_code = 0;

    if let Command::Completions { shell } = &cli.command {
        print!("{}", completions::script(shell));
        return Ok(0);
    }

    let workspace = Workspace::discover(&cli.path)?;
    verbose.log(format!(
        "workspace root={} snapshot_id={} dirty={} staged={} worktree={}",
        workspace.root.display(),
        workspace.snapshot_id,
        workspace.dirty,
        workspace.staged_count,
        workspace.worktree_count
    ));
    let scope_warnings = scope_warnings(&workspace, &scan_opts);

    let value = match &cli.command {
        Command::Find { text, mode } => {
            let query_output = search::find(
                &workspace,
                &scan_opts,
                text,
                (*mode).into(),
                cli.context,
                false,
            )?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "find",
                    "find",
                    scoped_query(
                        json!({ "pattern": text, "mode": mode.as_str(), "caseSensitive": scan_opts.case_sensitive, "context": cli.context }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    output::IndexedResponseParts::new(
                        query_output.index.clone(),
                        query_output.results.clone(),
                        scope_warnings.clone(),
                    ),
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
            let query_output = search::find(
                &workspace,
                &scan_opts,
                pattern,
                (*mode).into(),
                context,
                false,
            )?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "grep",
                    "find",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": mode.as_str(), "caseSensitive": scan_opts.case_sensitive, "context": context }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    output::IndexedResponseParts::new(
                        query_output.index.clone(),
                        query_output.results.clone(),
                        scope_warnings.clone(),
                    ),
                ),
                query_output,
            )
        }
        Command::Files { pattern, mode } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, *mode)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "files",
                    "files",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": path_mode_label("files", *mode), "caseSensitive": scan_opts.case_sensitive }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    output::IndexedResponseParts::new(
                        query_output.index.clone(),
                        query_output.results.clone(),
                        scope_warnings.clone(),
                    ),
                ),
                query_output,
            )
        }
        Command::FindPath { pattern, mode } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, *mode)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "find-path",
                    "files",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": path_mode_label("find-path", *mode), "caseSensitive": scan_opts.case_sensitive }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    output::IndexedResponseParts::new(
                        query_output.index.clone(),
                        query_output.results.clone(),
                        scope_warnings.clone(),
                    ),
                ),
                query_output,
            )
        }
        Command::Glob { pattern, mode } => {
            let query_output = search::files(&workspace, &scan_opts, pattern, *mode)?;
            exit_code = output::no_match_exit(&query_output.results);
            page_response(
                output::response_with_index(
                    "glob",
                    "files",
                    scoped_query(
                        json!({ "pattern": pattern, "mode": path_mode_label("glob", *mode), "caseSensitive": scan_opts.case_sensitive }),
                        &scan_opts,
                    ),
                    &workspace.snapshot_id,
                    output::source_fact(),
                    output::IndexedResponseParts::new(
                        query_output.index.clone(),
                        query_output.results.clone(),
                        scope_warnings.clone(),
                    ),
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
                if has_results(&precise.results)
                    || !compatible_input_needs_expansion(identifier, scan_opts.input_mode)
                {
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
                            output::IndexedResponseParts::new(
                                precise.index,
                                precise.results,
                                scope_warnings.clone(),
                            ),
                        ),
                        &workspace,
                        cli.save_query.as_deref(),
                    );
                }
            }
            let mut query_output = search::find(
                &workspace,
                &scan_opts,
                identifier,
                SearchPatternMode::Literal,
                cli.context,
                true,
            )?;
            let definition_ranges =
                syntax::definition_ranges(&workspace, &scan_opts, identifier).unwrap_or_default();
            query_output.results = search::annotate_identifier_refs_with_definitions(
                query_output.results,
                identifier,
                &definition_ranges,
            );
            exit_code = output::no_match_exit(&query_output.results);
            page_response(output::response_with_index(
                "refs",
                "refs",
                scoped_query(json!({ "identifier": identifier, "mode": "identifier_boundary_text_search" }), &scan_opts),
                &workspace.snapshot_id,
                output::source_fact(),
                output::IndexedResponseParts::new(
                    query_output.index.clone(),
                    query_output.results.clone(),
                    merge_warnings(
                        vec!["refs is identifier-boundary text search unless a precise occurrence index is available".to_string()],
                        scope_warnings.clone(),
                    ),
                ),
            ), query_output)
        }
        Command::Symbols { query } => {
            if let Some(precise) = scip_index::symbols(&workspace, &scan_opts, query)? {
                if has_results(&precise.results)
                    || !compatible_input_needs_expansion(query, scan_opts.input_mode)
                {
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
                                scoped_query(
                                    json!({ "query": query, "producer": "scip" }),
                                    &scan_opts,
                                ),
                                &workspace.snapshot_id,
                                output::precise_fact(),
                                output::IndexedResponseParts::new(
                                    precise.index,
                                    page.results.clone(),
                                    scope_warnings.clone(),
                                ),
                            ),
                            page.truncated,
                            page.next_cursor,
                            page.facets,
                        ),
                        &workspace,
                        cli.save_query.as_deref(),
                    );
                }
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
                    merge_warnings(
                        warnings,
                        merge_warnings(
                            vec![
                                "precise_scip_index_unavailable: using tree-sitter parser fallback"
                                    .to_string(),
                            ],
                            scope_warnings.clone(),
                        ),
                    ),
                ),
                page.truncated,
                page.next_cursor,
                page.facets,
            )
        }
        Command::Defs { identifier } => {
            if let Some(precise) = scip_index::defs(&workspace, &scan_opts, identifier)? {
                if has_results(&precise.results)
                    || !compatible_input_needs_expansion(identifier, scan_opts.input_mode)
                {
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
                            output::IndexedResponseParts::new(
                                precise.index,
                                precise.results,
                                scope_warnings.clone(),
                            ),
                        ),
                        &workspace,
                        cli.save_query.as_deref(),
                    );
                }
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
                merge_warnings(
                    warnings,
                    merge_warnings(
                        vec![
                            "precise_scip_index_unavailable: using tree-sitter parser fallback"
                                .to_string(),
                        ],
                        scope_warnings.clone(),
                    ),
                ),
            )
        }
        Command::Calls { identifier } => {
            // Try graph backend first (if built and fresh)
            let graph_store = graph::GraphStore::open(&workspace).ok();
            if let Some(ref store) = graph_store {
                if store.freshness_check().unwrap_or(false) {
                    let plan = InputPlan::new(identifier, scan_opts.input_mode);
                    let results = store
                        .query_calls_with_input(&plan, scan_opts.case_sensitive)
                        .and_then(|results| {
                            graph::filter_candidates_by_scan_scope(&workspace, &scan_opts, results)
                        })
                        .unwrap_or_default();
                    if !results.is_empty() {
                        let index_meta = store.index_meta(true);
                        let warnings: Vec<String> = Vec::new();
                        return emit_response(
                            &cli.output,
                            output::response_with_index(
                                "calls",
                                "calls",
                                scoped_query(
                                    json!({ "identifier": identifier, "producer": "graph" }),
                                    &scan_opts,
                                ),
                                &workspace.snapshot_id,
                                output::inferred_candidate(),
                                output::IndexedResponseParts::new(
                                    index_meta,
                                    json!(results),
                                    warnings,
                                ),
                            ),
                            &workspace,
                            cli.save_query.as_deref(),
                        );
                    }
                }
            }
            // Fall back to tree-sitter
            let (results, warnings) = syntax::calls(&workspace, &scan_opts, identifier)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "calls",
                "calls",
                scoped_query(
                    json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
                    &scan_opts,
                ),
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
                    let plan = InputPlan::new(identifier, scan_opts.input_mode);
                    let results = store
                        .query_callers_with_input(&plan, scan_opts.case_sensitive)
                        .and_then(|results| {
                            graph::filter_candidates_by_scan_scope(&workspace, &scan_opts, results)
                        })
                        .unwrap_or_default();
                    if !results.is_empty() {
                        let index_meta = store.index_meta(true);
                        let warnings: Vec<String> = Vec::new();
                        return emit_response(
                            &cli.output,
                            output::response_with_index(
                                "callers",
                                "callers",
                                scoped_query(
                                    json!({ "identifier": identifier, "producer": "graph" }),
                                    &scan_opts,
                                ),
                                &workspace.snapshot_id,
                                output::inferred_candidate(),
                                output::IndexedResponseParts::new(
                                    index_meta,
                                    json!(results),
                                    warnings,
                                ),
                            ),
                            &workspace,
                            cli.save_query.as_deref(),
                        );
                    }
                }
            }
            // Fall back to tree-sitter
            let (results, warnings) = syntax::callers(&workspace, &scan_opts, identifier)?;
            exit_code = output::no_match_exit(&results);
            output::response(
                "callers",
                "callers",
                scoped_query(
                    json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
                    &scan_opts,
                ),
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
                Vec::new(),
            )
        }
        Command::Query { command } => match command {
            QueryCommand::Replay { name, snapshot } => {
                let value = saved_query::replay(&workspace, name, snapshot)?;
                exit_code = output::no_match_exit(&value["results"]);
                value
            }
            QueryCommand::Show { name } => output::response(
                "query show",
                "query",
                json!({ "name": name }),
                &workspace.snapshot_id,
                output::source_fact(),
                json!([saved_query::show(&workspace, name)?]),
                Vec::new(),
            ),
            QueryCommand::List => output::response(
                "query list",
                "query",
                json!({}),
                &workspace.snapshot_id,
                output::source_fact(),
                saved_query::list(&workspace)?,
                Vec::new(),
            ),
            QueryCommand::Delete { name } => output::response(
                "query delete",
                "query",
                json!({ "name": name }),
                &workspace.snapshot_id,
                output::source_fact(),
                saved_query::delete(&workspace, name)?,
                Vec::new(),
            ),
        },
        Command::Index { command } => match command {
            IndexCommand::Build {
                staged,
                changed,
                force,
                no_semantic,
            } => {
                let semantic_enabled = !*no_semantic;
                let result = with_progress(
                    &cli.output,
                    "Building index",
                    "Index build complete",
                    || {
                        index::build(
                            &workspace,
                            &scan_opts,
                            *staged,
                            *changed,
                            *force,
                            semantic_enabled,
                            verbose,
                        )
                    },
                )?;
                output::response(
                    "index build",
                    "index build",
                    json!({ "staged": staged, "changed": changed, "force": force, "noSemantic": no_semantic }),
                    &workspace.snapshot_id,
                    output::freshness(),
                    json!([result]),
                    Vec::new(),
                )
            }
            IndexCommand::Update => output::response(
                "index update",
                "index update",
                json!({}),
                &workspace.snapshot_id,
                output::freshness(),
                json!([with_progress(
                    &cli.output,
                    "Updating index",
                    "Index update complete",
                    || index::update(&workspace, &scan_opts, verbose)
                )?]),
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
            IndexCommand::Skipped { staged } => output::response(
                "index skipped",
                "index skipped",
                json!({ "staged": staged }),
                &workspace.snapshot_id,
                output::freshness(),
                json!([index::skipped(&workspace, *staged)?]),
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
            IndexCommand::GenerateScip {
                lang,
                output: out_path,
            } => {
                if lang != "go" {
                    anyhow::bail!("SCIP generation is currently only supported for Go (--lang go)");
                }
                let out = out_path.as_deref().unwrap_or("index.scip.json");
                crate::scip_indexer::generate_go_scip(
                    std::path::Path::new(&cli.path),
                    std::path::Path::new(out),
                )?;
                output::response(
                    "index generate-scip",
                    "index generate-scip",
                    json!({"lang": lang, "output": out}),
                    &workspace.snapshot_id,
                    output::freshness(),
                    json!([{"status": "generated", "output": out}]),
                    Vec::new(),
                )
            }
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
                let value = with_progress(
                    &cli.output,
                    "Importing SCIP index",
                    "SCIP import complete",
                    || {
                        if is_json {
                            // JSON format (compatibility)
                            scip_index::import_scip_json(&workspace, path)
                        } else {
                            // Native SCIP protobuf format
                            scip_index::import_native_scip(&workspace, path)
                        }
                    },
                )?;
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
                let value =
                    with_progress(&cli.output, "Packing index", "Index pack complete", || {
                        index::pack(&workspace, output)
                    })?;
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
                let value = with_progress(
                    &cli.output,
                    "Unpacking index",
                    "Index unpack complete",
                    || index::unpack(&workspace, path),
                )?;
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

    let mut value = output::with_workspace_root(value, &workspace.root);
    attach_saved_query(&mut value, &workspace, cli.save_query.as_deref())?;
    output::emit(&cli.output, &value)?;
    Ok(exit_code)
}

fn emit_response(
    format: &crate::cli::OutputFormat,
    value: serde_json::Value,
    workspace: &Workspace,
    save_query: Option<&str>,
) -> AppResult<i32> {
    let mut value = output::with_workspace_root(value, &workspace.root);
    attach_saved_query(&mut value, workspace, save_query)?;
    let exit_code = output::no_match_exit(&value["results"]);
    output::emit(format, &value)?;
    Ok(exit_code)
}

fn with_progress<T, F>(
    format: &OutputFormat,
    start_message: &str,
    finish_message: &str,
    work: F,
) -> AppResult<T>
where
    F: FnOnce() -> AppResult<T>,
{
    let progress = output::ProgressIndicator::start(format, start_message);
    let result = work();
    progress.finish(if result.is_ok() { finish_message } else { "" });
    result
}

fn attach_saved_query(
    value: &mut Value,
    workspace: &Workspace,
    save_query: Option<&str>,
) -> AppResult<()> {
    if let Some(name) = save_query {
        value["savedQuery"] = saved_query::save_from_response(workspace, name, value)?;
    }
    Ok(())
}

fn page_response(value: Value, page: search::QueryOutput) -> Value {
    let page_value = output::with_budget(
        output::with_guard(
            output::with_page_meta(
                value,
                page.truncated,
                page.next_cursor.clone(),
                page.facets.clone(),
            ),
            page.guard.clone(),
        ),
        page.budget.clone(),
    );
    search::attach_query_diagnostics(page_value, &page)
}

fn has_results(value: &Value) -> bool {
    value.as_array().is_some_and(|results| !results.is_empty())
}

fn path_mode_label(command: &str, mode: SearchPatternMode) -> &'static str {
    match (command, mode) {
        ("files" | "find-path", SearchPatternMode::Literal) => "path_substring",
        ("glob", SearchPatternMode::Glob) => "strict_glob",
        (_, mode) => mode.as_str(),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Find { .. } => "find",
        Command::Grep { .. } => "grep",
        Command::Files { .. } => "files",
        Command::FindPath { .. } => "find-path",
        Command::Glob { .. } => "glob",
        Command::List { .. } => "list",
        Command::Tree { .. } => "tree",
        Command::Read { .. } => "read",
        Command::Refs { .. } => "refs",
        Command::Symbols { .. } => "symbols",
        Command::Defs { .. } => "defs",
        Command::Calls { .. } => "calls",
        Command::Callers { .. } => "callers",
        Command::Changed => "changed",
        Command::Status => "status",
        Command::Mcp => "mcp",
        Command::Watch { .. } => "watch",
        Command::Serve { .. } => "serve",
        Command::Query { .. } => "query",
        Command::Index { command } => match command {
            IndexCommand::Build { .. } => "index build",
            IndexCommand::Update => "index update",
            IndexCommand::Status => "index status",
            IndexCommand::Skipped { .. } => "index skipped",
            IndexCommand::Verify => "index verify",
            IndexCommand::Clean => "index clean",
            IndexCommand::ImportScip { .. } => "index import-scip",
            IndexCommand::GenerateScip { .. } => "index generate-scip",
            IndexCommand::Pack { .. } => "index pack",
            IndexCommand::Unpack { .. } => "index unpack",
        },
        Command::Hooks { .. } => "hooks",
        Command::Completions { .. } => "completions",
    }
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
