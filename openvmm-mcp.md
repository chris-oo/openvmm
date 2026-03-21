# OpenVMM MCP Server — Implementation Plan

## Implementation Status

> **Last updated:** 2026-03-21

### ✅ Phase 1 — COMPLETE

Phase 1 (Core MCP Infrastructure) has been fully implemented and tested.

**Commits:**
1. `rvrtymws` — **Add openvmm_mcp crate: MCP server Phase 1**
   - Created `openvmm/openvmm_mcp/` crate (~1,400 LOC, 13 files)
   - Hand-rolled MCP JSON-RPC protocol (no external SDK)
   - Stdio transport with stdin reader thread → mesh channel
   - Async event loop merging stdin messages + VM halt notifications
   - VmHandle abstraction over mesh RPC channels
   - 64KB serial ring buffer with cursor-based reads
   - 11 tools: vm/pause, vm/resume, vm/reset, vm/nmi, vm/clear_halt, vm/status, inspect/tree, inspect/get, inspect/update, serial/read, serial/write
   - 3 unit tests for serial ring buffer

2. `qopknvks` — **openvmm_mcp: wire into openvmm_entry with full serial I/O**
   - `--mcp` CLI flag with `conflicts_with_all = ["gdb", "ttrpc", "grpc"]`
   - Serial output teed to both ring buffer AND stderr via dup'd fd
   - Synchronous console_in writer (dup'd unix socket fd) for serial/write
   - Code review fixes: `parking_lot::Mutex`, protocol state machine enforcement, inspect depth clamped to 10, VP index validation, wrapping_add for buffer counter
   - `scripts/test_mcp.py` — end-to-end test exercising all tools

**Test results (34/34 passing) with Alpine linux-direct boot:**
- MCP initialize handshake ✅
- Tools listing (11 tools) ✅
- VM status/pause/resume ✅
- Inspect tree (root and specific paths like `vm`) ✅
- Serial read — captured full boot output in ring buffer ✅
- Serial write — sent `root\n`, received `Password:` prompt back ✅
- NMI ✅
- Error handling (unknown tool, unknown method) ✅

**Key design decisions made during implementation:**
- Used `parking_lot::Mutex` (not `std::sync::Mutex`) per project convention
- Serial fd is dup'd from the PolledSocket for synchronous writes — the PolledSocket is registered on the serial driver thread, so async writes from the MCP event loop context don't work (wrong epoll instance)
- Serial output thread uses `block_on(async { read loop })` pattern, matching existing `setup_serial()` approach
- Protocol state machine enforced: tools/list and tools/call rejected before initialize handshake
- MCP server replaces `run_control()` in the control flow — uses same `vm_config_from_command_line()` and VM worker launch path

### ✅ Phase 2 — COMPLETE (Serial Execute & Halt Wait)
- `serial/execute` convenience tool (write command, wait for prompt)
- `vm/wait_for_halt` tool (block until VM halts, returns halt reason)

**Commits:**
3. **openvmm_mcp: Phase 2 — serial/execute & vm/wait_for_halt**
   - Two new tools: `serial/execute` (write command, poll for prompt, return output) and `vm/wait_for_halt` (block until VM halts with timeout)
   - Event loop restructured for concurrent tool dispatch using `unicycle::FuturesUnordered` + `futures::future::select` — halt events are processed while long-running tools are pending
   - Tool handler signatures changed from `&'a VmHandle` to `Arc<VmHandle>` for `'static` futures
   - `VmHandle` gained halt waiter notification mechanism (list of `mesh::Sender<String>` drained on halt)
   - `serial/execute` spawns a polling thread that checks the ring buffer every 100ms for prompt patterns
   - Default prompt patterns: `# `, `$ `, `> `, `login: `, `Password: ` (configurable via `prompt_pattern` parameter)
   - 11 new unit tests (prompt detection, ANSI stripping, halt waiter mechanism); total: 14 unit tests
   - Concurrency fix: cursor snapshot + write are atomic under `console_in` lock
   - Tool count: 13 (11 from Phase 1 + 2 new)

4. **Guide: Add MCP server documentation**
   - New Guide page: `Guide/src/reference/openvmm/management/mcp.md`
   - Added to `SUMMARY.md` under Configuration and Management
   - `--mcp` flag added to CLI reference (`cli.md`)
   - Covers: protocol overview, all 13 tools, example session, architecture

### ⏳ Phase 3 — NOT STARTED (Petri-MCP Orchestrator)
- Standalone `petri_mcp` binary for multi-VM lifecycle management
- `vm/create`, `vm/start`, `vm/destroy`, `vm/list`
- Guest agent tools via pipette (`guest/execute`, `guest/read_file`, `guest/write_file`)

### ⏳ Phase 4 — NOT STARTED (Extended Tools & Features)
- `memory/read` and `memory/write` tools
- `display/screenshot` tool (framebuffer → PNG)
- `disk/add` and `disk/remove` tools
- `snapshot/save` tool
- MCP resources (`vm://config`, `vm://status`, `vm://serial/log`)
- SSE/Streamable HTTP transport
- MCP resource subscriptions
- VTL2 settings management
- OpenHCL diagnostics integration

### ⏳ Phase 5 — NOT STARTED (GDB Debug Integration)
- Debug channel wiring (`DebugRequest`)
- `debug/break`, `debug/continue`, `debug/get_registers`, `debug/set_registers`
- `debug/read_memory`, `debug/write_memory`
- `debug/set_breakpoint`, `debug/clear_breakpoint`, `debug/single_step`
- `debug/backtrace` (heuristic stack walker)
- OpenHCL diagnostics integration

---

## 1. Executive Summary

We propose adding **Model Context Protocol (MCP) server** support to OpenVMM, enabling AI agents (LLMs operating through tool-calling) to configure, launch, inspect, debug, and interact with virtual machines programmatically. The MCP server exposes the same capabilities available today through OpenVMM's interactive CLI console and petri test framework — VM lifecycle management, the inspect tree, GDB-stub debugging, serial console I/O, framebuffer screenshots, and guest-agent interaction — as structured MCP tools and resources consumable by any MCP-compatible client (Claude Desktop, VS Code Copilot, custom agents, etc.).

**Why MCP?** MCP is emerging as a standard protocol for connecting LLMs to external tools. A VMM is a uniquely valuable tool for AI agents: agents can spin up sandboxed environments, run code, inspect OS internals, debug kernel crashes, and automate integration testing — all without touching the host.

---

## 2. Architecture Decision

### Recommendation: Two-tier architecture — `openvmm-mcp` (embedded) + `petri-mcp` (orchestrator)

After analyzing the codebase, we recommend building **two** MCP servers that target different use cases, sharing a common core of MCP tool definitions.

#### Tier 1: `openvmm-mcp` — Embedded single-VM MCP server

- **What:** A new CLI mode in the OpenVMM binary (`openvmm --mcp`) that replaces the interactive `openvmm>` console with an MCP server communicating over stdio (or SSE/streamable-HTTP).
- **Where it fits:** The MCP server replaces `run_control()` — the interactive CLI loop that drives a single VM. It uses the same VM setup flow (`vm_config_from_command_line()` → launch VM worker → drive via `VmRpc` messages and `mesh` channels), but substitutes MCP JSON-RPC for the readline-based console. This is *not* analogous to the `--ttrpc`/`--grpc` mode, which constructs VMs internally from gRPC requests without using CLI args.
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
| Matches existing patterns | ✅ (like `run_control()`) | ✅ (like `vmm_tests`) | ✅ |

#### Build order: Tier 1 first, then Tier 2

Tier 1 (`openvmm-mcp`) can be built with minimal new dependencies and directly mirrors the interactive CLI. Tier 2 (`petri-mcp`) builds on top of Tier 1's tool definitions and adds orchestration.

---

## 3. MCP Tools Inventory

### 3.1 VM Lifecycle Tools (both tiers)

| Tool | Description | Tier 1 | Tier 2 |
|---|---|---|---|
| `vm/pause` | Pause VM execution (returns `bool` — `false` if already paused) | ✅ | ✅ |
| `vm/resume` | Resume paused VM (returns `bool` — `false` if already running) | ✅ | ✅ |
| `vm/reset` | Reset VM (power cycle) | ✅ | ✅ |
| `vm/shutdown` | Request guest shutdown (via shutdown IC) | ✅ | ✅ |
| `vm/nmi` | Inject NMI to a VP | ✅ | ✅ |
| `vm/clear_halt` | Clear halt condition | ✅ | ✅ |
| `vm/status` | Get current VM state (running/paused/halted + halt reason) | ✅ | ✅ |
| `vm/wait_for_halt` | Block until the VM halts; returns the halt reason. Useful for agents that need to wait for shutdown, triple fault, etc. without polling `vm/status`. Maps to the `notify_recv` channel. | ✅ | ✅ |

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

**Implementation note:** The existing interactive CLI pipes serial output directly to the terminal via a background thread (`setup_serial()` in `openvmm_entry`). In `--mcp` mode, this would corrupt the JSON-RPC stream on stdout. The serial I/O strategy is:

- **Phase 1 (minimal):** Default COM1 to stderr in MCP mode (`--com1 stderr`), so serial output is visible in logs but doesn't corrupt MCP. Add a basic `serial/read` that returns recent stderr-redirected output.
- **Phase 2 (full):** Introduce a `SerialRingBuffer` (fixed-size, e.g., 64KB) that intercepts serial output in-process. The `serial/read` tool reads from this buffer with cursor-based pagination (`since` parameter) to avoid re-reading old data. The `serial/write` tool writes to `console_in`.
- **Phase 2 (convenience):** `serial/execute` writes a command, waits for output until a configurable prompt pattern (default: common shell prompts like `$ `, `# `, `> `) or timeout.

### 3.6 Framebuffer / Screenshot Tools

| Tool | Description | Maps to |
|---|---|---|
| `display/screenshot` | Capture the current framebuffer as a PNG image | `FramebufferAccess` / `View::read_line()` |
| `display/resolution` | Get current display resolution | `View::resolution()` |

**⚠️ VNC conflict:** `FramebufferAccess` is **consumed** by the VNC worker when `--vnc` is enabled — only one consumer can hold it. Resolution strategy:
- **Phase 1:** In `--mcp` mode, skip VNC by default (the MCP client gets screenshots directly via `display/screenshot`). If `--vnc` is explicitly passed alongside `--mcp`, the display tools return an error explaining the conflict.
- **Future:** Share the framebuffer by cloning the underlying `Mappable` file descriptor, allowing both VNC and MCP screenshot access simultaneously.

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
| `debug/backtrace` | Walk the stack for a VP (heuristic, **best-effort** — requires frame pointers, unreliable on optimized code) | Read registers, then walk stack frames via memory reads |

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

### Phase 1: Core MCP Infrastructure (Tier 1 foundation) — ✅ COMPLETE

**Goal:** Establish the MCP server crate, transport layer, and first few tools working with a running OpenVMM instance. Include basic serial access so the server is immediately useful for linux-direct boot.

**Steps:**

1. ✅ **Create `openvmm/openvmm_mcp/` crate** — New workspace member with `Cargo.toml`, add to `Cargo.toml` workspace members list.
   - Dependencies: `serde`, `serde_json`, `mesh`, `mesh_worker`, `inspect`, `openvmm_defs`, `vmm_core_defs`, `anyhow`, `futures`, `futures-concurrency`, `parking_lot`, `tracing`.
   - Touches: `Cargo.toml` (workspace), new `openvmm/openvmm_mcp/Cargo.toml`, new `openvmm/openvmm_mcp/src/lib.rs`.

2. ✅ **Implement MCP JSON-RPC protocol layer** — Handle `initialize`, `tools/list`, `tools/call` over stdio. Resources deferred to Phase 2.
   - New files: `openvmm/openvmm_mcp/src/protocol.rs`, `openvmm/openvmm_mcp/src/transport.rs`.

3. ✅ **Implement tool registry and dispatch** — Function-pointer-based registry mapping tool names to async handlers.
   - New files: `openvmm/openvmm_mcp/src/tools/mod.rs`.

4. ✅ **Implement `VmHandle` abstraction** — Encapsulates VmRpc sender, WorkerHandle, serial buffer, and console_in writer.
   - New file: `openvmm/openvmm_mcp/src/vm_handle.rs`.

5. ✅ **Implement lifecycle tools** — `vm/pause`, `vm/resume`, `vm/status`, `vm/reset`, `vm/nmi`, `vm/clear_halt`. (`vm/shutdown` and `vm/wait_for_halt` deferred — shutdown requires shutdown IC plumbing, wait_for_halt needs oneshot channel wiring.)
   - New file: `openvmm/openvmm_mcp/src/tools/lifecycle.rs`.

6. ✅ **Wire into `openvmm_entry`** — Add `--mcp` CLI flag (with `conflicts_with_all(["gdb", "ttrpc", "grpc"])`), create the `VmHandle`, tee serial output to ring buffer + stderr in MCP mode, launch MCP server.
   - Touches: `openvmm/openvmm_entry/src/cli_args.rs`, `openvmm/openvmm_entry/src/lib.rs`, `openvmm/openvmm_entry/Cargo.toml`.

7. ✅ **Implement the MCP event loop** — Multiplex MCP protocol messages (stdin) and halt notifications (`notify_recv`) using `futures_concurrency::stream::Merge`. Worker events deferred to Phase 2.
   - New file: `openvmm/openvmm_mcp/src/event_loop.rs`.

8. ✅ **Implement `inspect/tree`, `inspect/get`, and `inspect/update` tools**.
   - New file: `openvmm/openvmm_mcp/src/tools/inspect.rs`.

9. ✅ **Implement serial tools** — Full serial ring buffer (64KB) capturing serial output. `serial/read` returns buffered output with cursor. `serial/write` writes to console_in via synchronous dup'd fd.
   - New files: `openvmm/openvmm_mcp/src/tools/serial.rs`, `openvmm/openvmm_mcp/src/serial_buffer.rs`.

10. ✅ **End-to-end validation** — `scripts/test_mcp.py` launches `openvmm --mcp` with Alpine linux-direct boot, exercises all 11 tools, 34/34 tests passing.

**Actual scope:** ~1,400 LOC new (openvmm_mcp), ~100 LOC modifications to openvmm_entry, ~350 LOC test script.

### Phase 2: Serial Execute & Halt Wait

**Goal:** Add the two highest-value tools for agent workflows before moving to the petri orchestrator.

**Steps:**

1. **Implement `serial/execute`** — Write a command to serial, poll the ring buffer until a prompt pattern (e.g. `# `, `$ `, `> `) appears or timeout expires, return the complete output in one response. This eliminates the fragile write→sleep→read pattern agents must use today.
   - Touches: `openvmm/openvmm_mcp/src/tools/serial.rs`.

2. **Implement `vm/wait_for_halt`** — Block until the VM halts (shutdown, triple fault, etc.), return the halt reason. Uses the existing `halt_recv` channel. Prevents agents from polling `vm/status` in a loop.
   - Touches: `openvmm/openvmm_mcp/src/tools/lifecycle.rs`, `openvmm/openvmm_mcp/src/event_loop.rs`.

### Phase 3: Petri-MCP Orchestrator (Tier 2)

**Goal:** Multi-VM lifecycle management via MCP, with reliable guest interaction through pipette.

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

### Phase 4: Extended Tools & Features

**Goal:** Round out the tool surface and add protocol features.

**Steps:**

1. **Implement `memory/read` and `memory/write`** — Wrap `VmRpc::ReadMemory` / `VmRpc::WriteMemory`, return hex dump or base64.
   - New file: `openvmm/openvmm_mcp/src/tools/memory.rs`.

2. **Implement `display/screenshot`** — Encode `FramebufferAccess` output as PNG, return as base64 MCP image content.
   - New file: `openvmm/openvmm_mcp/src/tools/display.rs`.

3. **Implement `disk/add`, `disk/remove`** — Wrap SCSI controller RPC.
   - New file: `openvmm/openvmm_mcp/src/tools/disk.rs`.

4. **Implement `snapshot/save` and `snapshot/pulse_save_restore`**.
   - New file: `openvmm/openvmm_mcp/src/tools/snapshot.rs`.

5. **Implement MCP resources** — `vm://config`, `vm://status`, `vm://serial/log`.
   - New file: `openvmm/openvmm_mcp/src/resources.rs`.

6. Add SSE and/or Streamable HTTP transport (for remote MCP clients).
7. Add MCP resource subscriptions (notify on VM state changes, serial output).
8. Add `vm://inspect/{path}` dynamic resources with change notifications.
9. Add VTL2 settings management tools (`vtl2/show`, `vtl2/add_scsi_disk`, `vtl2/remove_scsi_disk`).
10. Add OpenHCL diagnostics integration (via `DiagClient` — inspect paravisor, restart user-mode, etc.).
11. Add KVP interaction tools.

### Phase 5: GDB Debug Integration (Nice-to-Have)

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

---

## 6. Technical Design

### 6.1 Crate Structure (as implemented)

```
openvmm/
  openvmm_mcp/           # MCP server library (Phase 1 ✅)
    Cargo.toml
    src/
      lib.rs              # Module exports, re-exports run_mcp_server + VmHandle
      protocol.rs         # MCP JSON-RPC 2.0 message types + error codes
      transport.rs        # Stdin reader thread + stdout JSON writer
      tools/
        mod.rs            # Tool registry (fn-pointer based) + dispatch
        lifecycle.rs      # pause/resume/reset/nmi/clear_halt/status
        inspect.rs        # inspect tree query/get/update via InspectionBuilder
        serial.rs         # serial read (ring buffer) / write (sync fd)
      event_loop.rs       # Async event loop: merge(stdin_stream, halt_stream)
      vm_handle.rs        # VmHandle: VmRpc + WorkerHandle + serial + console_in
      serial_buffer.rs    # Thread-safe 64KB ring buffer with cursor reads

petri/
  petri_mcp/              # Orchestrator MCP server binary (Phase 4 — planned)
    ...
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

**Primary: stdio** — Standard for MCP. The `openvmm --mcp` mode reads JSON-RPC from stdin, writes responses to stdout. VM logs and serial output go to stderr.

```
[MCP Client] --stdin--> [openvmm --mcp] --stdout--> [MCP Client]
                              |
                         [VM Worker]  (stderr for logs + serial)
```

This mirrors the `run_control()` interactive CLI flow, but with structured JSON-RPC replacing the readline loop.

**Future: SSE / Streamable HTTP** — For remote access. Can be added behind a feature flag using the existing `pal_async` socket infrastructure.

### 6.4 Async Runtime Integration

OpenVMM uses `pal_async` which provides `DefaultPool` / `DefaultDriver` — a platform-abstracted async runtime (backed by epoll on Linux, IOCP on Windows). The MCP server will:

1. Run inside a `DefaultPool::run_with` block (same as `do_main()`).
2. Spawn a task to read lines from stdin (MCP messages are newline-delimited JSON).
3. Dispatch tool calls to async handlers that communicate with the VM via `mesh::Sender<VmRpc>` and other channels.
4. Multiplex MCP stdin, halt notifications, and worker events in a `merge()`-based event loop (see §6.7).

**Note:** The first parameter of `vm_config_from_command_line()` is `impl Spawn`, not `&DefaultDriver` specifically. `DefaultDriver` implements `Spawn`, so this works, but the abstraction boundary matters.

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
2. ttrpc/grpc mode   → --ttrpc / --grpc flags (VM constructed internally from gRPC requests)
3. Interactive mode   → run_control() with readline loop (VM from CLI args)
```

We add a fourth, parallel to mode 3 (not mode 2):

```rust
// In cli_args.rs:
#[clap(long, conflicts_with_all(["gdb", "ttrpc", "grpc"]))]
pub mcp: bool,

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
        // Note: VmRpc::Resume returns bool (false = already running)
        if !opt.paused {
            let _changed = vm_handle.vm_rpc.call(VmRpc::Resume, ()).await?;
        }

        // Run MCP server on stdio
        openvmm_mcp::run_mcp_server(driver, vm_handle).await
    });
}
```

This reuses the existing `vm_config_from_command_line()` to parse all the same CLI flags (memory, disks, firmware, etc.), then hands off to the MCP server instead of the interactive console.

### 6.7 Event Loop Design

The MCP server must multiplex multiple event sources, mirroring `run_control()`'s `merge()`-based select loop. Without this, tool calls hang when the VM worker dies, and halt events go unnoticed.

```rust
enum McpEvent {
    /// Incoming MCP JSON-RPC message from stdin
    McpMessage(Result<JsonRpcMessage, io::Error>),
    /// VM halt notification (power off, triple fault, etc.)
    VmHalted(HaltReason),
    /// VM worker event (stopped, failed, restarted)
    WorkerEvent(WorkerEvent),
    /// Pending tool call completed (for concurrent tool dispatch)
    ToolResult { id: JsonRpcId, result: Result<serde_json::Value, McpError> },
    /// Stdin closed (MCP client disconnected)
    ClientDisconnected,
}

async fn run_mcp_event_loop(driver: &DefaultDriver, vm_handle: VmHandle) -> anyhow::Result<()> {
    let mut events = merge!(
        mcp_stdin_reader.map(McpEvent::McpMessage),
        vm_handle.halt_recv.map(McpEvent::VmHalted),
        vm_handle.worker.events().map(McpEvent::WorkerEvent),
        tool_completions.map(|(id, result)| McpEvent::ToolResult { id, result }),
    );

    let mut vm_alive = true;

    while let Some(event) = events.next().await {
        match event {
            McpEvent::McpMessage(Ok(msg)) => {
                // Dispatch tool calls; read-only tools run concurrently,
                // state-changing tools are serialized
                dispatch_mcp_message(msg, &vm_handle, vm_alive).await?;
            }
            McpEvent::VmHalted(reason) => {
                vm_alive = false;
                // Send MCP notification (if supported) or log to stderr
                eprintln!("VM halted: {:?}", reason);
            }
            McpEvent::WorkerEvent(WorkerEvent::Failed(err)) => {
                vm_alive = false;
                // All subsequent VmRpc calls will fail — return errors
                eprintln!("VM worker failed: {:?}", err);
            }
            McpEvent::ToolResult { id, result } => {
                send_mcp_response(id, result).await?;
            }
            McpEvent::ClientDisconnected | McpEvent::McpMessage(Err(_)) => {
                break; // Clean shutdown
            }
            _ => {}
        }
    }

    // Cleanup (see §6.8)
    shutdown_vm(&vm_handle).await;
    Ok(())
}
```

Key design points:
- **Read-only tools** (`inspect/tree`, `debug/get_registers`, `serial/read`, `display/screenshot`) can execute concurrently.
- **State-changing tools** (`vm/pause`, `vm/resume`, `vm/reset`, `debug/break`, `debug/set_breakpoint`) are serialized to avoid races.
- **VM death detection:** When `WorkerEvent::Failed` arrives, the server sets `vm_alive = false` and returns clear errors for subsequent tool calls instead of hanging on `RpcError`.
- **`vm/wait_for_halt`:** Implemented as a pending future that resolves when `McpEvent::VmHalted` fires.

### 6.8 Shutdown & Cleanup

When the MCP client disconnects (stdin closes or EOF), the server must clean up gracefully:

1. **Detach debugger** — If debug tools were attached, send `DebugRequest::Detach` to release the debug channel.
2. **Stop the VM worker** — Signal the worker to shut down, wait for `WorkerEvent::Stopped`.
3. **Clean up mesh** — Drop all mesh channels and the `VmmMesh` instance.
4. **Exit** — Return from `do_main()`.

This mirrors the `InteractiveCommand::Quit` handling in `run_control()`. The server should also handle `SIGTERM`/`SIGINT` gracefully (the `DefaultPool` already handles this via `pal_async`'s signal integration).

### 6.9 Testing Strategy

**Unit tests:**
- MCP protocol layer: Test JSON-RPC parsing, framing, error responses. Mock stdin/stdout with in-memory buffers.
- Tool dispatch: Test tool registry, argument validation, unknown tool handling.
- Serial ring buffer: Test read/write, cursor-based pagination, overflow behavior.

**Integration tests (using petri):**
- Spawn `openvmm --mcp` as a child process from a petri test.
- Send MCP tool calls over stdin, validate JSON-RPC responses on stdout.
- Test lifecycle: pause → check status → resume → check status.
- Test inspect: query inspect tree, verify expected paths exist.
- Test serial: write to serial, read back output.
- Test error handling: call tools after VM halt, verify graceful errors.

**CI integration:**
- Add a new test target in `vmm_tests/` for MCP server tests.
- These tests should run as part of the existing petri-based CI pipeline.

**Manual validation script:**
- Provide a simple Python/Bash script that acts as an MCP client for manual testing during development. This is not a permanent test — just a development aid.

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

1. **MCP SDK maturity:** Is there a production-quality Rust MCP SDK on crates.io? Candidates: `rmcp`, `mcp-server`, etc. Need to evaluate API ergonomics, spec compliance, and maintenance status. If none are suitable, the hand-rolled approach is low-risk given the protocol's simplicity. The `rmcp` crate (0.1.x) is worth evaluating before committing — if lightweight, it saves ~300 lines of boilerplate and ensures spec compliance.

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

7. **`--mcp` flag interactions with other CLI flags:**
   - `--mcp` + `--vnc` — VNC is skipped by default in MCP mode (framebuffer goes to MCP). If `--vnc` is explicitly passed, display tools return errors.
   - `--mcp` + `--paused` — Works fine, VM starts paused and agent must call `vm/resume`.
   - `--mcp` + `--com1 <value>` — User-specified serial config is respected. If not specified, defaults to stderr in MCP mode (instead of the normal console default).
   - `--mcp` + `--gdb` / `--ttrpc` / `--grpc` — Mutually exclusive, enforced by clap `conflicts_with_all`.

### 8.2 Risks

1. **Protocol drift:** MCP is still evolving (2024-11-05 spec, 2025-03-26 Streamable HTTP update). We should pin to a specific spec version (recommend: 2025-03-26) and document which features we support.

2. **Performance of inspect:** The inspect tree can be large. Deep recursive inspects with short timeouts may produce incomplete results (`Unresolved` nodes). The MCP server should:
   - Default to a reasonable timeout (1 second, matching the interactive CLI)
   - Support a `timeout_ms` parameter on inspect tools
   - Return partial results with clear indication of timeouts

3. **Debug channel exclusivity:** Using `--mcp` with debug tools means `--gdb` cannot be used simultaneously. This is enforced at the CLI level via `conflicts_with_all(["gdb", "ttrpc", "grpc"])` on the `--mcp` flag. The `--mcp` flag is also mutually exclusive with `--ttrpc`/`--grpc` since they represent different VM management paradigms.

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
| `openvmm/openvmm_entry/src/ttrpc/mod.rs` | Existing gRPC/ttrpc VMService (different paradigm — constructs VMs from gRPC, not CLI) | Contrast reference |
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
