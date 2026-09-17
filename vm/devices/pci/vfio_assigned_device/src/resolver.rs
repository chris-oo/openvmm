// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for VFIO-assigned PCI devices.

use crate::VfioAssignedPciDevice;
use crate::manager::VfioContainerManager;
use crate::manager::VfioManagerClient;
use anyhow::Context as _;
use async_trait::async_trait;
use membacking::DmaMapperClient;
use pal_async::task::Spawn as _;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use std::sync::Arc;
use vfio_assigned_device_resources::VfioCdevDeviceHandle;
use vfio_assigned_device_resources::VfioDeviceHandle;
use vfio_assigned_device_resources::VfioRealmDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

/// Resource resolver for [`VfioDeviceHandle`].
///
/// Spawns a `VfioContainerManager` task internally and communicates with it
/// via RPC to share VFIO containers across assigned devices.
pub struct VfioDeviceResolver {
    client: VfioManagerClient,
    _task: pal_async::task::Task<()>,
}

/// Native CCA resolver. It never registers an ordinary DMA mapper or IOAS manager.
///
/// VM assembly must configure the static root's private BAR address view before
/// resolving this resource. Full registration remains pending until the VM
/// completes private RAM preparation.
pub struct VfioRealmDeviceResolver {
    provider: Option<Arc<dyn pci_core::vfio::VfioVmProvider>>,
    budget: tdisp::host::SnapshotBudget,
    teardown: RealmTeardownHandle,
}

/// Strong assignment ownership retained independently of the PCI frontend.
///
/// Stop all VPs and frontends before teardown. Release the frontend device
/// graph while keeping the partition and memory/membacking owners alive, then
/// call [`Self::teardown`]. On any error, retain this handle and the whole VM,
/// including its worker and DMA-reachable RAM, until the device/model domain
/// ends. Dropping this handle is not a checked cleanup operation.
#[derive(Clone, Default)]
pub struct RealmTeardownHandle {
    state: Arc<parking_lot::Mutex<RealmServices>>,
}

#[derive(Default)]
struct RealmServices {
    closing: bool,
    devices: std::collections::BTreeMap<u32, Arc<dyn tdisp::host::EvidenceService>>,
}

impl std::fmt::Debug for RealmTeardownHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("RealmTeardownHandle")
            .field("closing", &state.closing)
            .field("requester_ids", &state.devices.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Checked teardown failed; the handle retains every assignment service.
#[derive(Debug, thiserror::Error)]
#[error("Realm device {requester_id:#x} teardown failed; retain the VM and its backing")]
pub struct RealmTeardownError {
    /// Device whose cleanup first failed. All registered cleanups were attempted.
    pub requester_id: u32,
    /// Original typed device/coordinator failure.
    #[source]
    pub source: tdisp::host::EvidenceError,
}

impl RealmTeardownHandle {
    /// Permanently stop new device work without waiting for admitted workers.
    ///
    /// This does not revoke DMA or release any resources.
    pub fn close_admission(&self) {
        let mut state = self.state.lock();
        state.closing = true;
        for service in state.devices.values() {
            service.close_admission();
        }
    }

    fn retain(
        &self,
        requester_id: u32,
        service: Arc<dyn tdisp::host::EvidenceService>,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(!state.closing, "Realm assignment admission is closed");
        anyhow::ensure!(
            !state.devices.contains_key(&requester_id),
            "duplicate Realm requester ID {requester_id:#x}"
        );
        state.devices.insert(requester_id, service);
        Ok(())
    }

    /// Close admission and attempt every device's checked teardown.
    ///
    /// Success releases the retained service references. Failure or cancellation
    /// retains them for explicit retry; it is not permission to drop VM backing.
    pub async fn teardown(&self) -> Result<(), RealmTeardownError> {
        self.close_admission();
        let devices = {
            let mut state = self.state.lock();
            state.closing = true;
            state
                .devices
                .iter()
                .map(|(&rid, service)| (rid, service.clone()))
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for (requester_id, service) in devices {
            if let Err(source) = service.teardown().await {
                tracelimit::error_ratelimited!(
                    requester_id,
                    error = ?source,
                    "Realm teardown failed; retaining assignment and VM backing is required"
                );
                if first_error.is_none() {
                    first_error = Some(RealmTeardownError {
                        requester_id,
                        source,
                    });
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.state.lock().devices.clear();
        Ok(())
    }
}

impl VfioRealmDeviceResolver {
    /// Retain the lazy VM provider and the VM-wide evidence storage budget.
    pub fn new(
        provider: Option<Arc<dyn pci_core::vfio::VfioVmProvider>>,
        budget: tdisp::host::SnapshotBudget,
    ) -> Self {
        Self {
            provider,
            budget,
            teardown: RealmTeardownHandle::default(),
        }
    }

    /// Capture independent strong ownership before installing this resolver.
    pub fn teardown_handle(&self) -> RealmTeardownHandle {
        self.teardown.clone()
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VfioRealmDeviceHandle> for VfioRealmDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VfioRealmDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let VfioRealmDeviceHandle {
            pci_id,
            cdev,
            iommufd,
            requester_id,
            bar_addresses,
        } = resource;
        anyhow::ensure!(
            requester_id >> 16 == 0,
            "initial Realm fixture requires PCI segment zero"
        );
        anyhow::ensure!(
            matches!(
                input.dma_target.passthrough(),
                pci_core::dma::DmaPassthrough::Allowed
            ),
            "Realm S1-bypass assignment cannot sit behind a guest IOMMU"
        );
        let mut owner =
            crate::realm::RealmDevice::prepare(self.provider.as_deref(), cdev, iommufd)?;
        owner.attach(requester_id)?;
        let (device, association) = owner.frontend_access().map_err(|phase| {
            anyhow::anyhow!("Realm frontend requires attachment, got {phase:?}")
        })?;
        let gate = Arc::new(crate::realm::access::AccessGate::with_interrupts(
            requester_id,
            association.clone(),
        ));
        owner.set_access_gate(gate.clone());
        let mut frontend = VfioAssignedPciDevice::from_realm(
            device,
            gate,
            pci_id,
            input.register_mmio,
            input.dma_target.msi_target(),
            bar_addresses,
        )
        .await?;
        let service = owner
            .into_tdisp(self.budget.clone())?
            .into_evidence_service();
        frontend.install_realm_service(service.clone())?;
        self.teardown.retain(requester_id, service.clone())?;
        association.register_assignment(requester_id, Arc::downgrade(&service))?;
        Ok(frontend.into())
    }
}

impl VfioDeviceResolver {
    /// Create a new resolver, spawning the container manager task.
    ///
    /// The manager registers each new VFIO container with the region manager
    /// so that DMA mappings are kept in sync with the VM's memory map.
    pub fn new(spawner: impl pal_async::task::Spawn, dma_mapper_client: DmaMapperClient) -> Self {
        let mut manager = VfioContainerManager::new(dma_mapper_client);
        let client = manager.client();
        let task = spawner.spawn("vfio-container-mgr", manager.run());
        Self {
            client,
            _task: task,
        }
    }

    /// Returns a handle that can be stored in the VM's inspect tree to
    /// expose the VFIO container/group topology.
    pub fn inspect_handle(&self) -> VfioManagerClient {
        self.client.clone()
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VfioDeviceHandle> for VfioDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VfioDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let VfioDeviceHandle {
            pci_id,
            group,
            bar_addresses,
        } = resource;

        // The legacy VFIO group/type1 path can only do identity DMA, so only a
        // device with no relevant IOMMU may be passed through here. Match
        // exhaustively so a new disposition can't silently slip through.
        match input.dma_target.passthrough() {
            pci_core::dma::DmaPassthrough::Allowed => {}
            pci_core::dma::DmaPassthrough::SoftwareBlocked => {
                anyhow::bail!("VFIO device {pci_id} is behind a software IOMMU")
            }
            pci_core::dma::DmaPassthrough::HardwareNestable(_) => anyhow::bail!(
                "VFIO device {pci_id} needs a hardware-nestable IOMMU: use the cdev path"
            ),
        }

        tracing::info!(pci_id, "opening VFIO device");

        // Ask the container manager to prepare (or reuse) a container and
        // group for this device.
        let binding = self
            .client
            .prepare_device(pci_id.clone(), group)
            .await
            .context("VFIO container manager failed")?;

        let memory_mapper = input
            .shared_mem_mapper
            .context("memory mapper is required for VFIO device assignment")?;

        let device = VfioAssignedPciDevice::new(
            binding,
            pci_id,
            input.register_mmio,
            input.dma_target.msi_target(),
            memory_mapper,
            bar_addresses,
        )
        .await?;

        Ok(device.into())
    }
}

/// Resource resolver for [`VfioCdevDeviceHandle`] (cdev + iommufd path).
///
/// Spawns a `VfioCdevManager` task internally and communicates with it via RPC
/// to share IOAS contexts across devices referencing the same iommu ID.
///
/// Devices whose [`DmaTarget`](pci_core::dma::DmaTarget) reports
/// [`DmaPassthrough::HardwareNestable`](pci_core::dma::DmaPassthrough::HardwareNestable)
/// (an accel-capable SMMU) get iommufd nested S1 translation: the resolver
/// downcasts the opaque nesting handle to a [`smmu::SmmuNestingContext`],
/// allocates the S2 parent HWPT, creates the per-SMMU accel state and
/// per-device stream backend, and registers the backend with the SMMU shared
/// state.
pub struct VfioCdevDeviceResolver {
    realm_assignment: Option<Arc<dyn pci_core::vfio::VfioVmProvider>>,
    client: crate::manager::VfioCdevManagerClient,
    _task: pal_async::task::Task<()>,
}

impl VfioCdevDeviceResolver {
    /// Create a new cdev resolver, spawning the cdev dispatcher task.
    pub fn new(
        spawner: impl pal_async::driver::SpawnDriver,
        dma_mapper_client: DmaMapperClient,
    ) -> Self {
        // Arc the spawner so the dispatcher can spawn per-iommu manager tasks.
        let spawner: Arc<dyn pal_async::driver::SpawnDriver> = Arc::new(spawner);
        let mut manager = crate::manager::VfioCdevManager::new(spawner.clone(), dma_mapper_client);
        let client = manager.client();
        let task = spawner.spawn("vfio-cdev-dispatch", manager.run());
        Self {
            realm_assignment: None,
            client,
            _task: task,
        }
    }

    /// Supply lazy VM association access for the separate Realm object path.
    /// This does not enable CCA assignment or change ordinary cdev resolution.
    pub fn with_realm_assignment_provider(
        mut self,
        provider: Option<Arc<dyn pci_core::vfio::VfioVmProvider>>,
    ) -> Self {
        self.realm_assignment = provider;
        self
    }

    /// Prepare an unmapped Realm object graph without exposing a PCI device.
    ///
    /// The caller retains the returned owner through final-RID attachment and
    /// cleanup, including the recovery owner on failure. Ordinary resource
    /// resolution does not call this path; CCA admission remains disabled.
    pub fn prepare_realm_objects(
        &self,
        cdev: std::fs::File,
        iommufd: std::fs::File,
    ) -> Result<crate::realm::RealmDevice, crate::realm::RealmPrepareError> {
        crate::realm::RealmDevice::prepare(self.realm_assignment.as_deref(), cdev, iommufd)
    }

    /// Returns a handle for the VM's inspect tree.
    pub fn inspect_handle(&self) -> crate::manager::VfioCdevManagerClient {
        self.client.clone()
    }
}

fn validate_ordinary_cdev_path(realm_provider_present: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        !realm_provider_present,
        "Realm object preparation is not yet enabled as a PCI device; refusing ordinary DMA mapping"
    );
    Ok(())
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VfioCdevDeviceHandle> for VfioCdevDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VfioCdevDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        validate_ordinary_cdev_path(self.realm_assignment.is_some())?;
        let VfioCdevDeviceHandle {
            pci_id,
            cdev,
            iommufd,
            iommu_id,
            bar_addresses,
        } = resource;

        // Inspect the device's passthrough disposition. A software/emulated
        // IOMMU cannot program the host IOMMU, so reject. A hardware-nestable
        // IOMMU hands us an opaque handle we downcast to the SMMU nesting
        // context and wire up below; a plain (allowed) target needs no
        // nesting.
        let nesting_ctx: Option<smmu::SmmuNestingContext> = match input.dma_target.passthrough() {
            pci_core::dma::DmaPassthrough::SoftwareBlocked => {
                anyhow::bail!(
                    "VFIO device {pci_id} is behind a software IOMMU that cannot \
                     program the host IOMMU for passthrough DMA"
                );
            }

            pci_core::dma::DmaPassthrough::Allowed => None,
            pci_core::dma::DmaPassthrough::HardwareNestable(handle) => Some(
                handle
                    .downcast_ref::<smmu::SmmuNestingContext>()
                    .context("hardware-nestable DMA target was not an SMMU nesting context")?
                    .clone(),
            ),
        };

        // The manager shares one vIOMMU per emulated SMMU, matched by the
        // identity (`Arc::ptr_eq`) of the SMMU's shared state. Hand it the
        // `Arc` directly; `None` signals the plain identity-DMA path (no
        // nesting).
        let vsmmu = nesting_ctx.as_ref().map(|ctx| ctx.shared.clone());

        tracing::info!(
            pci_id,
            iommu_id,
            needs_nesting = nesting_ctx.is_some(),
            "opening VFIO cdev device with iommufd"
        );

        let mut resp = self
            .client
            .prepare_device(crate::manager::CdevPrepareRequest {
                pci_id: pci_id.clone(),
                cdev,
                iommufd,
                iommu_id,
                vsmmu,
            })
            .await
            .context("VFIO cdev manager failed")?;

        // One owned handle to the VFIO device, shared (via `Arc`) by the PCI
        // emulation and, for a nested device, the iommufd stream backend — so a
        // single fd serves both, with no `dup` (mirroring QEMU's one
        // `vbasedev->fd`).
        let nesting = resp.nesting.take();
        let iommufd_devid = resp.iommufd_devid;
        let (device, cdev_binding) =
            crate::manager::VfioCdevBindingState::from_response(resp, pci_id.clone());

        // If the device is nested, wire the manager's iommufd objects into
        // the emulated SMMU. The manager already created (or reused) the
        // shared vIOMMU and queried host capabilities. The device gets no
        // StreamID here — PCI routing supplies the BDF one is derived from,
        // so it stays blocked until the guest assigns it.
        let mut accel_stream = None;
        if let (Some(ctx), Some(nesting)) = (nesting_ctx, nesting) {
            // Bind the vSMMU to the physical SMMU and vIOMMU backing this
            // device, finalizing host-derived parameters (OAS, ...). Runs once
            // per vSMMU; a later device on a different physical SMMU or vIOMMU
            // is rejected here.
            ctx.shared
                .bind_accel_viommu(nesting.host_caps, &nesting.accel_state)
                .with_context(|| format!("device {pci_id} is incompatible with the host SMMU"))?;

            accel_stream = Some(
                crate::iommufd_nesting::AccelStream::new(
                    &ctx,
                    nesting.accel_state,
                    iommufd_devid,
                    device.clone(),
                )
                .with_context(|| format!("failed to attach device {pci_id} to the SMMU"))?,
            );

            tracing::info!(pci_id, "registered iommufd nesting backend with SMMU");
        }

        let memory_mapper = input
            .shared_mem_mapper
            .context("memory mapper is required for VFIO device assignment")?;

        let assigned = VfioAssignedPciDevice::from_cdev(
            device,
            cdev_binding,
            pci_id,
            input.register_mmio,
            input.dma_target.msi_target(),
            memory_mapper,
            bar_addresses,
            accel_stream,
        )
        .await?;

        Ok(assigned.into())
    }
}

#[cfg(test)]
mod realm_guard_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn realm_provider_cannot_fall_back_to_ordinary_dma_mapping() {
        validate_ordinary_cdev_path(false).unwrap();
        assert!(validate_ordinary_cdev_path(true).is_err());
    }

    struct TeardownService {
        fail: AtomicBool,
        calls: AtomicUsize,
        closed: AtomicBool,
    }

    #[async_trait::async_trait]
    impl tdisp::host::EvidenceService for TeardownService {
        fn close_admission(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }

        async fn object_size(
            &self,
            _: tdisp::host::Object,
        ) -> Result<usize, tdisp::host::EvidenceError> {
            Err(tdisp::host::EvidenceError::Unsupported)
        }

        async fn read_object(
            &self,
            _: tdisp::host::Object,
            _: u64,
            _: u64,
            _: Arc<dyn tdisp::host::EvidenceSink>,
        ) -> Result<usize, tdisp::host::EvidenceError> {
            Err(tdisp::host::EvidenceError::Unsupported)
        }

        async fn teardown(&self) -> Result<(), tdisp::host::EvidenceError> {
            assert!(self.closed.load(Ordering::SeqCst));
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(tdisp::host::EvidenceError::Device(Box::new(
                    std::io::Error::other("cleanup failed"),
                )));
            }
            Ok(())
        }
    }

    #[test]
    fn teardown_handle_retains_all_services_on_failure_and_retries() {
        let handle = RealmTeardownHandle::default();
        let first = Arc::new(TeardownService {
            fail: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });
        let second = Arc::new(TeardownService {
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });
        let weak_first = Arc::downgrade(&first);
        let weak_second = Arc::downgrade(&second);
        handle.retain(0x100, first.clone()).unwrap();
        handle.retain(0x200, second.clone()).unwrap();
        assert!(handle.retain(0x100, second.clone()).is_err());
        drop(first);
        drop(second);
        let error = futures::executor::block_on(handle.teardown()).unwrap_err();
        assert_eq!(error.requester_id, 0x100);
        let first = weak_first.upgrade().unwrap();
        let second = weak_second.upgrade().unwrap();
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.calls.load(Ordering::SeqCst), 1);
        first.fail.store(false, Ordering::SeqCst);
        assert!(handle.retain(0x300, first.clone()).is_err());
        drop(first);
        drop(second);
        futures::executor::block_on(handle.teardown()).unwrap();
        assert!(weak_first.upgrade().is_none());
        assert!(weak_second.upgrade().is_none());
    }
}
