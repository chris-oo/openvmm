#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""MCP client test script for exercising the openvmm MCP server.

Launches openvmm --mcp with an Alpine linux-direct guest and exercises
lifecycle, inspect, and serial tools via JSON-RPC over stdio.
"""

import json
import subprocess
import sys
import time
import os

ALPINE_DIR = os.path.join(os.path.dirname(__file__), "..", "target", "alpine")
OPENVMM = os.path.join(os.path.dirname(__file__), "..", "target", "x86_64-unknown-linux-gnu", "debug", "openvmm")

request_id = 0

def make_request(method, params=None):
    global request_id
    request_id += 1
    msg = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params is not None:
        msg["params"] = params
    return msg

def make_notification(method, params=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    return msg

def send(proc, msg):
    line = json.dumps(msg) + "\n"
    proc.stdin.write(line)
    proc.stdin.flush()

def recv(proc, timeout=10):
    """Read one JSON-RPC response line from stdout."""
    import select
    ready, _, _ = select.select([proc.stdout], [], [], timeout)
    if not ready:
        return None
    line = proc.stdout.readline()
    if not line:
        return None
    try:
        return json.loads(line.strip())
    except json.JSONDecodeError:
        return None

def send_and_recv(proc, method, params=None, timeout=10):
    """Send a request and receive the matching response, skipping stale messages."""
    global request_id
    request_id += 1
    msg = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params is not None:
        msg["params"] = params
    line = json.dumps(msg) + "\n"
    proc.stdin.write(line)
    proc.stdin.flush()
    # Read responses until we find one with our id
    deadline = time.time() + timeout
    while time.time() < deadline:
        resp = recv(proc, timeout=max(0.1, deadline - time.time()))
        if resp is None:
            return None
        if resp.get("id") == request_id:
            return resp
        # Skip stale/mismatched responses
    return None

def test_mcp_server():
    kernel = os.path.join(ALPINE_DIR, "vmlinux-virt")
    initrd = os.path.join(ALPINE_DIR, "initramfs-virt")
    disk = os.path.join(ALPINE_DIR, "disk.raw")
    cidata = os.path.join(ALPINE_DIR, "cidata.img")

    for f in [kernel, initrd, disk, cidata]:
        if not os.path.exists(f):
            print(f"ERROR: Missing file: {f}")
            sys.exit(1)

    cmd = [
        OPENVMM,
        "--mcp",
        "-k", kernel,
        "-r", initrd,
        "--hv",
        "-m", "512M",
        "-p", "2",
        "--pcie-root-complex", "rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G",
        "--pcie-root-port", "rc0:rp0",
        "--pcie-root-port", "rc0:rp1",
        "--virtio-blk", f"file:{disk},pcie_port=rp0",
        "--virtio-blk", f"file:{cidata},ro,pcie_port=rp1",
        "-c", "root=/dev/vda2 rootfstype=ext4 modules=virtio_pci,virtio_blk,ext4 console=ttyS0 clocksource=jiffies",
    ]

    print(f"Launching: {' '.join(cmd[:5])}...")
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )

    results = {"passed": 0, "failed": 0, "tests": []}

    def check(name, condition, detail=""):
        status = "PASS" if condition else "FAIL"
        results["tests"].append({"name": name, "status": status, "detail": detail})
        if condition:
            results["passed"] += 1
            print(f"  ✓ {name}")
        else:
            results["failed"] += 1
            print(f"  ✗ {name}: {detail}")

    try:
        # --- Test 1: Initialize handshake ---
        print("\n=== MCP Initialize ===")
        send(proc, make_request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "0.1"}
        }))
        resp = recv(proc, timeout=5)
        check("initialize response received", resp is not None, "no response")
        if resp:
            check("initialize has result", "result" in resp, str(resp))
            if "result" in resp:
                r = resp["result"]
                check("server name is openvmm-mcp",
                      r.get("serverInfo", {}).get("name") == "openvmm-mcp",
                      str(r.get("serverInfo")))
                check("has tools capability",
                      "tools" in r.get("capabilities", {}),
                      str(r.get("capabilities")))

        # Send initialized notification
        send(proc, make_notification("notifications/initialized"))
        time.sleep(0.5)

        # --- Test 2: Pre-initialize rejection ---
        # (Already initialized, so this tests the normal flow)

        # --- Test 3: List tools ---
        print("\n=== Tools List ===")
        send(proc, make_request("tools/list"))
        resp = recv(proc, timeout=5)
        check("tools/list response received", resp is not None)
        if resp and "result" in resp:
            tools = resp["result"].get("tools", [])
            tool_names = [t["name"] for t in tools]
            check(f"has tools ({len(tools)} total)", len(tools) > 0, str(tool_names))
            for expected in ["vm/pause", "vm/resume", "vm/status", "inspect/tree", "serial/read", "serial/write"]:
                check(f"tool '{expected}' present", expected in tool_names, str(tool_names))

        # --- Test 4: VM Status ---
        print("\n=== VM Status ===")
        send(proc, make_request("tools/call", {
            "name": "vm/status",
            "arguments": {}
        }))
        resp = recv(proc, timeout=5)
        check("vm/status response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                status_data = json.loads(content[0].get("text", "{}"))
                check("vm status is running", status_data.get("status") == "running",
                      str(status_data))

        # --- Test 5: Inspect tree (while VM boots) ---
        print("\n=== Inspect Tree ===")
        send(proc, make_request("tools/call", {
            "name": "inspect/tree",
            "arguments": {"path": "", "depth": 1}
        }))
        resp = recv(proc, timeout=5)
        check("inspect/tree response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                text = content[0].get("text", "")
                check("inspect/tree returns data", len(text) > 10, f"got {len(text)} chars")
                check("inspect tree has 'vm' node", "vm" in text, text[:200])

        # --- Test 6: Inspect specific path ---
        print("\n=== Inspect Specific Path ===")
        send(proc, make_request("tools/call", {
            "name": "inspect/tree",
            "arguments": {"path": "vm", "depth": 1}
        }))
        resp = recv(proc, timeout=5)
        check("inspect/tree vm response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                text = content[0].get("text", "")
                check("inspect vm subtree has content", len(text) > 5, text[:300])

        # --- Test 7: Wait for boot and test serial ---
        print("\n=== Serial I/O (waiting for boot) ===")
        # Poll serial output until we see a login prompt (max 60s)
        last_cursor = 0
        boot_text = ""
        login_seen = False
        for attempt in range(12):  # 12 * 5s = 60s max
            time.sleep(5)
            send(proc, make_request("tools/call", {
                "name": "serial/read",
                "arguments": {"cursor": last_cursor}
            }))
            resp = recv(proc, timeout=5)
            if resp and "result" in resp:
                content = resp["result"].get("content", [])
                if content:
                    data = json.loads(content[0].get("text", "{}"))
                    text = data.get("text", "")
                    last_cursor = data.get("cursor", last_cursor)
                    boot_text += text
                    if "login:" in boot_text.lower():
                        login_seen = True
                        print(f"  Login prompt seen after {(attempt+1)*5}s ({last_cursor} bytes)")
                        break
            print(f"  ... {(attempt+1)*5}s elapsed, {last_cursor} bytes")

        check("serial output captured", last_cursor > 0,
              f"got {last_cursor} bytes total")
        has_linux = any(kw in boot_text for kw in ["Linux", "linux", "Alpine", "login"])
        check("serial shows boot output", has_linux,
              boot_text[-300:] if boot_text else "(empty)")

        # --- Test 8: Serial write ---
        print("\n=== Serial Write ===")
        send(proc, make_request("tools/call", {
            "name": "serial/write",
            "arguments": {"text": "root\n"}
        }))
        resp = recv(proc, timeout=5)
        check("serial/write response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                data = json.loads(content[0].get("text", "{}"))
                check("serial/write reports success", data.get("written") == True, str(data))

        if login_seen:
            time.sleep(5)
            send(proc, make_request("tools/call", {
                "name": "serial/read",
                "arguments": {"cursor": last_cursor}
            }))
            resp = recv(proc, timeout=5)
            if resp and "result" in resp:
                content = resp["result"].get("content", [])
                if content:
                    data = json.loads(content[0].get("text", "{}"))
                    text = data.get("text", "")
                    new_cursor = data.get("cursor", 0)
                    check("serial shows response after write", len(text) > 0,
                          f"cursor={last_cursor}->{new_cursor}, text='{text[:200]}'")

        # --- Test 9: VM Pause/Resume ---
        print("\n=== VM Pause/Resume ===")
        send(proc, make_request("tools/call", {
            "name": "vm/pause",
            "arguments": {}
        }))
        resp = recv(proc, timeout=5)
        check("vm/pause response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                data = json.loads(content[0].get("text", "{}"))
                check("vm/pause reports paused", data.get("paused") == True, str(data))

        send(proc, make_request("tools/call", {
            "name": "vm/resume",
            "arguments": {}
        }))
        resp = recv(proc, timeout=5)
        check("vm/resume response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                data = json.loads(content[0].get("text", "{}"))
                check("vm/resume reports resumed", data.get("resumed") == True, str(data))

        # --- Test 10: NMI ---
        print("\n=== NMI ===")
        send(proc, make_request("tools/call", {
            "name": "vm/nmi",
            "arguments": {"vp": 0}
        }))
        resp = recv(proc, timeout=5)
        check("vm/nmi response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                data = json.loads(content[0].get("text", "{}"))
                check("vm/nmi reports sent", data.get("nmi_sent") == True, str(data))

        # --- Test 11: Unknown tool ---
        print("\n=== Error Handling ===")
        send(proc, make_request("tools/call", {
            "name": "nonexistent/tool",
            "arguments": {}
        }))
        resp = recv(proc, timeout=5)
        check("unknown tool response received", resp is not None)
        if resp and "result" in resp:
            content = resp["result"].get("content", [])
            if content:
                is_error = resp["result"].get("isError", False)
                check("unknown tool returns error", is_error, str(resp["result"]))

        # --- Test 12: Unknown method ---
        send(proc, make_request("bogus/method"))
        resp = recv(proc, timeout=5)
        check("unknown method response received", resp is not None)
        if resp:
            check("unknown method returns error", "error" in resp, str(resp))

    finally:
        print("\n=== Cleanup ===")
        proc.stdin.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        print("  openvmm process terminated")

    # --- Summary ---
    print(f"\n{'='*50}")
    print(f"Results: {results['passed']} passed, {results['failed']} failed")
    print(f"{'='*50}")

    if results["failed"] > 0:
        print("\nFailed tests:")
        for t in results["tests"]:
            if t["status"] == "FAIL":
                print(f"  - {t['name']}: {t['detail']}")
        return 1
    return 0

if __name__ == "__main__":
    sys.exit(test_mcp_server())
