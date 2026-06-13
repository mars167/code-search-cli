use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use super::registry::{path_to_uri, ReadinessStrategy, ServerSpec};
use super::transport::JsonRpcTransport;

#[derive(Clone, Debug)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Clone, Debug)]
pub struct LspLocation {
    pub uri: String,
    pub range: LspRange,
}

#[derive(Clone, Debug)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: u32,
    pub range: LspRange,
    pub selection_range: LspRange,
    pub children: Vec<DocumentSymbol>,
}

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

pub struct LspClient {
    transport: JsonRpcTransport,
    workspace_root: std::path::PathBuf,
    position_encoding: String,
    server_info: Option<ServerInfo>,
}

impl LspClient {
    pub fn spawn(spec: &ServerSpec, workspace_root: &Path) -> Result<Self> {
        let transport = JsonRpcTransport::spawn(&spec.program, &spec.args, workspace_root)?;
        Ok(Self {
            transport,
            workspace_root: workspace_root.to_path_buf(),
            position_encoding: "utf-16".to_string(),
            server_info: None,
        })
    }

    pub fn initialize(&mut self, root_uri: &str, readiness: &ReadinessStrategy) -> Result<()> {
        let result = self.transport.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {
                    "name": "codetrail",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "rootUri": root_uri,
                "capabilities": {
                    "workspace": {
                        "symbol": { "dynamicRegistration": false }
                    },
                    "textDocument": {
                        "documentSymbol": {
                            "dynamicRegistration": false,
                            "hierarchicalDocumentSymbolSupport": true,
                        },
                        "references": { "dynamicRegistration": false },
                    },
                    "general": {
                        "positionEncodings": ["utf-8", "utf-16"]
                    }
                },
            }),
        )?;

        if let Some(capabilities) = result.get("capabilities") {
            if let Some(general) = capabilities.get("general") {
                if let Some(encodings) = general.get("positionEncodings").and_then(Value::as_array)
                {
                    if encodings.iter().any(|v| v.as_str() == Some("utf-8")) {
                        self.position_encoding = "utf-8".to_string();
                    }
                }
            }
        }
        if let Some(info) = result.get("serverInfo") {
            self.server_info = Some(ServerInfo {
                name: info
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                version: info
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
            });
        }

        self.transport.notify("initialized", json!({}))?;
        self.wait_readiness(readiness)?;
        Ok(())
    }

    fn wait_readiness(&self, readiness: &ReadinessStrategy) -> Result<()> {
        match readiness {
            ReadinessStrategy::Immediate => Ok(()),
            ReadinessStrategy::ProgressEnd { timeout_ms } => {
                let _ = self
                    .transport
                    .wait_notification("$/progress", Duration::from_millis(*timeout_ms));
                Ok(())
            }
            ReadinessStrategy::LanguageStatus { timeout_ms } => {
                let deadline = std::time::Instant::now() + Duration::from_millis(*timeout_ms);
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return Ok(());
                    }
                    let wait_for = remaining.min(Duration::from_secs(1));
                    if let Some(notification) = self
                        .transport
                        .wait_notification("language/status", wait_for)?
                    {
                        if notification
                            .get("params")
                            .and_then(|p| p.get("type"))
                            .and_then(Value::as_u64)
                            == Some(2)
                        {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server_info.as_ref()
    }

    pub fn position_encoding(&self) -> &str {
        &self.position_encoding
    }

    pub fn did_open(&self, relative_path: &str, language_id: &str, text: &str) -> Result<()> {
        let uri = path_to_uri(&self.workspace_root, relative_path)?;
        self.transport.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )
    }

    pub fn document_symbol(&self, relative_path: &str) -> Result<Vec<DocumentSymbol>> {
        let uri = path_to_uri(&self.workspace_root, relative_path)?;
        let result = self.transport.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        parse_document_symbols(&result)
    }

    pub fn references(
        &self,
        relative_path: &str,
        position: &LspPosition,
        include_declaration: bool,
    ) -> Result<Vec<LspLocation>> {
        let uri = path_to_uri(&self.workspace_root, relative_path)?;
        let result = self.transport.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
                "context": { "includeDeclaration": include_declaration },
            }),
        )?;
        let Some(items) = result.as_array() else {
            return Ok(Vec::new());
        };
        Ok(items
            .iter()
            .filter_map(|item| parse_location(item, &uri))
            .collect())
    }

    pub fn shutdown(&self) -> Result<()> {
        let _ = self.transport.request("shutdown", json!(null));
        let _ = self.transport.notify("exit", json!(null));
        self.transport.shutdown_transport()?;
        self.transport.kill()?;
        Ok(())
    }
}

fn parse_document_symbols(value: &Value) -> Result<Vec<DocumentSymbol>> {
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    Ok(items.iter().filter_map(parse_document_symbol).collect())
}

fn parse_document_symbol(value: &Value) -> Option<DocumentSymbol> {
    let name = value.get("name")?.as_str()?.to_string();
    let kind = value.get("kind")?.as_u64()? as u32;
    let range = parse_range(value.get("range")?)?;
    let selection_range = value
        .get("selectionRange")
        .and_then(parse_range)
        .unwrap_or_else(|| range.clone());
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_document_symbol).collect())
        .unwrap_or_default();
    Some(DocumentSymbol {
        name,
        kind,
        range,
        selection_range,
        children,
    })
}

fn parse_range(value: &Value) -> Option<LspRange> {
    Some(LspRange {
        start: parse_position(value.get("start")?)?,
        end: parse_position(value.get("end")?)?,
    })
}

fn parse_position(value: &Value) -> Option<LspPosition> {
    Some(LspPosition {
        line: value.get("line")?.as_u64()? as u32,
        character: value.get("character")?.as_u64()? as u32,
    })
}

fn parse_location(value: &Value, fallback_uri: &str) -> Option<LspLocation> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)
        .unwrap_or(fallback_uri)
        .to_string();
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))
        .and_then(parse_range)
        .or_else(|| parse_range(value))?;
    Some(LspLocation { uri, range })
}
