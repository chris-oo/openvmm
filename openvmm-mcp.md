# OpenVMM MCP Server — Implementation Plan

## 1. Executive Summary

We propose adding **Model Context Protocol (MCP) server** support to OpenVMM, enabling AI agents (LLMs operating through tool-calling) to configure, launch, inspect, debug, and interact with virtual machines programmatically. The MCP server exposes the same capabilities available today through OpenVMM's interactive CLI console and petri test framework — VM lifecycle management, the inspect tree, GDB-stub debugging, serial console I/O, framebuffer screenshots, and guest-agent interaction — as structured MCP tools and resources consumable by any MCP-compatible client (Claude Desktop, VS Code Copilot, custom agents, etc.).

**Why MCP?** MCP is emerging as a standard protocol for connecting LLMs to external tools. A VMM is a uniquely valuable tool for AI agents: agents can spin up sandboxed environments, run code, inspect OS internals, debug kernel crashes, and automate integration testing — all without touching the host.

---

## 2. Architecture Decision

### Recommendation: Two-tier architecture — `openvmm-mcp` (embedded) + `petri-mcp` (orchestrator)

After analyzing the codebase, we recommend building **two** MCP servers that target different use cases, sharing a common core of MCP tool definitions.

#### Tier 1: `openvmm-mcp` — Embedded single-VM MCP server

- **What:** A new CLI mode in the OpenVMM binary (`openvmm --mcp`) that replaces the interactive `openvmm>` console with an MCP server communicating over stdio (or SSE/streamable-HTTP).
- **Where it fits:** Parallel to the existing `--ttrpc` / `--grpc` modes which launch a `TtrpcWorker` that manages a single VM via gRPC. The MCP server occupies the same architectural slot: it sits beside the VM worker and drives it via `VmRpc` messages and the `mesh` infrastructure.
- **Use case:** A developer (or AI agent) attaches to a single running VM to inspect state, debug guest issues, interact with serial, read/write memory, manage disks, etc.
- **Scope:** One VM, deep introspection.

#### Tier 2: `petri-mcp` — Orchestration MCP server (standalone binary)

- **What:** A standalone MCP server binary (new workspace member) that wraps the `petri` test framework to expose VM lifecycle orchestration — create VMs from configuration, start/stop/reset them, manage multiple VMs simultaneously.
- **Where it fits:** Uses `petri`'s `PetriVmBuilder`, `PetriVmConfig`, `OpenVmmPetriBackend`, and `PetriVmOpenVmm` to construct and manage VMs. Equivalent to what `vmm_tests` does, but driven by MCP tool calls instead of Rust test functions.
- **Use case:** An AI agent that needs to create VMs on-the-fly, run workloads, tear them down — e.g., automated testing, CI/CD, sandboxed code execution.
- **Scope:** Multi-VM lifecycle management + delegating to per-VM tools.

#### Why both?

| Concern | `openvmm-mcp` only | `petri-mcp` only | Both |
|---|---|---|---|
| Single-VM debugging depth | ✅ | ❌ (petri abstracts away internals) | ✅ |
| VM lifecycle management | ❌ (VM already running) | ✅ | ✅ |
| Incremental build path | ✅ (small surface area) | ❌ (large dependency surface) | ✅ |
| Matches existing patterns | ✅ (like `--ttrpc`) | ✅ (like `vmm_tests`) | ✅ |

#### Build order: Tier 1 first, then Tier 2

Tier 1 (`openvmm-mcp`) can be built with minimal new dependencies and directly mirrors the interactive CLI. Tier 2 (`petri-mcp`) builds on top of Tier 1's tool definitions and adds orchestration.

---

## 3. MCP Tools Inventory

### 3.1 VM Lifecycle Tools (both tiers)

| Tool | Description | Tier 1 | Tier 2 |
|---|---|---|---|
| `vm/pause` | Pause VM execution | ✅ | ✅ |
| `vm/resume` | Resume paused VM | ✅ | ✅ |
| `vm/reset` | Reset VM (power cycle) | ✅ | ✅ |
| `vm/shutdown` | Request guest shutdown (via shutdown IC) | ✅ | ✅ |
| `vm/nmi` | Inject NMI to a VP | ✅ | ✅ |
| `vm/clear_halt` | Clear halt condition | ✅ | ✅ |
| `vm/status` | Get current VM state (running/paused/halted + halt reason) | ✅ | ✅ |

### 3.2 VM Configuration & Creation (Tier 2 only)

| Tool | Description |
|---|---|
| `vm/create` | Create a new VM from configuration (processor count, memory, firmware, disks, NICs) |
| `vm/start` | Start a created VM |
| `vm/destroy` | Stop and tear down a VM |
| `vm/list` | List all managed VMs and their states |
| `vm/configure_disk` | Add/modify disk configuration before start |
| `vm/configure_nic` | Add/modify NIC configuration before start |

### 3.3 Inspect Tools

| Tool | Description | Maps to |
|---|---|---|
| `inspect/tree` | Query the inspect tree at a path with configurable depth | `InteractiveCommand::Inspect` / `InspectionBuilder` |
| `inspect/get` | Get a specific value from the inspect tree | `inspect::inspect()` with `depth(Some(0))` |
| `inspect/update` | Update a mutable inspect value | `inspect::update()` |
| `inspect/list_children` | List child nodes at a path (autocomplete-friendly) | `InspectionBuilder::new().depth(Some(1))` |

The inspect tree is OpenVMM's primary diagnostic interface. Key paths include:
- `vm/` — VM worker internals
- `vm/chipset/` — Chipset device state
- `vm/vmbus/` — VMBus channel state
- `vm/partition/` — VP state, memory maps
- `vm/devices/` — Individual device state
- `vnc/` — VNC worker state
- `gdb/` — GDB worker state

### 3.4 Memory Tools

| Tool | Description | Maps to |
|---|---|---|
| `memory/read` | Read guest physical memory (returns hex dump or base64) | `VmRpc::ReadMemory` |
| `memory/write` | Write guest physical memory | `VmRpc::WriteMemory` |

### 3.5 Serial Console Tools

| Tool | Description |
|---|---|
| `serial/write` | Write data to the VM's serial console (COM1) |
| `serial/read` | Read available data from the serial console output buffer |
| `serial/execute` | Write a command and read output until a prompt/timeout (convenience wrapper) |

**Implementation note:** The existing interactive CLI has a `console_in` (`AsyncWrite`) that writes to the serial port, and a background thread copying serial output to the terminal. For MCP, we'll instead buffer serial output in a ring buffer accessible via `serial/read`.

### 3.6 Framebuffer / Screenshot Tools

| Tool | Description | Maps to |
|---|---|---|
| `display/screenshot` | Capture the current framebuffer as a PNG image | `FramebufferAccess` / `View::read_line()` |
| `display/resolution` | Get current display resolution | `View::resolution()` |

### 3.7 Disk Management Tools

| Tool | Description | Maps to |
|---|---|---|
| `disk/add` | Hot-add a SCSI disk | `ScsiControllerRequest::AddDevice` |
| `disk/remove` | Hot-remove a SCSI disk | `ScsiControllerRequest::RemoveDevice` |
| `disk/add_nvme_ns` | Add NVMe namespace (VTL2) | `NvmeControllerRequest::AddNamespace` |
| `disk/remove_nvme_ns` | Remove NVMe namespace | `NvmeControllerRequest::RemoveNamespace` |

### 3.8 Save/Restore Tools

| Tool | Description | Maps to |
|---|---|---|
| `snapshot/save` | Pause + save VM state to a directory | `save_snapshot()` |
| `snapshot/pulse_save_restore` | Save/restore round-trip for testing | `VmRpc::PulseSaveRestore` |

### 3.9 Guest Agent Tools (Tier 2 with pipette)

| Tool | Description | Maps to |
|---|---|---|
| `guest/execute` | Run a command inside the guest OS | `PipetteClient::command()` |
| `guest/read_file` | Read a file from the guest filesystem | `PipetteClient::read_file()` |
| `guest/write_file` | Write a file to the guest filesystem | `PipetteClient::write_file()` |
| `guest/shell` | Execute a shell command (platform-aware) | `PipetteClient::unix_shell()` / `windows_shell()` |
| `guest/power_off` | Request guest-initiated power off | `PipetteClient::power_off()` |
| `guest/reboot` | Request guest-initiated reboot | `PipetteClient::reboot()` |

### 3.10 GDB Debug Tools

| Tool | Description | Maps to |
|---|---|---|
| `debug/break` | Break into the debugger (pause all VPs) | `DebugRequest::Break` |
| `debug/continue` | Resume execution | `DebugRequest::Resume` |
| `debug/get_registers` | Read VP register state | `DebugRequest::GetVpState` |
| `debug/set_registers` | Write VP register state | `DebugRequest::SetVpState` |
| `debug/read_memory` | Read guest virtual memory (via VP context) | `DebugRequest::ReadMemory` with `GuestAddress::Gva` |
| `debug/read_physical` | Read guest physical memory | `DebugRequest::ReadMemory` with `GuestAddress::Gpa` |
| `debug/write_memory` | Write guest virtual/physical memory | `DebugRequest::WriteMemory` |
| `debug/set_breakpoint` | Set a hardware breakpoint | `DebugRequest::SetDebugState` |
| `debug/clear_breakpoint` | Clear a hardware breakpoint | `DebugRequest::SetDebugState` |
| `debug/single_step` | Single-step a VP | `DebugRequest::SetDebugState` + `Resume` |
| `debug/list_vps` | List virtual processors and their state | Enumerate VP count, read each VP's state |
| `debug/backtrace` | Walk the stack for a VP (heuristic) | Read registers, then walk stack frames via memory reads |

---

## 4. MCP Resources

MCP resources provide read-only contextual data to the LLM.

| Resource URI | Description |
|---|---|
| `vm://config` | Current VM configuration (processor count, memory, devices) |
| `vm://status` | Current VM state (running/paused/halted, halt reason) |
| `vm://inspect/{path}` | Dynamic resource for any inspect tree path |
| `vm://serial/log` | Recent serial console output (ring buffer) |
| `vm://screenshot` | Current framebuffer as PNG (image resource) |
| `vm://vps` | VP states summary (for each VP: running/halted, registers if halted) |
| `vm://halt_reason` | Last halt reason if any |

For Tier 2 (petri-mcp):

| Resource URI | Description |
|---|---|
| `vms://list` | List of all managed VMs |
| `vm://{id}/config` | Configuration for a specific VM |
| `vm://{id}/status` | Status for a specific VM |

---

## 5. Implementation Phases

### Phase 1: Core MCP Infrastructure (Tier 1 foundation)

**Goal:** Establish the MCP server crate, transport layer, and first few tools working with a running OpenVMM instance.

**Steps:**

1. **Create `openvmm/openvmm_mcp/` crate** — New workspace member with `Cargo.toml`, add to `Cargo.toml` workspace members list.
   - Dependencies: `serde`, `serde_json`, `tokio` (or `pal_async`), `mesh`, `openvmm_defs`, `inspect`, `anyhow`.
   - Touches: `Cargo.toml` (workspace), new `openvmm/openvmm_mcp/Cargo.toml`, new `openvmm/openvmm_mcp/src/lib.rs`.

2. **Implement MCP JSON-RPC protocol layer** — Handle `initialize`, `tools/list`, `tools/call`, `resources/list`, `resources/read` over stdio.
   - New files: `openvmm/openvmm_mcp/src/protocol.rs`, `openvmm/openvmm_mcp/src/transport.rs`.

3. **Implement tool registry and dispatch** — Macro or trait-based tool registration that generates `tools/list` responses and dispatches `tools/call`.
   - New files: `openvmm/openvmm_mcp/src/tools/mod.rs`.

4. **Implement `VmHandle` abstraction** — Encapsulates all VM interaction channels.
   - New file: `openvmm/openvmm_mcp/src/vm_handle.rs`.

5. **Implement lifecycle tools** — `vm/pause`, `vm/resume`, `vm/status`, `vm/reset`, `vm/nmi`, `vm/clear_halt`, `vm/shutdown`.
   - New file: `openvmm/openvmm_mcp/src/tools/lifecycle.rs`.

6. **Wire into `openvmm_entry`** — Add `--mcp` CLI flag, create the `VmHandle`, launch MCP server.
   - Touches: `openvmm/openvmm_entry/src/cli_args.rs` (add `--mcp` flag), `openvmm/openvmm_entry/src/lib.rs` (add MCP mode in `do_main()`), `openvmm/openvmm_entry/Cargo.toml` (add `openvmm_mcp` dependency).

7. **Implement `inspect/tree` and `inspect/get` tools**.
   - New file: `openvmm/openvmm_mcp/src/tools/inspect.rs`.

8. **End-to-end validation** — Launch `openvmm --mcp -k <kernel> -r <initrd>` and verify tool calls via a simple MCP client script.

**Estimated scope:** ~1500 LOC new, ~100 LOC modifications to existing files.

### Phase 2: Interaction & Diagnostics Tools

**Goal:** Complete the Tier 1 single-VM tool surface.

**Steps:**

1. **Implement serial console tools** — Add `SerialRingBuffer` type that captures serial output; implement `serial/write`, `serial/read`, `serial/execute`.
   - New file: `openvmm/openvmm_mcp/src/tools/serial.rs`, `openvmm/openvmm_mcp/src/serial_buffer.rs`.
   - Touches: `openvmm/openvmm_entry/src/lib.rs` (wire serial output to ring buffer instead of/in addition to terminal).

2. **Implement `memory/read` and `memory/write`** — Wrap `VmRpc::ReadMemory` / `VmRpc::WriteMemory`, return hex dump or base64.
   - New file: `openvmm/openvmm_mcp/src/tools/memory.rs`.

3. **Implement `display/screenshot`** — Encode `FramebufferAccess` output as PNG, return as base64 MCP image content.
   - New file: `openvmm/openvmm_mcp/src/tools/display.rs`.
   - New dependency: `image` crate (for PNG encoding) or `png` crate.

4. **Implement `disk/add`, `disk/remove`** — Wrap SCSI controller RPC.
   - New file: `openvmm/openvmm_mcp/src/tools/disk.rs`.

5. **Implement `snapshot/save` and `snapshot/pulse_save_restore`**.
   - New file: `openvmm/openvmm_mcp/src/tools/snapshot.rs`.

6. **Implement `inspect/update`**.
   - Touches: `openvmm/openvmm_mcp/src/tools/inspect.rs`.

7. **Implement MCP resources** — `vm://config`, `vm://status`, `vm://serial/log`.
   - New file: `openvmm/openvmm_mcp/src/resources.rs`.

### Phase 3: GDB Debug Integration

**Goal:** Expose AI-driven guest debugging through MCP.

**Steps:**

1. **Wire up debug channel** — When `--mcp` is active, create `mesh::channel()` for `DebugRequest` and store sender in `VmHandle`. Set `vm_config.debugger_rpc = Some(debug_rx)`.
   - Touches: `openvmm/openvmm_entry/src/lib.rs` (MCP mode setup).

2. **Implement `debug/break` and `debug/continue`** — Send `DebugRequest::Break` / `DebugRequest::Resume`, manage the stop-reason oneshot channel.
   - New file: `openvmm/openvmm_mcp/src/tools/debug.rs`.

3. **Implement `debug/get_registers` and `debug/set_registers`** — Call `DebugRequest::GetVpState` / `SetVpState`, convert `DebuggerVpState` to/from JSON-friendly structures with named registers.
   - Touches: `openvmm/openvmm_mcp/src/tools/debug.rs`.

4. **Implement `debug/read_memory`, `debug/read_physical`, `debug/write_memory`** — Use `DebugRequest::ReadMemory` / `WriteMemory` with `GuestAddress::Gva` / `GuestAddress::Gpa`.
   - Touches: `openvmm/openvmm_mcp/src/tools/debug.rs`.

5. **Implement `debug/set_breakpoint` and `debug/clear_breakpoint`** — Use `DebugRequest::SetDebugState` with `HardwareBreakpoint` configuration.
   - Touches: `openvmm/openvmm_mcp/src/tools/debug.rs`.

6. **Implement `debug/single_step`** — Set single-step in `DebugState`, resume, wait for `DebugStopReason::SingleStep`.
   - Touches: `openvmm/openvmm_mcp/src/tools/debug.rs`.

7. **Implement `debug/backtrace`** — Heuristic stack walker: read registers, walk frame pointer chain via memory reads.
   - Touches: `openvmm/openvmm_mcp/src/tools/debug.rs`.

8. **Implement `debug/list_vps`** — Enumerate VPs, report state of each.
   - Touches: `openvmm/openvmm_mcp/src/tools/debug.rs`.

### Phase 4: Petri-MCP Orchestrator (Tier 2)

**Goal:** Multi-VM lifecycle management via MCP.

**Steps:**

1. **Create `petri/petri_mcp/` crate** — New workspace member with binary target.
   - Touches: `Cargo.toml` (workspace members), new `petri/petri_mcp/Cargo.toml`, `petri/petri_mcp/src/main.rs`.

2. **Implement VM creation** — Map MCP tool parameters to `PetriVmConfig` construction, handle firmware selection, disk configuration.
   - New file: `petri/petri_mcp/src/orchestrator.rs`.

3. **Implement `vm/create`, `vm/start`, `vm/destroy`, `vm/list`** — Manage a `HashMap<VmId, PetriVmOpenVmm>` of running VMs.
   - New file: `petri/petri_mcp/src/tools/lifecycle.rs`.

4. **Implement guest agent tools** — `guest/execute`, `guest/read_file`, `guest/write_file`, `guest/shell` using `PipetteClient`.
   - New file: `petri/petri_mcp/src/tools/guest.rs`.

5. **Implement artifact resolution** — Accept artifact paths via environment variables or configuration, using patterns from `petri_artifact_resolver_openvmm_known_paths`.
   - New file: `petri/petri_mcp/src/artifacts.rs`.

6. **Delegate per-VM tools** — Reuse `openvmm_mcp` tool implementations for inspect, memory, debug, etc., by extracting the underlying `VmRpc` channels from `PetriVmOpenVmm`'s `Worker`.

### Phase 5: Advanced Features

**Goal:** Polish and extend.

**Steps:**

1. Add SSE and/or Streamable HTTP transport (for remote MCP clients).
2. Add MCP resource subscriptions (notify on VM state changes, serial output).
3. Add `vm://inspect/{path}` dynamic resources with change notifications.
4. Add VTL2 settings management tools (`vtl2/show`, `vtl2/add_scsi_disk`, `vtl2/remove_scsi_disk`).
5. Add OpenHCL diagnostics integration (via `DiagClient` — inspect paravisor, restart user-mode, etc.).
6. Add KVP interaction tools.

---

## 6. Technical Design

### 6.1 Crate Structure

```
openvmm/
  openvmm_mcp/           # NEW: Core MCP server library
    Cargo.toml
    src/
      lib.rs              # MCP server setup, tool registry, run_mcp_server()
      protocol.rs         # MCP JSON-RPC message types + framing
      transport.rs        # stdio / SSE transport implementation
      tools/
        mod.rs            # Tool trait, registry, dispatch
        lifecycle.rs      # pause/resume/reset/shutdown/nmi/clear_halt/status
        inspect.rs        # inspect tree query/update tools
        memory.rs         # read/write guest physical memory
        serial.rs         # serial console I/O tools
        display.rs        # framebuffer/screenshot tools
        disk.rs           # hot-add/remove SCSI disks
        snapshot.rs       # save/restore tools
        debug.rs          # GDB debug tools (registers, breakpoints, memory, step)
      resources.rs        # MCP resource definitions
      vm_handle.rs        # VmHandle abstraction over VmRpc + other channels
      serial_buffer.rs    # Ring buffer for serial console output capture

petri/
  petri_mcp/              # NEW: Orchestrator MCP server binary (Phase 4)
    Cargo.toml
    src/
      main.rs             # Entry point, MCP server on stdio
      orchestrator.rs     # Multi-VM management (HashMap<VmId, VM>)
      artifacts.rs        # Artifact resolution for firmware/images
      tools/
        mod.rs
        lifecycle.rs      # create/start/destroy/list VMs
        guest.rs          # pipette-based guest interaction tools
```

### 6.2 MCP SDK Choice

**Recommendation: Hand-rolled minimal MCP protocol layer (Phase 1), evaluate `rmcp` for Phase 2+.**

MCP is JSON-RPC 2.0 over stdio with a specific schema. The core protocol surface needed:
- `initialize` / `initialized` handshake
- `tools/list` — return tool definitions with JSON Schema
- `tools/call` — dispatch tool invocations
- `resources/list` — return resource URIs
- `resources/read` — return resource contents

This is ~300 lines of Rust using `serde_json`. Benefits:
- No external dependency risk
- Full control over protocol behavior
- Minimal binary size impact
- Can switch to a library later if one proves mature

### 6.3 Transport

**Primary: stdio** — Standard for MCP. The `openvmm --mcp` mode reads JSON-RPC from stdin, writes responses to stdout. VM logs go to stderr.

```
[MCP Client] --stdin--> [openvmm --mcp] --stdout--> [MCP Client]
                              |
                         [VM Worker]  (stderr for logs)
```

This matches the existing `--ttrpc` pattern which closes stdout to signal readiness.

**Future: SSE / Streamable HTTP** — For remote access. Can be added behind a feature flag using the existing `pal_async` socket infrastructure.

### 6.4 Async Runtime Integration

OpenVMM uses `pal_async` which provides `DefaultPool` / `DefaultDriver` — a platform-abstracted async runtime (backed by epoll on Linux, IOCP on Windows). The MCP server will:

1. Run inside a `DefaultPool::run_with` block (same as `do_main()`).
2. Spawn a task to read lines from stdin (MCP messages are newline-delimited JSON).
3. Dispatch tool calls to async handlers that communicate with the VM via `mesh::Sender<VmRpc>` and other channels.
4. This mirrors the existing `run_control()` function's event loop, but driven by MCP protocol messages instead of interactive CLI commands.

### 6.5 VmHandle Abstraction

A `VmHandle` struct encapsulates all channels needed to interact with a running VM:

```rust
pub struct VmHandle {
    /// RPC channel to the VM worker
    pub vm_rpc: mesh::Sender<VmRpc>,
    /// Worker handle (for inspect tree access)
    pub worker: WorkerHandle,
    /// SCSI controller RPC (optional)
    pub scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    /// NVMe controller RPC (optional, VTL2)
    pub nvme_rpc: Option<mesh::Sender<NvmeControllerRequest>>,
    /// Shutdown IC sender (optional)
    pub shutdown_ic: Option<mesh::Sender<ShutdownRpc>>,
    /// GED RPC sender (optional, for VTL2)
    pub ged_rpc: Option<mesh::Sender<GuestEmulationRequest>>,
    /// Debug request channel (optional, for debug tools)
    pub debug_rpc: Option<mesh::Sender<DebugRequest>>,
    /// Halt notification receiver
    pub halt_recv: mesh::Receiver<HaltReason>,
    /// Framebuffer access (optional, for screenshot)
    pub framebuffer: Option<FramebufferAccess>,
    /// Serial output ring buffer
    pub serial_buffer: Arc<SerialRingBuffer>,
    /// Console input writer (serial COM1)
    pub console_in: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    /// Cached VTL2 settings (optional)
    pub vtl2_settings: Option<Vtl2Settings>,
}
```

This is the same data as the existing `VmResources` struct in `openvmm_entry/src/lib.rs`, refactored into a reusable struct that can be consumed by both the interactive CLI and the MCP server.

### 6.6 Wiring into `openvmm_entry`

In `openvmm_entry/src/lib.rs`, `do_main()` currently has three modes:

```
1. Worker host mode  → meshworker::run_vmm_mesh_host()
2. ttrpc/grpc mode   → --ttrpc / --grpc flags
3. Interactive mode   → run_control() with readline loop
```

We add a fourth:

```rust
// In do_main(), after ttrpc/grpc check:
if opt.mcp {
    return DefaultPool::run_with(async |driver| {
        let mesh = VmmMesh::new(&driver, opt.single_process)?;
        let (mut vm_config, resources) = vm_config_from_command_line(&driver, &mesh, &opt).await?;

        // Set up debug channel if not already configured
        let debug_tx = if vm_config.debugger_rpc.is_none() {
            let (tx, rx) = mesh::channel();
            vm_config.debugger_rpc = Some(rx);
            Some(tx)
        } else {
            None
        };

        // Launch VM worker (same as run_control)
        let (vm_rpc, rpc_recv) = mesh::channel();
        let (notify_send, notify_recv) = mesh::channel();
        let vm_host = mesh.make_host("vm", opt.log_file.clone()).await?;
        let vm_worker = vm_host.launch_worker(VM_WORKER, VmWorkerParameters {
            hypervisor: opt.hypervisor,
            cfg: vm_config,
            saved_state: None,
            shared_memory: None,
            rpc: rpc_recv,
            notify: notify_send,
        }).await?;

        // Build VmHandle from resources + channels
        let vm_handle = VmHandle {
            vm_rpc,
            worker: vm_worker,
            halt_recv: notify_recv,
            debug_rpc: debug_tx,
            scsi_rpc: resources.scsi_rpc,
            // ... etc
        };

        // Resume VM unless --paused
        if !opt.paused {
            vm_handle.vm_rpc.call(VmRpc::Resume, ()).await?;
        }

        // Run MCP server on stdio
        openvmm_mcp::run_mcp_server(driver, vm_handle).await
    });
}
```

This reuses the existing `vm_config_from_command_line()` to parse all the same CLI flags (memory, disks, firmware, etc.), then hands off to the MCP server instead of the interactive console.

---

## 7. GDB Stub Integration — Detailed Plan

### 7.1 Current Architecture

The existing GDB debugging flow is:

```
[GDB Client (gdb/lldb)]
    ↕ TCP socket (GDB Remote Serial Protocol)
[debug_worker (DebuggerWorker)]
    ↕ mesh::Sender<DebugRequest>
[Partition Unit (VM worker)]
    ↕ Hypervisor API
[Guest VM]
```

1. `openvmm --gdb <port>` creates a `DebuggerParameters` with a TCP listener.
2. The `debug_worker` (a `mesh_worker::Worker`) accepts GDB client connections over TCP.
3. The debug worker communicates with the VM via `mesh::Sender<DebugRequest>`.
4. The VM's `partition_unit` processes `DebugRequest` messages: attaching, detaching, reading/writing VP state, setting breakpoints, single-stepping, reading/writing memory.
5. The debug worker implements the GDB Remote Serial Protocol using the `gdbstub` crate, translating between GDB RSP and `DebugRequest`.

### 7.2 MCP Debug Architecture

For MCP, we **bypass the GDB RSP entirely** and talk directly to the `DebugRequest` channel:

```
[MCP Client (LLM / AI Agent)]
    ↕ JSON-RPC (MCP protocol, stdio)
[MCP Server (openvmm_mcp)]
    ↕ mesh::Sender<DebugRequest>
[Partition Unit (VM worker)]
    ↕ Hypervisor API
[Guest VM]
```

This is possible because:
- The `DebugRequest` enum already provides all the primitives: `Attach`, `Detach`, `Resume`, `Break`, `GetVpState`, `SetVpState`, `ReadMemory`, `WriteMemory`, `SetDebugState`.
- We don't need the GDB RSP text protocol — MCP tools provide structured access.
- An AI agent can call `debug/get_registers` and receive a JSON object with named registers, rather than parsing GDB's register dump format.

### 7.3 Key Implementation Details

**Creating the debug channel:**

When `--mcp` is active in `do_main()`, before launching the VM worker:
```rust
let (debug_tx, debug_rx) = mesh::channel();
vm_config.debugger_rpc = Some(debug_rx);
// debug_tx goes into VmHandle
```

This is mutually exclusive with `--gdb` (they both set `Config::debugger_rpc`).

**Attach/Detach lifecycle:**

The MCP server sends `DebugRequest::Attach` during initialization and `DebugRequest::Detach` during shutdown. The VM must be attached before any debug operations work.

**Tool: `debug/get_registers`**

```rust
async fn get_registers(handle: &VmHandle, vp: u32) -> Result<serde_json::Value> {
    let debug = handle.debug_rpc.as_ref().context("debug not enabled")?;
    let state = debug.call_failable(DebugRequest::GetVpState, vp).await?;
    match *state {
        DebuggerVpState::X86_64(ref s) => Ok(json!({
            "arch": "x86_64",
            "rip": format!("{:#x}", s.rip),
            "rsp": format!("{:#x}", s.gp[4]),
            "rbp": format!("{:#x}", s.gp[5]),
            "rax": format!("{:#x}", s.gp[0]),
            "rbx": format!("{:#x}", s.gp[3]),
            "rcx": format!("{:#x}", s.gp[1]),
            "rdx": format!("{:#x}", s.gp[2]),
            "rsi": format!("{:#x}", s.gp[6]),
            "rdi": format!("{:#x}", s.gp[7]),
            // ... r8-r15, rflags, cr0-cr4, segment registers
        })),
        DebuggerVpState::Aarch64(ref s) => Ok(json!({
            "arch": "aarch64",
            "pc": format!("{:#x}", s.pc),
            "sp_el0": format!("{:#x}", s.sp_el0),
            "sp_el1": format!("{:#x}", s.sp_el1),
            "cpsr": format!("{:#x}", s.cpsr),
            // ... x0-x30
        })),
    }
}
```

**Tool: `debug/backtrace` (heuristic stack walker)**

This is a higher-level tool that chains multiple debug primitives:

1. Read RBP/RSP/RIP (x86) or FP/SP/LR (aarch64) from `debug/get_registers`
2. Walk the frame pointer chain: read `[rbp]` → previous RBP, `[rbp+8]` → return address
3. Repeat until a null frame pointer, unmappable address, or maximum depth (default 32)
4. Return an array of `{ address, frame_pointer }` entries
5. The agent can then cross-reference addresses with symbol information it has

```rust
async fn backtrace(handle: &VmHandle, vp: u32, max_depth: usize) -> Result<Vec<StackFrame>> {
    let regs = get_registers_raw(handle, vp).await?;
    let mut frames = vec![StackFrame { rip: regs.rip, rbp: regs.rbp }];
    let mut rbp = regs.rbp;

    for _ in 0..max_depth {
        if rbp == 0 || rbp % 8 != 0 { break; }
        // Read [rbp] = previous frame pointer, [rbp+8] = return address
        let data = debug_read_virtual(handle, vp, rbp, 16).await;
        let Ok(data) = data else { break; };
        let prev_rbp = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let ret_addr = u64::from_le_bytes(data[8..16].try_into().unwrap());
        if ret_addr == 0 { break; }
        frames.push(StackFrame { rip: ret_addr, rbp: prev_rbp });
        rbp = prev_rbp;
    }
    Ok(frames)
}
```

**Tool: `debug/set_breakpoint`**

Hardware breakpoints on x86 use debug registers DR0-DR3 (4 slots). The MCP server manages breakpoint slot allocation:

```rust
async fn set_breakpoint(handle: &VmHandle, vp: u32, addr: u64, kind: &str) -> Result<u32> {
    let bp_type = match kind {
        "execute" => BreakpointType::Execute,
        "write" => BreakpointType::Write,
        "read_write" => BreakpointType::ReadOrWrite,
        _ => anyhow::bail!("invalid breakpoint type"),
    };
    // Find a free slot
    let slot = handle.find_free_breakpoint_slot()?;
    let bp = HardwareBreakpoint {
        address: addr,
        ty: bp_type,
        size: BreakpointSize::Bytes1,
    };
    // Build DebugState with the breakpoint in the appropriate slot
    let state = build_debug_state_with_breakpoint(handle, slot, Some(bp));
    handle.debug_rpc.send(DebugRequest::SetDebugState { vp, state });
    Ok(slot)
}
```

**Tool: `debug/single_step`**

```rust
async fn single_step(handle: &VmHandle, vp: u32) -> Result<StepResult> {
    let debug = handle.debug_rpc.as_ref().context("debug not enabled")?;
    // Enable single-step for this VP
    let state = DebugState { single_step: true, breakpoints: current_breakpoints(handle) };
    debug.send(DebugRequest::SetDebugState { vp, state });
    // Resume and wait for stop
    let (tx, rx) = mesh::oneshot();
    debug.send(DebugRequest::Resume { response: tx });
    let reason = rx.await?;
    // Read updated register state
    let regs = get_registers_raw(handle, vp).await?;
    Ok(StepResult { stop_reason: format!("{:?}", reason), registers: regs })
}
```

### 7.4 Limitations & Mitigations

| Limitation | Impact | Mitigation |
|---|---|---|
| **Hardware breakpoints only (4 on x86)** | Can't set many breakpoints | Support software breakpoints via memory patching (`INT 3`) in a future phase |
| **No symbol information** | Agent sees raw addresses | Agent can load symbols from separate sources; `debug/backtrace` provides frame addresses for manual lookup |
| **Single-debugger constraint** | Can't use `--gdb` and `--mcp` debug tools simultaneously | Document clearly; could multiplex in future |
| **Guest paging context required for GVA reads** | Virtual memory reads need a VP context for page table walks | All GVA operations take a `vp` parameter; the VP must be stopped |
| **No conditional breakpoints** | Hardware breakpoints don't support conditions | Agent can implement conditional logic by single-stepping + checking registers |

---

## 8. Open Questions & Risks

### 8.1 Open Questions

1. **MCP SDK maturity:** Is there a production-quality Rust MCP SDK on crates.io? Candidates: `rmcp`, `mcp-server`, etc. Need to evaluate API ergonomics, spec compliance, and maintenance status. If none are suitable, the hand-rolled approach is low-risk given the protocol's simplicity.

2. **Serial buffering strategy:** The current architecture pipes serial output directly to the terminal/file with no in-process buffering. For MCP, we need a ring buffer intercepting serial output. Options:
   - Fixed-size ring buffer (e.g., 64KB) with old data dropped on overflow — **recommended**
   - Unbounded buffer (memory risk for long-running VMs)
   - The `serial/read` tool should support a `since` cursor to avoid re-reading old data.

3. **Authentication / authorization:** MCP over stdio inherits the parent process's permissions (no additional auth needed). For SSE/HTTP transport (Phase 5), auth would be required. Options: bearer tokens, mTLS. Defer to Phase 5.

4. **Concurrent tool calls:** MCP allows concurrent tool invocations. The VM worker processes `VmRpc` messages sequentially. Most tools can safely be called concurrently (e.g., `inspect` while the VM is running), but some are mutually exclusive (e.g., `pause` + `resume` simultaneously). The MCP server should:
   - Allow concurrent read-only operations (inspect, memory read, register read)
   - Serialize state-changing operations (pause, resume, reset, breakpoint management)
   - Return clear errors for conflicting operations

5. **Petri artifact resolution for Tier 2:** `petri-mcp` needs firmware images, pipette binaries, guest OS images. How should these be provided?
   - (a) Environment variables pointing to known paths (matching `petri_artifact_resolver_openvmm_known_paths` pattern) — **recommended for initial implementation**
   - (b) An MCP tool to set artifact paths at runtime
   - (c) A TOML/JSON configuration file

6. **VM configuration surface for Tier 2:** The `Config` struct has dozens of fields. The MCP `vm/create` tool should expose a simplified surface. Proposed minimal set:
   - `processors` (u32, default 1)
   - `memory` (string like "1GB", default "1GB")
   - `firmware` (enum: "linux-direct" | "uefi" | "pcat")
   - `kernel` / `initrd` / `cmdline` (for linux-direct)
   - `disks` (array of disk specs)
   - `nics` (array of NIC specs)
   - Everything else gets sensible defaults.

### 8.2 Risks

1. **Protocol drift:** MCP is still evolving (2024-11-05 spec, 2025-03-26 Streamable HTTP update). We should pin to a specific spec version (recommend: 2025-03-26) and document which features we support.

2. **Performance of inspect:** The inspect tree can be large. Deep recursive inspects with short timeouts may produce incomplete results (`Unresolved` nodes). The MCP server should:
   - Default to a reasonable timeout (1 second, matching the interactive CLI)
   - Support a `timeout_ms` parameter on inspect tools
   - Return partial results with clear indication of timeouts

3. **Debug channel exclusivity:** Using `--mcp` with debug tools means `--gdb` cannot be used simultaneously. This is acceptable but should be documented. The MCP `--mcp` flag should error if `--gdb` is also specified.

4. **Framebuffer encoding cost:** Converting BGRA framebuffer → RGBA → PNG on every `display/screenshot` call has CPU cost (typical: 5-50ms for 1920×1080). Mitigations:
   - Cache the last screenshot if framebuffer hasn't changed
   - Offer resolution downscaling option
   - Return raw RGBA with dimensions if PNG encoding is too slow

5. **Build time impact:** Adding `openvmm_mcp` to the workspace adds a new crate to compile. Dependencies should be minimal (mostly `serde_json`, which is likely already in the dependency tree). The `image`/`png` dependency for screenshots could be feature-gated.

6. **Cross-platform:** OpenVMM runs on Linux, Windows, and macOS. The MCP stdio transport is inherently cross-platform. All async I/O goes through `pal_async`. No platform-specific concerns for Phase 1-3.

7. **Stability of `mesh` channels:** The `VmRpc` + `mesh` channel infrastructure is the backbone of VM communication. It's well-tested and used by the interactive CLI, petri, and ttrpc. The MCP server is just another consumer of these channels, so stability risk is low.

---

## Appendix A: Key Files Reference

| File/Crate | Role | Relevance |
|---|---|---|
| `openvmm/openvmm_entry/src/lib.rs` | Main entry point, VM config, interactive CLI loop (`run_control`) | Wire-in point for `--mcp` mode |
| `openvmm/openvmm_entry/src/cli_args.rs` | CLI argument parsing | Add `--mcp` flag |
| `openvmm/openvmm_defs/src/rpc.rs` | `VmRpc` enum — all RPC commands to VM worker | Primary control interface |
| `openvmm/openvmm_defs/src/config.rs` | `Config` struct — full VM configuration | VM setup |
| `vmm_core/vmm_core_defs/src/debug_rpc.rs` | `DebugRequest` / `DebuggerVpState` — debug protocol | GDB integration |
| `workers/debug_worker/src/lib.rs` | GDB stub worker (pattern reference) | Architecture reference |
| `workers/debug_worker/src/gdb/targets/base.rs` | GDB target: register read/write, memory access | API reference |
| `support/inspect/src/initiate.rs` | Inspect framework: `Node`, `Entry`, `InspectionBuilder` | Inspect tool implementation |
| `petri/src/vm/openvmm/runtime.rs` | `PetriVmOpenVmm` — running VM interaction | Tier 2 pattern reference |
| `petri/src/vm/openvmm/construct.rs` | VM construction from `PetriVmConfig` | Tier 2 implementation |
| `petri/src/worker.rs` | `Worker` — wraps `WorkerHandle` + `VmRpc` | Pattern for VM interaction |
| `petri/pipette_client/src/lib.rs` | Guest agent client | Tier 2 guest tools |
| `openvmm/openvmm_entry/src/ttrpc/mod.rs` | Existing gRPC/ttrpc VMService | Pattern to follow for MCP |
| `openvmm/openvmm_entry/src/serial_io.rs` | Serial port backend configuration | Serial tool implementation |
| `openvmm/openvmm_entry/src/meshworker.rs` | Mesh process management | Worker infrastructure |

## Appendix B: MCP Tool Schema Examples

### Tool Definition: `vm/pause`

```json
{
  "name": "vm/pause",
  "description": "Pause VM execution. All virtual processors stop executing guest code. Call vm/resume to continue.",
  "inputSchema": {
    "type": "object",
    "properties": {}
  }
}
```

### Tool Definition: `debug/get_registers`

```json
{
  "name": "debug/get_registers",
  "description": "Read the register state of a virtual processor. Returns all general-purpose registers, instruction pointer, flags, control registers, and segment registers. The VM must be in a debug-break state (call debug/break first).",
  "inputSchema": {
    "type": "object",
    "properties": {
      "vp": {
        "type": "integer",
        "description": "Virtual processor index (0-based)",
        "default": 0
      }
    }
  }
}
```

### Tool Call Response Example: `debug/get_registers`

```json
{
  "content": [{
    "type": "text",
    "text": "{\"arch\":\"x86_64\",\"rip\":\"0xfffff80524a0b230\",\"rsp\":\"0xfffff80524c87000\",\"rbp\":\"0xfffff80524c870a0\",\"rax\":\"0x0\",\"rbx\":\"0xfffff80524c87100\",\"rcx\":\"0x1\",\"rdx\":\"0x0\",\"rsi\":\"0x0\",\"rdi\":\"0x0\",\"r8\":\"0x0\",\"r9\":\"0x0\",\"r10\":\"0x0\",\"r11\":\"0x0\",\"r12\":\"0x0\",\"r13\":\"0x0\",\"r14\":\"0x0\",\"r15\":\"0x0\",\"rflags\":\"0x246\",\"cs\":\"0x10\",\"ss\":\"0x18\",\"cr0\":\"0x80050033\",\"cr3\":\"0x1aa000\",\"cr4\":\"0x370678\",\"efer\":\"0xd01\"}"
  }]
}
```

### Tool Definition: `inspect/tree`

```json
{
  "name": "inspect/tree",
  "description": "Query the OpenVMM inspect tree at a given path. The inspect tree exposes the internal state of the VMM for diagnostics — device state, VP registers, VMBus channels, memory maps, etc. Use an empty path to see the root, and set recursive=true to enumerate child nodes.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "Inspect tree path (e.g., 'vm/chipset', 'vm/partition'). Empty string for root.",
        "default": ""
      },
      "recursive": {
        "type": "boolean",
        "description": "If true, enumerate children recursively up to the depth limit.",
        "default": false
      },
      "depth": {
        "type": "integer",
        "description": "Maximum recursion depth (only used when recursive=true).",
        "default": 3
      },
      "timeout_ms": {
        "type": "integer",
        "description": "Timeout in milliseconds for the inspect operation.",
        "default": 1000
      }
    }
  }
}
```

### Tool Definition: `display/screenshot`

```json
{
  "name": "display/screenshot",
  "description": "Capture the current VM display framebuffer as a PNG image. Only available if the VM is configured with graphics (--gfx or --vnc). Returns the image as base64-encoded PNG data.",
  "inputSchema": {
    "type": "object",
    "properties": {}
  }
}
```

Response (with image content):
```json
{
  "content": [{
    "type": "image",
    "data": "<base64-encoded PNG>",
    "mimeType": "image/png"
  }]
}
```
