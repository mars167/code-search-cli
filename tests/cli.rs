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
fn index_build_writes_target_text_storage_layout() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "needle\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let code_search_dir = dir.path().join(".code-search");
    assert!(code_search_dir.join("snapshots").is_dir());
    assert!(code_search_dir.join("text").is_dir());
    assert!(code_search_dir
        .join("working")
        .join("manifest.json")
        .is_file());

    let snapshot = fs::read_dir(code_search_dir.join("snapshots"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(snapshot.join("manifest.json").is_file());
    assert!(snapshot.join("files.parquet").is_file());
    assert!(snapshot.join("blobs").is_dir());
    let text_snapshot = fs::read_dir(code_search_dir.join("text"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(text_snapshot.join("docs.idx").is_file());
    assert!(text_snapshot.join("paths.idx").is_file());
    assert!(text_snapshot.join("grams.idx").is_file());
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

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "import-scip"])
        .arg(&scip_path)
        .assert()
        .success();

    let scip_snapshot = fs::read_dir(dir.path().join(".code-search/scip"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(scip_snapshot.join("occurrences.idx").is_file());
    assert!(!scip_snapshot.join("occurrences.jsonl").exists());

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
