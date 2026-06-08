use super::ranges::line_range;
use super::{
    make_fact, split_whitespace_pair, ConfigFact, ConfigFactCaveat, ConfigFactKind, EdgeContext,
};
use crate::semantic_facts::FactReliability;

pub(super) fn extract_dockerfile(
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
