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

    // ------------------------------------------------------------------
    //  Search operations
    // ------------------------------------------------------------------

    /// Full-text / literal search (delegates to `search::find`).
    pub fn find(&self, text: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::find(&self.workspace, &scan, text, "literal", opts.context, false)?;
        Ok(output::response_with_index(
            "find",
            "find",
            json!({ "pattern": text, "mode": "literal" }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index,
            qo.results,
            Vec::new(),
        ))
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
        Ok(output::response_with_index(
            "grep",
            "find",
            json!({ "pattern": pattern, "mode": "regex", "context": opts.context }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index,
            qo.results,
            Vec::new(),
        ))
    }

    /// Find files whose path contains `pattern` (substring match).
    pub fn files(&self, pattern: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::files(&self.workspace, &scan, pattern, false)?;
        Ok(output::response_with_index(
            "files",
            "files",
            json!({ "pattern": pattern, "mode": "path_substring_or_glob" }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index,
            qo.results,
            Vec::new(),
        ))
    }

    /// Find files by strict glob pattern.
    pub fn glob(&self, pattern: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();
        let qo = search::files(&self.workspace, &scan, pattern, true)?;
        Ok(output::response_with_index(
            "glob",
            "files",
            json!({ "pattern": pattern, "mode": "strict_glob" }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index,
            qo.results,
            Vec::new(),
        ))
    }

    // ------------------------------------------------------------------
    //  Navigation
    // ------------------------------------------------------------------

    /// Read file contents (optionally with a line-range like `path:1-10`).
    pub fn read_file(&self, target: &str) -> Result<Value> {
        let result = search::read(&self.workspace, target)?;
        Ok(output::response(
            "read",
            "read",
            json!({ "target": target }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            json!([result]),
            Vec::new(),
        ))
    }

    /// List directory contents (non-recursive).
    pub fn list_dir(&self, dir: &str) -> Result<Value> {
        Ok(output::response(
            "list",
            "list",
            json!({ "dir": dir, "recursive": false }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            search::list(&self.workspace, Some(dir), false)?,
            Vec::new(),
        ))
    }

    // ------------------------------------------------------------------
    //  Precise queries  (SCIP → parser fallback)
    // ------------------------------------------------------------------

    /// Find definitions of `identifier` — prefers SCIP; falls back to tree-sitter.
    pub fn defs(&self, identifier: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try SCIP precise index first.
        if let Some(precise) = scip_index::defs(&self.workspace, &scan, identifier)? {
            return Ok(output::response_with_index(
                "defs",
                "defs",
                json!({ "identifier": identifier, "producer": "scip" }),
                &self.workspace.snapshot_id,
                output::precise_fact(),
                precise.index,
                precise.results,
                Vec::new(),
            ));
        }

        // 2. Fall back to tree-sitter parser.
        let (results, warnings) = syntax::defs(&self.workspace, &scan, identifier)?;
        Ok(output::response(
            "defs",
            "defs",
            json!({ "identifier": identifier, "producer": "tree_sitter_parser_fallback", "fallbackReason": "precise_scip_index_unavailable" }),
            &self.workspace.snapshot_id,
            output::parser_fact(),
            results,
            warnings,
        ))
    }

    /// Find references to `identifier` — prefers SCIP; falls back to text search.
    pub fn refs(&self, identifier: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try SCIP precise index first.
        if let Some(precise) = scip_index::refs(&self.workspace, &scan, identifier)? {
            return Ok(output::response_with_index(
                "refs",
                "refs",
                json!({ "identifier": identifier, "producer": "scip" }),
                &self.workspace.snapshot_id,
                output::precise_fact(),
                precise.index,
                precise.results,
                Vec::new(),
            ));
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
        Ok(output::response_with_index(
            "refs",
            "refs",
            json!({ "identifier": identifier, "mode": "identifier_boundary_text_search" }),
            &self.workspace.snapshot_id,
            output::source_fact(),
            qo.index,
            qo.results,
            vec!["refs is identifier-boundary text search unless a precise occurrence index is available"
                .to_string()],
        ))
    }

    /// Find symbols matching `query` — prefers SCIP; falls back to tree-sitter.
    pub fn symbols(&self, query: &str, opts: &QueryOptions) -> Result<Value> {
        let scan = opts.to_scan_options();

        // 1. Try SCIP precise index first.
        if let Some(precise) = scip_index::symbols(&self.workspace, &scan, query)? {
            return Ok(output::response_with_index(
                "symbols",
                "symbols",
                json!({ "query": query, "producer": "scip" }),
                &self.workspace.snapshot_id,
                output::precise_fact(),
                precise.index,
                precise.results,
                Vec::new(),
            ));
        }

        // 2. Fall back to tree-sitter.
        let (results, warnings) = syntax::symbols(&self.workspace, &scan, query)?;
        Ok(output::response(
            "symbols",
            "symbols",
            json!({ "query": query, "producer": "tree_sitter_parser" }),
            &self.workspace.snapshot_id,
            output::parser_fact(),
            results,
            warnings,
        ))
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
                return Ok(output::response_with_index(
                    "calls",
                    "calls",
                    json!({ "identifier": identifier, "producer": "graph" }),
                    &self.workspace.snapshot_id,
                    output::inferred_candidate(),
                    index_meta,
                    json!(results),
                    Vec::new(),
                ));
            }
        }

        // 2. Fall back to tree-sitter heuristic.
        let (results, warnings) = syntax::calls(&self.workspace, &scan, identifier)?;
        Ok(output::response(
            "calls",
            "calls",
            json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
            &self.workspace.snapshot_id,
            output::inferred_candidate(),
            results,
            warnings,
        ))
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
                return Ok(output::response_with_index(
                    "callers",
                    "callers",
                    json!({ "identifier": identifier, "producer": "graph" }),
                    &self.workspace.snapshot_id,
                    output::inferred_candidate(),
                    index_meta,
                    json!(results),
                    Vec::new(),
                ));
            }
        }

        // 2. Fall back to tree-sitter heuristic.
        let (results, warnings) = syntax::callers(&self.workspace, &scan, identifier)?;
        Ok(output::response(
            "callers",
            "callers",
            json!({ "identifier": identifier, "producer": "tree_sitter_call_heuristic" }),
            &self.workspace.snapshot_id,
            output::inferred_candidate(),
            results,
            warnings,
        ))
    }

    // ------------------------------------------------------------------
    //  Status
    // ------------------------------------------------------------------

    /// Return a list of changed / dirty files (git-status porcelain).
    pub fn changed(&self) -> Result<Value> {
        Ok(output::response(
            "changed",
            "changed",
            json!({}),
            &self.workspace.snapshot_id,
            output::source_fact(),
            search::changed(&self.workspace)?,
            Vec::new(),
        ))
    }

    /// Return workspace status including snapshot_id, dirty flag, etc.
    pub fn status(&self) -> Result<Value> {
        Ok(output::response(
            "status",
            "status",
            json!({}),
            &self.workspace.snapshot_id,
            output::source_fact(),
            json!([search::status(&self.workspace)]),
            Vec::new(),
        ))
    }
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
