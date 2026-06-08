use std::collections::BTreeMap;

use serde::Deserialize;

use super::detect::{is_compose, is_kubernetes_path, is_workflow, looks_like_kubernetes};
use super::ranges::{leading_spaces, line_range};
use super::{
    clean_key, key_value_fact, make_fact, normalize_key_path, ConfigFact, ConfigFactCaveat,
    ConfigFactKind, EdgeContext, RawKeyValue,
};
use crate::semantic_facts::FactReliability;

pub(super) fn extract_yaml(
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

fn split_yaml_pair(content: &str) -> Option<(&str, &str)> {
    let (key, value) = content.split_once(':')?;
    Some((key.trim(), value.trim()))
}
