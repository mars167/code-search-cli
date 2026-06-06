//! Static config and script fact extraction.
//!
//! Planned submodule split (tracked in #113):
//!   src/config_facts/
//!   ├── mod.rs        — public types, dispatcher, helpers
//!   ├── json.rs       — JSON extraction
//!   ├── yaml.rs       — YAML extraction + validation
//!   ├── toml.rs       — TOML extraction + validation
//!   ├── ini.rs        — INI/properties/conf extraction
//!   ├── ci.rs         — CI workflow extraction
//!   ├── shell.rs      — Shell script extraction
//!   ├── makefile.rs   — Makefile extraction
//!   ├── docker.rs     — Docker/Compose extraction
//!   └── k8s.rs        — Kubernetes manifest extraction
//!
//! Current state: all extraction functions live in mod.rs with clear section
//! boundaries. The split is deferred to avoid Rust module system friction
//! with cross-cutting helper dependencies.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    project_graph::{DependencyEdgeKind, ProjectGraph, ProjectGraphCaveatCode},
    semantic_facts::{FactReliability, InternalRange},
};

pub const CONFIG_FACT_PRODUCER: &str = "codetrail.config_facts/v1";
pub const DEFAULT_MAX_FILE_BYTES: usize = 256 * 1024;
const MASKED_PREVIEW: &str = "***MASKED***";
const MAX_PREVIEW_CHARS: usize = 160;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFact {
    pub schema_version: u32,
    pub path: String,
    pub range: InternalRange,
    pub fact_kind: ConfigFactKind,
    pub key_path: Option<String>,
    pub name: Option<String>,
    pub value_preview: Option<String>,
    pub preview_masked: bool,
    pub producer: String,
    pub reliability: FactReliability,
    pub affected_root_ids: Vec<String>,
    pub dependency_edge_kind: Option<DependencyEdgeKind>,
    pub dependency_edge_refs: Vec<ConfigDependencyEdgeRef>,
    pub caveats: Vec<ConfigFactCaveat>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFactKind {
    KeyValue,
    WorkflowJob,
    WorkflowStep,
    ScriptBlock,
    ScriptEntrypoint,
    ShellFunction,
    CommandInvocation,
    MakeTarget,
    DockerInstruction,
    DockerService,
    KubernetesResource,
    RuntimeConfigMarker,
    DependencyEdge,
    SourceFactFallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDependencyEdgeRef {
    pub edge_schema: String,
    pub kind: DependencyEdgeKind,
    pub from_root_id: Option<String>,
    pub to_root_id: Option<String>,
    pub via_path: Option<String>,
    pub unresolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFactCaveat {
    pub code: ConfigFactCaveatCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFactCaveatCode {
    LargeFileTruncated,
    ParseFailure,
    SecretMasked,
    ConfigEdgeUnresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigFactExtractOptions {
    pub max_file_bytes: usize,
}

impl ConfigFactExtractOptions {
    pub fn test() -> Self {
        Self {
            max_file_bytes: 16 * 1024,
        }
    }
}

impl Default for ConfigFactExtractOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

#[derive(Clone, Debug)]
struct EdgeContext {
    affected_root_ids: Vec<String>,
    dependency_edge_kind: Option<DependencyEdgeKind>,
    dependency_edge_refs: Vec<ConfigDependencyEdgeRef>,
    caveats: Vec<ConfigFactCaveat>,
}

#[derive(Clone, Debug)]
struct RawKeyValue {
    key_path: String,
    value: String,
    range: InternalRange,
}

#[derive(Clone, Debug)]
struct SourceView<'a> {
    source: Cow<'a, str>,
    caveats: Vec<ConfigFactCaveat>,
}

pub fn extract_workspace_config_facts(
    workspace_root: impl AsRef<Path>,
    graph: &ProjectGraph,
    options: ConfigFactExtractOptions,
) -> Result<Vec<ConfigFact>> {
    let workspace_root = workspace_root.as_ref();
    let mut paths = BTreeSet::new();
    paths.extend(graph.config_edges.iter().map(|edge| edge.path.clone()));
    paths.extend(graph.environment_edges.iter().map(|edge| edge.path.clone()));

    let mut facts = Vec::new();
    for path in paths {
        let full_path = workspace_root.join(&path);
        let source = fs::read_to_string(&full_path).with_context(|| {
            format!("failed to read config fact source {}", full_path.display())
        })?;
        facts.extend(extract_config_facts_for_file(
            &path, &source, graph, options,
        ));
    }
    Ok(facts)
}

pub fn extract_config_facts_for_file(
    path: &str,
    source: &str,
    graph: &ProjectGraph,
    options: ConfigFactExtractOptions,
) -> Vec<ConfigFact> {
    let edge_context = edge_context_for_path(path, graph);
    let view = source_view(source, options);
    let mut base_caveats = view.caveats.clone();
    merge_caveats(&mut base_caveats, edge_context.caveats.clone());

    let extraction = if is_dockerfile(path) {
        Ok(extract_dockerfile(
            path,
            &view.source,
            &edge_context,
            &base_caveats,
        ))
    } else if is_makefile(path) {
        Ok(extract_makefile(
            path,
            &view.source,
            &edge_context,
            &base_caveats,
        ))
    } else if is_shell_script(path, &view.source) {
        Ok(extract_shell(
            path,
            &view.source,
            &edge_context,
            &base_caveats,
        ))
    } else if is_ini_like(path) {
        Ok(extract_ini_like(
            path,
            &view.source,
            &edge_context,
            &base_caveats,
        ))
    } else if extension(path).as_deref() == Some("json") {
        extract_json(path, &view.source, &edge_context, &base_caveats)
    } else if extension(path).as_deref() == Some("toml") {
        extract_toml(path, &view.source, &edge_context, &base_caveats)
    } else if matches!(extension(path).as_deref(), Some("yaml" | "yml")) {
        extract_yaml(path, &view.source, &edge_context, &base_caveats)
    } else {
        Ok(Vec::new())
    };

    match extraction {
        Ok(mut facts) => {
            if facts.is_empty() {
                facts.push(source_fallback_fact(
                    path,
                    &view.source,
                    &edge_context,
                    base_caveats,
                ));
            }
            facts
        }
        Err(message) => {
            base_caveats.push(ConfigFactCaveat {
                code: ConfigFactCaveatCode::ParseFailure,
                message,
                max_bytes: None,
            });
            vec![source_fallback_fact(
                path,
                &view.source,
                &edge_context,
                base_caveats,
            )]
        }
    }
}

pub fn facts_matching_key<'a>(facts: &'a [ConfigFact], query: &str) -> Vec<&'a ConfigFact> {
    let normalized = query.trim();
    facts
        .iter()
        .filter(|fact| {
            fact.key_path.as_deref().is_some_and(|key_path| {
                key_path == normalized
                    || key_path.ends_with(&format!(".{normalized}"))
                    || key_path.contains(normalized)
            })
        })
        .collect()
}

fn extract_json(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> std::result::Result<Vec<ConfigFact>, String> {
    let value = serde_json::from_str::<Value>(source)
        .map_err(|err| format!("structured JSON parse failed: {err}"))?;
    let mut raw = Vec::new();
    collect_json_key_values(source, &value, &mut Vec::new(), &mut raw);
    Ok(raw
        .into_iter()
        .map(|item| {
            key_value_fact(
                path,
                item,
                edge_context,
                base_caveats,
                ConfigFactKind::KeyValue,
            )
        })
        .collect())
}

fn collect_json_key_values(
    source: &str,
    value: &Value,
    key_path: &mut Vec<String>,
    out: &mut Vec<RawKeyValue>,
) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                key_path.push(key.clone());
                collect_json_key_values(source, value, key_path, out);
                key_path.pop();
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                key_path.push(index.to_string());
                collect_json_key_values(source, value, key_path, out);
                key_path.pop();
            }
        }
        Value::String(value) => push_json_leaf(source, key_path, value.clone(), out),
        Value::Number(value) => push_json_leaf(source, key_path, value.to_string(), out),
        Value::Bool(value) => push_json_leaf(source, key_path, value.to_string(), out),
        Value::Null => push_json_leaf(source, key_path, "null".to_string(), out),
    }
}

fn push_json_leaf(source: &str, key_path: &[String], value: String, out: &mut Vec<RawKeyValue>) {
    if key_path.is_empty() {
        return;
    }
    let key = key_path.last().expect("non-empty key path");
    let range = find_line_containing(source, &format!("\"{key}\""))
        .map(|line| line_range(source, line, 0, line_len(source, line)))
        .unwrap_or_else(|| whole_file_range(source));
    out.push(RawKeyValue {
        key_path: key_path.join("."),
        value,
        range,
    });
}

fn extract_yaml(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> std::result::Result<Vec<ConfigFact>, String> {
    validate_yaml_structure(source)?;

    let mut facts = collect_yaml_key_values(source)
        .into_iter()
        .map(|item| {
            key_value_fact(
                path,
                item,
                edge_context,
                base_caveats,
                ConfigFactKind::KeyValue,
            )
        })
        .collect::<Vec<_>>();

    if is_workflow(path) {
        facts.extend(extract_workflow(path, source, edge_context, base_caveats));
    }
    if is_compose(path) {
        facts.extend(extract_compose(path, source, edge_context, base_caveats));
    }
    if is_kubernetes_path(path) || looks_like_kubernetes(source) {
        facts.extend(extract_kubernetes(path, source, edge_context, base_caveats));
    }
    Ok(facts)
}

fn collect_yaml_key_values(source: &str) -> Vec<RawKeyValue> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') || content == "---" || content == "..." {
            continue;
        }
        let indent = leading_spaces(line);
        let (sequence_item, content) = content
            .strip_prefix("- ")
            .map(|rest| (true, rest.trim()))
            .unwrap_or((false, content));

        while stack.last().is_some_and(|(level, _)| indent <= *level) {
            stack.pop();
        }

        let Some((key, value)) = split_yaml_pair(content) else {
            continue;
        };
        let key = clean_key(key);
        if key.is_empty() {
            continue;
        }

        if value.trim().is_empty() {
            stack.push((indent, key));
            continue;
        }

        let mut parts = stack.iter().map(|(_, key)| key.clone()).collect::<Vec<_>>();
        if sequence_item {
            parts.push("[]".to_string());
        }
        parts.push(key);
        out.push(RawKeyValue {
            key_path: normalize_key_path(&parts),
            value: value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
            range: line_range(source, line_index, 0, line.len()),
        });
    }

    out
}

fn validate_yaml_structure(source: &str) -> std::result::Result<(), String> {
    if !has_yaml_content(source) {
        return Ok(());
    }

    for document in serde_yml::Deserializer::from_str(source) {
        serde_yml::Value::deserialize(document)
            .map_err(|_| "structured YAML parse failed".to_string())?;
    }
    Ok(())
}

fn has_yaml_content(source: &str) -> bool {
    source.lines().any(|line| {
        let content = line.trim();
        !content.is_empty() && !content.starts_with('#') && content != "---" && content != "..."
    })
}

fn extract_toml(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> std::result::Result<Vec<ConfigFact>, String> {
    validate_toml_structure(source)?;

    let mut facts = Vec::new();
    let mut section = Vec::new();

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if content.starts_with('[') && content.ends_with(']') {
            let name = content
                .trim_matches('[')
                .trim_matches(']')
                .trim()
                .split('.')
                .map(clean_key)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            section = name;
            continue;
        }
        let Some((key, value)) = split_once_any(content, &['=']) else {
            continue;
        };
        let mut parts = section.clone();
        parts.push(clean_key(key));
        let raw = RawKeyValue {
            key_path: normalize_key_path(&parts),
            value: value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
            range: line_range(source, line_index, 0, line.len()),
        };
        facts.push(key_value_fact(
            path,
            raw,
            edge_context,
            base_caveats,
            ConfigFactKind::KeyValue,
        ));
    }

    Ok(facts)
}

fn validate_toml_structure(source: &str) -> std::result::Result<(), String> {
    toml::from_str::<toml::Value>(source)
        .map(|_| ())
        .map_err(|_| "structured TOML parse failed".to_string())
}

fn extract_ini_like(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let mut facts = Vec::new();
    let mut section = Vec::new();

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') || content.starts_with(';') {
            continue;
        }
        if content.starts_with('[') && content.ends_with(']') {
            section = content
                .trim_matches('[')
                .trim_matches(']')
                .split('.')
                .map(clean_key)
                .filter(|part| !part.is_empty())
                .collect();
            continue;
        }
        let Some((key, value)) =
            split_once_any(content, &['=', ':']).or_else(|| split_whitespace_pair(content))
        else {
            continue;
        };
        let mut parts = section.clone();
        parts.push(clean_key(key));
        let fact_kind = if matches!(extension(path).as_deref(), Some("conf" | "config" | "env")) {
            ConfigFactKind::RuntimeConfigMarker
        } else {
            ConfigFactKind::KeyValue
        };
        facts.push(key_value_fact(
            path,
            RawKeyValue {
                key_path: normalize_key_path(&parts),
                value: value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
                range: line_range(source, line_index, 0, line.len()),
            },
            edge_context,
            base_caveats,
            fact_kind,
        ));
    }

    facts
}

fn extract_workflow(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let mut facts = Vec::new();
    let mut jobs_indent = None;
    let mut current_job: Option<(String, usize)> = None;
    let mut steps_indent = None;

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if content == "jobs:" {
            jobs_indent = Some(indent);
            current_job = None;
            steps_indent = None;
            continue;
        }

        let Some(job_root_indent) = jobs_indent else {
            continue;
        };

        if indent == job_root_indent + 2 && content.ends_with(':') {
            let name = clean_key(content.trim_end_matches(':'));
            if !matches!(name.as_str(), "steps" | "runs-on" | "env" | "permissions") {
                current_job = Some((name.clone(), indent));
                steps_indent = None;
                facts.push(make_fact(
                    path,
                    line_range(source, line_index, 0, line.len()),
                    ConfigFactKind::WorkflowJob,
                    Some(format!("jobs.{name}")),
                    Some(name),
                    None,
                    FactReliability::ConfigFact,
                    edge_context,
                    base_caveats,
                ));
            }
            continue;
        }

        let Some((job_name, job_indent)) = current_job.clone() else {
            continue;
        };
        if indent == job_indent + 2 && content == "steps:" {
            steps_indent = Some(indent);
            continue;
        }

        let Some(step_indent) = steps_indent else {
            continue;
        };
        if indent <= step_indent {
            continue;
        }

        let step_content = content.strip_prefix("- ").unwrap_or(content).trim();
        if let Some((key, value)) = split_yaml_pair(step_content) {
            let key = clean_key(key);
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if matches!(key.as_str(), "name" | "uses") && !value.is_empty() {
                facts.push(make_fact(
                    path,
                    line_range(source, line_index, 0, line.len()),
                    ConfigFactKind::WorkflowStep,
                    Some(format!("jobs.{job_name}.steps.{key}")),
                    Some(value.to_string()),
                    Some(value),
                    FactReliability::ConfigFact,
                    edge_context,
                    base_caveats,
                ));
            }
            if key == "run" && !value.is_empty() {
                facts.push(make_fact(
                    path,
                    line_range(source, line_index, 0, line.len()),
                    ConfigFactKind::ScriptBlock,
                    Some(format!("jobs.{job_name}.steps.run")),
                    Some(job_name.clone()),
                    Some(value),
                    FactReliability::ConfigFact,
                    edge_context,
                    base_caveats,
                ));
            }
        }
    }

    facts
}

fn extract_shell(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let mut facts = Vec::new();

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') && !content.starts_with("#!") {
            continue;
        }
        let range = line_range(source, line_index, 0, line.len());
        if line_index == 0 {
            if let Some(entrypoint) = content.strip_prefix("#!") {
                let entrypoint = entrypoint.trim();
                facts.push(make_fact(
                    path,
                    range,
                    ConfigFactKind::ScriptEntrypoint,
                    Some("script.entrypoint".to_string()),
                    Some(entrypoint.to_string()),
                    Some(entrypoint),
                    FactReliability::ConfigFact,
                    edge_context,
                    base_caveats,
                ));
                continue;
            }
        }
        if let Some(name) = shell_function_name(content) {
            facts.push(make_fact(
                path,
                range,
                ConfigFactKind::ShellFunction,
                Some(format!("shell.function.{name}")),
                Some(name),
                None,
                FactReliability::ConfigFact,
                edge_context,
                base_caveats,
            ));
            continue;
        }
        if let Some(command) = command_name(content) {
            facts.push(make_fact(
                path,
                range,
                ConfigFactKind::CommandInvocation,
                Some(format!("shell.command.{command}")),
                Some(command),
                Some(content),
                FactReliability::ConfigFact,
                edge_context,
                base_caveats,
            ));
        }
    }

    facts
}

fn extract_makefile(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let mut facts = Vec::new();
    let mut current_target: Option<String> = None;

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            if let Some((target, _deps)) = split_once_any(content, &[':']) {
                if !target.contains('=') && !target.trim().is_empty() {
                    let target = target.trim().to_string();
                    current_target = Some(target.clone());
                    facts.push(make_fact(
                        path,
                        line_range(source, line_index, 0, line.len()),
                        ConfigFactKind::MakeTarget,
                        Some(format!("make.target.{target}")),
                        Some(target),
                        None,
                        FactReliability::ConfigFact,
                        edge_context,
                        base_caveats,
                    ));
                }
            }
            continue;
        }

        if let Some(command) = command_name(content) {
            let key_path = current_target
                .as_ref()
                .map(|target| format!("make.target.{target}.recipe.{command}"))
                .unwrap_or_else(|| format!("make.recipe.{command}"));
            facts.push(make_fact(
                path,
                line_range(source, line_index, 0, line.len()),
                ConfigFactKind::CommandInvocation,
                Some(key_path),
                Some(command),
                Some(content),
                FactReliability::ConfigFact,
                edge_context,
                base_caveats,
            ));
        }
    }

    facts
}

fn extract_dockerfile(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let mut facts = Vec::new();
    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let Some((instruction, rest)) = split_whitespace_pair(content) else {
            continue;
        };
        let instruction = instruction.to_ascii_uppercase();
        let range = line_range(source, line_index, 0, line.len());
        facts.push(make_fact(
            path,
            range,
            ConfigFactKind::DockerInstruction,
            Some(format!("dockerfile.{}", instruction.to_ascii_lowercase())),
            Some(instruction.clone()),
            Some(rest.trim()),
            FactReliability::ConfigFact,
            edge_context,
            base_caveats,
        ));
        if matches!(instruction.as_str(), "CMD" | "ENTRYPOINT") {
            facts.push(make_fact(
                path,
                range,
                ConfigFactKind::ScriptEntrypoint,
                Some(format!("dockerfile.{}", instruction.to_ascii_lowercase())),
                Some(instruction),
                Some(rest.trim()),
                FactReliability::ConfigFact,
                edge_context,
                base_caveats,
            ));
        }
    }
    facts
}

fn extract_compose(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let mut facts = Vec::new();
    let mut services_indent = None;

    for (line_index, line) in source.lines().enumerate() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let indent = leading_spaces(line);
        if content == "services:" {
            services_indent = Some(indent);
            continue;
        }
        let Some(root_indent) = services_indent else {
            continue;
        };
        if indent <= root_indent {
            services_indent = None;
            continue;
        }
        if indent == root_indent + 2 && content.ends_with(':') {
            let name = clean_key(content.trim_end_matches(':'));
            facts.push(make_fact(
                path,
                line_range(source, line_index, 0, line.len()),
                ConfigFactKind::DockerService,
                Some(format!("compose.services.{name}")),
                Some(name),
                None,
                FactReliability::ConfigFact,
                edge_context,
                base_caveats,
            ));
        }
    }

    facts
}

fn extract_kubernetes(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> Vec<ConfigFact> {
    let values = collect_yaml_key_values(source);
    let by_key = values
        .iter()
        .map(|item| (item.key_path.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let Some(kind) = by_key.get("kind") else {
        return Vec::new();
    };
    let Some(name) = by_key.get("metadata.name") else {
        return Vec::new();
    };
    let resource_name = format!("{}/{}", kind.value, name.value);
    vec![make_fact(
        path,
        name.range,
        ConfigFactKind::KubernetesResource,
        Some("kubernetes.resource".to_string()),
        Some(resource_name),
        by_key.get("apiVersion").map(|item| item.value.as_str()),
        FactReliability::ConfigFact,
        edge_context,
        base_caveats,
    )]
}

fn key_value_fact(
    path: &str,
    raw: RawKeyValue,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
    fact_kind: ConfigFactKind,
) -> ConfigFact {
    make_fact(
        path,
        raw.range,
        fact_kind,
        Some(raw.key_path),
        None,
        Some(&raw.value),
        FactReliability::ConfigFact,
        edge_context,
        base_caveats,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_fact(
    path: &str,
    range: InternalRange,
    fact_kind: ConfigFactKind,
    key_path: Option<String>,
    name: Option<String>,
    raw_value: Option<&str>,
    reliability: FactReliability,
    edge_context: &EdgeContext,
    base_caveats: &[ConfigFactCaveat],
) -> ConfigFact {
    let preview_context = [key_path.as_deref(), name.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(".");
    let (value_preview, preview_masked, mut caveats) = preview_for(&preview_context, raw_value);
    merge_caveats(&mut caveats, base_caveats.to_vec());

    ConfigFact {
        schema_version: 1,
        path: path.to_string(),
        range,
        fact_kind,
        key_path,
        name,
        value_preview,
        preview_masked,
        producer: CONFIG_FACT_PRODUCER.to_string(),
        reliability,
        affected_root_ids: edge_context.affected_root_ids.clone(),
        dependency_edge_kind: edge_context.dependency_edge_kind.clone(),
        dependency_edge_refs: edge_context.dependency_edge_refs.clone(),
        caveats,
    }
}

fn source_fallback_fact(
    path: &str,
    source: &str,
    edge_context: &EdgeContext,
    caveats: Vec<ConfigFactCaveat>,
) -> ConfigFact {
    make_fact(
        path,
        whole_file_range(source),
        ConfigFactKind::SourceFactFallback,
        Some("source.fallback".to_string()),
        Some(file_name(path).to_string()),
        None,
        FactReliability::SourceFact,
        edge_context,
        &caveats,
    )
}

fn edge_context_for_path(path: &str, graph: &ProjectGraph) -> EdgeContext {
    let mut affected_root_ids = BTreeSet::new();
    let mut unresolved = false;

    for edge in graph.config_edges.iter().filter(|edge| edge.path == path) {
        affected_root_ids.extend(edge.affected_root_ids.iter().cloned());
        unresolved |= edge.unresolved;
    }
    for edge in graph
        .environment_edges
        .iter()
        .filter(|edge| edge.path == path)
    {
        affected_root_ids.extend(edge.affected_root_ids.iter().cloned());
        unresolved |= edge.unresolved;
    }

    let dependency_edge_refs = graph
        .dependency_edges
        .iter()
        .filter(|edge| edge.via_path.as_deref() == Some(path))
        .map(|edge| ConfigDependencyEdgeRef {
            edge_schema: edge.edge_schema.clone(),
            kind: edge.kind.clone(),
            from_root_id: edge.from_root_id.clone(),
            to_root_id: edge.to_root_id.clone(),
            via_path: edge.via_path.clone(),
            unresolved: edge.unresolved,
        })
        .collect::<Vec<_>>();
    let dependency_edge_kind = dependency_edge_refs.first().map(|edge| edge.kind.clone());

    let mut caveats = Vec::new();
    if unresolved
        || graph.caveats.iter().any(|caveat| {
            caveat.path == path && caveat.code == ProjectGraphCaveatCode::ConfigEdgeUnresolved
        })
    {
        caveats.push(ConfigFactCaveat {
            code: ConfigFactCaveatCode::ConfigEdgeUnresolved,
            message: "project graph could not map this config edge to affected roots".to_string(),
            max_bytes: None,
        });
    }

    EdgeContext {
        affected_root_ids: affected_root_ids.into_iter().collect(),
        dependency_edge_kind,
        dependency_edge_refs,
        caveats,
    }
}

fn source_view(source: &str, options: ConfigFactExtractOptions) -> SourceView<'_> {
    let max_file_bytes = options.max_file_bytes.max(1);
    if source.len() <= max_file_bytes {
        return SourceView {
            source: Cow::Borrowed(source),
            caveats: Vec::new(),
        };
    }

    let end = if source.is_char_boundary(max_file_bytes) {
        max_file_bytes
    } else {
        (0..max_file_bytes)
            .rev()
            .find(|index| source.is_char_boundary(*index))
            .unwrap_or(0)
    };
    SourceView {
        source: Cow::Owned(source[..end].to_string()),
        caveats: vec![ConfigFactCaveat {
            code: ConfigFactCaveatCode::LargeFileTruncated,
            message: "config fact extraction used a byte budget and truncated this file"
                .to_string(),
            max_bytes: Some(max_file_bytes),
        }],
    }
}

fn preview_for(
    context: &str,
    raw_value: Option<&str>,
) -> (Option<String>, bool, Vec<ConfigFactCaveat>) {
    let Some(raw_value) = raw_value else {
        return (None, false, Vec::new());
    };

    if is_secret_like(context) || is_secret_like(raw_value) {
        return (
            Some(MASKED_PREVIEW.to_string()),
            true,
            vec![ConfigFactCaveat {
                code: ConfigFactCaveatCode::SecretMasked,
                message: "secret-like config value was masked in preview".to_string(),
                max_bytes: None,
            }],
        );
    }

    let trimmed = raw_value.trim().trim_matches('"').trim_matches('\'');
    let mut preview = trimmed.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    (Some(preview), false, Vec::new())
}

fn is_secret_like(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let normalized = lower
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    [
        "secret",
        "token",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "private_key",
        "privatekey",
        "client_secret",
        "access_key",
        "auth_key",
        "credential",
    ]
    .iter()
    .any(|term| lower.contains(term) || normalized.contains(&term.replace('_', "")))
}

fn merge_caveats(target: &mut Vec<ConfigFactCaveat>, source: Vec<ConfigFactCaveat>) {
    for caveat in source {
        if !target
            .iter()
            .any(|existing| existing.code == caveat.code && existing.message == caveat.message)
        {
            target.push(caveat);
        }
    }
}

fn shell_function_name(content: &str) -> Option<String> {
    if let Some(rest) = content.strip_prefix("function ") {
        let name = rest
            .trim()
            .split(|ch: char| ch.is_whitespace() || ch == '(' || ch == '{')
            .next()
            .unwrap_or_default();
        return valid_shell_name(name).then(|| name.to_string());
    }

    let name = content.split_once("()")?.0.trim();
    valid_shell_name(name).then(|| name.to_string())
}

fn command_name(content: &str) -> Option<String> {
    let content = content.trim_start_matches('@').trim();
    if content.is_empty()
        || content == "{"
        || content == "}"
        || content.starts_with('#')
        || content.starts_with("function ")
        || content.contains("()")
        || content.starts_with("if ")
        || content.starts_with("for ")
        || content.starts_with("while ")
        || content.starts_with("until ")
        || matches!(
            content,
            "then" | "else" | "fi" | "do" | "done" | "case" | "esac"
        )
    {
        return None;
    }
    let token = content.split_whitespace().next()?.trim_matches('"');
    if token.contains('=') && !token.contains('/') {
        return None;
    }
    Some(token.to_string())
}

fn valid_shell_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn split_yaml_pair(content: &str) -> Option<(&str, &str)> {
    let (key, value) = content.split_once(':')?;
    Some((key.trim(), value.trim()))
}

fn split_once_any<'a>(content: &'a str, delimiters: &[char]) -> Option<(&'a str, &'a str)> {
    let index = content.find(|ch| delimiters.contains(&ch))?;
    Some((&content[..index], &content[index + 1..]))
}

fn split_whitespace_pair(content: &str) -> Option<(&str, &str)> {
    let first_len = content
        .char_indices()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index))?;
    let key = &content[..first_len];
    let value = content[first_len..].trim();
    (!key.trim().is_empty() && !value.is_empty()).then_some((key.trim(), value))
}

fn clean_key(key: &str) -> String {
    key.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

fn normalize_key_path(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(".")
}

fn find_line_containing(source: &str, needle: &str) -> Option<usize> {
    source
        .lines()
        .enumerate()
        .find_map(|(index, line)| line.contains(needle).then_some(index))
}

fn line_range(
    source: &str,
    line_index: usize,
    start_column: usize,
    end_column: usize,
) -> InternalRange {
    let line_len = line_len(source, line_index);
    InternalRange {
        start_line: line_index as u32,
        start_column: start_column.min(line_len) as u32,
        end_line: line_index as u32,
        end_column: end_column.min(line_len) as u32,
    }
}

fn whole_file_range(source: &str) -> InternalRange {
    let line_count = source.lines().count();
    if line_count == 0 {
        return InternalRange {
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        };
    }
    let end_line = line_count - 1;
    InternalRange {
        start_line: 0,
        start_column: 0,
        end_line: end_line as u32,
        end_column: line_len(source, end_line) as u32,
    }
}

fn line_len(source: &str, line_index: usize) -> usize {
    source.lines().nth(line_index).map(str::len).unwrap_or(0)
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_workflow(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with(".github/workflows/")
        || lower.ends_with("/.gitlab-ci.yml")
        || lower.ends_with("/.gitlab-ci.yaml")
        || lower == ".gitlab-ci.yml"
        || lower == ".gitlab-ci.yaml"
}

fn is_compose(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("docker-compose.yml")
        || lower.ends_with("docker-compose.yaml")
        || lower.ends_with("compose.yml")
        || lower.ends_with("compose.yaml")
}

fn is_kubernetes_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("k8s/")
        || lower.starts_with("kubernetes/")
        || lower.contains("/k8s/")
        || lower.contains("/kubernetes/")
}

fn looks_like_kubernetes(source: &str) -> bool {
    source.contains("apiVersion:") && source.contains("kind:") && source.contains("metadata:")
}

fn is_dockerfile(path: &str) -> bool {
    let name = file_name(path);
    name == "Dockerfile" || name.starts_with("Dockerfile.")
}

fn is_makefile(path: &str) -> bool {
    let name = file_name(path);
    name == "Makefile" || name == "makefile" || name.ends_with(".mk")
}

fn is_shell_script(path: &str, source: &str) -> bool {
    matches!(extension(path).as_deref(), Some("sh" | "bash" | "zsh"))
        || source.starts_with("#!/bin/sh")
        || source.starts_with("#!/usr/bin/env sh")
        || source.starts_with("#!/usr/bin/env bash")
        || source.starts_with("#!/bin/bash")
}

fn is_ini_like(path: &str) -> bool {
    matches!(
        extension(path).as_deref(),
        Some("ini" | "properties" | "conf" | "config" | "env")
    ) || file_name(path).starts_with(".env")
}

fn extension(path: &str) -> Option<String> {
    file_name(path)
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
