# Plan: vmm-tests-run UX Improvements

## Problem Statement

`cargo xflowey vmm-tests-run` has three UX pain points:

1. **No positional filter**: Users must write `--filter "test(ttrpc)"` instead of
   just `cargo xflowey vmm-tests-run ttrpc`
2. **`--dir` is required**: Must always specify `--dir /path/to/output` even for
   simple native builds where `./target` would be fine
3. **Unnecessary nextest archive + binary copying**: The current flow always
   creates a `.tar.zst` nextest archive and copies ~10+ binaries to a staging
   directory, even when running locally and not cross-compiling

These will be implemented as **three separate PRs**, in order (1 → 2 → 3).

---

## PR 1: Simple Filter Language + Positional Argument

### Current behavior

```
--filter "test(ttrpc)"          # nextest filter expression (required syntax)
--filter "test(/^boot_/)"       # regex filter
--filter "all()"                # run all tests (default)
```

The `--filter` value is passed directly to `cargo nextest list --filter-expr`
(`vmm_tests_run.rs:262-263`) and later to `cargo nextest run --filter-expr`.

### Proposed behavior

```
cargo xflowey vmm-tests-run ttrpc                    # shorthand → test(ttrpc)
cargo xflowey vmm-tests-run "boot_"                  # shorthand → test(boot_)
cargo xflowey vmm-tests-run --filter "test(ttrpc)"   # explicit nextest filter (unchanged)
cargo xflowey vmm-tests-run                          # no filter → all() (unchanged)
```

The positional argument is a **substring shorthand**, not regex. It maps to
`test(<value>)` which is nextest's built-in substring-match semantics. This
matches the existing `--filter` docs (`vmm_tests_run.rs:42`): "test(alpine)
- run tests with "alpine" in the name".

**Important caveat:** `test(value)` uses nextest's filter expression parser,
so characters that are special in the filter grammar — `(`, `)`, `|`, `&`,
`!`, `/` — will break parsing. For example, `test(a(b))` produces a parse
error. Since test names in practice only contain alphanumerics, underscores,
and occasionally dots/hyphens, this is acceptable — but the helper must
validate the input and reject values containing filter-syntax metacharacters,
directing users to `--filter` instead.

### Implementation

**File: `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs`**

Change the CLI struct (lines 28-91):

```rust
#[derive(clap::Args)]
pub struct VmmTestsRunCli {
    /// Simple test name filter (substring match).
    ///
    /// Matches tests whose name contains this string. For full nextest
    /// filter expression syntax, use --filter instead.
    ///
    /// Examples:
    ///   cargo xflowey vmm-tests-run ttrpc
    ///   cargo xflowey vmm-tests-run "boot_"
    name_filter: Option<String>,

    /// Test filter (nextest filter expression).
    ///
    /// Mutually exclusive with the positional name filter.
    /// Examples:
    ///   --filter "test(alpine)"
    ///   --filter "test(/^boot_/) & !test(hyperv)"
    #[clap(long, conflicts_with = "name_filter")]
    filter: Option<String>,

    // ... rest unchanged
}
```

Add a pure helper function (new, testable):

```rust
/// Characters that are special in nextest filter expression syntax.
/// If the positional shorthand contains any of these, reject it and
/// direct the user to --filter.
const FILTER_METACHARACTERS: &[char] = &['(', ')', '|', '&', '!', '/'];

/// Resolve the user's filter arguments into a nextest filter expression.
fn resolve_filter(name_filter: Option<&str>, filter: Option<&str>) -> anyhow::Result<String> {
    match (filter, name_filter) {
        (Some(f), _) => Ok(f.to_string()),            // explicit --filter wins
        (None, Some(name)) => {
            if name.is_empty() {
                anyhow::bail!(
                    "positional filter cannot be empty. \
                     Omit it to run all tests, or provide a test name substring."
                );
            }
            if let Some(c) = name.chars().find(|c| FILTER_METACHARACTERS.contains(c)) {
                anyhow::bail!(
                    "positional filter contains special character '{c}'. \
                     Use --filter with nextest filter syntax instead.\n\
                     Example: --filter \"test({name})\""
                );
            }
            Ok(format!("test({name})"))                // positional → substring
        }
        (None, None) => Ok("all()".to_string()),      // no filter → all
    }
}
```

Key design decisions:
- **`conflicts_with = "name_filter"`** — clap enforces mutual exclusivity at
  parse time, producing a clear error. No ambiguous runtime warning needed.
- **Substring, not regex** — `test(value)` is nextest's substring match.
  No escaping needed. Users who want regex use `--filter "test(/regex/)"`.
- **Single positional arg** — multiple args could be OR'd later as a follow-up.
- **Empty string rejected** — `cargo xflowey vmm-tests-run ""` is rejected
  rather than producing `test()` which silently matches nothing.

### Integration point

In `into_pipeline` (line 99), the destructuring changes from:

```rust
let Self { target, dir, filter, verbose, ... } = self;
```

to:

```rust
let Self { target, dir, filter, name_filter, verbose, ... } = self;
```

And line 128-130 changes from:

```rust
let (artifacts_json, test_names, test_binary) =
    discover_artifacts(&repo_root, &target_str, &filter, release)
```

to:

```rust
let filter = resolve_filter(name_filter.as_deref(), filter.as_deref())?;
let (artifacts_json, test_names, test_binary) =
    discover_artifacts(&repo_root, &target_str, &filter, release)
```

The `filter` variable is now a `String` produced by `resolve_filter`, and the
rest of the code (lines 130-end) uses it identically. No other changes needed
downstream since `discover_artifacts` and `selections_from_resolved` already
take `&str`.

### Error behavior

- If both `name_filter` and `--filter` are given, clap errors before
  `into_pipeline` runs. The error message is clear and automatic.
- If the filter matches zero tests, existing behavior is preserved:
  `discover_artifacts` warns and returns empty selections
  (`vmm_tests_run.rs:276-287`).
- If `cargo nextest list` fails (bad filter syntax, etc.), existing error
  propagation applies (`vmm_tests_run.rs:270-271`). Note: stderr is already
  inherited (`vmm_tests_run.rs:254`), so the user sees nextest's error output.

### Test plan

Add a `#[cfg(test)]` module to `vmm_tests_run.rs` with unit tests for
`resolve_filter`:

| # | name_filter | filter | expected output | notes |
|---|-------------|--------|-----------------|-------|
| 1 | `None` | `None` | `Ok("all()")` | default: run all |
| 2 | `Some("ttrpc")` | `None` | `Ok("test(ttrpc)")` | basic substring |
| 3 | `Some("boot_")` | `None` | `Ok("test(boot_)")` | trailing underscore |
| 4 | `Some("foo.bar")` | `None` | `Ok("test(foo.bar)")` | dots are literal in substring mode |
| 5 | `Some("foo-bar")` | `None` | `Ok("test(foo-bar)")` | hyphens are safe |
| 6 | `Some("a(b)")` | `None` | `Err(...)` | parens are filter metacharacters → rejected |
| 7 | `Some("a|b")` | `None` | `Err(...)` | pipe is filter metacharacter → rejected |
| 8 | `Some("!foo")` | `None` | `Err(...)` | bang is filter metacharacter → rejected |
| 9 | `Some("")` | `None` | `Err(...)` | empty string rejected |
| 10 | `None` | `Some("test(/^boot_/)")` | `Ok("test(/^boot_/)")` | explicit filter passthrough |
| 11 | `None` | `Some("test(a) & !test(b)")` | `Ok("test(a) & !test(b)")` | complex filter passthrough |

Additionally, add a clap parse test to verify that `name_filter` and `--filter`
conflict:

```rust
#[test]
fn positional_and_filter_conflict() {
    use clap::Parser;
    // Wrap VmmTestsRunCli in a test command since it derives Args, not Parser
    #[derive(clap::Parser)]
    struct TestCli {
        #[clap(flatten)]
        inner: VmmTestsRunCli,
    }
    let result = TestCli::try_parse_from(["test", "ttrpc", "--filter", "test(foo)"]);
    assert!(result.is_err());
}
```

### Test plan (end-to-end)

In addition to the unit tests above, run `cargo xflowey vmm-tests-run`
end-to-end to verify the positional argument works in practice:

| # | Command | Expected behavior |
|---|---------|-------------------|
| 1 | `cargo xflowey vmm-tests-run ttrpc --dir /tmp/e2e-pr1` | Runs tests matching "ttrpc" (equivalent to `--filter "test(ttrpc)"`) |
| 2 | `cargo xflowey vmm-tests-run --filter "test(ttrpc)" --dir /tmp/e2e-pr1` | Same result via explicit filter |
| 3 | `cargo xflowey vmm-tests-run "a(b)" --dir /tmp/e2e-pr1` | Exits with error about metacharacter, directs user to `--filter` |
| 4 | `cargo xflowey vmm-tests-run ttrpc --filter "test(foo)" --dir /tmp/e2e-pr1` | Clap error: positional and `--filter` conflict |

### Documentation updates

Update all files that show the `--filter` syntax to lead with the positional
shorthand as the **primary** interface, and present `--filter` as **advanced
usage** for complex nextest filter expressions:

- `Guide/src/dev_guide/tests/vmm.md` (lines 89-94) — show positional first,
  then an "Advanced filtering" subsection for `--filter`
- `Guide/src/dev_guide/dev_tools/xflowey.md` (line 14) — use positional in the
  one-liner example
- `.github/skills/vmm-tests/SKILL.md` (lines 15-26, 32-41) — lead with
  positional, move `--filter` to an "Advanced" section
- `.github/instructions/vmm-tests.instructions.md` (lines 6-13) — positional
  first, `--filter` as advanced
- `.github/copilot-instructions.md` (lines 93-99) — positional first, `--filter`
  as advanced

In each file, use wording like:

```
# Basic usage — run tests by name substring:
cargo xflowey vmm-tests-run ttrpc

# Advanced: full nextest filter expression syntax:
cargo xflowey vmm-tests-run --filter "test(/^boot_/) & !test(hyperv)"
```

---

## PR 2: Default `--dir` to `./target/vmm-tests`

### Current behavior

`--dir` is required (`PathBuf` with no default, `vmm_tests_run.rs:36-37`).
Users must always specify it.

### Proposed behavior

- When `--dir` is not specified and the resolved target matches the host,
  default to `<repo_root>/target/vmm-tests`.
- When the resolved target does NOT match the host (cross-compiling), `--dir`
  remains required.
- The WSL→Windows case is always cross-compiling (Linux host targeting Windows),
  so `--dir` is required and `validate_output_dir` still enforces DrvFs paths.

### Implementation

**File: `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs`**

Change the CLI struct:

```rust
/// Directory for the output artifacts.
///
/// Required when cross-compiling (--target differs from host).
/// Defaults to ./target/vmm-tests for native builds.
#[clap(long)]
dir: Option<PathBuf>,
```

Add a pure helper function (new, testable):

```rust
/// Determine whether this is a cross-compilation scenario.
///
/// Cross-compilation means the resolved target triple differs from the host.
/// WSL targeting Windows is always cross (Linux host → Windows target).
fn is_cross_compile(
    target: &CommonTriple,
    backend_hint: PipelineBackendHint,
) -> bool {
    let host_arch = FlowArch::host(backend_hint);
    let host_platform = FlowPlatform::host(backend_hint);

    let host_target = match (host_arch, host_platform) {
        (FlowArch::Aarch64, FlowPlatform::Windows) => CommonTriple::AARCH64_WINDOWS_MSVC,
        (FlowArch::X86_64, FlowPlatform::Windows) => CommonTriple::X86_64_WINDOWS_MSVC,
        (FlowArch::X86_64, FlowPlatform::Linux(_)) => CommonTriple::X86_64_LINUX_GNU,
        _ => return true, // unknown host → treat as cross
    };

    // Compare the full triple, not just the CLI enum
    target.as_triple() != host_target.as_triple()
}
```

Resolution in `into_pipeline` — insert between target resolution (line 117)
and `validate_output_dir` (line 123). Move the existing `repo_root` call
(line 128) earlier so it can be shared:

```rust
let target = resolve_target(target, backend_hint)?;
let target_os = target.as_triple().operating_system;
let repo_root = crate::repo_root();  // moved up from line 128

// Resolve --dir, defaulting to target/vmm-tests for native builds
let dir = match dir {
    Some(d) => d,
    None => {
        if is_cross_compile(&target, backend_hint) {
            anyhow::bail!(
                "--dir is required when cross-compiling. \
                 Use --dir to specify where to put the test output.\n\
                 Hint: when targeting Windows from WSL, use a DrvFs path \
                 (e.g., /mnt/c/vmm-tests)."
            );
        }
        repo_root.join("target").join("vmm-tests")
    }
};

validate_output_dir(&dir, target_os)?;
// ... repo_root is reused below at the existing line 128 call site
```

Key design decisions:
- **Compare resolved triples, not "was --target specified"** — the original
  plan's `target.is_some()` was wrong because `resolve_target` consumes the
  `Option` and always produces a `CommonTriple`. We compare the resolved
  target against the host triple.
- **`--target linux-x64` on a linux-x64 host is NOT cross** — the user just
  explicitly stated the host target. The default dir still applies.
- **Stale artifacts**: `target/vmm-tests` is reused across runs. In the
  current archive flow, `init_vmm_tests_env` copies fresh binaries each run,
  overwriting stale ones — so same-name artifacts are always current. The only
  theoretical risk is an artifact from a prior run that is no longer needed
  (e.g., you ran with OpenHCL enabled, then without — the stale IGVM file
  remains). This is identical to reusing an explicit `--dir`, which users
  already do today, and is not a new problem introduced by defaulting.
  When PR 3 lands, native runs stop copying most binaries into this dir
  entirely, eliminating this class of issue for the common case.

### Test plan (unit tests)

Add unit tests for `is_cross_compile` and for the dir resolution logic:

| # | host | target | dir | expected | notes |
|---|------|--------|-----|----------|-------|
| 1 | linux-x64 | linux-x64 | `None` | `<repo>/target/vmm-tests` | native default |
| 2 | linux-x64 | linux-x64 | `Some("/tmp/out")` | `"/tmp/out"` | explicit overrides default |
| 3 | linux-x64 | windows-x64 | `None` | error | cross requires --dir |
| 4 | linux-x64 | windows-x64 | `Some("/mnt/c/out")` | `"/mnt/c/out"` | cross with explicit dir |
| 5 | windows-x64 | windows-x64 | `None` | `<repo>/target/vmm-tests` | native Windows |
| 6 | windows-x64 | windows-aarch64 | `None` | error | arch cross-compile |

Testing `is_cross_compile` directly is harder because `PipelineBackendHint` is
opaque. Instead, extract the comparison logic to work on `(FlowArch,
FlowPlatform, CommonTriple)` tuples and test that.

### Test plan (end-to-end)

Run `cargo xflowey vmm-tests-run` end-to-end to verify `--dir` defaulting:

| # | Command | Expected behavior |
|---|---------|-------------------|
| 1 | `cargo xflowey vmm-tests-run ttrpc` | Runs without `--dir`, output goes to `<repo>/target/vmm-tests` |
| 2 | `cargo xflowey vmm-tests-run ttrpc --dir /tmp/e2e-pr2` | Explicit `--dir` overrides the default |
| 3 | `cargo xflowey vmm-tests-run --target windows-x64 ttrpc` | Error: `--dir is required when cross-compiling` |

### Documentation updates

Same files as PR 1 — update examples to **omit `--dir`** in all standard
examples. The key principle: **users and agents should never need to specify
`--dir` for the common case (native builds)**. `--dir` should only appear in
docs under cross-compilation or advanced sections.

Specific changes per file:

- `Guide/src/dev_guide/tests/vmm.md` — the main "quick start" example becomes
  just `cargo xflowey vmm-tests-run ttrpc` with no `--dir`. Add a subsection
  "Output directory" explaining: output defaults to `target/vmm-tests/`,
  override with `--dir` if needed, required when cross-compiling.
- `Guide/src/dev_guide/dev_tools/xflowey.md` — one-liner example without `--dir`
- `.github/skills/vmm-tests/SKILL.md` — all basic examples without `--dir`;
  cross-compilation section keeps `--dir`. This is the authoritative place for
  detailed `vmm-tests-run` usage, including `--dir` behavior and defaults.
- `.github/instructions/vmm-tests.instructions.md` — simplest form without
  `--dir`; this is a brief pointer, not detailed usage
- `.github/copilot-instructions.md` — keep the example minimal (no `--dir`);
  detailed `--dir` semantics belong in the vmm-tests skill, not here

---

## PR 3: Avoiding the Nextest Archive for Native Local Runs

### Scope

This PR applies **only to native (non-cross-compile) local runs** where
`build_only == false`. All other scenarios are unchanged:
- `--build-only` → still uses `Archive` mode (unchanged)
- Cross-compile (including WSL→Windows) → still uses `Archive` mode (unchanged)

This scoping avoids the WSL→Windows nextest binary compatibility issue: the
archive path downloads a target-specific nextest binary
(`local_build_and_run_nextest_vmm_tests.rs:681-694`), while `ImmediatelyRun`
uses host-installed nextest (`install_cargo_nextest.rs:41` —
`target_lexicon::Triple::host()`). For native runs, host = target so this is
safe.

### Current architecture

The current non-`--build-only` flow works like this:

```
┌─────────────────────────────────────────────────────────┐
│  1. Build vmm_tests binary (cargo build -p vmm_tests)   │
│  2. Create nextest archive (.tar.zst)                    │
│  3. Copy archive to test_content_dir                     │
│  4. Copy nextest binary to test_content_dir              │
│  5. Copy nextest.toml, Cargo.toml to test_content_dir    │
│  6. Build all artifact binaries (openvmm, pipette, etc.) │
│  7. Copy all binaries to test_content_dir                │
│  8. Run nextest from archive                             │
│     (nextest run --archive-file vmm-tests-archive.tar.zst│
│      --workspace-remap .)                                │
└─────────────────────────────────────────────────────────┘
```

Key files:
- `local_build_and_run_nextest_vmm_tests.rs:627-631` — always uses `Archive`
- `init_vmm_tests_env.rs:257-450` — copies every binary to `test_content_dir`
- `test_nextest_vmm_tests_archive.rs` — runs tests from the archive
- `run_test_igvm_agent_rpc_server.rs:71` — starts binary from
  `VMM_TESTS_CONTENT_DIR` (Windows only)

### Proposed change: Use `ImmediatelyRun` for native non-build-only runs

`BuildNextestVmmTestsMode::ImmediatelyRun` (`build_nextest_vmm_tests.rs:33-46`)
already exists and uses `cargo nextest run` directly without archiving. It is
currently used only for unit tests (`build_and_run_nextest_unit_tests.rs:55`),
never for VMM tests.

### Implementation

#### Decision: native vs cross

A new parameter must be threaded from the pipeline construction
(`vmm_tests_run.rs`) into `local_build_and_run_nextest_vmm_tests.rs`. We can
reuse the `is_cross_compile` helper from PR 2.

Add to `Params` struct (`local_build_and_run_nextest_vmm_tests.rs:100-128`):

```rust
/// When true, use in-place ImmediatelyRun mode (no archive, no binary copy).
/// When false (cross-compile or build-only), use Archive mode as before.
pub use_immediate_run: bool,
```

Set it in `build_vmm_tests_pipeline` (`vmm_tests_run.rs:589-601`):

```rust
use_immediate_run: !opts.build_only && !is_cross_compile(&target, backend_hint),
```

#### `local_build_and_run_nextest_vmm_tests.rs` changes

Branch on `use_immediate_run`:

**When `use_immediate_run == true` (native, non-build-only):**
1. Use `BuildNextestVmmTestsMode::ImmediatelyRun` instead of `Archive`
2. Skip adding to `copy_to_dir`: nextest archive, nextest binary,
   `nextest.toml`, `Cargo.toml` (lines 627-679)
3. Pass `extra_env` and `pre_run_deps` (including dep install, RPC server
   start, prep steps) directly to `ImmediatelyRun`
4. Skip `run.sh`/`run.ps1` script generation (see below)
5. Still download artifacts (VHDs), set up disk images dir, etc.
6. Skip the `test_nextest_vmm_tests_archive::Request` call (lines 854-867) —
   `ImmediatelyRun` handles running internally and produces `TestResults`
   directly via `build_nextest_vmm_tests::Request`

**When `use_immediate_run == false` (cross-compile or build-only):**
- Entire existing flow unchanged

#### `init_vmm_tests_env.rs` changes

Add `skip_binary_copy: bool` to the `Request` struct. When `true`:
- **Skip**: copying openvmm, openvmm_vhost, pipette (windows + linux),
  guest_test_uefi, tmks, tmk_vmm, vmgstool, tpm_guest_tests, OpenHCL IGVM
  files, test_igvm_agent_rpc_server
- **Keep**: setting `VMM_TESTS_CONTENT_DIR`, `TEST_OUTPUT_PATH`,
  `VMM_TEST_IMAGES`, `PETRI_REMOTE_ARTIFACTS` env vars
- **Keep**: copying UEFI firmware and linux kernel/initrd (these come from
  `.packages/` and the resolver expects them at known relative paths under
  `VMM_TESTS_CONTENT_DIR`)

Important: `init_vmm_tests_env` is also used by archive consumers
(`_jobs/consume_and_test_nextest_vmm_tests_archive.rs`). The new parameter
must default to `false` to preserve existing behavior for all other callers.

#### `run_test_igvm_agent_rpc_server.rs` — no change needed

This node only runs on Windows (`local_build_and_run_nextest_vmm_tests.rs:837`).
On native Windows, `build.test_igvm_agent_rpc_server` is set to `false` by the
`!linux_host` gate (`local_build_and_run_nextest_vmm_tests.rs:208-214`), so the
binary is never built. The RPC server starter already handles this gracefully —
it checks `exe.exists()` and returns `Ok(())` with an info log if the binary
is missing (`run_test_igvm_agent_rpc_server.rs:73-79`).

For WSL→Windows (where the binary IS built on the Linux host), the archive
path is used (not `ImmediatelyRun`), so this flow is unchanged.

Therefore, no carve-out is needed in `init_vmm_tests_env` for the RPC server.
The `skip_binary_copy` flag can unconditionally skip all binary copies.

#### `run.sh` / `run.ps1` generation

The current script generation (`local_build_and_run_nextest_vmm_tests.rs:787-826`)
uses `gen_cargo_nextest_run_cmd` with `RunKindDeps::RunFromArchive`, which
produces a command referencing the archive file, a standalone nextest binary,
and portable `$PSScriptRoot`-relative paths. None of these exist in the
`ImmediatelyRun` path.

**Decision: skip script generation in the `ImmediatelyRun` path.**

Rationale:
- The script's purpose is re-running tests without flowey (e.g., on a
  different machine for `--build-only`). For native `ImmediatelyRun`, the user
  simply re-runs `cargo xflowey vmm-tests-run` — incremental compilation
  makes this fast.
- Generating a correct non-portable script would require a separate
  `gen_cargo_nextest_run_cmd` call with `RunKindDeps::BuildAndRun`,
  `portable = false`, and the repo root as `working_dir`. This is doable
  but adds complexity for marginal value.
- `install_deps.ps1` is still generated if dependency install commands exist
  (lines 770-783). This is independent of run mode.
- The `--build-only` path still generates the full self-contained bundle with
  `run.sh`/`run.ps1` as before.

#### Things NOT changed

- `--build-only` flow — still `Archive`, still copies everything, still
  generates self-contained bundle
- Cross-compile flow — still `Archive`
- `test_nextest_vmm_tests_archive.rs` — still used by `--build-only` and
  cross-compile paths, and by CI archive consumers
- `install_cargo_nextest.rs` — not changed; `ImmediatelyRun` uses host nextest
  which is correct for native runs

### Test plan

Testing this change is harder because the code is side-effect heavy (flowey
pipeline nodes). The approach is:

**1. Decision logic unit tests**

Test that `use_immediate_run` is set correctly:

| # | build_only | is_cross | expected use_immediate_run |
|---|------------|----------|---------------------------|
| 1 | false | false | true (native, run) |
| 2 | true | false | false (build-only) |
| 3 | false | true | false (cross-compile) |
| 4 | true | true | false (both) |

**2. `init_vmm_tests_env` skip_binary_copy tests**

Verify the skip logic by checking which copy operations occur:

| # | skip_binary_copy | expected copies | expected env vars |
|---|-----------------|-----------------|-------------------|
| 1 | false | all binaries + firmware + kernel | all env vars set |
| 2 | true | firmware + kernel only | all env vars set |

Since `init_vmm_tests_env` is a flowey node, these are best tested as
integration checks: run the pipeline with `--build-only` (archive path) and
verify the content dir has all files, then run without `--build-only` on a
native target and verify only firmware/kernel are copied.

**3. End-to-end smoke test**

Run `cargo xflowey vmm-tests-run <test_name>` on a native host and verify:
- No `.tar.zst` archive is created in `test_content_dir`
- Tests execute successfully
- No `run.sh`/`run.ps1` script is generated (only in `--build-only` mode)
- `test_content_dir` contains firmware/kernel but NOT openvmm, pipette, etc.

### Documentation updates

- `Guide/src/dev_guide/tests/vmm.md` — mention the faster native path
- `.github/skills/vmm-tests/SKILL.md` — update architecture description

### Risks and mitigations

| Risk | Mitigation |
|------|------------|
| `ImmediatelyRun` hasn't been used for VMM tests before | It's used for unit tests; the code path is proven. VMM tests add env vars and pre_run_deps which `ImmediatelyRun` supports. |
| Stale binaries in `VMM_TESTS_CONTENT_DIR` shadow fresh `target/` builds | In `ImmediatelyRun` mode, `VMM_TESTS_CONTENT_DIR` only contains firmware/kernel (not binaries), so stale binary shadows can't happen. |
| RPC server binary not found on native Windows | Not an issue: `build.test_igvm_agent_rpc_server = false` on non-Linux hosts (line 214), and the starter handles missing binary gracefully (lines 73-79). |
| WSL→Windows breaks with host nextest | Scoped to native-only; cross-compile still uses archive path with target-specific nextest. |
| No `run.sh`/`run.ps1` for native runs | Acceptable: users re-run via `cargo xflowey vmm-tests-run` (fast with incremental). `--build-only` still generates scripts. |

### PR dependency note

PR 3 reuses `is_cross_compile` from PR 2. Since they ship in order (1→2→3),
this is a direct dependency. If PR 2 hasn't landed when PR 3 is implemented,
duplicate the helper inline.

---

## PR 3: Open Issues from Review

The following issues were identified during plan review and need to be resolved
before PR 3 implementation begins.

### Issue 1: Document the two-tier resolver mechanism (justification gap)

The plan says to add `skip_binary_copy: bool` to skip copying binaries to
`test_content_dir`, but never explains *why* this is safe.

**The mechanism**: `OpenvmmKnownPathsTestArtifactResolver::get_path()`
(`petri_artifact_resolver_openvmm_known_paths/src/lib.rs:677-708`) uses a
two-tier lookup:

1. **Tier 1**: Check `$VMM_TESTS_CONTENT_DIR/<file_name>` — if the env var is
   set and the file exists there, return it immediately (lines 688-693)
2. **Tier 2**: Fall back to a build-output path (cargo target dir or
   `flowey-out/`) — resolve `search_path/file_name`, check existence, or emit
   a `MissingCommand` error with build instructions (lines 695-708)

For binaries built by cargo (openvmm, pipette, vmgstool, tmk_vmm, etc.), the
Tier 2 path points to the cargo target directory
(`target/<triple>/<profile>/`), which is where `ImmediatelyRun` mode leaves
them after building. So skipping the copy to `test_content_dir` is safe — the
resolver finds them at their original build location.

**Action**: Add this explanation to the plan's "Key design decisions" or
"Implementation" section so reviewers can evaluate correctness.

### Issue 2: OpenHCL IGVM files must NOT be skipped (correctness bug)

The plan lists "OpenHCL IGVM files" under the skip list for `skip_binary_copy`.
This is **incorrect** and would break OpenHCL tests.

**Why**: The resolver's Tier 2 fallback for IGVM files points to
`flowey-out/artifacts/build-igvm/debug/{arch}/` (lib.rs:535-580). But this
directory is only populated by the standalone `cargo xflowey build-igvm`
pipeline. When `vmm-tests-run` builds IGVM files, it goes through
`build_openhcl_igvm_from_recipe` → `run_igvmfilegen`, and the output
(`IgvmOutput.igvm_bin`) is written to a **flowey-managed temp/staging path**,
not to `flowey-out/artifacts/build-igvm/`.

The current code in `init_vmm_tests_env.rs` (lines 376-396) copies IGVM files
from this flowey-managed path to `test_content_dir` with the expected filename
(e.g., `openhcl-x64.bin`). If we skip this copy, the resolver won't find the
IGVM file at either tier:
- Tier 1: `test_content_dir/openhcl-x64.bin` — not copied
- Tier 2: `flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin` — not
  populated by `vmm-tests-run`

The same applies to **release IGVM files** (lines 399-416), which come from
`resolve_openvmm_deps` and are downloaded to a flowey-managed path, not to
`flowey-out/artifacts/last-release-igvm-files/`.

**Fix**: Keep IGVM files (both built and release) in the copy list. The
`skip_binary_copy` flag should skip cargo-built binaries only (openvmm,
pipette, vmgstool, tmks, tmk_vmm, tpm_guest_tests,
test_igvm_agent_rpc_server), NOT IGVM files or release IGVM files.

Consider renaming `skip_binary_copy` to something more precise like
`skip_cargo_binary_copy` to make the scope clear.

### Issue 3: `guest_test_uefi` — verify it's safe to skip

The resolver's Tier 2 fallback for `guest_test_uefi` is
`target/{arch}-unknown-uefi/debug/guest_test_uefi.img` (lib.rs:276-296).
This is a standard cargo target directory path.

**Question**: When flowey's `build_guest_test_uefi` node builds this artifact,
does it output to the same `target/{arch}-unknown-uefi/debug/` path? Or does
it use a flowey-managed path like the IGVM files?

If the build goes to the standard cargo path, skipping the copy is safe (Tier 2
will find it). If not, it must stay in the copy list alongside IGVM files.

**Action**: Verify the `build_guest_test_uefi` output path before implementing.

### Issue 4: Post-test processing wiring (incomplete plan)

The current archive path (lines 854-910) has significant post-test processing:

```
854-867: test_nextest_vmm_tests_archive::Request → results: ReadVar<TestResults>
869-880: Stop RPC server (Windows only) — after_tests = results.map(|_| ())
882-890: Publish test results — junit_xml = results.map(|r| r.junit_xml)
892-910: Report results — log pass/fail, claim published_results + done
```

The plan says "skip the `test_nextest_vmm_tests_archive::Request` call" and use
`ImmediatelyRun` instead, but doesn't show how the same post-processing (RPC
stop, JUnit publishing, result reporting) wires to the new `results` variable.

**What needs to happen**: In the `ImmediatelyRun` path:

1. Create a `results: WriteVar<TestResults>` via `ctx.new_var()`
2. Pass it to `BuildNextestVmmTestsMode::ImmediatelyRun { results, ... }`
3. The `read_results: ReadVar<TestResults>` side can then be wired to the
   **exact same** post-processing code (lines 869-910): RPC server stop,
   JUnit extraction, publish_test_results, result reporting

The post-processing code (lines 869-910) is the same for both paths — it only
depends on `results: ReadVar<TestResults>`. The implementation should factor
lines 869-910 out (or just have them follow either branch) so both archive and
ImmediatelyRun paths share the same post-test wiring.

**Action**: Show the branching structure in the plan:

```rust
let results = if use_immediate_run {
    // ImmediatelyRun path: build + run inline, get results directly
    ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
        target: target_triple,
        profile,
        build_mode: BuildNextestVmmTestsMode::ImmediatelyRun {
            nextest_profile,
            nextest_filter_expr,
            extra_env,
            pre_run_deps: side_effects,
            results: v,
        },
    })
} else {
    // Archive path: existing code (lines 854-867)
    ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request { ... })
};

// Shared post-processing (lines 869-910) — works with either `results`
let rpc_server_stopped = ...;
let junit_xml = results.map(ctx, |r| r.junit_xml);
let published_results = ...;
ctx.emit_rust_step("report test results", ...);
```

### Issue 5: Flowey variable ownership for archive-only variables

In the archive path, several variables are created that don't exist in the
`ImmediatelyRun` path:

| Variable | Created at | Purpose |
|----------|-----------|---------|
| `nextest_archive` | line 627 | Build result (WriteVar → ReadVar) |
| `nextest_archive_file` | line 632 | Path added to `copy_to_dir` |
| `nextest_config_file` | line 661 | Config file path |
| `nextest_bin` | line 681 | Nextest binary path |
| `nextest_run_cmd` | line 787 | Generated run command |

In flowey, every `ReadVar` that's created must be either `.claim(ctx)`'d in a
step or `.claim_unused(ctx)`'d. The codebase already uses `claim_unused` — see
lines 310-312 where `read_built_openvmm_hcl`, `read_built_openhcl_boot`,
`read_built_sidecar` are claimed unused when `copy_extras == false`.

**In the `ImmediatelyRun` path**: These variables are never created (the whole
archive-building code block is skipped), so there's nothing to claim. The
branching should be structured so that the `ctx.reqv(...)` calls that create
these variables only run in the archive branch.

**Action**: Structure the code as an `if use_immediate_run { ... } else { ... }`
block where the archive-specific variable creation (lines 627-806) only runs in
the `else` branch. The `ImmediatelyRun` branch creates its own
`results: ReadVar<TestResults>` directly. The post-processing code (lines
869-910) goes after the if/else and uses `results` from whichever branch ran.