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
use std::sync::Arc;

/// Type alias for an async tool handler function.
///
/// Each handler receives a cloned `Arc<VmHandle>` and the caller-supplied
/// arguments, returning a `'static` future so it can run concurrently in the
/// event loop's `FuturesUnordered`.
type ToolHandler = fn(
    Arc<VmHandle>,
    serde_json::Value,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>>;

/// Registry of available MCP tools.
pub struct ToolRegistry {
    tools: Vec<(ToolDefinition, ToolHandler)>,
}

impl ToolRegistry {
    /// Build a registry containing all tools.
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
    ///
    /// Returns a `'static` future that can be placed into `FuturesUnordered`
    /// for concurrent execution.
    pub fn call(
        &self,
        name: &str,
        vm: Arc<VmHandle>,
        args: serde_json::Value,
    ) -> Option<Pin<Box<dyn Future<Output = ToolResult> + Send + 'static>>> {
        for (def, handler) in &self.tools {
            if def.name == name {
                return Some(handler(vm, args));
            }
        }
        None
    }
}
