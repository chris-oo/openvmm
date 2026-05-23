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

pub use arch::Kvm;

use guestmem::GuestMemory;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
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
use hvdef::Vtl;
use loader::importer::BootPageAcceptance;
use std::fs::File;
#[cfg(guest_arch = "x86_64")]
use std::mem::size_of;
#[cfg(all(test, guest_arch = "x86_64"))]
use std::mem::size_of_val;
#[cfg(guest_arch = "x86_64")]
use std::os::fd::AsFd;
#[cfg(guest_arch = "x86_64")]
use std::os::fd::AsRawFd;
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
    #[error("failed to discard shared backing for SNP private conversion")]
    DiscardSharedBacking(#[source] std::io::Error),
    #[error("failed to discard private backing for SNP shared conversion")]
    DiscardPrivateBacking(#[source] std::io::Error),
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

#[derive(Debug, Inspect)]
struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
    #[inspect(skip)]
    guest_memfd: Option<File>,
    private_attributes_set: bool,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    ranges: Vec<Option<KvmMemoryRange>>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct KvmPrivateMemoryRange {
    gpa: MemoryRange,
    hva: *mut u8,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
enum KvmMemoryBackingMode {
    Userspace,
    GuestMemfd,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum KvmMemoryBacking {
    Userspace,
    GuestMemfd,
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

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum SnpLaunchState {
    NotStarted,
    Started,
    Finished,
    Failed,
}

#[cfg(guest_arch = "aarch64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum CcaLaunchState {
    NotStarted,
    Populating,
    Populated,
    Failed,
}

#[cfg(guest_arch = "x86_64")]
impl virt::AcceptInitialPages for KvmPartition {
    type Error = KvmError;

    fn accept_initial_pages(&self, pages: &[virt::InitialAcceptedPage]) -> Result<(), Self::Error> {
        self.inner.snp_launch_initial_pages(pages)
    }
}

#[cfg(guest_arch = "aarch64")]
impl virt::AcceptInitialPages for KvmPartition {
    type Error = KvmError;

    fn accept_initial_pages(&self, pages: &[virt::InitialAcceptedPage]) -> Result<(), Self::Error> {
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

    /// # Safety
    ///
    /// `data..data+size` must be and remain an allocated VA range until the
    /// partition is destroyed or the region is unmapped.
    unsafe fn map_region(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        readonly: bool,
    ) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size as u64);
        let backing = self.memory_backing(range)?;
        if backing == KvmMemoryBacking::GuestMemfd && !is_page_aligned(data, addr, size as u64) {
            return Err(KvmError::MisalignedMemoryRange.into());
        }
        let mut state = self.memory.lock();

        // Memory slots cannot be resized but can be moved within the guest
        // address space. Find the existing slot if there is one.
        let mut slot_to_use = None;
        for (slot, range) in state.ranges.iter_mut().enumerate() {
            match range {
                Some(range) if range.host_addr == data => {
                    slot_to_use = Some(slot);
                    break;
                }
                Some(_) => (),
                None => slot_to_use = Some(slot),
            }
        }
        if slot_to_use.is_none() {
            slot_to_use = Some(state.ranges.len());
            state.ranges.push(None);
        }
        let slot_to_use = slot_to_use.unwrap();
        if let Some(existing_range) = &state.ranges[slot_to_use] {
            if existing_range.guest_memfd.is_some() && existing_range.range.len() != size as u64 {
                return Err(KvmError::CannotResizeGuestMemfdSlot.into());
            }
            if existing_range.private_attributes_set {
                self.kvm.set_memory_attributes(
                    existing_range.range.start(),
                    existing_range.range.len(),
                    0,
                )?;
            }
            if existing_range.guest_memfd.is_some() {
                // SAFETY: clearing a slot removes the memory reference.
                unsafe {
                    self.kvm.set_user_memory_region2(
                        slot_to_use as u32,
                        std::ptr::null_mut(),
                        0,
                        0,
                        false,
                        None,
                    )?;
                }
                state.ranges[slot_to_use] = None;
            }
        }
        let guest_memfd = match backing {
            KvmMemoryBacking::Userspace => {
                // SAFETY: guaranteed by caller.
                unsafe {
                    self.kvm.set_user_memory_region(
                        slot_to_use as u32,
                        data,
                        size,
                        addr,
                        readonly,
                    )?
                };
                None
            }
            KvmMemoryBacking::GuestMemfd => {
                let guest_memfd = self.kvm.create_guest_memfd(size as u64)?;
                // SAFETY: guaranteed by caller. The slot record below owns the
                // guest_memfd for at least as long as KVM references it.
                unsafe {
                    self.kvm.set_user_memory_region2(
                        slot_to_use as u32,
                        data,
                        size,
                        addr,
                        readonly,
                        Some((&guest_memfd, 0)),
                    )?;
                };
                if let Err(err) = self.kvm.set_memory_attributes(
                    addr,
                    size as u64,
                    kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64,
                ) {
                    // SAFETY: clearing a slot removes the memory reference.
                    let clear_result = unsafe {
                        self.kvm.set_user_memory_region2(
                            slot_to_use as u32,
                            std::ptr::null_mut(),
                            0,
                            0,
                            false,
                            None,
                        )
                    };
                    if let Err(clear_err) = clear_result {
                        tracing::error!(
                            error = &clear_err as &dyn std::error::Error,
                            "failed to clear KVM guest_memfd slot after private attribute setup failed"
                        );
                        state.ranges[slot_to_use] = Some(KvmMemoryRange {
                            host_addr: data,
                            range,
                            guest_memfd: Some(guest_memfd),
                            private_attributes_set: false,
                        });
                    } else {
                        state.ranges[slot_to_use] = None;
                    }
                    return Err(err.into());
                }
                Some(guest_memfd)
            }
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range,
            guest_memfd,
            private_attributes_set: backing == KvmMemoryBacking::GuestMemfd,
        });
        Ok(())
    }

    fn memory_backing(&self, range: MemoryRange) -> Result<KvmMemoryBacking, KvmError> {
        match self.memory_backing_mode {
            KvmMemoryBackingMode::Userspace => Ok(KvmMemoryBacking::Userspace),
            KvmMemoryBackingMode::GuestMemfd => {
                classify_guest_memfd_backing(range, &self.ram_ranges)
            }
        }
    }

    #[cfg(guest_arch = "x86_64")]
    fn set_map_gpa_range_attributes(
        &self,
        gpa: u64,
        page_count: u64,
        map_attributes: u64,
    ) -> Result<(), KvmError> {
        const KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK: u64 = 0x3;
        const KVM_MAP_GPA_RANGE_ENC_STATUS_MASK: u64 = 0xf << 4;

        let size = page_count
            .checked_mul(hvdef::HV_PAGE_SIZE)
            .ok_or(KvmError::InvalidMapGpaRange)?;
        let end = gpa.checked_add(size).ok_or(KvmError::InvalidMapGpaRange)?;
        if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE) || size == 0 {
            return Err(KvmError::InvalidMapGpaRange);
        }
        let unsupported_attributes = map_attributes
            & !(KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK | KVM_MAP_GPA_RANGE_ENC_STATUS_MASK);
        if unsupported_attributes != 0 {
            return Err(KvmError::UnsupportedMapGpaRangeAttributes(map_attributes));
        }
        let private = match map_attributes & KVM_MAP_GPA_RANGE_ENC_STATUS_MASK {
            kvm::KVM_MAP_GPA_RANGE_DECRYPTED_UAPI => false,
            kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI => true,
            _ => return Err(KvmError::UnsupportedMapGpaRangeAttributes(map_attributes)),
        };

        let range = MemoryRange::new(gpa..end);
        if !self.ram_ranges.iter().any(|ram| ram.contains(&range)) {
            return Err(KvmError::InvalidMapGpaRange);
        }

        let attributes = if private {
            kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
        } else {
            0
        };
        tracing::debug!(
            gpa,
            size,
            page_count,
            map_attributes,
            private,
            "KVM_HC_MAP_GPA_RANGE set memory attributes"
        );
        self.kvm.set_memory_attributes(gpa, size, attributes)?;
        self.discard_stale_snp_backing(range, private)?;
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn discard_stale_snp_backing(&self, range: MemoryRange, private: bool) -> Result<(), KvmError> {
        let state = self.memory.lock();
        let slot = state
            .ranges
            .iter()
            .flatten()
            .find(|slot| slot.range.contains(&range))
            .ok_or(KvmError::InvalidMapGpaRange)?;
        let offset = range.start() - slot.range.start();
        if private {
            let addr = slot.host_addr.wrapping_add(offset as usize);
            tracing::debug!(
                gpa = range.start(),
                size = range.len(),
                hva = addr as usize,
                "discarding shared backing after SNP private conversion"
            );
            let ret =
                unsafe { libc::madvise(addr.cast(), range.len() as usize, libc::MADV_DONTNEED) };
            if ret != 0 {
                return Err(KvmError::DiscardSharedBacking(
                    std::io::Error::last_os_error(),
                ));
            }
        } else {
            let guest_memfd = slot
                .guest_memfd
                .as_ref()
                .ok_or(KvmError::InvalidMapGpaRange)?;
            tracing::debug!(
                gpa = range.start(),
                size = range.len(),
                guest_memfd_offset = offset,
                "discarding private backing after SNP shared conversion"
            );
            let ret = unsafe {
                libc::fallocate(
                    guest_memfd.as_raw_fd(),
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    offset as libc::off_t,
                    range.len() as libc::off_t,
                )
            };
            if ret != 0 {
                return Err(KvmError::DiscardPrivateBacking(
                    std::io::Error::last_os_error(),
                ));
            }
        }
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn snp_launch_initial_pages(
        &self,
        pages: &[virt::InitialAcceptedPage],
    ) -> Result<(), KvmError> {
        {
            let mut state = self.snp_launch_state.lock();
            match *state {
                SnpLaunchState::NotStarted => *state = SnpLaunchState::Started,
                SnpLaunchState::Started => return Err(KvmError::SnpLaunchInProgress),
                SnpLaunchState::Finished => return Ok(()),
                SnpLaunchState::Failed => return Err(KvmError::SnpLaunchFailed),
            }
        }

        tracing::info!(page_ranges = pages.len(), "starting SNP launch");
        match self.snp_launch_initial_pages_inner(pages) {
            Ok(()) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Finished;
                tracing::info!("finished SNP launch");
                Ok(())
            }
            Err(err) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Failed;
                tracing::error!(error = &err as &dyn std::error::Error, "failed SNP launch");
                Err(err)
            }
        }
    }

    #[cfg(guest_arch = "x86_64")]
    fn snp_launch_initial_pages_inner(
        &self,
        pages: &[virt::InitialAcceptedPage],
    ) -> Result<(), KvmError> {
        let sev = self.sev.as_ref().ok_or(KvmError::IsolationNotSupported)?;
        self.kvm.check_sev_snp_launch_extensions()?;
        let mut launch_start = kvm::kvm_sev_snp_launch_start {
            policy: (1 << 19) | (1 << 17) | (1 << 16),
            ..Default::default()
        };
        tracing::debug!(policy = launch_start.policy, "KVM_SEV_SNP_LAUNCH_START");
        self.kvm
            .sev_snp_launch_start(sev.as_fd(), &mut launch_start)?;

        // HACK: Until the loader provides complete launch metadata for SNP, fill in the
        // rest of RAM as accepted private pages so KVM can populate every private GFN
        // before the guest runs.
        let pages = snp_launch_pages_with_ram_hack(pages, &self.ram_ranges);

        let memory = self.memory.lock();
        for page in &pages {
            let launch_page_type = arch::snp::snp_launch_page_type(page.acceptance)?;
            let Some(kvm_page_type) = launch_page_type.kvm_page_type() else {
                return Err(KvmError::UnsupportedSnpPageAcceptance(page.acceptance));
            };

            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)?;
            if page.acceptance == BootPageAcceptance::CpuidPage {
                tracing::debug!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    cpuid_entries = self.bsp_cpuid.len(),
                    "writing SNP CPUID page"
                );
                write_snp_cpuid_page(private_range.hva, page.range.len(), &self.bsp_cpuid)?;
                Self::trace_snp_cpuid_page(
                    "SNP CPUID page before launch update",
                    private_range.hva,
                    page.range.len(),
                );
            }
            if page.tag == "linux-pagetables" {
                let c_bit = snp_c_bit_from_cpuid(&self.bsp_cpuid)?;
                tracing::debug!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    c_bit,
                    "setting SNP C-bit in Linux direct-boot page tables"
                );
                set_snp_c_bit_in_page_tables(private_range.hva, page.range.len(), c_bit)?;
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    private_range.hva.cast_const(),
                    page.range.len() as usize,
                )
            };
            for run in split_zero_page_runs(bytes)? {
                let page_type = if run.kind == SnpPageRunKind::Zero
                    && kvm_page_type == kvm::SevSnpPageType::Normal
                {
                    kvm::SevSnpPageType::Zero
                } else {
                    kvm_page_type
                };
                let gpa = page.range.start() + run.byte_offset as u64;
                let uaddr = if page_type == kvm::SevSnpPageType::Zero {
                    0
                } else {
                    private_range.hva.wrapping_add(run.byte_offset) as u64
                };
                tracing::trace!(
                    gpa,
                    len = run.byte_len,
                    ?page_type,
                    acceptance = ?page.acceptance,
                    tag = page.tag.as_str(),
                    "KVM_SEV_SNP_LAUNCH_UPDATE"
                );
                let cpuid_page_before =
                    (page.acceptance == BootPageAcceptance::CpuidPage).then(|| unsafe {
                        std::slice::from_raw_parts(private_range.hva, page.range.len() as usize)
                            .to_vec()
                    });
                if let Err(err) = self.kvm.sev_snp_launch_update(
                    sev.as_fd(),
                    gpa / hvdef::HV_PAGE_SIZE,
                    uaddr,
                    run.byte_len as u64,
                    page_type,
                ) {
                    if let Some(cpuid_page_before) = cpuid_page_before {
                        Self::trace_snp_cpuid_page_diff(
                            &cpuid_page_before,
                            private_range.hva,
                            page.range.len(),
                        );
                    }
                    return Err(err.into());
                }
                if page.acceptance == BootPageAcceptance::CpuidPage {
                    Self::trace_snp_cpuid_page(
                        "SNP CPUID page after launch update",
                        private_range.hva,
                        page.range.len(),
                    );
                }
            }
        }
        self.prepare_snp_vmsa_register_state()?;
        tracing::debug!("KVM_SEV_SNP_LAUNCH_FINISH");
        self.kvm
            .sev_snp_launch_finish(sev.as_fd(), &mut Default::default())?;
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    fn cca_populate_initial_pages(
        &self,
        pages: &[virt::InitialAcceptedPage],
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
        pages: &[virt::InitialAcceptedPage],
    ) -> Result<(), KvmError> {
        self.kvm
            .check_private_memory_extensions()
            .map_err(map_cca_capability_error)?;

        let memory = self.memory.lock();
        for page in pages {
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

    #[cfg(guest_arch = "x86_64")]
    fn prepare_snp_vmsa_register_state(&self) -> Result<(), KvmError> {
        for vp in &self.vps {
            let vp_info = vp.vp_info();
            let kvm_vp = self.kvm.vp(vp_info.apic_id);
            let sregs = kvm_vp.get_sregs()?;

            let xcr0 = kvm_vp.get_xcr0()?;
            if xcr0 & x86defs::xsave::XFEATURE_X87 == 0 {
                kvm_vp.set_xcr0(xcr0 | x86defs::xsave::XFEATURE_X87)?;
            }

            if vp_info.base.vp_index.is_bsp() {
                validate_snp_bsp_register_state(&kvm_vp.get_regs()?, &sregs)?;
            }
        }

        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn trace_snp_cpuid_page(message: &'static str, page: *const u8, page_len: u64) {
        const SNP_CPUID_COUNT_MAX: usize = 64;
        const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
        const SNP_CPUID_FN_SIZE: usize = 48;

        if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64
        {
            tracing::warn!(page_len, message);
            return;
        }

        let page = unsafe { std::slice::from_raw_parts(page, page_len as usize) };
        let count = u32::from_le_bytes(page[0..4].try_into().unwrap()) as usize;
        let mut standard_range = None;
        let mut hypervisor_range = None;
        let mut extended_range = None;
        let mut snp_leaf = None;
        for index in 0..count.min(SNP_CPUID_COUNT_MAX) {
            let offset = SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE;
            let entry = Self::read_snp_cpuid_fn(&page[offset..][..SNP_CPUID_FN_SIZE]);
            match (entry.eax_in, entry.ecx_in) {
                (0, 0) => standard_range = Some(entry.eax),
                (0x4000_0000, 0) => hypervisor_range = Some(entry.eax),
                (0x8000_0000, 0) => extended_range = Some(entry.eax),
                (0x8000_001f, 0) => snp_leaf = Some((entry.eax, entry.ebx, entry.ecx, entry.edx)),
                _ => {}
            }
        }

        tracing::debug!(
            count,
            ?standard_range,
            ?hypervisor_range,
            ?extended_range,
            ?snp_leaf,
            message
        );
    }

    #[cfg(guest_arch = "x86_64")]
    fn trace_snp_cpuid_page_diff(before: &[u8], after: *const u8, page_len: u64) {
        const SNP_CPUID_COUNT_MAX: usize = 64;
        const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
        const SNP_CPUID_FN_SIZE: usize = 48;

        if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64
        {
            tracing::warn!(page_len, "SNP CPUID debug page is too small");
            return;
        }

        let after = unsafe { std::slice::from_raw_parts(after, page_len as usize) };
        let count = u32::from_le_bytes(after[0..4].try_into().unwrap()) as usize;
        tracing::warn!(count, "SNP CPUID page after firmware rejection");
        for index in 0..count.min(SNP_CPUID_COUNT_MAX) {
            let offset = SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE;
            let before_entry = &before[offset..][..SNP_CPUID_FN_SIZE];
            let after_entry = &after[offset..][..SNP_CPUID_FN_SIZE];
            if before_entry == after_entry {
                continue;
            }
            let before = Self::read_snp_cpuid_fn(before_entry);
            let after = Self::read_snp_cpuid_fn(after_entry);
            tracing::warn!(
                index,
                before.eax_in,
                before.ecx_in,
                before.xcr0_in,
                before.xss_in,
                before.eax,
                before.ebx,
                before.ecx,
                before.edx,
                after.eax_in,
                after.ecx_in,
                after.xcr0_in,
                after.xss_in,
                after.eax,
                after.ebx,
                after.ecx,
                after.edx,
                "SNP CPUID entry changed by firmware"
            );
        }
    }

    #[cfg(guest_arch = "x86_64")]
    fn read_snp_cpuid_fn(entry: &[u8]) -> SnpCpuidFn {
        SnpCpuidFn {
            eax_in: u32::from_le_bytes(entry[0..4].try_into().unwrap()),
            ecx_in: u32::from_le_bytes(entry[4..8].try_into().unwrap()),
            xcr0_in: u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            xss_in: u64::from_le_bytes(entry[16..24].try_into().unwrap()),
            eax: u32::from_le_bytes(entry[24..28].try_into().unwrap()),
            ebx: u32::from_le_bytes(entry[28..32].try_into().unwrap()),
            ecx: u32::from_le_bytes(entry[32..36].try_into().unwrap()),
            edx: u32::from_le_bytes(entry[36..40].try_into().unwrap()),
        }
    }
}

#[cfg(guest_arch = "x86_64")]
struct SnpCpuidFn {
    eax_in: u32,
    ecx_in: u32,
    xcr0_in: u64,
    xss_in: u64,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

#[cfg(guest_arch = "x86_64")]
fn snp_launch_pages_with_ram_hack(
    pages: &[virt::InitialAcceptedPage],
    ram_ranges: &[MemoryRange],
) -> Vec<virt::InitialAcceptedPage> {
    let mut pages = pages.to_vec();
    let mut imported_ranges: Vec<_> = pages.iter().map(|page| page.range).collect();
    imported_ranges.sort_by_key(|range| (range.start(), range.end()));

    for ram_range in ram_ranges {
        let mut cursor = ram_range.start();
        for imported_range in &imported_ranges {
            let start = imported_range.start().max(ram_range.start());
            let end = imported_range.end().min(ram_range.end());
            if start >= end {
                continue;
            }
            if cursor < start {
                pages.push(snp_ram_hack_page(MemoryRange::new(cursor..start)));
            }
            cursor = cursor.max(end);
        }
        if cursor < ram_range.end() {
            pages.push(snp_ram_hack_page(MemoryRange::new(cursor..ram_range.end())));
        }
    }

    pages
}

#[cfg(guest_arch = "x86_64")]
fn snp_ram_hack_page(range: MemoryRange) -> virt::InitialAcceptedPage {
    virt::InitialAcceptedPage {
        range,
        visibility: virt::PageVisibility::Exclusive,
        acceptance: BootPageAcceptance::Exclusive,
        tag: "kvm-snp-ram-hack".into(),
    }
}

#[cfg(guest_arch = "x86_64")]
fn validate_snp_bsp_register_state(
    regs: &kvm::kvm_regs,
    sregs: &kvm::kvm_sregs,
) -> Result<(), KvmError> {
    const REQUIRED_CR0: u64 = x86defs::X64_CR0_PE | x86defs::X64_CR0_PG;
    const REQUIRED_CR4: u64 = x86defs::X64_CR4_PAE;
    const REQUIRED_EFER: u64 =
        x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA | x86defs::X64_EFER_NXE;

    if sregs.cr0 & REQUIRED_CR0 != REQUIRED_CR0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR0"));
    }
    if sregs.cr3 == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR3"));
    }
    if sregs.cr4 & REQUIRED_CR4 != REQUIRED_CR4 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR4"));
    }
    if sregs.efer & REQUIRED_EFER != REQUIRED_EFER {
        return Err(KvmError::InvalidState("invalid SNP BSP EFER"));
    }
    if sregs.cs.present == 0 || sregs.cs.l == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CS"));
    }
    if sregs.cs.selector != 0x10 || sregs.ds.selector != 0x18 || sregs.es.selector != 0x18 {
        return Err(KvmError::InvalidState(
            "invalid SNP BSP Linux boot selectors",
        ));
    }
    if regs.rip == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP RIP"));
    }

    tracing::debug!(
        rip = regs.rip,
        rsi = regs.rsi,
        cr0 = sregs.cr0,
        cr3 = sregs.cr3,
        cr4 = sregs.cr4,
        efer = sregs.efer,
        vmsa_efer = sregs.efer | x86defs::X64_EFER_SVME,
        cs_selector = sregs.cs.selector,
        cs_base = sregs.cs.base,
        cs_limit = sregs.cs.limit,
        cs_type = sregs.cs.type_,
        ds_selector = sregs.ds.selector,
        es_selector = sregs.es.selector,
        ss_selector = sregs.ss.selector,
        "validated SNP BSP register state"
    );

    Ok(())
}

#[cfg(guest_arch = "x86_64")]
fn snp_c_bit_from_cpuid(cpuid: &[kvm::kvm_cpuid_entry2]) -> Result<u8, KvmError> {
    cpuid
        .iter()
        .find(|entry| entry.function == 0x8000_001f && entry.index == 0)
        .map(|entry| (entry.ebx & 0x3f) as u8)
        .ok_or(KvmError::MissingSnpCBit)
}

#[cfg(guest_arch = "x86_64")]
fn set_snp_c_bit_in_page_tables(
    page_table: *mut u8,
    page_table_len: u64,
    c_bit: u8,
) -> Result<(), KvmError> {
    if !page_table_len.is_multiple_of(size_of::<u64>() as u64) {
        return Err(KvmError::InvalidSnpPageTableLength(page_table_len));
    }

    let c_bit_mask = 1u64 << c_bit;
    // SAFETY: The caller provides a valid page-table backing region, and the
    // length is validated to be a whole number of u64 entries.
    let entries = unsafe {
        std::slice::from_raw_parts_mut(page_table.cast::<u64>(), page_table_len as usize / 8)
    };
    for entry in entries {
        if *entry & 1 != 0 {
            *entry |= c_bit_mask;
        }
    }

    Ok(())
}

#[cfg(guest_arch = "x86_64")]
fn write_snp_cpuid_page(
    page: *mut u8,
    page_len: u64,
    cpuid: &[kvm::kvm_cpuid_entry2],
) -> Result<(), KvmError> {
    const SNP_CPUID_COUNT_MAX: usize = 64;
    const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
    const SNP_CPUID_FN_SIZE: usize = 48;

    if cpuid.len() > SNP_CPUID_COUNT_MAX {
        return Err(KvmError::TooManySnpCpuidEntries(cpuid.len()));
    }
    if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64 {
        return Err(KvmError::InvalidSnpLaunchRange);
    }

    let page = unsafe { std::slice::from_raw_parts_mut(page, page_len as usize) };
    page.fill(0);
    page[..4].copy_from_slice(&(cpuid.len() as u32).to_le_bytes());

    for (index, cpuid) in cpuid.iter().copied().enumerate() {
        let mut cpuid = cpuid;
        sanitize_snp_cpuid_entry(&mut cpuid);
        let entry = &mut page[SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE..]
            [..SNP_CPUID_FN_SIZE];
        entry[0..4].copy_from_slice(&cpuid.function.to_le_bytes());
        entry[4..8].copy_from_slice(&cpuid.index.to_le_bytes());
        let initial_xsave_leaf = cpuid.function
            == x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
            && (cpuid.index == 0 || cpuid.index == 1);
        let (xcr0, xss) = if initial_xsave_leaf {
            (1_u64, 0_u64)
        } else {
            (0_u64, 0_u64)
        };
        entry[8..16].copy_from_slice(&xcr0.to_le_bytes());
        entry[16..24].copy_from_slice(&xss.to_le_bytes());
        entry[24..28].copy_from_slice(&cpuid.eax.to_le_bytes());
        let ebx = if initial_xsave_leaf { 0x240 } else { cpuid.ebx };
        entry[28..32].copy_from_slice(&ebx.to_le_bytes());
        entry[32..36].copy_from_slice(&cpuid.ecx.to_le_bytes());
        entry[36..40].copy_from_slice(&cpuid.edx.to_le_bytes());
    }

    Ok(())
}

#[cfg(guest_arch = "x86_64")]
fn sanitize_snp_cpuid_entry(entry: &mut kvm::kvm_cpuid_entry2) {
    match (entry.function, entry.index) {
        // SNP firmware validates the CPUID page against hardware-supported
        // CPUID values, not KVM's synthetic guest CPUID additions.
        (0x1, _) => entry.ecx &= !0x01000000,
        (0x7, 0) => {
            entry.ebx &= !0x2;
            entry.edx = 0;
        }
        (0x80000008, _) => entry.ebx &= !0x02000000,
        (0x80000021, _) => {
            entry.eax &= !0x200;
            entry.ecx = 0;
        }
        _ => {}
    }
}

pub(crate) fn validate_snp_launch_range(range: MemoryRange) -> Result<(), KvmError> {
    if !is_page_aligned(std::ptr::null_mut(), range.start(), range.len()) {
        return Err(KvmError::UnalignedSnpLaunchRange);
    }
    Ok(())
}

pub(crate) fn private_memory_range_from_slots(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<KvmPrivateMemoryRange, KvmError> {
    validate_snp_launch_range(range)?;
    let slot = slots
        .iter()
        .flatten()
        .find(|slot| slot.range.contains(&range))
        .ok_or(KvmError::InvalidSnpLaunchRange)?;

    if slot.guest_memfd.is_none() || !slot.private_attributes_set {
        return Err(KvmError::InvalidSnpLaunchRange);
    }

    let offset = range.start() - slot.range.start();
    Ok(KvmPrivateMemoryRange {
        gpa: range,
        hva: slot.host_addr.wrapping_add(offset as usize),
    })
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
        KvmError::UnalignedSnpLaunchRange => KvmError::UnalignedCcaPopulateRange,
        KvmError::InvalidSnpLaunchRange => KvmError::InvalidCcaPopulateRange,
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

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum SnpPageRunKind {
    Zero,
    NonZero,
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct SnpPageRun {
    byte_offset: usize,
    byte_len: usize,
    kind: SnpPageRunKind,
}

#[cfg(guest_arch = "x86_64")]
pub(crate) fn split_zero_page_runs(bytes: &[u8]) -> Result<Vec<SnpPageRun>, KvmError> {
    const PAGE_SIZE: usize = hvdef::HV_PAGE_SIZE as usize;
    if !bytes.len().is_multiple_of(PAGE_SIZE) {
        return Err(KvmError::UnalignedSnpLaunchRange);
    }

    let mut runs: Vec<SnpPageRun> = Vec::new();
    for (page_index, page) in bytes.chunks_exact(PAGE_SIZE).enumerate() {
        let kind = if page.iter().all(|&byte| byte == 0) {
            SnpPageRunKind::Zero
        } else {
            SnpPageRunKind::NonZero
        };
        if let Some(run) = runs.last_mut()
            && run.kind == kind
        {
            run.byte_len += PAGE_SIZE;
            continue;
        }
        runs.push(SnpPageRun {
            byte_offset: page_index * PAGE_SIZE,
            byte_len: PAGE_SIZE,
            kind,
        });
    }
    Ok(runs)
}

fn is_page_aligned(data: *mut u8, addr: u64, size: u64) -> bool {
    const PAGE_SIZE: u64 = 4096;
    (data as usize as u64 | addr | size) & (PAGE_SIZE - 1) == 0
}

fn classify_guest_memfd_backing(
    range: MemoryRange,
    ram_ranges: &[MemoryRange],
) -> Result<KvmMemoryBacking, KvmError> {
    let containing_ranges = ram_ranges
        .iter()
        .filter(|ram_range| ram_range.contains(&range))
        .count();
    if containing_ranges == 1 {
        return Ok(KvmMemoryBacking::GuestMemfd);
    } else if containing_ranges > 1 {
        return Err(KvmError::UnsupportedIsolationConfiguration(
            "SNP guest_memfd mappings must be contained in exactly one RAM range",
        ));
    }

    if ram_ranges
        .iter()
        .any(|ram_range| ram_range.overlaps(&range))
    {
        return Err(KvmError::UnsupportedIsolationConfiguration(
            "SNP guest_memfd mappings must be fully contained in one RAM range",
        ));
    }

    Ok(KvmMemoryBacking::Userspace)
}

impl virt::PartitionMemoryMapper for KvmPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0);
        self.inner.clone()
    }
}

// TODO: figure out a better abstraction that works for both KVM and WHP.
impl virt::PartitionMemoryMap for KvmPartitionInner {
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        _exec: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: guaranteed by caller.
        unsafe { self.map_region(data, size, addr, !writable) }
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size);
        let mut state = self.memory.lock();
        for (slot, entry) in state.ranges.iter_mut().enumerate() {
            let Some(kvm_range) = entry else { continue };
            if range.contains(&kvm_range.range) {
                let guest_memfd_backed = kvm_range.guest_memfd.is_some();
                if kvm_range.private_attributes_set {
                    self.kvm.set_memory_attributes(
                        kvm_range.range.start(),
                        kvm_range.range.len(),
                        0,
                    )?;
                }
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                unsafe {
                    if guest_memfd_backed {
                        self.kvm.set_user_memory_region2(
                            slot as u32,
                            std::ptr::null_mut(),
                            0,
                            0,
                            false,
                            None,
                        )?;
                    } else {
                        self.kvm.set_user_memory_region(
                            slot as u32,
                            std::ptr::null_mut(),
                            0,
                            0,
                            false,
                        )?;
                    }
                }
                *entry = None;
            } else {
                assert!(
                    !range.overlaps(&kvm_range.range),
                    "can only unmap existing ranges of exact size"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> MemoryRange {
        MemoryRange::new(start..end)
    }

    fn dummy_file() -> File {
        File::open("/dev/null").unwrap()
    }

    #[test]
    fn guest_memfd_classifier_selects_contained_ram() {
        let ram_ranges = [range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)];

        assert_eq!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges).unwrap(),
            KvmMemoryBacking::GuestMemfd
        );
    }

    #[test]
    fn guest_memfd_classifier_keeps_non_ram_userspace() {
        let ram_ranges = [range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)];

        assert_eq!(
            classify_guest_memfd_backing(range(0xa000, 0xc000), &ram_ranges).unwrap(),
            KvmMemoryBacking::Userspace
        );
    }

    #[test]
    fn guest_memfd_classifier_rejects_partial_ram_overlap() {
        let ram_ranges = [range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)];

        assert!(matches!(
            classify_guest_memfd_backing(range(0x8000, 0xa000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_does_not_merge_adjacent_ram_ranges() {
        let ram_ranges = [range(0x1000, 0x3000), range(0x3000, 0x5000)];

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_rejects_ambiguous_ram_containment() {
        let ram_ranges = [range(0x1000, 0x5000), range(0x2000, 0x4000)];

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn private_memory_range_resolves_hva_offset() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd: Some(dummy_file()),
            private_attributes_set: true,
        })];

        let resolved = private_memory_range_from_slots(range(0x3000, 0x5000), &slots).unwrap();

        assert_eq!(resolved.gpa, range(0x3000, 0x5000));
        assert_eq!(resolved.hva, host_addr.wrapping_add(0x2000));
    }

    #[test]
    fn private_memory_range_rejects_non_private_or_non_guest_memfd_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let userspace_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd: None,
            private_attributes_set: true,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &userspace_slots),
            Err(KvmError::InvalidSnpLaunchRange)
        ));

        let shared_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd: Some(dummy_file()),
            private_attributes_set: false,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &shared_slots),
            Err(KvmError::InvalidSnpLaunchRange)
        ));
    }

    #[test]
    fn snp_launch_pages_with_ram_hack_fills_unaccepted_ram_gaps() {
        let pages = [virt::InitialAcceptedPage {
            range: range(0x2000, 0x4000),
            visibility: virt::PageVisibility::Exclusive,
            acceptance: BootPageAcceptance::Exclusive,
            tag: "loader".into(),
        }];
        let ram_ranges = [range(0x1000, 0x5000), range(0x8000, 0xa000)];

        let launch_pages = snp_launch_pages_with_ram_hack(&pages, &ram_ranges);
        let hack_ranges: Vec<_> = launch_pages
            .iter()
            .filter(|page| page.tag == "kvm-snp-ram-hack")
            .map(|page| page.range)
            .collect();

        assert_eq!(
            hack_ranges,
            vec![
                range(0x1000, 0x2000),
                range(0x4000, 0x5000),
                range(0x8000, 0xa000)
            ]
        );
    }

    #[test]
    fn split_zero_page_runs_coalesces_adjacent_pages_by_zero_state() {
        let mut bytes = vec![0u8; 5 * hvdef::HV_PAGE_SIZE as usize];
        bytes[hvdef::HV_PAGE_SIZE as usize] = 1;
        bytes[2 * hvdef::HV_PAGE_SIZE as usize] = 2;
        bytes[4 * hvdef::HV_PAGE_SIZE as usize] = 3;

        let runs = split_zero_page_runs(&bytes).unwrap();

        assert_eq!(
            runs,
            vec![
                SnpPageRun {
                    byte_offset: 0,
                    byte_len: hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::Zero,
                },
                SnpPageRun {
                    byte_offset: hvdef::HV_PAGE_SIZE as usize,
                    byte_len: 2 * hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::NonZero,
                },
                SnpPageRun {
                    byte_offset: 3 * hvdef::HV_PAGE_SIZE as usize,
                    byte_len: hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::Zero,
                },
                SnpPageRun {
                    byte_offset: 4 * hvdef::HV_PAGE_SIZE as usize,
                    byte_len: hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::NonZero,
                },
            ]
        );
    }

    #[test]
    fn split_zero_page_runs_rejects_partial_pages() {
        assert!(matches!(
            split_zero_page_runs(&[0; 17]),
            Err(KvmError::UnalignedSnpLaunchRange)
        ));
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn snp_c_bit_from_cpuid_reads_memory_encryption_bit() {
        let cpuid = [kvm::kvm_cpuid_entry2 {
            function: 0x8000_001f,
            index: 0,
            ebx: 51,
            ..Default::default()
        }];

        assert_eq!(snp_c_bit_from_cpuid(&cpuid).unwrap(), 51);
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn set_snp_c_bit_in_page_tables_updates_present_entries() {
        let mut entries = [0x1000u64 | 1, 0, 0x2000u64 | 1 << 1, 0x3000u64 | 1];

        set_snp_c_bit_in_page_tables(
            entries.as_mut_ptr().cast::<u8>(),
            size_of_val(&entries) as u64,
            51,
        )
        .unwrap();

        assert_eq!(entries[0], (1u64 << 51) | 0x1001);
        assert_eq!(entries[1], 0);
        assert_eq!(entries[2], 0x2002);
        assert_eq!(entries[3], (1u64 << 51) | 0x3001);
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn write_snp_cpuid_page_writes_linux_table_and_xsave_inputs() {
        let mut page = vec![0xff; hvdef::HV_PAGE_SIZE as usize];
        let cpuid = [
            kvm::kvm_cpuid_entry2 {
                function: 1,
                index: 0,
                eax: 0x11,
                ebx: 0x12,
                ecx: 0x13,
                edx: 0x14,
                ..Default::default()
            },
            kvm::kvm_cpuid_entry2 {
                function: x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0,
                index: 0,
                eax: 0x21,
                ebx: 0x22,
                ecx: 0x23,
                edx: 0x24,
                ..Default::default()
            },
        ];

        write_snp_cpuid_page(page.as_mut_ptr(), page.len() as u64, &cpuid).unwrap();

        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(page[16..20].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(page[40..44].try_into().unwrap()), 0x11);
        assert_eq!(u32::from_le_bytes(page[44..48].try_into().unwrap()), 0x12);
        let xsave = 16 + 48;
        assert_eq!(
            u32::from_le_bytes(page[xsave..xsave + 4].try_into().unwrap()),
            x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
        );
        assert_eq!(
            u64::from_le_bytes(page[xsave + 8..xsave + 16].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_le_bytes(page[xsave + 28..xsave + 32].try_into().unwrap()),
            0x240
        );
    }
}
