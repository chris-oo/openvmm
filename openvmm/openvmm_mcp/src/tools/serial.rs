// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial console tools: read output, write input, execute commands.

use crate::protocol::ToolAnnotations;
use crate::protocol::ToolDefinition;
use crate::protocol::ToolResult;
use crate::vm_handle::VmHandle;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type Handler = fn(
    Arc<VmHandle>,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>>;

/// Default prompt suffixes used by `serial/execute` to detect command
/// completion.
const DEFAULT_PROMPT_SUFFIXES: &[&str] = &["# ", "$ ", "> ", "login: ", "Password: "];

/// Return all serial tool definitions and handlers.
pub fn tools() -> Vec<(ToolDefinition, Handler)> {
    vec![
        (
            ToolDefinition {
                name: "serial/read".into(),
                title: Some("Read Serial Output".into()),
                description: "Read serial console output since the given cursor. Returns new output and an updated cursor for subsequent reads.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cursor": {
                            "type": "integer",
                            "description": "Cursor from a previous read (0 to read all buffered output)",
                            "default": 0
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "Maximum number of bytes to return. When set and more data is available, only the most recent max_bytes are returned (tail behavior). The cursor advances past skipped data. Use this to avoid large responses when polling, e.g. max_bytes=4096."
                        },
                        "raw": {
                            "type": "boolean",
                            "description": "If true, return raw text including ANSI escape sequences. Default: false (escapes stripped).",
                            "default": false
                        }
                    },
                    "required": []
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "cursor": {"type": "integer"},
                        "bytes_read": {"type": "integer"},
                        "truncated": {"type": "boolean"},
                        "bytes_skipped": {"type": "integer"}
                    },
                    "required": ["text", "cursor", "bytes_read", "truncated", "bytes_skipped"]
                })),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(true),
                    open_world_hint: Some(false),
                    ..Default::default()
                }),
            },
            handle_read as Handler,
        ),
        (
            ToolDefinition {
                name: "serial/write".into(),
                title: Some("Write to Serial Console".into()),
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
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "written": {"type": "boolean"},
                        "bytes": {"type": "integer"}
                    },
                    "required": ["written", "bytes"]
                })),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(false),
                    ..Default::default()
                }),
            },
            handle_write,
        ),
        (
            ToolDefinition {
                name: "serial/execute".into(),
                title: Some("Execute Serial Command".into()),
                description: "Write a command to the serial console and wait for the output until a shell prompt appears or a timeout expires. Returns the complete command output in one response. Much more convenient than separate serial/write + serial/read calls.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command to execute (a newline is appended automatically)"
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Maximum time to wait for prompt in milliseconds (default: 30000)",
                            "default": 30000
                        },
                        "prompt_pattern": {
                            "type": "string",
                            "description": "Custom prompt suffix to wait for. Default: detect common prompts (# $ > login: Password:)"
                        }
                    },
                    "required": ["command"]
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "output": {"type": "string"},
                        "cursor": {"type": "integer"},
                        "timed_out": {"type": "boolean"}
                    },
                    "required": ["output", "cursor", "timed_out"]
                })),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(false),
                    ..Default::default()
                }),
            },
            handle_execute,
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

fn handle_read(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let cursor = args.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0);
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);

        let (data, new_cursor) = vm.serial_buffer.read_since(cursor);

        // Apply tail truncation: if max_bytes is set and data exceeds it,
        // return only the most recent max_bytes bytes.
        let (data, bytes_skipped) = match max_bytes {
            Some(max) if data.len() > max => {
                let skip = data.len() - max;
                (data[skip..].to_vec(), skip)
            }
            _ => (data, 0),
        };

        let bytes_read = data.len();
        let text = String::from_utf8_lossy(&data);
        let text = if raw {
            text.into_owned()
        } else {
            strip_ansi_escapes(&text)
        };
        ToolResult::structured(serde_json::json!({
            "text": text,
            "cursor": new_cursor,
            "bytes_read": bytes_read,
            "truncated": bytes_skipped > 0,
            "bytes_skipped": bytes_skipped,
        }))
    })
}

fn handle_write(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
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
            Ok(()) => ToolResult::structured(serde_json::json!({
                "written": true,
                "bytes": text.len(),
            })),
            Err(e) => ToolResult::error(format!("serial write failed: {e}")),
        }
    })
}

/// Check whether the accumulated serial output ends with a prompt.
fn ends_with_prompt(output: &str, custom_pattern: &Option<String>) -> bool {
    // Strip trailing \r which serial consoles may produce.
    let trimmed = output.trim_end_matches('\r');
    if let Some(pattern) = custom_pattern {
        trimmed.ends_with(pattern)
    } else {
        DEFAULT_PROMPT_SUFFIXES
            .iter()
            .any(|suffix| trimmed.ends_with(suffix))
    }
}

fn handle_execute(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return ToolResult::error("missing required parameter: command"),
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);
        let prompt_pattern = args
            .get("prompt_pattern")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Snapshot cursor and write command atomically (under console_in lock)
        // to prevent interleaving with concurrent serial/execute calls.
        let start_cursor;
        {
            let mut guard = vm.console_in.lock();
            let Some(writer) = guard.as_mut() else {
                return ToolResult::error("serial console input not available");
            };
            start_cursor = vm.serial_buffer.cursor();
            use std::io::Write;
            let payload = format!("{command}\n");
            if let Err(e) = writer
                .write_all(payload.as_bytes())
                .and_then(|()| writer.flush())
            {
                return ToolResult::error(format!("serial write failed: {e}"));
            }
        }

        // Spawn a polling thread that watches the ring buffer for a prompt.
        let buffer = vm.serial_buffer.clone();
        let (result_tx, mut result_rx) = mesh::channel::<ToolResult>();

        std::thread::Builder::new()
            .name("mcp-serial-execute".into())
            .spawn(move || {
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                let mut cursor = start_cursor;
                let mut full_output = String::new();

                loop {
                    std::thread::sleep(std::time::Duration::from_millis(100));

                    let (data, new_cursor) = buffer.read_since(cursor);
                    if !data.is_empty() {
                        let text = String::from_utf8_lossy(&data);
                        full_output.push_str(&strip_ansi_escapes(&text));
                        cursor = new_cursor;

                        if ends_with_prompt(&full_output, &prompt_pattern) {
                            result_tx.send(ToolResult::structured(serde_json::json!({
                                "output": full_output,
                                "cursor": cursor,
                                "timed_out": false,
                            })));
                            return;
                        }
                    }

                    if std::time::Instant::now() >= deadline {
                        result_tx.send(ToolResult::structured(serde_json::json!({
                            "output": full_output,
                            "cursor": cursor,
                            "timed_out": true,
                        })));
                        return;
                    }
                }
            })
            .expect("spawn serial-execute thread");

        // Await the result from the polling thread.
        use futures::StreamExt;
        match result_rx.next().await {
            Some(result) => result,
            None => ToolResult::error("serial execute polling thread terminated unexpectedly"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_detection_default_root() {
        assert!(ends_with_prompt("localhost:~# ", &None));
    }

    #[test]
    fn prompt_detection_default_user() {
        assert!(ends_with_prompt("user@host:~$ ", &None));
    }

    #[test]
    fn prompt_detection_default_login() {
        assert!(ends_with_prompt("localhost login: ", &None));
    }

    #[test]
    fn prompt_detection_default_password() {
        assert!(ends_with_prompt("Password: ", &None));
    }

    #[test]
    fn prompt_detection_trailing_cr() {
        assert!(ends_with_prompt("localhost:~# \r", &None));
    }

    #[test]
    fn prompt_detection_no_match() {
        assert!(!ends_with_prompt("still booting...\n", &None));
    }

    #[test]
    fn prompt_detection_custom_pattern() {
        let custom = Some(">>> ".to_string());
        assert!(ends_with_prompt("Python >>> ", &custom));
        assert!(!ends_with_prompt("localhost:~# ", &custom));
    }

    #[test]
    fn ansi_stripping() {
        let input = "\x1b[32mgreen\x1b[0m plain";
        assert_eq!(strip_ansi_escapes(input), "green plain");
    }

    #[test]
    fn ansi_stripping_csi_params() {
        let input = "\x1b[1;31mbold red\x1b[0m";
        assert_eq!(strip_ansi_escapes(input), "bold red");
    }
}
