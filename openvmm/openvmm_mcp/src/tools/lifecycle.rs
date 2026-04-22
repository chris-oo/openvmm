// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM lifecycle tools: pause, resume, reset, NMI, clear-halt, status,
//! wait-for-halt.

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

fn empty_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

/// Annotations for read-only tools.
fn read_only() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        open_world_hint: Some(false),
        ..Default::default()
    })
}

/// Annotations for mutation tools that are idempotent.
fn mutation_idempotent() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
        ..Default::default()
    })
}

/// Return all lifecycle tool definitions and handlers.
pub fn tools() -> Vec<(ToolDefinition, Handler)> {
    vec![
        (
            ToolDefinition {
                name: "vm/pause".into(),
                title: Some("Pause VM".into()),
                description: "Pause the virtual machine. Returns whether the state changed."
                    .into(),
                input_schema: empty_schema(),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paused": {"type": "boolean"},
                        "state_changed": {"type": "boolean"}
                    },
                    "required": ["paused", "state_changed"]
                })),
                annotations: mutation_idempotent(),
            },
            handle_pause as Handler,
        ),
        (
            ToolDefinition {
                name: "vm/resume".into(),
                title: Some("Resume VM".into()),
                description: "Resume a paused virtual machine. Returns whether the state changed."
                    .into(),
                input_schema: empty_schema(),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "resumed": {"type": "boolean"},
                        "state_changed": {"type": "boolean"}
                    },
                    "required": ["resumed", "state_changed"]
                })),
                annotations: mutation_idempotent(),
            },
            handle_resume,
        ),
        (
            ToolDefinition {
                name: "vm/reset".into(),
                title: Some("Reset VM".into()),
                description: "Reset the virtual machine.".into(),
                input_schema: empty_schema(),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "reset": {"type": "boolean"}
                    },
                    "required": ["reset"]
                })),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    destructive_hint: Some(true),
                    idempotent_hint: Some(false),
                    open_world_hint: Some(false),
                }),
            },
            handle_reset,
        ),
        (
            ToolDefinition {
                name: "vm/nmi".into(),
                title: Some("Inject NMI".into()),
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
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "nmi_sent": {"type": "boolean"},
                        "vp": {"type": "integer"}
                    },
                    "required": ["nmi_sent", "vp"]
                })),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(false),
                    open_world_hint: Some(false),
                    ..Default::default()
                }),
            },
            handle_nmi,
        ),
        (
            ToolDefinition {
                name: "vm/clear_halt".into(),
                title: Some("Clear VM Halt".into()),
                description:
                    "Clear a halted state so the VM can be resumed. Returns whether the state changed."
                        .into(),
                input_schema: empty_schema(),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "halt_cleared": {"type": "boolean"},
                        "state_changed": {"type": "boolean"}
                    },
                    "required": ["halt_cleared", "state_changed"]
                })),
                annotations: mutation_idempotent(),
            },
            handle_clear_halt,
        ),
        (
            ToolDefinition {
                name: "vm/status".into(),
                title: Some("Get VM Status".into()),
                description:
                    "Get the current VM status: running, paused, or halted (with reason).".into(),
                input_schema: empty_schema(),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "status": {"type": "string"},
                        "halt_reason": {"type": "string"}
                    },
                    "required": ["status"]
                })),
                annotations: read_only(),
            },
            handle_status,
        ),
        (
            ToolDefinition {
                name: "vm/wait_for_halt".into(),
                title: Some("Wait for VM Halt".into()),
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
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "halted": {"type": "boolean"},
                        "reason": {"type": "string"},
                        "timed_out": {"type": "boolean"}
                    },
                    "required": ["halted"]
                })),
                annotations: read_only(),
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
                ToolResult::structured(serde_json::json!({
                    "paused": true,
                    "state_changed": changed,
                }))
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
                ToolResult::structured(serde_json::json!({
                    "resumed": true,
                    "state_changed": changed,
                }))
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
                ToolResult::structured(serde_json::json!({"reset": true}))
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
            Ok(()) => ToolResult::structured(serde_json::json!({"nmi_sent": true, "vp": vp})),
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
            Ok(changed) => ToolResult::structured(serde_json::json!({
                "halt_cleared": true,
                "state_changed": changed,
            })),
            Err(e) => ToolResult::error(format!("clear_halt failed: {e:#}")),
        }
    })
}

fn handle_status(
    vm: Arc<VmHandle>,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let status = vm.status_string();
        let reason = vm.halt_reason_string();
        ToolResult::structured(serde_json::json!({
            "status": status,
            "halt_reason": reason,
        }))
    })
}

fn handle_wait_for_halt(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(300_000);

        // Atomically check halt/worker state and register a waiter under a
        // single lock, preventing lost-wakeup races.
        let mut halt_rx = match vm.register_halt_waiter() {
            crate::vm_handle::HaltWaiterResult::AlreadyHalted(reason) => {
                return ToolResult::structured(serde_json::json!({
                    "halted": true,
                    "reason": reason,
                }));
            }
            crate::vm_handle::HaltWaiterResult::Registered(rx) => rx,
        };

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
            futures::future::Either::Left((Some(reason), _)) => {
                ToolResult::structured(serde_json::json!({
                    "halted": true,
                    "reason": reason,
                }))
            }
            futures::future::Either::Left((None, _)) => {
                ToolResult::error("halt notification channel closed unexpectedly")
            }
            futures::future::Either::Right(_) => {
                // Timeout — check once more in case of a race.
                if vm.is_halted() {
                    ToolResult::structured(serde_json::json!({
                        "halted": true,
                        "reason": vm.halt_reason_string(),
                    }))
                } else {
                    ToolResult::structured(serde_json::json!({
                        "halted": false,
                        "timed_out": true,
                    }))
                }
            }
        }
    })
}
