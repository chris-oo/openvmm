// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device guest_memfd leases, separate from RAM. A PCI enable-bit change uses
//! checked sharing and provider release before the real VFIO command write.
//! Shared backing is then reinstalled for the guest's later RIPAS_EMPTY/UNLOCK.

use crate::KvmPartitionInner;
use crate::cca_in_place::CcaInPlaceError;
use crate::memory::KvmMemoryRange;
use crate::memory::KvmMemoryRangeState;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use pci_core::vfio::VfioDeviceBar;
use pci_core::vfio::VfioDeviceMemory;
use pci_core::vfio::VfioVmError;
use sparse_mmap::SparseMapping;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid device guest_memfd lease")]
    Lease,
    #[error("device guest_memfd operation failed")]
    Kvm(#[from] kvm::Error),
    #[error("device provider I/O failed")]
    Io(#[from] std::io::Error),
    #[error("device provider mapping failed")]
    Mapping(#[from] sparse_mmap::SparseMappingError),
    #[error("device provider range failed")]
    Range(#[from] CcaInPlaceError),
}

struct Bar {
    description: VfioDeviceBar,
    mapping: Option<SparseMapping>,
    slot: Option<usize>,
    installed: bool,
    intercepted: bool,
}

struct State {
    resource: Option<File>,
    file: Option<File>,
    bars: Vec<Bar>,
    private: Vec<MemoryRange>,
}

pub(crate) struct DeviceProvider {
    partition: Arc<KvmPartitionInner>,
    rid: u32,
    config_offset: u64,
    nonsecure: Vec<MemoryRange>,
    state: Mutex<State>,
}

impl std::fmt::Debug for DeviceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceProvider")
            .field("rid", &self.rid)
            .finish_non_exhaustive()
    }
}

fn provider_bars(
    descriptions: &[VfioDeviceBar],
    nonsecure: &[MemoryRange],
    shared_gpa_bit: u64,
) -> Result<Vec<Bar>, Error> {
    let mut ranges = Vec::new();
    for description in descriptions {
        let size = description
            .guest
            .end
            .checked_sub(description.guest.start)
            .ok_or(Error::Lease)?;
        let range = crate::cca_in_place::checked_range(description.guest.start, size)?;
        crate::cca_in_place::checked_range(description.host, size)?;
        crate::cca_in_place::checked_range(description.file_offset, size)?;
        if description.file_offset >> 40 >= 6
            || (description.file_offset & ((1 << 40) - 1))
                .checked_add(size)
                .ok_or(Error::Lease)?
                > 1 << 40
            || ranges.iter().any(|r: &MemoryRange| r.overlaps(&range))
            || range.end() > shared_gpa_bit
        {
            return Err(Error::Lease);
        }
        ranges.push(range);
    }
    for range in nonsecure {
        crate::cca_in_place::checked_range(range.start(), range.len())?;
        if !ranges.iter().any(|bar| bar.contains(range)) {
            return Err(Error::Lease);
        }
    }
    let mut bars = Vec::new();
    for (description, range) in descriptions.iter().zip(ranges) {
        let mut exclusions = nonsecure
            .iter()
            .filter(|r| range.contains(r))
            .copied()
            .collect::<Vec<_>>();
        exclusions.sort_by_key(MemoryRange::start);
        // Table and PBA can share a page. Union exclusions within each BAR.
        let exclusions = memory_range::flatten_ranges(exclusions).map(|r| (r, ()));
        for (segment, side) in memory_range::walk_ranges([(range, ())], exclusions) {
            let intercepted = match side {
                memory_range::RangeWalkResult::Left(()) => false,
                memory_range::RangeWalkResult::Both((), ()) => true,
                _ => continue,
            };
            let displacement = segment.start() - range.start();
            bars.push(Bar {
                description: VfioDeviceBar {
                    guest: segment.start()..segment.end(),
                    host: description
                        .host
                        .checked_add(displacement)
                        .ok_or(Error::Lease)?,
                    file_offset: description
                        .file_offset
                        .checked_add(displacement)
                        .ok_or(Error::Lease)?,
                },
                mapping: None,
                slot: None,
                installed: false,
                intercepted,
            });
        }
    }
    Ok(bars)
}

pub(crate) fn create(
    partition: Arc<KvmPartitionInner>,
    rid: u32,
    resource: BorrowedFd<'_>,
    config_offset: u64,
    descriptions: Vec<VfioDeviceBar>,
    nonsecure: Vec<std::ops::Range<u64>>,
) -> Result<Arc<dyn VfioDeviceMemory>, VfioVmError> {
    let prepare = || -> Result<DeviceProvider, Error> {
        if partition.caps.isolation != virt::IsolationType::Cca
            || !partition.memory_backing_mode.is_in_place()
            || descriptions.is_empty()
        {
            return Err(Error::Lease);
        }
        let nonsecure = nonsecure
            .iter()
            .map(|r| {
                let size = r.end.checked_sub(r.start).ok_or(Error::Lease)?;
                Ok(crate::cca_in_place::checked_range(r.start, size)?)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let bars = provider_bars(
            &descriptions,
            &nonsecure,
            partition.shared_gpa_bit.ok_or(Error::Lease)?,
        )?;
        Ok(DeviceProvider {
            partition: partition.clone(),
            rid,
            config_offset,
            nonsecure,
            state: Mutex::new(State {
                resource: Some(resource.try_clone_to_owned()?.into()),
                file: None,
                bars,
                private: Vec::new(),
            }),
        })
    };
    let provider = Arc::new(prepare().map_err(VfioVmError::new)?);
    let result = (|| -> Result<(), Error> {
        let mut memory = partition.memory.lock();
        let mut state = provider.state.lock();
        for bar in &mut state.bars {
            let range = MemoryRange::new(bar.description.guest.clone());
            if partition.ram_ranges.iter().any(|r| r.overlaps(&range))
                || memory
                    .ranges
                    .iter()
                    .flatten()
                    .any(|r| r.range.overlaps(&range))
            {
                return Err(Error::Lease);
            }
            if bar.intercepted {
                continue;
            }
            bar.slot = Some(memory.ranges.len());
            memory.ranges.push(Some(KvmMemoryRange {
                range,
                host_addr: std::ptr::null_mut(),
                guest_memfd_offset: None,
                private_state: None,
            }));
        }
        provider.attach(&mut state, &mut memory)?;
        memory.device_providers.push(Arc::downgrade(&provider));
        Ok(())
    })();
    if let Err(error) = result {
        partition.mark_cca_fatal();
        // A slot ioctl can commit before its completion is lost.
        std::mem::forget(provider);
        return Err(VfioVmError::new(error));
    }
    Ok(provider)
}

impl DeviceProvider {
    fn attach(&self, state: &mut State, memory: &mut KvmMemoryRangeState) -> Result<(), Error> {
        if state.file.is_some() || !state.private.is_empty() {
            return Err(Error::Lease);
        }
        let size = state
            .bars
            .iter()
            .map(|b| {
                b.description.file_offset + b.description.guest.end - b.description.guest.start
            })
            .max()
            .ok_or(Error::Lease)?;
        state.file = Some(self.partition.kvm.create_device_guest_memfd(
            size,
            state.resource.as_ref().ok_or(Error::Lease)?.as_fd(),
        )?);
        for bar in &mut state.bars {
            let range = MemoryRange::new(bar.description.guest.clone());
            let length = usize::try_from(range.len()).map_err(|_| Error::Lease)?;
            let mapping = SparseMapping::new(length)?;
            mapping.map_file(
                0,
                length,
                state.file.as_ref().ok_or(Error::Lease)?,
                bar.description.file_offset,
                true,
            )?;
            let address = mapping.as_ptr().cast::<u8>();
            bar.mapping = Some(mapping);
            if bar.intercepted {
                continue;
            }
            let slot = bar.slot.ok_or(Error::Lease)?;
            bar.installed = true;
            memory.ranges[slot] = Some(KvmMemoryRange {
                range,
                host_addr: address,
                guest_memfd_offset: None,
                private_state: None,
            });
            // SAFETY: record the VMA, file and possibly installed slot before
            // issuing the ioctl; all remain retained on failure.
            unsafe {
                self.partition.kvm.set_user_memory_region2(
                    u32::try_from(slot).map_err(|_| Error::Lease)?,
                    address,
                    length,
                    range.start(),
                    false,
                    Some((
                        state.file.as_ref().ok_or(Error::Lease)?,
                        bar.description.file_offset,
                    )),
                )?;
            }
        }
        Ok(())
    }

    fn covers(&self, range: MemoryRange) -> bool {
        self.state
            .lock()
            .bars
            .iter()
            .any(|b| b.installed && MemoryRange::new(b.description.guest.clone()).contains(&range))
    }

    fn convert(
        &self,
        state: &mut State,
        range: MemoryRange,
        private: bool,
        pa: Option<u64>,
    ) -> Result<(), Error> {
        let bar = state
            .bars
            .iter()
            .find(|b| b.installed && MemoryRange::new(b.description.guest.clone()).contains(&range))
            .ok_or(Error::Lease)?;
        let displacement = range.start() - bar.description.guest.start;
        let offset = bar.description.file_offset + displacement;
        if private {
            crate::cca_in_place::validate_device_lease(
                range,
                pa.ok_or(Error::Lease)?,
                MemoryRange::new(bar.description.guest.clone()),
                bar.description.host,
                &self.nonsecure,
                &state.private,
            )?;
            state.private.push(range);
            state.private.sort_by_key(MemoryRange::start);
        }
        let mut attributes = kvm::KvmMemoryAttributes2 {
            offset,
            size: range.len(),
            attributes: if private {
                kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
            } else {
                0
            },
            ..Default::default()
        };
        kvm::set_guest_memfd_memory_attributes(
            state.file.as_ref().ok_or(Error::Lease)?.as_fd(),
            &mut attributes,
        )?;
        if !private {
            state.private =
                memory_range::walk_ranges(state.private.iter().map(|r| (*r, ())), [(range, ())])
                    .filter_map(|(r, side)| {
                        matches!(side, memory_range::RangeWalkResult::Left(())).then_some(r)
                    })
                    .collect();
        }
        Ok(())
    }

    fn release(&self, state: &mut State, memory: &mut KvmMemoryRangeState) -> Result<(), Error> {
        if !state.private.is_empty() {
            return Err(Error::Lease);
        }
        for bar in &mut state.bars {
            if bar.installed {
                let slot = bar.slot.ok_or(Error::Lease)?;
                // SAFETY: zero size removes only this recorded memslot.
                unsafe {
                    self.partition.kvm.set_user_memory_region2(
                        u32::try_from(slot).map_err(|_| Error::Lease)?,
                        std::ptr::null_mut(),
                        0,
                        0,
                        false,
                        None,
                    )?;
                }
                bar.installed = false;
                memory.ranges[slot] = None;
            }
            bar.mapping = None;
        }
        // Slots are SRCU-retired and every host VMA is gone before the last
        // guest_memfd reference is released. The VFIO/vDEVICE/VM remain owned.
        state.file = None;
        Ok(())
    }

    fn access<R>(
        state: &State,
        offset: u64,
        size: usize,
        work: impl FnOnce(&SparseMapping, usize) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        if ![1, 2, 4, 8].contains(&size) {
            return Err(std::io::Error::other("invalid BAR transfer size"));
        }
        let end = offset
            .checked_add(size as u64)
            .ok_or_else(|| std::io::Error::other("BAR overflow"))?;
        let bar = state
            .bars
            .iter()
            .find(|b| {
                b.mapping.is_some()
                    && offset >= b.description.file_offset
                    && end
                        <= b.description.file_offset + b.description.guest.end
                            - b.description.guest.start
            })
            .ok_or_else(|| std::io::Error::other("unknown provider BAR"))?;
        let displacement = offset - bar.description.file_offset;
        let start = bar.description.guest.start + displacement;
        let end = start + size as u64;
        let index = usize::try_from(displacement)
            .map_err(|_| std::io::Error::other("BAR displacement overflow"))?;
        if !index.is_multiple_of(size)
            || state
                .private
                .iter()
                .any(|r| start < r.end() && r.start() < end)
        {
            return Err(std::io::Error::other("private or invalid BAR transfer"));
        }
        work(
            bar.mapping
                .as_ref()
                .ok_or_else(|| std::io::Error::other("provider detached"))?,
            index,
        )
    }
}

impl VfioDeviceMemory for DeviceProvider {
    fn read(&self, offset: u64, data: &mut [u8]) -> std::io::Result<usize> {
        Self::access(&self.state.lock(), offset, data.len(), |mapping, index| {
            let bytes = match data.len() {
                1 => vec![
                    mapping
                        .read_volatile::<u8>(index)
                        .map_err(std::io::Error::other)?,
                ],
                2 => mapping
                    .read_volatile::<u16>(index)
                    .map_err(std::io::Error::other)?
                    .to_le_bytes()
                    .to_vec(),
                4 => mapping
                    .read_volatile::<u32>(index)
                    .map_err(std::io::Error::other)?
                    .to_le_bytes()
                    .to_vec(),
                8 => mapping
                    .read_volatile::<u64>(index)
                    .map_err(std::io::Error::other)?
                    .to_le_bytes()
                    .to_vec(),
                _ => return Err(std::io::Error::other("invalid BAR transfer")),
            };
            data.copy_from_slice(&bytes);
            Ok(data.len())
        })
    }

    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<usize> {
        Self::access(&self.state.lock(), offset, data.len(), |mapping, index| {
            let invalid = || std::io::Error::other("invalid BAR transfer");
            match data.len() {
                1 => mapping.write_volatile(index, &data[0]),
                2 => mapping.write_volatile(
                    index,
                    &u16::from_le_bytes(data.try_into().map_err(|_| invalid())?),
                ),
                4 => mapping.write_volatile(
                    index,
                    &u32::from_le_bytes(data.try_into().map_err(|_| invalid())?),
                ),
                8 => mapping.write_volatile(
                    index,
                    &u64::from_le_bytes(data.try_into().map_err(|_| invalid())?),
                ),
                _ => return Err(invalid()),
            }
            .map_err(std::io::Error::other)?;
            Ok(data.len())
        })
    }

    fn write_command(&self, byte_offset: u8, data: &[u8]) -> Result<(), VfioVmError> {
        let result = (|| -> Result<(), Error> {
            let mut memory = self.partition.memory.lock();
            let mut state = self.state.lock();
            if self
                .partition
                .cca_fatal
                .load(std::sync::atomic::Ordering::Acquire)
                || data.is_empty()
                || usize::from(byte_offset) + data.len() > 4
            {
                return Err(Error::Lease);
            }
            let mut original = [0; 4];
            let resource = state.resource.as_ref().ok_or(Error::Lease)?;
            if resource.read_at(&mut original, self.config_offset + 4)? != 4 {
                return Err(Error::Lease);
            }
            let mut requested = original;
            requested[usize::from(byte_offset)..usize::from(byte_offset) + data.len()]
                .copy_from_slice(data);
            let rebind = crate::cca_in_place::device_command_needs_rebind(
                original[0],
                requested[0],
                !state.private.is_empty(),
            )?;
            crate::cca_in_place::execute_device_command(rebind, |stage| -> Result<(), Error> {
                use crate::cca_in_place::DeviceCommandStage;
                match stage {
                    DeviceCommandStage::Share => {
                        for range in state.private.clone() {
                            self.convert(&mut state, range, false, None)?;
                        }
                    }
                    DeviceCommandStage::Release => self.release(&mut state, &mut memory)?,
                    DeviceCommandStage::Write => {
                        let resource = state.resource.as_ref().ok_or(Error::Lease)?;
                        if resource
                            .write_at(data, self.config_offset + 4 + u64::from(byte_offset))?
                            != data.len()
                        {
                            return Err(Error::Lease);
                        }
                        let mut actual = [0; 2];
                        if resource.read_at(&mut actual, self.config_offset + 4)? != 2
                            || (actual[0] ^ requested[0]) & 6 != 0
                        {
                            return Err(Error::Lease);
                        }
                    }
                    DeviceCommandStage::Attach => self.attach(&mut state, &mut memory)?,
                }
                Ok(())
            })?;
            Ok(())
        })();
        if result.is_err() {
            self.partition.mark_cca_fatal();
        }
        result.map_err(VfioVmError::new)
    }

    fn close(&self) -> Result<(), VfioVmError> {
        let mut memory = self.partition.memory.lock();
        let mut state = self.state.lock();
        if self
            .partition
            .cca_fatal
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(VfioVmError::new(Error::Lease));
        }
        self.release(&mut state, &mut memory)
            .map_err(VfioVmError::new)?;
        state.resource = None;
        Ok(())
    }

    fn prepare_lock(&self) -> Result<(), VfioVmError> {
        let mut command = [0; 2];
        {
            let state = self.state.lock();
            if !state.private.is_empty() {
                return Err(VfioVmError::new(Error::Lease));
            }
            let resource = state
                .resource
                .as_ref()
                .ok_or_else(|| VfioVmError::new(Error::Lease))?;
            if resource
                .read_at(&mut command, self.config_offset + 4)
                .map_err(VfioVmError::new)?
                != 2
            {
                return Err(VfioVmError::new(Error::Lease));
            }
        }
        command[0] |= 6;
        self.write_command(0, &command)
    }
}

impl Drop for DeviceProvider {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        if state.resource.is_some() || state.file.is_some() {
            tracelimit::error_ratelimited!("unclosed device provider; retaining backing and VM");
            let retained = std::mem::replace(
                state,
                State {
                    resource: None,
                    file: None,
                    bars: Vec::new(),
                    private: Vec::new(),
                },
            );
            std::mem::forget((retained, self.partition.clone()));
        }
    }
}

pub(crate) fn approve_mapping(
    partition: &KvmPartitionInner,
    memory: &mut KvmMemoryRangeState,
    request: crate::rhi::MappingRequest,
) -> Result<(), crate::KvmError> {
    if !memory.assignment_prepared_for_device()
        || partition
            .cca_fatal
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(Error::Lease.into());
    }
    let provider = memory
        .device_providers
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .find(|p| p.rid == request.rid && p.covers(request.range))
        .ok_or(Error::Lease)?;
    memory.protected_attempts.record(
        request.range,
        request.pa,
        partition.shared_gpa_bit.ok_or(Error::Lease)?,
    )?;
    let result = provider.convert(
        &mut provider.state.lock(),
        request.range,
        true,
        Some(request.pa),
    );
    if result.is_err() {
        partition.mark_cca_fatal();
    }
    result.map_err(Into::into)
}

pub(crate) fn handle_fault(
    memory: &KvmMemoryRangeState,
    range: MemoryRange,
    flags: u64,
    tracker: &mut crate::cca_in_place::FaultTracker,
    successful_exit: bool,
) -> Result<bool, crate::KvmError> {
    let Some(provider) = memory
        .device_providers
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .find(|p| p.covers(range))
    else {
        return Ok(false);
    };
    if flags != 0 {
        return Err(Error::Lease.into());
    }
    let mut state = provider.state.lock();
    if state.private.iter().any(|r| r.overlaps(&range)) {
        provider.convert(&mut state, range, false, None)?;
    } else {
        tracker.observe_device(
            crate::cca_in_place::Fault {
                gpa: range.start(),
                size: range.len(),
                flags,
            },
            successful_exit,
        )?;
    }
    Ok(true)
}

pub(crate) fn all_shared(memory: &KvmMemoryRangeState) -> bool {
    memory.device_providers.iter().all(|weak| {
        weak.upgrade()
            .is_some_and(|p| p.state.lock().private.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn range(start: u64, end: u64) -> MemoryRange {
        MemoryRange::new(start..end)
    }

    #[test]
    fn shared_msix_pages_split_bars_without_changing_backing_offsets() {
        let bars = provider_bars(
            &[VfioDeviceBar {
                guest: 0x1000..0x6000,
                host: 0x9000,
                file_offset: 0x1000,
            }],
            &[
                range(0x3000, 0x4000),
                range(0x2000, 0x3000),
                range(0x2000, 0x4000),
            ],
            1 << 47,
        )
        .unwrap();
        let layout = bars
            .iter()
            .map(|bar| {
                (
                    bar.description.guest.clone(),
                    bar.description.host,
                    bar.description.file_offset,
                    bar.intercepted,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            layout,
            [
                (0x1000..0x2000, 0x9000, 0x1000, false),
                (0x2000..0x4000, 0xa000, 0x2000, true),
                (0x4000..0x6000, 0xc000, 0x4000, false),
            ]
        );
        assert!(bars.iter().all(|bar| bar.slot.is_none() && !bar.installed));
    }

    #[test]
    fn adjacent_exclusions_stay_with_their_original_bars() {
        let bars = provider_bars(
            &[
                VfioDeviceBar {
                    guest: 0x1000..0x3000,
                    host: 0x9000,
                    file_offset: 0,
                },
                VfioDeviceBar {
                    guest: 0x3000..0x5000,
                    host: 0xb000,
                    file_offset: 1 << 40,
                },
            ],
            &[range(0x2000, 0x3000), range(0x3000, 0x4000)],
            1 << 47,
        )
        .unwrap();
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[1].description.guest, 0x2000..0x3000);
        assert_eq!(bars[2].description.guest, 0x3000..0x4000);
        assert!(bars[1].intercepted && bars[2].intercepted);
        assert_eq!(bars[1].description.file_offset, 0x1000);
        assert_eq!(bars[2].description.file_offset, 1 << 40);
    }

    #[test]
    fn fully_intercepted_bar_supports_host_byte_access_without_a_guest_slot() {
        let mut bars = provider_bars(
            &[VfioDeviceBar {
                guest: 0x1000..0x2000,
                host: 0x9000,
                file_offset: 0,
            }],
            &[range(0x1000, 0x2000)],
            1 << 47,
        )
        .unwrap();
        assert!(bars[0].intercepted);
        bars[0].mapping = Some(SparseMapping::new(4096).unwrap());
        let mut state = State {
            resource: None,
            file: None,
            bars,
            private: Vec::new(),
        };
        for size in [1, 2, 4, 8] {
            assert_eq!(
                DeviceProvider::access(&state, 16, size, |_, index| Ok(index)).unwrap(),
                16
            );
        }
        assert!(
            DeviceProvider::access::<()>(&state, 1, 2, |_, _| panic!("unaligned access")).is_err()
        );
        assert!(
            DeviceProvider::access::<()>(&state, 4094, 4, |_, _| panic!("cross-BAR access"))
                .is_err()
        );
        state.private.push(range(0x1000, 0x2000));
        assert!(
            DeviceProvider::access::<()>(&state, 16, 4, |_, _| panic!("private access")).is_err()
        );
    }

    #[test]
    fn invalid_bar_and_exclusion_layouts_fail_before_mapping() {
        let description = VfioDeviceBar {
            guest: 0x1000..0x3000,
            host: 0x9000,
            file_offset: 0,
        };
        for exclusion in [
            range(0, 0x2000),
            range(0x2000, 0x4000),
            range(0x1000, 0x1000),
        ] {
            assert!(
                provider_bars(std::slice::from_ref(&description), &[exclusion], 1 << 47).is_err()
            );
        }
        let overlapping = VfioDeviceBar {
            guest: 0x2000..0x4000,
            host: 0xb000,
            file_offset: 1 << 40,
        };
        assert!(provider_bars(&[description.clone(), overlapping], &[], 1 << 47).is_err());
        let crossing_region = VfioDeviceBar {
            file_offset: (1 << 40) - 4096,
            ..description
        };
        assert!(provider_bars(&[crossing_region], &[], 1 << 47).is_err());
    }
}
