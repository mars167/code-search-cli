use serde_json::{json, Value};

pub(super) fn command_action(kind: &str, argv: Vec<&str>, reason: &str) -> Value {
    let argv = argv
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let command = command_string_from_parts(&argv);
    json!({
        "kind": kind,
        "command": command,
        "argv": argv,
        "reason": reason
    })
}

pub(super) fn command_string_from_argv(argv: &Value) -> String {
    let parts = argv
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    command_string_from_parts(&parts)
}

fn command_string_from_parts(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}
