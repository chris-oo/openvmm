// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Inspect tools: query and update the VM inspect tree.

use crate::protocol::ToolAnnotations;
use crate::protocol::ToolDefinition;
use crate::protocol::ToolResult;
use crate::vm_handle::VmHandle;
use inspect::InspectionBuilder;
use mesh::CancelContext;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

type Handler = fn(
    Arc<VmHandle>,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>>;

/// Annotations for read-only inspect tools.
fn read_only() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        open_world_hint: Some(false),
        ..Default::default()
    })
}

/// Return all inspect tool definitions and handlers.
pub fn tools() -> Vec<(ToolDefinition, Handler)> {
    vec![
        (
            ToolDefinition {
                name: "inspect/tree".into(),
                title: Some("Inspect Tree".into()),
                description:
                    "Query the VM inspect tree at a given path. Returns a JSON representation of the subtree."
                        .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Inspect path (e.g. 'vm' or 'vm/chipset'). Empty string for root.",
                            "default": ""
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Maximum depth to traverse (default: 2)",
                            "default": 2
                        }
                    },
                    "required": []
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "result": {}
                    },
                    "required": ["path", "result"]
                })),
                annotations: read_only(),
            },
            handle_tree as Handler,
        ),
        (
            ToolDefinition {
                name: "inspect/get".into(),
                title: Some("Get Inspect Value".into()),
                description:
                    "Get a single value from the inspect tree at the specified path.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Inspect path to the value"
                        }
                    },
                    "required": ["path"]
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "value": {}
                    },
                    "required": ["path", "value"]
                })),
                annotations: read_only(),
            },
            handle_get,
        ),
        (
            ToolDefinition {
                name: "inspect/update".into(),
                title: Some("Update Inspect Value".into()),
                description: "Update a mutable value in the inspect tree.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Inspect path to the value"
                        },
                        "value": {
                            "type": "string",
                            "description": "New value to set (as a string)"
                        }
                    },
                    "required": ["path", "value"]
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_value": {"type": "string"}
                    },
                    "required": ["path", "old_value"]
                })),
                annotations: Some(ToolAnnotations {
                    read_only_hint: Some(false),
                    idempotent_hint: Some(false),
                    open_world_hint: Some(false),
                    ..Default::default()
                }),
            },
            handle_update,
        ),
    ]
}

fn handle_tree(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let depth = args
            .get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(2)
            .min(10) as usize;

        let vm_ref = vm.clone();
        let mut inspection =
            InspectionBuilder::new(&path)
                .depth(Some(depth))
                .inspect(inspect::adhoc(move |req| {
                    (vm_ref.inspect_fn)(req.defer());
                }));

        let _ = CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .until_cancelled(inspection.resolve())
            .await;

        let node = inspection.results();
        let node_json_str = format!("{}", node.json());
        let result_value = serde_json::from_str::<serde_json::Value>(&node_json_str)
            .unwrap_or(serde_json::Value::Null);
        ToolResult::structured(serde_json::json!({
            "path": path,
            "result": result_value,
        }))
    })
}

fn handle_get(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return ToolResult::error("missing required parameter: path"),
        };

        let vm_ref = vm.clone();
        let mut inspection = InspectionBuilder::new(&path)
            .depth(Some(0))
            .inspect(inspect::adhoc(move |req| {
                (vm_ref.inspect_fn)(req.defer());
            }));

        let _ = CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .until_cancelled(inspection.resolve())
            .await;

        let node = inspection.results();
        let node_json_str = format!("{}", node.json());
        let result_value = serde_json::from_str::<serde_json::Value>(&node_json_str)
            .unwrap_or(serde_json::Value::Null);
        ToolResult::structured(serde_json::json!({
            "path": path,
            "value": result_value,
        }))
    })
}

fn handle_update(
    vm: Arc<VmHandle>,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>> {
    Box::pin(async move {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return ToolResult::error("missing required parameter: path"),
        };
        let value = match args.get("value").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return ToolResult::error("missing required parameter: value"),
        };

        let vm_ref = vm.clone();
        let update = inspect::update(
            &path,
            &value,
            inspect::adhoc(move |req| {
                (vm_ref.inspect_fn)(req.defer());
            }),
        );

        let result = CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .until_cancelled(update)
            .await;

        match result {
            Ok(Ok(v)) => ToolResult::structured(serde_json::json!({
                "path": path,
                "old_value": format!("{v:#}"),
            })),
            Ok(Err(e)) => ToolResult::error(format!("update failed: {e}")),
            Err(_) => ToolResult::error("update timed out"),
        }
    })
}
