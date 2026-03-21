// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM lifecycle tools: pause, resume, reset, NMI, clear-halt, status.

use crate::protocol::ToolDefinition;
use crate::protocol::ToolResult;
use crate::vm_handle::VmHandle;
use std::future::Future;
use std::pin::Pin;

// VmRpc variants are used by VmHandle methods; no direct import needed here.

type Handler = for<'a> fn(
    &'a VmHandle,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

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
    ]
}

fn handle_pause<'a>(
    vm: &'a VmHandle,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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

fn handle_resume<'a>(
    vm: &'a VmHandle,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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

fn handle_reset<'a>(
    vm: &'a VmHandle,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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

fn handle_nmi<'a>(
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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

fn handle_clear_halt<'a>(
    vm: &'a VmHandle,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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

fn handle_status<'a>(
    vm: &'a VmHandle,
    _args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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
