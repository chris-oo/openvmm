// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stdio transport for the MCP server.
//!
//! Reads newline-delimited JSON-RPC messages from stdin on a dedicated thread
//! and sends them via a mesh channel. Responses are written to stdout.

use crate::protocol::JsonRpcMessage;
use std::io::BufRead;
use std::io::Write;

/// Spawn a background thread that reads JSON-RPC messages from stdin and
/// forwards them through the returned receiver.
///
/// The thread exits when stdin reaches EOF or encounters an error. The
/// receiver will be closed in that case.
pub fn spawn_stdin_reader() -> mesh::Receiver<String> {
    let (tx, rx) = mesh::channel::<String>();
    std::thread::Builder::new()
        .name("mcp-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let reader = stdin.lock();
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        tx.send(trimmed);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "stdin read error");
                        break;
                    }
                }
            }
            tracing::debug!("stdin reader thread exiting");
        })
        .expect("failed to spawn stdin reader thread");
    rx
}

/// Parse a raw JSON line into a `JsonRpcMessage`.
pub fn parse_message(line: &str) -> Result<JsonRpcMessage, serde_json::Error> {
    serde_json::from_str(line)
}

/// Writer that sends JSON-RPC responses to stdout.
///
/// All MCP protocol output goes to stdout as newline-delimited JSON.
/// Diagnostic/log output must go to stderr.
pub struct StdoutWriter {
    out: std::io::Stdout,
}

impl StdoutWriter {
    pub fn new() -> Self {
        Self {
            out: std::io::stdout(),
        }
    }

    /// Serialize `value` as a single JSON line to stdout.
    pub fn send(&mut self, value: &impl serde::Serialize) -> anyhow::Result<()> {
        let json = serde_json::to_string(value)?;
        let mut out = self.out.lock();
        out.write_all(json.as_bytes())?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(())
    }
}
