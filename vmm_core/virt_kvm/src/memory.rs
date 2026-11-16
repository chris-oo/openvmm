// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM memory-slot and confidential guest backing management.
//!
//! Confidential RAM uses one mmap-capable guestmemfd for both shared and private
//! access. File offsets, rather than GPAs or memory slots, own its attributes.

use crate::KvmPartition;
use crate::KvmPartitionInner;
use inspect::Inspect;
use memory_range::MemoryRange;
#[cfg(guest_arch = "x86_64")]
use std::fs::File;
#[cfg(guest_arch = "x86_64")]
use std::os::fd::AsFd;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("kvm memory operation failed")]
    Kvm(#[from] kvm::Error),
    #[error("cannot resize KVM guest_memfd memory slot")]
    CannotResizeGuestMemfdSlot,
    #[error("cannot move KVM guest_memfd RAM away from its allocated GPA")]
    CannotMoveGuestMemfdSlot,
    #[error("private memory range is not contained in guest_memfd private memory")]
    InvalidPrivateMemoryRange,
    #[error("invalid KVM_HC_MAP_GPA_RANGE request")]
    InvalidMapGpaRange,
    #[error("unsupported KVM_HC_MAP_GPA_RANGE attributes: {0:#x}")]
    UnsupportedMapGpaRangeAttributes(u64),
    #[error("failed to clone guest_memfd backing")]
    CloneBacking(#[source] std::io::Error),
    #[error(
        "guest_memfd file {file_index} conversion of {range:?} failed; memory state may be partially changed"
    )]
    Conversion {
        file_index: usize,
        range: MemoryRange,
        #[source]
        source: kvm::Error,
    },
    #[error("unsupported isolation configuration: {0}")]
    UnsupportedIsolationConfiguration(&'static str),
}

#[derive(Debug, Inspect)]
/// A registered KVM memory slot and its confidential-memory metadata.
pub(crate) struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
    guest_memfd: Option<KvmGuestMemfdMapping>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
struct KvmGuestMemfdMapping {
    file_index: usize,
    file_offset: u64,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
/// Slot-indexed memory mappings currently registered with KVM.
pub(crate) struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    pub(crate) ranges: Vec<Option<KvmMemoryRange>>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
/// A guestmemfd range paired with its initially shared userspace mapping.
pub(crate) struct KvmPrivateMemoryRange {
    /// Guest-physical range covered by the private slot.
    pub(crate) gpa: MemoryRange,
    /// Host address, usable only while the file range is shared.
    pub(crate) hva: *mut u8,
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct KvmMemoryRangeSegment {
    range: MemoryRange,
    mapping: KvmGuestMemfdMapping,
}

#[derive(Debug, Inspect)]
#[inspect(external_tag)]
/// Backing strategy for partition memory slots.
pub(crate) enum KvmMemoryBackingMode {
    /// Register only the caller-provided userspace mapping.
    Userspace,
    #[cfg(guest_arch = "x86_64")]
    /// Register the same guestmemfd for shared and private RAM.
    GuestMemfd(KvmGuestMemfdBacking),
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Inspect)]
/// Partition-owned guestmemfds and their packed mappings of guest RAM ranges.
#[derive(Default)]
pub(crate) struct KvmGuestMemfdBacking {
    #[inspect(skip)]
    files: Vec<File>,
    #[inspect(iter_by_index)]
    ranges: Vec<KvmGuestMemfdRange>,
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
struct KvmGuestMemfdRange {
    range: MemoryRange,
    mapping: KvmGuestMemfdMapping,
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug)]
enum KvmMemoryBacking<'a> {
    Userspace,
    GuestMemfd {
        file: &'a File,
        mapping: KvmGuestMemfdMapping,
    },
}

#[cfg(guest_arch = "aarch64")]
#[derive(Debug)]
enum KvmMemoryBacking {
    Userspace,
}

#[cfg(guest_arch = "x86_64")]
impl KvmGuestMemfdBacking {
    pub(crate) fn allocate(
        &mut self,
        kvm: &kvm::Partition,
        ram_ranges: &[MemoryRange],
    ) -> Result<File, MemoryError> {
        let mut file_size = 0u64;
        let mut ranges = Vec::new();
        for &range in ram_ranges {
            if range.is_empty()
                || !range.start().is_multiple_of(hvdef::HV_PAGE_SIZE)
                || !range.len().is_multiple_of(hvdef::HV_PAGE_SIZE)
                || self
                    .ranges
                    .iter()
                    .chain(&ranges)
                    .any(|existing: &KvmGuestMemfdRange| existing.range.overlaps(&range))
            {
                return Err(MemoryError::UnsupportedIsolationConfiguration(
                    "guest_memfd RAM ranges must be nonempty, page aligned, and nonoverlapping",
                ));
            }
            ranges.push(KvmGuestMemfdRange {
                range,
                mapping: KvmGuestMemfdMapping {
                    file_index: self.files.len(),
                    file_offset: file_size,
                },
            });
            file_size = file_size.checked_add(range.len()).ok_or(
                MemoryError::UnsupportedIsolationConfiguration("guest_memfd RAM size overflow"),
            )?;
        }
        let file = kvm.create_guest_memfd(
            file_size,
            kvm::GUEST_MEMFD_FLAG_MMAP_UAPI | kvm::GUEST_MEMFD_FLAG_INIT_SHARED_UAPI,
        )?;
        let shared_file = file.try_clone().map_err(MemoryError::CloneBacking)?;
        self.files.push(file);
        self.ranges.extend(ranges);
        Ok(shared_file)
    }

    pub(crate) fn validate_ram(&self, ranges: &[MemoryRange]) -> Result<(), MemoryError> {
        if ranges.len() != self.ranges.len()
            || self
                .ranges
                .iter()
                .any(|allocated| !ranges.contains(&allocated.range))
        {
            return Err(MemoryError::UnsupportedIsolationConfiguration(
                "all SNP RAM must use backend-allocated guest_memfd backing",
            ));
        }
        Ok(())
    }
}

impl KvmPartitionInner {
    /// # Safety
    ///
    /// `data..data+size` must be and remain an allocated VA range until the
    /// partition is destroyed or the region is unmapped.
    /// RAM must map its backend-allocated file and offset. A different backing
    /// must not overlay SNP RAM: KVM does not check the host mapping's identity.
    unsafe fn map_region(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        readonly: bool,
    ) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size as u64);
        let backing = self.memory_backing(range)?;
        #[cfg(guest_arch = "x86_64")]
        if readonly && matches!(&backing, KvmMemoryBacking::GuestMemfd { .. }) {
            return Err(MemoryError::UnsupportedIsolationConfiguration(
                "read-only guest_memfd RAM mappings are not supported",
            )
            .into());
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
                return Err(MemoryError::CannotResizeGuestMemfdSlot.into());
            }
            if existing_range.guest_memfd.is_some() && existing_range.range != range {
                return Err(MemoryError::CannotMoveGuestMemfdSlot.into());
            }
            if existing_range.guest_memfd.is_some() {
                // SAFETY: clearing a slot removes the memory reference.
                unsafe { self.clear_slot(slot_to_use, true)? };
                state.ranges[slot_to_use] = None;
            }
        }
        let guest_memfd = match backing {
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
                None
            }
            #[cfg(guest_arch = "x86_64")]
            KvmMemoryBacking::GuestMemfd { file, mapping } => {
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
                        Some((file, mapping.file_offset)),
                    )?;
                };
                Some(mapping)
            }
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range,
            guest_memfd,
        });
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn memory_backing(&self, range: MemoryRange) -> Result<KvmMemoryBacking<'_>, MemoryError> {
        match &self.memory_backing_mode {
            KvmMemoryBackingMode::Userspace => Ok(KvmMemoryBacking::Userspace),
            KvmMemoryBackingMode::GuestMemfd(backing) => {
                match classify_guest_memfd_backing(range, &backing.ranges)? {
                    Some(mapping) => Ok(KvmMemoryBacking::GuestMemfd {
                        file: &backing.files[mapping.file_index],
                        mapping,
                    }),
                    None => Ok(KvmMemoryBacking::Userspace),
                }
            }
        }
    }

    #[cfg(guest_arch = "aarch64")]
    fn memory_backing(&self, _range: MemoryRange) -> Result<KvmMemoryBacking, MemoryError> {
        Ok(KvmMemoryBacking::Userspace)
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

    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn set_guest_memfd_attributes(
        &self,
        range: MemoryRange,
        private: bool,
    ) -> Result<(), MemoryError> {
        let state = self.memory.lock();
        let segments = guest_memfd_range_segments(range, &state.ranges)?;
        let KvmMemoryBackingMode::GuestMemfd(backing) = &self.memory_backing_mode else {
            return Err(MemoryError::InvalidMapGpaRange);
        };
        // Validate every file reference before the first irreversible conversion.
        let conversions = segments
            .iter()
            .map(|segment| {
                let file = backing
                    .files
                    .get(segment.mapping.file_index)
                    .ok_or(MemoryError::InvalidMapGpaRange)?;
                Ok((file, segment))
            })
            .collect::<Result<Vec<_>, MemoryError>>()?;
        let attributes = if private {
            kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
        } else {
            0
        };
        for (file, segment) in conversions {
            kvm::set_guest_memfd_memory_attributes(
                file.as_fd(),
                segment.mapping.file_offset,
                segment.range.len(),
                attributes,
            )
            .map_err(|source| MemoryError::Conversion {
                file_index: segment.mapping.file_index,
                range: segment.range,
                source,
            })?;
        }
        Ok(())
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
    /// state. Any ioctl failure is terminal: earlier segments or pages may
    /// already have changed state.
    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn set_map_gpa_range_attributes(
        &self,
        gpa: u64,
        page_count: u64,
        map_attributes: u64,
    ) -> Result<(), MemoryError> {
        let (range, private) = map_gpa_range_request(gpa, page_count, map_attributes)?;
        tracing::debug!(
            gpa,
            page_count,
            map_attributes,
            private,
            "KVM_HC_MAP_GPA_RANGE set memory attributes"
        );
        self.set_guest_memfd_attributes(range, private)
    }
}

#[cfg(guest_arch = "x86_64")]
fn map_gpa_range_request(
    gpa: u64,
    page_count: u64,
    map_attributes: u64,
) -> Result<(MemoryRange, bool), MemoryError> {
    const PAGE_SIZE_MASK: u64 = 0x3;
    const ENC_STATUS_MASK: u64 = 0xf << 4;
    let size = page_count
        .checked_mul(hvdef::HV_PAGE_SIZE)
        .ok_or(MemoryError::InvalidMapGpaRange)?;
    let end = gpa
        .checked_add(size)
        .ok_or(MemoryError::InvalidMapGpaRange)?;
    if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE) || size == 0 {
        return Err(MemoryError::InvalidMapGpaRange);
    }
    if map_attributes & !(PAGE_SIZE_MASK | ENC_STATUS_MASK) != 0 {
        return Err(MemoryError::UnsupportedMapGpaRangeAttributes(
            map_attributes,
        ));
    }
    let private = match map_attributes & ENC_STATUS_MASK {
        kvm::KVM_MAP_GPA_RANGE_DECRYPTED_UAPI => false,
        kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI => true,
        _ => {
            return Err(MemoryError::UnsupportedMapGpaRangeAttributes(
                map_attributes,
            ));
        }
    };
    Ok((MemoryRange::new(gpa..end), private))
}

#[cfg(guest_arch = "x86_64")]
fn guest_memfd_range_segments(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<Vec<KvmMemoryRangeSegment>, MemoryError> {
    let mut segments = slots
        .iter()
        .flatten()
        .filter_map(|slot| {
            let mapping = slot.guest_memfd?;
            let start = range.start().max(slot.range.start());
            let end = range.end().min(slot.range.end());
            (start < end).then(|| {
                let slot_offset = start - slot.range.start();
                KvmMemoryRangeSegment {
                    range: MemoryRange::new(start..end),
                    mapping: KvmGuestMemfdMapping {
                        file_index: mapping.file_index,
                        file_offset: mapping.file_offset + slot_offset,
                    },
                }
            })
        })
        .collect::<Vec<_>>();
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

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
/// Resolves an imported range to its initially shared guestmemfd mapping.
///
/// The caller must finish host access before converting the range private.
pub(crate) fn private_memory_range_from_slots(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<KvmPrivateMemoryRange, MemoryError> {
    let slot = slots
        .iter()
        .flatten()
        .find(|slot| slot.range.contains(&range))
        .ok_or(MemoryError::InvalidPrivateMemoryRange)?;

    if slot.guest_memfd.is_none() {
        return Err(MemoryError::InvalidPrivateMemoryRange);
    }

    let offset = range.start() - slot.range.start();
    Ok(KvmPrivateMemoryRange {
        gpa: range,
        hva: slot.host_addr.wrapping_add(offset as usize),
    })
}

#[cfg(guest_arch = "x86_64")]
/// Verifies the KVM capabilities required for guestmemfd private memory.
pub(crate) fn check_private_memory_extensions(kvm: &kvm::Partition) -> Result<(), MemoryError> {
    require_kvm_extension(kvm, kvm::KVM_CAP_USER_MEMORY2, "KVM_CAP_USER_MEMORY2")?;
    require_kvm_extension(kvm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;
    let memory_attributes = require_kvm_extension(
        kvm,
        kvm::KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES_UAPI,
        "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES (enable kvm.gmem_in_place_conversion=1)",
    )?;
    if memory_attributes as u64 & kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64 == 0 {
        return Err(kvm::Error::MissingCapability(
            "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES(PRIVATE)",
        )
        .into());
    }
    let flags = require_kvm_extension(
        kvm,
        kvm::KVM_CAP_GUEST_MEMFD_FLAGS_UAPI,
        "KVM_CAP_GUEST_MEMFD_FLAGS",
    )? as u64;
    let required = kvm::GUEST_MEMFD_FLAG_MMAP_UAPI | kvm::GUEST_MEMFD_FLAG_INIT_SHARED_UAPI;
    if flags & required != required {
        return Err(
            kvm::Error::MissingCapability("KVM_CAP_GUEST_MEMFD_FLAGS(MMAP | INIT_SHARED)").into(),
        );
    }
    Ok(())
}

#[cfg(guest_arch = "x86_64")]
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

#[cfg(guest_arch = "x86_64")]
fn classify_guest_memfd_backing(
    range: MemoryRange,
    ram_ranges: &[KvmGuestMemfdRange],
) -> Result<Option<KvmGuestMemfdMapping>, MemoryError> {
    let mut containing_ranges = ram_ranges
        .iter()
        .filter(|ram_range| ram_range.range.contains(&range));
    if let Some(ram_range) = containing_ranges.next() {
        if containing_ranges.next().is_some() {
            return Err(MemoryError::UnsupportedIsolationConfiguration(
                "KVM guest_memfd mappings must be contained in exactly one RAM range",
            ));
        }
        return Ok(Some(KvmGuestMemfdMapping {
            file_index: ram_range.mapping.file_index,
            file_offset: ram_range.mapping.file_offset + (range.start() - ram_range.range.start()),
        }));
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
        for (slot, entry) in state.ranges.iter_mut().enumerate() {
            let Some(kvm_range) = entry else { continue };
            if range.contains(&kvm_range.range) {
                let guest_memfd_backed = kvm_range.guest_memfd.is_some();
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                unsafe { self.clear_slot(slot, guest_memfd_backed)? };
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

    fn range(start: u64, end: u64) -> MemoryRange {
        MemoryRange::new(start..end)
    }

    #[cfg(guest_arch = "x86_64")]
    fn guest_memfd_ranges(ranges: &[MemoryRange]) -> Vec<KvmGuestMemfdRange> {
        let mut file_offset = 0;
        ranges
            .iter()
            .map(|&range| {
                let guest_memfd_range = KvmGuestMemfdRange {
                    range,
                    mapping: KvmGuestMemfdMapping {
                        file_index: 0,
                        file_offset,
                    },
                };
                file_offset += range.len();
                guest_memfd_range
            })
            .collect()
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_selects_contained_ram() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges).unwrap(),
            Some(KvmGuestMemfdMapping {
                file_index: 0,
                file_offset: 0x1000
            })
        );
        assert_eq!(
            classify_guest_memfd_backing(range(0x1_1000, 0x1_3000), &ram_ranges).unwrap(),
            Some(KvmGuestMemfdMapping {
                file_index: 0,
                file_offset: 0x9000
            })
        );
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_keeps_non_ram_userspace() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0xa000, 0xc000), &ram_ranges).unwrap(),
            None
        );
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_rejects_partial_ram_overlap() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x8000, 0xa000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_does_not_merge_adjacent_ram_ranges() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x3000), range(0x3000, 0x5000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[cfg(guest_arch = "x86_64")]
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
            guest_memfd: Some(KvmGuestMemfdMapping {
                file_index: 0,
                file_offset: 0,
            }),
        })];

        let resolved = private_memory_range_from_slots(range(0x3000, 0x5000), &slots).unwrap();

        assert_eq!(resolved.gpa, range(0x3000, 0x5000));
        assert_eq!(resolved.hva, host_addr.wrapping_add(0x2000));
    }

    #[test]
    fn private_memory_range_rejects_non_guest_memfd_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let userspace_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd: None,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &userspace_slots),
            Err(MemoryError::InvalidPrivateMemoryRange)
        ));
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn guest_memfd_segments_cover_adjacent_unordered_slots() {
        let mut first_backing = vec![0u8; 0x2000];
        let mut second_backing = vec![0u8; 0x2000];
        let first_host_addr = first_backing.as_mut_ptr();
        let second_host_addr = second_backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr: second_host_addr,
                range: range(0x3000, 0x5000),
                guest_memfd: Some(KvmGuestMemfdMapping {
                    file_index: 1,
                    file_offset: 0,
                }),
            }),
            Some(KvmMemoryRange {
                host_addr: first_host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd: Some(KvmGuestMemfdMapping {
                    file_index: 0,
                    file_offset: 0,
                }),
            }),
        ];

        let segments = guest_memfd_range_segments(range(0x2000, 0x4000), &slots).unwrap();

        assert_eq!(
            segments,
            [
                KvmMemoryRangeSegment {
                    range: range(0x2000, 0x3000),
                    mapping: KvmGuestMemfdMapping {
                        file_index: 0,
                        file_offset: 0x1000
                    },
                },
                KvmMemoryRangeSegment {
                    range: range(0x3000, 0x4000),
                    mapping: KvmGuestMemfdMapping {
                        file_index: 1,
                        file_offset: 0
                    },
                },
            ]
        );
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn guest_memfd_segments_reject_incomplete_coverage() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let gapped_slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x2000),
                guest_memfd: Some(KvmGuestMemfdMapping {
                    file_index: 0,
                    file_offset: 0,
                }),
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x2000),
                range: range(0x3000, 0x4000),
                guest_memfd: Some(KvmGuestMemfdMapping {
                    file_index: 0,
                    file_offset: 0x2000,
                }),
            }),
        ];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &gapped_slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));

        let userspace_slot = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x4000),
            guest_memfd: None,
        })];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &userspace_slot),
            Err(MemoryError::InvalidMapGpaRange)
        ));
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn guest_memfd_segments_reject_overlapping_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd: Some(KvmGuestMemfdMapping {
                    file_index: 0,
                    file_offset: 0,
                }),
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x1000),
                range: range(0x2000, 0x4000),
                guest_memfd: Some(KvmGuestMemfdMapping {
                    file_index: 0,
                    file_offset: 0x1000,
                }),
            }),
        ];

        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn map_gpa_range_uses_4k_page_counts_for_every_granularity() {
        for granularity in 0..=2 {
            assert_eq!(
                map_gpa_range_request(
                    0x1000,
                    2,
                    kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI | granularity,
                )
                .unwrap(),
                (range(0x1000, 0x3000), true),
            );
            assert_eq!(
                map_gpa_range_request(
                    0x1000,
                    2,
                    kvm::KVM_MAP_GPA_RANGE_DECRYPTED_UAPI | granularity,
                )
                .unwrap(),
                (range(0x1000, 0x3000), false),
            );
        }
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn map_gpa_range_rejects_invalid_guest_requests() {
        for (gpa, count, attributes) in [
            (0, 0, kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI),
            (1, 1, kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI),
            (0, u64::MAX, kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI),
            (u64::MAX - 0xfff, 1, kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI),
            (0, 1, 0xf0),
            (0, 1, 0x30),
            (0, 1, kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI | 4),
            (0, 1, kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI | (1 << 63)),
        ] {
            assert!(map_gpa_range_request(gpa, count, attributes).is_err());
        }
    }
}
