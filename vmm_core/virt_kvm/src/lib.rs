// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM implementation of the virt::generic interfaces.

#![cfg(all(target_os = "linux", guest_is_native))]
#![expect(missing_docs)]
// UNSAFETY: Calling KVM APIs and manually managing memory.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

mod arch;
mod gsi;
mod memory;
#[cfg(guest_arch = "x86_64")]
mod snp;

pub use arch::Kvm;

use guestmem::GuestMemory;
use inspect::Inspect;
use memory::KvmMemoryBackingMode;
use memory::KvmMemoryRangeState;
#[cfg(guest_arch = "aarch64")]
use memory::private_memory_range_from_slots;
use memory_range::MemoryRange;
use parking_lot::Mutex;
#[cfg(guest_arch = "x86_64")]
use snp::SnpLaunchState;
use std::sync::Arc;
use thiserror::Error;
use virt::state::StateError;

/// Returns whether KVM is available on this machine.
pub fn is_available() -> Result<bool, KvmError> {
    match std::fs::metadata("/dev/kvm") {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(KvmError::AvailableCheck(err)),
    }
}

use arch::KvmVpInner;
use loader::importer::BootPageAcceptance;
#[cfg(guest_arch = "x86_64")]
use std::fs::File;
use std::sync::atomic::Ordering;
use virt::VpIndex;
use vmcore::vmtime::VmTimeAccess;

#[derive(Error, Debug)]
pub enum KvmError {
    #[error("operation not supported")]
    NotSupported,
    #[error("vtl2 is not supported on this hypervisor")]
    Vtl2NotSupported,
    #[error("isolation is not supported on this hypervisor")]
    IsolationNotSupported,
    #[error("unsupported isolation configuration: {0}")]
    UnsupportedIsolationConfiguration(&'static str),
    #[error("failed to open /dev/sev")]
    OpenSev(#[source] std::io::Error),
    #[error("SNP private memory is not implemented")]
    SnpPrivateMemoryNotImplemented,
    #[error("missing KVM CCA capability: {0}")]
    MissingCcaCapability(&'static str),
    #[error("CCA realm VMs require GICv3")]
    CcaRequiresGicV3,
    #[error("guest_memfd-backed KVM VM launch is not implemented")]
    GuestMemfdLaunchNotImplemented,
    #[error("unsupported SNP launch page acceptance: {0:?}")]
    UnsupportedSnpPageAcceptance(BootPageAcceptance),
    #[error("kvm error")]
    Kvm(#[from] kvm::Error),
    #[error("failed to stat /dev/kvm")]
    AvailableCheck(#[source] std::io::Error),
    #[error(transparent)]
    State(#[from] Box<StateError<KvmError>>),
    #[error("invalid state while restoring: {0}")]
    InvalidState(&'static str),
    #[error("misaligned memory range for KVM guest_memfd")]
    MisalignedMemoryRange,
    #[error("cannot resize KVM guest_memfd memory slot")]
    CannotResizeGuestMemfdSlot,
    #[error("private memory range is not page aligned")]
    UnalignedPrivateMemoryRange,
    #[error("private memory range is not contained in guest_memfd private memory")]
    InvalidPrivateMemoryRange,
    #[error("SNP launch range is not page aligned")]
    UnalignedSnpLaunchRange,
    #[error("SNP launch range is not contained in guest_memfd private memory")]
    InvalidSnpLaunchRange,
    #[error("too many CPUID entries for SNP launch page: {0}")]
    TooManySnpCpuidEntries(usize),
    #[error("missing SNP C-bit CPUID information")]
    MissingSnpCBit,
    #[error("invalid SNP direct-boot page table length: {0:#x}")]
    InvalidSnpPageTableLength(u64),
    #[error("SNP launch is already in progress")]
    SnpLaunchInProgress,
    #[error("SNP launch previously failed")]
    SnpLaunchFailed,
    #[error("unsupported CCA initial page acceptance: {0:?}")]
    UnsupportedCcaPageAcceptance(BootPageAcceptance),
    #[error("CCA initial page population is already in progress")]
    CcaPopulateInProgress,
    #[error("CCA initial page population previously failed")]
    CcaPopulateFailed,
    #[error("CCA initial population range is not page aligned")]
    UnalignedCcaPopulateRange,
    #[error("CCA initial population range is not contained in guest_memfd private memory")]
    InvalidCcaPopulateRange,
    #[error("invalid KVM_HC_MAP_GPA_RANGE request")]
    InvalidMapGpaRange,
    #[error("unsupported KVM_HC_MAP_GPA_RANGE attributes: {0:#x}")]
    UnsupportedMapGpaRangeAttributes(u64),
    #[error("failed to discard shared backing after private conversion")]
    DiscardSharedBacking(#[source] std::io::Error),
    #[error("failed to discard private backing after shared conversion")]
    DiscardPrivateBacking(#[source] std::io::Error),
    #[cfg(guest_arch = "aarch64")]
    #[error("invalid CCA memory fault")]
    InvalidCcaMemoryFault,
    #[cfg(guest_arch = "aarch64")]
    #[error("unsupported CCA memory fault flags: {0:#x}")]
    UnsupportedCcaMemoryFaultFlags(u64),
    #[error("misaligned gic base address")]
    Misaligned,
    #[error("host does not support GICv2 or GICv3")]
    NoGic,
    #[error("host does not support required cpu capabilities")]
    Capabilities(virt::PartitionCapabilitiesError),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to compute topology cpuid")]
    TopologyCpuid(#[source] virt::x86::topology::UnknownVendor),
}

#[derive(Inspect)]
pub struct KvmPartition {
    #[inspect(flatten)]
    inner: Arc<KvmPartitionInner>,
    #[inspect(skip)]
    synic_ports: Arc<virt::synic::SynicPorts<KvmPartitionInner>>,
    #[inspect(skip)]
    irqfd_state: Arc<gsi::KvmIrqFdState>,
}

#[derive(Inspect)]
struct KvmPartitionInner {
    #[inspect(skip)]
    kvm: kvm::Partition,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    sev: Option<File>,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    snp_launch_state: Mutex<SnpLaunchState>,
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    cca_launch_state: Mutex<CcaLaunchState>,
    #[cfg(guest_arch = "aarch64")]
    cca_shared_gpa_bit: Option<u64>,
    memory: Mutex<KvmMemoryRangeState>,
    memory_backing_mode: KvmMemoryBackingMode,
    #[inspect(iter_by_index)]
    ram_ranges: Vec<MemoryRange>,
    hv1_enabled: bool,
    gm: GuestMemory,
    #[cfg(guest_arch = "x86_64")]
    #[inspect(skip)]
    bsp_cpuid: Vec<kvm::kvm_cpuid_entry2>,
    #[inspect(skip)]
    vps: Vec<KvmVpInner>,
    #[inspect(skip)]
    gsi_routing: Mutex<gsi::GsiRouting>,
    caps: virt::PartitionCapabilities,

    // This is used for debugging via Inspect
    #[cfg(guest_arch = "x86_64")]
    cpuid: virt::CpuidLeafSet,

    /// The GIC device fd, kept alive for the VM lifetime.
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    _gic_device: kvm::Device,
    /// The ITS device fd, kept alive for the VM lifetime.
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    _its_device: Option<kvm::Device>,
    /// MSI controller configuration (v2m, ITS, or none).
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    gic_msi: vm_topology::processor::aarch64::GicMsiController,
    /// Total configured GIC interrupt count (SGIs + PPIs + SPIs).
    #[cfg(guest_arch = "aarch64")]
    gic_nr_irqs: u32,
    synic_ports: virt::synic::SynicPortMap,
}

#[cfg(guest_arch = "aarch64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum CcaLaunchState {
    NotStarted,
    Populating,
    Populated,
    Failed,
}

#[cfg(guest_arch = "aarch64")]
impl virt::FinalizeInitialPageImports for KvmPartition {
    type Error = KvmError;

    fn finalize_initial_page_imports(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), Self::Error> {
        self.inner.cca_populate_initial_pages(pages)
    }
}

// TODO: Chunk this up into smaller types.
#[derive(Debug, Error)]
enum KvmRunVpError {
    #[error("KVM internal error: {0:#x}")]
    InternalError(u32),
    #[error("invalid vp state")]
    InvalidVpState,
    #[error("failed to run VP")]
    Run(#[source] kvm::Error),
    #[cfg(guest_arch = "aarch64")]
    #[error(
        "unsupported KVM memory fault/RIPAS change: flags={flags:#x}, gpa={gpa:#x}, size={size:#x}"
    )]
    UnsupportedMemoryFault { flags: u64, gpa: u64, size: u64 },
    #[cfg(guest_arch = "aarch64")]
    #[error("unhandled KVM exit: {0}")]
    UnhandledExit(String),
    #[cfg(guest_arch = "aarch64")]
    #[error("CCA initial page population has not completed")]
    CcaNotPopulated,
    #[cfg(guest_arch = "aarch64")]
    #[error("CCA initial page population failed")]
    CcaPopulationFailed,
    #[error("unhandled system event type: {0:#x}")]
    UnhandledSystemEvent(u32),
    #[cfg(guest_arch = "x86_64")]
    #[error(
        "SEV guest requested termination: ghcb_msr={ghcb_msr:#x} reason_set={reason_set:#x} reason={reason:#x}"
    )]
    SevTermination {
        ghcb_msr: u64,
        reason_set: u64,
        reason: u64,
    },
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to inject an extint interrupt")]
    ExtintInterrupt(#[source] kvm::Error),
}

impl KvmRunVpError {
    #[cfg(guest_arch = "aarch64")]
    fn from_kvm_run_error(err: kvm::Error) -> Self {
        match err {
            kvm::Error::RunMemoryFault {
                flags, gpa, size, ..
            } => {
                tracelimit::warn_ratelimited!(
                    flags,
                    gpa,
                    size,
                    "unsupported KVM memory fault/RIPAS change"
                );
                KvmRunVpError::UnsupportedMemoryFault { flags, gpa, size }
            }
            err => KvmRunVpError::Run(err),
        }
    }
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
pub struct KvmProcessorBinder {
    partition: Arc<KvmPartitionInner>,
    vpindex: VpIndex,
    vmtime: VmTimeAccess,
}

impl KvmPartitionInner {
    #[cfg(guest_arch = "x86_64")]
    fn bsp(&self) -> &KvmVpInner {
        &self.vps[0]
    }

    fn vp(&self, vp_index: VpIndex) -> Option<&KvmVpInner> {
        self.vps.get(vp_index.index() as usize)
    }

    fn evaluate_vp(&self, vp_index: VpIndex) {
        let Some(vp) = self.vp(vp_index) else { return };
        vp.set_eval(true, Ordering::Relaxed);

        #[cfg(guest_arch = "x86_64")]
        self.kvm.vp(vp.vp_info().apic_id).force_exit();

        #[cfg(guest_arch = "aarch64")]
        self.kvm.vp(vp.vp_info().base.vp_index.index()).force_exit();
    }

    #[cfg(guest_arch = "aarch64")]
    fn cca_populate_initial_pages(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        {
            let mut state = self.cca_launch_state.lock();
            match *state {
                CcaLaunchState::NotStarted => *state = CcaLaunchState::Populating,
                CcaLaunchState::Populating => return Err(KvmError::CcaPopulateInProgress),
                CcaLaunchState::Populated => return Ok(()),
                CcaLaunchState::Failed => return Err(KvmError::CcaPopulateFailed),
            }
        }

        tracing::info!(page_ranges = pages.len(), "starting CCA initial population");
        match self.cca_populate_initial_pages_inner(pages) {
            Ok(()) => {
                *self.cca_launch_state.lock() = CcaLaunchState::Populated;
                tracing::info!("finished CCA initial population");
                Ok(())
            }
            Err(err) => {
                *self.cca_launch_state.lock() = CcaLaunchState::Failed;
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed CCA initial population"
                );
                Err(err)
            }
        }
    }

    #[cfg(guest_arch = "aarch64")]
    fn cca_populate_initial_pages_inner(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        self.kvm
            .check_private_memory_extensions()
            .map_err(map_cca_capability_error)?;

        let pages = pages.to_vec();

        let memory = self.memory.lock();
        for page in &pages {
            if page.visibility == virt::PageVisibility::Shared {
                tracing::trace!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    acceptance = ?page.acceptance,
                    tag = page.tag.as_str(),
                    "skipping shared CCA initial page"
                );
                continue;
            }

            let flags = cca_populate_flags(page.acceptance)?;
            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)
                .map_err(map_cca_private_range_error)?;
            let mut populate = kvm::KvmArmRmiPopulate {
                base: private_range.gpa.start(),
                size: private_range.gpa.len(),
                source_uaddr: private_range.hva as u64,
                flags,
                reserved: 0,
            };

            while populate.size != 0 {
                tracing::trace!(
                    gpa = populate.base,
                    len = populate.size,
                    source_uaddr = populate.source_uaddr,
                    flags = populate.flags,
                    acceptance = ?page.acceptance,
                    tag = page.tag.as_str(),
                    "KVM_ARM_RMI_POPULATE"
                );
                self.kvm.arm_rmi_populate(&mut populate)?;
            }
        }

        Ok(())
    }
}

#[cfg(guest_arch = "aarch64")]
fn cca_populate_flags(acceptance: BootPageAcceptance) -> Result<u32, KvmError> {
    match acceptance {
        BootPageAcceptance::Exclusive => Ok(kvm::KVM_ARM_RMI_POPULATE_FLAGS_MEASURE_UAPI),
        BootPageAcceptance::ExclusiveUnmeasured => Ok(0),
        BootPageAcceptance::Shared => Ok(0),
        BootPageAcceptance::VpContext
        | BootPageAcceptance::SecretsPage
        | BootPageAcceptance::CpuidPage
        | BootPageAcceptance::CpuidExtendedStatePage
        | BootPageAcceptance::ErrorPage => Err(KvmError::UnsupportedCcaPageAcceptance(acceptance)),
    }
}

#[cfg(guest_arch = "aarch64")]
fn map_cca_private_range_error(err: KvmError) -> KvmError {
    match err {
        KvmError::UnalignedPrivateMemoryRange => KvmError::UnalignedCcaPopulateRange,
        KvmError::InvalidPrivateMemoryRange => KvmError::InvalidCcaPopulateRange,
        err => err,
    }
}

#[cfg(guest_arch = "aarch64")]
fn map_cca_conversion_error(err: KvmError) -> KvmError {
    match err {
        KvmError::InvalidMapGpaRange => KvmError::InvalidCcaMemoryFault,
        err => err,
    }
}

#[cfg(guest_arch = "aarch64")]
fn map_cca_capability_error(err: kvm::Error) -> KvmError {
    match err {
        kvm::Error::MissingCapability(capability) => KvmError::MissingCcaCapability(capability),
        err => KvmError::Kvm(err),
    }
}
