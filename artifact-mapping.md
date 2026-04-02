# Moving Artifact-to-Build Mapping to Compile Time

## Problem Statement

The current `test_artifact_mapping_completeness` CI gate validates at **runtime**
that every petri artifact ID has a corresponding entry in
`flowey_lib_hvlite::artifact_to_build_mapping::resolve_artifact()`. This is
fragile for several reasons:

1. **Late feedback** — developers only discover missing mappings when 4 CI jobs
   run (one per platform: x64-linux, x64-windows, aarch64-linux,
   aarch64-windows), adding ~5+ minutes to the feedback loop.
2. **String-based matching** — the mapping matches on `module_path!()`-derived
   strings like `"petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"`.
   Renaming a module silently breaks the mapping with no compiler warning.
3. **Two disconnected sources of truth** — artifact declarations live in
   `petri_artifacts_vmm_test` while mappings live in
   `flowey_lib_hvlite::artifact_to_build_mapping`, with no compile-time link
   between them.
4. **Extra CI cost** — 4 dedicated PR gate jobs exist solely to catch this
   class of error.

## Current Architecture

### How artifacts are declared

Artifacts are declared via the `declare_artifacts!` macro in
`petri_artifacts_core` (used in `petri_artifacts_vmm_test` and
`petri_artifacts_common`):

```rust
// petri_artifacts_vmm_test/src/lib.rs
pub mod artifacts {
    declare_artifacts! {
        OPENVMM_WIN_X64,
        OPENVMM_LINUX_X64,
        // ...
    }

    pub mod openhcl_igvm {
        declare_artifacts! {
            LATEST_STANDARD_X64,
            // ...
        }
    }

    pub mod test_vhd {
        declare_artifacts! {
            GUEST_TEST_UEFI_X64,
            ALPINE_3_23_X64,
            // ...
        }
    }
    // ... more submodules
}
```

For each artifact name, the macro generates:
1. A **marker enum** (zero-sized type, e.g., `enum OPENVMM_WIN_X64 {}`)
2. A **const handle** (`pub const OPENVMM_WIN_X64: ArtifactHandle<OPENVMM_WIN_X64>`)
3. An **`ArtifactId` impl** with `GLOBAL_UNIQUE_ID` set to `module_path!()`

### How tests declare artifact requirements

Tests use a resolver closure that calls `resolver.require(handle)`:

```rust
petri::test!(my_test, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    Some(MyArtifacts { openvmm, kernel })
});
```

The resolver operates in two modes:
- **Collection mode** (`ArtifactResolver::collector`): records artifact IDs into
  `TestArtifactRequirements` without resolving paths.
- **Resolution mode** (`ArtifactResolver::resolver`): returns actual `PathBuf`s.

### How artifact IDs become strings

When `--list-required-artifacts` runs, collected `ErasedArtifactHandle`s are
formatted via their `Debug` impl, which outputs the `module_path!()`-based
string with `__ty` suffix stripped:

```rust
// petri_artifacts_core/src/lib.rs
impl Debug for ErasedArtifactHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.artifact_id_str.strip_suffix("__ty").unwrap_or(self.artifact_id_str))
    }
}
```

Result: `"petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"`

### How flowey maps strings to build selections

`artifact_to_build_mapping.rs` has a ~250-line `match` statement on these
strings:

```rust
fn resolve_artifact(&mut self, artifact_id: &str, ...) -> bool {
    match artifact_id {
        "petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"
        | "petri_artifacts_vmm_test::artifacts::OPENVMM_LINUX_X64" => {
            self.build.openvmm = true;
            true
        }
        // ... ~60 more arms
        _ => false  // unknown → CI gate fails
    }
}
```

### The runtime completeness check

`test_artifact_mapping_completeness.rs` wires up:
1. `local_discover_vmm_tests_artifacts` — runs `cargo nextest list` + test
   binary `--list-required-artifacts` to discover all artifact ID strings
2. `ResolvedArtifactSelections::from_artifact_list_json()` — feeds them through
   the match
3. Fails if `resolved.unknown` is non-empty

This runs as **4 separate CI jobs** in `checkin_gates.rs` (lines 1365–1417).

### Dependency graph (relevant crates)

```
petri_artifacts_core          (declares ArtifactId, ArtifactHandle, declare_artifacts!)
    ↑
petri_artifacts_common        (declares PIPETTE_*, TEST_LOG_DIRECTORY)
    ↑
petri_artifacts_vmm_test      (declares OPENVMM_*, OPENHCL_*, test VHDs, etc.)

flowey_lib_hvlite             (has artifact_to_build_mapping.rs)
    └── depends on: vmm_test_images (for KnownTestArtifacts enum)
    └── does NOT depend on: petri_artifacts_core or petri_artifacts_vmm_test
```

This dependency gap is the root cause: `flowey_lib_hvlite` cannot reference
artifact types, so it must match on strings.

---

## Root Cause Analysis

The fundamental issue is **type erasure across a crate boundary with no
shared vocabulary**:

1. Artifacts have rich type information (`ArtifactHandle<OPENVMM_WIN_X64>`)
2. When collected, types are erased to `ErasedArtifactHandle` (just a `&'static str`)
3. The erased handles cross into flowey, which has no dependency on the artifact
   crates
4. Flowey reconstructs meaning by string-matching — an inherently open-ended
   operation with no exhaustiveness guarantee

Any compile-time solution must either:
- **Preserve type information** long enough for flowey to consume it, or
- **Embed build metadata** into the artifact declaration so flowey doesn't need
  to independently maintain a mapping, or
- **Create a shared vocabulary** (enum/trait) that both sides reference

---

## Proposed Approaches

## Approach A: Trait bound on `require()` + build-category enum (CHOSEN)

**Core idea**: Add a `HasBuildMapping` trait to `petri_artifacts_core` (minimal,
just a marker). Put `ArtifactBuildCategory`, `BuildTarget`, and all
`HasBuildMapping` impls in `vmm_test_images` — the existing bridge crate that
already sits between petri artifact crates and flowey. Add `HasBuildMapping` as
a bound on `ArtifactResolver::require()`. Flowey matches on the category enum
(exhaustive) instead of strings.

### Dependency chain (unchanged — no new crates, no cycles)

```
petri_artifacts_core          (HasBuildMapping trait definition)
    ↑
petri_artifacts_common        (declares PIPETTE_*, TEST_LOG_DIRECTORY)
    ↑
petri_artifacts_vmm_test      (declares all VMM artifacts)
    ↑
vmm_test_images               (ArtifactBuildCategory, BuildTarget,
                               KnownTestArtifacts, HasBuildMapping impls)
    ↑
flowey_lib_hvlite             (consumes build categories to set BuildSelections)
```

`vmm_test_images` already depends on `petri_artifacts_vmm_test` and is already
depended on by `flowey_lib_hvlite`. It's the natural place for the mapping logic.

### Changes

**1. `petri_artifacts_core/src/lib.rs` — minimal trait definition**

```rust
/// Every artifact used in a test must declare how the build system should
/// provide it. See `ArtifactBuildCategory` in `vmm_test_images` for the
/// concrete categories.
///
/// This trait is intentionally minimal here to avoid coupling
/// `petri_artifacts_core` to build-system concepts. The associated type
/// is defined as an opaque associated const; `vmm_test_images` provides
/// the concrete `ArtifactBuildCategory` type and all implementations.
pub trait HasBuildMapping: ArtifactId {}
```

Add bound to `require()` and `try_require()`:

```rust
pub fn require<A: ArtifactId + HasBuildMapping>(
    &self, handle: ArtifactHandle<A>,
) -> ResolvedArtifact<A> { ... }

pub fn try_require<A: ArtifactId + HasBuildMapping>(
    &self, handle: ArtifactHandle<A>,
) -> ResolvedOptionalArtifact<A> { ... }
```

**2. `vmm_test_images/src/lib.rs` — enums + impls**

Add dependency on `petri_artifacts_core` (already transitive via
`petri_artifacts_vmm_test`).

```rust
/// What the build system needs to do to provide this artifact.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactBuildCategory {
    /// Must be cargo-built.
    Build(BuildTarget),
    /// Downloaded from an external source (VHDs, ISOs, VMGS files).
    Download {
        artifact: KnownTestArtifacts,
        also_build: &'static [BuildTarget],
    },
    /// Downloaded release IGVM from GitHub.
    ReleaseDownload,
    /// Always available from deps/environment (firmware, log dir).
    AlwaysAvailable,
}

/// Build targets that correspond to fields on `BuildSelections`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BuildTarget {
    Openvmm,
    OpenvmmVhost,
    Openhcl,
    GuestTestUefi,
    Tmks,
    TmkVmmWindows,
    TmkVmmLinux,
    Vmgstool,
    PipetteWindows,
    PipetteLinux,
    TpmGuestTestsWindows,
    TpmGuestTestsLinux,
    TestIgvmAgentRpcServer,
    PrepSteps,
}
```

Then implement `HasBuildMapping` for every artifact. Each impl provides the
build category via a method or associated const on a local extension trait
(since the core trait is a marker):

```rust
use petri_artifacts_vmm_test::artifacts::*;

/// Extension of HasBuildMapping with the concrete category.
pub trait ArtifactBuildInfo: petri_artifacts_core::HasBuildMapping {
    const BUILD_CATEGORY: ArtifactBuildCategory;
}

// Build artifacts
impl HasBuildMapping for OPENVMM_WIN_X64 {}
impl ArtifactBuildInfo for OPENVMM_WIN_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory =
        ArtifactBuildCategory::Build(BuildTarget::Openvmm);
}

impl HasBuildMapping for OPENVMM_LINUX_X64 {}
impl ArtifactBuildInfo for OPENVMM_LINUX_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory =
        ArtifactBuildCategory::Build(BuildTarget::Openvmm);
}

// Download artifacts with side effects
impl HasBuildMapping for test_vhd::ALPINE_3_23_X64 {}
impl ArtifactBuildInfo for test_vhd::ALPINE_3_23_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory =
        ArtifactBuildCategory::Download {
            artifact: KnownTestArtifacts::Alpine323X64Vhd,
            also_build: &[BuildTarget::PipetteLinux],
        };
}

impl HasBuildMapping for test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64 {}
impl ArtifactBuildInfo for test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory =
        ArtifactBuildCategory::Download {
            artifact: KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd,
            also_build: &[BuildTarget::PipetteWindows, BuildTarget::PrepSteps],
        };
}

// Always-available artifacts
impl HasBuildMapping for loadable::UEFI_FIRMWARE_X64 {}
impl ArtifactBuildInfo for loadable::UEFI_FIRMWARE_X64 {
    const BUILD_CATEGORY: ArtifactBuildCategory =
        ArtifactBuildCategory::AlwaysAvailable;
}
```

**3. `ErasedArtifactHandle` — no changes needed**

`ErasedArtifactHandle` stays as-is with just `artifact_id_str`. The compile-time
enforcement comes from the `HasBuildMapping` bound on `require()`, not from data
in the erased handle. Flowey resolves artifact ID strings to categories via
`vmm_test_images::lookup_build_category()` (see step 4 in the implementation
plan below).

**4. Flowey side: replace string match with category match**

```rust
// flowey_lib_hvlite/src/artifact_to_build_mapping.rs
fn resolve_from_category(
    &mut self,
    category: ArtifactBuildCategory,
) {
    match category {
        ArtifactBuildCategory::Build(target) => {
            match target {
                BuildTarget::Openvmm => self.build.openvmm = true,
                BuildTarget::OpenvmmVhost => self.build.openvmm_vhost = true,
                BuildTarget::Openhcl => self.build.openhcl = true,
                BuildTarget::GuestTestUefi => self.build.guest_test_uefi = true,
                BuildTarget::Tmks => self.build.tmks = true,
                BuildTarget::TmkVmmWindows => self.build.tmk_vmm_windows = true,
                BuildTarget::TmkVmmLinux => self.build.tmk_vmm_linux = true,
                BuildTarget::Vmgstool => self.build.vmgstool = true,
                BuildTarget::PipetteWindows => self.build.pipette_windows = true,
                BuildTarget::PipetteLinux => self.build.pipette_linux = true,
                BuildTarget::TpmGuestTestsWindows => self.build.tpm_guest_tests_windows = true,
                BuildTarget::TpmGuestTestsLinux => self.build.tpm_guest_tests_linux = true,
                BuildTarget::TestIgvmAgentRpcServer => self.build.test_igvm_agent_rpc_server = true,
                BuildTarget::PrepSteps => self.build.prep_steps = true,
            }
        }
        ArtifactBuildCategory::Download { artifact, also_build } => {
            self.downloads.insert(artifact);
            for &target in also_build {
                self.resolve_build_target(target);
            }
        }
        ArtifactBuildCategory::ReleaseDownload => {
            self.needs_release_igvm = true;
        }
        ArtifactBuildCategory::AlwaysAvailable => {}
    }
}
```

This match on `BuildTarget` is **exhaustive** — adding a new `BuildTarget`
variant forces updating this match. The `Download` variant's `also_build` array
encodes side effects (e.g., "this VHD needs pipette_linux") at the declaration
site, not in flowey.

Platform-specific filtering (e.g., "don't build pipette_linux on a Windows-only
host") is already handled by the existing code in
`local_build_and_run_nextest_vmm_tests` at line 467:
`if !linux_host { build.pipette_linux = false; }`. This stays unchanged.

---

### Approach B: Dedicated build-target enum as the category key

**Core idea**: Instead of `Build(&'static str)`, use an enum for build targets.
This makes the flowey-side match exhaustive.

```rust
// In petri_artifacts_core (or a new shared crate)
#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BuildTarget {
    Openvmm,
    OpenvmmVhost,
    Openhcl,
    GuestTestUefi,
    Tmk,
    TmkVmmWindows,
    TmkVmmLinux,
    Vmgstool,
    PipetteWindows,
    PipetteLinux,
    TpmGuestTestsWindows,
    TpmGuestTestsLinux,
    TestIgvmAgentRpcServer,
    PrepSteps,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactBuildCategory {
    Build(BuildTarget),
    Download,
    ReleaseDownload,
    AlwaysAvailable,
}
```

**Additional compile-time guarantee**: Adding a new `BuildTarget` variant forces
updating every `match` on `BuildTarget` in flowey (unless `#[non_exhaustive]` is
used, in which case there's a `_` arm — but we can choose not to use it in
flowey's internal match).

**Pros**: Two layers of compile-time checking — trait bound AND exhaustive match
**Cons**: More coupling; the enum must live somewhere both petri and flowey can
see. ~15 variants today, grows with new build targets.

---

### Approach C: `linkme` distributed-slice registration (co-located mappings)

**Core idea**: Instead of a centralized match statement, each artifact
declaration site also registers its build mapping via a `linkme::distributed_slice`.
At "resolve time" the slice is iterated to build the `BuildSelections`.

```rust
// petri_artifacts_core or a new crate
pub struct ArtifactBuildRegistration {
    pub artifact_id: &'static str,
    pub apply: fn(&mut BuildSelectionsAccumulator),
}

#[linkme::distributed_slice]
pub static ARTIFACT_BUILD_REGISTRY: [ArtifactBuildRegistration];
```

At declaration sites:

```rust
// petri_artifacts_vmm_test/src/lib.rs
declare_artifacts! { OPENVMM_WIN_X64 }

#[linkme::distributed_slice(ARTIFACT_BUILD_REGISTRY)]
static REG_OPENVMM_WIN_X64: ArtifactBuildRegistration = ArtifactBuildRegistration {
    artifact_id: "petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64",
    apply: |acc| acc.set_openvmm(true),
};
```

**Pros**:
- Co-locates mapping with declaration (harder to forget)
- No centralized match statement to maintain
- Uses existing `linkme` pattern (already used for test registration)

**Cons**:
- **Not strictly compile-time** — forgetting the registration is still possible
  (just less likely due to co-location)
- Requires a `BuildSelectionsAccumulator` trait/interface visible to both sides
- Registration boilerplate per artifact (could be wrapped in the macro)
- The registry is populated at link time, not compile time — a forgotten
  registration is still a runtime error

---

### Approach D: Extend `declare_artifacts!` to generate everything

**Core idea**: A single macro invocation declares the artifact AND its build
mapping, making it impossible to have one without the other.

```rust
declare_artifacts! {
    OPENVMM_WIN_X64 => build(openvmm),
    OPENVMM_LINUX_X64 => build(openvmm),
    GUEST_TEST_UEFI_X64 => build(guest_test_uefi),
    ALPINE_3_23_X64 => download,
    LINUX_DIRECT_TEST_KERNEL_X64 => always_available,
}
```

The macro expands to:
1. The existing marker enum + const handle + `ArtifactId` impl
2. A `HasBuildMapping` impl (as in Approach A)
3. Optionally, a `linkme` registration (as in Approach C)

**Pros**:
- Single source of truth — impossible to declare without mapping
- Clean API
- Compile-time enforcement (via trait bound on `require()`)

**Cons**:
- Larger macro, more complex to maintain
- Every `declare_artifacts!` call site must be updated
- Build concepts baked into the declaration macro

---

### Approach E: Separate mapping crate with exhaustive const array

**Core idea**: Create a new crate (e.g., `petri_artifact_mappings`) that depends
on both `petri_artifacts_vmm_test` and provides the mapping. Use a const array
of `(ErasedArtifactHandle, BuildCategory)` pairs and a compile-time or test-time
length assertion.

```rust
// petri_artifact_mappings/src/lib.rs
use petri_artifacts_vmm_test::artifacts::*;

pub const ARTIFACT_MAPPINGS: &[(ErasedArtifactHandle, BuildCategory)] = &[
    (OPENVMM_WIN_X64.erase(), BuildCategory::Build("openvmm")),
    (OPENVMM_LINUX_X64.erase(), BuildCategory::Build("openvmm")),
    // ...
];
```

Then add a unit test that compares `ARTIFACT_MAPPINGS` against the full list
of artifacts (obtained by running the test binary). This is still a test-time
check, but it runs as a fast `cargo test` in the mapping crate rather than a
full CI gate.

Alternatively, if `ErasedArtifactHandle::erase()` can be `const fn`, the array
construction is fully compile-time, and a `const_assert!(ARTIFACT_MAPPINGS.len()
== EXPECTED_COUNT)` provides a compile-time length check.

**Pros**:
- Centralizes mapping in one crate with proper dependencies
- Uses actual types (not strings) for the artifact handles
- Fast unit test replaces slow CI gate

**Cons**:
- Still requires manual maintenance of the array
- Length assertion catches additions but not removals or mismatches
- `erase()` may not be `const fn` today (depends on `module_path!()` usage)

---

## Comparison Matrix

| Criterion                          | A (trait bound) | B (build enum) | C (linkme) | D (unified macro) | E (mapping crate) |
|------------------------------------|:---:|:---:|:---:|:---:|:---:|
| True compile-time error            | ✅  | ✅  | ❌  | ✅  | ⚠️  |
| No string matching                 | ✅  | ✅  | ❌  | ✅  | ✅  |
| Co-located with declaration        | ✅  | ✅  | ✅  | ✅  | ❌  |
| Low coupling to build system       | ⚠️  | ❌  | ❌  | ⚠️  | ✅  |
| Small diff / incremental adoption  | ⚠️  | ❌  | ⚠️  | ❌  | ✅  |
| Eliminates CI gate jobs            | ✅  | ✅  | ⚠️  | ✅  | ⚠️  |
| Handles platform-specific logic    | ⚠️  | ✅  | ✅  | ⚠️  | ⚠️  |

✅ = fully addressed, ⚠️ = partially addressed, ❌ = not addressed

---

## Implementation Plan (Approach A)

### Step 1: Add `HasBuildMapping` marker trait to `petri_artifacts_core`

**File**: `petri/petri_artifacts_core/src/lib.rs`

Add the trait and the bound on `require()` / `try_require()`. This is the
minimal change that enables compile-time enforcement. The trait is a marker —
no associated consts, no dependency on build-system types.

### Step 2: Add `BuildTarget`, `ArtifactBuildCategory` to `vmm_test_images`

**File**: `vmm_tests/vmm_test_images/src/lib.rs`

Add `BuildTarget` enum (14 variants matching `BuildSelections` fields) and
`ArtifactBuildCategory` enum with `Build`, `Download`, `ReleaseDownload`,
`AlwaysAvailable` variants. `Download` carries `KnownTestArtifacts` and
`also_build: &'static [BuildTarget]`.

Add the `ArtifactBuildInfo` extension trait that provides the concrete
`BUILD_CATEGORY` associated const.

### Step 3: Implement `HasBuildMapping` + `ArtifactBuildInfo` for all artifacts

**Files**: `vmm_tests/vmm_test_images/src/lib.rs`

~52 impls total (47 in `petri_artifacts_vmm_test`, 5 in `petri_artifacts_common`).
Each impl provides the build category — same information that's currently in the
string match arms of `artifact_to_build_mapping.rs`, but type-safe and
co-located.

### Step 4: Provide a lookup function in `vmm_test_images`

**File**: `vmm_tests/vmm_test_images/src/lib.rs`

`ErasedArtifactHandle` stays unchanged — it keeps just `artifact_id_str`. The
compile-time enforcement comes from the `HasBuildMapping` trait bound on
`require()` (step 1), not from data carried in the erased handle.

For flowey to resolve an artifact ID string to its `ArtifactBuildCategory`,
`vmm_test_images` provides a lookup function:

```rust
/// Look up the build category for an artifact by its ID string.
///
/// This is the bridge between the string-based discovery output and the
/// type-safe build category system. The lookup table is derived from the
/// `ArtifactBuildInfo` impls — it cannot get out of sync because adding
/// a new artifact without an impl is a compile error (via the
/// `HasBuildMapping` bound on `require()`).
pub fn lookup_build_category(artifact_id: &str) -> Option<ArtifactBuildCategory> {
    // Generated from the ArtifactBuildInfo impls. Can be a match, a
    // phf map, or a linear scan of a const array.
    ARTIFACT_BUILD_TABLE.iter()
        .find(|(id, _)| *id == artifact_id)
        .map(|(_, cat)| *cat)
}

const ARTIFACT_BUILD_TABLE: &[(&str, ArtifactBuildCategory)] = &[
    (
        <OPENVMM_WIN_X64 as ArtifactId>::GLOBAL_UNIQUE_ID,
        <OPENVMM_WIN_X64 as ArtifactBuildInfo>::BUILD_CATEGORY,
    ),
    (
        <OPENVMM_LINUX_X64 as ArtifactId>::GLOBAL_UNIQUE_ID,
        <OPENVMM_LINUX_X64 as ArtifactBuildInfo>::BUILD_CATEGORY,
    ),
    // ... one entry per artifact, referencing the trait consts directly
];
```

This table is built from trait associated consts, so it's always consistent with
the `ArtifactBuildInfo` impls. A typo or missing entry is impossible — the
compiler resolves `<TYPE as ArtifactId>::GLOBAL_UNIQUE_ID` and
`<TYPE as ArtifactBuildInfo>::BUILD_CATEGORY` at compile time.

### Step 5: Update flowey to use the lookup function

**File**: `flowey/flowey_lib_hvlite/src/artifact_to_build_mapping.rs`

Replace the ~250-line string match with:

```rust
fn resolve_artifact(&mut self, artifact_id: &str) -> bool {
    let Some(category) = vmm_test_images::lookup_build_category(artifact_id) else {
        log::warn!("unknown artifact ID: {artifact_id}");
        return false;
    };
    self.apply_category(category);
    true
}

fn apply_category(&mut self, category: ArtifactBuildCategory) {
    match category {
        ArtifactBuildCategory::Build(target) => self.apply_build_target(target),
        ArtifactBuildCategory::Download { artifact, also_build } => {
            self.downloads.insert(artifact);
            for &target in also_build {
                self.apply_build_target(target);
            }
        }
        ArtifactBuildCategory::ReleaseDownload => {
            self.needs_release_igvm = true;
        }
        ArtifactBuildCategory::AlwaysAvailable => {}
    }
}

fn apply_build_target(&mut self, target: BuildTarget) {
    match target {
        BuildTarget::Openvmm => self.build.openvmm = true,
        BuildTarget::OpenvmmVhost => self.build.openvmm_vhost = true,
        BuildTarget::Openhcl => self.build.openhcl = true,
        // ... exhaustive — adding a new variant is a compile error
    }
}
```

### Step 6: Remove CI validation infrastructure

**Files**:
- Delete `flowey/flowey_lib_hvlite/src/_jobs/test_artifact_mapping_completeness.rs`
- Remove the 4 CI gate jobs from `flowey/flowey_hvlite/src/pipelines/checkin_gates.rs`
  (lines 1365–1417)
- Remove the module declaration from `flowey/flowey_lib_hvlite/src/_jobs/mod.rs`
- Run `cargo xflowey regen` to regenerate CI pipeline YAMLs

## Files involved

| File | Change |
|------|--------|
| `petri/petri_artifacts_core/src/lib.rs` | Add `HasBuildMapping` trait; add bound to `require()`/`try_require()` |
| `vmm_tests/vmm_test_images/src/lib.rs` | Add `BuildTarget`, `ArtifactBuildCategory`, `ArtifactBuildInfo`; ~52 trait impls; `lookup_build_category()` function |
| `vmm_tests/petri_artifacts_vmm_test/src/lib.rs` | No changes (artifacts stay as-is) |
| `petri/petri_artifacts_common/src/lib.rs` | No changes (artifacts stay as-is) |
| `flowey/flowey_lib_hvlite/src/artifact_to_build_mapping.rs` | Replace string match with `lookup_build_category()` + category match |
| `flowey/flowey_lib_hvlite/src/_jobs/test_artifact_mapping_completeness.rs` | Delete |
| `flowey/flowey_hvlite/src/pipelines/checkin_gates.rs` | Remove 4 CI gate jobs |
| `flowey/flowey_lib_hvlite/src/_jobs/mod.rs` | Remove module declaration |
