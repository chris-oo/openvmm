// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

use crate::memory::MappedDmaTarget;
use anyhow::Context;
use inspect::Inspect;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use zerocopy::IntoBytes;

const PAGE_SIZE: usize = 4096;

pub struct LockedMemory {
    mapping: Mapping,
    pfns: Vec<u64>,
}

// SAFETY: The result of an mmap is safe to share amongst threads.
unsafe impl Send for Mapping {}
// SAFETY: The result of an mmap is safe to share amongst threads.
unsafe impl Sync for Mapping {}

struct Mapping {
    addr: *mut c_void,
    len: usize,
}

impl Mapping {
    fn new(len: usize) -> std::io::Result<Self> {
        // overallocate such that we are guaranteed to have a 2mb region
        let size_2m = 0x200000;

        if len % size_2m == 0 {
            // try to allocate with hugetlb and huge 2mb
            let addr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE
                        | libc::MAP_ANONYMOUS
                        | libc::MAP_LOCKED
                        | libc::MAP_HUGETLB
                        | libc::MAP_HUGE_2MB,
                    -1,
                    0,
                )
            };
            if addr == libc::MAP_FAILED {
                tracing::error!(
                    ?len,
                    "mmap with hugetlb failed, falling back to normal mmap"
                );
            } else {
                tracing::error!(?addr, len, "addr mmap with hugetlb");
                return Ok(Self { addr, len });
            }
        }

        let larger_len = if len < size_2m { len } else { len + size_2m };

        // SAFETY: No file descriptor or address is being passed.
        // The result is being validated.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                larger_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_LOCKED,
                // | libc::MAP_HUGETLB
                // | libc::MAP_HUGE_2MB,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        tracing::error!(?addr, len, larger_len, "addr mmap");

        // figure out the address that is 2MB aligned. then unmap the head.
        let addr = addr as usize;
        let aligned_address = if len > size_2m {
            (addr + size_2m - 1) & !(size_2m - 1)
        } else {
            addr
        };

        // munmap up to unaligned
        if addr != aligned_address {
            let result = unsafe { libc::munmap(addr as *mut c_void, aligned_address - addr) };
            if result < 0 {
                let last_error = std::io::Error::last_os_error();
                tracing::error!(
                    ?last_error,
                    ?result,
                    ?addr,
                    aligned_address,
                    "munmap head failed"
                );
                return Err(last_error);
            }
        }

        // madvise original allocation, which will only succeed if len was 2mb aligned
        let head_len = aligned_address - addr;
        let tail_len = larger_len - head_len - len;
        const MADV_COLLAPSE: libc::c_int = 25;
        let result = unsafe { libc::madvise(aligned_address as *mut c_void, len, MADV_COLLAPSE) };
        // let result = unsafe { libc::madvise(addr, len, libc::MADV_HUGEPAGE) };

        // TODO: mlock instead of MAP_LOCKED? or should we instead use MAP_HUGETLB? ask kernel folks what's better and implications of each

        if result < 0 {
            let last_error = std::io::Error::last_os_error();
            tracing::error!(?last_error, ?result, ?addr, len, "madvise failed");
        }

        // unmap any ranges larger than the alloc than we needed
        if tail_len > 0 {
            tracing::error!(?aligned_address, len, tail_len, "munmap tail");
            let result = unsafe { libc::munmap((addr as usize + len) as *mut c_void, tail_len) };
            if result < 0 {
                let last_error = std::io::Error::last_os_error();
                tracing::error!(?last_error, ?result, ?addr, len, "munmap tail failed");
                return Err(last_error);
            }
        }

        Ok(Self {
            addr: aligned_address as *mut c_void,
            len,
        })
    }

    fn lock(&self) -> std::io::Result<()> {
        // SAFETY: self contains a valid mmap result.
        if unsafe { libc::mlock(self.addr, self.len) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn pages(&self) -> anyhow::Result<Vec<u64>> {
        let mut pagemap = File::open("/proc/self/pagemap").context("failed to open pagemap")?;
        pagemap
            .seek(SeekFrom::Start((8 * self.addr as usize / PAGE_SIZE) as u64))
            .context("failed to seek")?;
        let n = self.len / PAGE_SIZE;
        let mut pfns = vec![0u64; n];
        pagemap
            .read(pfns.as_mut_bytes())
            .context("failed to read from pagemap")?;
        for pfn in &mut pfns {
            if *pfn & (1 << 63) == 0 {
                anyhow::bail!("page not present in RAM");
            }
            *pfn &= 0x3f_ffff_ffff_ffff;
        }
        Ok(pfns)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: self contains a valid mmap result.
        if unsafe { libc::munmap(self.addr, self.len) } < 0 {
            panic!("{:?}", std::io::Error::last_os_error());
        }
    }
}

impl LockedMemory {
    pub fn new(mut len: usize) -> anyhow::Result<Self> {
        if len % PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let mapping = Mapping::new(len).context("failed to create mapping")?;
        mapping.lock().context("failed to lock mapping")?;
        let pages = mapping.pages()?;

        Ok(Self {
            mapping,
            pfns: pages,
        })
    }
}

// SAFETY: The stored mapping is valid for the lifetime of the LockedMemory.
// It is only unmapped on drop.
unsafe impl MappedDmaTarget for LockedMemory {
    fn base(&self) -> *const u8 {
        self.mapping.addr.cast()
    }

    fn len(&self) -> usize {
        self.mapping.len
    }

    fn pfns(&self) -> &[u64] {
        &self.pfns
    }

    fn pfn_bias(&self) -> u64 {
        0
    }
}

#[derive(Clone, Inspect)]
pub struct LockedMemorySpawner;

impl crate::DmaClient for LockedMemorySpawner {
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<crate::memory::MemoryBlock> {
        Ok(crate::memory::MemoryBlock::new(LockedMemory::new(len)?))
    }

    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<crate::memory::MemoryBlock>> {
        anyhow::bail!("restore not supported for lockmem")
    }
}
