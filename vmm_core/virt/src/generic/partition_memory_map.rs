// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Declaring unsafe trait functions for manual memory management.
#![expect(unsafe_code)]

/// Trait for mapping process memory into a partition.
pub trait PartitionMemoryMap: Send + Sync {
    /// Unmaps any ranges in the given guest physical address range.
    ///
    /// The specified range may overlap zero, one, or many ranges mapped with
    /// `map_range`. Any overlapped ranges must be completely contained in the
    /// specified range.
    ///
    /// The hypervisor must ensure that this operation does not fail as long as
    /// the preconditions are satisfied.
    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()>;

    /// Maps a range from process memory into the VM.
    ///
    /// This may fail if the range overlaps any other mapped range.
    ///
    /// # Safety
    /// The caller must ensure that the VA region (data..data+size) is not
    /// reused for the lifetime of this mapping.
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()>;

    /// Prefetches any memory in the given range so that it can be accessed
    /// quickly by the partition without exits.
    fn prefetch_range(&self, _addr: u64, _size: u64) -> anyhow::Result<()> {
        Ok(())
    }

    /// Pins a range in memory so that it can be accessed by assigned devices.
    fn pin_range(&self, _addr: u64, _size: u64) -> anyhow::Result<()> {
        Ok(())
    }

    /// Maps a range residing in a remote process.
    ///
    /// This may fail if the range overlaps any other mapped range.
    ///
    /// # Safety
    /// The caller must ensure that the VA region (data..data+size) within
    /// `process` is not reused for the lifetime of this mapping.
    #[cfg(windows)]
    unsafe fn map_remote_range(
        &self,
        process: std::os::windows::io::BorrowedHandle<'_>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()>;
}

/// Interface for acquiring host access to guest memory.
///
/// Some hypervisors do not make a guest page accessible to userspace
/// merely because the guest marked it shared. The VMM must also ask the
/// hypervisor to grant the host permission to touch the existing backing.
pub trait PartitionHostAccess: Send + Sync {
    /// Acquires host access without changing guest visibility.
    ///
    /// `addr` and `size` are byte offsets in the guest physical address space.
    ///
    /// TODO: This trait is sufficient for MSHV bring-up, but a redesign is
    /// required to safely lower host access and track that pages are not
    /// currently in use before revoking access.
    fn acquire_host_access(&self, addr: u64, size: u64, write: bool) -> anyhow::Result<()>;

    /// Reserves host access to guest pages before their host virtual addresses
    /// are exposed to a caller.
    ///
    /// `gpns` contains guest page numbers in the partition GPA space. `write`
    /// specifies whether the caller will write through the mapping.
    ///
    /// Returns an owned reservation that releases the pages when dropped. If
    /// this returns an error, the implementation must release every
    /// reservation it made during the call. Repeated GPNs are permitted and
    /// represent repeated reservations. `None` means no reservation was
    /// needed.
    ///
    /// The caller probes each page after this method returns and before it
    /// exposes the page's virtual address. The reservation prevents a
    /// concurrent visibility transition during that probe.
    ///
    /// The returned reservation must own everything needed to release the
    /// pages and must remain valid after this interface is dropped.
    fn lock_gpns(
        &self,
        gpns: &[u64],
        write: bool,
    ) -> anyhow::Result<Option<Box<dyn guestmem::GuestMemoryBackingLock>>> {
        let _ = (gpns, write);
        Ok(None)
    }
}
