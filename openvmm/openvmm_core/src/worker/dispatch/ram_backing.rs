// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(target_os = "linux")]
use anyhow::Context;
use membacking::GuestMemoryBuilder;
use membacking::Mappable;
use membacking::RamBackingRequest;
use membacking::SharedMemoryBacking;
use memory_range::MemoryRange;
use openvmm_defs::config::MemoryConfig;

/// Selects one exclusive source for userspace RAM.
pub(super) enum RamBackingSource {
    Ordinary(Option<Mappable>),
    #[cfg(target_os = "linux")]
    Partition(virt::MappableRamBacking),
}

impl RamBackingSource {
    pub(super) fn new(
        restored: Option<SharedMemoryBacking>,
        nodes_with_ranges: usize,
        #[cfg(target_os = "linux")] partition: Option<virt::MappableRamBacking>,
        #[cfg(target_os = "linux")] layout: &vm_topology::memory::MemoryLayout,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        if let Some(partition) = partition {
            anyhow::ensure!(
                restored.is_none(),
                "partition-supplied RAM backing does not support restore"
            );
            partition
                .validate_layout(layout)
                .context("invalid partition RAM backing")?;
            return Ok(Self::Partition(partition));
        }
        if restored.is_some() && nodes_with_ranges > 1 {
            anyhow::bail!(
                "shared memory restore not supported with {nodes_with_ranges} memory nodes"
            );
        }
        Ok(Self::Ordinary(
            restored.map(SharedMemoryBacking::into_mappable),
        ))
    }

    pub(super) fn add_node(
        &mut self,
        mut builder: GuestMemoryBuilder,
        vnode: usize,
        ranges: Vec<MemoryRange>,
        mem: &MemoryConfig,
    ) -> anyhow::Result<GuestMemoryBuilder> {
        if let Some(size) = mem.hugepage_size
            && !mem.hugepages
        {
            anyhow::bail!("node {vnode}: hugepage_size={size} requires hugepages=on");
        }

        let request = |ranges| {
            let mut backing = RamBackingRequest::new(ranges)
                .prefetch(mem.prefetch_memory)
                .private_memory(mem.private_memory)
                .transparent_hugepages(mem.transparent_hugepages)
                .host_numa_node(mem.host_numa_node);
            if mem.hugepages {
                backing = backing.hugepages(mem.hugepage_size);
            }
            backing
        };
        match self {
            Self::Ordinary(restored) => {
                let mut backing = request(ranges);
                if let Some(mappable) = restored.take() {
                    backing = backing.existing_mappable(mappable);
                }
                builder = builder.add_backing(backing);
            }
            #[cfg(target_os = "linux")]
            Self::Partition(partition) => {
                anyhow::ensure!(
                    !mem.private_memory && !mem.hugepages,
                    "node {vnode}: partition-supplied RAM backing is incompatible with private or hugetlb memory"
                );
                // A node's ranges need not be consecutive in the backend's
                // packed file (for example, interleaved NUMA ranges).
                for range in ranges {
                    let offset = partition
                        .file_offset(range)
                        .with_context(|| format!("node {vnode}: invalid partition RAM slice"))?;
                    let file = partition
                        .file()
                        .try_clone()
                        .context("failed to clone partition RAM file")?;
                    let fd = std::os::fd::OwnedFd::from(file);
                    builder = builder
                        .add_backing(request(vec![range]).existing_mappable_at(fd.into(), offset));
                }
            }
        }
        Ok(builder)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use pal_async::async_test;
    use std::fs::File;
    use std::os::unix::fs::FileExt;
    use test_with_tracing::test;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::memory::MemoryRangeWithNode;

    fn page_size() -> u64 {
        sparse_mmap::SparseMapping::page_size().try_into().unwrap()
    }

    fn memory_config() -> MemoryConfig {
        MemoryConfig {
            mem_size: 2 * page_size(),
            prefetch_memory: false,
            private_memory: false,
            transparent_hugepages: false,
            hugepages: false,
            hugepage_size: None,
            host_numa_node: None,
        }
    }

    fn layout() -> MemoryLayout {
        let page = page_size();
        MemoryLayout::new_from_resolved_ranges(
            vec![
                MemoryRangeWithNode {
                    range: MemoryRange::new(page..2 * page),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(4 * page..5 * page),
                    vnode: 1,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(8 * page..9 * page),
                    vnode: 0,
                },
            ],
            vec![],
            vec![],
            vec![],
            Some(MemoryRange::new(16 * page..17 * page)),
        )
        .unwrap()
    }

    fn backing(layout: &MemoryLayout) -> virt::MappableRamBacking {
        virt::MappableRamBacking::new(
            sparse_mmap::alloc_shared_memory(
                4 * sparse_mmap::SparseMapping::page_size(),
                "worker-ram-test",
            )
            .unwrap()
            .into(),
            layout,
        )
        .unwrap()
    }

    #[async_test]
    async fn supplied_backing_preserves_interleaved_numa_and_vtl2_offsets() {
        let page = page_size();
        let layout = layout();
        let backing = backing(&layout);
        let file: File = backing.file().try_clone().unwrap();
        let mut source = RamBackingSource::new(None, 2, Some(backing), &layout).unwrap();
        let mut builder = GuestMemoryBuilder::new();
        for vnode in 0..2 {
            let mut ranges: Vec<_> = layout
                .ram()
                .iter()
                .filter(|r| r.vnode == vnode)
                .map(|r| r.range)
                .collect();
            if vnode == 0 {
                ranges.extend(layout.vtl2_range());
            }
            let mut mem = memory_config();
            // Exercise policy on imported slices, without requiring host NUMA
            // binding permissions or configured huge pages.
            mem.prefetch_memory = vnode == 0;
            mem.transparent_hugepages = vnode == 1;
            builder = source
                .add_node(builder, usize::try_from(vnode).unwrap(), ranges, &mem)
                .unwrap();
        }
        let manager = builder.build(17 * page).await.unwrap();
        let gm = manager.client().guest_memory().await.unwrap();
        for (gpa, offset, byte) in [
            (page, 0, 1u8),
            (4 * page, page, 2),
            (8 * page, 2 * page, 3),
            (16 * page, 3 * page, 4),
        ] {
            file.write_all_at(&[byte], offset).unwrap();
            assert_eq!(gm.read_plain::<u8>(gpa).unwrap(), byte);
            gm.write_at(gpa, &[byte + 4]).unwrap();
            let mut actual = [0];
            file.read_exact_at(&mut actual, offset).unwrap();
            assert_eq!(actual, [byte + 4]);
        }
        assert!(gm.read_plain::<u8>(3 * page).is_err());
        assert!(manager.shared_memory_backing().is_none());
    }

    #[test]
    fn supplied_backing_rejects_incompatible_policy_and_holes() {
        let page = page_size();
        let layout = layout();
        for (private, hugepages, range) in [
            (true, false, page..2 * page),
            (false, true, page..2 * page),
            (false, false, 2 * page..3 * page),
        ] {
            let mut source =
                RamBackingSource::new(None, 2, Some(backing(&layout)), &layout).unwrap();
            let mut mem = memory_config();
            mem.private_memory = private;
            mem.hugepages = hugepages;
            assert!(
                source
                    .add_node(
                        GuestMemoryBuilder::new(),
                        0,
                        vec![MemoryRange::new(range)],
                        &mem
                    )
                    .is_err()
            );
        }
    }

    #[async_test]
    async fn ordinary_backing_and_restore_remain_available() {
        let page = page_size();
        let layout = layout();
        let mut source = RamBackingSource::new(None, 1, None, &layout).unwrap();
        let ranges = vec![MemoryRange::new(page..2 * page)];
        let manager = source
            .add_node(
                GuestMemoryBuilder::new(),
                0,
                ranges.clone(),
                &memory_config(),
            )
            .unwrap()
            .build(2 * page)
            .await
            .unwrap();
        let gm = manager.client().guest_memory().await.unwrap();
        gm.write_at(page, &[0x42]).unwrap();
        assert!(
            RamBackingSource::new(
                manager.shared_memory_backing(),
                1,
                Some(backing(&layout)),
                &layout
            )
            .is_err()
        );
        assert!(RamBackingSource::new(manager.shared_memory_backing(), 2, None, &layout).is_err());
        let mut restored =
            RamBackingSource::new(manager.shared_memory_backing(), 1, None, &layout).unwrap();
        let restored = restored
            .add_node(GuestMemoryBuilder::new(), 0, ranges, &memory_config())
            .unwrap()
            .build(2 * page)
            .await
            .unwrap();
        let restored_gm = restored.client().guest_memory().await.unwrap();
        assert_eq!(restored_gm.read_plain::<u8>(page).unwrap(), 0x42);
    }
}
