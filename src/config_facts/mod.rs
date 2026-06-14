//! Static config and script fact extraction.
//!
mod containers;
mod detect;
mod ini;
mod json;
mod mybatis;
mod ranges;
mod scripts;
mod support;
mod toml;
mod yaml;

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    project_graph::{DependencyEdgeKind, ProjectGraph},
    semantic_facts::{FactReliability, InternalRange},
};

use containers::extract_dockerfile;
use detect::{extension, is_dockerfile, is_ini_like, is_makefile, is_shell_script};
use ini::extract_ini_like;
use json::extract_json;
use mybatis::extract_mybatis_xml;
use scripts::{extract_makefile, extract_shell};
pub(super) use support::{
    clean_key, key_value_fact, make_fact, normalize_key_path, split_once_any,
    split_whitespace_pair, EdgeContext, RawKeyValue,
};
use support::{edge_context_for_path, merge_caveats, source_fallback_fact, source_view};
use toml::extract_toml;
use yaml::extract_yaml;

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
    MyBatisNamespace,
    MyBatisStatement,
    MyBatisResultMap,
    MyBatisSqlFragment,
    MyBatisReference,
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
        let Ok(source) = fs::read_to_string(&full_path) else {
            continue;
        };
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
    } else if extension(path).as_deref() == Some("xml") {
        extract_mybatis_xml(path, &view.source, &edge_context, &base_caveats)
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
