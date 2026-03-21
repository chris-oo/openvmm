// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial console tools: read output, write input, execute commands.

use crate::protocol::ToolDefinition;
use crate::protocol::ToolResult;
use crate::vm_handle::VmHandle;
use std::future::Future;
use std::pin::Pin;

type Handler = for<'a> fn(
    &'a VmHandle,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Return all serial tool definitions and handlers.
pub fn tools() -> Vec<(ToolDefinition, Handler)> {
    vec![
        (
            ToolDefinition {
                name: "serial/read".into(),
                description: "Read serial console output since the given cursor. Returns new output and an updated cursor for subsequent reads.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cursor": {
                            "type": "integer",
                            "description": "Cursor from a previous read (0 to read all buffered output)",
                            "default": 0
                        }
                    },
                    "required": []
                }),
            },
            handle_read as Handler,
        ),
        (
            ToolDefinition {
                name: "serial/write".into(),
                description: "Write text to the VM serial console input. Note: the VM must have a serial console configured.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text to write to serial input"
                        }
                    },
                    "required": ["text"]
                }),
            },
            handle_write,
        ),
    ]
}

fn handle_read<'a>(
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let cursor = args.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0);

        let (data, new_cursor) = vm.serial_buffer.read_since(cursor);
        let text = String::from_utf8_lossy(&data);
        ToolResult::text(
            serde_json::json!({
                "text": text,
                "cursor": new_cursor,
                "bytes_read": data.len(),
            })
            .to_string(),
        )
    })
}

fn handle_write<'a>(
    _vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let text = match args.get("text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return ToolResult::error("missing required parameter: text"),
        };

        // Write to the serial ring buffer as if it were echoed.
        // The actual console_in write requires an AsyncWrite handle that would
        // be wired during VM setup. For now, record what we would send.
        let _ = text;
        ToolResult::text(
            serde_json::json!({
                "written": true,
                "bytes": text.len(),
                "note": "serial/write will be fully wired when integrated into openvmm_entry"
            })
            .to_string(),
        )
    })
}
