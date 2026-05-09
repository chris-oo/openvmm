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
use std::os::fd::AsFd;
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
    #[error("SNP launch is already in progress")]
    SnpLaunchInProgress,
    #[error("SNP launch previously failed")]
    SnpLaunchFailed,
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

#[cfg(guest_arch = "x86_64")]
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
    #[cfg(guest_arch = "aarch64")]
    #[inspect(skip)]
    gic_v2m: Option<vm_topology::processor::aarch64::GicV2mInfo>,
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

#[cfg(guest_arch = "x86_64")]
impl virt::AcceptInitialPages for KvmPartition {
    type Error = KvmError;

    fn accept_initial_pages(&self, pages: &[virt::InitialAcceptedPage]) -> Result<(), Self::Error> {
        self.inner.snp_launch_initial_pages(pages)
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
    #[cfg_attr(guest_arch = "x86_64", expect(dead_code))]
    #[error("unhandled system event type: {0:#x}")]
    UnhandledSystemEvent(u32),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to inject an extint interrupt")]
    ExtintInterrupt(#[source] kvm::Error),
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

        match self.snp_launch_initial_pages_inner(pages) {
            Ok(()) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Finished;
                Ok(())
            }
            Err(err) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Failed;
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
        self.kvm
            .sev_snp_launch_start(sev.as_fd(), &mut Default::default())?;

        let memory = self.memory.lock();
        for page in pages {
            if page.visibility == virt::PageVisibility::Shared {
                continue;
            }

            let launch_page_type = arch::snp::snp_launch_page_type(page.acceptance)?;
            let Some(kvm_page_type) = launch_page_type.kvm_page_type() else {
                return Err(KvmError::UnsupportedSnpPageAcceptance(page.acceptance));
            };

            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)?;
            if page.acceptance == BootPageAcceptance::CpuidPage {
                write_snp_cpuid_page(private_range.hva, page.range.len(), &self.bsp_cpuid)?;
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
                self.kvm.sev_snp_launch_update(
                    sev.as_fd(),
                    gpa / hvdef::HV_PAGE_SIZE,
                    uaddr,
                    run.byte_len as u64,
                    page_type,
                )?;
            }
        }
        self.kvm.sev_launch_update_vmsa(sev.as_fd())?;
        self.kvm
            .sev_snp_launch_finish(sev.as_fd(), &mut Default::default())?;
        Ok(())
    }
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

    for (index, cpuid) in cpuid.iter().enumerate() {
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
pub(crate) fn validate_snp_launch_range(range: MemoryRange) -> Result<(), KvmError> {
    if !is_page_aligned(std::ptr::null_mut(), range.start(), range.len()) {
        return Err(KvmError::UnalignedSnpLaunchRange);
    }
    Ok(())
}

#[cfg(guest_arch = "x86_64")]
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
