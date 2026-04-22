// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MCP server event loop.
//!
//! Reads JSON-RPC messages from stdin, dispatches tool calls, and writes
//! responses to stdout. Also monitors VM controller events (guest halt,
//! worker stopped).
//!
//! Tool calls run concurrently with the event loop via `FuturesUnordered`,
//! allowing long-running tools (like `vm/wait_for_halt` and `serial/execute`)
//! to proceed while events and new requests are still processed.

use crate::VmEvent;
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
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use unicycle::FuturesUnordered;

/// Events processed by the main event loop.
enum Event {
    /// A raw JSON line arrived from stdin.
    StdinMessage(String),
    /// A VM controller event arrived.
    Controller(VmEvent),
    /// stdin closed — time to shut down.
    StdinClosed,
}

/// Run the MCP server until stdin closes or the VM worker stops.
///
/// This is the top-level entry point. It sets up the transport, tool registry,
/// and enters the event loop.
pub async fn run_mcp_server(
    vm_handle: VmHandle,
    controller_events: mesh::Receiver<VmEvent>,
) -> anyhow::Result<()> {
    let vm = Arc::new(vm_handle);
    let stdin_rx = transport::spawn_stdin_reader();
    let mut stdout = StdoutWriter::new();
    let registry = ToolRegistry::new();

    // Map streams into the Event enum, matching the pattern used in run_control.
    let mut stdin_stream = stdin_rx
        .map(Event::StdinMessage)
        .chain(futures::stream::repeat_with(|| Event::StdinClosed));
    let mut controller_stream = controller_events.map(Event::Controller);

    // Pending tool futures that run concurrently with the event loop.
    let mut pending_tools: FuturesUnordered<
        Pin<Box<dyn Future<Output = (serde_json::Value, ToolResult)> + Send>>,
    > = FuturesUnordered::new();

    let mut initialized = false;

    loop {
        // Multiplex: event streams + pending tool completions.
        // When no tools are pending, just poll the event streams.
        // When tools are pending, use select to poll both.
        let event = if pending_tools.is_empty() {
            (&mut stdin_stream, &mut controller_stream)
                .merge()
                .next()
                .await
        } else {
            match futures::future::select(
                Box::pin((&mut stdin_stream, &mut controller_stream).merge().next()),
                Box::pin(pending_tools.next()),
            )
            .await
            {
                futures::future::Either::Left((event, _)) => event,
                futures::future::Either::Right((Some((id, result)), _)) => {
                    let resp = JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap());
                    let _ = stdout.send(&resp);
                    continue;
                }
                futures::future::Either::Right((None, _)) => {
                    continue;
                }
            }
        };

        let Some(event) = event else { break };

        match event {
            Event::StdinClosed => {
                tracing::debug!("stdin closed, shutting down MCP server");
                break;
            }
            Event::Controller(vm_event) => match vm_event {
                VmEvent::GuestHalt(reason) => {
                    tracing::info!(%reason, "VM halted");
                    vm.set_halted(reason);
                }
                VmEvent::WorkerStopped { error } => {
                    if let Some(err) = &error {
                        tracing::error!(error = %err, "VM worker stopped with error");
                    } else {
                        tracing::info!("VM worker stopped");
                    }
                    vm.set_worker_stopped(error);
                    break;
                }
            },
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

                handle_message(
                    msg,
                    &vm,
                    &registry,
                    &mut stdout,
                    &mut initialized,
                    &mut pending_tools,
                )
                .await;
            }
        }
    }

    Ok(())
}

async fn handle_message(
    msg: JsonRpcMessage,
    vm: &Arc<VmHandle>,
    registry: &ToolRegistry,
    stdout: &mut StdoutWriter,
    initialized: &mut bool,
    pending_tools: &mut FuturesUnordered<
        Pin<Box<dyn Future<Output = (serde_json::Value, ToolResult)> + Send>>,
    >,
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
        _ if !*initialized => {
            // Reject any method other than initialize/notifications before handshake.
            if !is_notification {
                let resp = JsonRpcErrorResponse::new(
                    id,
                    crate::protocol::INVALID_REQUEST,
                    "server not initialized — send 'initialize' first",
                );
                let _ = stdout.send(&resp);
            }
        }
        "tools/list" => {
            let result = ToolsListResult {
                tools: registry.definitions(),
            };
            let resp = JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap());
            let _ = stdout.send(&resp);
        }
        "tools/call" => {
            let params: ToolCallParams = match msg.params.map(serde_json::from_value).transpose() {
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

            match registry.call(&params.name, vm.clone(), params.arguments) {
                Some(future) => {
                    // Spawn the tool call as a concurrent future so the event
                    // loop continues processing halt events and other requests.
                    pending_tools.push(Box::pin(async move { (id, future.await) }));
                }
                None => {
                    let result = ToolResult::error(format!("unknown tool: {}", params.name));
                    let resp = JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap());
                    let _ = stdout.send(&resp);
                }
            }
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
