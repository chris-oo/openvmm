# OpenVMM MCP Server — Implementation Plan

## Implementation Status

> **Last updated:** 2026-04-22

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

### ✅ VmController Refactor — COMPLETE

Refactored the MCP server to use `VmController` (from PR #3259) instead of
directly holding a `WorkerHandle` and raw `VmRpc` sender. This brings MCP
in line with how the ttrpc and REPL frontends manage VM lifecycle.

**Changes:**
- VmController now owns the WorkerHandle, VmmMesh, and worker lifecycle
- MCP receives `VmControllerEvent` via a bridge task for halt and
  worker-stopped notifications
- Inspect routes through `VmControllerRpc::Inspect` via a callback,
  giving richer inspection (mesh + vm + vnc + gdb workers)
- Pause/resume/reset/nmi/memory ops remain as direct VmRpc calls
  (same pattern as ttrpc)
- Fixed a lost-wakeup race in `wait_for_halt`: halt-state check and
  waiter registration are now atomic under a single lock
- Added worker-stopped state tracking: pending halt waiters are drained
  with an error when the worker dies, and subsequent tool calls fail fast
- Removed `mesh_worker` and `vmm_core_defs` dependencies from `openvmm_mcp`
- 17 unit tests, 43/43 integration tests passing

### ✅ vm-debugging Skill — COMPLETE

Added `.github/skills/vm-debugging/SKILL.md` — teaches agents how to
launch `openvmm --mcp`, send JSON-RPC commands, and debug VM issues
interactively. Complements the `vmm-tests` skill.

### ✅ MCP Spec Bump — COMPLETE (2024-11-05 → 2025-06-18)

Bumped the MCP protocol to version `2025-06-18`, adopting:
- **Ping support** — `ping` method works before and after initialize
- **Tool titles** — human-friendly `title` field on all 13 tools
- **Tool annotations** — `readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint` on every tool to help clients auto-approve safe tools
- **Structured output** — `outputSchema` on tool definitions and
  `structuredContent` in tool results alongside text content for
  backwards compatibility
- **Unknown-tool error handling** — now returns JSON-RPC protocol error
  instead of `isError` tool result (per 2025-06-18 spec)
- 7 new unit tests (24 total), updated integration test script and Guide docs

### ⏳ Next — Display Screenshot
- `display/screenshot` tool (framebuffer → PNG)

### ⏳ Then — Memory Tools
- `memory/read` and `memory/write` tools (raw GPA access via VmRpc)

### ⏳ Then — Disk & Snapshot
- `disk/add` and `disk/remove` tools
- `snapshot/save` tool

### ⏳ Then — MCP Resources
- `vm://config`, `vm://status`, `vm://serial/log`

### ⏳ Later — GDB Debug Integration
- Debug channel wiring (`DebugRequest`)
- `debug/break`, `debug/continue`, `debug/get_registers`, `debug/set_registers`
- `debug/read_memory`, `debug/write_memory`
- `debug/set_breakpoint`, `debug/clear_breakpoint`, `debug/single_step`
- `debug/backtrace` (heuristic stack walker)

### ⏳ Deferred — Guest Agent Tools (Windows Guest Support)

The current `serial/execute` approach only works for Linux guests (shell
prompt on COM1). Windows guests don't expose a shell over the serial
console, so guest command execution requires a different approach.

**Recommended path:** Wire pipette into the embedded MCP server. The
building blocks already exist in the repo:

- **pipette** (`petri/pipette/`) — lightweight guest agent, runs as a
  Windows service or Linux daemon
- **pipette_protocol** (`petri/pipette_protocol/`) — mesh RPC protocol
  over vsock (port `0x1337`)
- **pipette_client** (`petri/pipette_client/`) — host-side client library
- **IMC hive injection** (`petri/make_imc_hive/`) — auto-configures
  pipette as a Windows service via registry hive injected over VmBus

New MCP tools to add:
- `guest/execute` — run a command in the guest via pipette (works for
  both Windows and Linux, unlike `serial/execute`)
- `guest/read_file` — read a file from the guest filesystem
- `guest/write_file` — write a file to the guest filesystem

Implementation work:
1. Add vsock listener setup to the `--mcp` CLI path (plumbing exists in
   `petri/src/vm/openvmm/construct.rs`)
2. Add `pipette_client` as a dependency of `openvmm_mcp`
3. Implement `guest/*` MCP tools that proxy to pipette RPCs
4. Document how to prepare a Windows guest image with pipette
5. Optionally support IMC hive injection from the CLI for zero-prep boot

**Alternative lower-effort options:**
- **EMS/SAC** — Windows Server's Emergency Management Services exposes a
  limited command prompt over COM1. Works with existing `serial/execute`
  using `prompt_pattern: "SAC>"`, but only on Server SKUs and the shell
  is very limited.
- **`display/screenshot`** — once implemented, gives visual interaction
  with Windows guests (read screen + type via serial/write). Usable for
  simple tasks without guest-side software.

### ⏳ Deferred — Petri-MCP Orchestrator
- Standalone `petri_mcp` binary for multi-VM lifecycle management
- `vm/create`, `vm/start`, `vm/destroy`, `vm/list`
- Guest agent tools via pipette (subsumed by the above if wired into
  the embedded server first)

---

## 1. Overview

OpenVMM includes a built-in **Model Context Protocol (MCP) server** that
enables AI agents to interact with a running VM through structured tool
calls over JSON-RPC 2.0. The MCP server exposes VM lifecycle management,
the inspect tree, and serial console I/O as MCP tools consumable by any
MCP-compatible client (Claude Desktop, VS Code Copilot, custom agents, etc.).

**Why MCP?** MCP is the standard protocol for connecting LLMs to external
tools. A VMM is a uniquely valuable tool for AI agents: agents can run
code in sandboxed environments, inspect OS internals, debug kernel crashes,
and automate integration testing — all without touching the host.

---

## 2. Architecture

### Single-VM embedded MCP server (`openvmm --mcp`)

The MCP server is a CLI mode in the OpenVMM binary that replaces the
interactive `openvmm>` console with an MCP server communicating over stdio.
It uses the same VM setup flow (`vm_config_from_command_line()` → launch VM
worker → drive via `VmRpc` messages and `mesh` channels), but substitutes
MCP JSON-RPC for the readline-based console.

**Use case:** A developer or AI agent attaches to a single running VM to
inspect state, debug guest issues, interact with serial, etc.

### Multi-VM orchestrator (`petri-mcp`) — deferred

A standalone MCP server wrapping the `petri` test framework for multi-VM
lifecycle management. Deferred until a concrete use case drives it.

---

## 3. Current Implementation

### Protocol

- **MCP spec version:** `2025-06-18`
- **Transport:** stdio (line-delimited JSON-RPC 2.0)
- **Handshake:** `initialize` → `notifications/initialized` → `tools/list` / `tools/call`
- **Ping:** Supported before and after initialize
- **Tool annotations:** All tools have `readOnlyHint`, `destructiveHint`,
  `idempotentHint`, `openWorldHint` hints
- **Structured output:** All tools return `outputSchema` definitions and
  `structuredContent` in responses alongside `content[0].text` for backwards
  compatibility
- **Error handling:** Unknown tools return JSON-RPC protocol errors (not
  `isError` tool results)

### Tools (13 implemented)

#### VM Lifecycle

| Tool | Title | Read-only | Description |
|---|---|---|---|
| `vm/status` | Get VM Status | ✅ | Current state: running, paused, halted |
| `vm/pause` | Pause VM | | Pause execution (idempotent) |
| `vm/resume` | Resume VM | | Resume paused VM (idempotent) |
| `vm/reset` | Reset VM | | Power-cycle (destructive) |
| `vm/nmi` | Inject NMI | | Send NMI to a virtual processor |
| `vm/clear_halt` | Clear VM Halt | | Clear halt so VM can resume (idempotent) |
| `vm/wait_for_halt` | Wait for VM Halt | ✅ | Block until halt or timeout |

#### Serial Console

| Tool | Title | Read-only | Description |
|---|---|---|---|
| `serial/read` | Read Serial Output | ✅ | Read since cursor position |
| `serial/write` | Write to Serial Console | | Write text to COM1 |
| `serial/execute` | Execute Serial Command | | Write command, wait for prompt, return output |

#### Inspect Tree

| Tool | Title | Read-only | Description |
|---|---|---|---|
| `inspect/tree` | Inspect Tree | ✅ | Query subtree at path with depth |
| `inspect/get` | Get Inspect Value | ✅ | Get single value |
| `inspect/update` | Update Inspect Value | | Update mutable value |

### Crate structure

```
openvmm/
  openvmm_mcp/
    Cargo.toml
    src/
      lib.rs              # Module exports, re-exports run_mcp_server + VmHandle
      protocol.rs         # MCP JSON-RPC 2.0 types, ToolAnnotations, structured output
      transport.rs        # Stdin reader thread + stdout JSON writer
      tools/
        mod.rs            # Tool registry (fn-pointer based) + dispatch
        lifecycle.rs      # pause/resume/reset/nmi/clear_halt/status/wait_for_halt
        inspect.rs        # inspect tree query/get/update via InspectionBuilder
        serial.rs         # serial read (ring buffer) / write (sync fd) / execute
      event_loop.rs       # Async event loop: merge(stdin, controller events, pending tools)
      vm_handle.rs        # VmHandle: VmRpc + inspect callback + serial + halt waiters
      serial_buffer.rs    # Thread-safe 64KB ring buffer with cursor reads
```

### Integration with openvmm_entry

- `--mcp` CLI flag with `conflicts_with_all = ["gdb", "ttrpc", "grpc"]`
- Serial output teed to both ring buffer AND stderr via dup'd fd
- Synchronous console_in writer (dup'd unix socket fd)
- VmController manages VM lifecycle (same pattern as REPL/ttrpc)
- Bridge task converts `VmControllerEvent` → `openvmm_mcp::VmEvent`
- Inspect routes through `VmControllerRpc::Inspect`

### Testing

- **24 unit tests** — serial ring buffer, prompt detection, ANSI stripping,
  halt waiter mechanism, protocol type serialization
- **Integration test** — `scripts/test_mcp.py` launches openvmm with Alpine
  linux-direct, exercises all tools, validates structured output and annotations
- **vm-debugging skill** — `.github/skills/vm-debugging/SKILL.md` teaches
  agents to use the MCP server interactively

### Key design decisions

- Hand-rolled MCP JSON-RPC protocol (no external SDK) — ~300 lines, full control
- `parking_lot::Mutex` per project convention
- Serial fd is dup'd from PolledSocket for synchronous writes (wrong epoll instance otherwise)
- Tool calls run concurrently via `FuturesUnordered`
- Inspect results wrapped in object envelopes (`{path, result}`) for spec-compliant structured output
- Protocol state machine enforced (tools rejected before initialize, ping allowed anytime)

---

## 4. Future Tools

### Display / Screenshot (next priority)

| Tool | Description | Maps to |
|---|---|---|
| `display/screenshot` | Capture framebuffer as PNG | `FramebufferAccess` / `View::read_line()` |
| `display/resolution` | Get current display resolution | `View::resolution()` |

**⚠️ VNC conflict:** `FramebufferAccess` is consumed by VNC worker. In `--mcp`
mode, skip VNC by default. If `--vnc` is explicitly passed, display tools
return an error.

### Memory Tools

| Tool | Description | Maps to |
|---|---|---|
| `memory/read` | Read guest physical memory (hex or base64) | `VmRpc::ReadMemory` |
| `memory/write` | Write guest physical memory | `VmRpc::WriteMemory` |

### Disk Management

| Tool | Description | Maps to |
|---|---|---|
| `disk/add` | Hot-add a SCSI disk | `ScsiControllerRequest::AddDevice` |
| `disk/remove` | Hot-remove a SCSI disk | `ScsiControllerRequest::RemoveDevice` |

### Save/Restore

| Tool | Description | Maps to |
|---|---|---|
| `snapshot/save` | Pause + save VM state | `save_snapshot()` |

### MCP Resources

| Resource URI | Description |
|---|---|
| `vm://config` | Current VM configuration |
| `vm://status` | Current VM state |
| `vm://serial/log` | Recent serial console output |

### GDB Debug Tools

| Tool | Description | Maps to |
|---|---|---|
| `debug/break` | Break into debugger | `DebugRequest::Break` |
| `debug/continue` | Resume execution | `DebugRequest::Resume` |
| `debug/get_registers` | Read VP register state | `DebugRequest::GetVpState` |
| `debug/set_registers` | Write VP register state | `DebugRequest::SetVpState` |
| `debug/read_memory` | Read guest virtual memory | `DebugRequest::ReadMemory` (GVA) |
| `debug/read_physical` | Read guest physical memory | `DebugRequest::ReadMemory` (GPA) |
| `debug/write_memory` | Write guest memory | `DebugRequest::WriteMemory` |
| `debug/set_breakpoint` | Set hardware breakpoint | `DebugRequest::SetDebugState` |
| `debug/clear_breakpoint` | Clear hardware breakpoint | `DebugRequest::SetDebugState` |
| `debug/single_step` | Single-step a VP | `DebugRequest::SetDebugState` + Resume |
| `debug/backtrace` | Walk stack (heuristic) | Register reads + memory walks |

The MCP debug architecture bypasses GDB RSP entirely, talking directly to
`DebugRequest` channels. This gives agents structured JSON register output
instead of GDB's text protocol.

### Guest Agent Tools (requires petri-mcp orchestrator)

| Tool | Description | Maps to |
|---|---|---|
| `guest/execute` | Run command in guest OS | `PipetteClient::command()` |
| `guest/read_file` | Read guest filesystem file | `PipetteClient::read_file()` |
| `guest/write_file` | Write guest filesystem file | `PipetteClient::write_file()` |

---

## 5. Implementation Priorities

| Priority | Feature | Status |
|---|---|---|
| ✅ | Core MCP infrastructure (13 tools) | Complete |
| ✅ | VmController refactor | Complete |
| ✅ | vm-debugging skill | Complete |
| ✅ | MCP 2025-06-18 spec bump | Complete |
| 1 | `display/screenshot` | Not started |
| 2 | `memory/read` + `memory/write` | Not started |
| 3 | `disk/add`/`remove` + `snapshot/save` | Not started |
| 4 | MCP resources | Not started |
| 5 | GDB debug integration | Not started |
| 6 | Petri-MCP orchestrator | Deferred |

---

## 6. Key Files Reference

| File/Crate | Role |
|---|---|
| `openvmm/openvmm_mcp/` | MCP server crate (protocol, tools, event loop) |
| `openvmm/openvmm_entry/src/lib.rs` | Wire-in point for `--mcp` mode |
| `openvmm/openvmm_entry/src/cli_args.rs` | `--mcp` CLI flag |
| `openvmm/openvmm_defs/src/rpc.rs` | `VmRpc` enum — VM worker RPC |
| `vmm_core/vmm_core_defs/src/debug_rpc.rs` | `DebugRequest` / `DebuggerVpState` |
| `support/inspect/src/initiate.rs` | Inspect framework |
| `scripts/test_mcp.py` | Integration test script |
| `.github/skills/vm-debugging/SKILL.md` | Agent skill for MCP debugging |
| `Guide/src/reference/openvmm/management/mcp.md` | User-facing documentation |

---

## 7. Design Notes for Future Phases

### 7.1 VmHandle Expansion

Future tools will require additional RPC channels wired into VmHandle.
The target shape (fields to add as each phase lands):

```rust
pub struct VmHandle {
    // Currently implemented:
    pub vm_rpc: mesh::Sender<VmRpc>,
    pub inspect_fn: Box<dyn Fn(inspect::Deferred) + Send + Sync>,
    pub serial_buffer: Arc<SerialRingBuffer>,
    pub console_in: parking_lot::Mutex<Option<Box<dyn Write + Send>>>,

    // Future — display/screenshot:
    pub framebuffer: Option<FramebufferAccess>,

    // Future — disk management:
    pub scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,

    // Future — GDB debug:
    pub debug_rpc: Option<mesh::Sender<DebugRequest>>,

    // Future — guest shutdown:
    pub shutdown_ic: Option<mesh::Sender<ShutdownRpc>>,
}
```

### 7.2 Framebuffer / VNC Conflict

`FramebufferAccess` is **consumed** by the VNC worker when `--vnc` is
enabled — only one consumer can hold it.

**Resolution strategy:**
- In `--mcp` mode, skip VNC by default. The MCP client gets screenshots
  directly via `display/screenshot`.
- If `--vnc` is explicitly passed alongside `--mcp`, the display tools
  return an error explaining the conflict.
- **Future:** Share the framebuffer by cloning the underlying `Mappable`
  file descriptor, allowing both VNC and MCP screenshot access.

### 7.3 GDB Debug Integration — Detailed Design

#### Current GDB Architecture

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
2. The `debug_worker` accepts GDB client connections over TCP.
3. The debug worker communicates with the VM via `mesh::Sender<DebugRequest>`.
4. The VM's `partition_unit` processes `DebugRequest` messages.
5. The debug worker implements GDB RSP using the `gdbstub` crate.

#### MCP Debug Architecture

For MCP, we **bypass GDB RSP entirely** and talk directly to `DebugRequest`:

```
[MCP Client (LLM / AI Agent)]
    ↕ JSON-RPC (MCP protocol, stdio)
[MCP Server (openvmm_mcp)]
    ↕ mesh::Sender<DebugRequest>
[Partition Unit (VM worker)]
    ↕ Hypervisor API
[Guest VM]
```

Benefits:
- `DebugRequest` already provides all primitives: `Attach`, `Detach`,
  `Resume`, `Break`, `GetVpState`, `SetVpState`, `ReadMemory`,
  `WriteMemory`, `SetDebugState`
- Agents get structured JSON register output instead of GDB's text protocol
- No need for GDB RSP parsing

#### Creating the debug channel

When `--mcp` is active, before launching the VM worker:
```rust
let (debug_tx, debug_rx) = mesh::channel();
vm_config.debugger_rpc = Some(debug_rx);
// debug_tx goes into VmHandle
```

Mutually exclusive with `--gdb` (both set `Config::debugger_rpc`).

#### Implementation sketches

**`debug/get_registers`:**
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
            // ... r8-r15, rflags, cr0-cr4, segment registers
        })),
        DebuggerVpState::Aarch64(ref s) => Ok(json!({
            "arch": "aarch64",
            "pc": format!("{:#x}", s.pc),
            "sp_el0": format!("{:#x}", s.sp_el0),
            // ... x0-x30
        })),
    }
}
```

**`debug/backtrace` (heuristic stack walker):**
```rust
async fn backtrace(handle: &VmHandle, vp: u32, max_depth: usize) -> Result<Vec<StackFrame>> {
    let regs = get_registers_raw(handle, vp).await?;
    let mut frames = vec![StackFrame { rip: regs.rip, rbp: regs.rbp }];
    let mut rbp = regs.rbp;

    for _ in 0..max_depth {
        if rbp == 0 || rbp % 8 != 0 { break; }
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

**`debug/set_breakpoint`:**
```rust
async fn set_breakpoint(handle: &VmHandle, vp: u32, addr: u64, kind: &str) -> Result<u32> {
    let bp_type = match kind {
        "execute" => BreakpointType::Execute,
        "write" => BreakpointType::Write,
        "read_write" => BreakpointType::ReadOrWrite,
        _ => anyhow::bail!("invalid breakpoint type"),
    };
    let slot = handle.find_free_breakpoint_slot()?;
    let bp = HardwareBreakpoint { address: addr, ty: bp_type, size: BreakpointSize::Bytes1 };
    let state = build_debug_state_with_breakpoint(handle, slot, Some(bp));
    handle.debug_rpc.send(DebugRequest::SetDebugState { vp, state });
    Ok(slot)
}
```

**`debug/single_step`:**
```rust
async fn single_step(handle: &VmHandle, vp: u32) -> Result<StepResult> {
    let debug = handle.debug_rpc.as_ref().context("debug not enabled")?;
    let state = DebugState { single_step: true, breakpoints: current_breakpoints(handle) };
    debug.send(DebugRequest::SetDebugState { vp, state });
    let (tx, rx) = mesh::oneshot();
    debug.send(DebugRequest::Resume { response: tx });
    let reason = rx.await?;
    let regs = get_registers_raw(handle, vp).await?;
    Ok(StepResult { stop_reason: format!("{:?}", reason), registers: regs })
}
```

#### Limitations

| Limitation | Impact | Mitigation |
|---|---|---|
| Hardware breakpoints only (4 on x86) | Can't set many breakpoints | Software breakpoints via `INT 3` patching in future |
| No symbol information | Agent sees raw addresses | Agent loads symbols separately; backtrace provides frame addresses |
| Single-debugger constraint | Can't use `--gdb` and `--mcp` debug simultaneously | Document clearly; could multiplex later |
| GVA reads need VP context | Virtual memory reads require stopped VP | All GVA ops take `vp` parameter |
| No conditional breakpoints | Hardware BPs don't support conditions | Agent single-steps + checks registers |
