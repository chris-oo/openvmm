---
name: vm-debugging
description: "Interactive VM debugging with the MCP server. Load when you need to boot a VM, run guest commands, inspect device state, or reproduce bugs interactively — as opposed to running automated VMM tests."
---

# Interactive VM Debugging

The OpenVMM MCP server lets you boot a VM and interact with it
programmatically over JSON-RPC. Use this for ad-hoc debugging,
reproducing issues, and exploring VM state — things that are hard to
do with automated vmm-tests.

**When to use which:**

| Goal | Tool |
|------|------|
| Run an existing regression test | `vmm-tests` skill |
| Boot a VM and poke around interactively | This skill |
| Reproduce a specific bug with a live guest | This skill |
| Inspect device/chipset state at runtime | This skill |
| Validate a code change end-to-end | Either — start here, then write a vmm-test |

## Prerequisites

1. **Linux with KVM** — `--hv` requires `/dev/kvm` access
2. **Alpine guest artifacts** — run `scripts/setup-alpine.sh target/alpine`
   (needs `curl`, `qemu-img`, `python3`, `mkfs.vfat`, `mcopy`, `sudo`)
3. **Built openvmm** — `cargo build -p openvmm`

Check prerequisites:

```bash
ls target/alpine/vmlinux-virt target/alpine/initramfs-virt target/alpine/disk.raw target/alpine/cidata.img
ls target/x86_64-unknown-linux-gnu/debug/openvmm
```

If the Alpine artifacts are missing, run:

```bash
./scripts/setup-alpine.sh target/alpine
```

## Launching the MCP Server

Start openvmm as an **async bash process** so you can send commands
interactively:

```bash
# mode="async" — this runs in the background
target/x86_64-unknown-linux-gnu/debug/openvmm --mcp \
  -k target/alpine/vmlinux-virt \
  -r target/alpine/initramfs-virt \
  --hv -m 512M -p 2 \
  --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G \
  --pcie-root-port rc0:rp0 --pcie-root-port rc0:rp1 \
  --virtio-blk file:target/alpine/disk.raw,pcie_port=rp0 \
  --virtio-blk file:target/alpine/cidata.img,ro,pcie_port=rp1 \
  -c "root=/dev/vda2 rootfstype=ext4 modules=virtio_pci,virtio_blk,ext4 console=ttyS0 clocksource=jiffies"
```

Then send the MCP handshake via `write_bash`:

```
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"copilot-agent","version":"0.1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

Read the response with `read_bash`. You should see the server capabilities.

## JSON-RPC Command Reference

Every command is a single JSON line written to stdin. Responses come back
as single JSON lines on stdout.

### MCP Handshake (required first)

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"agent"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

### Tool Calls

All tool operations use `tools/call`. Increment the `id` for each request.

#### Check VM Status

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"vm/status","arguments":{}}}
```

Returns `{"status": "running"|"paused"|"halted"|"worker_stopped", "halt_reason": ...}`.

#### Run a Guest Command

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"serial/execute","arguments":{"command":"uname -a","timeout_ms":15000}}}
```

Returns `{"output": "...", "cursor": N, "timed_out": false}`.

**Important:** The guest must be booted to a shell prompt for
`serial/execute` to work. Wait for boot first (see workflow below).

#### Read Serial Output (Boot Log)

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"serial/read","arguments":{"cursor":0,"max_bytes":4096}}}
```

Returns `{"text": "...", "cursor": N}`. Use the returned `cursor` in
subsequent reads to get only new output.

#### Write Raw Text to Serial

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"serial/write","arguments":{"text":"root\n"}}}
```

#### Inspect VM State

```json
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"inspect/tree","arguments":{"path":"","depth":1}}}
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"inspect/tree","arguments":{"path":"vm/chipset","depth":2}}}
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"inspect/get","arguments":{"path":"vm/chipset/pic/irr"}}}
```

#### Update a VM Value

```json
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"inspect/update","arguments":{"path":"vm/some/mutable/path","value":"42"}}}
```

#### Pause / Resume / Reset

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"vm/pause","arguments":{}}}
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"vm/resume","arguments":{}}}
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"vm/reset","arguments":{}}}
```

#### Wait for VM Halt

```json
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"vm/wait_for_halt","arguments":{"timeout_ms":60000}}}
```

Blocks until the guest halts (shutdown, triple fault) or times out.

#### Send NMI

```json
{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"vm/nmi","arguments":{"vp":0}}}
```

#### List All Available Tools

```json
{"jsonrpc":"2.0","id":15,"method":"tools/list"}
```

## Common Debugging Workflows

### Boot and Get a Shell

1. Launch openvmm with `--mcp` (async bash)
2. Send the MCP handshake
3. Poll `serial/read` with `cursor: 0` and `max_bytes: 4096` every 5 seconds until you see `login:`
4. Send `serial/write` with `"root\n"` then `"alpine\n"` (password)
5. Now use `serial/execute` to run commands

### Reproduce a Guest Crash

1. Boot the VM (workflow above)
2. Use `serial/execute` to set up the conditions that trigger the crash
3. Use `vm/wait_for_halt` to catch the halt event
4. Inspect the halt reason — `vm/status` shows the halt reason string
5. Use `inspect/tree` at `vm` to examine device state at the time of crash

### Inspect Device State

1. Start with `inspect/tree` at `""` depth 1 to see the root tree
2. Drill into specific subsystems: `vm/chipset`, `vm/virtio`, etc.
3. Use `inspect/get` for specific values
4. Compare with expected state from the code

### Test a Code Change

1. Make your code change
2. `cargo build -p openvmm`
3. Boot the VM with `--mcp`
4. Exercise the changed functionality via serial/inspect
5. Verify behavior matches expectations
6. If it works, consider writing a vmm-test to lock it in

## Shutting Down

Close stdin to the async bash process — this tells the MCP server to
send `Quit` to the VM controller and shut down cleanly. Use `stop_bash`
on the shell ID.

If the process doesn't exit within 5 seconds after closing stdin, kill
it by PID.

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `serial/execute` times out | Guest hasn't booted to a prompt yet. Poll `serial/read` and wait for `login:` |
| `inspect/tree` returns `unresolved` | Timeout too short or path doesn't exist. Try a more specific path |
| `vm/status` returns `worker_stopped` | VM worker crashed. Check stderr for error. Restart openvmm |
| No response after handshake | Make sure each JSON message is on its own line (terminated by `\n`) |
| `/dev/kvm` permission denied | Add your user to the `kvm` group: `sudo usermod -aG kvm $USER` |

## Alpine Guest Credentials

| User | Password |
|------|----------|
| `root` | `alpine` |

Set by the cloud-init disk created by `scripts/setup-alpine.sh`.
