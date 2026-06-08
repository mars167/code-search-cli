use serde::Serialize;
use serde_json::{json, Value};

use super::caveats::{public_caveats, public_page_truncated};

#[derive(Debug, Serialize)]
pub(super) struct PublicResponse {
    pub(super) results: Value,
    pub(super) page: PublicPage,
    pub(super) caveats: Vec<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PublicPage {
    pub(super) truncated: bool,
    pub(super) next_cursor: Value,
}

pub(super) fn public_response(value: &Value) -> PublicResponse {
    PublicResponse {
        results: public_results(value),
        page: PublicPage {
            truncated: public_page_truncated(value),
            next_cursor: value.get("nextCursor").cloned().unwrap_or(Value::Null),
        },
        caveats: public_caveats(value),
    }
}

pub fn public_response_value(value: &Value) -> Value {
    serde_json::to_value(public_response(value)).unwrap_or_else(|_| {
        json!({
            "results": [],
            "page": {
                "truncated": false,
                "nextCursor": null
            },
            "caveats": [
                {
                    "code": "serialization_error",
                    "message": "failed to serialize public response"
                }
            ]
        })
    })
}

fn public_results(value: &Value) -> Value {
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };
    Value::Array(results.iter().map(public_result).collect())
}

fn public_result(result: &Value) -> Value {
    let Value::Object(object) = result else {
        return result.clone();
    };
    let mut object = object.clone();
    for field in [
        "fileHash",
        "readCommand",
        "readCommandArgv",
        "producer",
        "sourceReason",
        "indexFresh",
        "reliability",
        "exact",
        "knownBlindSpots",
        "fallbackReason",
        "previewTruncatedReason",
    ] {
        object.remove(field);
    }
    sanitize_public_object(&mut object);
    Value::Object(object)
}

fn sanitize_public_object(object: &mut serde_json::Map<String, Value>) {
    for value in object.values_mut() {
        sanitize_public_value(value);
    }
    object.retain(|key, value| keep_public_field(key, value));
}

fn sanitize_public_value(value: &mut Value) {
    match value {
        Value::Object(object) => sanitize_public_object(object),
        Value::Array(values) => {
            for value in values {
                sanitize_public_value(value);
            }
        }
        _ => {}
    }
}

fn keep_public_field(key: &str, value: &Value) -> bool {
    if value.is_null() {
        return false;
    }
    if matches!(key, "context" | "warnings") {
        return !value.as_array().is_some_and(Vec::is_empty);
    }
    if matches!(key, "previewTruncated" | "truncated" | "binary") {
        return value.as_bool().unwrap_or(true);
    }
    if key == "warning" {
        return value.as_str().is_some_and(|warning| !warning.is_empty());
    }
    true
}
