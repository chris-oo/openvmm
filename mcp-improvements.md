# MCP Improvements for IGVM Multi-Context Debugging

These MCP additions would make the lightweight IGVM v2 multi-context validation
usable without switching between MCP and `ohcldiag-dev`.

1. **Paravisor inspect target**
   - Add a `target` parameter, such as `host` or `paravisor`, to
     `inspect/tree`, `inspect/get`, and `inspect/update`, or add dedicated
     `paravisor/inspect/*` tools.
   - Today MCP inspect is wired to OpenVMM host inspect only. For this prototype
     we need to compare OpenHCL release vs debug inspect surfaces: release should
     expose `build_info` only, while debug should expose `vm`, `vm/partition`,
     and the broader partition/object graph.

2. **Paravisor log access**
   - Add an MCP tool equivalent to `ohcldiag-dev kmsg`, including one-shot read
     and follow modes.
   - This would let the manual validation check for the debug-only
     "confidential debug enabled" OpenHCL log without a separate diagnostics
     process.

3. **VTL2 diagnostics readiness**
   - Add a wait tool that blocks until the OpenHCL diagnostics server is
     reachable, or reports a clear timeout/error if VTL2 diag never comes up.
   - This would avoid ad hoc polling around the VTL2 hybrid-vsock path during
     repeated release/debug/default boot attempts.

4. **Integrated VTL2 vsock handling**
   - Expose the configured VTL2 hybrid-vsock path through MCP, and optionally let
     MCP create a temporary listener path for the run.
   - This would reduce Windows/WSL path handling mistakes and make it easier for
     tools to connect to the correct OpenHCL diagnostics endpoint.

5. **Selected IGVM context diagnostics**
   - Expose the selected IGVM context name, compatibility mask, platform header
     type, and selected `vtl2_memory_info` through host inspect or a dedicated MCP
     tool.
   - This would give immediate loader-side proof that `--igvm-context release`,
     `--igvm-context debug`, and default selection chose the expected context.

6. **Log/inspect wait predicates**
   - Add tools such as `logs/wait_for` and `inspect/wait_for` that wait for a log
     pattern or inspect value to appear.
   - This would make manual validation less timing-sensitive when checking the
     confidential-debug warning, the release inspect surface, and debug-only
     paravisor nodes.

7. **Relaunch support with changed VM arguments**
   - Add a controlled stop-and-relaunch workflow so the same MCP client can run
     the same VM command with a different `--igvm-context`.
   - The prototype needs repeated release, debug, default, and fallback boots; a
     relaunch primitive would reduce manual process management.

8. **Structured run summary**
   - Add a tool that returns the effective OpenVMM command/config, important
     artifact paths, selected isolation/VTL2 settings, and current VM lifecycle
     state.
   - This would make implementation notes and bug reports reproducible without
     reconstructing state from command lines and logs.
