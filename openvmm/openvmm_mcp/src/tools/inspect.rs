// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Inspect tools: query and update the VM inspect tree.

use crate::protocol::ToolDefinition;
use crate::protocol::ToolResult;
use crate::vm_handle::VmHandle;
use inspect::InspectionBuilder;
use mesh::CancelContext;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

type Handler = for<'a> fn(
    &'a VmHandle,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Return all inspect tool definitions and handlers.
pub fn tools() -> Vec<(ToolDefinition, Handler)> {
    vec![
        (
            ToolDefinition {
                name: "inspect/tree".into(),
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
            },
            handle_tree as Handler,
        ),
        (
            ToolDefinition {
                name: "inspect/get".into(),
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
            },
            handle_get,
        ),
        (
            ToolDefinition {
                name: "inspect/update".into(),
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
            },
            handle_update,
        ),
    ]
}

fn handle_tree<'a>(
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
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

        let mut inspection = InspectionBuilder::new(&path)
            .depth(Some(depth))
            .inspect(&vm.worker);

        let _ = CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .until_cancelled(inspection.resolve())
            .await;

        let node = inspection.results();
        ToolResult::text(format!("{}", node.json()))
    })
}

fn handle_get<'a>(
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return ToolResult::error("missing required parameter: path"),
        };

        let mut inspection = InspectionBuilder::new(&path)
            .depth(Some(0))
            .inspect(&vm.worker);

        let _ = CancelContext::new()
            .with_timeout(Duration::from_secs(1))
            .until_cancelled(inspection.resolve())
            .await;

        let node = inspection.results();
        ToolResult::text(format!("{}", node.json()))
    })
}

fn handle_update<'a>(
    vm: &'a VmHandle,
    args: serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return ToolResult::error("missing required parameter: path"),
        };
        let value = match args.get("value").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return ToolResult::error("missing required parameter: value"),
        };

        let result = InspectionBuilder::new(&path)
            .update(&value, &vm.worker)
            .await;

        match result {
            Ok(v) => ToolResult::text(format!("{v:#}")),
            Err(e) => ToolResult::error(format!("update failed: {e}")),
        }
    })
}
