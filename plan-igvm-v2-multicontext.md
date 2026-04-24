# IGVM v2 multi-context OpenHCL prototype plan

## Goal

Build an end-to-end prototype that can generate and load one OpenHCL IGVM file
containing separate debug-only and release-only contexts, selected through the
new supported-platform v2 requirements header. The prototype should also define
the v1 fallback story explicitly so existing v1 consumers continue to have a
usable path.

## End-state target

The end result should be a **single deployable OpenHCL IGVM file** that contains
all supported hardware/security platforms and both debug flavors:

| Context | Platform type | Debug requirement | Purpose |
|---|---|---|---|
| `vbs-release` | `VSM_ISOLATION` | debug disabled | VBS/OpenVMM release path |
| `vbs-debug` | `VSM_ISOLATION` | debug enabled | VBS/OpenVMM debug path |
| `snp-release` | `SEV_SNP` | debug disabled | SNP release path |
| `snp-debug` | `SEV_SNP` | debug enabled | SNP debug path |
| `tdx-release` | `TDX` | debug disabled | TDX release path |
| `tdx-debug` | `TDX` | debug enabled | TDX debug path |

Concretely, the final single file should contain six
`IGVM_VHS_SUPPORTED_PLATFORM_V2` headers, each with a unique compatibility mask
and requirements describing its platform/debug constraints. Directives,
initialization headers, command lines, required memory, and measurement inputs
must be masked so selecting one header loads exactly the matching context.

The file should also contain one v1 supported-platform fallback header per
hardware isolation architecture. Each v1 header shares the compatibility mask of
the release context for that platform:

| V1 fallback header | Shares mask with |
|---|---|
| `VSM_ISOLATION` | `vbs-release` |
| `SEV_SNP` | `snp-release` |
| `TDX` | `tdx-release` |

These v1 headers are the same-file default/fallback paths for loaders that
understand this mixed v1/v2 layout and select by hardware platform.

The temporary sibling `foo-fallback-<platform>-<context>.igvm` output described
below is only a prototype/transition artifact for already-shipped old loaders
that cannot skip or parse v2 platform headers. It is **not** the desired final packaging
model. To make the final single-file result work for older loader scenarios, the
IGVM spec/parser story must settle either on safe-ignore behavior for v2 headers
or on requiring those loaders to be updated before consuming the unified file.

## Current findings

### Local IGVM header changes

The local `~/ai/leafeon/igvm` repository is at jj revision
`noqsrlnoplpsovksuzxmtwwxrwllmoxv` /
`5e655407b6577895683611a4306b1b4492afe9e1` with description
`igvm_defs: define new supported platform with requirement bits`.

The commit adds `IGVM_VHS_SUPPORTED_PLATFORM_V2` and
`SupportedPlatformRequirements` in `igvm_defs/src/lib.rs`, but it does not yet
add a variable-header type for the v2 structure. The existing v1 supported
platform header is still `IGVM_VHT_SUPPORTED_PLATFORM = 0x1`, and the platform
header type range is `0x1..=0x100`.

Relevant code:

- `../igvm/igvm_defs/src/lib.rs:233-239` defines only the v1 supported platform
  header type.
- `../igvm/igvm_defs/src/lib.rs:423-441` defines
  `IGVM_VHS_SUPPORTED_PLATFORM`.
- `../igvm/igvm_defs/src/lib.rs:444-470` defines
  `IGVM_VHS_SUPPORTED_PLATFORM_V2`; the field is currently misspelled
  `requirments`.
- `../igvm/igvm_defs/src/lib.rs:473-500` defines debug and migration
  requirement bits. The current comments are logically consistent with the
  `reject_*` field names: a set bit rejects that state.

Use those reject-bit semantics throughout the prototype:

- debug-only means `reject_debug_disabled = 1` and
  `reject_debug_enabled = 0`;
- release-only means `reject_debug_enabled = 1` and
  `reject_debug_disabled = 0`;
- debug-flexible means both bits are zero.

### IGVM crate parser/writer gaps

The `igvm` crate only exposes a v1 platform-header variant today. Parsing
accepts only `IGVM_VHT_SUPPORTED_PLATFORM` with the v1 structure size, and
serialization emits only v1 platform headers.

Relevant code:

- `../igvm/igvm/src/lib.rs:181-183` has only
  `IgvmPlatformHeader::SupportedPlatform`.
- `../igvm/igvm/src/lib.rs:207-272` validates only v1 platform headers.
- `../igvm/igvm/src/lib.rs:277-292` rejects anything except a v1 supported
  platform header with the v1 length.
- `../igvm/igvm/src/lib.rs:300-314` writes only v1 supported platform headers.
- `../igvm/igvm/src/lib.rs:2357-2383` rejects multiple platform headers with
  the same `platform_type`.
- `../igvm/igvm/src/lib.rs:2875-3008` parses the variable-header section and
  optionally filters directives by one isolation-type mask during parsing.
- `../igvm/igvm/src/lib.rs:3153-3440` merges IGVM files and rewrites
  compatibility masks; this is useful for multi-context files, but it currently
  matches only the v1 platform-header variant.

Current fallback compatibility is not automatic. Older `igvm` parsers will
fail if they see an unknown platform header type or a supported-platform header
with an unexpected length. The format comment says the high bit can indicate a
loader may safely ignore a structure, but the current parser does not implement
that skip path for unknown headers. A single file containing both v1 and v2
platform headers therefore does not, by itself, guarantee older-loader fallback.

### OpenVMM dependency and igvmfilegen state

OpenVMM currently consumes published `igvm = "0.4.0"` and
`igvm_defs = "0.4.0"` through workspace dependencies
(`Cargo.toml:502-503`). The `igvmfilegen` crate depends on those workspace
dependencies (`vm/loader/igvmfilegen/Cargo.toml:24-25`).

The igvmfilegen schema is a simple list of guest configs:

- `vm/loader/igvmfilegen_config/src/lib.rs:47-81` defines
  `ConfigIsolationType`, including `enable_debug` for VBS/SNP/TDX.
- `vm/loader/igvmfilegen_config/src/lib.rs:84-116` defines `Image::Openhcl`,
  including the embedded OpenHCL command line and memory size.
- `vm/loader/igvmfilegen_config/src/lib.rs:162-195` defines `GuestConfig` and
  `Config { guest_arch, guest_configs }`.

Generation currently processes each `GuestConfig`, builds an `IgvmLoader`,
finalizes a one-platform-header IGVM file, and then `merge_simple`s it into the
aggregate file:

- `vm/loader/igvmfilegen/src/main.rs:168-230` performs per-guest generation
  and merging.
- `vm/loader/igvmfilegen/src/file_loader.rs:496-543` creates one
  `platform_header` per loader.
- `vm/loader/igvmfilegen/src/file_loader.rs:706-713` creates the final
  `IgvmFile` with `vec![self.platform_header]`.
- `vm/loader/igvmfilegen/src/file_loader.rs:776-784` already computes whether
  the generated context has confidential debug enabled.

The existing output side will need labels for multi-context builds. Measurement
JSON files are currently named only by isolation string, for example `-vbs.json`
(`vm/loader/igvmfilegen/src/main.rs:234-259`), which will collide if one file
contains both debug and release VBS contexts.

### Existing debug/release inputs

The current manifests already encode distinct OpenHCL debug/release behavior,
but as separate files:

- `vm/loader/manifests/openhcl-x64-dev.json:9-11` uses
  `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=debug` and a larger VTL2 memory size.
- `vm/loader/manifests/openhcl-x64-release.json:9-11` uses the release GPA
  pool and a smaller VTL2 memory size.
- `vm/loader/manifests/openhcl-x64-cvm-dev.json:7-18` enables SNP debug and
  passes `OPENHCL_CONFIDENTIAL_DEBUG=1`; the same file also has TDX and VBS
  entries.
- `vm/loader/manifests/openhcl-x64-cvm-release.json:7-18` disables SNP debug
  and uses an empty OpenHCL command line; the same pattern applies to TDX and
  VBS.

OpenHCL boot command-line parsing recognizes
`OPENHCL_CONFIDENTIAL_DEBUG` and `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG`
(`openhcl/openhcl_boot/src/cmdline.rs:111-146`).

### OpenVMM loader and CLI state

OpenVMM parses IGVM files through
`openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:130-136` using
`IgvmFile::new_from_binary(..., Some(IsolationType::Vbs))`. That parse-time
isolation filter is too early for a multi-context VBS file because it selects
one VBS compatibility mask before the loader has a debug/release selector.

The runtime loader currently selects the first VSM isolation platform header:

- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:141-151` finds the first
  `VSM_ISOLATION` platform.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:153-162` uses that
  platform for relocation support.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:168-224` uses that
  platform for VTL2 memory sizing.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:613-620` uses that
  platform's compatibility mask for the actual load.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:829-875` filters some
  directives for VTL2-only relocation-related loading and asserts mask
  compatibility. The real compatibility-mask filtering currently happens
  primarily during IGVM parsing through `new_from_binary(..., Some(Vbs))`.

The serializable load-mode config has no IGVM context selector today:
`openvmm/openvmm_defs/src/config.rs:169-174` has only `file`, `cmdline`,
`vtl2_base_address`, and `com_serial`.

The CLI has `--igvm` and `--igvm-vtl2-relocation-type`, but no context selector:
`openvmm/openvmm_entry/src/cli_args.rs:392-399`.
`openvmm/openvmm_entry/src/lib.rs:907-922` constructs `LoadMode::Igvm` from
those flags.

The Petri OpenVMM builder also constructs `LoadMode::Igvm` with no selector
(`petri/src/vm/openvmm/construct.rs:865-876`), and its existing OpenHCL helper
can append command-line arguments
(`petri/src/vm/mod.rs:1217-1228`).

## Prototype decisions

These decisions make the prototype implementable while keeping the spec risks
visible:

1. **Wire type:** use `IGVM_VHT_SUPPORTED_PLATFORM_V2 = 0x2` inside the platform
   header range for the local prototype. Treat this as provisional until the IGVM
   spec assigns the final value.
2. **Requirement semantics:** use the documented `reject_*` semantics described
   above. Add parser, writer, generator, and loader tests for both debug
   directions.
3. **Fixed-header revision:** keep x64 multi-context prototype files at
   `IgvmRevision::V1` and allow supported-platform v2 headers in V1 files in the
   local parser. This minimizes OpenVMM churn and matches igvmfilegen's current
   x64 compatibility default. If the spec later requires fixed-header V2 or a
   later version, make that a follow-up change.
4. **Fallback strategy:** the generated multi-context file should include a v1
   platform header per hardware isolation architecture, each sharing the selected
   fallback context's final compatibility mask for that platform. This exercises
   the desired new-loader fallback behavior for VBS, SNP, and TDX independently.
   However, this same file is not a true fallback for already-shipped old
   loaders. To support old loaders in the prototype, igvmfilegen should also emit
   sibling v1-only fallback IGVMs generated from the configured fallback contexts.
5. **Scope:** the first implementation step should make the path work for
   OpenVMM's VSM/VBS IGVM loader and OpenHCL debug/release payload selection.
   This is not the final deliverable; it is the smallest live slice of the final
   six-context single-file design. The final design still requires adding SNP
   and TDX contexts to the same file.

## Proposed prototype design

### 1. Implement v2 platform header support in `igvm`

Make the local IGVM crate understand the new header before wiring OpenVMM to it.

Planned changes:

1. Add an `IgvmVariableHeaderType` value for the v2 supported platform header,
   `IGVM_VHT_SUPPORTED_PLATFORM_V2 = 0x2`, marked as provisional.
2. Rename `IGVM_VHS_SUPPORTED_PLATFORM_V2.requirments` to `requirements` while
   it is still prototype-only.
3. Add `IgvmPlatformHeader::SupportedPlatformV2`.
4. Add small accessors on `IgvmPlatformHeader` for `compatibility_mask`,
   `highest_vtl`, `platform_type`, `shared_gpa_boundary`, and optional
   requirements. This avoids scattering v1/v2 matches through OpenVMM.
5. Extend parsing and serialization for the v2 type and size.
6. Extend validation:
   - compatibility mask still has exactly one bit set;
   - highest VTL is still 0 or 2;
   - platform type/version/shared GPA validation is shared with v1;
   - reserved requirement bits must be zero;
   - both reject bits for the same capability must not be set at the same time.
7. Update duplicate-platform validation to allow multiple v2 headers for the
   same platform type when their requirement sets are distinct. Ambiguous
   duplicates with the same platform type and same requirements should be an
   error. Also allow one v1 fallback header per hardware platform to share the
   same compatibility mask as exactly one v2 context for that platform.
8. Update merge and mask-fixup code for the v2 variant.
9. Add IGVM crate tests for:
   - v2 parse/write round trip;
   - invalid v2 requirement combinations;
   - v1 plus v2 fallback sharing one mask;
   - multiple same-platform v2 headers with different masks and requirements;
   - merge mask rewriting with v2 headers.

Format-version decision: do not conflate the IGVM fixed-header format v2 with
the supported-platform v2 structure. X64 igvmfilegen intentionally emits
`IgvmRevision::V1` today for older loader compatibility
(`vm/loader/igvmfilegen/src/file_loader.rs:305-310`), while the IGVM defs comment
near the new structure asks whether it requires a newer format. For the
prototype, allow supported-platform v2 headers inside x64 format-v1 files and
cover that with parser/writer tests. Do not implement unknown-header safe-ignore
as part of the first prototype; rely on the sibling v1-only fallback file for
already-shipped old loaders.

### 2. Point OpenVMM at the local IGVM clone

For the prototype change, change OpenVMM root workspace dependencies from crates
io to the local clone:

```toml
igvm = { path = "../igvm/igvm" }
igvm_defs = { path = "../igvm/igvm_defs", default-features = false }
```

This is the smallest local override because `igvmfilegen` and the loader already
use the workspace dependencies. Keep this change clearly marked as temporary so
it can be replaced by a normal version bump or git revision once the IGVM changes
land upstream.

### 3. Extend the igvmfilegen manifest schema

Keep existing manifests valid by defaulting to v1 behavior. Add a file-level
platform-header configuration and per-context labels/requirements.

Proposed schema shape:

```json
{
  "guest_arch": "x64",
  "platform_headers": {
    "version": "v2",
    "v1_fallback_contexts": {
      "vbs": "release",
      "snp": "snp-release",
      "tdx": "tdx-release"
    }
  },
  "guest_configs": [
    {
      "context_name": "release",
      "requirements": {
        "debug": "disabled",
        "migration": "any"
      },
      "guest_svn": 1,
      "max_vtl": 2,
      "isolation_type": "none",
      "image": {
        "openhcl": {
          "command_line": "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=release",
          "memory_page_count": 17920,
          "uefi": true
        }
      }
    },
    {
      "context_name": "debug",
      "requirements": {
        "debug": "enabled",
        "migration": "any"
      },
      "guest_svn": 1,
      "max_vtl": 2,
      "isolation_type": "none",
      "image": {
        "openhcl": {
          "command_line": "OPENHCL_CONFIDENTIAL_DEBUG=1 OPENHCL_BOOT_LOG=com3 OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=debug",
          "memory_page_count": 131072,
          "uefi": true
        }
      }
    }
  ]
}
```

Schema details and invariants:

- `platform_headers.version` defaults to `"v1"` when omitted.
- `platform_headers.v1_fallback_contexts` is required when `version == "v2"`.
  It maps each hardware isolation architecture present in the file to the
  context that supplies that platform's same-file v1 fallback header and sibling
  v1-only fallback output. For the initial VBS-only prototype, only the VBS entry
  is required; the final six-context file requires VBS, SNP, and TDX entries.
- `context_name` is required for v2 builds and optional for existing v1 builds.
  Existing manifests should continue to deserialize without changes.
- Every v2 context must have explicit `requirements`.
- `requirements.debug` should use positive semantic values
  `"enabled"`, `"disabled"`, and `"any"` rather than exposing the low-level
  `reject_*` bit names in manifests.
- `requirements.migration` should exist now because the v2 header already has
  migration bits, but it can default to `"any"` for the debug/release prototype.
- Context names must be unique.
- Requirement sets for the same generated `platform_type` must be unique;
  duplicate requirement sets are ambiguous and should be rejected.
- Each fallback context name must refer to exactly one context whose generated
  platform type matches the fallback map key.
- `requirements.debug` is platform-selection metadata. For `isolation_type:
  "none"`, it can still be used to select between OpenHCL debug/release payloads
  because both `None` and `Vbs` generate a `VSM_ISOLATION` platform header today.
  For VBS/SNP/TDX, additionally validate that `requirements.debug = "enabled"`
  agrees with `enable_debug: true`, and `"disabled"` agrees with
  `enable_debug: false`.
- For OpenHCL image command lines, reject obvious contradictions such as
  `OPENHCL_CONFIDENTIAL_DEBUG=1` in a debug-disabled context. Treat
  `context_name` values such as `"debug"` and `"release"` as labels; the
  actual semantics come from `requirements`.
- Add schema tests proving every existing manifest still parses, plus invalid
  tests for duplicate names, duplicate requirement sets, missing requirements,
  unknown fallback context, fallback/platform mismatches, and contradictory debug
  settings.

### 4. Generate v2 contexts and v1 fallback in igvmfilegen

The cleanest prototype is to keep the existing "one `GuestConfig` becomes one
IGVM context" flow and add platform-header metadata at finalization time.

Generation changes:

1. Generate each v2 context with only its v2 platform header.
2. Preserve `merge_simple` as the mechanism that assigns unique compatibility
   masks to the debug and release contexts, but update the IGVM merge code so it
   rewrites masks inside v2 platform headers too.
3. Track `context_name -> final compatibility mask` after all merges.
4. After merging, synthesize one same-file v1 fallback header for each entry in
   `v1_fallback_contexts` by copying that context's final mask, `highest_vtl`,
   `platform_type`, and `shared_gpa_boundary` into an
   `IGVM_VHS_SUPPORTED_PLATFORM`. Insert the v1 fallback headers before the v2
   headers for deterministic default selection.
5. Also emit sibling v1-only fallback files from the fallback contexts. These
   are the old-loader fallbacks; the multi-context same-file v1 headers are only
   useful to updated parsers/loaders. Implement this by preserving or separately
   regenerating each configured fallback context as a single-context v1 IGVM
   before converting the final merged artifact to v2. Do not try to derive a
   v1-only fallback by stripping v2 headers from the already-merged multi-context
   file, because that risks carrying unrelated masked directives. For an output
   named `foo.igvm`, emit fallbacks as `foo-fallback-<platform>-<context>.igvm`
   for v2 manifests, where `<platform>` and `<context>` are sanitized from the
   fallback map. Reject configurations whose computed output paths collide.
6. Change measurement and map output naming to include `context_name`, for
   example `openhcl-multicontext-release-vbs.json` and
   `openhcl-multicontext-debug-vbs.json`, instead of using only the isolation
   string.
7. Add a prototype manifest, likely
   `vm/loader/manifests/openhcl-x64-multicontext.json`, with release and debug
   entries based on the current x64 dev/release manifests.
8. Add a small inspection command or test helper that prints platform headers,
   requirements, masks, command-line directives, and required-memory entries for
   a generated file. This can live as an igvmfilegen debug subcommand or as a
   test-only helper; the important part is making the generated structure easy
   to assert.

Add a validation test that every v1 fallback header still shares its configured
fallback context's final mask after merge/fixup.

### 5. Add OpenVMM loader context selection

The loader should select a platform by requested runtime properties, not by
"first VSM header".

Config/API changes:

1. Add a config-compatible selector to `LoadMode::Igvm`, defaulting to current
   behavior. `LoadMode` derives `MeshPayload`, so add the field directly to the
   `Igvm` variant and update every construction site to initialize the selector
   explicitly or with `Default`:

   ```rust
   pub enum IgvmContextSelector {
       Default,
       DebugEnabled,
       DebugDisabled,
       CompatibilityMask(u32),
   }
   ```

   Make `IgvmContextSelector` derive the payload traits needed by
   `LoadMode::Igvm` and implement `Default` as `IgvmContextSelector::Default`.
   The `LoadMode::Igvm` variant should gain an `igvm_context: IgvmContextSelector`
   field, and every `LoadMode::Igvm { ... }` construction must set it. Do not use
   `Option<IgvmContextSelector>` or add a schema migration for the first
   prototype; update all in-repo constructors/tests so the non-optional field is
   always present.

2. Thread the selector through:
   - `openvmm/openvmm_defs/src/config.rs`;
   - `openvmm/openvmm_entry/src/lib.rs`;
   - `openvmm/openvmm_core/src/worker/dispatch.rs`;
   - `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs::LoadIgvmParams`;
   - Petri's OpenVMM builder.

3. Introduce a selected-context value so all pre-load and load paths use the same
   decision:

   ```rust
   struct SelectedIgvmContext {
       platform_type: IgvmPlatformType,
       compatibility_mask: u32,
       highest_vtl: u8,
       requirements: Option<SupportedPlatformRequirements>,
   }
   ```

4. Change `read_igvm_file` to avoid parse-time VBS mask filtering for
   multi-context files. For the prototype, parse with `None` and rely on
   loader-time mask filtering after selecting `SelectedIgvmContext`.
5. Replace `vbs_platform_header()` with a selector-aware function that:
   - filters to `VSM_ISOLATION`;
   - prefers v2 headers when an explicit debug selector is supplied;
   - matches the selector against v2 requirements;
   - falls back to the v1 header only for `Default`;
   - supports `CompatibilityMask(u32)` for debugging and targeted tests;
   - returns an explicit error if no platform matches;
   - returns an explicit ambiguous-match error if more than one v2 context
     matches the selector.
6. Define deterministic matching rules:
   - `DebugEnabled` matches only v2 headers that require debug enabled
     (`reject_debug_disabled = 1`);
   - `DebugDisabled` matches only v2 headers that require debug disabled
     (`reject_debug_enabled = 1`);
   - `Default` chooses the v1 fallback header and returns an explicit error if no
     v1 fallback header exists in a v2 multi-context file;
   - `CompatibilityMask(u32)` selects by mask and ignores requirements. If the
     mask matches both a same-file v1 fallback header and exactly one v2 header,
     treat them as one selected context and prefer the v2 header's requirements
     for diagnostics. If it matches multiple v2 headers, return an ambiguity
     error. If it matches only one v1 header, select that v1 header.
7. Move compatibility-mask filtering from parse-time into loader-time paths:
   after selecting a context, skip initialization and directive headers whose
   compatibility mask does not include the selected mask, matching the previous
   parse-time filtering behavior. This filtering must happen before memory-size,
   relocation, and load decisions consume the headers.
8. Pass `SelectedIgvmContext` to `supports_relocations`, `vtl2_memory_info`,
   `vtl2_memory_range`, and `load_igvm_x86` so all pre-load decisions use the
   same context as the actual load. Keep compatibility wrappers only where
   needed for existing callers, and make them use `Default`.
9. Audit every selected-mask consumer during implementation: required memory,
   command line, parameter areas, VP context, memory map/imported regions,
   relocation support/ranges, initialization directives, and final load iteration
   must all use the same selected mask. Add tests before live validation so a
   context mismatch fails locally rather than only at boot time.
10. Add one `tracing::info!` event after context selection and before loading any
    directives. It must include the requested selector, selected platform type,
    selected compatibility mask, selected requirements, selected
    `vtl2_memory_info`, and whether the v1 fallback header was used. This is the
    required loader-side evidence for manual release/debug/default validation.

CLI flags:

- Add `--igvm-context <default|debug|release>` for the prototype.
  - `default` preserves today's behavior and should choose the v1 fallback when
    present.
  - `debug` selects a v2 context requiring debug enabled.
  - `release` selects a v2 context requiring debug disabled.
- Add a hidden diagnostic `--igvm-compatibility-mask <u32>` in the first
  prototype. It maps to `IgvmContextSelector::CompatibilityMask`, is mutually
  exclusive with `--igvm-context`, and exists only for generated-file debugging
  and targeted loader tests.

The selector should control IGVM platform/context selection only. Existing
OpenHCL command-line arguments should remain the way to control OpenHCL runtime
options such as log level and GPA pool behavior; the generated contexts can bake
different command-line directives into the file.

### 6. Lightweight MCP + ohcldiag manual validation before Petri

Before committing to full Petri tests, use OpenVMM's embedded MCP server as a
lighter-weight launch/control path, and use OpenHCL diagnostics to inspect the
paravisor state. This is especially useful from WSL because the Windows OpenVMM
target can be launched with WHP while the agent drives the VMM over MCP
JSON-RPC.

Why this works:

- `--mcp` runs OpenVMM with the same CLI/config path as normal boot, but replaces
  the interactive console with an MCP stdio server
  (`Guide/src/reference/openvmm/management/mcp.md:7-16`).
- MCP exposes `serial/read`, `serial/write`, `serial/execute`, `inspect/tree`,
  `inspect/get`, `vm/status`, and lifecycle tools
  (`Guide/src/reference/openvmm/management/mcp.md:52-100`).
- The server is in-process with the VM worker, so it validates exactly the same
  loader path that `--igvm` and the planned `--igvm-context` flags use.
- Current MCP inspect tools route to OpenVMM host inspect only: the MCP setup
  sends `InspectTarget::Host` (`openvmm/openvmm_entry/src/lib.rs:2376-2385`).
  OpenVMM has an internal `InspectTarget::Paravisor`
  (`openvmm/openvmm_entry/src/vm_controller.rs:27-31`), and the interactive REPL
  can choose it, but MCP does not expose that target today.
- Therefore, first-prototype paravisor validation should use `ohcldiag-dev`
  against OpenVMM's VTL2 hybrid-vsock listener. `ohcldiag-dev` accepts
  `vsock:<path>` and supports `inspect`, `kmsg`, and `run`
  (`openhcl/ohcldiag-dev/src/main.rs:65-114`, `315-365`, `570-618`).
  Alternatively, extend MCP with an explicit paravisor inspect target and then
  use MCP `inspect/tree`/`inspect/get` for the same checks.

Manual validation flow:

1. Start OpenVMM in MCP mode with a VTL2 hybrid-vsock listener. Do not assume an
   interactive OpenHCL shell on COM3; there is not one. MCP serial can still be
   useful for VMM/serial log capture, but paravisor state and OpenHCL logs should
   come from `ohcldiag-dev`.

   ```bash
   cargo run --target x86_64-pc-windows-msvc -- --mcp \
     --hv --vtl2 \
     --igvm <WINDOWS_PATH_TO_MULTICONTEXT_IGVM> \
     --igvm-context release \
     --vmbus-vtl2-vsock-path <NATIVE_PATH_TO_VTL2_VSOCK>
   ```

   Use Windows paths for files consumed by the Windows process. Convert WSL
   paths with `wslpath -w` when launching from WSL. Cross-running the Windows
   target requires the WSL/Windows cross-tooling described in
   `Guide/src/dev_guide/getting_started/cross_compile.md`; using an already
   built Windows `openvmm.exe` is fine too. Run `ohcldiag-dev` on the same OS
   side as the OpenVMM process so it can connect to the native hybrid-vsock path.
2. Drive the MCP stdio protocol from the agent:
   - send `initialize` with protocol version `2025-06-18`;
   - send `notifications/initialized`;
   - call `tools/list` once to confirm the server is ready.
3. Use MCP lifecycle/status tools to verify the VM is running and to catch early
   loader failures. Use MCP host `inspect/tree` for OpenVMM-side state only. It
   should not be treated as proof of paravisor debug/release state unless MCP has
   first been extended to target `InspectTarget::Paravisor`.
4. Use `ohcldiag-dev` over the VTL2 vsock path for paravisor validation:

   ```powershell
   ohcldiag-dev.exe vsock:<NATIVE_PATH_TO_VTL2_VSOCK> inspect build_info
   ohcldiag-dev.exe vsock:<NATIVE_PATH_TO_VTL2_VSOCK> inspect -r
   ohcldiag-dev.exe vsock:<NATIVE_PATH_TO_VTL2_VSOCK> inspect vm/partition
   ohcldiag-dev.exe vsock:<NATIVE_PATH_TO_VTL2_VSOCK> kmsg
   ```

   Use `kmsg` to check for debug vs release OpenHCL logging behavior. The debug
   boot should include the existing "confidential debug enabled" warning emitted
   when `OPENHCL_CONFIDENTIAL_DEBUG=1` is active; the release boot should not.
   Use `inspect` to compare the obvious release/debug surface-area difference:
   release should expose `build_info` and should not expose the full paravisor
   object graph; debug should expose the broader tree, including `vm` and
   `vm/partition`.
5. Use loader-side evidence for release memory-size validation, because release
   paravisor inspect intentionally exposes only `build_info`. For debug, use
   `ohcldiag-dev inspect vm/partition` and other available debug-only nodes as
   additional evidence that the debug image and partition state booted. If the
   prototype needs a live memory-size check for both debug and release, use the
   required loader info log described below.
6. Relaunch OpenVMM with `--igvm-context debug` and then relaunch again with
   default context selection. The context selector is a CLI input, so each
   context needs a fresh OpenVMM/MCP process. The debug boot must show debug
   logging/inspect behavior and the larger debug VTL2 memory range; default must
   match the configured per-platform v1 fallback context.
7. Relaunch with the sibling `foo-fallback-<platform>-<context>.igvm` file without
   an explicit context selector and verify the same log/inspect/memory evidence
   as the fallback context.
8. Record the exact launch commands, MCP JSON-RPC calls, `ohcldiag-dev`
   commands, and observed log/inspect/memory output in the implementation notes.
   This lightweight validation is required before adding Petri tests. If the
   manual path proves stable, convert it into Petri tests later; if it is flaky,
   keep it as an interactive debug workflow and rely on lower-level automated
   tests initially.

This MCP path is the first live end-to-end check after the builder and loader
selector compile, and it must happen before investing in Petri artifact plumbing
or durable VM regression tests. It is not a full regression test replacement
because it depends on local Windows/WHP setup and manual artifact paths, but it
is much lighter than adding artifact plumbing and durable Petri tests before the
design settles.

If `ohcldiag-dev` cannot connect to the VTL2 vsock path, the MCP path is only a
launch smoke test (`vm/status`, `vm/wait_for_halt`, and host inspect); it is not
enough to prove the selected paravisor image booted. In that case, either fix the
VTL2 hybrid-vsock/diag path first or extend MCP to expose
`InspectTarget::Paravisor` and equivalent log access before claiming live
end-to-end validation.

### 7. Deferred Petri and test-surface support

This section describes durable test coverage to consider after the first
prototype, once the lightweight MCP/manual validation in the previous section
proves the selector boots the expected debug and release images. Petri artifact
plumbing is explicitly out of scope for the first prototype implementation.

Add a Petri builder method such as:

```rust
with_igvm_context(IgvmContextSelector::DebugEnabled)
```

Use it to create tests that load the same generated IGVM file twice, once with
the release selector and once with the debug selector. The tests should assert
loader-visible differences without relying only on boot success.

Candidate assertions:

- `vtl2_memory_info` returns the release context's required-memory size for the
  release selector and the debug context's size for the debug selector.
- The selected compatibility mask causes the expected existing OpenHCL
  debug/release command-line settings to be used, especially
  `OPENHCL_CONFIDENTIAL_DEBUG=1` for debug contexts and no confidential-debug
  setting for release contexts.
- A debug selector fails against a file that only has a release-only v2 context,
  and vice versa.
- The default selector uses the configured per-platform v1 fallback context.

Concrete VM-boot tests:

1. Add a multi-context OpenHCL Linux-direct IGVM artifact generated from the new
   manifest. Do not add synthetic marker arguments such as
   `OPENHCL_IGVM_CONTEXT=*` to the static command line. The debug contexts should
   use the existing confidential-debug signal, `OPENHCL_CONFIDENTIAL_DEBUG=1`,
   and the release contexts should not set it. Preserve the existing
   debug/release OpenHCL settings such as GPA-pool sizing and logging behavior.
2. Add or reuse Petri plumbing for custom IGVM artifacts:
   `PetriVmBuilder::with_custom_openhcl(...)` already replaces the OpenHCL IGVM
   artifact (`petri/src/vm/mod.rs:1202-1214`), and the new
   `with_igvm_context(...)` builder method should set the selector on the
   OpenVMM `LoadMode::Igvm`. The VMM test artifact resolver/flowey build must
   expose the generated multi-context IGVM and its sibling fallback IGVM as
   `ResolvedArtifact<impl IsOpenhclIgvm>` values so the tests can pass them to
   `with_custom_openhcl(...)`.
3. Add tests under
   `vmm_tests/vmm_tests/tests/tests/x86_64/openhcl_linux_direct.rs`, following
   the existing OpenHCL Linux-direct pattern where `config.run()` boots the VM
   and `vm.wait_for_vtl2_agent()` obtains a VTL2 Linux agent
   (`vmm_tests/vmm_tests/tests/tests/x86_64/openhcl_linux_direct.rs:195-220`).
4. `openhcl_igvm_multicontext_release_context_boots`: boot the multi-context
   IGVM with `IgvmContextSelector::DebugDisabled`. After the VM boots, assert the
   OpenHCL log output looks like the release path: no
   `OPENHCL_CONFIDENTIAL_DEBUG=1`-driven confidential-debug warning and release
   log filtering/verbosity. Query the paravisor inspect tree, either through
   Petri plumbing or `ohcldiag-dev`, and assert the release surface only exposes
   `build_info`; debug-only nodes such as `vm` and `vm/partition` should be absent
   or rejected. Assert the loader selected the release context's expected memory
   size through the required loader info log.
5. `openhcl_igvm_multicontext_debug_context_boots`: boot the same IGVM with
   `IgvmContextSelector::DebugEnabled`. Assert the OpenHCL log output shows the
   debug path, including the existing confidential-debug warning emitted when
   confidential debug is active. Query the same paravisor inspect tree and assert
   the debug surface exposes `vm`, `vm/partition`, and the broader partition/object
   graph. Assert VTL2 memory is in the debug context's expected range using the
   required loader info log.
6. `openhcl_igvm_multicontext_default_context_boots_fallback`: boot the same
   multi-context IGVM with the default selector. Assert it boots the configured
   per-platform v1 fallback context by checking the same log, inspect, and
   memory-size evidence used by the fallback context.
7. `openhcl_igvm_multicontext_v1_fallback_file_boots`: boot the sibling
   `foo-fallback-<platform>-<context>.igvm` output with the default selector and
   assert the log/inspect/memory evidence matches the fallback context. This is
   the real old-loader compatibility artifact.
8. Keep selector negative cases as non-booting loader tests: a debug selector
   against a release-only file, a release selector against a debug-only file, and
   ambiguous matches should fail before VM boot. These are still important, but
   they do not prove that the right image booted.

The release/debug VM tests should compare runtime evidence, not just header
inspection. The minimum acceptable evidence is:

- debug contexts include `OPENHCL_CONFIDENTIAL_DEBUG=1`, and release contexts do
  not;
- the OpenHCL logs show the expected debug-vs-release behavior. For debug
  contexts, expect full confidential-debug logging, including the existing
  "confidential debug enabled" warning emitted when confidential debug is active;
  for release contexts, expect confidential/release filtering and no
  confidential-debug warning;
- paravisor inspect via `ohcldiag-dev` or a future MCP paravisor inspect target
  shows the expected debug-vs-release inspection-tree difference: release exposes
  `build_info` only, while debug exposes the broader tree including `vm` and
  `vm/partition`;
- a loader-side VTL2 memory-size assertion proving the selected context's
  required-memory directives were used by the loader.

The VM tests must not append OpenHCL command-line options that could override
the embedded context-specific settings being verified, especially
`OPENHCL_ENABLE_VTL2_GPA_POOL` or anything that changes the selected debug vs
release memory-pool behavior.

When running these tests locally, use `cargo xflowey vmm-tests-run` with a
filter such as `test(igvm_multicontext)` and a user-provided `--dir`.

### 8. Path to final single-file SNP/TDX support

Do not include SNP/TDX loading in the initial VSM/OpenVMM prototype, but the
schema, IGVM crate support, and igvmfilegen merge/fallback logic must be designed
for the final single-file target from the start. The follow-up work should add
the SNP/TDX contexts into the **same IGVM file**, not into sidecar files.

Follow-up SNP/TDX generation work:

1. Reuse the same `requirements` schema for SNP and TDX contexts. Validate that
   `requirements.debug = "enabled"` agrees with SNP `policy.debug() == 1` and
   TDX `policy.debug_allowed() == 1`; validate `"disabled"` agrees with those
   bits being zero.
2. Generate `SupportedPlatformV2` headers for `SEV_SNP` and `TDX` as well as
   VSM, preserving the existing `shared_gpa_boundary`, guest-policy, SNP ID
   block, and TDX/SNP measurement behavior. igvmfilegen already creates SNP and
   TDX platform headers and measurement documents in
   `vm/loader/igvmfilegen/src/file_loader.rs:202-303`; the follow-up should add
   v2 requirements without changing the measurement inputs accidentally.
3. Name SNP/TDX measurement JSON files by both context and isolation type, for
   example `openhcl-multicontext-snp-debug.json` and
   `openhcl-multicontext-tdx-release.json`, so debug/release contexts do not
   collide.
4. Add generated-file inspection tests for SNP and TDX that verify v2
   requirements, guest-policy debug bits, measurement-document `debug_enabled`
   fields, and compatibility masks all agree.
5. Add a final unified-manifest test that generates one file containing all six
   contexts (`vbs-release`, `vbs-debug`, `snp-release`, `snp-debug`,
   `tdx-release`, `tdx-debug`) and verifies each has a unique mask and the
   correct platform/debug requirements.

Follow-up SNP/TDX loader work:

1. Extend OpenVMM's isolation configuration beyond VBS. Today
   `openvmm/openvmm_defs/src/config.rs:435-437` and
   `openvmm/openvmm_entry/src/cli_args.rs:1715-1718` only expose `Vbs`.
   Add `Snp` and `Tdx` only after the underlying OpenVMM hypervisor path can
   create those isolated partitions.
2. Extend IGVM platform selection to include the requested platform type, not
   just VSM. The selected-context abstraction should become
   `(platform_type, requirements, compatibility_mask, highest_vtl)` and should
   reject debug/release selectors that do not match the host isolation mode.
3. Implement SNP/TDX directive loading before attempting boot tests. The current
   OpenVMM IGVM loader still has `todo!("snp not supported")` for
   `SnpVpContext` and `SnpIdBlock`
   (`openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:1031-1032`), and TDX
   will need equivalent audited support for its initialization and measurement
   assumptions.
4. Add confidential-VM boot tests only after the loader and host support exist.
   The tests should mirror the VSM proof: boot one SNP/TDX multi-context file
   with debug disabled and debug enabled selectors, then verify guest-visible
   confidential-debug log/inspect behavior plus measurement/attestation evidence
   that the selected SNP/TDX policy was used.
5. Once SNP/TDX loader support exists, the acceptance criterion for the feature
   is one generated IGVM file that can be selected by platform and debug mode for
   VBS, SNP, and TDX. Any additional v1-only fallback file remains a transition
   aid, not the primary artifact.

## First prototype implementation phases

1. **IGVM local crate enablement**
   - Add v2 header type, enum variant, parser, writer, validation, merge
     support, and tests.
   - Test debug requirement bit semantics in both directions.

2. **OpenVMM local dependency override**
   - Point OpenVMM workspace `igvm` and `igvm_defs` dependencies at
     `../igvm/igvm` and `../igvm/igvm_defs`.
   - Build enough of OpenVMM to catch API fallout.

3. **igvmfilegen schema and generation**
   - Add `platform_headers`, `context_name`, and `requirements` schema fields
     with v1 defaults.
   - Generate v2 headers, the same-file configured per-platform v1 fallback
     headers, and the sibling v1-only fallback files.
   - Fix output naming for multi-context measurement artifacts.
   - Add a multi-context OpenHCL manifest.

4. **OpenVMM loader and CLI**
   - Add `IgvmContextSelector` to `LoadMode::Igvm`.
   - Add `--igvm-context` and hidden `--igvm-compatibility-mask` diagnostic
     selection.
   - Parse all needed contexts, select by v2 requirements, and use the selected
     mask consistently for memory sizing, relocation, and loading.

5. **MCP/manual end-to-end validation**
    - Generate the multi-context file.
    - Inspect the generated headers and directive masks.
    - Boot it manually through OpenVMM MCP with release, debug, and default
      selectors, with a VTL2 hybrid-vsock path for `ohcldiag-dev`.
    - Verify OpenHCL debug/release log behavior and paravisor inspect-tree
      differences using `ohcldiag-dev` or an MCP paravisor-inspect extension:
      release exposes `build_info` only, while debug exposes `vm`,
      `vm/partition`, and the broader partition/object graph.
    - Verify memory-size evidence for each boot from the required loader
      `tracing::info!` event that reports the selected `vtl2_memory_info`.
    - Verify selector negative cases and fallback behavior with unit tests,
      loader tests, generated-file inspection, and MCP/manual fallback boots where
      useful.

**Deferred post-prototype Petri regression coverage:** do not implement Petri
artifact plumbing or durable Petri VM tests in the first prototype. After the
MCP/manual path proves the selector boots the expected image and the design has
settled, decide whether to convert that workflow into Petri/OpenVMM VM boot
tests.

## Validation plan

Static validation:

- IGVM crate unit tests for v2 parse/write and validation failures.
- igvmfilegen config parse tests for existing v1 manifests and the new
  multi-context manifest.
- igvmfilegen invalid-schema tests for duplicate context names, duplicate
  requirement sets, missing requirements in v2 mode, unknown fallback context,
  and contradictory debug configuration.
- A generated-file inspection test asserting:
  - one release-only v2 platform header;
  - one debug-only v2 platform header;
  - the configured per-platform v1 fallback headers sharing the fallback context
    masks;
  - distinct compatibility masks for debug and release contexts;
  - no measurement JSON filename collisions;
  - expected OpenHCL command-line and required-memory directives per mask.
- A merge/fixup test proving the same-file v1 fallback headers still share their
  intended final masks after all context merges.

Loader validation:

- Unit tests for selector-to-platform matching, including no-match and
  ambiguous-match errors.
- CLI/config construction and mesh-payload tests for the new `LoadMode::Igvm`
  selector field.
- Loader filtering tests proving headers whose compatibility mask does not
  include the selected mask are skipped after parsing with `None`.
- Unit tests or small integration tests proving `vtl2_memory_info`,
  `supports_relocations`, required-memory parsing, command-line selection,
  parameter-area handling, VP-context loading, relocation decisions, and actual
  load iteration all use the same selected mask.
- Early fallback/default tests proving v1+v2 same-mask fallback is allowed,
  duplicate/ambiguous v2 requirement sets are rejected, and `Default` selects the
  v1 fallback header rather than accidentally selecting the first v2 header.
- Required lightweight MCP/manual boot validation using the same multi-context
  IGVM for release, debug, and default selectors. This is the first prototype's
  live end-to-end validation and must prove the selected image actually booted by
  checking OpenHCL debug/release log behavior, paravisor inspect-tree
  differences (`build_info` only in release, `vm`/`vm/partition` and broader
  object graph in debug), and the required loader info log for selected context
  and VTL2 memory-size evidence.
- Deferred Petri/OpenVMM VM boot tests are not part of the first prototype. If
  they are added later, use the same multi-context IGVM for release, debug, and
  default selectors and prove the selected image actually booted by checking
  OpenHCL debug/release log behavior, paravisor inspect-tree differences, and VTL2
  `/proc/meminfo` memory-size differences.

Fallback validation:

- Test that the same-file v1 fallback header selects the fallback context with
  the updated parser/loader.
- Test that the sibling v1-only fallback file parses and loads with current v1
  behavior. This is the prototype's old-loader fallback. If Petri coverage is
  added later, include a Petri/OpenVMM VM boot test that checks the fallback
  context's log, inspect, and memory-size evidence.

Repository validation once code changes exist:

- For modified OpenVMM packages, run the scoped `cargo check`, `cargo clippy
  --all-targets`, `cargo doc --no-deps`, and `cargo nextest run --profile
  agent` commands.
- Run `cargo xtask fmt --fix` last.

## Deferred spec decisions

These do not block the prototype because the choices above make the local
behavior explicit, but they must be settled before upstreaming:

1. Authoritative wire value for the v2 supported-platform variable header.
2. Whether supported-platform v2 is allowed in IGVM fixed-header format v1, or
   must require fixed-header format v2 or later.
3. Whether the final format should permit same-file v1/v2 platform-header
   coexistence.
4. Whether unknown safe-ignore variable headers should be implemented for older
   compatibility, and how the high ignore bit interacts with platform-header
   range detection.
5. How SNP/TDX selection and measurement outputs should use the same
   requirements model after the VSM/OpenVMM prototype works.

## Review

The plan was reviewed through approval.

1. The first review verdict was **Needs rework**. It found that fallback
   compatibility, fixed-header/wire-format choices, requirement semantics,
   manifest invariants, loader ambiguity behavior, and v1/v2 mask sequencing
   were too open-ended. The plan was revised to make explicit prototype
   decisions for each of those areas.
2. The second review verdict was **Minor revisions**. It asked for five
   clarifications: move compatibility-mask filtering explicitly into loader-time
   paths after parsing with `None`; correct the current-code statement about
   directive filtering; require config/payload compatibility for the new
   `LoadMode::Igvm` selector field; define the sibling fallback filename; and
   specify exact selector matching behavior. Those
   clarifications have been incorporated above.
3. A later review of the VM-boot and SNP/TDX additions returned **Minor
    revisions**. It asked to clarify how the multi-context IGVM becomes a
    `ResolvedArtifact<impl IsOpenhclIgvm>`, require runtime evidence beyond
    boot success, use memory-size tolerances rather than exact `/proc/meminfo`
    equality, and avoid OpenHCL command-line overrides in the selector tests.
    Those clarifications have been incorporated above.
4. The final review verdict was **Approved** with no remaining blocking
   feedback.
5. After rebasing onto the OpenVMM MCP support branch, the lightweight
   MCP/manual validation section was reviewed separately. The first MCP review
   returned **Minor revisions** for Windows serial-target accuracy and the
   second returned **Minor revisions** for the Windows MCP serial precondition.
   Both were incorporated, and the final MCP review verdict was **Approved**.
