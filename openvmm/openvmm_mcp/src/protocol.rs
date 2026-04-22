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
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Annotations describing tool behavior, as defined in MCP 2025-06-18.
///
/// All fields are optional hints. Clients should not rely on these for
/// security decisions from untrusted servers.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// If true, the tool does not modify its environment. Default: false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// If true, the tool may perform destructive updates. Default: true.
    /// Only meaningful when `read_only_hint` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// If true, calling repeatedly with same args has no additional effect.
    /// Default: false. Only meaningful when `read_only_hint` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// If true, the tool may interact with external entities. Default: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
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
    /// Structured output as a JSON object (MCP 2025-06-18).
    /// For backwards compatibility, the serialized JSON is also in `content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
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
    /// Create a successful text result (unstructured).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::Text { text: text.into() }],
            is_error: false,
            structured_content: None,
        }
    }

    /// Create a result with both structured content and a text fallback.
    /// The value MUST be a JSON object (MCP spec requirement).
    pub fn structured(value: serde_json::Value) -> Self {
        assert!(value.is_object(), "structuredContent must be a JSON object");
        let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
        Self {
            content: vec![ContentItem::Text { text }],
            is_error: false,
            structured_content: Some(value),
        }
    }

    /// Create an error text result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::Text { text: text.into() }],
            is_error: true,
            structured_content: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn tool_annotations_serializes_camel_case() {
        let ann = ToolAnnotations {
            read_only_hint: Some(true),
            open_world_hint: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_value(&ann).unwrap();
        assert_eq!(json["readOnlyHint"], true);
        assert_eq!(json["openWorldHint"], false);
        assert!(json.get("destructiveHint").is_none());
        assert!(json.get("idempotentHint").is_none());
    }

    #[test]
    fn tool_result_structured_has_both_fields() {
        let value = serde_json::json!({"status": "running", "paused": false});
        let result = ToolResult::structured(value.clone());
        let json = serde_json::to_value(&result).unwrap();

        assert_eq!(json["structuredContent"], value);

        let text = json["content"][0]["text"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed, value);

        assert!(json.get("isError").is_none());
    }

    #[test]
    fn tool_result_text_has_no_structured_content() {
        let result = ToolResult::text("hello");
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("structuredContent").is_none());
        assert_eq!(json["content"][0]["text"], "hello");
    }

    #[test]
    fn tool_result_error_has_no_structured_content() {
        let result = ToolResult::error("something broke");
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("structuredContent").is_none());
        assert_eq!(json["isError"], true);
    }

    #[test]
    #[should_panic(expected = "structuredContent must be a JSON object")]
    fn tool_result_structured_rejects_non_object() {
        ToolResult::structured(serde_json::json!("just a string"));
    }

    #[test]
    fn tool_definition_serializes_all_fields() {
        let def = ToolDefinition {
            name: "vm/status".into(),
            title: Some("Get VM Status".into()),
            description: "Get status".into(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {"status": {"type": "string"}}
            })),
            annotations: Some(ToolAnnotations {
                read_only_hint: Some(true),
                ..Default::default()
            }),
        };
        let json = serde_json::to_value(&def).unwrap();

        assert_eq!(json["name"], "vm/status");
        assert_eq!(json["title"], "Get VM Status");
        assert!(json["outputSchema"].is_object());
        assert_eq!(json["annotations"]["readOnlyHint"], true);
        assert!(json.get("input_schema").is_none());
        assert!(json.get("inputSchema").is_some());
    }

    #[test]
    fn tool_definition_omits_none_fields() {
        let def = ToolDefinition {
            name: "test".into(),
            title: None,
            description: "desc".into(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            annotations: None,
        };
        let json = serde_json::to_value(&def).unwrap();

        assert!(json.get("title").is_none());
        assert!(json.get("outputSchema").is_none());
        assert!(json.get("annotations").is_none());
    }
}
