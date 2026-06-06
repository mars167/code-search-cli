use std::fs;

use codetrail::project_graph::{
    discover_project_graph, ConfigEdgeKind, DependencyEdgeKind, EnvironmentEdgeKind,
    ProjectGraphCaveatCode, ProjectRootKind, SemanticFactPolicy,
};
use tempfile::tempdir;

fn write(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn discovers_single_language_go_root_and_owned_sources() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("go.mod"), "module example.com/api\n");
    write(&dir.path().join("cmd/api/main.go"), "package main\n");

    let graph = discover_project_graph(dir.path()).unwrap();

    assert_eq!(graph.roots.len(), 1);
    assert_eq!(graph.roots[0].id, "go:.");
    assert_eq!(graph.roots[0].kind, ProjectRootKind::GoModule);
    assert_eq!(graph.source_owners.len(), 1);
    assert_eq!(graph.source_owners[0].path, "cmd/api/main.go");
    assert_eq!(graph.source_owners[0].root_id, "go:.");
    assert_eq!(
        graph.source_owners[0].semantic_fact_policy,
        SemanticFactPolicy::PreciseEligible
    );
}

#[test]
fn discovers_polyglot_multi_root_workspace_with_stable_ids() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("services/api/go.mod"), "module api\n");
    write(&dir.path().join("services/api/main.go"), "package main\n");
    write(
        &dir.path().join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\n",
    );
    write(
        &dir.path().join("crates/core/src/lib.rs"),
        "pub fn core() {}\n",
    );
    write(&dir.path().join("web/package.json"), "{}\n");
    write(&dir.path().join("web/tsconfig.json"), "{}\n");
    write(
        &dir.path().join("web/src/app.ts"),
        "export const app = 1;\n",
    );
    write(&dir.path().join("java/pom.xml"), "<project />\n");
    write(
        &dir.path().join("java/src/main/java/App.java"),
        "class App {}\n",
    );

    let graph = discover_project_graph(dir.path()).unwrap();
    let root_ids = graph
        .roots
        .iter()
        .map(|root| (root.id.as_str(), root.kind.clone()))
        .collect::<Vec<_>>();

    assert_eq!(
        root_ids,
        vec![
            ("rust:crates/core", ProjectRootKind::RustCargo),
            ("java:java", ProjectRootKind::JavaMaven),
            ("go:services/api", ProjectRootKind::GoModule),
            ("typescript:web", ProjectRootKind::TypeScriptConfig),
        ]
    );
    assert_eq!(
        graph
            .source_owners
            .iter()
            .map(|owner| (owner.path.as_str(), owner.root_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("crates/core/src/lib.rs", "rust:crates/core"),
            ("java/src/main/java/App.java", "java:java"),
            ("services/api/main.go", "go:services/api"),
            ("web/src/app.ts", "typescript:web"),
        ]
    );
}

#[test]
fn maps_root_and_shared_config_edges_to_affected_roots() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("api/go.mod"), "module api\n");
    write(&dir.path().join("api/main.go"), "package main\n");
    write(&dir.path().join("ui/package.json"), "{}\n");
    write(&dir.path().join("ui/src/app.ts"), "export const app = 1;\n");
    write(&dir.path().join("Makefile"), "test:\n\ttrue\n");
    write(&dir.path().join(".github/workflows/ci.yml"), "name: ci\n");
    write(&dir.path().join("api/config/app.yaml"), "debug: false\n");

    let graph = discover_project_graph(dir.path()).unwrap();

    let go_mod = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "api/go.mod")
        .unwrap();
    assert_eq!(go_mod.kind, ConfigEdgeKind::BuildConfig);
    assert_eq!(go_mod.affected_root_ids, vec!["go:api"]);

    for path in ["Makefile", ".github/workflows/ci.yml"] {
        let edge = graph
            .config_edges
            .iter()
            .find(|edge| edge.path == path)
            .unwrap();
        assert_eq!(edge.kind, ConfigEdgeKind::AutomationScript);
        assert_eq!(edge.affected_root_ids, vec!["go:api", "typescript:ui"]);
    }

    let runtime = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "api/config/app.yaml")
        .unwrap();
    assert_eq!(runtime.kind, ConfigEdgeKind::RuntimeConfig);
    assert_eq!(runtime.affected_root_ids, vec!["go:api"]);
}

#[test]
fn same_directory_polyglot_build_configs_keep_language_specific_owners() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("api/go.mod"), "module api\n");
    write(
        &dir.path().join("api/go.sum"),
        "example.com/lib v1.0.0 h1:abc\n",
    );
    write(&dir.path().join("api/main.go"), "package main\n");
    write(&dir.path().join("api/package.json"), "{}\n");
    write(&dir.path().join("api/package-lock.json"), "{}\n");
    write(
        &dir.path().join("api/src/app.ts"),
        "export const app = 1;\n",
    );

    let graph = discover_project_graph(dir.path()).unwrap();

    let go_mod = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "api/go.mod")
        .unwrap();
    assert_eq!(go_mod.kind, ConfigEdgeKind::BuildConfig);
    assert_eq!(go_mod.owner_root_id.as_deref(), Some("go:api"));
    assert_eq!(go_mod.affected_root_ids, vec!["go:api"]);

    let package_json = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "api/package.json")
        .unwrap();
    assert_eq!(package_json.kind, ConfigEdgeKind::BuildConfig);
    assert_eq!(
        package_json.owner_root_id.as_deref(),
        Some("typescript:api")
    );
    assert_eq!(package_json.affected_root_ids, vec!["typescript:api"]);

    let go_sum = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "api/go.sum")
        .unwrap();
    assert_eq!(go_sum.kind, ConfigEdgeKind::DependencyConfig);
    assert_eq!(go_sum.owner_root_id.as_deref(), Some("go:api"));
    assert_eq!(go_sum.affected_root_ids, vec!["go:api"]);

    let package_lock = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "api/package-lock.json")
        .unwrap();
    assert_eq!(package_lock.kind, ConfigEdgeKind::DependencyConfig);
    assert_eq!(
        package_lock.owner_root_id.as_deref(),
        Some("typescript:api")
    );
    assert_eq!(package_lock.affected_root_ids, vec!["typescript:api"]);

    assert!(graph
        .dependency_edges
        .iter()
        .any(|edge| edge.via_path.as_deref() == Some("api/go.mod")
            && edge.from_root_id.as_deref() == Some("go:api")
            && edge.to_root_id.as_deref() == Some("go:api")));
    assert!(graph
        .dependency_edges
        .iter()
        .any(|edge| edge.via_path.as_deref() == Some("api/package.json")
            && edge.from_root_id.as_deref() == Some("typescript:api")
            && edge.to_root_id.as_deref() == Some("typescript:api")));
}

#[test]
fn shared_config_and_environment_files_affect_multiple_roots() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("api/go.mod"), "module api\n");
    write(&dir.path().join("api/main.go"), "package main\n");
    write(
        &dir.path().join("worker/Cargo.toml"),
        "[package]\nname = \"worker\"\n",
    );
    write(
        &dir.path().join("worker/src/lib.rs"),
        "pub fn worker() {}\n",
    );
    write(&dir.path().join("package-lock.json"), "{}\n");
    write(&dir.path().join("config/app.yaml"), "env: test\n");
    write(&dir.path().join("compose.yaml"), "services: {}\n");
    write(
        &dir.path().join("k8s/deployment.yaml"),
        "apiVersion: apps/v1\n",
    );

    let graph = discover_project_graph(dir.path()).unwrap();

    for path in ["package-lock.json", "config/app.yaml"] {
        let edge = graph
            .config_edges
            .iter()
            .find(|edge| edge.path == path)
            .unwrap();
        assert_eq!(edge.affected_root_ids, vec!["go:api", "rust:worker"]);
        assert_eq!(edge.unresolved, false);
    }

    let compose = graph
        .environment_edges
        .iter()
        .find(|edge| edge.path == "compose.yaml")
        .unwrap();
    assert_eq!(compose.kind, EnvironmentEdgeKind::Compose);
    assert_eq!(compose.affected_root_ids, vec!["go:api", "rust:worker"]);
    assert_eq!(compose.unresolved, false);

    let k8s = graph
        .environment_edges
        .iter()
        .find(|edge| edge.path == "k8s/deployment.yaml")
        .unwrap();
    assert_eq!(k8s.kind, EnvironmentEdgeKind::Kubernetes);
    assert_eq!(k8s.affected_root_ids, vec!["go:api", "rust:worker"]);
}

#[test]
fn exposes_dependency_edges_for_config_environment_and_root_relationships() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("go.work"), "use ./api\n");
    write(&dir.path().join("api/go.mod"), "module api\n");
    write(&dir.path().join("api/main.go"), "package main\n");
    write(
        &dir.path().join("worker/Cargo.toml"),
        "[package]\nname = \"worker\"\n",
    );
    write(
        &dir.path().join("worker/src/lib.rs"),
        "pub fn worker() {}\n",
    );
    write(&dir.path().join("package-lock.json"), "{}\n");
    write(&dir.path().join("compose.yaml"), "services: {}\n");

    let graph = discover_project_graph(dir.path()).unwrap();

    let lock_edges = graph
        .dependency_edges
        .iter()
        .filter(|edge| edge.via_path.as_deref() == Some("package-lock.json"))
        .collect::<Vec<_>>();
    assert_eq!(lock_edges.len(), 3);
    assert!(lock_edges.iter().all(|edge| edge.kind
        == DependencyEdgeKind::DependencyConfigAffectsRoot
        && edge.from_root_id.is_none()
        && edge.to_root_id.is_some()
        && edge.unresolved == false));

    let compose_edges = graph
        .dependency_edges
        .iter()
        .filter(|edge| edge.via_path.as_deref() == Some("compose.yaml"))
        .collect::<Vec<_>>();
    assert_eq!(compose_edges.len(), 3);
    assert!(compose_edges
        .iter()
        .all(|edge| edge.kind == DependencyEdgeKind::EnvironmentAffectsRoot));

    assert!(graph.dependency_edges.iter().any(|edge| edge.kind
        == DependencyEdgeKind::RootDependsOnRoot
        && edge.from_root_id.as_deref() == Some("go:.")
        && edge.to_root_id.as_deref() == Some("go:api")
        && edge.via_path.as_deref() == Some("go.work")));
}

#[test]
fn records_generated_sources_as_source_facts_not_precise_semantic_facts() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    );
    write(&dir.path().join("src/lib.rs"), "pub fn handwritten() {}\n");
    write(
        &dir.path().join("src/generated/schema.rs"),
        "pub fn generated() {}\n",
    );

    let graph = discover_project_graph(dir.path()).unwrap();

    assert_eq!(graph.generated_sources.len(), 1);
    assert_eq!(graph.generated_sources[0].path, "src/generated/schema.rs");
    assert_eq!(graph.generated_sources[0].owner_root_id, "rust:.");
    assert_eq!(
        graph.generated_sources[0].semantic_fact_policy,
        SemanticFactPolicy::SourceOrConfigFactOnly
    );
    assert!(!graph
        .source_owners
        .iter()
        .any(|owner| owner.path == "src/generated/schema.rs"));
}

#[test]
fn orphan_files_get_no_semantic_owner_and_emit_unresolved_caveats() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("scripts/deploy.sh"), "#!/bin/sh\ntrue\n");
    write(&dir.path().join("notes.txt"), "loose file\n");

    let graph = discover_project_graph(dir.path()).unwrap();

    assert!(graph.roots.is_empty());
    assert!(graph.source_owners.is_empty());
    let script_edge = graph
        .config_edges
        .iter()
        .find(|edge| edge.path == "scripts/deploy.sh")
        .unwrap();
    assert_eq!(script_edge.kind, ConfigEdgeKind::AutomationScript);
    assert!(script_edge.affected_root_ids.is_empty());
    assert_eq!(script_edge.unresolved, true);
    assert!(graph.caveats.iter().any(|caveat| caveat.code
        == ProjectGraphCaveatCode::ConfigEdgeUnresolved
        && caveat.path == "scripts/deploy.sh"));
}

#[test]
fn serializes_machine_readable_schema_snapshot() {
    let dir = tempdir().unwrap();
    write(&dir.path().join("go.mod"), "module example.com/api\n");
    write(&dir.path().join("main.go"), "package main\n");
    write(&dir.path().join("Dockerfile"), "FROM scratch\n");

    let graph = discover_project_graph(dir.path()).unwrap();
    let json = serde_json::to_value(&graph).unwrap();

    assert_eq!(json["schemaVersion"], 1);
    assert_eq!(json["roots"][0]["id"], "go:.");
    assert_eq!(json["roots"][0]["language"], "go");
    assert_eq!(
        json["sourceOwners"][0]["semanticFactPolicy"],
        "precise_eligible"
    );
    assert_eq!(json["configEdges"][0]["edgeSchema"], "config_edge/v1");
    assert_eq!(
        json["dependencyEdges"][0]["edgeSchema"],
        "dependency_edge/v1"
    );
}
