use super::detect::extension;
use super::ranges::line_range;
use super::{
    clean_key, key_value_fact, normalize_key_path, split_once_any, split_whitespace_pair,
    ConfigFact, ConfigFactCaveat, ConfigFactKind, EdgeContext, RawKeyValue,
};

pub(super) fn extract_ini_like(
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
