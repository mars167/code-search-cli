use serde_json::Value;

use super::ranges::{find_line_containing, line_len, line_range, whole_file_range};
use super::{
    key_value_fact, ConfigFact, ConfigFactCaveat, ConfigFactKind, EdgeContext, RawKeyValue,
};

pub(super) fn extract_json(
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
