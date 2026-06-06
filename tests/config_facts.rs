use std::fs;

use codetrail::{
    config_facts::{
        extract_config_facts_for_file, extract_workspace_config_facts, facts_matching_key,
        ConfigFactCaveatCode, ConfigFactExtractOptions, ConfigFactKind,
    },
    project_graph::{discover_project_graph, DependencyEdgeKind},
    semantic_facts::FactReliability,
};
use tempfile::tempdir;

fn write(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn assert_parse_failure_fallback(path: &str, source: &str, forbidden: &str) {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        "{\"scripts\":{\"test\":\"node test.js\"}}\n",
    );
    write(&dir.path().join("src/app.ts"), "export const app = 1;\n");
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    assert_eq!(facts.len(), 1, "expected one source fallback for {path}");
    let fallback = &facts[0];
    assert_eq!(fallback.fact_kind, ConfigFactKind::SourceFactFallback);
    assert_eq!(fallback.reliability, FactReliability::SourceFact);
    assert!(fallback
        .caveats
        .iter()
        .any(|caveat| caveat.code == ConfigFactCaveatCode::ParseFailure));
    assert!(!serde_json::to_string(fallback).unwrap().contains(forbidden));
}

#[test]
fn extracts_structured_config_key_paths_and_supports_key_filtering() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    );
    write(&dir.path().join("src/lib.rs"), "pub fn demo() {}\n");
    write(
        &dir.path().join("config/app.yaml"),
        "database:\n  url: postgres://localhost/app\n  pool_size: 8\n",
    );
    write(
        &dir.path().join("config/web.json"),
        "{\"service\":{\"port\":8080}}\n",
    );
    write(
        &dir.path().join("config/runtime.toml"),
        "[server]\nhost = \"127.0.0.1\"\n",
    );

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_workspace_config_facts(dir.path(), &graph, ConfigFactExtractOptions::test())
            .unwrap();
    let matches = facts_matching_key(&facts, "database.url");

    assert_eq!(matches.len(), 1);
    let fact = matches[0];
    assert_eq!(fact.path, "config/app.yaml");
    assert_eq!(fact.fact_kind, ConfigFactKind::KeyValue);
    assert_eq!(fact.key_path.as_deref(), Some("database.url"));
    assert_eq!(
        fact.value_preview.as_deref(),
        Some("postgres://localhost/app")
    );
    assert!(!fact.preview_masked);
    assert_eq!(fact.reliability, FactReliability::ConfigFact);
    assert_eq!(fact.affected_root_ids, vec!["rust:."]);
    assert_eq!(
        fact.dependency_edge_kind,
        Some(DependencyEdgeKind::RuntimeConfigAffectsRoot)
    );
    assert_eq!(
        facts_matching_key(&facts, "service.port")[0]
            .value_preview
            .as_deref(),
        Some("8080")
    );
    assert_eq!(
        facts_matching_key(&facts, "server.host")[0]
            .value_preview
            .as_deref(),
        Some("127.0.0.1")
    );
}

#[test]
fn extracts_workflow_jobs_steps_and_script_blocks() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    );
    write(&dir.path().join("src/lib.rs"), "pub fn demo() {}\n");
    let path = ".github/workflows/ci.yml";
    let source = "name: ci\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - name: Test\n        run: cargo test --lib\n";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    let job = facts
        .iter()
        .find(|fact| fact.fact_kind == ConfigFactKind::WorkflowJob)
        .unwrap();
    assert_eq!(job.name.as_deref(), Some("build"));
    assert_eq!(job.affected_root_ids, vec!["rust:."]);

    let step = facts
        .iter()
        .find(|fact| fact.fact_kind == ConfigFactKind::WorkflowStep)
        .unwrap();
    assert_eq!(step.name.as_deref(), Some("actions/checkout@v4"));

    let script = facts
        .iter()
        .find(|fact| fact.fact_kind == ConfigFactKind::ScriptBlock)
        .unwrap();
    assert_eq!(script.value_preview.as_deref(), Some("cargo test --lib"));
}

#[test]
fn extracts_shell_entrypoints_functions_and_static_commands() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("go.mod"), "module example.com/api\n");
    write(&dir.path().join("main.go"), "package main\n");
    let path = "scripts/deploy.sh";
    let source = "#!/usr/bin/env bash\nset -euo pipefail\ndeploy() {\n  docker compose up --build\n}\ndeploy\n";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    assert!(facts.iter().any(|fact| {
        fact.fact_kind == ConfigFactKind::ScriptEntrypoint
            && fact.name.as_deref() == Some("/usr/bin/env bash")
    }));
    assert!(facts.iter().any(|fact| {
        fact.fact_kind == ConfigFactKind::ShellFunction && fact.name.as_deref() == Some("deploy")
    }));
    assert!(facts.iter().any(|fact| {
        fact.fact_kind == ConfigFactKind::CommandInvocation
            && fact.name.as_deref() == Some("docker")
            && fact.value_preview.as_deref() == Some("docker compose up --build")
    }));
}

#[test]
fn extracts_make_targets_docker_services_and_kubernetes_resources() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    );
    write(&dir.path().join("src/lib.rs"), "pub fn demo() {}\n");
    write(&dir.path().join("Makefile"), "test:\n\tcargo test --lib\n");
    write(
        &dir.path().join("Dockerfile"),
        "FROM rust:1\nENTRYPOINT [\"./demo\"]\n",
    );
    write(
        &dir.path().join("compose.yaml"),
        "services:\n  api:\n    image: example/api:latest\n",
    );
    write(
        &dir.path().join("k8s/deployment.yaml"),
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\n",
    );

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_workspace_config_facts(dir.path(), &graph, ConfigFactExtractOptions::test())
            .unwrap();

    assert!(facts.iter().any(|fact| {
        fact.path == "Makefile"
            && fact.fact_kind == ConfigFactKind::MakeTarget
            && fact.name.as_deref() == Some("test")
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == "Dockerfile"
            && fact.fact_kind == ConfigFactKind::DockerInstruction
            && fact.name.as_deref() == Some("FROM")
            && fact.dependency_edge_kind == Some(DependencyEdgeKind::EnvironmentAffectsRoot)
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == "compose.yaml"
            && fact.fact_kind == ConfigFactKind::DockerService
            && fact.name.as_deref() == Some("api")
            && fact.affected_root_ids == vec!["rust:."]
            && fact.dependency_edge_kind == Some(DependencyEdgeKind::EnvironmentAffectsRoot)
    }));
    assert!(facts.iter().any(|fact| {
        fact.path == "k8s/deployment.yaml"
            && fact.fact_kind == ConfigFactKind::KubernetesResource
            && fact.name.as_deref() == Some("Deployment/api")
    }));
}

#[test]
fn masks_secret_like_values_and_marks_machine_readable_caveat() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    );
    write(&dir.path().join("src/lib.rs"), "pub fn demo() {}\n");
    let path = "config/app.properties";
    let source = "api_key=not-a-real-test-secret\npublic_url=https://example.test\n";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    let secret = facts_matching_key(&facts, "api_key")[0];
    assert_eq!(secret.value_preview.as_deref(), Some("***MASKED***"));
    assert!(secret.preview_masked);
    assert!(secret
        .caveats
        .iter()
        .any(|caveat| caveat.code == ConfigFactCaveatCode::SecretMasked));
    assert!(!serde_json::to_string(secret)
        .unwrap()
        .contains("not-a-real-test-secret"));
}

#[test]
fn unresolved_edges_and_large_files_emit_machine_readable_caveats() {
    let dir = tempdir().unwrap();
    let path = "scripts/orphan.sh";
    let source = "#!/bin/sh\nprintf '%s\\n' hello\n";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts = extract_config_facts_for_file(
        path,
        source,
        &graph,
        ConfigFactExtractOptions { max_file_bytes: 12 },
    );

    assert!(facts.iter().all(|fact| fact.affected_root_ids.is_empty()));
    assert!(facts.iter().any(|fact| {
        fact.caveats
            .iter()
            .any(|caveat| caveat.code == ConfigFactCaveatCode::ConfigEdgeUnresolved)
    }));
    assert!(facts.iter().any(|fact| {
        fact.caveats
            .iter()
            .any(|caveat| caveat.code == ConfigFactCaveatCode::LargeFileTruncated)
    }));
}

#[test]
fn malformed_structured_config_yields_parse_failure_and_source_fact_fallback() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        "{\"scripts\":{\"test\":\"node test.js\"}}\n",
    );
    write(&dir.path().join("src/app.ts"), "export const app = 1;\n");
    let path = "config/bad.json";
    let source = "{\"database\": { \"url\": \"postgres://localhost/app\", }";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    assert!(facts.iter().any(|fact| {
        fact.fact_kind == ConfigFactKind::SourceFactFallback
            && fact.reliability == FactReliability::SourceFact
            && fact
                .caveats
                .iter()
                .any(|caveat| caveat.code == ConfigFactCaveatCode::ParseFailure)
    }));
    assert!(!facts
        .iter()
        .any(|fact| fact.key_path.as_deref() == Some("database.url")));
}

#[test]
fn malformed_yaml_yields_parse_failure_source_fact_fallback_without_secret_leak() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        "{\"scripts\":{\"test\":\"node test.js\"}}\n",
    );
    write(&dir.path().join("src/app.ts"), "export const app = 1;\n");
    let path = "config/bad.yaml";
    let source =
        "database:\n  url: postgres://localhost/app\n    api_key: not-a-real-test-secret\n";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    assert_eq!(facts.len(), 1);
    let fallback = &facts[0];
    assert_eq!(fallback.fact_kind, ConfigFactKind::SourceFactFallback);
    assert_eq!(fallback.reliability, FactReliability::SourceFact);
    assert!(fallback
        .caveats
        .iter()
        .any(|caveat| caveat.code == ConfigFactCaveatCode::ParseFailure));
    assert!(!serde_json::to_string(fallback)
        .unwrap()
        .contains("not-a-real-test-secret"));
}

#[test]
fn malformed_toml_yields_parse_failure_source_fact_fallback_without_secret_leak() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("package.json"),
        "{\"scripts\":{\"test\":\"node test.js\"}}\n",
    );
    write(&dir.path().join("src/app.ts"), "export const app = 1;\n");
    let path = "config/bad.toml";
    let source = "[server\napi_key = \"not-a-real-test-secret\"\n";
    write(&dir.path().join(path), source);

    let graph = discover_project_graph(dir.path()).unwrap();
    let facts =
        extract_config_facts_for_file(path, source, &graph, ConfigFactExtractOptions::test());

    assert_eq!(facts.len(), 1);
    let fallback = &facts[0];
    assert_eq!(fallback.fact_kind, ConfigFactKind::SourceFactFallback);
    assert_eq!(fallback.reliability, FactReliability::SourceFact);
    assert!(fallback
        .caveats
        .iter()
        .any(|caveat| caveat.code == ConfigFactCaveatCode::ParseFailure));
    assert!(!serde_json::to_string(fallback)
        .unwrap()
        .contains("not-a-real-test-secret"));
}

#[test]
fn real_yaml_parser_rejects_unterminated_secret_scalar_with_source_fallback() {
    assert_parse_failure_fallback(
        "config/bad-secret.yaml",
        "api_key: \"not-a-real-test-secret\n",
        "not-a-real-test-secret",
    );
}

#[test]
fn real_toml_parser_rejects_invalid_number_and_unclosed_array_with_source_fallback() {
    for (path, source) in [
        (
            "config/bad-number.toml",
            "port = 8080abc\napi_key = \"not-a-real-test-secret\"\n",
        ),
        (
            "config/bad-array.toml",
            "items = [1, 2\napi_key = \"not-a-real-test-secret\"\n",
        ),
    ] {
        assert_parse_failure_fallback(path, source, "not-a-real-test-secret");
    }
}
