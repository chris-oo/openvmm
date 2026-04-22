# Plan: Refactor MCP Server to Use VmController

## Problem

The MCP server manages VM lifecycle directly — `VmHandle` holds a raw
`WorkerHandle` and `mesh::Sender<VmRpc>`, manually tracks halt/pause state, and
has no visibility into worker crashes. PR #3259 introduced `VmController` which
centralizes all of this; the REPL and ttrpc server already use it. MCP is the
only frontend that doesn't.

## Approach

Follow the same pattern as the ttrpc server:
- **VmController** owns `WorkerHandle`, `VmmMesh`, and exclusive resources
- **MCP** holds `mesh::Sender<VmControllerRpc>` for inspect/quit, direct
  `mesh::Sender<VmRpc>` for pause/resume/reset/nmi/memory ops, and subscribes
  to `mesh::Receiver<VmControllerEvent>` for halt/worker-stopped events
- Serial buffer and console_in remain MCP-specific

Key design decisions (informed by rubber-duck review):
1. **Keep direct `vm_rpc`** for pause/resume/reset/nmi/clear_halt/read_memory/write_memory
   (same as ttrpc — VmController doesn't wrap these)
2. **Keep local pause tracking** in VmHandle — VmControllerEvent has no
   pause/resume events, so we need it for `vm/status`
3. **Fix the `wait_for_halt` lost-wakeup race** — make waiter registration
   atomic with the halt-state check (single locked method)
4. **Handle `WorkerStopped`** — drain halt waiters with error, mark VM
   unusable, fail subsequent tool calls fast

## Todos

### 1. wire-mcp-through-controller
**Wire MCP through VmController in `openvmm_entry/src/lib.rs`**

At the MCP entry point (lib.rs:2323-2331), create VmController + channels
like the REPL path does, then pass them to a new `McpResources` struct.
Take `mesh_slot` for VmController ownership. Pass `driver` for spawning
the controller task.

### 2. refactor-vmhandle
**Refactor VmHandle to accept VmController channels**

- Remove `worker: WorkerHandle` (VmController owns it)
- Add `controller_rpc: mesh::Sender<VmControllerRpc>` for inspect/quit
- Keep `vm_rpc: mesh::Sender<VmRpc>` for direct ops
- Keep `serial_buffer` and `console_in`
- Keep `paused: AtomicBool` for `vm/status`
- Keep halt-waiter pattern but **fix lost-wakeup race**: new
  `register_halt_waiter_or_get_reason()` method that atomically checks
  halted state + registers under a single lock
- Add `worker_stopped: AtomicBool` + `worker_error: Mutex<Option<String>>`
  to track worker death

Depends on: wire-mcp-through-controller

### 3. update-event-loop
**Update event loop to consume VmControllerEvent**

Replace `halt_recv: mesh::Receiver<HaltReason>` with
`events: mesh::Receiver<VmControllerEvent>`:
- `GuestHalt(reason)` → call `vm.set_halted(reason)`, drain waiters
- `WorkerStopped { error }` → set `vm.worker_stopped`, drain halt waiters
  with error signal, break event loop
- On stdin close → send `VmControllerRpc::Quit`, await controller task

Depends on: refactor-vmhandle

### 4. update-inspect-tools
**Route inspect through VmControllerRpc::Inspect**

Instead of `req.respond().field("vm", &vm.worker)`, send
`VmControllerRpc::Inspect(InspectTarget::Host, req.defer())` through
`controller_rpc`. This gives richer inspection (mesh + vm + vnc + gdb)
matching what ttrpc provides.

Depends on: refactor-vmhandle

### 5. update-lifecycle-tools
**Update lifecycle tools for new VmHandle API**

- Pause/resume/reset/nmi/clear_halt/memory: keep direct `vm_rpc` calls
- `vm/wait_for_halt`: use new atomic `register_halt_waiter_or_get_reason()`
- `vm/status`: keep local pause tracking, add `worker_stopped` to status
- All tools: check `vm.is_worker_stopped()` first, fail fast if true

Depends on: update-event-loop

### 6. update-run-mcp-server-signature
**Update `run_mcp_server` to accept McpResources bundle**

Change signature from `(VmHandle, Receiver<HaltReason>)` to a resource
struct that carries everything needed:
- `vm_rpc`
- `controller_rpc`
- `controller_events`
- `controller_task: pal_async::task::Task<()>`
- serial buffer + console_in

Depends on: wire-mcp-through-controller

### 7. add-unit-tests
**Add/update unit tests for new VmHandle behavior**

New unit tests in `vm_handle.rs`:
- **`halt_waiter_race_free`** — register waiter on an already-halted VM, assert
  it returns the reason immediately (not via receiver). Validates the
  `register_halt_waiter_or_get_reason()` atomic check.
- **`halt_waiter_receives_after_registration`** — register waiter on running VM,
  then set_halted, assert receiver gets reason. (Existing test covers similar
  but this uses the new API.)
- **`worker_stopped_drains_halt_waiters`** — register halt waiter, then call
  `set_worker_stopped(Some("boom"))`, assert the waiter receives the error
  signal (not a normal halt reason).
- **`worker_stopped_blocks_new_waiters`** — call `set_worker_stopped`, then try
  `register_halt_waiter_or_get_reason()`, assert it returns an error
  immediately.

Update existing tests to use new API surface if method signatures changed.

Depends on: refactor-vmhandle

### 8. validate-build
**Build, clippy, doc, fmt**

Run the full pre-commit validation loop:
1. `cargo check -p openvmm_mcp -p openvmm_entry`
2. `cargo clippy --all-targets -p openvmm_mcp -p openvmm_entry`
3. `cargo doc --no-deps -p openvmm_mcp -p openvmm_entry`
4. `cargo xtask fmt --fix`

Depends on: all above

### 9. run-unit-tests
**Run unit tests for both crates**

`cargo nextest run --profile agent -p openvmm_mcp -p openvmm_entry`

Verify all existing tests still pass (serial_buffer, serial tools, halt
waiters) and all new tests pass.

Depends on: validate-build

### 10. run-integration-test
**Run `scripts/test_mcp.py` end-to-end**

This requires a built openvmm binary and Alpine linux-direct artifacts.
Build openvmm, then run:
```
python3 scripts/test_mcp.py
```

This exercises the full MCP flow: initialize handshake, tools/list,
vm/status, inspect/tree, serial read/write/execute, vm/pause/resume,
vm/nmi, vm/wait_for_halt timeout, and error handling. All 20+ checks
must pass.

If Alpine artifacts aren't available, at minimum validate that the MCP
server starts and completes the handshake without crashing (smoke test
by piping initialize + tools/list).

Depends on: validate-build

## Validation Summary

| Layer | What | How |
|-------|------|-----|
| **Compile** | Type-checks all changes | `cargo check -p openvmm_mcp -p openvmm_entry` |
| **Lint** | No clippy warnings | `cargo clippy --all-targets -p ...` |
| **Docs** | No doc warnings | `cargo doc --no-deps -p ...` |
| **Format** | Passes fmt | `cargo xtask fmt --fix` |
| **Unit tests** | VmHandle halt race fix, worker-stopped state | `cargo nextest run --profile agent -p openvmm_mcp` |
| **Unit tests** | openvmm_entry still builds/tests | `cargo nextest run --profile agent -p openvmm_entry` |
| **Integration** | Full MCP protocol exercised against real VM | `python3 scripts/test_mcp.py` |

## Notes

- The ttrpc server keeps a direct `vm_rpc` reference (`Vm::worker_rpc`)
  alongside VmController channels — we follow the same pattern
- VmController.run() breaks on WorkerStopped/Failed events, so the
  controller task will end when the worker dies
- The MCP crate's `parking_lot` dependency can be removed if we eliminate
  `worker: WorkerHandle` (the main reason it was added for Mutex-guarded
  console_in can stay)
