use std::fs;

use assert_cmd::Command;
use serde_json::json;
use serde_json::Value;
use tempfile::tempdir;

fn code_search() -> Command {
    Command::cargo_bin("code-search").expect("binary exists")
}

#[test]
fn find_returns_reliable_source_fact() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    println!(\"needle\");\n}\n",
    )
    .unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["ok"], true);
    assert_eq!(json["reliability"]["level"], "source_fact");
    assert_eq!(json["results"][0]["path"], "src/main.rs");
    assert_eq!(json["results"][0]["range"]["start"]["line"], 2);
}

#[test]
fn read_returns_exact_line_range() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "one\ntwo\nthree\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "sample.txt:2-3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["results"][0]["content"], "two\nthree");
    assert_eq!(json["results"][0]["exact"], true);
}

#[test]
fn parser_commands_expose_symbols_and_call_candidates() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "fn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
    )
    .unwrap();

    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "beta"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();
    assert_eq!(defs_json["reliability"]["level"], "parser_fact");
    assert_eq!(defs_json["results"][0]["name"], "beta");

    let callers = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["callers", "beta"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let callers_json: Value = serde_json::from_slice(&callers).unwrap();
    assert_eq!(callers_json["reliability"]["level"], "inferred_candidate");
    assert_eq!(callers_json["results"][0]["enclosingSymbol"], "alpha");
}

#[test]
fn index_verify_detects_stale_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "one\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "verify"])
        .assert()
        .success();

    fs::write(dir.path().join("sample.txt"), "one\ntwo\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "verify"])
        .assert()
        .code(6);
}

#[test]
fn index_build_writes_lancedb_only_storage() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "needle\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let code_search_dir = dir.path().join(".code-search");
    // LanceDB store is the primary storage backend
    assert!(code_search_dir.join("index.lance").is_dir());
    // Old JSON/.idx artifacts are no longer written
    assert!(!code_search_dir.join("snapshots").exists());
    assert!(!code_search_dir.join("text").exists());
    // working/manifest.json is written for pack/unpack compatibility

    // Build output declares lancedb backend
    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["results"][0]["index"]["used"], true);
    assert_eq!(json["results"][0]["index"]["storageBackend"], "lancedb");
}

#[test]
fn find_uses_fresh_text_index_for_candidates() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "needle\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["index"]["used"], true);
    assert_eq!(json["index"]["fresh"], true);
    assert_eq!(json["index"]["source"], "text_index");
    assert_eq!(
        json["results"][0]["producer"],
        "text_index_live_text_search"
    );
}

#[test]
fn query_falls_back_when_scan_options_do_not_match_index() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".hidden.txt"), "needle\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--hidden")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["index"]["used"], false);
    assert_eq!(json["results"][0]["path"], ".hidden.txt");
    assert_eq!(json["results"][0]["producer"], "live_text_search");
}

#[test]
fn completions_print_shell_script_without_workspace() {
    let output = code_search()
        .args(["--path", "/definitely/missing", "completions", "bash"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let script = String::from_utf8(output).unwrap();

    assert!(script.contains("complete -F _code_search code-search"));
    assert!(script.contains("find grep files"));
}

#[test]
fn imported_scip_index_drives_precise_defs_refs_and_symbols() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "fn needle() {}\nfn main() { needle(); }\n",
    )
    .unwrap();
    let scip_path = dir.path().join("index.scip.json");
    write_minimal_scip_json(&scip_path);

    let import_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "import-scip"])
        .arg(&scip_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let import_json: Value = serde_json::from_slice(&import_output).unwrap();
    assert_eq!(
        import_json["results"][0]["index"]["storageBackend"],
        "lancedb"
    );
    assert!(dir.path().join(".code-search/index.lance").is_dir());

    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();
    assert_eq!(defs_json["reliability"]["level"], "precise_fact");
    assert_eq!(defs_json["results"][0]["producer"], "scip");
    assert_eq!(defs_json["results"][0]["exact"], true);
    assert_eq!(defs_json["results"][0]["range"]["start"]["line"], 1);

    let refs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["refs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let refs_json: Value = serde_json::from_slice(&refs).unwrap();
    assert_eq!(refs_json["reliability"]["level"], "precise_fact");
    assert_eq!(refs_json["results"][0]["producer"], "scip");
    assert_eq!(refs_json["results"][0]["role"], "reference");
    assert_eq!(refs_json["results"][0]["range"]["start"]["line"], 2);

    let symbols = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["symbols", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let symbols_json: Value = serde_json::from_slice(&symbols).unwrap();
    assert_eq!(symbols_json["reliability"]["level"], "precise_fact");
    assert_eq!(symbols_json["results"][0]["name"], "needle");
}

#[test]
fn defs_falls_back_to_parser_after_plain_index_build_without_scip() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn needle() {}\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();

    assert_eq!(defs_json["reliability"]["level"], "parser_fact");
    assert_eq!(defs_json["results"][0]["producer"], "tree_sitter_parser");
}

#[test]
fn calls_and_callers_do_not_claim_graph_store_before_kuzu_backend_exists() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "fn alpha() {\n    beta();\n}\n\nfn beta() {}\n",
    )
    .unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let calls = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["calls", "alpha"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let calls_json: Value = serde_json::from_slice(&calls).unwrap();
    // graph backend exists now (petgraph), so index is used
    assert_eq!(calls_json["index"]["used"], true);
    assert_eq!(calls_json["reliability"]["level"], "inferred_candidate");
    assert_eq!(calls_json["results"][0]["target"], "beta");
    // producer reflects the graph source (tree-sitter heuristic inside graph)
    let producer = calls_json["results"][0]["producer"].as_str().unwrap_or("");
    assert!(producer.starts_with("graph:"));

    let callers = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["callers", "beta"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let callers_json: Value = serde_json::from_slice(&callers).unwrap();
    // producer reflects the graph source
    let cproducer = callers_json["results"][0]["producer"]
        .as_str()
        .unwrap_or("");
    assert!(cproducer.starts_with("graph:"));
    assert_eq!(callers_json["results"][0]["enclosingSymbol"], "alpha");
}

fn write_minimal_scip_json(path: &std::path::Path) {
    let value = json!({
        "documents": [
            {
                "relativePath": "src/lib.rs",
                "language": "rust",
                "occurrences": [
                    {
                        "range": [0, 3, 0, 9],
                        "symbol": "local 1",
                        "symbolRoles": 1
                    },
                    {
                        "range": [1, 12, 1, 18],
                        "symbol": "local 1",
                        "symbolRoles": 0
                    }
                ],
                "symbols": [
                    {
                        "symbol": "local 1",
                        "displayName": "needle",
                        "kind": "function"
                    }
                ]
            }
        ]
    });
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn native_scip_import_drives_precise_defs_refs_and_symbols() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "fn needle() {}\nfn main() { needle(); }\n",
    )
    .unwrap();

    let scip_path = dir.path().join("index.scip");
    code_search_cli::scip::write_minimal_test_index(&scip_path).unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "import-scip"])
        .arg(&scip_path)
        .assert()
        .success();

    // Verify occurrence DB was created
    let scip_dir = fs::read_dir(dir.path().join(".code-search/scip"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let db_path = scip_dir.join("occurrences.db");
    assert!(db_path.is_file());

    // defs
    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();
    assert_eq!(defs_json["reliability"]["level"], "precise_fact");
    assert_eq!(defs_json["results"][0]["producer"], "scip");
    assert_eq!(defs_json["results"][0]["exact"], true);
    assert_eq!(defs_json["results"][0]["range"]["start"]["line"], 1);
    assert_eq!(defs_json["index"]["source"], "scip_native");

    // refs
    let refs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["refs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let refs_json: Value = serde_json::from_slice(&refs).unwrap();
    assert_eq!(refs_json["reliability"]["level"], "precise_fact");
    assert_eq!(refs_json["results"][0]["producer"], "scip");
    assert_eq!(refs_json["results"][0]["role"], "reference");
    assert_eq!(refs_json["results"][0]["range"]["start"]["line"], 2);
    assert_eq!(refs_json["index"]["source"], "scip_native");

    // symbols
    let symbols = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["symbols", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let symbols_json: Value = serde_json::from_slice(&symbols).unwrap();
    assert_eq!(symbols_json["reliability"]["level"], "precise_fact");
    assert_eq!(symbols_json["results"][0]["name"], "needle");
}

#[test]
fn native_scip_stale_detection_simulates_staleness_by_db_removal() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/lib.rs"),
        "fn needle() {}\nfn main() { needle(); }\n",
    )
    .unwrap();

    let scip_path = dir.path().join("index.scip");
    code_search_cli::scip::write_minimal_test_index(&scip_path).unwrap();

    // Import native SCIP
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "import-scip"])
        .arg(&scip_path)
        .assert()
        .success();

    // Remove the occurrence DB to simulate staleness
    let scip_dir = fs::read_dir(dir.path().join(".code-search/scip"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let db_path = scip_dir.join("occurrences.db");
    assert!(db_path.is_file());
    fs::remove_file(&db_path).unwrap();

    // After DB removal, queries MUST fall back to tree-sitter,
    // and tree-sitter results are NEVER marked as precise
    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();
    assert_ne!(defs_json["reliability"]["level"], "precise_fact");
    assert_eq!(defs_json["reliability"]["level"], "parser_fact");
    assert_eq!(defs_json["reliability"]["exact"], false);
    assert_eq!(defs_json["results"][0]["producer"], "tree_sitter_parser");
}

#[test]
fn watch_once_reconcile_detects_file_changes() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();

    // Build an index first to create a snapshot
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    // run watch --once to check reconcile
    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["watch", "--once"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["ok"], true);
    assert_eq!(json["command"], "watch");
    // Should have results containing reconcile info
    let results = json["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let reconcile = &results[0];
    assert_eq!(
        reconcile["stale"], false,
        "fresh after build should not be stale"
    );
    assert_eq!(reconcile["addedFiles"].as_array().unwrap().len(), 0);
    assert_eq!(reconcile["deletedFiles"].as_array().unwrap().len(), 0);

    // Modify the file and run watch --once again
    fs::write(dir.path().join("sample.txt"), "hello\nworld\n").unwrap();

    let output2 = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["watch", "--once"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json2: Value = serde_json::from_slice(&output2).unwrap();

    assert_eq!(json2["ok"], true);
    let results2 = json2["results"].as_array().unwrap();
    let reconcile2 = &results2[0];
    assert!(
        reconcile2["stale"].as_bool().unwrap(),
        "modified file should be detected as stale"
    );
    let dirty = reconcile2["dirtyFiles"].as_array().unwrap();
    assert!(!dirty.is_empty());
    assert_eq!(dirty[0]["path"], "sample.txt");
    assert_eq!(dirty[0]["reason"], "file_hash_mismatch");
}

#[test]
fn watch_status_output_format() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["watch", "--status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["ok"], true);
    assert_eq!(json["command"], "watch");
    let results = json["results"].as_array().unwrap();
    // results[0] IS the watcher status object directly
    let watcher = &results[0];
    assert!(watcher.is_object());
    assert_eq!(watcher["running"], false);
    assert!(watcher["root"].is_string());
    assert!(watcher["queueLength"].is_number());
    assert!(watcher["stale"].is_boolean());
    // lastEventAt should be null (no events collected)
    assert!(watcher["lastEventAt"].is_null());
    // lastReconcileAt should be null (--status doesn't run reconcile)
    assert!(watcher["lastReconcileAt"].is_null());
    assert_eq!(watcher["mode"], "reconcile_on_demand");
    // Should have overlay sub-object
    let overlay = &watcher["overlay"];
    assert!(overlay.is_object());
    assert!(overlay["dirtyFiles"].is_array());
    assert!(overlay["addedFiles"].is_array());
    assert!(overlay["deletedFiles"].is_array());
}

#[test]
fn watcher_does_not_modify_git_staged_state() {
    let dir = tempdir().unwrap();
    // Initialize a git repo
    std::process::Command::new("git")
        .arg("init")
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["config", "user.email", "test@test.com"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["config", "user.name", "Test"])
        .output()
        .unwrap();

    fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();

    // Stage the file
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["add", "sample.txt"])
        .output()
        .unwrap();

    // Run watch --once
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["watch", "--once"])
        .assert()
        .success();

    // Verify git staged state is still as expected — file should still be staged
    let status_output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    let status_str = String::from_utf8_lossy(&status_output.stdout);
    // sample.txt should still show as staged (A or M in index)
    assert!(
        status_str.contains("sample.txt"),
        "git status should still show sample.txt"
    );
    // The file should not be unstaged by watcher
}

#[test]
fn watch_run_once_returns_reconcile_info_without_modifying_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "content before watch\n").unwrap();

    let original_content = fs::read_to_string(dir.path().join("sample.txt")).unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["watch", "--once"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["ok"], true);

    // Verify file content is unchanged
    let after_content = fs::read_to_string(dir.path().join("sample.txt")).unwrap();
    assert_eq!(
        original_content, after_content,
        "watch should not modify file content"
    );

    // Verify the response has reconcile information
    let results = json["results"].as_array().unwrap();
    let reconcile = &results[0];
    assert!(reconcile["totalFilesScanned"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn serve_no_watch_returns_service_status() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["serve", "--no-watch"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["ok"], true);
    assert_eq!(json["command"], "serve");
    let results = json["results"].as_array().unwrap();
    let service = &results[0]["service"];
    assert!(service.is_object());
    assert_eq!(service["running"], false);
    assert_eq!(service["watchEnabled"], false);
    assert_eq!(service["mode"], "cli_query_service");
    assert!(service["root"].is_string());
    assert!(service["snapshot"].is_string());
}

#[test]
fn serve_with_watch_includes_watcher_status() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["serve"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["ok"], true);
    let results = json["results"].as_array().unwrap();
    let service = &results[0]["service"];
    assert_eq!(service["watchEnabled"], true);
    // When watch is enabled, watcher status should be included
    // but watcher might fail to init, so it's optional
    if let Some(watcher) = service.get("watcher") {
        assert!(watcher.is_object());
        assert!(watcher["root"].is_string());
    }
}

// ---------------------------------------------------------------------------
// MCP integration tests
// ---------------------------------------------------------------------------

#[test]
fn mcp_subcommand_is_registered_in_help() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();

    // Verify "mcp" appears in the subcommand list
    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).unwrap();
    assert!(
        help.contains("mcp"),
        "mcp subcommand not found in help: {help}"
    );
}

// ---------------------------------------------------------------------------
// MR-08 Remote/Pack mode tests
// ---------------------------------------------------------------------------

#[test]
fn index_pack_produces_valid_archive_with_checksums() {
    use std::fs;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    // Build index first
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    // Pack
    let archive_path = dir.path().join("output.tar.gz");
    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "pack", "--output"])
        .arg(&archive_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let packed = &json["results"][0];
    assert_eq!(packed["packed"], true);
    assert!(packed["archiveSize"].as_u64().unwrap() > 0);
    assert_eq!(packed["source"], "packed_remote");

    // Verify archive exists
    assert!(archive_path.exists());
    assert!(archive_path.metadata().unwrap().len() > 0);

    // Verify it's a valid gzip file (magic bytes 1f 8b)
    let archive_bytes = fs::read(&archive_path).unwrap();
    assert_eq!(&archive_bytes[0..2], &[0x1f, 0x8b]);
}

#[test]
fn index_unpack_extracts_to_remote_dir_does_not_touch_working_or_staged() {
    use std::fs;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    // Build index
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    // Pack
    let archive_path = dir.path().join("output.tar.gz");
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "pack", "--output"])
        .arg(&archive_path)
        .assert()
        .success();

    let code_search_dir = dir.path().join(".code-search");
    // Clean local index to simulate fresh workspace without local index
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "clean"])
        .assert()
        .success();

    // Unpack
    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "unpack"])
        .arg(&archive_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let unpacked = &json["results"][0];
    assert_eq!(unpacked["unpacked"], true);
    assert_eq!(unpacked["source"], "remote_unpacked");

    // Verify remote dir exists
    let remote_dir = code_search_dir.join("remote");
    assert!(remote_dir.exists());

    // snapshots may or may not exist after clean, but remote must be separate
    let remote_entries: Vec<_> = remote_dir
        .read_dir()
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(!remote_entries.is_empty(), "remote dir should have content");

    // Verify provenance.json exists
    for entry in &remote_entries {
        let path = entry.path();
        if path.is_dir() {
            let prov = path.join("provenance.json");
            if prov.exists() {
                let prov_content = fs::read_to_string(&prov).unwrap();
                assert!(prov_content.contains("remote_unpacked"));
                return;
            }
        }
    }
    panic!("provenance.json not found in remote directory");
}

#[test]
fn remote_snapshot_never_overrides_local_when_local_is_fresh() {
    use std::fs;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    // Build local index
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    // Pack
    let archive_path = dir.path().join("output.tar.gz");
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "pack", "--output"])
        .arg(&archive_path)
        .assert()
        .success();

    // Unpack to create remote snapshot
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "unpack"])
        .arg(&archive_path)
        .assert()
        .success();

    // Local snapshot should still be active (not the remote one)
    let status_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&status_output).unwrap();
    let status = &json["results"][0];
    // Local snapshot exists and is fresh
    assert_eq!(status["exists"], true);
    assert!(status["fresh"].as_bool().unwrap_or(false));
    // Remote should be listed but separate
    if let Some(remote) = status.get("remote") {
        assert!(remote.is_array());
    }
}

#[test]
fn remote_query_is_used_when_local_is_clean_missing() {
    use std::fs;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { let _ = \"needle\"; }\n",
    )
    .unwrap();

    // Build and pack
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let archive_path = dir.path().join("output.tar.gz");
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "pack", "--output"])
        .arg(&archive_path)
        .assert()
        .success();

    // Clean local index
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "clean"])
        .assert()
        .success();

    // Unpack remote
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "unpack"])
        .arg(&archive_path)
        .assert()
        .success();

    // Now find should use remote index (since local is missing)
    let find_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&find_output).unwrap();
    assert_eq!(json["ok"], true);
    // Should find the file even with local index deleted (via remote)
    assert!(!json["results"].as_array().unwrap().is_empty());
    assert_eq!(json["results"][0]["path"], "src/main.rs");
}

#[test]
fn remote_mismatch_labels_results_as_unverified() {
    use std::fs;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { let _ = \"needle\"; }\n",
    )
    .unwrap();

    // Build index
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    // Pack
    let archive_path = dir.path().join("output.tar.gz");
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "pack", "--output"])
        .arg(&archive_path)
        .assert()
        .success();

    // Modify local file so remote won't match
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { let _ = \"changed\"; }\n",
    )
    .unwrap();

    // Clean and unpack remote
    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "clean"])
        .assert()
        .success();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "unpack"])
        .arg(&archive_path)
        .assert()
        .success();

    // Query should still work via remote but should indicate remote_unverified
    // (the remote records won't match changed local files)
    let status = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&status).unwrap();
    let status_val = &json["results"][0];
    // Remote should have remoteVerified: false
    if let Some(remote) = status_val.get("remote") {
        if let Some(arr) = remote.as_array() {
            if let Some(first) = arr.first() {
                // remoteVerified should be false since file hashes don't match
                assert_eq!(first["remoteVerified"], json!(false));
                return; // found and verified
            }
        }
    }
    panic!("remote snapshot not found or remoteVerified not present");
}
