// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM memory-slot and confidential guest backing management.
//!
//! Confidential RAM slots use userspace memory for shared access and a
//! guestmemfd for private access. This module records both sides of each slot,
//! selects the appropriate backing when a range is mapped, validates private
//! launch ranges, and discards stale contents when ownership changes.

#[cfg(guest_arch = "aarch64")]
use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
#[cfg(guest_arch = "aarch64")]
use crate::cca::map_cca_conversion_error;
use inspect::Inspect;
use memory_range::MemoryRange;
use std::fs::File;
#[cfg(guest_arch = "aarch64")]
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("kvm memory operation failed")]
    Kvm(#[from] kvm::Error),
    #[error("cannot resize KVM guest_memfd memory slot")]
    CannotResizeGuestMemfdSlot,
    #[error("private memory range is not contained in guest_memfd private memory")]
    InvalidPrivateMemoryRange,
    #[error("invalid KVM_HC_MAP_GPA_RANGE request")]
    InvalidMapGpaRange,
    #[error("unsupported KVM_HC_MAP_GPA_RANGE attributes: {0:#x}")]
    UnsupportedMapGpaRangeAttributes(u64),
    #[error("failed to discard shared backing after private conversion")]
    DiscardSharedBacking(#[source] std::io::Error),
    #[error("failed to discard private backing after shared conversion")]
    DiscardPrivateBacking(#[source] std::io::Error),
    #[error("unsupported isolation configuration: {0}")]
    UnsupportedIsolationConfiguration(&'static str),
    #[cfg(any(guest_arch = "aarch64", test))]
    #[error("RAM layout differs from the prepared KVM backing")]
    PreparedRamLayoutChanged,
    #[cfg(guest_arch = "aarch64")]
    #[error("invalid partition-supplied RAM backing")]
    MappableRam(#[from] virt::RamBackingError),
    #[cfg(guest_arch = "aarch64")]
    #[error("failed to clone guestmemfd")]
    CloneGuestMemfd(#[source] std::io::Error),
    #[cfg(any(guest_arch = "aarch64", test))]
    #[error(
        "assignment RAM mappings are frozen; retain the VM memory owner until DMA is contained"
    )]
    AssignmentRamFrozen,
    #[cfg(any(guest_arch = "aarch64", test))]
    #[error("CCA assignment DMA mapping failed")]
    AssignmentDma(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[cfg(any(guest_arch = "aarch64", test))]
#[derive(Debug, Error)]
pub(crate) enum SharedBufferError {
    #[error("native evidence requires in-place CCA backing")]
    Unsupported,
    #[error("CCA partition is marked fatal")]
    Fatal,
    #[error("invalid shared guest buffer: address={gpa:#x}, length={length}")]
    InvalidRange { gpa: u64, length: usize },
    #[error("shared buffer is not fully covered by current guestmemfd slots")]
    Slots(#[source] MemoryError),
    #[error("shared buffer visibility check failed")]
    Visibility(#[source] crate::cca_in_place::CcaInPlaceError),
    #[error("shared guest buffer copy failed; a prefix may have been copied")]
    Copy(#[source] guestmem::GuestMemoryError),
}

#[cfg(any(guest_arch = "aarch64", test))]
fn shared_buffer_pages(
    gpa: u64,
    length: usize,
    shared_bit: u64,
) -> Result<MemoryRange, SharedBufferError> {
    let invalid = || SharedBufferError::InvalidRange { gpa, length };
    let end = gpa.checked_add(length as u64).ok_or_else(invalid)?;
    if length == 0
        || shared_bit < 4096
        || !shared_bit.is_power_of_two()
        || gpa >= shared_bit
        || end > shared_bit
    {
        return Err(invalid());
    }
    let page_end = end.checked_add(4095).ok_or_else(invalid)? & !4095;
    MemoryRange::try_new((gpa & !4095)..page_end).map_err(|_| invalid())
}

#[cfg(any(guest_arch = "aarch64", test))]
fn with_shared_buffer(
    memory: &parking_lot::Mutex<KvmMemoryRangeState>,
    fatal: &std::sync::atomic::AtomicBool,
    gpa: u64,
    length: usize,
    shared_bit: u64,
    copy: impl FnOnce() -> Result<(), guestmem::GuestMemoryError>,
) -> Result<(), SharedBufferError> {
    let pages = shared_buffer_pages(gpa, length, shared_bit)?;
    let state = memory.lock();
    if fatal.load(std::sync::atomic::Ordering::Acquire) {
        return Err(SharedBufferError::Fatal);
    }
    guest_memfd_range_segments(pages, &state.ranges).map_err(SharedBufferError::Slots)?;
    if state.ranges.iter().flatten().any(|slot| {
        slot.range.overlaps(&pages)
            && slot.private_state != Some(KvmGuestMemfdPrivateState::InPlace)
    }) {
        return Err(SharedBufferError::Unsupported);
    }
    state
        .cca_visibility
        .require_shared(pages)
        .map_err(SharedBufferError::Visibility)?;
    // The same lock guards conversion and slot removal. Keep it until the
    // fault-safe copy finishes; never rely on a selector bit as sharing proof.
    let result = copy().map_err(SharedBufferError::Copy);
    drop(state);
    result
}

#[derive(Debug, Inspect)]
/// A registered KVM memory slot and its confidential-memory metadata.
pub(crate) struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
    guest_memfd_offset: Option<u64>,
    private_state: Option<KvmGuestMemfdPrivateState>,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
/// Slot-indexed memory mappings currently registered with KVM.
pub(crate) struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    pub(crate) ranges: Vec<Option<KvmMemoryRange>>,
    #[cfg(any(guest_arch = "aarch64", test))]
    #[inspect(skip)]
    pub(crate) cca_visibility: crate::cca_in_place::Visibility,
    #[cfg(any(guest_arch = "aarch64", test))]
    #[inspect(skip)]
    assignment_prepared: bool,
    #[cfg(any(guest_arch = "aarch64", test))]
    #[inspect(skip)]
    assignment_frozen: bool,
    #[cfg(any(guest_arch = "aarch64", test))]
    #[inspect(skip)]
    pub(crate) protected_attempts: crate::cca_in_place::ProtectedAttempts,
}

#[cfg(guest_arch = "aarch64")]
#[derive(Clone, Copy)]
enum AssignmentAction {
    Prepare,
    Fault {
        vpindex: u32,
        fault: crate::cca_in_place::Fault,
        successful_exit: bool,
    },
}

#[cfg(guest_arch = "aarch64")]
struct AssignmentWork {
    partition: Arc<KvmPartitionInner>,
    action: AssignmentAction,
}

#[cfg(guest_arch = "aarch64")]
impl tdisp::host::RamWork for AssignmentWork {
    fn run(
        &self,
        operations: &mut dyn tdisp::host::SharedDma,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let result = match self.action {
            AssignmentAction::Prepare => self.partition.prepare_assignment_ram(operations),
            AssignmentAction::Fault {
                vpindex,
                fault,
                successful_exit,
            } => self
                .partition
                .vps
                .get(vpindex as usize)
                .ok_or(KvmError::InvalidState("invalid assignment VP"))
                .and_then(|vp| {
                    self.partition.handle_cca_in_place_memory_fault_inner(
                        &mut vp.cca_fault.lock(),
                        fault,
                        successful_exit,
                        Some((vpindex, operations)),
                    )
                }),
        };
        if result.is_err() {
            self.partition.mark_cca_fatal();
        }
        result.map_err(Into::into)
    }
}

impl KvmMemoryRangeState {
    #[cfg(any(guest_arch = "aarch64", test))]
    pub(crate) fn in_place_ram_slots(&self) -> Vec<MemoryRange> {
        self.ranges
            .iter()
            .flatten()
            .filter_map(|slot| {
                (slot.private_state == Some(KvmGuestMemfdPrivateState::InPlace)
                    && slot.guest_memfd_offset.is_some())
                .then_some(slot.range)
            })
            .collect()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
/// A private guest range paired with the userspace source used for launch.
pub(crate) struct KvmPrivateMemoryRange {
    /// Guest-physical range covered by the private slot.
    pub(crate) gpa: MemoryRange,
    /// Userspace source address corresponding to the start of `gpa`.
    pub(crate) hva: *mut u8,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct KvmMemoryRangeSegment {
    range: MemoryRange,
    host_addr: *mut u8,
    guest_memfd_offset: u64,
}

#[derive(Debug, Inspect)]
#[inspect(external_tag)]
/// Backing strategy for partition memory slots.
pub(crate) enum KvmMemoryBackingMode {
    /// Register only the caller-provided userspace mapping.
    Userspace,
    /// Register shared userspace and private guestmemfd backing for RAM.
    GuestMemfd(KvmGuestMemfdBacking),
}

/// Retains Arm backing between prototype preparation and partition creation.
#[cfg(any(guest_arch = "aarch64", test))]
#[derive(Default)]
pub(crate) struct KvmPreparedRamBacking {
    backing: Option<(Vec<MemoryRange>, KvmMemoryBackingMode)>,
}

#[cfg(any(guest_arch = "aarch64", test))]
impl KvmPreparedRamBacking {
    /// Repeated preparation is idempotent only for the same RAM ranges.
    /// A failed allocation leaves the prototype unprepared and can be retried.
    pub(crate) fn prepare(
        &mut self,
        ranges: Vec<MemoryRange>,
        create: impl FnOnce(&[MemoryRange]) -> Result<KvmMemoryBackingMode, MemoryError>,
    ) -> Result<&mut KvmMemoryBackingMode, MemoryError> {
        if self.backing.is_none() {
            let mode = create(&ranges)?;
            self.backing = Some((ranges.clone(), mode));
        }
        let (prepared_ranges, mode) = self.backing.as_mut().expect("backing was prepared above");
        if *prepared_ranges != ranges {
            return Err(MemoryError::PreparedRamLayoutChanged);
        }
        Ok(mode)
    }
}

#[derive(Debug, Inspect)]
/// Partition-owned guestmemfd and its packed mapping of guest RAM ranges.
pub(crate) struct KvmGuestMemfdBacking {
    #[inspect(skip)]
    file: File,
    #[inspect(iter_by_index)]
    ranges: Vec<KvmGuestMemfdRange>,
    private_state: KvmGuestMemfdPrivateState,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
pub(crate) enum KvmGuestMemfdPrivateState {
    #[cfg(guest_arch = "x86_64")]
    VmAttributes,
    #[cfg(guest_arch = "aarch64")]
    GuestMemfdDefault,
    #[cfg(any(guest_arch = "aarch64", test))]
    InPlace,
}

impl KvmGuestMemfdPrivateState {
    fn uses_vm_attributes(self) -> bool {
        #[cfg(guest_arch = "x86_64")]
        {
            matches!(self, Self::VmAttributes)
        }
        #[cfg(guest_arch = "aarch64")]
        {
            false
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
struct KvmGuestMemfdRange {
    range: MemoryRange,
    file_offset: u64,
}

#[derive(Debug)]
enum KvmMemoryBacking<'a> {
    Userspace,
    GuestMemfd {
        file: &'a File,
        file_offset: u64,
        private_state: KvmGuestMemfdPrivateState,
    },
}

impl KvmMemoryBackingMode {
    /// Creates one guestmemfd spanning the supplied RAM ranges.
    ///
    /// Guest ranges are packed contiguously into the file in iteration order.
    /// `private_state` describes how private state is established.
    pub(crate) fn guest_memfd(
        kvm: &kvm::Partition,
        ram_ranges: impl IntoIterator<Item = MemoryRange>,
        private_state: KvmGuestMemfdPrivateState,
    ) -> Result<Self, MemoryError> {
        check_private_memory_extensions(kvm, private_state)?;

        let mut file_size = 0u64;
        let mut ranges = Vec::new();
        for range in ram_ranges {
            ranges.push(KvmGuestMemfdRange {
                range,
                file_offset: file_size,
            });
            file_size = file_size.checked_add(range.len()).ok_or(
                MemoryError::UnsupportedIsolationConfiguration("guestmemfd size overflow"),
            )?;
        }

        Ok(Self::GuestMemfd(KvmGuestMemfdBacking {
            file: kvm.create_guest_memfd_with_flags(file_size, private_state.create_flags())?,
            ranges,
            private_state,
        }))
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn mappable_ram_backing(
        &self,
        layout: &vm_topology::memory::MemoryLayout,
    ) -> Result<Option<virt::MappableRamBacking>, MemoryError> {
        match self {
            Self::GuestMemfd(backing)
                if backing.private_state == KvmGuestMemfdPrivateState::InPlace =>
            {
                Ok(Some(virt::MappableRamBacking::new(
                    backing
                        .file
                        .try_clone()
                        .map_err(MemoryError::CloneGuestMemfd)?,
                    layout,
                )?))
            }
            _ => Ok(None),
        }
    }

    #[cfg(any(guest_arch = "aarch64", test))]
    pub(crate) fn is_in_place(&self) -> bool {
        matches!(self, Self::GuestMemfd(backing) if backing.private_state == KvmGuestMemfdPrivateState::InPlace)
    }
}

impl KvmGuestMemfdPrivateState {
    fn create_flags(self) -> u64 {
        #[cfg(any(guest_arch = "aarch64", test))]
        if self == Self::InPlace {
            return kvm::GUEST_MEMFD_FLAG_MMAP_UAPI | kvm::GUEST_MEMFD_FLAG_INIT_SHARED_UAPI;
        }
        0
    }
}

impl KvmPartitionInner {
    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn record_protected_attempt(
        &self,
        request: crate::rhi::MappingRequest,
    ) -> Result<(), KvmError> {
        let mut state = self.memory.lock();
        if self.cca_fatal.load(std::sync::atomic::Ordering::Acquire)
            || !state.assignment_prepared
            || self
                .ram_ranges
                .iter()
                .any(|range| range.overlaps(&request.range))
            || state
                .ranges
                .iter()
                .flatten()
                .any(|slot| slot.range.overlaps(&request.range))
        {
            return Err(crate::cca_in_place::CcaInPlaceError::InvalidProtectedMapping.into());
        }
        state.protected_attempts.record(
            request.range,
            request.pa,
            self.shared_gpa_bit.ok_or(KvmError::InvalidCcaMemoryFault)?,
        )?;
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    async fn assignment_memory_work(
        self: &Arc<Self>,
        action: AssignmentAction,
    ) -> Result<(), KvmError> {
        let guard = crate::rhi::RequestGuard::new(|| self.mark_cca_fatal());
        let service = self
            .cca_assignment_service
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or(tdisp::host::EvidenceError::Closed)?;
        service
            .assignment(tdisp::host::AssignmentOperation::ConvertRam(Arc::new(
                AssignmentWork {
                    partition: self.clone(),
                    action,
                },
            )))
            .await?;
        if self.cca_fatal.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::cca_in_place::CcaInPlaceError::AmbiguousFault.into());
        }
        guard.complete(Ok(()))?;
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) async fn prepare_assignment_memory(self: &Arc<Self>) -> Result<(), KvmError> {
        if self.cca_assignment_service.get().is_none() {
            return Ok(());
        }
        self.assignment_memory_work(AssignmentAction::Prepare)
            .await?;
        self.rhi.lock().assignment_prepared();
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    fn prepare_assignment_ram(
        &self,
        operations: &mut dyn tdisp::host::SharedDma,
    ) -> Result<(), KvmError> {
        if *self.cca_launch_state.lock() != crate::CcaLaunchState::Populated
            || !self.memory_backing_mode.is_in_place()
            || self.vps.is_empty()
        {
            return Err(KvmError::InvalidState(
                "assignment requires populated in-place RAM and a VP",
            ));
        }
        let mut state = self.memory.lock();
        if self.cca_fatal.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::cca_in_place::CcaInPlaceError::AmbiguousFault.into());
        }
        if state.assignment_prepared {
            return Ok(());
        }
        for range in &self.ram_ranges {
            guest_memfd_range_segments(*range, &state.ranges)?;
        }
        if state.in_place_ram_slots().is_empty() {
            return Err(KvmError::InvalidState("assignment has no RAM slots"));
        }
        let slots = state.in_place_ram_slots();
        for &range in &slots {
            if !state.cca_visibility.changes(range, true)?.is_empty() {
                return Err(KvmError::InvalidState(
                    "assignment RAM is not private after import",
                ));
            }
        }
        state.assignment_frozen = true;
        if let Err(error) = prepare_private_assignment(&slots, operations, |range| {
            self.kvm
                .vp(0)
                .pre_fault_memory_all(range.start(), range.len())
        }) {
            self.mark_cca_fatal();
            return Err(error.into());
        }
        state.assignment_prepared = true;
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) async fn handle_cca_assignment_fault(
        self: &Arc<Self>,
        vpindex: u32,
        fault: crate::cca_in_place::Fault,
        successful_exit: bool,
    ) -> Result<(), KvmError> {
        self.assignment_memory_work(AssignmentAction::Fault {
            vpindex,
            fault,
            successful_exit,
        })
        .await
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn read_rhi_shared(
        &self,
        gpa: u64,
        data: &mut [u8],
    ) -> Result<(), SharedBufferError> {
        if !self.memory_backing_mode.is_in_place()
            || self.caps.isolation != virt::IsolationType::Cca
        {
            return Err(SharedBufferError::Unsupported);
        }
        let shared_bit = self.shared_gpa_bit.ok_or(SharedBufferError::Unsupported)?;
        with_shared_buffer(
            &self.memory,
            &self.cca_fatal,
            gpa,
            data.len(),
            shared_bit,
            || self.gm.read_at(gpa, data),
        )
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn write_rhi_shared(&self, gpa: u64, data: &[u8]) -> Result<(), SharedBufferError> {
        if !self.memory_backing_mode.is_in_place()
            || self.caps.isolation != virt::IsolationType::Cca
        {
            return Err(SharedBufferError::Unsupported);
        }
        let shared_bit = self.shared_gpa_bit.ok_or(SharedBufferError::Unsupported)?;
        with_shared_buffer(
            &self.memory,
            &self.cca_fatal,
            gpa,
            data.len(),
            shared_bit,
            || self.gm.write_at(gpa, data),
        )
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn handle_cca_in_place_memory_fault(
        &self,
        tracker: &mut crate::cca_in_place::FaultTracker,
        fault: crate::cca_in_place::Fault,
        successful_exit: bool,
    ) -> Result<(), KvmError> {
        self.handle_cca_in_place_memory_fault_inner(tracker, fault, successful_exit, None)
    }

    #[cfg(guest_arch = "aarch64")]
    fn handle_cca_in_place_memory_fault_inner(
        &self,
        tracker: &mut crate::cca_in_place::FaultTracker,
        fault: crate::cca_in_place::Fault,
        successful_exit: bool,
        mut assignment: Option<(u32, &mut dyn tdisp::host::SharedDma)>,
    ) -> Result<(), KvmError> {
        let mut state = self.memory.lock();
        let result = (|| {
            if self.cca_fatal.load(std::sync::atomic::Ordering::Acquire) {
                return Err(crate::cca_in_place::CcaInPlaceError::AmbiguousFault.into());
            }
            let range = crate::cca_in_place::checked_range(fault.gpa, fault.size)?;
            // Validate slot coverage even for probes and no-op requests.
            if guest_memfd_range_segments(range, &state.ranges).is_err()
                && assignment.is_some()
                && state.protected_attempts.covers(range)
            {
                // Re-entry may let KVM handle DEV -> EMPTY. This is not a
                // completion acknowledgment: never remove the attempted range.
                tracker.observe_device(fault, successful_exit)?;
                return Ok(());
            }
            guest_memfd_range_segments(range, &state.ranges)?;
            let action = tracker.observe(fault, successful_exit, &state.cca_visibility)?;
            if action == crate::cca_in_place::FaultAction::Convert {
                let private = fault.flags != 0;
                let changes = state.cca_visibility.changes(range, private)?;
                for change in changes {
                    let segments = guest_memfd_range_segments(change, &state.ranges)?;
                    if let Some((vpindex, operations)) = assignment.as_mut() {
                        let KvmMemoryBackingMode::GuestMemfd(backing) = &self.memory_backing_mode
                        else {
                            return Err(KvmError::InvalidCcaMemoryFault);
                        };
                        let file = Arc::new(
                            backing
                                .file
                                .try_clone()
                                .map_err(MemoryError::CloneGuestMemfd)?,
                        );
                        let shared_bit =
                            self.shared_gpa_bit.ok_or(KvmError::InvalidCcaMemoryFault)?;
                        convert_assignment_segments(
                            &segments,
                            private,
                            shared_bit,
                            file,
                            *operations,
                            |attributes| {
                                kvm::set_guest_memfd_memory_attributes(
                                    backing.file.as_fd(),
                                    attributes,
                                )
                            },
                            |range| {
                                self.kvm
                                    .vp(*vpindex)
                                    .pre_fault_memory_all(range.start(), range.len())
                            },
                        )?;
                    } else {
                        self.convert_cca_segments(&segments, private)?;
                    }
                    state.cca_visibility.record(change, private);
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // Conversion can fail after changing part of a segment. Poison
            // before releasing the memory lock; never retry from the ledger.
            self.mark_cca_fatal();
        }
        result
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
        let mut state = self.memory.lock();
        #[cfg(guest_arch = "aarch64")]
        if state.assignment_frozen
            && (self.ram_ranges.iter().any(|ram| ram.overlaps(&range))
                || state
                    .ranges
                    .iter()
                    .flatten()
                    .any(|slot| slot.host_addr == data && slot.guest_memfd_offset.is_some()))
        {
            self.mark_cca_fatal();
            return Err(MemoryError::AssignmentRamFrozen.into());
        }

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
            if existing_range.guest_memfd_offset.is_some()
                && existing_range.range.len() != size as u64
            {
                return Err(MemoryError::CannotResizeGuestMemfdSlot.into());
            }
            if existing_range
                .private_state
                .is_some_and(KvmGuestMemfdPrivateState::uses_vm_attributes)
            {
                self.kvm.set_memory_attributes(
                    existing_range.range.start(),
                    existing_range.range.len(),
                    0,
                )?;
            }
            #[cfg(guest_arch = "aarch64")]
            if existing_range.private_state == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault) {
                let guest_memfd_offset = existing_range
                    .guest_memfd_offset
                    .ok_or(MemoryError::InvalidPrivateMemoryRange)?;
                if let Err(err) = self.discard_stale_private_memory_backing(
                    &[KvmMemoryRangeSegment {
                        range: existing_range.range,
                        host_addr: existing_range.host_addr,
                        guest_memfd_offset,
                    }],
                    false,
                    "CCA slot replacement",
                ) {
                    self.mark_cca_fatal();
                    return Err(err.into());
                }
            }
            if existing_range.guest_memfd_offset.is_some() {
                // SAFETY: clearing a slot removes the memory reference.
                if let Err(err) = unsafe { self.clear_slot(slot_to_use, true) } {
                    #[cfg(guest_arch = "aarch64")]
                    if existing_range.private_state
                        == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault)
                    {
                        self.mark_cca_fatal();
                    }
                    return Err(err.into());
                }
                state.ranges[slot_to_use] = None;
            }
        }
        let (guest_memfd_offset, private_state) = match backing {
            KvmMemoryBacking::Userspace => {
                // SAFETY: `map_region` requires its caller to keep
                // `data..data+size` valid until this guest-physical range is
                // unmapped or the partition is destroyed.
                unsafe {
                    self.kvm.set_user_memory_region(
                        slot_to_use as u32,
                        data,
                        size,
                        addr,
                        readonly,
                    )?
                };
                (None, None)
            }
            KvmMemoryBacking::GuestMemfd {
                file,
                file_offset,
                private_state,
            } => {
                // SAFETY: `map_region` requires its caller to keep
                // `data..data+size` valid until this guest-physical range is
                // unmapped or the partition is destroyed. The partition owns the
                // backing guestmemfd for at least as long as KVM references it.
                unsafe {
                    self.kvm.set_user_memory_region2(
                        slot_to_use as u32,
                        data,
                        size,
                        addr,
                        readonly,
                        Some((file, file_offset)),
                    )?;
                };
                if private_state.uses_vm_attributes() {
                    if let Err(err) = self.kvm.set_memory_attributes(
                        addr,
                        size as u64,
                        kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64,
                    ) {
                        // SAFETY: clearing a slot removes the memory reference.
                        unsafe { self.clear_slot(slot_to_use, true)? };
                        state.ranges[slot_to_use] = None;
                        return Err(err.into());
                    }
                }
                (Some(file_offset), Some(private_state))
            }
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range,
            guest_memfd_offset,
            private_state,
        });
        Ok(())
    }

    fn memory_backing(&self, range: MemoryRange) -> Result<KvmMemoryBacking<'_>, MemoryError> {
        match &self.memory_backing_mode {
            KvmMemoryBackingMode::Userspace => Ok(KvmMemoryBacking::Userspace),
            KvmMemoryBackingMode::GuestMemfd(backing) => {
                match classify_guest_memfd_backing(range, &backing.ranges)? {
                    Some(file_offset) => Ok(KvmMemoryBacking::GuestMemfd {
                        file: &backing.file,
                        file_offset,
                        private_state: backing.private_state,
                    }),
                    None => Ok(KvmMemoryBacking::Userspace),
                }
            }
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that clearing the target slot is valid.
    unsafe fn clear_slot(&self, slot: usize, guest_memfd_backed: bool) -> Result<(), kvm::Error> {
        if guest_memfd_backed {
            // SAFETY: the caller ensures clearing this slot is valid.
            unsafe {
                self.kvm.set_user_memory_region2(
                    slot as u32,
                    std::ptr::null_mut(),
                    0,
                    0,
                    false,
                    None,
                )
            }
        } else {
            // SAFETY: the caller ensures clearing this slot is valid.
            unsafe {
                self.kvm
                    .set_user_memory_region(slot as u32, std::ptr::null_mut(), 0, 0, false)
            }
        }
    }

    /// Marks an IGVM-provided range shared before SNP launch.
    ///
    /// KVM private memory starts with private attributes. Shared IGVM page
    /// imports must clear those attributes and discard stale private backing
    /// before launch updates begin.
    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn set_initial_shared_memory(&self, range: MemoryRange) -> Result<(), MemoryError> {
        let segments = {
            let state = self.memory.lock();
            guest_memfd_range_segments(range, &state.ranges)?
        };
        self.kvm
            .set_memory_attributes(range.start(), range.len(), 0)?;
        self.discard_stale_private_memory_backing(&segments, false, "SNP")
    }

    /// Applies a guest-requested SNP shared/private state change.
    ///
    /// `page_count` is always expressed in 4-KiB pages by
    /// `KVM_HC_MAP_GPA_RANGE`. The page-size bits in `map_attributes` describe
    /// the guest's preferred processing granularity, but do not change the
    /// units of `page_count`.
    ///
    /// The range must be non-empty, page-aligned, continuously covered by
    /// guestmemfd-backed slots, and request either the encrypted or decrypted
    /// state. After updating KVM's private-memory attributes, the backing for
    /// the old state is discarded so stale data cannot be reused if the page
    /// later transitions back.
    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn set_map_gpa_range_attributes(
        &self,
        gpa: u64,
        page_count: u64,
        map_attributes: u64,
    ) -> Result<(), MemoryError> {
        const KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK: u64 = 0x3;
        const KVM_MAP_GPA_RANGE_ENC_STATUS_MASK: u64 = 0xf << 4;

        let size = page_count
            .checked_mul(hvdef::HV_PAGE_SIZE)
            .ok_or(MemoryError::InvalidMapGpaRange)?;
        let end = gpa
            .checked_add(size)
            .ok_or(MemoryError::InvalidMapGpaRange)?;
        if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE) || size == 0 {
            return Err(MemoryError::InvalidMapGpaRange);
        }
        let unsupported_attributes = map_attributes
            & !(KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK | KVM_MAP_GPA_RANGE_ENC_STATUS_MASK);
        if unsupported_attributes != 0 {
            return Err(MemoryError::UnsupportedMapGpaRangeAttributes(
                map_attributes,
            ));
        }
        let private = match map_attributes & KVM_MAP_GPA_RANGE_ENC_STATUS_MASK {
            kvm::KVM_MAP_GPA_RANGE_DECRYPTED_UAPI => false,
            kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI => true,
            _ => {
                return Err(MemoryError::UnsupportedMapGpaRangeAttributes(
                    map_attributes,
                ));
            }
        };

        let range = MemoryRange::new(gpa..end);
        let state = self.memory.lock();
        let segments = guest_memfd_range_segments(range, &state.ranges)?;

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
        self.discard_stale_private_memory_backing(&segments, private, "SNP")?;
        Ok(())
    }

    /// Discards data from the backing that is no longer selected by KVM.
    ///
    /// Guestmemfd memory slots have separate shared userspace and private
    /// guestmemfd backings. For a shared-to-private conversion, discard the
    /// shared backing with `MADV_REMOVE` (falling back to `MADV_DONTNEED` for
    /// anonymous mappings). For a private-to-shared conversion, punch a hole in
    /// guestmemfd so private data cannot become visible after a later conversion
    /// back to private.
    pub(crate) fn discard_stale_private_memory_backing(
        &self,
        segments: &[KvmMemoryRangeSegment],
        private: bool,
        isolation_name: &'static str,
    ) -> Result<(), MemoryError> {
        #[cfg(guest_arch = "aarch64")]
        if self.memory_backing_mode.is_in_place() {
            // There is only one backing. Removing either alias destroys data.
            return Ok(());
        }
        if private {
            for segment in segments {
                tracing::debug!(
                    gpa = segment.range.start(),
                    size = segment.range.len(),
                    hva = segment.host_addr as usize,
                    isolation_name,
                    "discarding shared backing after private conversion"
                );
                let mut ret = unsafe {
                    libc::madvise(
                        segment.host_addr.cast(),
                        segment.range.len() as usize,
                        libc::MADV_REMOVE,
                    )
                };
                if ret != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                {
                    // MADV_REMOVE requires a shared file-backed mapping.
                    ret = unsafe {
                        libc::madvise(
                            segment.host_addr.cast(),
                            segment.range.len() as usize,
                            libc::MADV_DONTNEED,
                        )
                    };
                }
                if ret != 0 {
                    return Err(MemoryError::DiscardSharedBacking(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
        } else {
            let KvmMemoryBackingMode::GuestMemfd(backing) = &self.memory_backing_mode else {
                return Err(MemoryError::InvalidMapGpaRange);
            };
            for segment in segments {
                tracing::debug!(
                    gpa = segment.range.start(),
                    size = segment.range.len(),
                    guest_memfd_offset = segment.guest_memfd_offset,
                    isolation_name,
                    "discarding private backing after shared conversion"
                );
                let ret = unsafe {
                    libc::fallocate(
                        backing.file.as_raw_fd(),
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        segment.guest_memfd_offset as libc::off_t,
                        segment.range.len() as libc::off_t,
                    )
                };
                if ret != 0 {
                    return Err(MemoryError::DiscardPrivateBacking(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Applies a KVM CCA v15 memory-fault/RIPAS state transition.
    ///
    /// The kernel supplies a page-aligned range and indicates whether it must
    /// become private. The range must remain within configured RAM. As with SNP
    /// conversions, the old backing is discarded after KVM accepts the new
    /// memory attribute so stale contents cannot reappear on a later transition.
    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn handle_cca_ripas_change(
        &self,
        gpa: u64,
        size: u64,
        flags: u64,
    ) -> Result<(), KvmError> {
        let end = gpa
            .checked_add(size)
            .ok_or(KvmError::InvalidCcaMemoryFault)?;
        if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE)
            || !size.is_multiple_of(hvdef::HV_PAGE_SIZE)
            || size == 0
        {
            return Err(KvmError::InvalidCcaMemoryFault);
        }

        let unsupported_flags = flags & !kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI;
        if unsupported_flags != 0 {
            return Err(KvmError::UnsupportedCcaMemoryFaultFlags(flags));
        }

        let private = flags & kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI != 0;
        let range = MemoryRange::new(gpa..end);
        let state = self.memory.lock();
        let segments = guest_memfd_range_intersections(range, &state.ranges)
            .map_err(map_cca_conversion_error)?;

        tracing::debug!(gpa, size, flags, private, "KVM CCA RIPAS change");
        if let Err(err) = self.discard_stale_private_memory_backing(&segments, private, "CCA") {
            self.mark_cca_fatal();
            return Err(map_cca_conversion_error(err));
        }
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn convert_cca_segments(
        &self,
        segments: &[KvmMemoryRangeSegment],
        private: bool,
    ) -> Result<(), KvmError> {
        let KvmMemoryBackingMode::GuestMemfd(backing) = &self.memory_backing_mode else {
            return Err(KvmError::InvalidCcaMemoryFault);
        };
        convert_in_place_segments(segments, private, |attributes| {
            kvm::set_guest_memfd_memory_attributes(backing.file.as_fd(), attributes)
        })?;
        Ok(())
    }
}

#[cfg(any(guest_arch = "aarch64", test))]
fn convert_in_place_segments(
    segments: &[KvmMemoryRangeSegment],
    private: bool,
    mut set_attributes: impl FnMut(&mut kvm::KvmMemoryAttributes2) -> Result<(), kvm::Error>,
) -> Result<(), MemoryError> {
    validate_conversion_segments(segments)?;
    for segment in segments {
        let mut attributes = kvm::KvmMemoryAttributes2 {
            offset: segment.guest_memfd_offset,
            size: segment.range.len(),
            attributes: if private {
                kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
            } else {
                0
            },
            ..Default::default()
        };
        // No retry: even EFAULT can follow a partial conversion. error_offset
        // is diagnostic, not a safe restart point.
        set_attributes(&mut attributes)?;
    }
    Ok(())
}

#[cfg(any(guest_arch = "aarch64", test))]
fn validate_conversion_segments(segments: &[KvmMemoryRangeSegment]) -> Result<(), MemoryError> {
    // Validate all offsets before the first ioctl, including packed NUMA
    // offsets that are not consecutive in guest-physical order.
    for segment in segments {
        let size = segment.range.len();
        if size == 0
            || !segment.range.start().is_multiple_of(hvdef::HV_PAGE_SIZE)
            || !size.is_multiple_of(hvdef::HV_PAGE_SIZE)
            || !segment
                .guest_memfd_offset
                .is_multiple_of(hvdef::HV_PAGE_SIZE)
            || segment.guest_memfd_offset.checked_add(size).is_none()
        {
            return Err(MemoryError::InvalidMapGpaRange);
        }
    }
    Ok(())
}

#[cfg(any(guest_arch = "aarch64", test))]
fn convert_assignment_segments(
    segments: &[KvmMemoryRangeSegment],
    private: bool,
    shared_bit: u64,
    file: Arc<File>,
    operations: &mut dyn tdisp::host::SharedDma,
    mut set_attributes: impl FnMut(&mut kvm::KvmMemoryAttributes2) -> Result<(), kvm::Error>,
    mut prefault: impl FnMut(MemoryRange) -> Result<(), kvm::Error>,
) -> Result<(), MemoryError> {
    validate_conversion_segments(segments)?;
    if shared_bit < 4096
        || !shared_bit.is_power_of_two()
        || segments.iter().any(|s| {
            s.range.end() > shared_bit
                || (s.range.start() | shared_bit)
                    .checked_add(s.range.len())
                    .is_none()
        })
    {
        return Err(MemoryError::InvalidMapGpaRange);
    }

    for segment in segments {
        let iova = segment.range.start() | shared_bit;
        let length = segment.range.len();
        if private {
            operations
                .unmap(iova, length)
                .map_err(MemoryError::AssignmentDma)?;
        }
        convert_in_place_segments(std::slice::from_ref(segment), private, &mut set_attributes)?;
        if private {
            prefault(segment.range)?;
        } else {
            operations
                .map(tdisp::host::SharedMapping {
                    file: file.clone(),
                    file_offset: segment.guest_memfd_offset,
                    iova,
                    length,
                })
                .map_err(MemoryError::AssignmentDma)?;
        }
    }
    Ok(())
}

#[cfg(any(guest_arch = "aarch64", test))]
fn prepare_private_assignment(
    slots: &[MemoryRange],
    operations: &mut dyn tdisp::host::SharedDma,
    mut prefault: impl FnMut(MemoryRange) -> Result<(), kvm::Error>,
) -> Result<(), MemoryError> {
    if slots.is_empty() || crate::cca_in_place::validate_imports(slots.iter().copied()).is_err() {
        return Err(MemoryError::InvalidMapGpaRange);
    }
    for &slot in slots {
        prefault(slot)?;
    }
    operations
        .prepare_private_memory()
        .map_err(MemoryError::AssignmentDma)?;
    Ok(())
}

#[cfg(any(guest_arch = "aarch64", test))]
fn guest_memfd_range_intersections(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<Vec<KvmMemoryRangeSegment>, MemoryError> {
    let mut segments = guest_memfd_intersections(range, slots);
    segments.sort_by_key(|segment| segment.range.start());
    if segments
        .windows(2)
        .any(|segments| segments[0].range.end() > segments[1].range.start())
    {
        return Err(MemoryError::InvalidMapGpaRange);
    }
    Ok(segments)
}

pub(crate) fn guest_memfd_range_segments(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<Vec<KvmMemoryRangeSegment>, MemoryError> {
    let mut segments = guest_memfd_intersections(range, slots);
    segments.sort_by_key(|segment| segment.range.start());

    let mut cursor = range.start();
    for segment in &segments {
        if segment.range.start() != cursor {
            return Err(MemoryError::InvalidMapGpaRange);
        }
        cursor = segment.range.end();
    }
    if cursor != range.end() {
        return Err(MemoryError::InvalidMapGpaRange);
    }

    Ok(segments)
}

fn guest_memfd_intersections(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Vec<KvmMemoryRangeSegment> {
    slots
        .iter()
        .flatten()
        .filter_map(|slot| {
            let guest_memfd_offset = slot.guest_memfd_offset?;
            let start = range.start().max(slot.range.start());
            let end = range.end().min(slot.range.end());
            (start < end).then(|| {
                let slot_offset = start - slot.range.start();
                KvmMemoryRangeSegment {
                    range: MemoryRange::new(start..end),
                    host_addr: slot.host_addr.wrapping_add(slot_offset as usize),
                    guest_memfd_offset: guest_memfd_offset + slot_offset,
                }
            })
        })
        .collect()
}

/// Resolves an imported range to a private guestmemfd slot and source HVA.
///
/// The entire range must be contained in one slot whose private attribute is
/// already active.
pub(crate) fn private_memory_range_from_slots(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<KvmPrivateMemoryRange, MemoryError> {
    let slot = slots
        .iter()
        .flatten()
        .find(|slot| slot.range.contains(&range))
        .ok_or(MemoryError::InvalidPrivateMemoryRange)?;

    if slot.guest_memfd_offset.is_none() || slot.private_state.is_none() {
        return Err(MemoryError::InvalidPrivateMemoryRange);
    }

    let offset = range.start() - slot.range.start();
    Ok(KvmPrivateMemoryRange {
        gpa: range,
        hva: slot.host_addr.wrapping_add(offset as usize),
    })
}

/// Verifies the KVM capabilities required for guestmemfd private memory.
pub(crate) fn check_private_memory_extensions(
    kvm: &kvm::Partition,
    private_state: KvmGuestMemfdPrivateState,
) -> Result<(), MemoryError> {
    require_kvm_extension(kvm, kvm::KVM_CAP_USER_MEMORY2, "KVM_CAP_USER_MEMORY2")?;
    require_kvm_extension(kvm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;
    let flags = private_state.create_flags();
    if flags != 0 {
        let supported = require_kvm_extension(
            kvm,
            kvm::KVM_CAP_GUEST_MEMFD_FLAGS_UAPI,
            "KVM_CAP_GUEST_MEMFD_FLAGS",
        )?;
        require_capability_bits(
            supported,
            flags,
            "KVM_CAP_GUEST_MEMFD_FLAGS (MMAP | INIT_SHARED)",
        )?;
    }
    let (capability, capability_name) = match private_state {
        #[cfg(guest_arch = "x86_64")]
        KvmGuestMemfdPrivateState::VmAttributes => {
            (kvm::KVM_CAP_MEMORY_ATTRIBUTES, "KVM_CAP_MEMORY_ATTRIBUTES")
        }
        #[cfg(guest_arch = "aarch64")]
        KvmGuestMemfdPrivateState::GuestMemfdDefault => (
            kvm::KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES_UAPI,
            "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES",
        ),
        #[cfg(any(guest_arch = "aarch64", test))]
        KvmGuestMemfdPrivateState::InPlace => (
            kvm::KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES_UAPI,
            "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES",
        ),
    };
    let memory_attributes = require_kvm_extension(kvm, capability, capability_name)?;
    require_capability_bits(
        memory_attributes,
        kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64,
        capability_name,
    )
}

fn require_capability_bits(
    supported: i32,
    required: u64,
    name: &'static str,
) -> Result<(), MemoryError> {
    if supported < 0 || supported as u64 & required != required {
        return Err(kvm::Error::MissingCapability(name).into());
    }
    Ok(())
}

fn require_kvm_extension(
    kvm: &kvm::Partition,
    extension: u32,
    capability: &'static str,
) -> Result<i32, MemoryError> {
    let value = kvm
        .check_extension(extension)
        .map_err(kvm::Error::CheckExtension)?;
    if value == 0 {
        return Err(kvm::Error::MissingCapability(capability).into());
    }
    Ok(value)
}

fn classify_guest_memfd_backing(
    range: MemoryRange,
    ram_ranges: &[KvmGuestMemfdRange],
) -> Result<Option<u64>, MemoryError> {
    let mut containing_ranges = ram_ranges
        .iter()
        .filter(|ram_range| ram_range.range.contains(&range));
    if let Some(ram_range) = containing_ranges.next() {
        if containing_ranges.next().is_some() {
            return Err(MemoryError::UnsupportedIsolationConfiguration(
                "KVM guest_memfd mappings must be contained in exactly one RAM range",
            ));
        }
        return Ok(Some(
            ram_range.file_offset + (range.start() - ram_range.range.start()),
        ));
    }

    if ram_ranges
        .iter()
        .any(|ram_range| ram_range.range.overlaps(&range))
    {
        return Err(MemoryError::UnsupportedIsolationConfiguration(
            "KVM guest_memfd mappings must be fully contained in one RAM range",
        ));
    }

    Ok(None)
}

impl virt::PartitionMemoryMapper for KvmPartition {
    fn memory_mapper(&self, vtl: hvdef::Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, hvdef::Vtl::Vtl0);
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
        // SAFETY: `PartitionMemoryMap::map_range` requires the caller to keep
        // `data..data+size` valid for the lifetime of the mapping. `map_region`
        // preserves that lifetime requirement and records the mapped range so
        // it can be cleared on unmap.
        unsafe { self.map_region(data, size, addr, !writable) }
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size);
        let mut state = self.memory.lock();
        #[cfg(guest_arch = "aarch64")]
        if state.assignment_frozen
            && state
                .ranges
                .iter()
                .flatten()
                .any(|slot| slot.guest_memfd_offset.is_some() && slot.range.overlaps(&range))
        {
            self.mark_cca_fatal();
            return Err(MemoryError::AssignmentRamFrozen.into());
        }
        for (slot, entry) in state.ranges.iter_mut().enumerate() {
            let Some(kvm_range) = entry else { continue };
            if range.contains(&kvm_range.range) {
                let guest_memfd_backed = kvm_range.guest_memfd_offset.is_some();
                if kvm_range
                    .private_state
                    .is_some_and(KvmGuestMemfdPrivateState::uses_vm_attributes)
                {
                    self.kvm.set_memory_attributes(
                        kvm_range.range.start(),
                        kvm_range.range.len(),
                        0,
                    )?;
                }
                #[cfg(guest_arch = "aarch64")]
                if kvm_range.private_state == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault) {
                    let guest_memfd_offset = kvm_range
                        .guest_memfd_offset
                        .ok_or(MemoryError::InvalidPrivateMemoryRange)?;
                    if let Err(err) = self.discard_stale_private_memory_backing(
                        &[KvmMemoryRangeSegment {
                            range: kvm_range.range,
                            host_addr: kvm_range.host_addr,
                            guest_memfd_offset,
                        }],
                        false,
                        "CCA slot unmap",
                    ) {
                        self.mark_cca_fatal();
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed CCA slot backing cleanup; partition marked fatal"
                        );
                    }
                }
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                // TODO: This error propagates to `PartitionMapper::unmap_region`,
                // which currently panics because partition unmap is treated as
                // infallible. Any recoverable policy must keep the slot's backing
                // VA valid until the slot is cleared.
                if let Err(err) = unsafe { self.clear_slot(slot, guest_memfd_backed) } {
                    #[cfg(guest_arch = "aarch64")]
                    if kvm_range.private_state == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault)
                    {
                        self.mark_cca_fatal();
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed to clear CCA slot; partition marked fatal"
                        );
                        return Err(err.into());
                    }
                    return Err(err.into());
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
    use test_with_tracing::test;

    #[derive(Debug, PartialEq, Eq)]
    enum DmaStep {
        Prepare,
        Unmap(u64, u64),
        Attributes(u64, u64, bool),
        Prefault(u64, u64),
        Map(u64, u64, u64),
    }

    struct DmaOps(Arc<parking_lot::Mutex<Vec<DmaStep>>>);

    impl tdisp::host::SharedDma for DmaOps {
        fn map(
            &mut self,
            mapping: tdisp::host::SharedMapping,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.0.lock().push(DmaStep::Map(
                mapping.iova,
                mapping.length,
                mapping.file_offset,
            ));
            Ok(())
        }

        fn unmap(
            &mut self,
            iova: u64,
            length: u64,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.0.lock().push(DmaStep::Unmap(iova, length));
            Ok(())
        }

        fn prepare_private_memory(
            &mut self,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.0.lock().push(DmaStep::Prepare);
            Ok(())
        }
    }

    #[test]
    fn assignment_prefaults_only_all_supplied_ram_before_dma_readiness() {
        let steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let slots = [range(0x1000, 0x3000), range(0x8000, 0xa000)];
        prepare_private_assignment(&slots, &mut DmaOps(steps.clone()), |range| {
            steps
                .lock()
                .push(DmaStep::Prefault(range.start(), range.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            *steps.lock(),
            [
                DmaStep::Prefault(0x1000, 0x2000),
                DmaStep::Prefault(0x8000, 0x2000),
                DmaStep::Prepare,
            ]
        );
        steps.lock().clear();
        assert!(
            prepare_private_assignment(&slots, &mut DmaOps(steps.clone()), |_| {
                Err(kvm::Error::MissingCapability("injected prefault failure"))
            })
            .is_err()
        );
        assert!(steps.lock().is_empty());
        assert!(
            prepare_private_assignment(&[], &mut DmaOps(steps.clone()), |_| panic!("empty RAM"))
                .is_err()
        );
    }

    #[test]
    fn assignment_conversion_uses_selector_and_packed_file_offsets_in_order() {
        let steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let segments = [
            KvmMemoryRangeSegment {
                range: range(0x1000, 0x2000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: 0x7000,
            },
            KvmMemoryRangeSegment {
                range: range(0x3000, 0x4000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: 0xa000,
            },
        ];
        let file = Arc::new(File::open("/dev/null").unwrap());
        let selector = 1 << 40;
        for private in [true, false] {
            steps.lock().clear();
            convert_assignment_segments(
                &segments,
                private,
                selector,
                file.clone(),
                &mut DmaOps(steps.clone()),
                |attributes| {
                    steps.lock().push(DmaStep::Attributes(
                        attributes.offset,
                        attributes.size,
                        attributes.attributes != 0,
                    ));
                    Ok(())
                },
                |range| {
                    steps
                        .lock()
                        .push(DmaStep::Prefault(range.start(), range.len()));
                    Ok(())
                },
            )
            .unwrap();
            let mut expected = Vec::new();
            for segment in segments {
                if private {
                    expected.push(DmaStep::Unmap(
                        selector | segment.range.start(),
                        segment.range.len(),
                    ));
                }
                expected.push(DmaStep::Attributes(
                    segment.guest_memfd_offset,
                    segment.range.len(),
                    private,
                ));
                if private {
                    expected.push(DmaStep::Prefault(
                        segment.range.start(),
                        segment.range.len(),
                    ));
                } else {
                    expected.push(DmaStep::Map(
                        selector | segment.range.start(),
                        segment.range.len(),
                        segment.guest_memfd_offset,
                    ));
                }
            }
            assert_eq!(*steps.lock(), expected);
        }
    }

    #[test]
    fn assignment_conversion_stops_after_failed_attributes_without_remapping() {
        let steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let segment = KvmMemoryRangeSegment {
            range: range(0x1000, 0x2000),
            host_addr: std::ptr::null_mut(),
            guest_memfd_offset: 0x7000,
        };
        for private in [true, false] {
            steps.lock().clear();
            assert!(
                convert_assignment_segments(
                    &[segment],
                    private,
                    1 << 40,
                    Arc::new(File::open("/dev/null").unwrap()),
                    &mut DmaOps(steps.clone()),
                    |_| Err(kvm::Error::MissingCapability("partial conversion")),
                    |_| panic!("prefault after failed attributes"),
                )
                .is_err()
            );
            if private {
                assert_eq!(*steps.lock(), [DmaStep::Unmap((1 << 40) | 0x1000, 0x1000)]);
            } else {
                assert!(steps.lock().is_empty());
            }
        }
    }

    #[test]
    fn assignment_dma_and_prefault_failures_stop_the_transaction() {
        struct RejectDma;
        impl tdisp::host::SharedDma for RejectDma {
            fn map(
                &mut self,
                _: tdisp::host::SharedMapping,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Err(std::io::Error::other("injected map failure").into())
            }
            fn unmap(
                &mut self,
                _: u64,
                _: u64,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Err(std::io::Error::other("injected unmap failure").into())
            }
            fn prepare_private_memory(
                &mut self,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Err(std::io::Error::other("injected readiness failure").into())
            }
        }
        let segments = [
            KvmMemoryRangeSegment {
                range: range(0x1000, 0x2000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: 0,
            },
            KvmMemoryRangeSegment {
                range: range(0x2000, 0x3000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: 0x1000,
            },
        ];
        let file = Arc::new(File::open("/dev/null").unwrap());
        assert!(
            convert_assignment_segments(
                &segments,
                true,
                1 << 40,
                file.clone(),
                &mut RejectDma,
                |_| panic!("attributes after unmap failure"),
                |_| panic!("prefault after unmap failure"),
            )
            .is_err()
        );
        let mut attributes = 0;
        assert!(
            convert_assignment_segments(
                &segments,
                false,
                1 << 40,
                file.clone(),
                &mut RejectDma,
                |_| {
                    attributes += 1;
                    Ok(())
                },
                |_| panic!("prefault shared RAM"),
            )
            .is_err()
        );
        assert_eq!(attributes, 1);
        let steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        attributes = 0;
        assert!(
            convert_assignment_segments(
                &segments,
                true,
                1 << 40,
                file,
                &mut DmaOps(steps.clone()),
                |_| {
                    attributes += 1;
                    Ok(())
                },
                |_| Err(kvm::Error::MissingCapability(
                    "injected private prefault failure"
                )),
            )
            .is_err()
        );
        assert_eq!(attributes, 1);
        assert_eq!(*steps.lock(), [DmaStep::Unmap((1 << 40) | 0x1000, 0x1000)]);
    }

    #[test]
    fn assignment_conversion_rejects_alias_and_offset_overflow_before_dma() {
        let steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        for (start, offset) in [(1 << 40, 0), (0x1000, u64::MAX - 0xfff)] {
            let segment = KvmMemoryRangeSegment {
                range: range(start, start + 0x1000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: offset,
            };
            assert!(
                convert_assignment_segments(
                    &[segment],
                    true,
                    1 << 40,
                    Arc::new(File::open("/dev/null").unwrap()),
                    &mut DmaOps(steps.clone()),
                    |_| panic!("invalid conversion attributes"),
                    |_| panic!("invalid conversion prefault"),
                )
                .is_err()
            );
        }
        assert!(steps.lock().is_empty());
    }

    fn shared_state(slots: Vec<MemoryRange>) -> parking_lot::Mutex<KvmMemoryRangeState> {
        let mut visibility = crate::cca_in_place::Visibility::all_private(slots.clone()).unwrap();
        for &slot in &slots {
            visibility.record(slot, false);
        }
        parking_lot::Mutex::new(KvmMemoryRangeState {
            ranges: slots
                .into_iter()
                .map(|range| {
                    Some(KvmMemoryRange {
                        range,
                        host_addr: std::ptr::null_mut(),
                        guest_memfd_offset: Some(0),
                        private_state: Some(KvmGuestMemfdPrivateState::InPlace),
                    })
                })
                .collect(),
            cca_visibility: visibility,
            ..Default::default()
        })
    }

    #[test]
    fn shared_buffer_byte_ranges_reject_aliases_and_overflow() {
        let bit = 1 << 48;
        assert_eq!(
            shared_buffer_pages(0x1fff, 2, bit).unwrap(),
            range(0x1000, 0x3000)
        );
        assert_eq!(
            shared_buffer_pages(bit - 1, 1, bit).unwrap(),
            range(bit - 4096, bit)
        );
        for (gpa, len, selector) in [
            (0, 0, bit),
            (bit, 1, bit),
            (bit + 0x1000, 1, bit),
            (bit - 1, 2, bit),
            (u64::MAX, 2, bit),
            (0x1000, 1, 0),
            (0, 1, 1),
            (0x1000, 1, 0x3000),
        ] {
            assert!(shared_buffer_pages(gpa, len, selector).is_err());
        }
    }

    #[test]
    fn shared_buffer_checks_all_slots_and_visibility_before_copy() {
        use std::sync::atomic::AtomicBool;
        let fatal = AtomicBool::new(false);
        let memory = shared_state(vec![range(0x1000, 0x2000), range(0x2000, 0x3000)]);
        let gm = guestmem::GuestMemory::allocate(0x4000);
        with_shared_buffer(&memory, &fatal, 0x1fff, 2, 1 << 48, || {
            assert!(memory.try_lock().is_none());
            gm.write_at(0x1fff, &[0xa5, 0x5a])
        })
        .unwrap();
        let mut bytes = [0; 2];
        gm.read_at(0x1fff, &mut bytes).unwrap();
        assert_eq!(bytes, [0xa5, 0x5a]);

        memory.lock().ranges[0].as_mut().unwrap().private_state = Some(test_private_state());
        assert!(matches!(
            with_shared_buffer(&memory, &fatal, 0x1000, 1, 1 << 48, || panic!(
                "separate backing"
            )),
            Err(SharedBufferError::Unsupported)
        ));
        memory.lock().ranges[0].as_mut().unwrap().private_state =
            Some(KvmGuestMemfdPrivateState::InPlace);
        memory
            .lock()
            .cca_visibility
            .record(range(0x2000, 0x3000), true);
        assert!(matches!(
            with_shared_buffer(&memory, &fatal, 0x1fff, 2, 1 << 48, || panic!(
                "private copy"
            )),
            Err(SharedBufferError::Visibility(
                crate::cca_in_place::CcaInPlaceError::PrivateBuffer
            ))
        ));
        memory
            .lock()
            .cca_visibility
            .record(range(0x2000, 0x3000), false);
        memory.lock().ranges[1] = None;
        assert!(matches!(
            with_shared_buffer(&memory, &fatal, 0x1fff, 2, 1 << 48, || panic!(
                "removed slot"
            )),
            Err(SharedBufferError::Slots(_))
        ));
        let holes = shared_state(vec![range(0x1000, 0x2000), range(0x3000, 0x4000)]);
        assert!(
            with_shared_buffer(&holes, &fatal, 0x1fff, 0x1002, 1 << 48, || panic!(
                "RAM hole"
            ))
            .is_err()
        );
    }

    #[test]
    fn uninitialized_visibility_and_fatal_state_reject_access() {
        use std::sync::atomic::AtomicBool;
        let memory = shared_state(vec![range(0x1000, 0x2000)]);
        memory.lock().cca_visibility = Default::default();
        let fatal = AtomicBool::new(false);
        assert!(matches!(
            with_shared_buffer(&memory, &fatal, 0x1000, 1, 1 << 48, || panic!(
                "unknown visibility"
            )),
            Err(SharedBufferError::Visibility(_))
        ));
        fatal.store(true, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            with_shared_buffer(&memory, &fatal, 0x1000, 1, 1 << 48, || panic!("fatal copy")),
            Err(SharedBufferError::Fatal)
        ));
    }

    #[test]
    fn conversion_cannot_race_shared_buffer_copy() {
        use std::sync::atomic::AtomicBool;
        let memory = shared_state(vec![range(0x1000, 0x2000)]);
        let fatal = AtomicBool::new(false);
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (checked_tx, checked_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let memory_ref = &memory;
            scope.spawn(move || {
                start_rx.recv().unwrap();
                assert!(memory_ref.try_lock().is_none());
                checked_tx.send(()).unwrap();
                memory_ref
                    .lock()
                    .cca_visibility
                    .record(range(0x1000, 0x2000), true);
            });
            with_shared_buffer(&memory, &fatal, 0x1001, 2, 1 << 48, || {
                start_tx.send(()).unwrap();
                checked_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        assert!(
            with_shared_buffer(&memory, &fatal, 0x1001, 2, 1 << 48, || panic!(
                "converted memory"
            ))
            .is_err()
        );
    }

    #[test]
    fn shared_copy_fault_preserves_error_and_does_not_claim_rollback() {
        let memory = shared_state(vec![range(0x1000, 0x2000)]);
        let fatal = std::sync::atomic::AtomicBool::new(false);
        let gm = guestmem::GuestMemory::allocate(0x2000);
        let result = with_shared_buffer(&memory, &fatal, 0x1000, 2, 1 << 48, || {
            gm.write_at(0x1000, &[0xa5])?;
            gm.write_at(0x2000, &[0x5a])
        });
        assert!(matches!(result, Err(SharedBufferError::Copy(_))));
        let mut prefix = [0];
        gm.read_at(0x1000, &mut prefix).unwrap();
        assert_eq!(prefix, [0xa5]);
        assert!(!fatal.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn in_place_requires_both_creation_flags_and_private_attributes() {
        assert_eq!(test_private_state().create_flags(), 0);
        let flags = KvmGuestMemfdPrivateState::InPlace.create_flags();
        assert_eq!(flags, 3);
        for supported in [-1, 0, 1, 2] {
            assert!(require_capability_bits(supported, flags, "flags").is_err());
        }
        require_capability_bits(3, flags, "flags").unwrap();
        require_capability_bits(7, flags, "flags").unwrap();
        assert!(
            require_capability_bits(1, kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64, "attributes")
                .is_err()
        );
        require_capability_bits(
            kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as i32,
            kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64,
            "attributes",
        )
        .unwrap();
    }

    #[test]
    fn in_place_converts_packed_offsets_without_retry_or_discard() {
        let segments = [
            KvmMemoryRangeSegment {
                range: range(0x1000, 0x2000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: 0x8000,
            },
            KvmMemoryRangeSegment {
                range: range(0x2000, 0x4000),
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: 0,
            },
        ];
        for private in [false, true] {
            for fail in [false, true] {
                let mut requests = Vec::new();
                let result = convert_in_place_segments(&segments, private, |request| {
                    requests.push(*request);
                    if fail {
                        request.error_offset = request.offset + 4096;
                        return Err(kvm::Error::MissingCapability("injected conversion error"));
                    }
                    Ok(())
                });
                assert_eq!(result.is_err(), fail);
                assert_eq!(requests.len(), if fail { 1 } else { 2 });
                assert_eq!(requests[0].offset, 0x8000);
                assert_eq!(
                    requests[0].attributes,
                    if private {
                        kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
                    } else {
                        0
                    }
                );
                if !fail {
                    assert_eq!(requests[1].offset, 0);
                    assert_eq!(requests[1].size, 0x2000);
                }
            }
        }
        for offset in [1, u64::MAX - 4095] {
            let invalid = [KvmMemoryRangeSegment {
                guest_memfd_offset: offset,
                ..segments[0]
            }];
            assert!(
                convert_in_place_segments(&invalid, true, |_| panic!("invalid request")).is_err()
            );
        }
    }

    #[test]
    fn in_place_mixed_visibility_preserves_shared_pages_across_packed_slots() {
        let slots = vec![
            Some(KvmMemoryRange {
                host_addr: std::ptr::null_mut(),
                range: range(0x1000, 0x4000),
                guest_memfd_offset: Some(0x3000),
                private_state: Some(KvmGuestMemfdPrivateState::InPlace),
            }),
            Some(KvmMemoryRange {
                host_addr: std::ptr::null_mut(),
                range: range(0x4000, 0x7000),
                guest_memfd_offset: Some(0),
                private_state: Some(KvmGuestMemfdPrivateState::InPlace),
            }),
        ];
        let mut visibility = crate::cca_in_place::Visibility::all_private(
            slots.iter().flatten().map(|slot| slot.range).collect(),
        )
        .unwrap();
        visibility.record(range(0x1000, 0x2000), false);
        visibility.record(range(0x5000, 0x6000), false);
        let request = range(0x1000, 0x7000);
        let mut calls = Vec::new();
        for change in visibility.changes(request, false).unwrap() {
            let segments = guest_memfd_range_segments(change, &slots).unwrap();
            convert_in_place_segments(&segments, false, |attributes| {
                calls.push((attributes.offset, attributes.size));
                Ok(())
            })
            .unwrap();
            visibility.record(change, false);
        }
        assert_eq!(calls, [(0x4000, 0x2000), (0, 0x1000), (0x2000, 0x1000)]);
        assert!(visibility.changes(request, false).unwrap().is_empty());
    }

    #[test]
    fn in_place_init_includes_only_ram_slots() {
        let mut state = KvmMemoryRangeState {
            ranges: vec![
                None,
                Some(KvmMemoryRange {
                    host_addr: std::ptr::null_mut(),
                    range: range(0x1000, 0x3000),
                    guest_memfd_offset: Some(0),
                    private_state: Some(KvmGuestMemfdPrivateState::InPlace),
                }),
                Some(KvmMemoryRange {
                    host_addr: std::ptr::null_mut(),
                    range: range(0x5000, 0x7000),
                    guest_memfd_offset: None,
                    private_state: None,
                }),
            ],
            ..Default::default()
        };
        assert!(!state.assignment_prepared);
        assert!(!state.assignment_frozen);
        assert!(!state.protected_attempts.covers(range(0x1000, 0x2000)));
        assert_eq!(state.in_place_ram_slots(), [range(0x1000, 0x3000)]);
        state.cca_visibility =
            crate::cca_in_place::Visibility::all_private(state.in_place_ram_slots()).unwrap();
        assert!(
            state
                .cca_visibility
                .changes(range(0x1000, 0x3000), true)
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .cca_visibility
                .changes(range(0x1000, 0x7000), true)
                .is_err()
        );
        assert!(!KvmMemoryBackingMode::Userspace.is_in_place());
        let mode = KvmMemoryBackingMode::GuestMemfd(KvmGuestMemfdBacking {
            file: sparse_mmap::alloc_shared_memory(0x2000, "in-place-test")
                .unwrap()
                .into(),
            ranges: guest_memfd_ranges(&[range(0x1000, 0x3000)]),
            private_state: KvmGuestMemfdPrivateState::InPlace,
        });
        assert!(mode.is_in_place());
    }

    #[test]
    fn default_prototype_preparation_leaves_userspace_allocation_to_worker() {
        use crate::KvmError;
        use crate::KvmProcessorBinder;
        use virt::ProtoPartition;
        use vm_topology::memory::MemoryLayout;
        use vm_topology::memory::MemoryRangeWithNode;

        struct DefaultProto;
        impl ProtoPartition for DefaultProto {
            type Partition = KvmPartition;
            type ProcessorBinder = KvmProcessorBinder;
            type Error = KvmError;

            fn max_physical_address_size(&self) -> u8 {
                32
            }

            fn build(
                self,
                _config: virt::PartitionConfig<'_>,
            ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
                Err(KvmError::NotSupported)
            }
        }

        let layout = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: range(0x1000, 0x3000),
                vnode: 0,
            }],
            &[],
        )
        .unwrap();
        assert!(DefaultProto.prepare_ram_backing(&layout).unwrap().is_none());
    }

    #[test]
    fn prepared_ram_retains_backing_and_rejects_layout_changes() {
        let mut prepared = KvmPreparedRamBacking::default();
        let ranges = vec![range(0x1000, 0x3000), range(0x8000, 0xa000)];
        let file: File = sparse_mmap::alloc_shared_memory(0x4000, "prepared-kvm-ram-test")
            .unwrap()
            .into();
        let fd = file.as_raw_fd();
        let mode = prepared
            .prepare(ranges.clone(), |ranges| {
                Ok(KvmMemoryBackingMode::GuestMemfd(KvmGuestMemfdBacking {
                    file,
                    ranges: guest_memfd_ranges(ranges),
                    private_state: test_private_state(),
                }))
            })
            .unwrap();
        assert!(
            matches!(mode, KvmMemoryBackingMode::GuestMemfd(backing) if backing.file.as_raw_fd() == fd)
        );
        // Both an explicit preparation and the direct-build fallback use this
        // method. A repeat must not replace the file or its packed offsets.
        let mode = prepared
            .prepare(ranges, |_| panic!("must reuse prepared backing"))
            .unwrap();
        let KvmMemoryBackingMode::GuestMemfd(backing) = mode else {
            panic!("lost guestmemfd backing");
        };
        assert_eq!(backing.file.as_raw_fd(), fd);
        assert_eq!(backing.ranges[1].file_offset, 0x2000);
        assert!(matches!(
            prepared.prepare(vec![range(0x1000, 0x5000)], |_| panic!(
                "must reject changed layout"
            )),
            Err(MemoryError::PreparedRamLayoutChanged)
        ));
    }

    #[test]
    fn unprepared_ram_falls_back_and_failed_preparation_can_retry() {
        let mut prepared = KvmPreparedRamBacking::default();
        let ranges = vec![range(0x1000, 0x3000)];
        assert!(
            prepared
                .prepare(ranges.clone(), |_| {
                    Err(MemoryError::UnsupportedIsolationConfiguration(
                        "test allocation failure",
                    ))
                })
                .is_err()
        );
        assert!(matches!(
            prepared
                .prepare(ranges.clone(), |_| Ok(KvmMemoryBackingMode::Userspace))
                .unwrap(),
            KvmMemoryBackingMode::Userspace
        ));
        assert!(matches!(
            prepared
                .prepare(ranges, |_| panic!("must reuse ordinary mode"))
                .unwrap(),
            KvmMemoryBackingMode::Userspace
        ));
    }

    fn range(start: u64, end: u64) -> MemoryRange {
        MemoryRange::new(start..end)
    }

    fn guest_memfd_ranges(ranges: &[MemoryRange]) -> Vec<KvmGuestMemfdRange> {
        let mut file_offset = 0;
        ranges
            .iter()
            .map(|&range| {
                let guest_memfd_range = KvmGuestMemfdRange { range, file_offset };
                file_offset += range.len();
                guest_memfd_range
            })
            .collect()
    }

    fn test_private_state() -> KvmGuestMemfdPrivateState {
        #[cfg(guest_arch = "x86_64")]
        {
            KvmGuestMemfdPrivateState::VmAttributes
        }
        #[cfg(guest_arch = "aarch64")]
        {
            KvmGuestMemfdPrivateState::GuestMemfdDefault
        }
    }

    #[test]
    fn guest_memfd_classifier_selects_contained_ram() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges).unwrap(),
            Some(0x1000)
        );
        assert_eq!(
            classify_guest_memfd_backing(range(0x1_1000, 0x1_3000), &ram_ranges).unwrap(),
            Some(0x9000)
        );
    }

    #[test]
    fn guest_memfd_classifier_keeps_non_ram_userspace() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0xa000, 0xc000), &ram_ranges).unwrap(),
            None
        );
    }

    #[test]
    fn guest_memfd_classifier_rejects_partial_ram_overlap() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x8000, 0xa000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_does_not_merge_adjacent_ram_ranges() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x3000), range(0x3000, 0x5000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_rejects_ambiguous_ram_containment() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x5000), range(0x2000, 0x4000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn private_memory_range_resolves_hva_offset() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd_offset: Some(0),
            private_state: Some(test_private_state()),
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
            guest_memfd_offset: None,
            private_state: Some(test_private_state()),
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &userspace_slots),
            Err(MemoryError::InvalidPrivateMemoryRange)
        ));

        let shared_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd_offset: Some(0),
            private_state: None,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &shared_slots),
            Err(MemoryError::InvalidPrivateMemoryRange)
        ));
    }

    #[test]
    fn guest_memfd_segments_cover_adjacent_unordered_slots() {
        let mut first_backing = vec![0u8; 0x2000];
        let mut second_backing = vec![0u8; 0x2000];
        let first_host_addr = first_backing.as_mut_ptr();
        let second_host_addr = second_backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr: second_host_addr,
                range: range(0x3000, 0x5000),
                guest_memfd_offset: Some(0x8000),
                private_state: None,
            }),
            Some(KvmMemoryRange {
                host_addr: first_host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd_offset: Some(0x4000),
                private_state: Some(test_private_state()),
            }),
        ];

        let segments = guest_memfd_range_segments(range(0x2000, 0x4000), &slots).unwrap();

        assert_eq!(
            segments,
            [
                KvmMemoryRangeSegment {
                    range: range(0x2000, 0x3000),
                    host_addr: first_host_addr.wrapping_add(0x1000),
                    guest_memfd_offset: 0x5000,
                },
                KvmMemoryRangeSegment {
                    range: range(0x3000, 0x4000),
                    host_addr: second_host_addr,
                    guest_memfd_offset: 0x8000,
                },
            ]
        );
    }

    #[test]
    fn guest_memfd_segments_reject_incomplete_coverage() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let gapped_slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x2000),
                guest_memfd_offset: Some(0),
                private_state: Some(test_private_state()),
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x2000),
                range: range(0x3000, 0x4000),
                guest_memfd_offset: Some(0x2000),
                private_state: Some(test_private_state()),
            }),
        ];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &gapped_slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));

        let userspace_slot = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x4000),
            guest_memfd_offset: None,
            private_state: None,
        })];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &userspace_slot),
            Err(MemoryError::InvalidMapGpaRange)
        ));
    }

    #[test]
    fn guest_memfd_intersections_allow_unbacked_ranges() {
        let mut backing = vec![0u8; 0x1000];
        let host_addr = backing.as_mut_ptr();
        let slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x2000, 0x3000),
            guest_memfd_offset: Some(0x4000),
            private_state: Some(test_private_state()),
        })];

        let segments = guest_memfd_range_intersections(range(0x1000, 0x4000), &slots).unwrap();
        assert_eq!(
            segments,
            [KvmMemoryRangeSegment {
                range: range(0x2000, 0x3000),
                host_addr,
                guest_memfd_offset: 0x4000,
            }]
        );
        assert!(
            guest_memfd_range_intersections(range(0x4000, 0x5000), &slots)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn guest_memfd_segments_reject_overlapping_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd_offset: Some(0),
                private_state: Some(test_private_state()),
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x1000),
                range: range(0x2000, 0x4000),
                guest_memfd_offset: Some(0x1000),
                private_state: Some(test_private_state()),
            }),
        ];

        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));
        assert!(matches!(
            guest_memfd_range_intersections(range(0x1000, 0x4000), &slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));
    }
}
