// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MCP server event loop.
//!
//! Reads JSON-RPC messages from stdin, dispatches tool calls, and writes
//! responses to stdout. Also monitors VM halt notifications.

use crate::protocol::InitializeParams;
use crate::protocol::InitializeResult;
use crate::protocol::JsonRpcErrorResponse;
use crate::protocol::JsonRpcMessage;
use crate::protocol::JsonRpcResponse;
use crate::protocol::MCP_PROTOCOL_VERSION;
use crate::protocol::METHOD_NOT_FOUND;
use crate::protocol::ServerCapabilities;
use crate::protocol::ServerInfo;
use crate::protocol::ToolCallParams;
use crate::protocol::ToolResult;
use crate::protocol::ToolsCapability;
use crate::protocol::ToolsListResult;
use crate::tools::ToolRegistry;
use crate::transport;
use crate::transport::StdoutWriter;
use crate::vm_handle::VmHandle;
use futures::StreamExt;
use futures_concurrency::stream::Merge;

/// Events processed by the main event loop.
enum Event {
    /// A raw JSON line arrived from stdin.
    StdinMessage(String),
    /// The VM halted.
    Halt(vmm_core_defs::HaltReason),
    /// stdin closed — time to shut down.
    StdinClosed,
}

/// Run the MCP server until stdin closes or the VM worker stops.
///
/// This is the top-level entry point. It sets up the transport, tool registry,
/// and enters the event loop.
pub async fn run_mcp_server(
    vm_handle: VmHandle,
    halt_recv: mesh::Receiver<vmm_core_defs::HaltReason>,
) -> anyhow::Result<()> {
    let stdin_rx = transport::spawn_stdin_reader();
    let mut stdout = StdoutWriter::new();
    let registry = ToolRegistry::new();

    // Map streams into the Event enum, matching the pattern used in run_control.
    let mut stdin_stream = stdin_rx
        .map(Event::StdinMessage)
        .chain(futures::stream::repeat_with(|| Event::StdinClosed));
    let mut halt_stream = halt_recv.map(Event::Halt);

    let mut initialized = false;

    loop {
        let event = (&mut stdin_stream, &mut halt_stream)
            .merge()
            .next()
            .await
            .unwrap();

        match event {
            Event::StdinClosed => {
                tracing::debug!("stdin closed, shutting down MCP server");
                break;
            }
            Event::Halt(reason) => {
                let reason_str = format!("{reason:?}");
                tracing::info!(?reason, "VM halted");
                vm_handle.set_halted(reason_str);
            }
            Event::StdinMessage(line) => {
                let msg = match transport::parse_message(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to parse JSON-RPC message");
                        let resp = JsonRpcErrorResponse::new(
                            serde_json::Value::Null,
                            crate::protocol::PARSE_ERROR,
                            format!("parse error: {e}"),
                        );
                        let _ = stdout.send(&resp);
                        continue;
                    }
                };

                handle_message(msg, &vm_handle, &registry, &mut stdout, &mut initialized).await;
            }
        }
    }

    Ok(())
}

async fn handle_message(
    msg: JsonRpcMessage,
    vm: &VmHandle,
    registry: &ToolRegistry,
    stdout: &mut StdoutWriter,
    initialized: &mut bool,
) {
    // Notifications have no id — we don't send a response.
    let is_notification = msg.id.is_none();
    let id = msg.id.clone().unwrap_or(serde_json::Value::Null);

    match msg.method.as_str() {
        "initialize" => {
            let _params: InitializeParams = match msg
                .params
                .as_ref()
                .map(|p| serde_json::from_value(p.clone()))
                .transpose()
            {
                Ok(p) => p.unwrap_or(InitializeParams {
                    protocol_version: MCP_PROTOCOL_VERSION.to_string(),
                    capabilities: serde_json::Value::Null,
                    client_info: None,
                }),
                Err(e) => {
                    let resp = JsonRpcErrorResponse::new(
                        id,
                        crate::protocol::INVALID_PARAMS,
                        format!("invalid initialize params: {e}"),
                    );
                    let _ = stdout.send(&resp);
                    return;
                }
            };

            let result = InitializeResult {
                protocol_version: MCP_PROTOCOL_VERSION,
                capabilities: ServerCapabilities {
                    tools: ToolsCapability {
                        list_changed: false,
                    },
                },
                server_info: ServerInfo {
                    name: "openvmm-mcp",
                    version: env!("CARGO_PKG_VERSION"),
                },
            };

            let resp = JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap());
            let _ = stdout.send(&resp);
            *initialized = true;
        }
        "notifications/initialized" => {
            // Acknowledgement from client; no response needed.
            tracing::info!("MCP client initialized");
        }
        "tools/list" => {
            let result = ToolsListResult {
                tools: registry.definitions(),
            };
            let resp = JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap());
            let _ = stdout.send(&resp);
        }
        "tools/call" => {
            let params: ToolCallParams =
                match msg.params.map(|p| serde_json::from_value(p)).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        let resp = JsonRpcErrorResponse::new(
                            id,
                            crate::protocol::INVALID_PARAMS,
                            "missing params for tools/call",
                        );
                        let _ = stdout.send(&resp);
                        return;
                    }
                    Err(e) => {
                        let resp = JsonRpcErrorResponse::new(
                            id,
                            crate::protocol::INVALID_PARAMS,
                            format!("invalid tools/call params: {e}"),
                        );
                        let _ = stdout.send(&resp);
                        return;
                    }
                };

            let result = match registry.call(&params.name, vm, params.arguments).await {
                Some(r) => r,
                None => ToolResult::error(format!("unknown tool: {}", params.name)),
            };

            let resp = JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap());
            let _ = stdout.send(&resp);
        }
        _ => {
            if !is_notification {
                let resp = JsonRpcErrorResponse::new(
                    id,
                    METHOD_NOT_FOUND,
                    format!("unknown method: {}", msg.method),
                );
                let _ = stdout.send(&resp);
            }
        }
    }
}
