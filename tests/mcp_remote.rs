use std::fs;

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::tempdir;

fn codetrail_json() -> Command {
    let mut command = Command::cargo_bin("codetrail").expect("binary exists");
    command
        .env("CODETRAIL_INTERNAL_JSON", "1")
        .arg("--output")
        .arg("json");
    command
}

fn codetrail_raw() -> Command {
    Command::cargo_bin("codetrail").expect("binary exists")
}

fn parse_json(output: Vec<u8>) -> Value {
    serde_json::from_slice(&output).unwrap()
}

fn mcp_call(root: &std::path::Path, tool: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments
        }
    });
    let output = codetrail_raw()
        .arg("--path")
        .arg(root)
        .arg("mcp")
        .write_stdin(format!("{request}\n"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&output).unwrap();
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn mcp_find_can_query_selected_remote_snapshot_without_local_source() {
    // Given: a remote-unpacked snapshot containing source text, and the local
    // source file is no longer readable from the workspace.
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    let marker = \"remote-needle\";\n}\n",
    )
    .unwrap();

    codetrail_json()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "build"])
        .assert()
        .success();

    let archive_path = dir.path().join("remote.tar.gz");
    codetrail_json()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "pack", "--output"])
        .arg(&archive_path)
        .assert()
        .success();

    codetrail_json()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "clean"])
        .assert()
        .success();

    let unpack_output = codetrail_json()
        .arg("--path")
        .arg(dir.path())
        .args(["index", "unpack"])
        .arg(&archive_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let unpacked = parse_json(unpack_output);
    let remote_snapshot = unpacked["results"][0]["remote_snapshot_key"]
        .as_str()
        .unwrap();

    fs::remove_file(dir.path().join("src/main.rs")).unwrap();

    // When: MCP find is explicitly scoped to the selected remote snapshot.
    let result = mcp_call(
        dir.path(),
        "codetrail_find",
        json!({
            "text": "remote-needle",
            "remoteMode": "only",
            "remoteSnapshot": remote_snapshot,
            "allowBroad": true
        }),
    );

    // Then: the result comes from the remote snapshot and is caveated because
    // it cannot be verified against local source files.
    assert_eq!(result["results"][0]["path"], "src/main.rs");
    assert!(result["results"][0]["preview"]
        .as_str()
        .unwrap()
        .contains("remote-needle"));
    assert!(result["caveats"].as_array().unwrap().iter().any(|caveat| {
        caveat["code"] == "remote_only"
            && caveat["message"]
                .as_str()
                .unwrap()
                .contains(remote_snapshot)
    }));
    assert!(result["caveats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|caveat| { caveat["code"] == "remote_unverified" }));
}
