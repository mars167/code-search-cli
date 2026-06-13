//! Minimal LSP server for integration tests.
//! Responds to initialize, documentSymbol, references, and shutdown.

use std::{
    io::{self, BufRead, Write},
    time::Duration,
};

fn main() {
    let language_status_delay_ms = language_status_delay_ms();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut reader = stdin.lock();
    while let Some(message) = read_message(&mut reader) {
        if message.get("method").and_then(|v| v.as_str()) == Some("initialized")
            && message.get("id").is_none()
        {
            if let Some(delay_ms) = language_status_delay_ms {
                std::thread::sleep(Duration::from_millis(delay_ms));
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "language/status",
                    "params": { "type": 2, "message": "ready" }
                });
                write_message(&mut stdout, &notification).ok();
            }
            continue;
        }
        if message.get("method").is_some() && message.get("id").is_none() {
            continue;
        }
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let result = match method {
            "initialize" => serde_json::json!({
                "capabilities": {
                    "general": { "positionEncodings": ["utf-8"] }
                },
                "serverInfo": { "name": "fake-lsp", "version": "0.1.0" }
            }),
            "shutdown" => serde_json::Value::Null,
            "textDocument/documentSymbol" => document_symbols(
                message["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default(),
            ),
            "textDocument/references" => references(&message),
            _ => serde_json::Value::Null,
        };
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        write_message(&mut stdout, &response).ok();
        if method == "shutdown" {
            break;
        }
    }
}

fn language_status_delay_ms() -> Option<u64> {
    std::env::args().skip(1).find_map(|arg| {
        arg.strip_prefix("--language-status-delay-ms=")
            .and_then(|value| value.parse().ok())
    })
}

fn document_symbols(uri: &str) -> serde_json::Value {
    if uri.ends_with("needle.go") {
        return serde_json::json!([{
            "name": "Needle",
            "kind": 12,
            "range": {
                "start": { "line": 2, "character": 0 },
                "end": { "line": 2, "character": 16 }
            },
            "selectionRange": {
                "start": { "line": 2, "character": 5 },
                "end": { "line": 2, "character": 11 }
            },
            "children": []
        }]);
    }
    if uri.ends_with("main.go") {
        return serde_json::json!([{
            "name": "main",
            "kind": 12,
            "range": {
                "start": { "line": 2, "character": 0 },
                "end": { "line": 4, "character": 1 }
            },
            "selectionRange": {
                "start": { "line": 2, "character": 5 },
                "end": { "line": 2, "character": 9 }
            },
            "children": []
        }]);
    }
    serde_json::json!([])
}

fn references(message: &serde_json::Value) -> serde_json::Value {
    let uri = message["params"]["textDocument"]["uri"]
        .as_str()
        .unwrap_or_default();
    let line = message["params"]["position"]["line"].as_u64().unwrap_or(0);
    if !uri.ends_with("needle.go") || line != 2 {
        return serde_json::json!([]);
    }
    serde_json::json!([{
        "uri": uri.replace("needle.go", "main.go"),
        "range": {
            "start": { "line": 3, "character": 4 },
            "end": { "line": 3, "character": 10 }
        }
    }])
}

fn read_message(reader: &mut impl BufRead) -> Option<serde_json::Value> {
    let mut content_length = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let length = content_length?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(writer: &mut impl Write, payload: &serde_json::Value) -> io::Result<()> {
    let body = payload.to_string();
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()
}
