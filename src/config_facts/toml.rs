use super::ranges::line_range;
use super::{
    clean_key, key_value_fact, normalize_key_path, split_once_any, ConfigFact, ConfigFactCaveat,
    ConfigFactKind, EdgeContext, RawKeyValue,
};

pub(super) fn extract_toml(
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
