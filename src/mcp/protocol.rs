//! JSON-RPC 2.0 message types for the MCP (Model Context Protocol) adapter.
//!
//! The MCP adapter communicates over stdio using JSON-RPC 2.0.  This
//! module defines the wire-format types: [`Request`], [`Response`],
//! [`Notification`], and the JSON-RPC envelope [`Envelope`].
//!
//! All types are serialized / deserialized via `serde_json`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

/// Top-level JSON-RPC 2.0 message — either a request, response, or notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Envelope {
    Request(Request),
    SuccessResponse(SuccessResponse),
    ErrorResponse(ErrorResponse),
    Notification(Notification),
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request (client → server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

// ---------------------------------------------------------------------------
// Success response
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 success response (server → client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub result: Value,
}

// ---------------------------------------------------------------------------
// Error response
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 error response (server → client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub error: RpcError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// Standard JSON-RPC error codes.
#[allow(dead_code)]
impl RpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    pub fn method_not_found(id: Value) -> ErrorResponse {
        ErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: RpcError {
                code: Self::METHOD_NOT_FOUND,
                message: "Method not found".to_string(),
                data: None,
            },
        }
    }

    pub fn invalid_params(id: Value, message: impl Into<String>) -> ErrorResponse {
        ErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: RpcError {
                code: Self::INVALID_PARAMS,
                message: message.into(),
                data: None,
            },
        }
    }

    pub fn internal_error(id: Value, message: impl Into<String>) -> ErrorResponse {
        ErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: RpcError {
                code: Self::INTERNAL_ERROR,
                message: message.into(),
                data: None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Notification  (no id — server does not reply)
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 notification (no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP-specific message payloads
// ---------------------------------------------------------------------------

// -- initialize ---------------------------------------------------------

/// Content of the `result` for an `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: Value,
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// -- tools/list ---------------------------------------------------------

/// `tools/list` response result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDef>,
}

/// Definition of a single MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

// -- tools/call ---------------------------------------------------------

/// Content of `params` for a `tools/call` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// `tools/call` response result.  The `content` field is an array of
/// [`ToolResultContent`] items.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    pub content: Vec<ToolResultContent>,
    #[serde(default)]
    pub is_error: bool,
}

/// MCP text content item within a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a success [`Response`] with the given id and result.
pub fn ok_response(id: Value, result: Value) -> Envelope {
    Envelope::SuccessResponse(SuccessResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result,
    })
}

/// Check whether the id is `null` (JSON null → notification, no response).
pub fn is_notification(id: &Value) -> bool {
    id.is_null()
}
