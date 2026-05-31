//! MCP (Model Context Protocol) adapter that wraps the [`QueryService`] and
//! exposes code-search operations to LLM agents (Claude, Cursor, etc.) over
//! stdio-based JSON-RPC 2.0.
//!
//! ## Protocol flow
//!
//! ```text
//! Client                          Server
//!   │                                │
//!   ├─ initialize ──────────────────>│
//!   │<─────── capabilities ─────────┤
//!   │                                │
//!   ├─ tools/list ──────────────────>│
//!   │<─────── tool definitions ─────┤
//!   │                                │
//!   ├─ tools/call {name, args} ─────>│
//!   │<─────── result (JSON) ────────┤
//!   │                                │
//! ```
//!
//! All results carry reliability metadata
//! (`snapshot_id`, `reliability`, `producer`, `exact`) produced by the
//! underlying [`QueryService`].

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::{
    output,
    query::{QueryOptions, QueryService},
};

mod protocol;

use crate::mcp::protocol::{
    ok_response, Envelope, InitializeResult, ServerInfo, ToolCallParams, ToolCallResult, ToolDef,
    ToolResultContent, ToolsListResult,
};
// ---------------------------------------------------------------------------
// MCP constants
// ---------------------------------------------------------------------------

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "code-search";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Tool definitions  (static so we can serve tools/list without I/O)
// ---------------------------------------------------------------------------

/// Build the list of all available tools.
fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "code_search_find".to_string(),
            description:
                "Full-text / literal search across the codebase. Returns matching lines with file paths and line numbers."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Literal text to search for" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include (AND filter)" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" },
                    "context": { "type": "integer", "minimum": 0, "maximum": 65535, "default": 0, "description": "Lines of context around each match" }
                },
                "required": ["text"]
            }),
        },
        ToolDef {
            name: "code_search_grep".to_string(),
            description: "Regex search across the codebase. Returns matching lines with file paths and line numbers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression pattern" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" },
                    "context": { "type": "integer", "minimum": 0, "maximum": 65535, "default": 0, "description": "Lines of context around each match" }
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "code_search_files".to_string(),
            description:
                "Find files whose path contains the given substring. Returns file metadata."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Substring to match in file paths" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "code_search_glob".to_string(),
            description: "Find files matching a strict glob pattern (e.g. `**/*.rs`). Returns file metadata."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (e.g. **/*.rs)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "code_search_list".to_string(),
            description:
                "List directory contents in the workspace. Returns path facts with file/directory metadata."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "default": ".", "description": "Directory to list relative to the workspace root" },
                    "recursive": { "type": "boolean", "default": false, "description": "List recursively" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "code_search_tree".to_string(),
            description:
                "Return a recursive tree view for a workspace directory."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "default": ".", "description": "Directory to traverse relative to the workspace root" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 255, "description": "Maximum traversal depth" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "code_search_read".to_string(),
            description:
                "Read file contents, optionally with a line-range like `path:1-10`. Returns the file content with metadata."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "File path with optional `:start-end` line range" }
                },
                "required": ["target"]
            }),
        },
        ToolDef {
            name: "code_search_defs".to_string(),
            description:
                "Find definitions of a given identifier. Prefers SCIP precise index; falls back to tree-sitter parser."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "identifier": { "type": "string", "description": "Identifier to find definitions for" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["identifier"]
            }),
        },
        ToolDef {
            name: "code_search_refs".to_string(),
            description:
                "Find references to a given identifier. Prefers SCIP precise index; falls back to text search."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "identifier": { "type": "string", "description": "Identifier to find references for" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["identifier"]
            }),
        },
        ToolDef {
            name: "code_search_symbols".to_string(),
            description:
                "Find symbols (functions, structs, classes, etc.) matching a query. Prefers SCIP; falls back to tree-sitter."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Symbol name query (substring match)" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "lang": { "type": "array", "items": { "type": "string" }, "description": "Languages to include" },
                    "changed": { "type": "boolean", "default": false, "description": "Restrict search to git changed files" },
                    "cursor": { "type": "string", "description": "Pagination cursor from a previous response" },
                    "allowBroad": { "type": "boolean", "default": false, "description": "Allow broad queries to return full paginated results" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "code_search_calls".to_string(),
            description:
                "Find outgoing calls from a given function/symbol. Results are inferred candidates due to limitations in static analysis."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "identifier": { "type": "string", "description": "Function/symbol name to query outgoing calls for" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["identifier"]
            }),
        },
        ToolDef {
            name: "code_search_callers".to_string(),
            description:
                "Find incoming callers of a given function/symbol. Results are inferred candidates due to limitations in static analysis."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "identifier": { "type": "string", "description": "Function/symbol name to query incoming callers for" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Path substrings to exclude" },
                    "limit": { "type": "integer", "minimum": 0, "default": 100, "description": "Max results" }
                },
                "required": ["identifier"]
            }),
        },
        ToolDef {
            name: "code_search_changed".to_string(),
            description:
                "List changed (git-modified or untracked) files in the workspace."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDef {
            name: "code_search_status".to_string(),
            description:
                "Return workspace status including snapshot_id, dirty flag, git root, and index information."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

/// Stdio-based MCP server that wraps [`QueryService`].
pub struct Server {
    service: QueryService,
}

impl Server {
    /// Create a new MCP server backed by the given workspace root.
    pub fn new(root: &std::path::Path) -> Result<Self> {
        let service = QueryService::new(root)?;
        Ok(Self { service })
    }

    /// Run the server loop: read JSON-RPC lines from stdin, dispatch, write responses to stdout.
    pub fn run(&self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();

        for line in stdin.lock().lines() {
            let line = line.context("failed to read stdin line")?;
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            let envelope: Envelope = match serde_json::from_str(&line) {
                Ok(env) => env,
                Err(e) => {
                    let err_resp = protocol::ErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id: Value::Null,
                        error: protocol::RpcError {
                            code: protocol::RpcError::PARSE_ERROR,
                            message: format!("Parse error: {e}"),
                            data: None,
                        },
                    };
                    let resp_str = serde_json::to_string(&Envelope::ErrorResponse(err_resp))?;
                    writeln!(stdout, "{resp_str}")?;
                    stdout.flush()?;
                    continue;
                }
            };

            let response = self.dispatch(envelope);

            // Notifications have no id → no response.
            if let Some(resp) = response {
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(stdout, "{resp_str}")?;
                stdout.flush()?;
            }
        }

        Ok(())
    }

    /// Dispatch a single JSON-RPC envelope to the appropriate handler.
    fn dispatch(&self, envelope: Envelope) -> Option<Envelope> {
        match envelope {
            Envelope::Request(req) => {
                if protocol::is_notification(&req.id) {
                    // Treat as notification — MCP clients may send initialized as notification.
                    return None;
                }
                self.handle_request(req)
            }
            Envelope::Notification(_notif) => {
                // MCP clients send `notifications/initialized` as a notification;
                // we silently acknowledge it.
                None
            }
            // Server shouldn't receive responses, but ignore them gracefully.
            _ => None,
        }
    }

    /// Handle a JSON-RPC request.
    fn handle_request(&self, req: protocol::Request) -> Option<Envelope> {
        let id = req.id.clone();
        let result = match req.method.as_str() {
            "initialize" => self.handle_initialize(),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => {
                let params: ToolCallParams =
                    match serde_json::from_value(req.params.unwrap_or(Value::Null)) {
                        Ok(p) => p,
                        Err(e) => {
                            return Some(Envelope::ErrorResponse(
                                protocol::RpcError::invalid_params(
                                    id,
                                    format!("Invalid params: {e}"),
                                ),
                            ));
                        }
                    };
                self.handle_tool_call(&params)
            }
            _ => {
                return Some(Envelope::ErrorResponse(
                    protocol::RpcError::method_not_found(id),
                ));
            }
        };

        match result {
            Ok(value) => Some(ok_response(id, value)),
            Err(e) => Some(Envelope::ErrorResponse(protocol::RpcError::internal_error(
                id,
                e.to_string(),
            ))),
        }
    }

    // -- initialize ----------------------------------------------------

    fn handle_initialize(&self) -> Result<Value> {
        let init = InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: json!({
                "tools": {}
            }),
            server_info: ServerInfo {
                name: SERVER_NAME.to_string(),
                version: SERVER_VERSION.to_string(),
            },
        };
        Ok(serde_json::to_value(init)?)
    }

    // -- tools/list ----------------------------------------------------

    fn handle_tools_list(&self) -> Result<Value> {
        let result = ToolsListResult {
            tools: tool_definitions(),
        };
        Ok(serde_json::to_value(result)?)
    }

    // -- tools/call ----------------------------------------------------

    fn handle_tool_call(&self, params: &ToolCallParams) -> Result<Value> {
        let result = self.execute_tool(&params.name, params.arguments.as_ref())?;
        Ok(serde_json::to_value(result)?)
    }

    /// Execute a named tool with optional arguments.
    fn execute_tool(&self, name: &str, args: Option<&Value>) -> Result<ToolCallResult> {
        match self.execute_tool_value(name, args) {
            Ok(query_result) => Ok(tool_result(query_result, false)),
            Err(error) => Ok(tool_result(output::error_response(error), true)),
        }
    }

    fn execute_tool_value(&self, name: &str, args: Option<&Value>) -> Result<Value> {
        let opts = parse_query_options(args)?;

        match name {
            "code_search_find" => {
                let text = required_str(args, "text")?;
                self.service.find(text, &opts)
            }
            "code_search_grep" => {
                let pattern = required_str(args, "pattern")?;
                self.service.grep(pattern, &opts)
            }
            "code_search_files" => {
                let pattern = required_str(args, "pattern")?;
                self.service.files(pattern, &opts)
            }
            "code_search_glob" => {
                let pattern = required_str(args, "pattern")?;
                self.service.glob(pattern, &opts)
            }
            "code_search_list" => {
                reject_unsupported_browse_scope(&opts)?;
                let dir = optional_str(args, "dir");
                let recursive = optional_bool(args, "recursive").unwrap_or(false);
                self.service.list(dir, recursive, &opts)
            }
            "code_search_tree" => {
                reject_unsupported_browse_scope(&opts)?;
                let dir = optional_str(args, "dir");
                let depth = optional_depth(args)?;
                self.service.tree(dir, depth, &opts)
            }
            "code_search_read" => {
                let target = required_str(args, "target")?;
                self.service.read_file(target)
            }
            "code_search_defs" => {
                let identifier = required_str(args, "identifier")?;
                self.service.defs(identifier, &opts)
            }
            "code_search_refs" => {
                let identifier = required_str(args, "identifier")?;
                self.service.refs(identifier, &opts)
            }
            "code_search_symbols" => {
                let query = required_str(args, "query")?;
                self.service.symbols(query, &opts)
            }
            "code_search_calls" => {
                let identifier = required_str(args, "identifier")?;
                self.service.calls(identifier, &opts)
            }
            "code_search_callers" => {
                let identifier = required_str(args, "identifier")?;
                self.service.callers(identifier, &opts)
            }
            "code_search_changed" => self.service.changed(),
            "code_search_status" => self.service.status(),
            _ => Err(anyhow::anyhow!("unknown tool: {name}")),
        }
    }
}

fn tool_result(value: Value, is_error: bool) -> ToolCallResult {
    ToolCallResult {
        content: vec![ToolResultContent {
            content_type: "text".to_string(),
            text: value.to_string(),
        }],
        is_error,
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Extract a required string argument from the tool arguments JSON object.
fn required_str<'a>(args: Option<&'a Value>, field: &str) -> Result<&'a str> {
    let obj = args
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("missing arguments"))?;
    let value = obj
        .get(field)
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {field}"))?;
    value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("argument '{field}' must be a string"))
}

fn optional_str<'a>(args: Option<&'a Value>, field: &str) -> Option<&'a str> {
    args.and_then(Value::as_object)?
        .get(field)
        .and_then(Value::as_str)
}

fn optional_bool(args: Option<&Value>, field: &str) -> Option<bool> {
    args.and_then(Value::as_object)?
        .get(field)
        .and_then(Value::as_bool)
}

fn optional_depth(args: Option<&Value>) -> Result<Option<u8>> {
    let Some(depth_value) = args
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("depth"))
    else {
        return Ok(None);
    };
    let Some(depth) = depth_value.as_u64() else {
        return Err(anyhow::anyhow!(
            "invalid_mcp_argument: depth must be an integer between 0 and 255"
        ));
    };
    if depth > u8::MAX as u64 {
        return Err(anyhow::anyhow!(
            "invalid_mcp_argument: depth must be between 0 and 255"
        ));
    }
    Ok(Some(depth as u8))
}

fn reject_unsupported_browse_scope(opts: &QueryOptions) -> Result<()> {
    if !opts.lang.is_empty() || opts.changed {
        return Err(anyhow::anyhow!(
            "unsupported_mcp_scope: code_search_list/tree support include/exclude/limit, but not lang or changed scope"
        ));
    }
    Ok(())
}

/// Parse [`QueryOptions`] from the tool arguments JSON object.
fn parse_query_options(args: Option<&Value>) -> Result<QueryOptions> {
    let obj = match args.and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return Ok(QueryOptions::default()),
    };

    Ok(QueryOptions {
        include: extract_string_array(obj, "include"),
        exclude: extract_string_array(obj, "exclude"),
        lang: extract_string_array(obj, "lang"),
        changed: obj
            .get("changed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        cursor: obj
            .get("cursor")
            .and_then(|v| v.as_str())
            .map(ToString::to_string),
        allow_broad: obj
            .get("allowBroad")
            .or_else(|| obj.get("allow_broad"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        limit: optional_usize_arg(obj, "limit", 100)?,
        context: optional_u16_arg(obj, "context", 0)?,
    })
}

fn extract_string_array(obj: &serde_json::Map<String, Value>, field: &str) -> Vec<String> {
    obj.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn optional_usize_arg(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    default: usize,
) -> Result<usize> {
    let Some(value) = obj.get(field) else {
        return Ok(default);
    };
    let Some(number) = value.as_u64() else {
        return Err(anyhow::anyhow!(
            "invalid_mcp_argument: {field} must be a non-negative integer"
        ));
    };
    usize::try_from(number).map_err(|_| {
        anyhow::anyhow!("invalid_mcp_argument: {field} must fit in the platform usize")
    })
}

fn optional_u16_arg(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    default: u16,
) -> Result<u16> {
    let Some(value) = obj.get(field) else {
        return Ok(default);
    };
    let Some(number) = value.as_u64() else {
        return Err(anyhow::anyhow!(
            "invalid_mcp_argument: {field} must be an integer between 0 and 65535"
        ));
    };
    if number > u16::MAX as u64 {
        return Err(anyhow::anyhow!(
            "invalid_mcp_argument: {field} must be an integer between 0 and 65535"
        ));
    }
    Ok(number as u16)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn call_tool(server: &Server, name: &str, arguments: Value) -> ToolCallResult {
        server.execute_tool(name, Some(&arguments)).unwrap()
    }

    fn call_tool_json(server: &Server, name: &str, arguments: Value) -> Value {
        let result = call_tool(server, name, arguments);
        assert!(!result.is_error, "tool returned error: {result:?}");
        serde_json::from_str(&result.content[0].text).unwrap()
    }

    // ------------------------------------------------------------------
    //  Protocol-level tests  (unit)
    // ------------------------------------------------------------------

    #[test]
    fn server_handles_initialize() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: "initialize".to_string(),
            params: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {}
            })),
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let init: InitializeResult = serde_json::from_value(sr.result).unwrap();
                assert_eq!(init.protocol_version, "2024-11-05");
                assert_eq!(init.server_info.name, "code-search");
                assert!(init.capabilities.get("tools").is_some());
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn server_handles_tools_list() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(2),
            method: "tools/list".to_string(),
            params: None,
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let list: ToolsListResult = serde_json::from_value(sr.result).unwrap();
                let names: Vec<&str> = list.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"code_search_find"));
                assert!(names.contains(&"code_search_defs"));
                assert!(names.contains(&"code_search_list"));
                assert!(names.contains(&"code_search_tree"));
                assert!(names.contains(&"code_search_status"));
                // All core CLI-backed tools should be present.
                assert_eq!(list.tools.len(), 14);
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn server_handles_tools_call_find() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    println!(\"needle\");\n}\n",
        )
        .unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(3),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "code_search_find",
                "arguments": { "text": "needle" }
            })),
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let result: ToolCallResult = serde_json::from_value(sr.result).unwrap();
                assert!(!result.is_error);
                let text = &result.content[0].text;
                let parsed: Value = serde_json::from_str(text).unwrap();
                assert_eq!(parsed["ok"], true);
                assert_eq!(parsed["reliability"]["level"], "source_fact");
                assert_eq!(parsed["results"][0]["path"], "src/main.rs");
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn server_handles_tools_call_defs() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(4),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "code_search_defs",
                "arguments": { "identifier": "alpha" }
            })),
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let result: ToolCallResult = serde_json::from_value(sr.result).unwrap();
                assert!(!result.is_error);
                let text = &result.content[0].text;
                let parsed: Value = serde_json::from_str(text).unwrap();
                assert_eq!(parsed["ok"], true);
                assert!(
                    parsed["reliability"]["level"] == "parser_fact"
                        || parsed["reliability"]["level"] == "precise_fact"
                );
                let results = parsed["results"].as_array().unwrap();
                assert!(results.iter().any(|r| r["name"] == "alpha"));
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn server_returns_error_for_unknown_tool() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(5),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "code_search_nonexistent",
                "arguments": {}
            })),
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let result: ToolCallResult = serde_json::from_value(sr.result).unwrap();
                assert!(result.is_error);
                let parsed: Value = serde_json::from_str(&result.content[0].text).unwrap();
                assert_eq!(parsed["ok"], false);
                assert_eq!(parsed["error"]["code"], "unknown_tool");
            }
            _ => panic!("expected success for unknown tool"),
        }
    }

    #[test]
    fn server_returns_error_for_unknown_method() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(6),
            method: "unknown/method".to_string(),
            params: None,
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::ErrorResponse(er) => {
                assert_eq!(er.error.code, protocol::RpcError::METHOD_NOT_FOUND);
            }
            _ => panic!("expected error response"),
        }
    }

    #[test]
    fn tools_call_changed_returns_results() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(7),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "code_search_changed",
                "arguments": {}
            })),
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let result: ToolCallResult = serde_json::from_value(sr.result).unwrap();
                assert!(!result.is_error);
                let text = &result.content[0].text;
                let parsed: Value = serde_json::from_str(text).unwrap();
                assert_eq!(parsed["ok"], true);
                assert_eq!(parsed["reliability"]["level"], "source_fact");
                assert!(parsed["summary"]["changed"]["changedCount"]
                    .as_u64()
                    .is_some());
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn tools_call_status_returns_snapshot_id() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let req = protocol::Request {
            jsonrpc: "2.0".to_string(),
            id: json!(8),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "code_search_status",
                "arguments": {}
            })),
        };

        let resp = server.handle_request(req).unwrap();
        match resp {
            Envelope::SuccessResponse(sr) => {
                let result: ToolCallResult = serde_json::from_value(sr.result).unwrap();
                assert!(!result.is_error);
                let text = &result.content[0].text;
                let parsed: Value = serde_json::from_str(text).unwrap();
                assert_eq!(parsed["ok"], true);
                let items = parsed["results"].as_array().unwrap();
                assert!(items[0]["snapshot_id"].as_str().is_some());
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn tools_call_list_and_tree_reuse_cli_envelope_contract() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/nested/lib.rs"), "fn helper() {}\n").unwrap();
        fs::write(dir.path().join("src/nested/readme.txt"), "notes\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let list = call_tool_json(
            &server,
            "code_search_list",
            json!({ "dir": "src", "recursive": false }),
        );
        assert_eq!(list["ok"], true);
        assert_eq!(list["command"], "list");
        assert_eq!(list["canonicalCommand"], "list");
        assert!(list["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "src/nested" && entry["kind"] == "directory"));

        let tree = call_tool_json(
            &server,
            "code_search_tree",
            json!({ "dir": "src", "depth": 2 }),
        );
        assert_eq!(tree["ok"], true);
        assert_eq!(tree["command"], "tree");
        assert_eq!(tree["canonicalCommand"], "tree");
        assert!(tree["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "src/nested/lib.rs"));
    }

    #[test]
    fn tools_call_list_and_tree_reject_unsupported_scope_and_bad_depth() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "fn helper() {}\n").unwrap();
        fs::write(dir.path().join("src/readme.txt"), "notes\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let lang_result = call_tool(
            &server,
            "code_search_list",
            json!({ "dir": "src", "lang": ["rust"] }),
        );
        assert!(lang_result.is_error);
        let lang_error: Value = serde_json::from_str(&lang_result.content[0].text).unwrap();
        assert_eq!(lang_error["ok"], false);
        assert_eq!(lang_error["error"]["code"], "unsupported_mcp_scope");

        let changed_result = call_tool(
            &server,
            "code_search_tree",
            json!({ "dir": "src", "changed": true }),
        );
        assert!(changed_result.is_error);
        let changed_error: Value = serde_json::from_str(&changed_result.content[0].text).unwrap();
        assert_eq!(changed_error["error"]["code"], "unsupported_mcp_scope");

        let depth_result = call_tool(
            &server,
            "code_search_tree",
            json!({ "dir": "src", "depth": 256 }),
        );
        assert!(depth_result.is_error);
        let depth_error: Value = serde_json::from_str(&depth_result.content[0].text).unwrap();
        assert_eq!(depth_error["ok"], false);
        assert_eq!(depth_error["error"]["code"], "invalid_mcp_argument");

        for invalid_depth in [json!(-1), json!(1.5)] {
            let invalid_result = call_tool(
                &server,
                "code_search_tree",
                json!({ "dir": "src", "depth": invalid_depth }),
            );
            assert!(invalid_result.is_error);
            let invalid_error: Value =
                serde_json::from_str(&invalid_result.content[0].text).unwrap();
            assert_eq!(invalid_error["error"]["code"], "invalid_mcp_argument");
        }
    }

    #[test]
    fn tools_call_invalid_regex_returns_tool_error_envelope() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "hello\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        let result = call_tool(&server, "code_search_grep", json!({ "pattern": "[" }));
        assert!(result.is_error);
        let parsed: Value = serde_json::from_str(&result.content[0].text).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_ne!(parsed["error"]["code"], "no_match");
        assert!(parsed["error"]["code"].as_str().is_some());
    }

    #[test]
    fn tools_call_find_rejects_invalid_context_values() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("sample.txt"), "needle\n").unwrap();
        let server = Server::new(dir.path()).unwrap();

        for invalid_context in [json!(65536), json!(-1), json!(1.5)] {
            let result = call_tool(
                &server,
                "code_search_find",
                json!({ "text": "needle", "context": invalid_context }),
            );
            assert!(result.is_error);
            let parsed: Value = serde_json::from_str(&result.content[0].text).unwrap();
            assert_eq!(parsed["ok"], false);
            assert_eq!(parsed["error"]["code"], "invalid_mcp_argument");
        }
    }

    #[test]
    fn tools_call_find_to_read_flow_returns_verifiable_source_range() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    let needle = 42;\n}\n",
        )
        .unwrap();
        let server = Server::new(dir.path()).unwrap();

        let found = call_tool_json(
            &server,
            "code_search_find",
            json!({ "text": "needle", "context": 1 }),
        );
        assert_eq!(found["ok"], true);
        assert_eq!(found["results"][0]["path"], "src/main.rs");
        assert!(found["nextActions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "read"));
        let target = found["results"][0]["readCommandArgv"][4]
            .as_str()
            .unwrap()
            .to_string();

        let read = call_tool_json(&server, "code_search_read", json!({ "target": target }));
        assert_eq!(read["ok"], true);
        assert!(read["results"][0]["content"]
            .as_str()
            .unwrap()
            .contains("needle"));
    }

    #[test]
    fn tools_call_broad_query_uses_guarded_cli_contract() {
        let dir = tempdir().unwrap();
        for idx in 0..8 {
            fs::write(
                dir.path().join(format!("file{idx}.java")),
                "public class Sample {}\n",
            )
            .unwrap();
        }
        let server = Server::new(dir.path()).unwrap();

        let found = call_tool_json(&server, "code_search_find", json!({ "text": "public" }));
        assert_eq!(found["guard"]["triggered"], true);
        assert_eq!(found["guard"]["reason"], "broad_literal_pattern");
        assert!(found["results"].as_array().unwrap().len() <= 5);
        assert!(found["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning["code"] == "broad_query_guard_triggered"));
        assert!(found["guard"]["nextActions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "allow_broad"));
    }

    // ------------------------------------------------------------------
    //  CLI integration test  (E2E via process)
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    //  Argument parsing tests
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    //  Argument parsing tests
    // ------------------------------------------------------------------

    #[test]
    fn parse_query_options_extracts_include_exclude() {
        let args = json!({
            "include": ["src", "lib"],
            "exclude": ["test"],
            "lang": ["rust"],
            "changed": true,
            "cursor": "v1:abc:10",
            "allowBroad": true,
            "limit": 50,
            "context": 3
        });
        let opts = parse_query_options(Some(&args)).unwrap();
        assert_eq!(opts.include, vec!["src", "lib"]);
        assert_eq!(opts.exclude, vec!["test"]);
        assert_eq!(opts.lang, vec!["rust"]);
        assert!(opts.changed);
        assert_eq!(opts.cursor.as_deref(), Some("v1:abc:10"));
        assert!(opts.allow_broad);
        assert_eq!(opts.limit, 50);
        assert_eq!(opts.context, 3);
    }

    #[test]
    fn parse_query_options_uses_defaults_when_missing() {
        let args = json!({});
        let opts = parse_query_options(Some(&args)).unwrap();
        assert_eq!(opts.limit, 100);
        assert_eq!(opts.context, 0);
        assert!(opts.include.is_empty());
    }

    #[test]
    fn parse_query_options_rejects_invalid_numeric_values() {
        for args in [
            json!({ "context": 65536 }),
            json!({ "context": -1 }),
            json!({ "context": 1.5 }),
            json!({ "limit": -1 }),
            json!({ "limit": 1.5 }),
        ] {
            let error = parse_query_options(Some(&args)).unwrap_err();
            assert!(error.to_string().starts_with("invalid_mcp_argument:"));
        }
    }
}
