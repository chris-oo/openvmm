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
                        },
                        "raw": {
                            "type": "boolean",
                            "description": "If true, return raw text including ANSI escape sequences. Default: false (escapes stripped).",
                            "default": false
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

/// Strip ANSI escape sequences from text.
///
/// Handles:
/// - CSI sequences: `ESC [ <params> <final byte>`
/// - Other ESC sequences: `ESC <intermediates> <final byte>`
fn strip_ansi_escapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            if bytes[i] == b'[' {
                // CSI sequence: ESC [ <0x30-0x3F>* <0x20-0x2F>* <0x40-0x7E>
                i += 1;
                while i < bytes.len() && (0x30..=0x3F).contains(&bytes[i]) {
                    i += 1;
                }
                while i < bytes.len() && (0x20..=0x2F).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() && (0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
            } else {
                // Other ESC sequence: ESC <0x20-0x2F>* <0x30-0x7E>
                while i < bytes.len() && (0x20..=0x2F).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() && (0x30..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle_read<'a>(
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let cursor = args.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0);
        let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);

        let (data, new_cursor) = vm.serial_buffer.read_since(cursor);
        let text = String::from_utf8_lossy(&data);
        let text = if raw {
            text.into_owned()
        } else {
            strip_ansi_escapes(&text)
        };
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
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let text = match args.get("text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return ToolResult::error("missing required parameter: text"),
        };

        let mut guard = vm.console_in.lock();
        let Some(writer) = guard.as_mut() else {
            return ToolResult::error("serial console input not available");
        };

        use std::io::Write;
        match writer
            .write_all(text.as_bytes())
            .and_then(|()| writer.flush())
        {
            Ok(()) => ToolResult::text(
                serde_json::json!({
                    "written": true,
                    "bytes": text.len(),
                })
                .to_string(),
            ),
            Err(e) => ToolResult::error(format!("serial write failed: {e}")),
        }
    })
}
