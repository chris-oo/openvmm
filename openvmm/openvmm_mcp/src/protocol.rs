// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MCP JSON-RPC 2.0 protocol types.
//!
//! Hand-rolled types implementing the Model Context Protocol over JSON-RPC 2.0.
//! The protocol flow is:
//! 1. Client sends `initialize` → server responds with capabilities
//! 2. Client sends `notifications/initialized`
//! 3. Client sends `tools/list` → server responds with tool definitions
//! 4. Client sends `tools/call` → server responds with tool result

use serde::Deserialize;
use serde::Serialize;

/// The JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// The MCP protocol version we advertise.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ── Incoming messages ──

/// A JSON-RPC 2.0 message received from the client.
///
/// This is either a request (has `id`) or a notification (no `id`).
#[derive(Debug, Deserialize)]
pub struct JsonRpcMessage {
    /// Must be "2.0".
    pub jsonrpc: String,
    /// Request ID. Absent for notifications.
    pub id: Option<serde_json::Value>,
    /// Method name.
    pub method: String,
    /// Parameters (optional).
    pub params: Option<serde_json::Value>,
}

// ── Outgoing messages ──

/// A successful JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Serialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    pub error: JsonRpcError,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// Standard JSON-RPC error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

// ── MCP Initialize ──

/// Parameters for `initialize` request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The MCP protocol version the client supports.
    pub protocol_version: String,
    /// Client capabilities.
    #[serde(default)]
    pub capabilities: serde_json::Value,
    /// Information about the client.
    #[serde(default)]
    pub client_info: Option<ClientInfo>,
}

/// Client identification.
#[derive(Debug, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Result of the `initialize` request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: &'static str,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

/// Server identification.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// Capabilities the server advertises.
#[derive(Debug, Serialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
}

/// Indicates the server supports tools.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    pub list_changed: bool,
}

// ── Tools ──

/// A tool definition returned in `tools/list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Parameters for `tools/call`.
#[derive(Debug, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Result of a `tools/call` containing content items.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<ContentItem>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// A content block inside a tool result.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentItem {
    #[serde(rename = "text")]
    Text { text: String },
}

// ── Tools list ──

/// Result of `tools/list`.
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDefinition>,
}

// ── Helpers ──

impl JsonRpcResponse {
    /// Create a success response.
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result,
        }
    }
}

impl JsonRpcErrorResponse {
    /// Create an error response.
    pub fn new(id: serde_json::Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            error: JsonRpcError {
                code,
                message: message.into(),
                data: None,
            },
        }
    }
}

impl ToolResult {
    /// Create a successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::Text { text: text.into() }],
            is_error: false,
        }
    }

    /// Create an error text result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::Text { text: text.into() }],
            is_error: true,
        }
    }
}
