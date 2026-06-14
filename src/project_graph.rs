//! Internal polyglot project graph schema and root/config discovery.
//!
//! The graph answers three questions needed by later code-intelligence work:
//! which project roots exist in a workspace, which source files have exactly one
//! semantic owner, and which config or automation files can invalidate which
//! roots. It is deliberately internal and does not change the public CLI
//! contract.
//!
//! Schema constraints:
//! - `ProjectRoot` is a language root discovered from build/package markers.
//!   Stable root ids are `language:relative/root/path`; the workspace root is
//!   represented as `.`.
//! - `SourceOwner` records handwritten source files that are eligible for
//!   precise semantic facts. A source file may appear in at most one
//!   `SourceOwner`.
//! - `GeneratedSource` records generated source files under a root. Generated
//!   files are source facts only and are intentionally excluded from precise
//!   semantic facts.
//! - `ConfigEdge` records build config, runtime config, automation script, and
//!   dependency config files. A config/script may affect multiple roots.
//! - `EnvironmentEdge` records shared environment-sensitive files such as
//!   Docker, compose, Kubernetes, and dotenv files.
//! - `DependencyEdge` is the machine-readable invalidation/import edge layer.
//!   It records config/environment-to-root influence and root-to-root
//!   relationships without requiring semantic providers to parse manifests yet.
//! - Unmapped config/script files keep an empty `affectedRootIds` list,
//!   `unresolved = true`, and a `config_edge_unresolved` caveat so freshness
//!   consumers can make a machine-readable conservative choice.
//! - `--lang`-style filters must be applied by callers as scope constraints
//!   over this graph; they are not workspace language assertions.
//! - Files that do not enter precise semantic facts, including generated
//!   sources, shared CI/Docker/Makefile/shell, runtime configs, dependency
//!   manifests, and orphan files, remain representable as source/config facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGraph {
    pub schema_version: u32,
    pub roots: Vec<ProjectRoot>,
    pub source_owners: Vec<SourceOwner>,
    pub generated_sources: Vec<GeneratedSource>,
    pub config_edges: Vec<ConfigEdge>,
    pub environment_edges: Vec<EnvironmentEdge>,
    pub dependency_edges: Vec<DependencyEdge>,
    pub caveats: Vec<ProjectGraphCaveat>,
}

impl ProjectGraph {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLanguage {
    Go,
    Rust,
    Java,
    TypeScript,
}

impl ProjectLanguage {
    fn source_extensions(&self) -> &'static [&'static str] {
        match self {
            ProjectLanguage::Go => &["go"],
            ProjectLanguage::Rust => &["rs"],
            ProjectLanguage::Java => &["java"],
            ProjectLanguage::TypeScript => &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        }
    }
}

impl fmt::Display for ProjectLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectLanguage::Go => write!(f, "go"),
            ProjectLanguage::Rust => write!(f, "rust"),
            ProjectLanguage::Java => write!(f, "java"),
            ProjectLanguage::TypeScript => write!(f, "typescript"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRootKind {
    GoModule,
    GoWorkspace,
    RustCargo,
    JavaMaven,
    JavaGradle,
    TypeScriptConfig,
    TypeScriptPackage,
}

impl ProjectRootKind {
    fn language(&self) -> ProjectLanguage {
        match self {
            ProjectRootKind::GoModule | ProjectRootKind::GoWorkspace => ProjectLanguage::Go,
            ProjectRootKind::RustCargo => ProjectLanguage::Rust,
            ProjectRootKind::JavaMaven | ProjectRootKind::JavaGradle => ProjectLanguage::Java,
            ProjectRootKind::TypeScriptConfig | ProjectRootKind::TypeScriptPackage => {
                ProjectLanguage::TypeScript
            }
        }
    }

    fn priority(&self) -> u8 {
        match self {
            ProjectRootKind::GoWorkspace => 0,
            ProjectRootKind::GoModule => 1,
            ProjectRootKind::RustCargo => 0,
            ProjectRootKind::JavaMaven => 0,
            ProjectRootKind::JavaGradle => 1,
            ProjectRootKind::TypeScriptConfig => 0,
            ProjectRootKind::TypeScriptPackage => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoot {
    pub id: String,
    pub path: String,
    pub language: ProjectLanguage,
    pub kind: ProjectRootKind,
    pub markers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceOwner {
    pub path: String,
    pub root_id: String,
    pub language: ProjectLanguage,
    pub semantic_fact_policy: SemanticFactPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedSource {
    pub path: String,
    pub owner_root_id: String,
    pub language: ProjectLanguage,
    pub semantic_fact_policy: SemanticFactPolicy,
    pub reason: GeneratedSourceReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedSourceReason {
    GeneratedPath,
    GeneratedFilename,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticFactPolicy {
    PreciseEligible,
    SourceOrConfigFactOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEdge {
    pub edge_schema: String,
    pub path: String,
    pub kind: ConfigEdgeKind,
    pub owner_root_id: Option<String>,
    pub affected_root_ids: Vec<String>,
    pub unresolved: bool,
    pub semantic_fact_policy: SemanticFactPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEdgeKind {
    BuildConfig,
    RuntimeConfig,
    AutomationScript,
    DependencyConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentEdge {
    pub edge_schema: String,
    pub path: String,
    pub kind: EnvironmentEdgeKind,
    pub affected_root_ids: Vec<String>,
    pub unresolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentEdgeKind {
    Container,
    Compose,
    Kubernetes,
    DotEnv,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyEdge {
    pub edge_schema: String,
    pub kind: DependencyEdgeKind,
    pub from_root_id: Option<String>,
    pub to_root_id: Option<String>,
    pub via_path: Option<String>,
    pub unresolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyEdgeKind {
    BuildConfigAffectsRoot,
    RuntimeConfigAffectsRoot,
    AutomationAffectsRoot,
    DependencyConfigAffectsRoot,
    EnvironmentAffectsRoot,
    RootDependsOnRoot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGraphCaveat {
    pub code: ProjectGraphCaveatCode,
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectGraphCaveatCode {
    ConfigEdgeUnresolved,
}

#[derive(Clone, Debug)]
struct RootCandidate {
    path: String,
    language: ProjectLanguage,
    kind: ProjectRootKind,
    markers: BTreeSet<String>,
}

/// Discovers a project graph from files under `workspace_root`.
pub fn discover_project_graph(workspace_root: impl AsRef<Path>) -> Result<ProjectGraph> {
    let workspace_root = workspace_root.as_ref();
    let files = workspace_files(workspace_root)?;
    let roots = discover_roots(&files);
    let (source_owners, generated_sources) = discover_sources(&files, &roots);
    let (config_edges, environment_edges, dependency_edges, caveats) =
        discover_edges(&files, &roots);

    Ok(ProjectGraph {
        schema_version: ProjectGraph::CURRENT_SCHEMA_VERSION,
        roots,
        source_owners,
        generated_sources,
        config_edges,
        environment_edges,
        dependency_edges,
        caveats,
    })
}

fn workspace_files(workspace_root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut builder = WalkBuilder::new(workspace_root);
    builder.hidden(false).ignore(true).git_ignore(true);

    for entry in builder.build() {
        let entry =
            entry.with_context(|| format!("failed to scan {}", workspace_root.display()))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || should_skip_path(entry.path()) {
            continue;
        }
        let rel = rel_path(workspace_root, entry.path());
        files.push(rel);
    }

    files.sort();
    Ok(files)
}

fn discover_roots(files: &[String]) -> Vec<ProjectRoot> {
    let mut candidates: BTreeMap<(String, ProjectLanguage), RootCandidate> = BTreeMap::new();

    for file in files {
        let Some((kind, marker_dir)) = root_marker(file) else {
            continue;
        };
        let language = kind.language();
        let key = (marker_dir.clone(), language.clone());
        let marker = file.clone();
        candidates
            .entry(key)
            .and_modify(|candidate| {
                candidate.markers.insert(marker.clone());
                if kind.priority() < candidate.kind.priority() {
                    candidate.kind = kind.clone();
                }
            })
            .or_insert_with(|| RootCandidate {
                path: marker_dir,
                language,
                kind,
                markers: BTreeSet::from([marker]),
            });
    }

    let mut roots = candidates
        .into_values()
        .map(|candidate| {
            let id = stable_root_id(&candidate.language, &candidate.path);
            ProjectRoot {
                id,
                path: candidate.path,
                language: candidate.language,
                kind: candidate.kind,
                markers: candidate.markers.into_iter().collect(),
            }
        })
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.id.cmp(&right.id))
    });
    roots
}

fn discover_sources(
    files: &[String],
    roots: &[ProjectRoot],
) -> (Vec<SourceOwner>, Vec<GeneratedSource>) {
    let mut source_owners = Vec::new();
    let mut generated_sources = Vec::new();

    for file in files {
        let Some(language) = source_language(file) else {
            continue;
        };
        let Some(root) = owning_root(file, &language, roots) else {
            continue;
        };
        if let Some(reason) = generated_source_reason(file) {
            generated_sources.push(GeneratedSource {
                path: file.clone(),
                owner_root_id: root.id.clone(),
                language,
                semantic_fact_policy: SemanticFactPolicy::SourceOrConfigFactOnly,
                reason,
            });
        } else {
            source_owners.push(SourceOwner {
                path: file.clone(),
                root_id: root.id.clone(),
                language,
                semantic_fact_policy: SemanticFactPolicy::PreciseEligible,
            });
        }
    }

    source_owners.sort_by(|left, right| left.path.cmp(&right.path));
    generated_sources.sort_by(|left, right| left.path.cmp(&right.path));
    (source_owners, generated_sources)
}

fn discover_edges(
    files: &[String],
    roots: &[ProjectRoot],
) -> (
    Vec<ConfigEdge>,
    Vec<EnvironmentEdge>,
    Vec<DependencyEdge>,
    Vec<ProjectGraphCaveat>,
) {
    let mut config_edges = Vec::new();
    let mut environment_edges = Vec::new();
    let mut dependency_edges = Vec::new();
    let mut caveats = Vec::new();

    for file in files {
        if let Some(kind) = config_edge_kind(file) {
            let (owner_root_id, affected_root_ids, unresolved) =
                affected_roots_for_config(file, &kind, roots);
            if unresolved {
                caveats.push(unresolved_caveat(file));
            }
            config_edges.push(ConfigEdge {
                edge_schema: "config_edge/v1".to_string(),
                path: file.clone(),
                kind: kind.clone(),
                owner_root_id: owner_root_id.clone(),
                affected_root_ids: affected_root_ids.clone(),
                unresolved,
                semantic_fact_policy: SemanticFactPolicy::SourceOrConfigFactOnly,
            });
            dependency_edges.extend(config_dependency_edges(
                file,
                &kind,
                owner_root_id,
                &affected_root_ids,
                unresolved,
            ));
        }

        if let Some(kind) = environment_edge_kind(file) {
            let affected_root_ids = all_root_ids(roots);
            let unresolved = affected_root_ids.is_empty();
            if unresolved {
                caveats.push(unresolved_caveat(file));
            }
            environment_edges.push(EnvironmentEdge {
                edge_schema: "environment_edge/v1".to_string(),
                path: file.clone(),
                kind,
                affected_root_ids: affected_root_ids.clone(),
                unresolved,
            });
            dependency_edges.extend(affected_root_ids.iter().map(|root_id| DependencyEdge {
                edge_schema: "dependency_edge/v1".to_string(),
                kind: DependencyEdgeKind::EnvironmentAffectsRoot,
                from_root_id: None,
                to_root_id: Some(root_id.clone()),
                via_path: Some(file.clone()),
                unresolved,
            }));
            if unresolved {
                dependency_edges.push(unresolved_dependency_edge(
                    DependencyEdgeKind::EnvironmentAffectsRoot,
                    file,
                ));
            }
        }

        dependency_edges.extend(root_dependency_edges(file, roots));
    }

    config_edges.sort_by(|left, right| left.path.cmp(&right.path));
    environment_edges.sort_by(|left, right| left.path.cmp(&right.path));
    dependency_edges.sort_by(|left, right| {
        left.via_path
            .cmp(&right.via_path)
            .then_with(|| left.from_root_id.cmp(&right.from_root_id))
            .then_with(|| left.to_root_id.cmp(&right.to_root_id))
            .then_with(|| format!("{:?}", left.kind).cmp(&format!("{:?}", right.kind)))
    });
    caveats.sort_by(|left, right| left.path.cmp(&right.path));
    (config_edges, environment_edges, dependency_edges, caveats)
}

fn root_marker(path: &str) -> Option<(ProjectRootKind, String)> {
    let file_name = file_name(path);
    let kind = match file_name {
        "go.mod" => ProjectRootKind::GoModule,
        "go.work" => ProjectRootKind::GoWorkspace,
        "Cargo.toml" => ProjectRootKind::RustCargo,
        "pom.xml" => ProjectRootKind::JavaMaven,
        "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts" => {
            ProjectRootKind::JavaGradle
        }
        "tsconfig.json" | "jsconfig.json" => ProjectRootKind::TypeScriptConfig,
        "package.json" => ProjectRootKind::TypeScriptPackage,
        _ => return None,
    };
    Some((kind, parent_dir(path)))
}

fn config_edge_kind(path: &str) -> Option<ConfigEdgeKind> {
    if root_marker(path).is_some() {
        return Some(ConfigEdgeKind::BuildConfig);
    }
    if dependency_config_file(path) {
        return Some(ConfigEdgeKind::DependencyConfig);
    }
    if automation_script(path) {
        return Some(ConfigEdgeKind::AutomationScript);
    }
    if runtime_config(path) {
        return Some(ConfigEdgeKind::RuntimeConfig);
    }
    None
}

fn environment_edge_kind(path: &str) -> Option<EnvironmentEdgeKind> {
    let name = file_name(path);
    let lower = path.to_ascii_lowercase();
    let first_component = first_component(&lower);
    if name == "Dockerfile" || name.starts_with("Dockerfile.") {
        Some(EnvironmentEdgeKind::Container)
    } else if lower.ends_with("docker-compose.yml")
        || lower.ends_with("docker-compose.yaml")
        || lower.ends_with("compose.yml")
        || lower.ends_with("compose.yaml")
    {
        Some(EnvironmentEdgeKind::Compose)
    } else if name == ".env" || name.starts_with(".env.") {
        Some(EnvironmentEdgeKind::DotEnv)
    } else if matches!(first_component, Some("k8s" | "kubernetes"))
        || lower.contains("/k8s/")
        || lower.contains("/kubernetes/")
    {
        Some(EnvironmentEdgeKind::Kubernetes)
    } else {
        None
    }
}

fn dependency_config_file(path: &str) -> bool {
    matches!(
        file_name(path),
        "Cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "gradle.lockfile"
            | "go.sum"
    )
}

fn automation_script(path: &str) -> bool {
    let name = file_name(path);
    let lower = path.to_ascii_lowercase();
    name == "Makefile"
        || name == "makefile"
        || name.ends_with(".mk")
        || name.ends_with(".sh")
        || lower.starts_with(".github/workflows/")
        || lower.starts_with(".gitlab-ci")
        || lower.ends_with("/.gitlab-ci.yml")
        || lower.ends_with("/.gitlab-ci.yaml")
        || lower.ends_with("jenkinsfile")
}

fn runtime_config(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if environment_edge_kind(path).is_some() {
        return false;
    }
    matches!(
        extension(&lower).as_deref(),
        Some(
            "env"
                | "conf"
                | "config"
                | "properties"
                | "yaml"
                | "yml"
                | "json"
                | "toml"
                | "ini"
                | "xml",
        )
    )
}

fn affected_roots_for_config(
    path: &str,
    kind: &ConfigEdgeKind,
    roots: &[ProjectRoot],
) -> (Option<String>, Vec<String>, bool) {
    match kind {
        ConfigEdgeKind::BuildConfig => {
            if let Some(root) = root_for_build_config(path, roots) {
                (Some(root.id.clone()), vec![root.id.clone()], false)
            } else if shared_config_path(path) && !roots.is_empty() {
                (None, all_root_ids(roots), false)
            } else {
                (None, Vec::new(), true)
            }
        }
        ConfigEdgeKind::DependencyConfig => affected_roots_for_dependency_config(path, roots),
        ConfigEdgeKind::RuntimeConfig => {
            if root_marker(path).is_none() && shared_config_path(path) && !roots.is_empty() {
                return (None, all_root_ids(roots), false);
            }
            if let Some(root) = roots
                .iter()
                .filter(|root| path_under_root(path, &root.path))
                .max_by_key(|root| root.path.len())
            {
                if shared_config_path(path) {
                    (Some(root.id.clone()), all_root_ids(roots), false)
                } else {
                    (Some(root.id.clone()), vec![root.id.clone()], false)
                }
            } else if shared_config_path(path) && !roots.is_empty() {
                (None, all_root_ids(roots), false)
            } else {
                (None, Vec::new(), true)
            }
        }
        ConfigEdgeKind::AutomationScript => {
            let affected = all_root_ids(roots);
            let unresolved = affected.is_empty();
            (None, affected, unresolved)
        }
    }
}

fn root_for_build_config<'a>(path: &str, roots: &'a [ProjectRoot]) -> Option<&'a ProjectRoot> {
    let (kind, marker_dir) = root_marker(path)?;
    let language = kind.language();
    roots.iter().find(|root| {
        root.path == marker_dir
            && root.language == language
            && root.markers.iter().any(|m| m == path)
    })
}

fn affected_roots_for_dependency_config(
    path: &str,
    roots: &[ProjectRoot],
) -> (Option<String>, Vec<String>, bool) {
    if let Some(language) = dependency_config_language(path) {
        let matching = matching_language_roots(path, &language, roots);
        if !matching.is_empty() {
            let owner_root_id = if matching.len() == 1 {
                Some(matching[0].clone())
            } else {
                None
            };
            return (owner_root_id, matching, false);
        }
    }

    if shared_config_path(path) && !roots.is_empty() {
        return (None, all_root_ids(roots), false);
    }

    if let Some(root) = nearest_root(path, roots) {
        (Some(root.id.clone()), vec![root.id.clone()], false)
    } else {
        (None, Vec::new(), true)
    }
}

fn dependency_config_language(path: &str) -> Option<ProjectLanguage> {
    match file_name(path) {
        "go.sum" => Some(ProjectLanguage::Go),
        "Cargo.lock" => Some(ProjectLanguage::Rust),
        "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" => Some(ProjectLanguage::TypeScript),
        "gradle.lockfile" => Some(ProjectLanguage::Java),
        _ => None,
    }
}

fn matching_language_roots(
    path: &str,
    language: &ProjectLanguage,
    roots: &[ProjectRoot],
) -> Vec<String> {
    let parent = parent_dir(path);
    let exact = roots
        .iter()
        .filter(|root| root.path == parent && &root.language == language)
        .map(|root| root.id.clone())
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }

    let nearest_len = roots
        .iter()
        .filter(|root| &root.language == language && path_under_root(path, &root.path))
        .map(|root| root.path.len())
        .max();
    let Some(nearest_len) = nearest_len else {
        return Vec::new();
    };
    roots
        .iter()
        .filter(|root| {
            &root.language == language
                && root.path.len() == nearest_len
                && path_under_root(path, &root.path)
        })
        .map(|root| root.id.clone())
        .collect()
}

fn nearest_root<'a>(path: &str, roots: &'a [ProjectRoot]) -> Option<&'a ProjectRoot> {
    roots
        .iter()
        .filter(|root| path_under_root(path, &root.path))
        .max_by_key(|root| root.path.len())
}

fn config_dependency_edges(
    path: &str,
    kind: &ConfigEdgeKind,
    owner_root_id: Option<String>,
    affected_root_ids: &[String],
    unresolved: bool,
) -> Vec<DependencyEdge> {
    let dependency_kind = match kind {
        ConfigEdgeKind::BuildConfig => DependencyEdgeKind::BuildConfigAffectsRoot,
        ConfigEdgeKind::RuntimeConfig => DependencyEdgeKind::RuntimeConfigAffectsRoot,
        ConfigEdgeKind::AutomationScript => DependencyEdgeKind::AutomationAffectsRoot,
        ConfigEdgeKind::DependencyConfig => DependencyEdgeKind::DependencyConfigAffectsRoot,
    };

    let mut edges = affected_root_ids
        .iter()
        .map(|root_id| DependencyEdge {
            edge_schema: "dependency_edge/v1".to_string(),
            kind: dependency_kind.clone(),
            from_root_id: owner_root_id.clone(),
            to_root_id: Some(root_id.clone()),
            via_path: Some(path.to_string()),
            unresolved,
        })
        .collect::<Vec<_>>();

    if unresolved {
        edges.push(unresolved_dependency_edge(dependency_kind, path));
    }
    edges
}

fn root_dependency_edges(path: &str, roots: &[ProjectRoot]) -> Vec<DependencyEdge> {
    let Some((kind, marker_dir)) = root_marker(path) else {
        return Vec::new();
    };
    if !matches!(kind, ProjectRootKind::GoWorkspace) {
        return Vec::new();
    }
    let Some(source_root) = roots
        .iter()
        .find(|root| root.path == marker_dir && root.language == ProjectLanguage::Go)
    else {
        return Vec::new();
    };

    roots
        .iter()
        .filter(|root| {
            root.language == ProjectLanguage::Go
                && root.id != source_root.id
                && path_under_root(&root.path, &source_root.path)
        })
        .map(|root| DependencyEdge {
            edge_schema: "dependency_edge/v1".to_string(),
            kind: DependencyEdgeKind::RootDependsOnRoot,
            from_root_id: Some(source_root.id.clone()),
            to_root_id: Some(root.id.clone()),
            via_path: Some(path.to_string()),
            unresolved: false,
        })
        .collect()
}

fn unresolved_dependency_edge(kind: DependencyEdgeKind, path: &str) -> DependencyEdge {
    DependencyEdge {
        edge_schema: "dependency_edge/v1".to_string(),
        kind,
        from_root_id: None,
        to_root_id: None,
        via_path: Some(path.to_string()),
        unresolved: true,
    }
}

fn all_root_ids(roots: &[ProjectRoot]) -> Vec<String> {
    roots.iter().map(|root| root.id.clone()).collect()
}

fn shared_config_path(path: &str) -> bool {
    let parent = parent_dir(path);
    parent == "." || matches!(first_component(path), Some("config" | ".config"))
}

fn owning_root<'a>(
    path: &str,
    language: &ProjectLanguage,
    roots: &'a [ProjectRoot],
) -> Option<&'a ProjectRoot> {
    roots
        .iter()
        .filter(|root| &root.language == language && path_under_root(path, &root.path))
        .max_by_key(|root| root.path.len())
}

fn source_language(path: &str) -> Option<ProjectLanguage> {
    let ext = extension(path)?;
    [
        ProjectLanguage::Go,
        ProjectLanguage::Rust,
        ProjectLanguage::Java,
        ProjectLanguage::TypeScript,
    ]
    .into_iter()
    .find(|language| language.source_extensions().contains(&ext.as_str()))
}

fn generated_source_reason(path: &str) -> Option<GeneratedSourceReason> {
    let lower = path.to_ascii_lowercase();
    if lower
        .split('/')
        .any(|part| matches!(part, "gen" | "generated" | "__generated__" | "dist"))
    {
        Some(GeneratedSourceReason::GeneratedPath)
    } else if file_name(&lower).contains(".generated.") || file_name(&lower).contains(".gen.") {
        Some(GeneratedSourceReason::GeneratedFilename)
    } else {
        None
    }
}

fn unresolved_caveat(path: &str) -> ProjectGraphCaveat {
    ProjectGraphCaveat {
        code: ProjectGraphCaveatCode::ConfigEdgeUnresolved,
        path: path.to_string(),
        message: "config or automation file could not be mapped to a concrete project root"
            .to_string(),
    }
}

fn stable_root_id(language: &ProjectLanguage, path: &str) -> String {
    format!("{language}:{path}")
}

fn path_under_root(path: &str, root_path: &str) -> bool {
    root_path == "." || path == root_path || path.starts_with(&format!("{root_path}/"))
}

fn rel_path(root: &Path, path: &Path) -> String {
    crate::path_compat::relative_path(root, path)
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent.to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn first_component(path: &str) -> Option<&str> {
    path.split('/').next()
}

fn extension(path: &str) -> Option<String> {
    let name = file_name(path);
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext.to_string())
}

fn should_skip_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        matches!(
            value.as_ref(),
            ".git" | ".codetrail" | "node_modules" | "target"
        )
    })
}
