# MCP Server

OpenVMM includes a built-in [Model Context Protocol][mcp] (MCP) server
that allows AI agents to interact with a running VM through structured
tool calls over JSON-RPC 2.0.

To start OpenVMM in MCP mode, pass `--mcp`:

```bash
openvmm --mcp [other flags...]
```

In this mode, OpenVMM reads JSON-RPC messages from stdin and writes
responses to stdout. Serial console output and logs go to stderr.
The interactive console is replaced by the MCP protocol — the VM is
configured from the same CLI flags as usual.

```admonish note
`--mcp` conflicts with `--gdb`, `--ttrpc`, and `--grpc`. Only one
management interface can be active at a time.
```

## Protocol

The MCP server implements the [MCP specification][mcp-spec] (version
`2024-11-05`) over stdio transport. The handshake is:

1. Client sends `initialize` → server responds with capabilities
2. Client sends `notifications/initialized`
3. Client sends `tools/list` → server responds with tool definitions
4. Client sends `tools/call` → server responds with tool result

Each message is a single line of JSON followed by a newline.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"2024-11-05",
  "capabilities":{},
  "clientInfo":{"name":"my-agent","version":"0.1"}
}}
```

## Available Tools

### VM Lifecycle

| Tool | Description |
|---|---|
| `vm/status` | Current state: running, paused, or halted |
| `vm/pause` | Pause execution |
| `vm/resume` | Resume paused VM |
| `vm/reset` | Power-cycle the VM |
| `vm/nmi` | Inject NMI to a virtual processor |
| `vm/clear_halt` | Clear a halt so the VM can resume |
| `vm/wait_for_halt` | Block until the VM halts or timeout |

### Serial Console

| Tool | Description |
|---|---|
| `serial/read` | Read output since a cursor position |
| `serial/write` | Write text to COM1 input |
| `serial/execute` | Write a command, wait for prompt, return output |

`serial/execute` is the recommended way to run commands in the guest.
It writes the command, then polls the serial ring buffer until a shell
prompt is detected (default patterns: `# `, `$ `, `> `, `login: `,
`Password: `) or the timeout expires. A custom prompt pattern can be
provided via the `prompt_pattern` parameter.

### Inspect Tree

| Tool | Description |
|---|---|
| `inspect/tree` | Query the inspect tree at a path with depth |
| `inspect/get` | Get a single value |
| `inspect/update` | Update a mutable value |

These expose the same inspect infrastructure available via the `x`
command in the [interactive console](./interactive_console.md).

## Example Session

A minimal session that boots a VM and runs a command:

```bash
openvmm --mcp -k vmlinux -r initramfs --hv -m 512M \
  -c "root=/dev/vda2 console=ttyS0"
```

```json
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{
     "protocolVersion":"2024-11-05","capabilities":{},
     "clientInfo":{"name":"example"}}}
← {"jsonrpc":"2.0","id":1,"result":{
     "protocolVersion":"2024-11-05",
     "capabilities":{"tools":{"listChanged":false}},
     "serverInfo":{"name":"openvmm-mcp","version":"0.0.0"}}}

→ {"jsonrpc":"2.0","method":"notifications/initialized"}

→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
     "name":"serial/execute",
     "arguments":{"command":"uname -a","timeout_ms":10000}}}
← {"jsonrpc":"2.0","id":2,"result":{
     "content":[{"type":"text","text":"{\"output\":\"...\",
       \"cursor\":12345,\"timed_out\":false}"}]}}
```

## Architecture

The MCP server runs in the same process as the VM worker. It replaces
the interactive console's `run_control()` loop with an async event
loop that multiplexes:

- **stdin** — incoming JSON-RPC messages
- **halt notifications** — VM halt events from the worker
- **pending tools** — concurrent tool call futures

Tool calls run concurrently via `FuturesUnordered`, so long-running
tools like `vm/wait_for_halt` and `serial/execute` do not block event
processing.

Serial output is captured in an in-process 64 KB ring buffer. The
`serial/read` tool provides cursor-based pagination to avoid
re-reading old data.

## Crate

The implementation lives in the `openvmm_mcp` crate
(`openvmm/openvmm_mcp/`). The entry point wired into `openvmm_entry`
is `openvmm_mcp::run_mcp_server()`.

[mcp]: https://modelcontextprotocol.io
[mcp-spec]: https://spec.modelcontextprotocol.io/specification/2024-11-05/
