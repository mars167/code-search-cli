use std::{borrow::Cow, collections::BTreeSet};

use crate::{
    project_graph::{DependencyEdgeKind, ProjectGraph, ProjectGraphCaveatCode},
    semantic_facts::{FactReliability, InternalRange},
};

use super::detect::file_name;
use super::ranges::whole_file_range;
use super::{
    ConfigDependencyEdgeRef, ConfigFact, ConfigFactCaveat, ConfigFactCaveatCode,
    ConfigFactExtractOptions, ConfigFactKind, CONFIG_FACT_PRODUCER, MASKED_PREVIEW,
    MAX_PREVIEW_CHARS,
};

#[derive(Clone, Debug)]
pub(crate) struct EdgeContext {
    affected_root_ids: Vec<String>,
    dependency_edge_kind: Option<DependencyEdgeKind>,
    dependency_edge_refs: Vec<ConfigDependencyEdgeRef>,
    pub(crate) caveats: Vec<ConfigFactCaveat>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawKeyValue {
    pub(crate) key_path: String,
    pub(crate) value: String,
    pub(crate) range: InternalRange,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceView<'a> {
    pub(crate) source: Cow<'a, str>,
    pub(crate) caveats: Vec<ConfigFactCaveat>,
}

pub(crate) fn key_value_fact(
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
pub(crate) fn make_fact(
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

pub(crate) fn source_fallback_fact(
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

pub(crate) fn edge_context_for_path(path: &str, graph: &ProjectGraph) -> EdgeContext {
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

pub(crate) fn source_view(source: &str, options: ConfigFactExtractOptions) -> SourceView<'_> {
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

pub(crate) fn merge_caveats(target: &mut Vec<ConfigFactCaveat>, source: Vec<ConfigFactCaveat>) {
    for caveat in source {
        if !target
            .iter()
            .any(|existing| existing.code == caveat.code && existing.message == caveat.message)
        {
            target.push(caveat);
        }
    }
}

pub(crate) fn split_once_any<'a>(
    content: &'a str,
    delimiters: &[char],
) -> Option<(&'a str, &'a str)> {
    let index = content.find(|ch| delimiters.contains(&ch))?;
    Some((&content[..index], &content[index + 1..]))
}

pub(crate) fn split_whitespace_pair(content: &str) -> Option<(&str, &str)> {
    let first_len = content
        .char_indices()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index))?;
    let key = &content[..first_len];
    let value = content[first_len..].trim();
    (!key.trim().is_empty() && !value.is_empty()).then_some((key.trim(), value))
}

pub(crate) fn clean_key(key: &str) -> String {
    key.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

pub(crate) fn normalize_key_path(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(".")
}
