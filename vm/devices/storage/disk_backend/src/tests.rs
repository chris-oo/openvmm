// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use pal_async::async_test;
use parking_lot::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use test_with_tracing::test;

#[derive(Default)]
struct State {
    calls: AtomicUsize,
    snapshot: Mutex<Vec<u8>>,
    fua: Mutex<Option<bool>>,
    completion: Mutex<Option<mesh::OneshotReceiver<()>>>,
    fail_read: bool,
}

#[derive(Inspect)]
struct FakeDisk {
    #[inspect(skip)]
    state: Arc<State>,
    #[inspect(skip)]
    guest: GuestMemory,
    require_owned: bool,
}

impl FakeDisk {
    fn check_buffers(&self, buffers: &RequestBuffers<'_>, for_write: bool) -> usize {
        if self.require_owned {
            assert!(!std::ptr::eq(buffers.guest_memory(), &self.guest));
            assert_eq!(
                buffers.guest_memory().io_policy(),
                GuestMemoryIoPolicy::Direct
            );
        }
        assert!(buffers.is_aligned(512));
        let locked = buffers.lock(for_write).unwrap();
        let first = locked
            .io_vecs()
            .first()
            .map_or(0, |iov| iov.as_ptr() as usize);
        assert_eq!(first % 4096, 0);
        if !self.require_owned {
            let original = RequestBuffers::new(&self.guest, buffers.range(), true);
            let original_locked = original.lock(for_write).unwrap();
            assert_eq!(first, original_locked.io_vecs()[0].as_ptr() as usize);
        }
        first
    }

    async fn wait(&self) {
        let completion = self.state.completion.lock().take();
        if let Some(completion) = completion {
            completion.await.unwrap();
        }
    }
}

impl DiskIo for FakeDisk {
    fn disk_type(&self) -> &str {
        "owned-io-test"
    }

    fn sector_count(&self) -> u64 {
        4096
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        4096
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn unmap(&self, _: u64, _: u64, _: bool) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Unspecified
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        assert_eq!(sector, 7);
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        let address = self.check_buffers(buffers, true);
        buffers.writer().fill(0x5a, buffers.len())?;
        self.wait().await;
        assert_eq!(self.check_buffers(buffers, true), address);
        if self.state.fail_read {
            return Err(DiskError::IllegalBlock);
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        assert_eq!(sector, 7);
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        let address = self.check_buffers(buffers, false);
        *self.state.fua.lock() = Some(fua);
        let mut before = vec![0; buffers.len()];
        buffers.reader().read(&mut before)?;
        self.wait().await;
        assert_eq!(self.check_buffers(buffers, false), address);
        let mut after = vec![0; buffers.len()];
        buffers.reader().read(&mut after)?;
        assert_eq!(before, after);
        *self.state.snapshot.lock() = after;
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }
}

fn fixture(state: Arc<State>, policy: GuestMemoryIoPolicy) -> (Disk, GuestMemory) {
    let guest = GuestMemory::try_allocate(8192)
        .unwrap()
        .with_io_policy(policy);
    guest.write_at(0, &[0x11; 8192]).unwrap();
    let disk = Disk::new(FakeDisk {
        state,
        guest: guest.clone(),
        require_owned: policy == GuestMemoryIoPolicy::CopyOnly,
    })
    .unwrap();
    (disk, guest)
}

fn request<'a>(
    guest: &'a GuestMemory,
    gpns: &'a [u64],
    offset: usize,
    len: usize,
    is_write: bool,
) -> RequestBuffers<'a> {
    RequestBuffers::new(guest, PagedRange::new(offset, len, gpns).unwrap(), is_write)
}

#[async_test]
async fn copy_only_write_snapshots_before_backend_wait_and_forwards_fua() {
    for fua in [false, true] {
        let (complete, completion) = mesh::oneshot();
        let state = Arc::new(State {
            completion: Mutex::new(Some(completion)),
            ..State::default()
        });
        let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
        let buffers = request(&guest, &[0, 1], 17, 4608, false);
        let mut io = std::pin::pin!(disk.write_vectored(&buffers, 7, fua));
        assert!(futures::poll!(&mut io).is_pending());
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        guest.write_at(17, &[0x22; 4608]).unwrap();
        complete.send(());
        io.await.unwrap();
        assert_eq!(*state.snapshot.lock(), vec![0x11; 4608]);
        assert_eq!(*state.fua.lock(), Some(fua));
    }
}

#[async_test]
async fn copy_only_read_is_published_only_on_success() {
    for fail_read in [false, true] {
        let (complete, completion) = mesh::oneshot();
        let state = Arc::new(State {
            completion: Mutex::new(Some(completion)),
            fail_read,
            ..State::default()
        });
        let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
        let buffers = request(&guest, &[0, 1], 17, 4608, true);
        let mut io = std::pin::pin!(disk.read_vectored(&buffers, 7));
        assert!(futures::poll!(&mut io).is_pending());
        let mut data = [0; 4608];
        guest.read_at(17, &mut data).unwrap();
        assert_eq!(data, [0x11; 4608]);
        complete.send(());
        let result = io.await;
        assert_eq!(result.is_err(), fail_read);
        guest.read_at(17, &mut data).unwrap();
        assert_eq!(data, [if fail_read { 0x11 } else { 0x5a }; 4608]);
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    }
}

#[async_test]
async fn aligned_copy_only_requests_are_staged_including_size_limit() {
    let state = Arc::new(State::default());
    let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
    for len in [512, 4096, 8192, MAX_BOUNCE_IO_SIZE] {
        let gpns = vec![0; len.div_ceil(4096)];
        let buffers = request(&guest, &gpns, 0, len, true);
        disk.read_vectored(&buffers, 7).await.unwrap();
        disk.write_vectored(&buffers, 7, false).await.unwrap();
        assert_eq!(*state.snapshot.lock(), vec![0x5a; len]);
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 8);
}

#[async_test]
async fn invalid_requests_never_reach_backend() {
    let state = Arc::new(State::default());
    let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
    let gpns = vec![0; (MAX_BOUNCE_IO_SIZE + 512).div_ceil(4096)];
    for len in [1, 511, 513, MAX_BOUNCE_IO_SIZE + 512] {
        let buffers = request(&guest, &gpns, 0, len, true);
        assert!(matches!(
            disk.read_vectored(&buffers, 7).await,
            Err(DiskError::InvalidInput)
        ));
        assert!(matches!(
            disk.write_vectored(&buffers, 7, true).await,
            Err(DiskError::InvalidInput)
        ));
    }
    let buffers = request(&guest, &[0], 0, 512, true);
    for sector in [u64::MAX, (i64::MAX as u64) / 512] {
        assert!(matches!(
            disk.read_vectored(&buffers, sector).await,
            Err(DiskError::IllegalBlock)
        ));
        assert!(matches!(
            disk.write_vectored(&buffers, sector, false).await,
            Err(DiskError::IllegalBlock)
        ));
    }
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn copy_input_and_direction_failures_never_reach_backend() {
    let state = Arc::new(State::default());
    let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
    let absent = request(&guest, &[2], 0, 512, false);
    assert!(matches!(
        disk.write_vectored(&absent, 7, false).await,
        Err(DiskError::MemoryAccess(_))
    ));
    let readonly = request(&guest, &[0], 0, 512, false);
    assert!(matches!(
        disk.read_vectored(&readonly, 7).await,
        Err(DiskError::MemoryAccess(AccessError::ReadOnly))
    ));
    let mut data = [0; 512];
    guest.read_at(0, &mut data).unwrap();
    assert_eq!(data, [0x11; 512]);
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn copyback_failure_is_not_success_and_does_not_retry() {
    let state = Arc::new(State::default());
    let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
    let absent = request(&guest, &[2], 0, 512, true);
    assert!(matches!(
        disk.read_vectored(&absent, 7).await,
        Err(DiskError::MemoryAccess(_))
    ));
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
}

#[async_test]
async fn zero_length_copy_only_requests_need_no_backend() {
    let state = Arc::new(State::default());
    let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::CopyOnly);
    let empty = request(&guest, &[], 0, 0, false);
    disk.read_vectored(&empty, 7).await.unwrap();
    disk.write_vectored(&empty, 7, true).await.unwrap();
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);
}

#[async_test]
async fn direct_owned_requests_are_not_staged_again() {
    let state = Arc::new(State::default());
    let (disk, guest) = fixture(state.clone(), GuestMemoryIoPolicy::Direct);
    let buffers = request(&guest, &[0], 0, 512, true);
    disk.read_vectored(&buffers, 7).await.unwrap();
    let mut data = [0; 512];
    guest.read_at(0, &mut data).unwrap();
    assert_eq!(data, [0x5a; 512]);
    disk.write_vectored(&buffers, 7, true).await.unwrap();
    assert_eq!(*state.snapshot.lock(), vec![0x5a; 512]);
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    let partial = request(&guest, &[0], 0, 513, true);
    assert!(matches!(
        disk.read_vectored(&partial, 7).await,
        Err(DiskError::InvalidInput)
    ));
    assert!(matches!(
        disk.write_vectored(&partial, 7, false).await,
        Err(DiskError::InvalidInput)
    ));
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
}

#[test]
#[cfg(target_pointer_width = "64")]
fn public_disk_io_future_sizes_stay_within_budget() {
    let (disk, guest) = fixture(Arc::new(State::default()), GuestMemoryIoPolicy::Direct);
    let buffers = request(&guest, &[0], 0, 512, true);
    let read = disk.read_vectored(&buffers, 7);
    let write = disk.write_vectored(&buffers, 7, false);
    // Measured after boxing only owned staging. Frontends embed these futures
    // in fixed-size buffers, so staging must not increase this budget.
    const FUTURE_SIZE_BUDGET: usize = 1304;
    assert!(size_of_val(&read) <= FUTURE_SIZE_BUDGET);
    assert!(size_of_val(&write) <= FUTURE_SIZE_BUDGET);
}
