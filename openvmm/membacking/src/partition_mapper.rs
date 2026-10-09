// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the partition mapper, which is responsible for mapping regions
//! into VM partitions.

// UNSAFETY: Calling unsafe partition memory mapping functions.
#![expect(unsafe_code)]

use crate::mapping_manager::VaMapper;
use crate::region_manager::MapParams;
use memory_range::MemoryRange;
use std::sync::Arc;
use std::sync::Weak;
use thiserror::Error;
use virt::PartitionMemoryMap;

/// The partition mapper.
#[derive(Debug)]
pub struct PartitionMapper {
    partition: Weak<dyn PartitionMemoryMap>,
    mapper: Arc<VaMapper>,
    offset: u64,
    pin_mappings: bool,
    retained_backing: bool,
}

/// Failure to map a region.
#[derive(Debug, Error)]
pub enum PartitionMapperError {
    #[error("failed to map range to partition")]
    Map(#[source] anyhow::Error),
    #[error("failed to pin range to partition")]
    Pin(#[source] anyhow::Error),
}

impl PartitionMapper {
    /// Returns a new partition mapper.
    ///
    /// If `pin_mappings`, call [`PartitionMemoryMap::pin_range`] on any region that is mapped.
    ///
    /// `backing` must retain the RAM region handles and this mapper. A partition
    /// that retains it owns teardown instead of unmapping when this object drops.
    pub fn new(
        partition: &Arc<dyn PartitionMemoryMap>,
        mapper: Arc<VaMapper>,
        offset: u64,
        pin_mappings: bool,
        backing: Arc<dyn Send + Sync>,
    ) -> Self {
        assert!(
            mapper.is_eager(),
            "partition mapper requires an eager VaMapper"
        );
        Self {
            retained_backing: partition.retain_memory_backing(backing),
            partition: Arc::downgrade(partition),
            mapper,
            offset,
            pin_mappings,
        }
    }

    /// Maps a region.
    pub async fn map_region(
        &self,
        range: MemoryRange,
        params: MapParams,
    ) -> Result<(), PartitionMapperError> {
        // Ensure this range does not exceed the mapper's reserved VA range.
        assert!(range.end() <= self.mapper.len() as u64);

        // If the partition is gone then there is nothing to do.
        let Some(partition) = self.partition.upgrade() else {
            return Ok(());
        };

        let addr = range.start().checked_add(self.offset).unwrap();
        let size = range.len() as usize;
        let data = self.mapper.as_ptr().wrapping_add(range.start() as usize);

        match self.mapper.process() {
            None => {
                // SAFETY: Mapper will ensure the VA range is reserved (but not
                // necessarily mapped) for its lifetime.
                unsafe { partition.map_range(data, size, addr, params.writable, params.executable) }
            }
            Some(process) => {
                match process {
                    #[cfg(not(windows))]
                    _ => unreachable!(),
                    #[cfg(windows)]
                    process => {
                        // SAFETY: Mapper will ensure the VA range is reserved (but not
                        // necessarily mapped) for its lifetime.
                        unsafe {
                            partition.map_remote_range(
                                process.as_handle(),
                                data,
                                size,
                                addr,
                                params.writable,
                                params.executable,
                            )
                        }
                    }
                }
            }
        }
        .map_err(PartitionMapperError::Map)?;

        if params.prefetch {
            if let Err(err) = partition.prefetch_range(addr, size as u64) {
                tracing::warn!(
                    error = err.as_ref() as &dyn std::error::Error,
                    addr,
                    size,
                    "prefetch failed"
                );
            }
        }

        if self.pin_mappings {
            if let Err(err) = partition.pin_range(addr, size as u64) {
                // Unmap the range to ensure we stay in a consistent state.
                partition
                    .unmap_range(addr, size as u64)
                    .expect("unmap cannot fail");
                return Err(PartitionMapperError::Pin(err));
            }
        }

        Ok(())
    }

    /// Unmaps regions in `range`.
    ///
    /// `range` may overlap zero, one, or many regions that were mapped with
    /// `map_region`, but it must fully contain any regions it overlaps.
    ///
    /// This cannot fail, but on some hypervisors, it may panic on partial
    /// region unmap.
    pub fn unmap_region(&mut self, range: MemoryRange) {
        if let Some(partition) = self.partition.upgrade() {
            partition
                .unmap_range(range.start().checked_add(self.offset).unwrap(), range.len())
                .expect("unmap cannot fail");
        }
    }
}

impl Drop for PartitionMapper {
    fn drop(&mut self) {
        if self.retained_backing {
            return;
        }
        // Ensure everything is unmapped from the partition since the underlying
        // VA is going away.
        self.unmap_region(MemoryRange::new(0..self.mapper.len() as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping_manager::MappingBacking;
    use crate::mapping_manager::MappingManager;
    use crate::mapping_manager::MemoryPolicy;
    use crate::region_manager::MappingType;
    use crate::region_manager::RegionManager;
    use guestmem::GuestMemory;
    use pal_async::task::Spawn;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct DropEvent {
        events: Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
    }

    impl Drop for DropEvent {
        fn drop(&mut self) {
            self.events.lock().push(self.name);
        }
    }

    struct KernelOwner {
        _kernel: DropEvent,
        backing: Mutex<Vec<Arc<dyn Send + Sync>>>,
    }

    struct TestPartition {
        owner: Arc<KernelOwner>,
        retain: bool,
        unmaps: Arc<AtomicUsize>,
    }

    impl PartitionMemoryMap for TestPartition {
        fn retain_memory_backing(&self, backing: Arc<dyn Send + Sync>) -> bool {
            if self.retain {
                self.owner.backing.lock().push(backing);
            }
            self.retain
        }

        unsafe fn map_range(
            &self,
            _data: *mut u8,
            _size: usize,
            _addr: u64,
            _writable: bool,
            _exec: bool,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn unmap_range(&self, _addr: u64, _size: u64) -> anyhow::Result<()> {
            self.unmaps.fetch_add(1, Ordering::Relaxed);
            anyhow::ensure!(!self.retain, "reserved or uncertain kernel mapping");
            Ok(())
        }

        #[cfg(windows)]
        unsafe fn map_remote_range(
            &self,
            _process: std::os::windows::io::BorrowedHandle<'_>,
            _data: *mut u8,
            _size: usize,
            _addr: u64,
            _writable: bool,
            _exec: bool,
        ) -> anyhow::Result<()> {
            unreachable!()
        }
    }

    #[pal_async::async_test]
    async fn retained_ram_survives_mapper_and_partition_until_last_copy_owner(spawn: impl Spawn) {
        let mm = MappingManager::new_without_primary(&spawn, 0x10000, None);
        let mapper = mm.client().new_mapper(true).await.unwrap();
        let regions = RegionManager::new(&spawn, mm.client().clone());
        let region = regions
            .client()
            .new_region(
                "ram".into(),
                MemoryRange::new(0..0x1000),
                0,
                MappingType::Ram,
            )
            .await
            .unwrap();
        region
            .add_mapping(
                MemoryRange::new(0..0x1000),
                MappingBacking::Private,
                true,
                MemoryPolicy::none(),
            )
            .await
            .unwrap();
        region
            .map(MapParams {
                writable: true,
                executable: false,
                prefetch: false,
            })
            .await
            .unwrap();
        let gm = GuestMemory::new("retained", mapper.clone());
        gm.write_plain(0, &42u64).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let owner = Arc::new(KernelOwner {
            _kernel: DropEvent {
                events: events.clone(),
                name: "kernel closed",
            },
            backing: Mutex::new(Vec::new()),
        });
        let unmaps = Arc::new(AtomicUsize::new(0));
        let partition: Arc<dyn PartitionMemoryMap> = Arc::new(TestPartition {
            owner: owner.clone(),
            retain: true,
            unmaps: unmaps.clone(),
        });
        let backing = Arc::new((
            region,
            mapper.clone(),
            DropEvent {
                events: events.clone(),
                name: "backing released",
            },
        ));
        let backing_weak = Arc::downgrade(&backing);
        let partition_mapper = PartitionMapper::new(&partition, mapper, 0, false, backing);
        assert!(partition.unmap_range(0, 0x1000).is_err());
        assert_eq!(unmaps.load(Ordering::Relaxed), 1);
        drop(partition_mapper);
        assert_eq!(unmaps.load(Ordering::Relaxed), 1);
        drop(partition);
        drop(regions);
        drop(mm);
        assert!(backing_weak.upgrade().is_some());
        assert!(events.lock().is_empty());
        assert_eq!(gm.read_plain::<u64>(0).unwrap(), 42);
        // This is the last owner held by a copy guard, not the partition.
        drop(owner);
        assert!(backing_weak.upgrade().is_none());
        assert_eq!(*events.lock(), ["kernel closed", "backing released"]);
    }

    #[pal_async::async_test]
    async fn ordinary_mapper_still_unmaps_on_drop(spawn: impl Spawn) {
        let mm = MappingManager::new_without_primary(&spawn, 0x10000, None);
        let mapper = mm.client().new_mapper(true).await.unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let unmaps = Arc::new(AtomicUsize::new(0));
        let partition: Arc<dyn PartitionMemoryMap> = Arc::new(TestPartition {
            owner: Arc::new(KernelOwner {
                _kernel: DropEvent {
                    events: events.clone(),
                    name: "kernel closed",
                },
                backing: Mutex::new(Vec::new()),
            }),
            retain: false,
            unmaps: unmaps.clone(),
        });
        let backing = Arc::new(DropEvent {
            events: events.clone(),
            name: "unused backing released",
        });
        drop(PartitionMapper::new(&partition, mapper, 0, false, backing));
        assert_eq!(unmaps.load(Ordering::Relaxed), 1);
        assert_eq!(*events.lock(), ["unused backing released"]);
        drop(partition);
        assert_eq!(*events.lock(), ["unused backing released", "kernel closed"]);
    }

    #[pal_async::async_test]
    async fn attached_file_backing_survives_memory_manager_drop() {
        use crate::memory_manager::GuestMemoryBuilder;
        use crate::memory_manager::RamBackingRequest;

        let mut manager = GuestMemoryBuilder::new()
            .add_backing(RamBackingRequest::new(vec![MemoryRange::new(0..0x10000)]))
            .build(0x10000)
            .await
            .unwrap();
        let gm = manager.client().guest_memory().await.unwrap();
        gm.write_plain(0x1000, &123u64).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let owner = Arc::new(KernelOwner {
            _kernel: DropEvent {
                events: events.clone(),
                name: "kernel closed",
            },
            backing: Mutex::new(Vec::new()),
        });
        let unmaps = Arc::new(AtomicUsize::new(0));
        let partition: Arc<dyn PartitionMemoryMap> = Arc::new(TestPartition {
            owner: owner.clone(),
            retain: true,
            unmaps: unmaps.clone(),
        });
        manager
            .attach_partition(hvdef::Vtl::Vtl0, &partition, None, None)
            .await
            .unwrap();
        assert_eq!(owner.backing.lock().len(), 1);
        let backing = Arc::downgrade(&owner.backing.lock()[0]);
        drop(manager);
        drop(partition);
        assert!(backing.upgrade().is_some());
        assert_eq!(unmaps.load(Ordering::Relaxed), 0);
        assert_eq!(gm.read_plain::<u64>(0x1000).unwrap(), 123);
        gm.write_plain(0x1000, &456u64).unwrap();
        assert_eq!(gm.read_plain::<u64>(0x1000).unwrap(), 456);
        assert!(events.lock().is_empty());
        drop(owner);
        assert!(backing.upgrade().is_none());
        assert_eq!(*events.lock(), ["kernel closed"]);
    }
}
