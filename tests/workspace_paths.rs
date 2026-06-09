use std::fs;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::tempdir;

fn codetrail() -> Command {
    let mut command = Command::cargo_bin("codetrail").expect("binary exists");
    command
        .env("CODETRAIL_INTERNAL_JSON", "1")
        .arg("--output")
        .arg("json");
    command
}

fn init_git_repo(path: &std::path::Path) {
    std::process::Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "user.name", "Test"])
        .output()
        .unwrap();
}

#[test]
fn tree_dot_in_git_workspace_uses_one_canonical_path_form() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let output = codetrail()
        .arg("--path")
        .arg(dir.path())
        .args(["tree", "."])
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

    assert!(paths.contains(&"src"));
    assert!(paths.contains(&"src/main.rs"));
}
