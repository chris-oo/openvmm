// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: required for mapping memory to a partition.
#![expect(unsafe_code)]

//! Guest memory access via `/dev/mem`.
//!
//! This is used in the KVM nested virtualization configuration, where the
//! `mshv_vtl` driver is not available and guest memory is accessed through
//! the standard Linux `/dev/mem` interface.

use anyhow::Context;
use guestmem::GuestMemory;
use inspect::Inspect;
use std::sync::Arc;
use std::sync::Weak;
use vm_topology::memory::MemoryLayout;

/// Guest OS memory accessed via `/dev/mem`.
///
/// This is less efficient than using [`MemoryMappings`](crate::MemoryMappings),
/// but it works without the `mshv_vtl` driver and is suitable for the KVM
/// nested virtualization configuration.
#[derive(Inspect)]
pub struct DevMemMemory {
    #[inspect(skip)]
    mapping: Arc<sparse_mmap::SparseMapping>,
    #[inspect(skip)]
    mem: GuestMemory,
    layout: MemoryLayout,
    #[inspect(skip)]
    partition: Option<Weak<dyn virt::PartitionMemoryMap>>,
}

impl DevMemMemory {
    /// Opens and maps ranges from `/dev/mem`.
    pub fn new(layout: &MemoryLayout) -> anyhow::Result<Self> {
        let mmap = sparse_mmap::SparseMapping::new(layout.end_of_ram() as usize)
            .context("failed to allocate VA space")?;
        let ram = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mem")?;
        for range in layout.ram() {
            let range = range.range;
            mmap.map_file(
                range.start() as usize,
                range.len() as usize,
                ram.file(),
                range.start(),
                true,
            )
            .with_context(|| format!("failed to map range {range}"))?;
        }
        let mmap = Arc::new(mmap);
        Ok(Self {
            mapping: mmap.clone(),
            mem: GuestMemory::new("guest", mmap),
            layout: layout.clone(),
            partition: None,
        })
    }

    /// Returns the guest memory object for VTL0.
    pub fn vtl0(&self) -> &GuestMemory {
        &self.mem
    }

    /// Maps the guest memory into the given partition.
    pub fn map_partition(
        &mut self,
        partition: &dyn virt::PartitionMemoryMapper,
    ) -> anyhow::Result<()> {
        let map = partition.memory_mapper(hvdef::Vtl::Vtl0);
        for range in self.layout.ram() {
            let range = range.range;
            // SAFETY: the VA range is valid until `drop`, which unmaps the
            // memory from the partition.
            unsafe {
                map.map_range(
                    self.mapping
                        .as_ptr()
                        .byte_add(range.start() as usize)
                        .cast(),
                    range.len() as usize,
                    range.start(),
                    true,
                    true,
                )
                .with_context(|| format!("failed to map range {range}"))?;
            }
        }
        self.partition = Some(Arc::downgrade(&map));
        Ok(())
    }
}

impl Drop for DevMemMemory {
    fn drop(&mut self) {
        let Some(partition) = self.partition.take().and_then(|p| p.upgrade()) else {
            return;
        };
        for range in self.layout.ram() {
            let range = range.range;
            partition.unmap_range(range.start(), range.len()).unwrap();
        }
    }
}
