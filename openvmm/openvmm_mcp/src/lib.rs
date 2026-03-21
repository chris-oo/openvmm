// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MCP (Model Context Protocol) server for OpenVMM.
//!
//! Provides a JSON-RPC 2.0 based MCP interface for AI agents to interact with
//! a running VM through lifecycle management, inspection, and serial console
//! tools.

#![expect(missing_docs)]

pub mod event_loop;
pub mod protocol;
pub mod serial_buffer;
pub mod tools;
pub mod transport;
pub mod vm_handle;

pub use event_loop::run_mcp_server;
pub use vm_handle::VmHandle;
