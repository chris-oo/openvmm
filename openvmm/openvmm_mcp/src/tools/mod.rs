// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MCP tool registry and dispatch.
//!
//! Tools are registered as `(ToolDefinition, handler_fn)` pairs and dispatched
//! by name from the event loop.

pub mod inspect;
pub mod lifecycle;
pub mod serial;

use crate::protocol::ToolDefinition;
use crate::protocol::ToolResult;
use crate::vm_handle::VmHandle;
use std::future::Future;
use std::pin::Pin;

/// Type alias for an async tool handler function.
///
/// Each handler receives a shared reference to the VM handle and the
/// caller-supplied arguments, returning a `ToolResult`.
type ToolHandler = for<'a> fn(
    &'a VmHandle,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Registry of available MCP tools.
pub struct ToolRegistry {
    tools: Vec<(ToolDefinition, ToolHandler)>,
}

impl ToolRegistry {
    /// Build a registry containing all Phase 1 tools.
    pub fn new() -> Self {
        let mut tools = Vec::new();

        // Lifecycle tools
        for (def, handler) in lifecycle::tools() {
            tools.push((def, handler));
        }

        // Inspect tools
        for (def, handler) in inspect::tools() {
            tools.push((def, handler));
        }

        // Serial tools
        for (def, handler) in serial::tools() {
            tools.push((def, handler));
        }

        Self { tools }
    }

    /// Return definitions for all registered tools.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|(def, _)| def.clone()).collect()
    }

    /// Dispatch a tool call by name.
    pub async fn call(
        &self,
        name: &str,
        vm: &VmHandle,
        args: serde_json::Value,
    ) -> Option<ToolResult> {
        for (def, handler) in &self.tools {
            if def.name == name {
                return Some(handler(vm, args).await);
            }
        }
        None
    }
}
