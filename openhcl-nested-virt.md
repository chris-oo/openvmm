# OpenHCL Nested Virtualization (VTL0 + KVM) — Implementation Plan

## Goal

Enable OpenHCL to run in VTL0 (a non-VSM partition) and use KVM nested
virtualization to run the guest OS. This makes it possible to do basic OpenHCL
development and testing on a Linux platform with no Windows/Hyper-V
dependencies.

This plan is based on the design in
[PR #281](https://github.com/microsoft/openvmm/pull/281) (by @jstarks,
Nov 2024), updated for the current shape of the codebase and incorporating
review feedback from that PR.

### What This Achieves

- OpenHCL boots in VTL0 instead of VTL2, using KVM to run the guest as an
  L2 VM via nested virtualization.
- The existing `virt_kvm` crate (already used by regular openvmm on Linux)
  is built into the paravisor binary behind a feature flag.
- Guest memory is mapped via `/dev/mem` instead of the `mshv_vtl` driver.
- KVM kernel modules (kvm.ko + vendor-specific kvm-intel.ko/kvm-amd.ko)
  are loaded at init time.
- A new IGVM build recipe (`X64Nested`) produces the image.

### Known Limitations

- Host is unaware of L2 guest → no vmbus/assigned devices; all devices
  emulated in paravisor.
- Still depends on GET (Guest Emulation Transport) / Hyper-V HCL devices →
  requires openvmm as host VMM. Future work can add alternate config
  mechanisms for qemu.
- Only x64 (no KVM ARM64 nested virt environment tested).
- Performance depends on nesting depth; expected to be acceptable on a
  native Linux host.

---

## Design Decisions (incorporating PR #281 review feedback)

### 1. Backend enum, not a bool

The original PR used `kvm: bool`. Review feedback (smalis-msft) suggested
using an enum. We'll use:

```rust
pub enum UnderhillBackend {
    /// Standard OpenHCL: mshv_vtl driver, VTL2.
    MshvVtl,
    /// Nested virt: KVM, VTL0, /dev/mem.
    Kvm,
}
```

This is cleaner, extensible, and makes the conditional logic explicit in
`worker.rs` (match arms rather than `if env_cfg.kvm`).

### 2. Partition abstraction: `OpenhclPartition` trait

Today, `underhill_core` is hardcoded to `Arc<UhPartition>` everywhere. We
need a trait that both `UhPartition` and `KvmPartition` can implement.

The codebase already has a precedent: `openvmm_core/src/partition.rs`
defines `HvlitePartition` — an object-safe trait with a blanket impl over
the `virt` traits. KvmPartition already satisfies `HvlitePartition` in the
regular openvmm code path.

However, OpenHCL needs **additional methods** that `HvlitePartition` doesn't
have:

| Method | Purpose | KVM impl |
|--------|---------|----------|
| `reference_time() → u64` | Paravisor reference clock | Delegate to `GetReferenceTime::now()` |
| `vtl0_guest_os_id() → HvGuestOsId` | NetVSP needs it | Return default (no guest OS ID in KVM) |
| `register_host_io_port_fast_path()` | VGA proxy optimization | No-op (return dummy handle) |
| `revoke_guest_vsm()` | UEFI NV config | No-op (no VSM in KVM mode) |
| `set_pm_timer_assist()` | PM timer HW offload | No-op (not available) |
| `assert_debug_interrupt()` | GDB debug support | Forward to KVM |

So `OpenhclPartition` will be a purpose-built trait for OpenHCL, **not** a
reuse of `HvlitePartition`. It wraps the `virt` traits plus the
OpenHCL-specific methods.

**Trait also provides access to sub-traits via methods:**
- `fn caps() → &PartitionCapabilities` — from `virt::Partition`
- `fn request_msi()` — from `virt::Partition`
- `fn ioapic_routing()` — from `virt::X86Partition` (x86 only)
- `fn control_gic()` — from `virt::Aarch64Partition` (aarch64 only)
- `fn into_synic(self: Arc<Self>) → Arc<dyn Synic>` — needed for `SynicPorts`

The blanket impl for `UhPartition` delegates to its existing methods. The
`KvmPartition` impl stubs the OpenHCL-specific methods and delegates `virt`
methods to its existing trait impls.

### 3. Memory abstraction: minimal, not a big trait

The original PR introduced `AccessGuestMemory` as a broad trait. But
examining today's code, `MemoryMappings` is accessed in very specific ways:

**Used in `worker.rs`:**
- `gm.vtl0()` — 6 call sites (GuestMemory for VTL0)
- `gm.vtl1()` — 3 call sites (optional VTL1 memory)
- `gm.vtl0_kernel_execute()` — 1 call site (new since PR)
- `gm.vtl0_user_execute()` — 1 call site (new since PR)
- `gm.cvm_memory()` — 3 call sites (CVM shared/private/protector)

**Used in `dispatch/mod.rs`:**
- `self.memory` stored as `underhill_mem::MemoryMappings` (1 field)

The CVM-specific methods (`cvm_memory()`, `vtl0_kernel_execute()`,
`vtl0_user_execute()`) are **only relevant for mshv_vtl mode**. In KVM
mode, there's no CVM, no VTL1, no separate execute permissions.

**Approach:** Rather than abstracting all of `MemoryMappings`, we can:
- Keep `MemoryMappings` as-is for the mshv_vtl path.
- Create `DevMemMemory` for the KVM path.
- Both provide `vtl0() → &GuestMemory` (the minimal common interface).
- In `worker.rs`, the KVM code path simply doesn't call CVM/VTL1 methods.
- Use an `enum UnderhillMemory { MshvVtl(MemoryMappings), Kvm(DevMemMemory) }`
  in `LoadedVm` rather than `Box<dyn Trait>`, to keep the CVM methods
  accessible without dyn dispatch for the mshv_vtl path.

### 4. Rootfs config: `auto/` subdirectory for auto-loaded modules

The PR moves modules from `/lib/modules/NNN/` to `/lib/modules/auto/NNN/`.
KVM modules go in `/lib/modules/` (top-level) for explicit loading. The
current code uses `${OPENHCL_MODULES_PATH}` (not the old kernel path).

### 5. Build recipe: `initrd_rootfs` for composable rootfs configs

The PR introduced `InitrdRootfsPath` to compose multiple rootfs configs
(base + KVM overlay). Per review feedback, rename to
`InitrdRootfsConfigPath`. This enables recipes to declare which rootfs
config files they need.

---

## Implementation Plan

### Phase 1: Partition Abstraction (no KVM yet, refactor only)

The goal is to replace `Arc<UhPartition>` with `Arc<dyn OpenhclPartition>`
throughout `underhill_core`, without changing any behavior. This is a pure
refactoring step.

#### 1a. Create `openhcl/underhill_core/src/partition.rs`

Define the `OpenhclPartition` trait:

```rust
/// Partition abstraction for OpenHCL, covering both the standard
/// mshv_vtl backend (`UhPartition`) and alternative backends
/// like KVM for nested virtualization.
///
/// This is analogous to `HvlitePartition` in openvmm_core, but
/// includes OpenHCL-specific methods (reference_time, guest VSM
/// revocation, PM timer assist, etc.) that don't apply to the
/// regular openvmm host path.
pub trait OpenhclPartition: Send + Sync + Inspect {
    fn reference_time(&self) -> u64;
    fn vtl0_guest_os_id(&self) -> anyhow::Result<HvGuestOsId>;
    fn register_host_io_port_fast_path(&self, range: RangeInclusive<u16>)
        -> Box<dyn Send>;
    fn revoke_guest_vsm(&self) -> anyhow::Result<()>;
    fn request_msi(&self, vtl: Vtl, request: MsiRequest);
    fn caps(&self) -> &PartitionCapabilities;
    fn set_pm_timer_assist(&self, port: Option<u16>) -> anyhow::Result<()>;
    fn assert_debug_interrupt(&self, vtl: u8);

    // Sub-trait access
    fn into_synic(self: Arc<Self>) -> Arc<dyn Synic>;

    #[cfg(guest_arch = "x86_64")]
    fn ioapic_routing(&self) -> Arc<dyn IoApicRouting>;
    #[cfg(guest_arch = "x86_64")]
    fn into_lint_target(self: Arc<Self>, vtl: Vtl)
        -> Arc<dyn LineSetTarget>;

    #[cfg(guest_arch = "aarch64")]
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn ControlGic>;

    fn into_request_yield(self: Arc<Self>)
        -> Arc<dyn RequestYield>;
}
```

Implement for `UhPartition` by delegating to its existing methods.

**Call sites that use `UhPartition` today (exhaustive):**

| File | Usage | Change to |
|------|-------|-----------|
| `dispatch/mod.rs:116` | `add_network(partition: Arc<UhPartition>)` | `Arc<dyn OpenhclPartition>` |
| `dispatch/mod.rs:164` | `pub partition: Arc<UhPartition>` in LoadedVm | `Arc<dyn OpenhclPartition>` |
| `dispatch/mod.rs:839,867` | `self.partition.reference_time()` | Works via trait |
| `dispatch/mod.rs:1035` | `self.partition.clone()` passed to `add_network()` | Works (type is `Arc<dyn OpenhclPartition>`) |
| `worker.rs:806,993` | network setup takes `Arc<UhPartition>` | `Arc<dyn OpenhclPartition>` |
| `worker.rs:901-903` | `p.vtl0_guest_os_id()` | Works via trait |
| `worker.rs:2587` | `p.assert_debug_interrupt(vtl)` | Works via trait |
| `worker.rs:2675` | `SynicPorts::new(partition.clone())` | Use `partition.clone().into_synic()` |
| `worker.rs:2734` | `partition.ioapic_routing()` | Works via trait |
| `worker.rs:2855` | `UhRegisterHostIoFastPath(partition.clone())` | Works via trait |
| `worker.rs:3084` | `ApicLintLineTarget::new(partition.clone(), vtl)` | Use `partition.clone().into_lint_target(vtl)` |
| `worker.rs:3097` | `partition.clone().control_gic(Vtl::Vtl0)` | Works via trait |
| `worker.rs:3447` | network add_network call | `Arc<dyn OpenhclPartition>` |
| `worker.rs:3586` | `WrappedPartition(partition.clone())` | Works (WrappedPartition updated) |
| `worker.rs:3906,3928` | `partition.caps()` | Works via trait |
| `worker.rs:3948` | `Weak<UhPartition>` in PmTimerAssist | `Weak<dyn OpenhclPartition>` |
| `worker.rs:4023` | `Arc<UhPartition>` in WatchdogTimeoutNmi | `Arc<dyn OpenhclPartition>` |
| `worker.rs:4034` | `self.partition.request_msi(...)` | Works via trait |
| `emuplat/firmware.rs:54` | `Weak<UhPartition>` in UnderhillVsmConfig | `Weak<dyn OpenhclPartition>` |
| `emuplat/vga_proxy.rs:8` | `Arc<UhPartition>` | `Arc<dyn OpenhclPartition>` |
| `wrapped_partition.rs:20` | `WrappedPartition(Arc<UhPartition>)` | `Arc<dyn OpenhclPartition>` |

**Key pattern changes:**
- `SynicPorts::new(partition.clone())` — currently works because
  `UhPartition: Synic`. With trait object, use
  `partition.clone().into_synic()` since `SynicPorts::new` takes
  `Arc<dyn Synic>`.
- `ApicLintLineTarget::new(partition.clone(), vtl)` — generic over
  `T: X86Partition`. Use `partition.clone().into_lint_target(vtl)` which
  returns `Arc<dyn LineSetTarget>`.
- `partition.clone().control_gic(vtl)` — same pattern, use trait method.

#### 1b. Update all consumers

Replace `use virt_mshv_vtl::UhPartition;` with
`use crate::partition::OpenhclPartition;` in affected files. Add
`mod partition;` to `lib.rs`.

#### 1c. Validate

After this phase, everything compiles and behaves identically — only
concrete types have been replaced with trait objects. The only `impl
OpenhclPartition` is for `UhPartition`.

---

### Phase 2: Memory Abstraction

#### 2a. Create the `UnderhillMemory` enum

In `underhill_mem`, add:

```rust
pub enum UnderhillMemory {
    MshvVtl(MemoryMappings),
    Kvm(DevMemMemory),
}
```

With common accessor methods:
- `fn vtl0(&self) → &GuestMemory`
- `fn vtl1(&self) → Option<&GuestMemory>`

And mshv_vtl-specific accessors that return `None`/panic for KVM:
- `fn as_mshv_vtl(&self) → Option<&MemoryMappings>`
- `fn cvm_memory(&self) → Option<&CvmMemory>`
- `fn vtl0_kernel_execute(&self) → &GuestMemory` (returns `vtl0()` for KVM)
- `fn vtl0_user_execute(&self) → &GuestMemory` (returns `vtl0()` for KVM)

#### 2b. Create `DevMemMemory`

New file `openhcl/underhill_mem/src/devmem.rs`:
- Opens `/dev/mem` and maps guest RAM ranges from the `MemoryLayout`.
- Provides `vtl0() → &GuestMemory`.
- Implements `virt::PartitionMemoryMapper`-compatible mapping to register
  memory with KvmPartition.

#### 2c. Update `LoadedVm` and `worker.rs`

Change `LoadedVm.memory` from `MemoryMappings` to `UnderhillMemory`.
Update the ~15 access sites in `worker.rs` that call memory methods.
For CVM-specific paths, gate with `if let UnderhillMemory::MshvVtl(m) = ..`.

---

### Phase 3: KVM Backend (the actual feature)

#### 3a. Cargo.toml feature flags

Add `virt_kvm` feature flag chain:
- `openhcl/openvmm_hcl/Cargo.toml`: `virt_kvm = ["underhill_entry/virt_kvm"]`
- `openhcl/underhill_entry/Cargo.toml`: `virt_kvm = ["underhill_core/virt_kvm"]`
- `openhcl/underhill_core/Cargo.toml`: `virt_kvm = ["dep:virt_kvm"]` with
  `virt_kvm` as an optional dependency.

Add `fs-err` to `openhcl/underhill_mem/Cargo.toml`.

#### 3b. Implement `OpenhclPartition` for `KvmPartition`

In `partition.rs`, behind `#[cfg(feature = "virt_kvm")]`:

```rust
impl OpenhclPartition for KvmPartition {
    fn reference_time(&self) -> u64 {
        // KvmPartition.inner is private; access reference time via the
        // virt::Hv1 trait's reference_time_source() method instead.
        self.reference_time_source()
            .map_or(0, |s| s.now().as_100ns())
    }
    fn vtl0_guest_os_id(&self) -> anyhow::Result<HvGuestOsId> {
        Ok(HvGuestOsId::new()) // No guest OS ID concept in KVM
    }
    fn register_host_io_port_fast_path(&self, _range: RangeInclusive<u16>)
        -> Box<dyn Send> {
        Box::new(()) // No-op: no fast-path optimization in KVM
    }
    fn revoke_guest_vsm(&self) -> anyhow::Result<()> {
        Ok(()) // No VSM in KVM mode
    }
    fn set_pm_timer_assist(&self, _port: Option<u16>) -> anyhow::Result<()> {
        Ok(()) // Not available
    }
    // ... delegate virt traits to existing impls
}
```

#### 3c. Backend config option

Add to `Options` / `UnderhillEnvCfg`:

```rust
pub backend: UnderhillBackend,  // parsed from OPENHCL_KVM env var
```

#### 3d. Conditional partition creation in `worker.rs`

In `new_underhill_vm()`:

```rust
match env_cfg.backend {
    UnderhillBackend::MshvVtl => {
        // Existing code: UhPartitionNewParams → UhProtoPartition → build
        // underhill_mem::init() → MemoryMappings
        // ...
    }
    UnderhillBackend::Kvm => {
        // New code:
        // 1. Create DevMemMemory from mem_layout
        // 2. Create KvmPartition via virt_kvm::Kvm hypervisor
        // 3. Map memory into partition via PartitionMemoryMapper
        // 4. Build VPs
        // No proto_partition, no mshv_vtl driver
    }
}
```

The rest of worker.rs uses `partition: Arc<dyn OpenhclPartition>` and
`memory: UnderhillMemory` — backend-agnostic.

---

### Phase 4: Init & Rootfs

#### 4a. Refactor `load_modules()` in `underhill_init`

Split into reusable parts:
- `parse_module_options() → HashMap<String, String>` — parse kernel cmdline
  for module parameters.
- `load_module(params, path) → Result` — load a single module via
  `finit_module`.
- `load_modules(params, path)` — walk directory and load all modules.
- `load_kvm(params)` — load kvm.ko + vendor-specific module (uses CPUID
  to detect Intel vs AMD).

Add `safe_intrinsics` and `x86defs` as x86_64-specific dependencies to
`underhill_init/Cargo.toml`.

#### 4b. KVM module loading in `do_main()`

After existing module loading thread spawn, add:

```rust
// Load KVM modules if configured for nested virt.
if std::env::var("OPENHCL_KVM").as_deref() == Ok("1") {
    // Must complete before underhill starts (KVM needed at partition create).
    load_kvm(&mut module_params)?;
}
```

Unlike regular modules (loaded in a background thread), KVM modules must
be loaded **before** underhill starts since partition creation needs
`/dev/kvm`.

#### 4c. Rootfs config changes

**`openhcl/rootfs.config`** — Move module paths under `auto/` subdirectory:
```
dir /lib/modules/auto        0755 0 0
dir /lib/modules/auto/000    0755 0 0
...
file /lib/modules/auto/000/pci-hyperv-intf.ko  ${OPENHCL_MODULES_PATH}/...
```

Update `load_modules()` call in `do_main()` to use `/lib/modules/auto`
as the walk path.

**`openhcl/rootfs.kvm.config`** — New file:
```
file /lib/modules/kvm.ko        ${OPENHCL_MODULES_PATH}/kernel/arch/x86/kvm/kvm.ko        0644 0 0
file /lib/modules/kvm-amd.ko    ${OPENHCL_MODULES_PATH}/kernel/arch/x86/kvm/kvm-amd.ko    0644 0 0
file /lib/modules/kvm-intel.ko  ${OPENHCL_MODULES_PATH}/kernel/arch/x86/kvm/kvm-intel.ko  0644 0 0
```

These go in `/lib/modules/` (top-level, not `auto/`) because they are
explicitly loaded by `load_kvm()`, not auto-loaded by `load_modules()`.

---

### Phase 5: Build Pipeline

#### 5a. Add `X64Nested` recipe

- `OpenhclRecipeCli::X64Nested` in `build_igvm.rs`
- `OpenhclIgvmRecipe::X64Nested` in `build_openhcl_igvm_from_recipe.rs`
- Recipe details:
  ```rust
  Self::X64Nested => OpenhclIgvmRecipeDetails {
      local_only: None,
      igvm_manifest: in_repo_template("openhcl-x64-nested.json",
                                       "openhcl-x64-nested.json"),
      openhcl_kernel_package: OpenhclKernelPackage::Dev,
      openvmm_hcl_features: {
          let mut f = base_openvmm_hcl_features();
          f.insert(OpenvmmHclFeature::VirtKvm);
          f
      },
      target: CommonTriple::Common {
          arch: CommonArchitecture::X86_64,
          platform: CommonPlatform::LinuxMusl,
      },
      vtl0_kernel_type: None,
      with_uefi: true,
      with_interactive: false,
      with_sidecar: false,
      max_trace_level: MaxTraceLevel::default(),
  },
  ```

#### 5b. Composable rootfs configs (`InitrdRootfsConfigPath`)

Add to recipe details:
```rust
pub initrd_rootfs_configs: Vec<InitrdRootfsConfigPath>,
```

Where `InitrdRootfsConfigPath` is:
```rust
pub enum InitrdRootfsConfigPath {
    InTree(String),           // e.g., "rootfs.config"
    LocalOnlyCustom(PathBuf), // for local dev overrides
}
```

Base recipes use `vec!["rootfs.config"]`. The `X64Nested` recipe uses
`vec!["rootfs.config", "rootfs.kvm.config"]`.

#### 5c. `OpenvmmHclFeature::VirtKvm`

Add to `build_openvmm_hcl.rs`:
```rust
pub enum OpenvmmHclFeature {
    Gdb,
    Tpm,
    VirtKvm,
    LocalOnlyCustom(String),
}
```

Maps to cargo feature `"virt_kvm"`.

#### 5d. IGVM manifest

Create `vm/loader/manifests/openhcl-x64-nested.json`:
```json
{
    "guest_arch": "x64",
    "guest_configs": [{
        "guest_svn": 1,
        "max_vtl": 0,
        "isolation_type": "none",
        "image": {
            "openhcl": {
                "command_line": "OPENHCL_KVM=1",
                "memory_page_count": 163840,
                "memory_page_base": 131072,
                "uefi": true
            }
        }
    }]
}
```

Key: `max_vtl: 0` (not 2), `OPENHCL_KVM=1` on command line.

#### 5e. Filename mapping + regenerate CI

- `recipe_to_filename`: `X64Nested => "openhcl-nested"`
- `non_production_build_igvm_tool_out_name`: `X64Nested => "x64-nested"`
- `cargo xflowey regen` to regenerate CI YAMLs.

---

### Phase 6: Validation

1. `cargo check -p underhill_core` (both with and without `--features virt_kvm`)
2. `cargo clippy --all-targets -p underhill_core`
3. `cargo doc --no-deps -p underhill_core`
4. `cargo nextest run -p underhill_core`
5. Repeat for `underhill_mem`, `underhill_init`, `underhill_entry`,
   `openvmm_hcl`, and affected flowey crates.
6. `cargo xtask fmt --fix`
7. Build the X64Nested IGVM: `cargo xflowey build-igvm x64-nested`

---

## Summary of Files Changed

### New files (4)
| File | Purpose |
|------|---------|
| `openhcl/underhill_core/src/partition.rs` | `OpenhclPartition` trait + impls |
| `openhcl/underhill_mem/src/devmem.rs` | `/dev/mem` guest memory mapping |
| `openhcl/rootfs.kvm.config` | KVM kernel modules for initrd |
| `vm/loader/manifests/openhcl-x64-nested.json` | IGVM manifest for nested |

### Modified files (~20)
| File | Nature of change |
|------|-----------------|
| `openhcl/underhill_core/src/worker.rs` | Backend enum, conditional partition creation, use trait objects |
| `openhcl/underhill_core/src/dispatch/mod.rs` | `Arc<dyn OpenhclPartition>`, `UnderhillMemory` enum |
| `openhcl/underhill_core/src/lib.rs` | Add `mod partition`, plumb backend option |
| `openhcl/underhill_core/src/options.rs` | Add `OPENHCL_KVM` → `UnderhillBackend` |
| `openhcl/underhill_core/src/wrapped_partition.rs` | Use `Arc<dyn OpenhclPartition>` |
| `openhcl/underhill_core/src/emuplat/firmware.rs` | Use `Weak<dyn OpenhclPartition>` |
| `openhcl/underhill_core/src/emuplat/vga_proxy.rs` | Use `Arc<dyn OpenhclPartition>` |
| `openhcl/underhill_core/Cargo.toml` | Add optional `virt_kvm` dep + feature |
| `openhcl/underhill_entry/Cargo.toml` | Add `virt_kvm` feature |
| `openhcl/openvmm_hcl/Cargo.toml` | Add `virt_kvm` feature |
| `openhcl/underhill_mem/src/lib.rs` | Add `devmem` module, `UnderhillMemory` enum |
| `openhcl/underhill_mem/src/init.rs` | No changes needed (MemoryMappings stays as-is) |
| `openhcl/underhill_mem/Cargo.toml` | Add `fs-err` dependency |
| `openhcl/underhill_init/src/lib.rs` | Refactor module loading, add `load_kvm()` |
| `openhcl/underhill_init/Cargo.toml` | Add x86_64-specific deps |
| `openhcl/rootfs.config` | Move modules under `auto/` subdirectory |
| `flowey/flowey_hvlite/src/pipelines/build_igvm.rs` | Add `X64Nested` CLI variant |
| `flowey/flowey_lib_hvlite/src/build_openhcl_igvm_from_recipe.rs` | Add recipe, rootfs config composition |
| `flowey/flowey_lib_hvlite/src/_jobs/local_build_igvm.rs` | Handle `initrd_rootfs_configs` |
| `flowey/flowey_lib_hvlite/src/artifact_openhcl_igvm_from_recipe.rs` | Add filename mapping |
| `flowey/flowey_lib_hvlite/src/build_openvmm_hcl.rs` | Add `VirtKvm` feature variant |

### Auto-regenerated (do not hand-edit)
- `.github/workflows/openvmm-ci.json`
- `.github/workflows/openvmm-pr.json`
- `Cargo.lock`

---

## Risk Assessment

| Risk | Severity | Mitigation |
|------|----------|------------|
| Trait object refactor breaks existing mshv_vtl path | High | Phase 1 is a pure refactor — validate everything compiles and tests pass before adding KVM |
| `MemoryMappings` enum pattern is awkward for CVM code | Medium | CVM code stays in the `MshvVtl` match arm; KVM path never touches CVM |
| KVM dev kernel doesn't include kvm-intel/kvm-amd modules | Medium | Verify dev kernel package contents; may need kernel config change |
| Binary size increase when `virt_kvm` feature enabled | Low | Feature-gated; only the nested recipe enables it |
| Existing features (TDISP, VPCI, battery) don't work in KVM mode | Low | Expected; they require mshv_vtl. KVM mode is for basic dev/test only |

---

## Open Questions

1. **Dev kernel KVM modules** — Does the OpenHCL dev kernel include
   `kvm.ko`, `kvm-intel.ko`, `kvm-amd.ko`? If not, a kernel config change
   is needed first.
2. **Testing** — Should a basic vmm test be added as part of this work
   (as requested in PR review)? If so, it would be a petri test that boots
   OpenHCL in nested mode on a Linux x64 host.
3. **Scope of rootfs refactor** — Should the `auto/` subdirectory change
   and `InitrdRootfsConfigPath` composition be a separate preparatory PR
   to reduce the size of the main change?
