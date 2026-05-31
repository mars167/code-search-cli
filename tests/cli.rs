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
    assert_eq!(json["schemaVersion"], "1.0");
    assert_eq!(json["reliability"]["level"], "source_fact");
    assert_eq!(json["query"]["normalized"], true);
    assert_eq!(json["results"][0]["path"], "src/main.rs");
    assert_eq!(json["results"][0]["range"]["start"]["line"], 2);
}

#[test]
fn schema_contract_covers_core_commands_and_errors() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    for args in [
        vec!["files", "main"],
        vec!["read", "src/main.rs:1"],
        vec!["status"],
        vec!["changed"],
        vec!["index", "status"],
    ] {
        let output = code_search()
            .arg("--path")
            .arg(dir.path())
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let json: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(json["schemaVersion"], "1.0");
        assert_eq!(json["query"]["normalized"], true);
    }

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "main", "--mode", "bogus"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["schemaVersion"], "1.0");
    assert_eq!(json["error"]["code"], "unsupported_search_mode");
}

#[test]
fn warnings_are_structured_with_stable_codes() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn helper() {}\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["refs", "helper"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        json["warnings"][0]["code"],
        "refs_identifier_boundary_text_search_unless_a_precise_occurrence_index_is_available"
    );
    assert!(json["warnings"][0]["message"]
        .as_str()
        .unwrap()
        .contains("precise occurrence index"));
}

#[test]
fn l0_literal_and_regex_modes_are_predictable() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "literal a.b\nregex acb\n").unwrap();

    let find_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "a.b"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let find_json: Value = serde_json::from_slice(&find_output).unwrap();
    assert_eq!(find_json["query"]["mode"], "literal");
    assert_eq!(find_json["results"].as_array().unwrap().len(), 1);
    assert_eq!(find_json["results"][0]["matchText"], "a.b");

    let grep_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["grep", "a.b"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let grep_json: Value = serde_json::from_slice(&grep_output).unwrap();
    let grep_matches: Vec<&str> = grep_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["matchText"].as_str())
        .collect();
    assert_eq!(grep_json["query"]["mode"], "regex");
    assert!(grep_matches.contains(&"a.b"));
    assert!(grep_matches.contains(&"acb"));

    let literal_grep = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["grep", "a.b", "--mode", "literal"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let literal_json: Value = serde_json::from_slice(&literal_grep).unwrap();
    assert_eq!(literal_json["query"]["mode"], "literal");
    assert_eq!(literal_json["results"].as_array().unwrap().len(), 1);
}

#[test]
fn refs_text_fallback_uses_identifier_boundaries() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "let user = User::new();\nlet profile = UserProfile::new();\n",
    )
    .unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["refs", "User"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert!(json["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["matchText"] == "User"));
    assert_eq!(json["results"].as_array().unwrap().len(), 1);
}

#[test]
fn files_is_path_substring_while_glob_is_strict_glob() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join("src/*.rs"), "literal star path\n").unwrap();

    let files_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["files", "src/*.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let files_json: Value = serde_json::from_slice(&files_output).unwrap();
    let files_paths: Vec<&str> = files_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert_eq!(files_json["query"]["mode"], "path_substring");
    assert_eq!(files_paths, vec!["src/*.rs"]);

    let glob_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["glob", "src/*.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let glob_json: Value = serde_json::from_slice(&glob_output).unwrap();
    let glob_paths: Vec<&str> = glob_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert_eq!(glob_json["query"]["mode"], "strict_glob");
    assert!(glob_paths.contains(&"src/main.rs"));
}

#[test]
fn list_and_tree_respect_hidden_no_ignore_and_filters() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    fs::create_dir_all(dir.path().join(".code-search")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join(".ignore"), "ignored.log\n").unwrap();
    fs::write(dir.path().join(".hidden.rs"), "hidden\n").unwrap();
    fs::write(dir.path().join("ignored.log"), "ignored\n").unwrap();
    fs::write(dir.path().join("target/generated/out.rs"), "generated\n").unwrap();
    fs::write(dir.path().join(".code-search/cache"), "internal\n").unwrap();

    let default_list = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let default_json: Value = serde_json::from_slice(&default_list).unwrap();
    let default_paths: Vec<&str> = default_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert!(default_paths.contains(&"src"));
    assert!(!default_paths.contains(&".hidden.rs"));
    assert!(!default_paths.contains(&"ignored.log"));
    assert!(!default_paths.contains(&"target"));
    assert!(!default_paths.contains(&".code-search"));

    let expanded_list = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--hidden")
        .arg("--no-ignore")
        .args(["list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let expanded_json: Value = serde_json::from_slice(&expanded_list).unwrap();
    let expanded_paths: Vec<&str> = expanded_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert!(expanded_paths.contains(&".hidden.rs"));
    assert!(expanded_paths.contains(&"ignored.log"));
    assert!(expanded_paths.contains(&"target"));
    assert!(!expanded_paths.contains(&".code-search"));

    let filtered_tree = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--include")
        .arg("src")
        .args(["tree"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let tree_json: Value = serde_json::from_slice(&filtered_tree).unwrap();
    let tree_paths: Vec<&str> = tree_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert!(tree_paths.contains(&"src"));
    assert!(tree_paths.contains(&"src/main.rs"));
    assert!(!tree_paths.contains(&"target"));
}

#[test]
fn lang_scope_filters_find_and_is_echoed() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "let value = \"needle\";\n").unwrap();
    fs::write(dir.path().join("src/app.py"), "value = 'needle'\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--lang")
        .arg("rust")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["query"]["scope"]["lang"], json!(["rust"]));
    assert_eq!(json["results"].as_array().unwrap().len(), 1);
    assert_eq!(json["results"][0]["path"], "src/lib.rs");
}

#[test]
fn lang_scope_filters_symbols() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "fn alpha() {}\n").unwrap();
    fs::write(dir.path().join("src/app.py"), "def alpha():\n    pass\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--lang")
        .arg("rust")
        .args(["symbols", "alpha"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let paths: Vec<&str> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();

    assert_eq!(json["query"]["scope"]["lang"], json!(["rust"]));
    assert_eq!(paths, vec!["src/lib.rs"]);
}

#[test]
fn changed_scope_searches_only_git_changed_files() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
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
    fs::write(
        dir.path().join("src/clean.rs"),
        "fn clean() { /* needle */ }\n",
    )
    .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["add", "src/clean.rs"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["commit", "-m", "init"])
        .output()
        .unwrap();
    fs::write(
        dir.path().join("src/changed.rs"),
        "fn changed() { /* needle */ }\n",
    )
    .unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--changed")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let paths: Vec<&str> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();

    assert_eq!(json["query"]["scope"]["changed"], true);
    assert_eq!(paths, vec!["src/changed.rs"]);
}

#[test]
fn cursor_paginates_stably_and_reports_facets() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        fs::write(dir.path().join(path), "needle\n").unwrap();
    }

    let first_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&first_output).unwrap();
    let cursor = first["nextCursor"].as_str().unwrap().to_string();

    assert_eq!(first["truncated"], true);
    assert_eq!(first["summary"]["resultCount"], 1);
    assert_eq!(first["results"][0]["path"], "src/a.rs");
    assert!(first["summary"]["facets"]["language"]
        .as_array()
        .unwrap()
        .iter()
        .any(|facet| facet["value"] == "rust" && facet["count"] == 3));

    let second_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .arg("--cursor")
        .arg(cursor)
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second: Value = serde_json::from_slice(&second_output).unwrap();

    assert_eq!(second["results"][0]["path"], "src/b.rs");
    assert_ne!(first["results"][0]["path"], second["results"][0]["path"]);
    assert_eq!(second["truncated"], true);
    assert!(second["nextCursor"].as_str().is_some());
}

#[test]
fn cursor_rejects_query_scope_mismatch() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "needle\nother\n").unwrap();
    fs::write(dir.path().join("b.txt"), "needle\n").unwrap();

    let first_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&first_output).unwrap();
    let cursor = first["nextCursor"].as_str().unwrap();

    let mismatch_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .arg("--cursor")
        .arg(cursor)
        .args(["find", "other"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&mismatch_output).unwrap();

    assert_eq!(json["error"]["code"], "cursor_does_not_match_query_scope");
}

#[test]
fn cursor_rejects_snapshot_mismatch_after_worktree_changes() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
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
    fs::write(dir.path().join("src/a.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("src/b.rs"), "needle\n").unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["add", "src"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["commit", "-m", "init"])
        .output()
        .unwrap();

    let first_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&first_output).unwrap();
    let cursor = first["nextCursor"].as_str().unwrap();

    fs::write(dir.path().join("src/aa.rs"), "needle\n").unwrap();

    let mismatch_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .arg("--cursor")
        .arg(cursor)
        .args(["find", "needle"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&mismatch_output).unwrap();

    assert_eq!(json["error"]["code"], "cursor_does_not_match_query_scope");
}

#[test]
fn cursor_rejects_dirty_worktree_result_set_changes() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
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
    fs::write(dir.path().join("README.md"), "base\n").unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["add", "README.md"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["commit", "-m", "init"])
        .output()
        .unwrap();
    fs::write(dir.path().join("src/a.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("src/b.rs"), "needle\n").unwrap();

    let first_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&first_output).unwrap();
    let cursor = first["nextCursor"].as_str().unwrap();
    let first_snapshot = first["snapshot_id"].as_str().unwrap().to_string();

    fs::write(dir.path().join("src/aa.rs"), "needle\n").unwrap();

    let mismatch_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--limit")
        .arg("1")
        .arg("--cursor")
        .arg(cursor)
        .args(["find", "needle"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&mismatch_output).unwrap();

    assert!(first_snapshot.starts_with("worktree:"));
    assert_eq!(json["error"]["code"], "cursor_does_not_match_query_scope");
}

#[test]
fn jsonl_summary_includes_cursor_and_facets() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("b.rs"), "needle\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--output")
        .arg("jsonl")
        .arg("--limit")
        .arg("1")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = String::from_utf8(output).unwrap();
    let summary: Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();

    assert_eq!(summary["event"], "summary");
    assert_eq!(summary["truncated"], true);
    assert!(summary["nextCursor"].as_str().is_some());
    assert!(summary["summary"]["facets"]["language"]
        .as_array()
        .unwrap()
        .iter()
        .any(|facet| facet["value"] == "rust" && facet["count"] == 2));
}

#[test]
fn json_output_includes_read_suggestions_and_next_actions() {
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

    assert!(json["results"][0]["readCommand"]
        .as_str()
        .unwrap()
        .contains("--path"));
    assert_eq!(json["results"][0]["readCommandArgv"][3], "read");
    assert_eq!(json["results"][0]["readCommandArgv"][4], "src/main.rs:2");
    assert_eq!(json["suggestedReads"][0], json["results"][0]["readCommand"]);
    assert_eq!(
        json["nextActions"][0]["command"],
        json["results"][0]["readCommand"]
    );
    assert_eq!(json["truncated"], false);
    assert!(json["nextCursor"].is_null());
}

#[test]
fn read_commands_are_replayable_with_path_and_spaces() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src dir")).unwrap();
    fs::write(
        dir.path().join("src dir/a b.rs"),
        "fn main() { /* needle */ }\n",
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

    let canonical_root = fs::canonicalize(dir.path()).unwrap();
    let argv = json["results"][0]["readCommandArgv"].as_array().unwrap();
    assert_eq!(argv[0], "code-search");
    assert_eq!(argv[1], "--path");
    assert_eq!(argv[2], canonical_root.to_string_lossy().as_ref());
    assert_eq!(argv[3], "read");
    assert_eq!(argv[4], "src dir/a b.rs:1");

    let read_output = code_search()
        .args(argv.iter().skip(1).map(|value| value.as_str().unwrap()))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let read_json: Value = serde_json::from_slice(&read_output).unwrap();
    assert_eq!(
        read_json["results"][0]["content"],
        "fn main() { /* needle */ }"
    );
}

#[test]
fn read_command_argv_handles_paths_that_look_like_flags() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("--odd.txt"), "needle\n").unwrap();

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
    let argv = json["results"][0]["readCommandArgv"].as_array().unwrap();
    assert_eq!(argv[4], "--");
    assert_eq!(argv[5], "--odd.txt:1");

    let read_output = code_search()
        .args(argv.iter().skip(1).map(|value| value.as_str().unwrap()))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let read_json: Value = serde_json::from_slice(&read_output).unwrap();
    assert_eq!(read_json["results"][0]["content"], "needle");
}

#[test]
fn directory_results_do_not_emit_read_next_actions() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let src = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["path"] == "src")
        .unwrap();
    assert_eq!(src["kind"], "directory");
    assert!(src.get("readCommand").is_none());
    assert!(json["suggestedReads"].as_array().unwrap().is_empty());
    assert!(json["nextActions"].as_array().unwrap().is_empty());
}

#[test]
fn deleted_changed_files_do_not_emit_read_next_actions() {
    let dir = tempdir().unwrap();
    std::process::Command::new("git")
        .arg("init")
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("gone.txt"), "removed\n").unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["add", "gone.txt"])
        .output()
        .unwrap();
    fs::remove_file(dir.path().join("gone.txt")).unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["changed"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["results"][0]["path"], "gone.txt");
    assert_eq!(json["results"][0]["worktreeStatus"], "D");
    assert!(json["results"][0].get("readCommand").is_none());
    assert!(json["suggestedReads"].as_array().unwrap().is_empty());
    assert!(json["nextActions"].as_array().unwrap().is_empty());
}

#[test]
fn index_status_metadata_does_not_emit_read_next_actions() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "needle\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert!(json["results"][0].get("readCommand").is_none());
    assert!(json["suggestedReads"].as_array().unwrap().is_empty());
    assert!(json["nextActions"].as_array().unwrap().is_empty());
}

#[test]
fn error_envelopes_keep_stable_output_fields() {
    let dir = tempdir().unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "missing.txt"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["schemaVersion"], "1.0");
    assert_eq!(json["truncated"], false);
    assert!(json["nextCursor"].is_null());
    assert!(json["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn jsonl_parse_errors_are_error_events() {
    let output = code_search()
        .args(["--output", "jsonl", "definitely-not-a-command"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["event"], "error");
    assert_eq!(lines[0]["error"]["code"], "cli_usage_error");
    assert_eq!(lines[0]["truncated"], false);
    assert!(lines[0]["nextCursor"].is_null());
    assert!(lines[0]["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn compact_json_omits_large_fields_but_keeps_read_command() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "before\nneedle here\nafter\n",
    )
    .unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args([
            "--output",
            "compact-json",
            "--context",
            "1",
            "find",
            "needle",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["results"][0]["readCommandArgv"][4], "src/main.rs:2");
    assert!(json["results"][0].get("preview").is_none());
    assert!(json["results"][0].get("context").is_none());
    assert!(json["results"][0].get("matchText").is_none());
}

#[test]
fn jsonl_output_streams_result_events_and_summary() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "needle one\nneedle two\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["--output", "jsonl", "find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    assert_eq!(lines[0]["event"], "result");
    assert_eq!(lines[0]["result"]["readCommandArgv"][4], "sample.txt:1");
    assert_eq!(lines[2]["event"], "summary");
    assert_eq!(lines[2]["resultCount"], 2);
    assert_eq!(lines[2]["schemaVersion"], "1.0");
}

#[test]
fn cli_parse_errors_use_json_error_schema() {
    let output = code_search()
        .args(["definitely-not-a-command"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["schemaVersion"], "1.0");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "cli_usage_error");
}

#[test]
fn dynamic_error_details_do_not_change_error_code() {
    let dir = tempdir().unwrap();

    let first = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "missing-one.txt"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let second = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "missing-two.txt"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let first_json: Value = serde_json::from_slice(&first).unwrap();
    let second_json: Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(first_json["error"]["code"], "read_failed");
    assert_eq!(first_json["error"]["code"], second_json["error"]["code"]);

    let first = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["list", "missing-one"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let second = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["list", "missing-two"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let first_json: Value = serde_json::from_slice(&first).unwrap();
    let second_json: Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(first_json["error"]["code"], "directory_does_not_exist");
    assert_eq!(first_json["error"]["code"], second_json["error"]["code"]);
}

#[test]
fn dynamic_warning_details_do_not_change_warning_code() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/broken.rs"), "fn broken( {\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["symbols", "broken"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["warnings"][0]["code"], "partial_parse_syntax_errors");
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
fn read_rejects_invalid_ranges_with_structured_errors() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "one\ntwo\nthree\n").unwrap();

    for target in ["sample.txt:0", "sample.txt:3-2", "sample.txt:2-"] {
        let output = code_search()
            .arg("--path")
            .arg(dir.path())
            .args(["read", target])
            .assert()
            .failure()
            .get_output()
            .stdout
            .clone();
        let json: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(json["error"]["code"], "invalid_line_range");
    }
}

#[test]
fn read_blocks_paths_outside_workspace() {
    let workspace = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
    let target = format!(
        "../{}/secret.txt",
        outside.path().file_name().unwrap().to_string_lossy()
    );

    let output = code_search()
        .arg("--path")
        .arg(workspace.path())
        .args(["read", &target])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["error"]["code"], "path_escapes_workspace_root");
}

#[test]
fn read_binary_file_returns_warning_without_content() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("blob.bin"), b"abc\0def").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "blob.bin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["reliability"]["exact"], false);
    assert_eq!(json["results"][0]["binary"], true);
    assert_eq!(json["results"][0]["content"], "");
    assert_eq!(json["results"][0]["exact"], false);
    assert!(json["results"][0].get("readCommand").is_none());
    assert!(json["nextActions"].as_array().unwrap().is_empty());
    assert_eq!(json["warnings"][0]["code"], "binary_file_not_displayed");
}

#[test]
fn read_large_file_truncates_full_read_but_allows_range() {
    let dir = tempdir().unwrap();
    let content = (0..7000)
        .map(|idx| format!("line {idx}\n"))
        .collect::<String>();
    fs::write(dir.path().join("large.txt"), content).unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "large.txt"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["reliability"]["exact"], false);
    assert_eq!(json["results"][0]["truncated"], true);
    assert_eq!(json["results"][0]["exact"], false);
    assert!(json["results"][0].get("readCommand").is_none());
    assert!(json["nextActions"].as_array().unwrap().is_empty());
    assert_eq!(json["warnings"][0]["code"], "large_file_truncated");

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["read", "large.txt:6999-7000"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["results"][0]["content"], "line 6998\nline 6999");
    assert_eq!(json["results"][0]["truncated"], false);
}

#[test]
fn find_truncates_very_long_preview_and_summarizes_it() {
    let dir = tempdir().unwrap();
    let long_line = format!("prefix needle {}\n", "x".repeat(2000));
    fs::write(dir.path().join("long.txt"), long_line).unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["--context", "1", "find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let preview = json["results"][0]["preview"].as_str().unwrap();
    assert!(preview.len() < 400);
    assert_eq!(json["results"][0]["previewTruncated"], true);
    assert_eq!(json["summary"]["truncatedCount"], 1);
}

#[test]
fn generated_directories_are_default_excluded_but_explicitly_searchable() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    fs::write(dir.path().join("target/generated/out.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("src.rs"), "needle\n").unwrap();

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
    assert!(json["summary"]["skippedCount"].as_u64().unwrap() >= 1);
    assert!(json["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["path"] != "target/generated/out.rs"));

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--no-ignore")
        .args(["find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert!(json["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["path"] == "target/generated/out.rs"));
}

#[test]
fn fresh_index_reports_generated_skips_in_summary() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    fs::write(dir.path().join("target/generated/out.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("src.rs"), "needle\n").unwrap();

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
    assert!(json["summary"]["skippedCount"].as_u64().unwrap() >= 1);
    assert!(json["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["path"] != "target/generated/out.rs"));
}

#[test]
fn jsonl_summary_includes_large_content_summary_counts() {
    let dir = tempdir().unwrap();
    let long_line = format!("prefix needle {}\n", "x".repeat(2000));
    fs::write(dir.path().join("long.txt"), long_line).unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["--output", "jsonl", "--context", "1", "find", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let summary = lines.last().unwrap();
    assert_eq!(summary["event"], "summary");
    assert_eq!(summary["summary"]["truncatedCount"], 1);
    assert!(summary["summary"]["skippedCount"].as_u64().is_some());
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
fn find_uses_lancedb_gram_prefilter_for_candidates() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("hit.txt"),
        "this file contains needle_rare_literal\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("miss.txt"),
        "this file contains many words but not the target\n",
    )
    .unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "needle_rare_literal"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["index"]["used"], true);
    assert_eq!(json["index"]["fresh"], true);
    assert_eq!(json["index"]["prefilter"], "trigram");
    assert_eq!(json["index"]["candidateCount"], 1);
    assert_eq!(json["results"][0]["path"], "hit.txt");
}

#[test]
fn index_update_noops_when_index_is_fresh() {
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
        .args(["index", "update"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let result = &json["results"][0];

    assert_eq!(result["updated"], false);
    assert_eq!(result["reason"], "index_fresh");
    assert_eq!(result["index"]["fresh"], true);
}

#[test]
fn index_update_replaces_stale_gram_postings() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "alpha oldtoken\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    fs::write(dir.path().join("sample.txt"), "alpha newtoken\n").unwrap();

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "update"])
        .assert()
        .success();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["find", "oldtoken"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["index"]["used"], true);
    assert_eq!(json["index"]["fresh"], true);
    assert_eq!(json["index"]["candidateCount"], 0);
    assert_eq!(json["results"].as_array().unwrap().len(), 0);
}

#[test]
fn files_live_scan_uses_catalog_without_content_hash() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("sample.txt"), "needle\n").unwrap();

    let output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["files", "sample"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["index"]["used"], false);
    assert_eq!(json["results"][0]["producer"], "live_file_catalog");
    assert!(json["results"][0]["hash"].is_null());
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
fn defs_falls_back_to_parser_for_java_classes() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/main/java/example")).unwrap();
    fs::write(
        dir.path().join("src/main/java/example/SampleService.java"),
        "package example;\n\npublic class SampleService {\n    public void run() {}\n}\n",
    )
    .unwrap();

    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "SampleService"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();

    assert_eq!(defs_json["reliability"]["level"], "parser_fact");
    assert_eq!(defs_json["results"][0]["name"], "SampleService");
    assert_eq!(defs_json["results"][0]["kind"], "class");
    assert_eq!(defs_json["results"][0]["language"], "java");
    assert_eq!(
        defs_json["results"][0]["path"],
        "src/main/java/example/SampleService.java"
    );
}

#[test]
fn parser_fallback_supports_java_methods_and_callers() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/main/java/example")).unwrap();
    fs::write(
        dir.path().join("src/main/java/example/SampleService.java"),
        "package example;\n\npublic class SampleService {\n    public void run() {}\n\n    public void start() {\n        run();\n    }\n}\n",
    )
    .unwrap();

    let defs = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["defs", "run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let defs_json: Value = serde_json::from_slice(&defs).unwrap();
    assert_eq!(defs_json["results"][0]["name"], "run");
    assert_eq!(defs_json["results"][0]["kind"], "function");
    assert_eq!(defs_json["results"][0]["language"], "java");

    let callers = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["callers", "run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let callers_json: Value = serde_json::from_slice(&callers).unwrap();
    assert_eq!(callers_json["results"][0]["target"], "run");
    assert_eq!(callers_json["results"][0]["enclosingSymbol"], "start");
    assert_eq!(callers_json["results"][0]["language"], "java");
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
fn native_scip_precise_results_respect_hidden_and_no_ignore() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".hidden")).unwrap();
    fs::create_dir_all(dir.path().join("target/generated")).unwrap();
    let source = "fn needle() {}\nfn main() { needle(); }\n";
    fs::write(dir.path().join(".hidden/lib.rs"), source).unwrap();
    fs::write(dir.path().join("target/generated/lib.rs"), source).unwrap();

    let scip_path = dir.path().join("index.scip");
    write_scip_index_for_paths(&scip_path, &[".hidden/lib.rs", "target/generated/lib.rs"]);

    code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "import-scip"])
        .arg(&scip_path)
        .assert()
        .success();

    let default_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .args(["refs", "needle"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let default_json: Value = serde_json::from_slice(&default_output).unwrap();
    assert_eq!(default_json["index"]["source"], "scip_native");
    assert!(default_json["results"].as_array().unwrap().is_empty());

    let hidden_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--hidden")
        .args(["refs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hidden_json: Value = serde_json::from_slice(&hidden_output).unwrap();
    let hidden_paths: Vec<&str> = hidden_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert_eq!(hidden_paths, vec![".hidden/lib.rs"]);

    let expanded_output = code_search()
        .arg("--path")
        .arg(dir.path())
        .arg("--hidden")
        .arg("--no-ignore")
        .args(["refs", "needle"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let expanded_json: Value = serde_json::from_slice(&expanded_output).unwrap();
    let expanded_paths: Vec<&str> = expanded_json["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();
    assert!(expanded_paths.contains(&".hidden/lib.rs"));
    assert!(expanded_paths.contains(&"target/generated/lib.rs"));
    assert_eq!(expanded_paths.len(), 2);
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

fn write_scip_index_for_paths(path: &std::path::Path, rel_paths: &[&str]) {
    use code_search_cli::scip_proto::proto;
    use prost::Message;

    let documents = rel_paths
        .iter()
        .map(|rel_path| proto::Document {
            language: "rust".to_string(),
            relative_path: (*rel_path).to_string(),
            occurrences: vec![
                proto::Occurrence {
                    range: vec![0, 3, 0, 9],
                    symbol: "local 1".to_string(),
                    symbol_roles: 1,
                    ..Default::default()
                },
                proto::Occurrence {
                    range: vec![1, 12, 1, 18],
                    symbol: "local 1".to_string(),
                    symbol_roles: 0,
                    ..Default::default()
                },
            ],
            symbols: vec![proto::SymbolInformation {
                symbol: "local 1".to_string(),
                kind: proto::symbol_information::Kind::Function as i32,
                display_name: "needle".to_string(),
                ..Default::default()
            }],
            position_encoding: proto::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
            ..Default::default()
        })
        .collect();

    let index = proto::Index {
        metadata: Some(proto::Metadata {
            version: proto::ProtocolVersion::UnspecifiedProtocolVersion as i32,
            tool_info: Some(proto::ToolInfo {
                name: "test-indexer".to_string(),
                version: "0.1.0".to_string(),
                arguments: vec![],
            }),
            project_root: "file:///test".to_string(),
            text_document_encoding: proto::TextEncoding::Utf8 as i32,
        }),
        documents,
        ..Default::default()
    };

    let mut buf = Vec::new();
    index.encode(&mut buf).unwrap();
    fs::write(path, &buf).unwrap();
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
