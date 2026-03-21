// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM lifecycle tools: pause, resume, reset, NMI, clear-halt, status,
//! wait-for-halt.

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

fn empty_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

/// Return all lifecycle tool definitions and handlers.
pub fn tools() -> Vec<(ToolDefinition, Handler)> {
    vec![
        (
            ToolDefinition {
                name: "vm/pause".into(),
                description: "Pause the virtual machine. Returns whether the state changed."
                    .into(),
                input_schema: empty_schema(),
            },
            handle_pause as Handler,
        ),
        (
            ToolDefinition {
                name: "vm/resume".into(),
                description: "Resume a paused virtual machine. Returns whether the state changed."
                    .into(),
                input_schema: empty_schema(),
            },
            handle_resume,
        ),
        (
            ToolDefinition {
                name: "vm/reset".into(),
                description: "Reset the virtual machine.".into(),
                input_schema: empty_schema(),
            },
            handle_reset,
        ),
        (
            ToolDefinition {
                name: "vm/nmi".into(),
                description: "Send a Non-Maskable Interrupt to a virtual processor.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "vp": {
                            "type": "integer",
                            "description": "Virtual processor index (default: 0)",
                            "default": 0
                        }
                    },
                    "required": []
                }),
            },
            handle_nmi,
        ),
        (
            ToolDefinition {
                name: "vm/clear_halt".into(),
                description:
                    "Clear a halted state so the VM can be resumed. Returns whether the state changed."
                        .into(),
                input_schema: empty_schema(),
            },
            handle_clear_halt,
        ),
        (
            ToolDefinition {
                name: "vm/status".into(),
                description:
                    "Get the current VM status: running, paused, or halted (with reason).".into(),
                input_schema: empty_schema(),
            },
            handle_status,
        ),
        (
            ToolDefinition {
                name: "vm/wait_for_halt".into(),
                description:
                    "Block until the VM halts (shutdown, triple fault, etc.) and return the halt reason. Avoids polling vm/status in a loop."
                        .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Maximum time to wait in milliseconds (default: 300000 = 5 minutes)",
                            "default": 300000
                        }
                    },
                    "required": []
                }),
            },
            handle_wait_for_halt,
        ),
    ]
}

fn handle_pause(
    vm: Arc<VmHandle>,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        match vm.pause().await {
            Ok(changed) => {
                vm.set_paused(true);
                ToolResult::text(
                    serde_json::json!({
                        "paused": true,
                        "state_changed": changed,
                    })
                    .to_string(),
                )
            }
            Err(e) => ToolResult::error(format!("pause failed: {e:#}")),
        }
    })
}

fn handle_resume(
    vm: Arc<VmHandle>,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        match vm.resume().await {
            Ok(changed) => {
                vm.set_paused(false);
                ToolResult::text(
                    serde_json::json!({
                        "resumed": true,
                        "state_changed": changed,
                    })
                    .to_string(),
                )
            }
            Err(e) => ToolResult::error(format!("resume failed: {e:#}")),
        }
    })
}

fn handle_reset(
    vm: Arc<VmHandle>,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        match vm.reset().await {
            Ok(()) => {
                vm.set_paused(false);
                ToolResult::text(r#"{"reset": true}"#.to_string())
            }
            Err(e) => ToolResult::error(format!("reset failed: {e:#}")),
        }
    })
}

fn handle_nmi(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let vp_u64 = args.get("vp").and_then(|v| v.as_u64()).unwrap_or(0);
        if vp_u64 > u32::MAX as u64 {
            return ToolResult::error("vp index out of range");
        }
        let vp = vp_u64 as u32;
        match vm.nmi(vp).await {
            Ok(()) => ToolResult::text(format!(r#"{{"nmi_sent": true, "vp": {vp}}}"#)),
            Err(e) => ToolResult::error(format!("nmi failed: {e:#}")),
        }
    })
}

fn handle_clear_halt(
    vm: Arc<VmHandle>,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        match vm.clear_halt().await {
            Ok(changed) => ToolResult::text(
                serde_json::json!({
                    "halt_cleared": true,
                    "state_changed": changed,
                })
                .to_string(),
            ),
            Err(e) => ToolResult::error(format!("clear_halt failed: {e:#}")),
        }
    })
}

fn handle_status(
    vm: Arc<VmHandle>,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let halted = vm.is_halted();
        let paused = vm.is_paused();
        let reason = vm.halt_reason_string();
        let status = if halted {
            "halted"
        } else if paused {
            "paused"
        } else {
            "running"
        };
        ToolResult::text(
            serde_json::json!({
                "status": status,
                "halt_reason": reason,
            })
            .to_string(),
        )
    })
}

fn handle_wait_for_halt(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        // If already halted, return immediately.
        if vm.is_halted() {
            return ToolResult::text(
                serde_json::json!({
                    "halted": true,
                    "reason": vm.halt_reason_string(),
                })
                .to_string(),
            );
        }

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(300_000);

        // Register a halt waiter — we receive the reason string when the VM
        // halts, sent by VmHandle::set_halted() which the event loop calls
        // when it processes the Halt event.
        let mut halt_rx = vm.register_halt_waiter();

        // Create a timeout channel driven by a sleeping thread.
        let (timeout_tx, mut timeout_rx) = mesh::channel::<()>();
        std::thread::Builder::new()
            .name("mcp-wait-halt-timeout".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms));
                timeout_tx.send(());
            })
            .expect("spawn halt timeout thread");

        // Race halt notification against the timeout.
        use futures::StreamExt;
        let halt_fut = Box::pin(halt_rx.next());
        let timeout_fut = Box::pin(timeout_rx.next());

        match futures::future::select(halt_fut, timeout_fut).await {
            futures::future::Either::Left((Some(reason), _)) => ToolResult::text(
                serde_json::json!({
                    "halted": true,
                    "reason": reason,
                })
                .to_string(),
            ),
            futures::future::Either::Left((None, _)) => {
                ToolResult::error("halt notification channel closed unexpectedly")
            }
            futures::future::Either::Right(_) => {
                // Timeout — check once more in case of a race.
                if vm.is_halted() {
                    ToolResult::text(
                        serde_json::json!({
                            "halted": true,
                            "reason": vm.halt_reason_string(),
                        })
                        .to_string(),
                    )
                } else {
                    ToolResult::text(
                        serde_json::json!({
                            "halted": false,
                            "timed_out": true,
                        })
                        .to_string(),
                    )
                }
            }
        }
    })
}
