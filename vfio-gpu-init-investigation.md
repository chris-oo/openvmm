# VFIO GPU Initialization Investigation

## Context

GPU initialization on an L1VH host is taking several minutes when assigning 14
VFIO PCIe functions to an OpenVMM guest. The captured log is:

`/home/coo/.copilot/session-state/ec6746a9-50d9-4cc7-a50f-c6a5c7f6e5e4/files/paste-1786465084723.txt`

The service/ttrpc path currently creates `VfioDeviceHandle`, which uses the
legacy VFIO group/container interface rather than the cdev/iommufd interface:

- `openvmm/openvmm_entry/src/ttrpc/mod.rs:1777-1816`
- `vm/devices/pci/vfio_assigned_device_resources/src/lib.rs`

## Startup Ordering

Static PCIe devices are constructed before guest VPs execute:

1. `openvmm/openvmm_core/src/worker/dispatch.rs:2440-2517` resolves all
   `cfg.pcie_devices` with `try_join_all`.
2. `vm/devices/pci/vfio_assigned_device/src/resolver.rs` prepares the VFIO
   binding and constructs `VfioAssignedPciDevice`.
3. `vm/devices/pci/vfio_assigned_device/src/lib.rs:278-569` opens and probes
   the physical device and creates its BAR mappings.
4. `openvmm/openvmm_core/src/worker/dispatch.rs:2895-2925` creates the VP
   backing threads only after device construction completes.
5. The VP runners wait for `VpEvent::Start`; creating the threads does not
   start guest execution.
6. During PCI resource assignment,
   `openvmm/openvmm_core/src/worker/dispatch.rs:3411-3427` explicitly holds the
   VPs stopped while state units are temporarily started.

The device state-unit `start` operation is not the expensive VFIO
initialization. `ChangeDeviceState::start` for `VfioAssignedPciDevice` is empty
at `vm/devices/pci/vfio_assigned_device/src/lib.rs:1197-1200`.

## Evidence of Serialization

### Legacy VFIO container preparation

All 14 resolver futures log `opening VFIO device` at approximately
`18:18:52.328`, showing that the outer `try_join_all` starts them concurrently.

Only the first request enters container preparation immediately. It completes
at `18:19:21.963`, about 29.6 seconds later. The remaining 13 requests then
attach to the existing container within roughly one millisecond.

This matches `VfioContainerManager::run` in
`vm/devices/pci/vfio_assigned_device/src/manager.rs:207-215`: the manager has a
single receive loop and awaits each `PrepareDevice` request before receiving
the next one.

Creation of the first container calls `DmaMapperClient::add_dma_mapper` in
`manager.rs:408-443`. The region manager handles requests in one task and
replays every active memory sub-mapping sequentially in
`openvmm/membacking/src/region_manager.rs:408-466`. This is the leading
hypothesis for the first 29.6-second delay.

### Per-device VFIO probing

After the manager releases the queued requests, the eight GPU functions with
MSI-X finish approximately one every 9.1 seconds. The six auxiliary functions
then finish approximately one every 2.4 seconds.

Although the outer code uses `try_join_all`, each future directly issues
synchronous VFIO operations while being polled:

- `Group::open_device` / `VFIO_GROUP_GET_DEVICE_FD`:
  `vm/devices/user_driver/vfio_sys/src/lib.rs:340-350`
- `Device::info` / `VFIO_DEVICE_GET_INFO`:
  `vm/devices/user_driver/vfio_sys/src/lib.rs:525-547`
- `Device::region_info` / `VFIO_DEVICE_GET_REGION_INFO`:
  `vm/devices/user_driver/vfio_sys/src/lib.rs:549-568`
- `Device::region_mmap_areas` / one or two
  `VFIO_DEVICE_GET_REGION_INFO` calls:
  `vm/devices/user_driver/vfio_sys/src/lib.rs:577-623`
- synchronous config-space `pread` calls and BAR mapping setup:
  `vm/devices/pci/vfio_assigned_device/src/lib.rs:326-538`

`VfioAssignedPciDevice::from_device` is declared `async`, but currently has no
internal `.await`. Declaring a function async does not make these synchronous
ioctls nonblocking. If one ioctl blocks, the task polling the `try_join_all`
cannot poll the other device futures until that call returns.

The logs place the first capability-discovery messages about 9.2 seconds after
the manager finishes, while all capability messages for a device complete
within milliseconds. The delay is therefore somewhere before capability
discovery, but the existing log cannot distinguish `Group::open_device` from
the region ioctls, config-space reads, or BAR mmap setup that precede the
capability walk.

### PCI resource assignment

After all devices are constructed, there is another approximately 48-second
interval between the temporary state-unit start and stop used for PCI resource
assignment.

`assign_pci_resources_for_root_complexes` iterates through root complexes in a
serial `for` loop at
`openvmm/openvmm_core/src/worker/dispatch/ecam_config_access.rs:53-69`.
Config-space reads and writes also execute through the chipset one at a time.

### Guest-time BAR mapping and GPU initialization

After VPs start, the guest enables device BARs. The log contains repeated
failed `VFIO_IOMMU_MAP_DMA` calls for device-memory ranges, often separated by
about 2.7 seconds. These calls are issued synchronously by
`VfioType1DmaTarget::map_dma` at
`vm/devices/pci/vfio_assigned_device/src/manager.rs:34-60`.

MSI-X enable events then occur about ten seconds apart for successive GPU
functions. This is consistent with sequential guest-side GPU initialization,
but the host log alone does not prove whether the delay is in the guest driver,
the VFIO irqfd ioctl, or an earlier operation.

## Proposed First Experiment

First add timing instrumentation to identify which synchronous operation
accounts for the repeated delay. Then use the repository's existing
`blocking::unblock` pattern to move the legacy `Group::open_device` call off
the async executor thread as the narrowest concurrency experiment.

OpenHCL already uses this pattern to run synchronous PCI sysfs unbind operations
concurrently:

`openhcl/underhill_core/src/dispatch/pci_shutdown.rs:47-58`

The experimental change in `VfioAssignedPciDevice::new` would have this shape:

```rust
let (binding, vfio_device, pci_id) = blocking::unblock(move || {
    let device = binding
        .group()
        .open_device(&pci_id)
        .with_context(|| format!("failed to open VFIO device {pci_id}"))?;

    Ok::<_, anyhow::Error>((binding, device, pci_id))
})
.await?;

Self::from_device(
    vfio_device,
    manager::VfioBinding::Group(binding),
    pci_id,
    register_mmio,
    msi_target,
    memory_mapper,
    bar_addresses,
)
.await
```

Add `blocking.workspace = true` to:

`vm/devices/pci/vfio_assigned_device/Cargo.toml`

This is intentionally a narrow experiment. It should allow the concurrent
device futures to submit `VFIO_GROUP_GET_DEVICE_FD` calls to blocking worker
threads without first restructuring all device initialization.

Keep this behavior behind a temporary experimental switch so a test host can
immediately return to the original serialized path. Parallel opens may exercise
vfio-pci open/reset behavior concurrently across related GPU functions or
functions in the same IOMMU group. Stop the experiment on reset errors, hangs,
or device-state regressions.

The shared `blocking` pool may also be used by unrelated components. Record
closure submission and worker-start timestamps to detect pool queueing. If this
is taken beyond initial diagnosis, use an explicit concurrency limit rather
than submitting an unbounded number of slow device opens.

Dropping a `try_join_all` future does not cancel a blocking closure that has
already started. Such a closure continues until the ioctl returns and retains
its moved binding and container references during that time.

## Expected Result

If `Group::open_device` accounts for the repeated delay:

- The eight approximately 9-second GPU opens should overlap.
- The six approximately 2.4-second auxiliary-function opens should also
  overlap.
- Total device-construction time should approach the duration of the slowest
  device open rather than the sum of all device opens.

The initial approximately 29.6-second container/DMA-replay delay will remain,
because `VfioContainerManager` still prepares requests serially.

The later PCI resource-assignment and guest-time BAR/MSI-X delays will also
remain.

## Measurement Plan

Add temporary duration tracing around:

1. `VfioManagerClient::prepare_device`.
2. Submission to `blocking::unblock`, start of its worker closure, entry and
   exit of `Group::open_device`, and completion observed by the async caller.
3. Each pre-capability operation in `from_device`: config-region lookup, each
   BAR `region_info`, each config read, each `region_mmap_areas`, device info,
   and BAR backing setup.
4. `DmaMapperClient::add_dma_mapper`, split into eager
   `mapping_manager.new_mapper(true)` creation and active-mapping replay.
5. Each replayed `map_dma` call, including mapping type and range.
6. Each root complex in `assign_pci_resources_for_root_complexes`.
7. Guest-time `map_to_guest`, entry and exit of `VFIO_IOMMU_MAP_DMA`, irqfd
   allocation, and `map_msix`.

Compare baseline and experimental runs using:

- total time from the first `opening VFIO device` event to the last
  `VFIO assigned PCI device initialized` event;
- individual `open_device` durations and overlap;
- first-container DMA replay duration;
- PCI resource-assignment duration;
- time from VP start to each GPU's MSI-X enable.

## Added Tracing

The investigation change adds explicit duration and success events for:

- resolver wait time versus legacy/cdev manager service time;
- `VFIO_GROUP_GET_DEVICE_FD`;
- config-region, BAR-region, BAR config-read, BAR mmap, capability, and device
  info probing;
- BAR memory-region creation, backing mapping, and rate-limited guest mapping;
- eager VA mapper creation and active DMA-mapping replay;
- rate-limited completion timing for legacy `VFIO_IOMMU_MAP_DMA` ioctls;
- each root complex's PCI resource assignment;
- MSI-X irqfd allocation and `VFIO_DEVICE_SET_IRQS`.

These events preserve the existing execution behavior. The
`blocking::unblock` experiment remains a separate follow-up after a traced
baseline identifies the blocking operation.

## Follow-up Options

If only `open_device` is slow, keep the narrow offload and add focused tests.

If other synchronous probe calls are also slow, split `from_device` into:

1. An owned, synchronous VFIO probing phase that can run entirely inside
   `blocking::unblock`.
2. A completion phase on the VM executor that uses the borrowed
   `register_mmio`, `msi_target`, and `memory_mapper` interfaces.

Wrapping all of `from_device` directly is not straightforward because
`blocking::unblock` requires owned `Send + 'static` inputs and results, while
`from_device` currently borrows those three VM interfaces.

Separately investigate reducing or parallelizing the first-container DMA replay
and the serial per-root-complex PCI assignment. Those changes have wider
correctness implications and should not be combined with the initial
`open_device` experiment.

## Review

Verdict: **Minor revisions**

The review confirmed the startup ordering, legacy manager serialization,
absence of suspension points in `from_device`, and the suitability of
`blocking::unblock` as a narrow diagnostic experiment.

The plan was revised to avoid attributing the pre-capability delay specifically
to `open_device` without measurements, instrument individual operations before
changing behavior, separate eager mapper creation from DMA replay, observe
blocking-pool queueing, and document concurrency, cancellation, and rollback
risks.
