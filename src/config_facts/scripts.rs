use super::ranges::line_range;
use super::{make_fact, split_once_any, ConfigFact, ConfigFactCaveat, ConfigFactKind, EdgeContext};
use crate::semantic_facts::FactReliability;

pub(super) fn extract_shell(
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

pub(super) fn extract_makefile(
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
