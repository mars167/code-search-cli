//! Query service abstraction that wraps all code-search operations into a
//! unified interface.  Each method delegates to the appropriate backend
//! (text index, SCIP, tree-sitter parser, filesystem, git status) and
//! returns a JSON value that carries reliability metadata.
//!
//! The outputs follow the same envelope convention as the CLI layer so that
//! both the CLI and MCP adapter can consume identical results.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    graph, output, scip_index, search, syntax,
    workspace::{ScanOptions, Workspace},
};

// ---------------------------------------------------------------------------
// QueryOptions
// ---------------------------------------------------------------------------

/// Per-query filtering and display options.
#[derive(Clone, Debug)]
pub struct QueryOptions {
    /// Path substrings that files must contain to be included.
    pub include: Vec<String>,
    /// Path substrings that exclude files.
    pub exclude: Vec<String>,
    /// Language names to include.
    pub lang: Vec<String>,
    /// Restrict to git changed files.
    pub changed: bool,
    /// Pagination cursor.
    pub cursor: Option<String>,
    /// Allow broad queries to return full paginated results.
    pub allow_broad: bool,
    /// Maximum number of result items.
    pub limit: usize,
    /// Number of surrounding context lines (grep / find).
    pub context: u16,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            include: vec![],
            exclude: vec![],
            lang: vec![],
            changed: false,
            cursor: None,
            allow_broad: false,
            limit: 100,
            context: 0,
        }
    }
}

impl QueryOptions {
    fn to_scan_options(&self) -> ScanOptions {
        ScanOptions {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            lang: self.lang.clone(),
            changed: self.changed,
            cursor: self.cursor.clone(),
            allow_broad: self.allow_broad,
            hidden: false,
            no_ignore: false,
            limit: self.limit,
        }
    }
}

// ---------------------------------------------------------------------------
// QueryService
// ---------------------------------------------------------------------------

/// Stable query-service facade that wraps [`Workspace`] and all backends.
pub struct QueryService {
    workspace: Workspace,
}

impl QueryService {
    /// Discover the workspace rooted at `root`.
    pub fn new(root: &Path) -> Result<Self> {
        let workspace = Workspace::discover(root)?;
        Ok(Self { workspace })
    }

    /// Expose the workspace snapshot id (used for reliability metadata).
    pub fn snapshot_id(&self) -> &str {
        &self.workspace.snapshot_id
    }

    fn finalize(&self, value: Value) -> Value {
        output::with_workspace_root(value, &self.workspace.root)
    }

    // ------------------------------------------------------------------
    //  Search operations
    // ------------------------------------------------------------------

    /// Full-text / literal search (delegates to `search::find`).
    pub fn find(&self, text: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::find(&self.workspace, &scan, text, "literal", opts.context, false)?;
        let response = output::response_with_index(
            "find",
            "find",
            scoped_query(json!({ "pattern": text, "mode": "literal" }), &scan),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index.clone(),
            qo.results.clone(),
            Vec::new(),
        );
        Ok(self.finalize(page_response(response, qo)))
    }

    /// Regex search (delegates to `search::find` with mode=regex).
    pub fn grep(&self, pattern: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::find(
            &self.workspace,
            &scan,
            pattern,
            "regex",
            opts.context,
            false,
        )?;
        let response = output::response_with_index(
            "grep",
            "find",
            scoped_query(
                json!({ "pattern": pattern, "mode": "regex", "context": opts.context }),
                &scan,
            ),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index.clone(),
            qo.results.clone(),
            Vec::new(),
        );
        Ok(self.finalize(page_response(response, qo)))
    }

    /// Find files whose path contains `pattern` (substring match).
    pub fn files(&self, pattern: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::files(&self.workspace, &scan, pattern, false)?;
        let response = output::response_with_index(
            "files",
            "files",
            scoped_query(
                json!({ "pattern": pattern, "mode": "path_substring" }),
                &scan,
            ),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index.clone(),
            qo.results.clone(),
            Vec::new(),
        );
        Ok(self.finalize(page_response(response, qo)))
    }

    /// Find files by strict glob pattern.
    pub fn glob(&self, pattern: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::files(&self.workspace, &scan, pattern, true)?;
        let response = output::response_with_index(
            "glob",
            "files",
            scoped_query(json!({ "pattern": pattern, "mode": "strict_glob" }), &scan),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index.clone(),
            qo.results.clone(),
            Vec::new(),
        );
        Ok(self.finalize(page_response(response, qo)))
    }

    // ------------------------------------------------------------------
    //  Navigation
    // ------------------------------------------------------------------

    /// Read file contents (optionally with a line-range like `path:1-10`).
    pub fn read_file(&self, target: &str) -> Result<Value> {
        let result = search::read(&self.workspace, target)?;
        let reliability = if result
            .get("exact")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            output::source_fact()
        } else {
            output::source_fact_inexact()
        };
        let warnings = result
            .get("warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        Ok(self.finalize(output::response(
            "read",
            "read",
            json!({ "target": target }),
            &self.workspace.snapshot_id,
            reliability,
            json!([result]),
            warnings,
        )))
    }

    /// List directory contents.
    pub fn list(&self, dir: Option<&str>, recursive: bool, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        Ok(self.finalize(output::response(
            "list",
            "list",
            scoped_query(json!({ "dir": dir, "recursive": recursive }), &scan),
            &self.workspace.snapshot_id,
            output::source_fact(),
            search::list(&self.workspace, &scan, dir, recursive)?,
            Vec::new(),
        )))
    }

    /// List directory contents (non-recursive).
    pub fn list_dir(&self, dir: &str) -> Result<Value> {
        self.list(Some(dir), false, &QueryOptions::default())
    }

    /// Return a recursive tree view.
    pub fn tree(&self, dir: Option<&str>, depth: Option<u8>, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        Ok(self.finalize(output::response(
            "tree",
            "tree",
            scoped_query(json!({ "dir": dir, "depth": depth }), &scan),
            &self.workspace.snapshot_id,
            output::source_fact(),
            search::tree(&self.workspace, &scan, dir, depth)?,
            Vec::new(),
        )))
    }

    // ------------------------------------------------------------------
    //  Precise queries  (SCIP → parser fallback)
    // ------------------------------------------------------------------

    /// Find definitions of `identifier` — prefers SCIP; falls back to tree-sitter.
    pub fn defs(&self, identifier: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try SCIP precise index first.
        if let Some(precise) = scip_index::defs(&self.workspace, &scan, identifier)? {
            return Ok(self.finalize(output::response_with_index(
                "defs",
                "defs",
                json!({ "identifier": identifier, "producer": "scip" }),
                &self.workspace.snapshot_id,
                output::precise_fact(),
                precise.index,
                precise.results,
                Vec::new(),
            )));
        }

        // 2. Fall back to tree-sitter parser.
        let (results, warnings) = syntax::defs(&self.workspace, &scan, identifier)?;
        Ok(self.finalize(output::response(
            "defs",
            "defs",
            json!({ "identifier": identifier, "producer": "tree_sitter_parser_fallback", "fallbackReason": "precise_scip_index_unavailable" }),
            &self.workspace.snapshot_id,
            output::parser_fact(),
            results,
            merge_warnings(
                warnings,
                vec![
                    "precise_scip_index_unavailable: using tree-sitter parser fallback"
                        .to_string(),
                ],
            ),
        )))
    }

    /// Find references to `identifier` — prefers SCIP; falls back to text search.
    pub fn refs(&self, identifier: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try SCIP precise index first.
        if let Some(precise) = scip_index::refs(&self.workspace, &scan, identifier)? {
            return Ok(self.finalize(output::response_with_index(
                "refs",
                "refs",
                scoped_query(
                    json!({ "identifier": identifier, "producer": "scip" }),
                    &scan,
                ),
                &self.workspace.snapshot_id,
                output::precise_fact(),
                precise.index,
                precise.results,
                Vec::new(),
            )));
        }

        // 2. Fall back to identifier-boundary text search.
        let qo = search::find(
            &self.workspace,
            &scan,
            identifier,
            "literal",
            opts.context,
            true,
        )?;
        let response = output::response_with_index(
            "refs",
            "refs",
            scoped_query(
                json!({ "identifier": identifier, "mode": "identifier_boundary_text_search" }),
                &scan,
            ),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index.clone(),
            qo.results.clone(),
            vec!["refs is identifier-boundary text search unless a precise occurrence index is available"
                .to_string()],
        );
        Ok(self.finalize(page_response(response, qo)))
    }

    /// Find symbols matching `query` — prefers SCIP; falls back to tree-sitter.
    pub fn symbols(&self, query: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try SCIP precise index first.
        if let Some(precise) = scip_index::symbols(&self.workspace, &scan, query)? {
            let page = search::page_results(
                precise.results,
                &scan,
                "symbols",
                json!({ "query": query, "producer": "scip" }),
                &self.workspace.snapshot_id,
            )?;
            let response = output::response_with_index(
                "symbols",
                "symbols",
                scoped_query(json!({ "query": query, "producer": "scip" }), &scan),
                &self.workspace.snapshot_id,
                output::precise_fact(),
                precise.index,
                page.results.clone(),
                Vec::new(),
            );
            return Ok(self.finalize(output::with_page_meta(
                response,
                page.truncated,
                page.next_cursor,
                page.facets,
            )));
        }

        // 2. Fall back to tree-sitter.
        let (results, warnings) = syntax::symbols(&self.workspace, &scan, query)?;
        let page = search::page_results(
            results,
            &scan,
            "symbols",
            json!({ "query": query, "producer": "tree_sitter_parser" }),
            &self.workspace.snapshot_id,
        )?;
        let response = output::response(
            "symbols",
            "symbols",
            scoped_query(
                json!({ "query": query, "producer": "tree_sitter_parser" }),
                &scan,
            ),
            &self.workspace.snapshot_id,
            output::parser_fact(),
            page.results.clone(),
            merge_warnings(
                warnings,
                vec![
                    "precise_scip_index_unavailable: using tree-sitter parser fallback".to_string(),
                ],
            ),
        );
        Ok(self.finalize(output::with_page_meta(
            response,
            page.truncated,
            page.next_cursor,
            page.facets,
        )))
    }

    // ------------------------------------------------------------------
    //  Relation queries  (graph → tree-sitter fallback)
    // ------------------------------------------------------------------

    /// Find outgoing calls from `identifier`.
    pub fn calls(&self, identifier: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try graph backend first.
        let graph_store = graph::GraphStore::open(&self.workspace).ok();
        if let Some(ref store) = graph_store {
            if store.freshness_check().unwrap_or(false) {
                let results = store.query_calls(identifier).unwrap_or_default();
                let index_meta = store.index_meta(true);
                return Ok(self.finalize(output::response_with_index(
                    "calls",
                    "calls",
                    json!({ "identifier": identifier, "producer": "graph" }),
                    &self.workspace.snapshot_id,
                    output::inferred_candidate(),
                    index_meta,
                    json!(results),
                    Vec::new(),
                )));
            }
        }

        // 2. Fall back to tree-sitter heuristic.
        let (results, warnings) = syntax::calls(&self.workspace, &scan, identifier)?;
        Ok(self.finalize(output::response(
            "calls",
            "calls",
            json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
            &self.workspace.snapshot_id,
            output::inferred_candidate(),
            results,
            warnings,
        )))
    }

    /// Find incoming callers of `identifier`.
    pub fn callers(&self, identifier: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try graph backend first.
        let graph_store = graph::GraphStore::open(&self.workspace).ok();
        if let Some(ref store) = graph_store {
            if store.freshness_check().unwrap_or(false) {
                let results = store.query_callers(identifier).unwrap_or_default();
                let index_meta = store.index_meta(true);
                return Ok(self.finalize(output::response_with_index(
                    "callers",
                    "callers",
                    json!({ "identifier": identifier, "producer": "graph" }),
                    &self.workspace.snapshot_id,
                    output::inferred_candidate(),
                    index_meta,
                    json!(results),
                    Vec::new(),
                )));
            }
        }

        // 2. Fall back to tree-sitter heuristic.
        let (results, warnings) = syntax::callers(&self.workspace, &scan, identifier)?;
        Ok(self.finalize(output::response(
            "callers",
            "callers",
            json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
            &self.workspace.snapshot_id,
            output::inferred_candidate(),
            results,
            warnings,
        )))
    }

    // ------------------------------------------------------------------
    //  Status
    // ------------------------------------------------------------------

    /// Return a list of changed / dirty files (git-status porcelain).
    pub fn changed(&self) -> Result<Value> {
        Ok(self.finalize(output::with_summary_field(
            output::response(
                "changed",
                "changed",
                json!({}),
                &self.workspace.snapshot_id,
                output::source_fact(),
                search::changed(&self.workspace)?,
                Vec::new(),
            ),
            "changed",
            search::changed_summary(&self.workspace),
        )))
    }

    /// Return workspace status including snapshot_id, dirty flag, etc.
    pub fn status(&self) -> Result<Value> {
        Ok(self.finalize(output::response(
            "status",
            "status",
            json!({}),
            &self.workspace.snapshot_id,
            output::source_fact(),
            json!([search::status(&self.workspace)]),
            Vec::new(),
        )))
    }
}

fn scoped_query(mut query: Value, opts: &ScanOptions) -> Value {
    if let Some(object) = query.as_object_mut() {
        object.insert("scope".to_string(), search::scope_value(opts));
    }
    query
}

fn page_response(value: Value, page: search::QueryOutput) -> Value {
    output::with_budget(
        output::with_guard(
            output::with_page_meta(value, page.truncated, page.next_cursor, page.facets),
            page.guard,
        ),
        page.budget,
    )
}

fn merge_warnings(mut first: Vec<String>, second: Vec<String>) -> Vec<String> {
    first.extend(second);
    first
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn setup_with_file(name: &str, content: &str) -> (tempfile::TempDir, QueryService) {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join(name), content).unwrap();
        let svc = QueryService::new(dir.path()).unwrap();
        (dir, svc)
    }

    // -- QueryOptions → ScanOptions round-trip --------------------------

    #[test]
    fn query_options_default_is_sensible() {
        let opts = QueryOptions::default();
        assert_eq!(opts.limit, 100);
        assert_eq!(opts.context, 0);
        assert!(opts.include.is_empty());
        assert!(opts.exclude.is_empty());
    }

    #[test]
    fn query_options_to_scan_options_preserves_limit() {
        let opts = QueryOptions {
            limit: 42,
            ..Default::default()
        };
        let scan = opts.to_scan_options();
        assert_eq!(scan.limit, 42);
    }

    // -- find -----------------------------------------------------------

    #[test]
    fn find_returns_source_fact_reliability() {
        let (_dir, svc) =
            setup_with_file("src/main.rs", "fn main() {\n    println!(\"needle\");\n}\n");
        let result = svc.find("needle", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        assert_eq!(result["results"][0]["path"], "src/main.rs");
        assert_eq!(result["results"][0]["range"]["start"]["line"], 2);
        assert!(result["snapshot_id"]
            .as_str()
            .unwrap()
            .contains("worktree:"));
    }

    #[test]
    fn find_includes_broad_guard_for_query_service_consumers() {
        let dir = tempdir().unwrap();
        for idx in 0..6 {
            fs::write(
                dir.path().join(format!("file{idx}.rs")),
                "pub fn sample() { println!(\"public\"); }\n",
            )
            .unwrap();
        }
        let svc = QueryService::new(dir.path()).unwrap();

        let result = svc.find("public", &QueryOptions::default()).unwrap();

        assert_eq!(result["guard"]["triggered"], true);
        assert_eq!(result["guard"]["reason"], "broad_literal_pattern");
        assert_eq!(result["results"].as_array().unwrap().len(), 5);
        assert!(result["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "broad_query_guard_triggered"));
    }

    #[test]
    fn query_service_read_command_preserves_workspace_root() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src dir")).unwrap();
        fs::write(dir.path().join("src dir/a b.rs"), "needle\n").unwrap();
        let svc = QueryService::new(dir.path()).unwrap();

        let result = svc.find("needle", &QueryOptions::default()).unwrap();
        let argv = result["results"][0]["readCommandArgv"].as_array().unwrap();
        assert_eq!(argv[1], "--path");
        assert_eq!(argv[3], "read");
        assert_eq!(argv[4], "src dir/a b.rs:1");
    }

    // -- grep -----------------------------------------------------------

    #[test]
    fn grep_returns_regex_matches() {
        let (_dir, svc) = setup_with_file("sample.txt", "foo\nbar\nbaz\n");
        let result = svc.grep("ba[rz]", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        let paths: Vec<_> = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["matchText"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"bar".to_string()));
        assert!(paths.contains(&"baz".to_string()));
        assert_eq!(paths.len(), 2);
    }

    // -- files ----------------------------------------------------------

    #[test]
    fn files_returns_matching_paths() {
        let (_dir, svc) = setup_with_file("src/main.rs", "// empty\n");
        let result = svc.files("main", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        assert_eq!(result["results"][0]["path"], "src/main.rs");
    }

    // -- glob -----------------------------------------------------------

    #[test]
    fn glob_strictly_matches_patterns() {
        let (_dir, svc) = setup_with_file("src/main.rs", "// empty\n");
        let result = svc.glob("**/main.rs", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["results"][0]["path"], "src/main.rs");
    }

    // -- read_file ------------------------------------------------------

    #[test]
    fn read_file_returns_content_with_reliability() {
        let (_dir, svc) = setup_with_file("sample.txt", "one\ntwo\nthree\n");
        let result = svc.read_file("sample.txt:2-3").unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["results"][0]["content"], "two\nthree");
        assert_eq!(result["results"][0]["exact"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
    }

    // -- list_dir -------------------------------------------------------

    #[test]
    fn list_dir_returns_directory_entries() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let svc = QueryService::new(dir.path()).unwrap();
        let result = svc.list_dir("src").unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        let results = result["results"].as_array().unwrap();
        assert!(results.iter().any(|r| r["path"] == "src/main.rs"));
    }

    // -- defs (parser fallback) -----------------------------------------

    #[test]
    fn defs_falls_back_to_parser_when_no_scip_index() {
        let (_dir, svc) = setup_with_file("src/lib.rs", "fn alpha() {}\nfn beta() {}\n");
        let result = svc.defs("alpha", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "parser_fact");
        assert_eq!(result["reliability"]["exact"], false);
        let results = result["results"].as_array().unwrap();
        assert!(results.iter().any(|r| r["name"] == "alpha"));
    }

    // -- refs (text-search fallback) ------------------------------------

    #[test]
    fn refs_falls_back_to_text_search_when_no_scip_index() {
        let (_dir, svc) = setup_with_file(
            "src/main.rs",
            "fn main() {\n    helper();\n}\nfn helper() {}\n",
        );
        let result = svc.refs("helper", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        assert!(result["results"].as_array().unwrap().len() >= 2);
        // Warnings should mention that it's text search, not SCIP
        let warnings = result["warnings"].as_array().unwrap();
        assert!(!warnings.is_empty());
    }

    // -- symbols (parser fallback) --------------------------------------

    #[test]
    fn symbols_falls_back_to_parser() {
        let (_dir, svc) = setup_with_file("src/lib.rs", "fn alpha() {}\nstruct Beta {}\n");
        let result = svc.symbols("alpha", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "parser_fact");
        let results = result["results"].as_array().unwrap();
        assert!(results.iter().any(|r| r["name"] == "alpha"));
    }

    // -- calls / callers (tree-sitter fallback) -------------------------

    #[test]
    fn calls_returns_inferred_candidates() {
        let (_dir, svc) =
            setup_with_file("src/lib.rs", "fn alpha() {\n    beta();\n}\nfn beta() {}\n");
        let result = svc.calls("alpha", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "inferred_candidate");
        assert_eq!(result["reliability"]["exact"], false);
        let results = result["results"].as_array().unwrap();
        assert!(results.iter().any(|r| r["target"] == "beta"));
    }

    #[test]
    fn callers_returns_inferred_candidates() {
        let (_dir, svc) =
            setup_with_file("src/lib.rs", "fn alpha() {\n    beta();\n}\nfn beta() {}\n");
        let result = svc.callers("beta", &QueryOptions::default()).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "inferred_candidate");
        let results = result["results"].as_array().unwrap();
        assert!(results.iter().any(|r| r["enclosingSymbol"] == "alpha"));
    }

    // -- changed --------------------------------------------------------

    #[test]
    fn changed_returns_array_without_git() {
        let (_dir, svc) = setup_with_file("sample.txt", "hello\n");
        let result = svc.changed().unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        // In a non-git dir this returns an empty array.
        assert!(result["results"].is_array());
    }

    // -- status ---------------------------------------------------------

    #[test]
    fn status_contains_snapshot_and_dirty() {
        let (_dir, svc) = setup_with_file("sample.txt", "hello\n");
        let result = svc.status().unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["reliability"]["level"], "source_fact");
        let items = result["results"].as_array().unwrap();
        let status_item = &items[0];
        assert!(status_item["snapshot_id"].as_str().is_some());
        assert!(status_item["dirty"].as_bool().is_some());
    }
}
