# Phase 1 Fixes — openvmm-mcp

Issues found during hands-on testing of the MCP server (Phase 1) that should
be fixed before moving to Phase 2+.

---

## Fix 1: Inspect tree root doesn't match interactive console

**Severity:** High — agents and users see different paths, docs/examples won't transfer.

**Problem:** The interactive console wraps the VM worker in a composite inspect
root with named fields:

```rust
// openvmm_entry/src/lib.rs — inspect_obj()
resp.field("mesh", mesh)
    .field("vm", vm_worker)    // ← worker is a child under "vm"
    .field("vnc", vnc_worker)
    .field("gdb", gdb_worker);
```

The MCP server passes `vm.worker` directly as the inspect root:

```rust
// openvmm_mcp/src/tools/inspect.rs
InspectionBuilder::new(&path).inspect(&vm.worker);
```

**Effect:** `inspect vm/chipset` in the console → `inspect/tree path="chipset"` in
MCP. The `"vm"` prefix doesn't exist in MCP because the worker *is* the root.
The tool description even suggests `"vm"` as an example path, which returns
`{"$error": "not found"}`.

**Fix:** Wrap the worker in the same ad-hoc composite that the console uses. The
MCP server needs access to the same set of inspect sources (mesh, vnc_worker,
gdb_worker) or at minimum should nest the worker under a `"vm"` field so paths
match. At a minimum:

```rust
let obj = inspect::adhoc(|req| {
    req.respond().field("vm", &self.worker);
});
InspectionBuilder::new(&path).inspect(obj);
```

If vnc/gdb workers are available, include them too. Update the tool description
to list accurate example paths.

---

## Fix 2: vm/status doesn't reflect paused state

**Severity:** Medium — agents can't determine if the VM is paused.

**Problem:** `VmHandle` only tracks halt state (`halted: AtomicBool`). The
`vm/status` tool reports:

```rust
let status = if halted { "halted" } else { "running" };
```

After a successful `vm/pause` (which does work — VPs stop executing), status
still returns `"running"` because "paused" ≠ "halted".

**Fix:** Add a `paused: AtomicBool` to `VmHandle`. Set it to `true` on
successful pause, `false` on successful resume/reset. Update `vm/status`:

```rust
let status = if halted {
    "halted"
} else if paused {
    "paused"
} else {
    "running"
};
```

The interactive console has the same gap (it doesn't track paused state either)
but it's less critical there because the user sees "pause complete" printed
inline.

---

## Fix 3: Tool descriptions reference non-existent inspect paths

**Severity:** Low — misleading for agents trying to discover the inspect tree.

**Problem:** The `inspect/tree` tool description says:

> `path: "Inspect path (e.g. 'vm' or 'vm/chipset')"`

These paths don't exist with the current root. Actual top-level nodes are
`partition`, `chipset`, `serial-com1`, `vmbus`, etc.

**Fix:** After fixing the inspect root (Fix 1), update the example paths to
match. If Fix 1 nests the worker under `"vm"`, the current examples become
correct. Otherwise update to reflect actual paths like `"partition"`,
`"chipset"`.

---

## Fix 4: ANSI escape sequences in serial output

**Severity:** Low — cosmetic, but agents must strip control codes to parse output.

**Problem:** Serial output includes raw ANSI escape sequences from the guest
terminal (e.g. `\x1b[6n`, `\x1b[40;14R`). These are cursor position
request/response sequences from the guest shell.

**Fix options (pick one):**
- Strip ANSI escapes in `serial/read` before returning text (regex: `\x1b\[[0-9;]*[A-Za-z]`)
- Add an `raw: bool` parameter to `serial/read` — default to stripped, allow raw
- Document that serial output may contain ANSI escapes (lowest effort)

Stripping by default is recommended since AI agents are the primary consumer.
