// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Explicit, test-only DMA-copy revocation rendezvous for Linux/x86-64.
//!
//! This module does not change [`crate::try_copy`]. Its caller must retain a
//! shared DMA request, install a bounded cancellation watchdog, and resume
//! only after the revocation ioctl returns. Linux protection tests below
//! validate recovery, not Hyper-V host-access enforcement.

use crate::MemoryError;
use crate::recovery_descriptor;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;

/// Instruction width selected independently of the production copy heuristic.
#[derive(Debug, Clone, Copy)]
pub enum Width {
    /// Byte scalar accesses.
    Byte,
    /// Eight-byte scalar accesses.
    Word,
    /// Two 16-byte SIMD accesses per iteration.
    Simd,
    /// `rep movsb`, retaining its pointer registers across the checkpoint.
    Rep,
}

/// Owned controls. Assembly never borrows a guest page for its rendezvous.
#[derive(Default)]
pub struct Rendezvous {
    progress: AtomicUsize,
    decision: AtomicUsize,
}

impl Rendezvous {
    /// Wait for the prefix to become observable. No sleep-based race is used.
    pub fn wait_for_prefix(&self, timeout: Duration) -> Result<usize, CopyError> {
        let start = Instant::now();
        loop {
            let progress = self.progress.load(Ordering::Acquire);
            if progress != 0 {
                return Ok(progress);
            }
            if start.elapsed() >= timeout || self.decision.load(Ordering::Acquire) == 2 {
                self.cancel();
                return Err(CopyError::Cancelled);
            }
            std::thread::yield_now();
        }
    }

    /// Resume only after a successful, fully returned revocation.
    pub fn resume(&self) {
        // Cancellation cannot be overwritten by a late ioctl completion.
        let _ = self
            .decision
            .compare_exchange(0, 1, Ordering::Release, Ordering::Relaxed);
    }

    /// Abandon the tail without treating cancellation as a memory fault.
    pub fn cancel(&self) {
        self.decision.store(2, Ordering::Release);
    }
}

/// Test-copy failure. Cancellation has no stale OS fault information.
#[derive(Debug, Error)]
pub enum CopyError {
    /// The caller supplied an unsupported copy shape.
    #[error("experiment requires disjoint buffers and 32-byte aligned lengths")]
    InvalidShape,
    /// The revoker or watchdog cancelled the paused tail.
    #[error("copy rendezvous cancelled")]
    Cancelled,
    /// A fault occurred in the annotated guest-copy instructions.
    #[error(transparent)]
    Fault(#[from] MemoryError),
}

/// Identity of the device request whose completion is still withheld.
#[derive(Debug, Clone, Copy)]
pub struct DmaLease {
    /// Guest GPA of this payload descriptor.
    pub payload_gpa: u64,
    /// Available descriptor-chain head.
    pub descriptor_index: u16,
    /// Block request sector, in backend units.
    pub sector: u64,
    /// Request header descriptor GPA.
    pub header_gpa: u64,
    /// Request status descriptor GPA.
    pub status_gpa: u64,
}

type DmaProbe = dyn Fn(DmaLease, &mut [u8], bool) -> Result<(), String> + Send + Sync;
static DMA_PROBE: OnceLock<Box<DmaProbe>> = OnceLock::new();

/// Install the single VM's opt-in observer. Multiple VMs are not supported.
pub fn install_dma_probe(
    probe: impl Fn(DmaLease, &mut [u8], bool) -> Result<(), String> + Send + Sync + 'static,
) -> Result<(), &'static str> {
    DMA_PROBE
        .set(Box::new(probe))
        .map_err(|_| "a DMA revocation experiment is already installed")
}

/// Called only while the device still owns its uncompleted DMA request.
pub fn probe_dma(lease: DmaLease, owned: &mut [u8], write: bool) -> Result<(), String> {
    match DMA_PROBE.get() {
        Some(probe) => probe(lease, owned, write),
        None => Ok(()),
    }
}

/// Copy with exactly one mid-function checkpoint in the annotated assembly.
///
/// The prefix and tail are not separate calls. Source, destination, loop index
/// and (for `rep movsb`) updated pointer registers remain live at the pause.
///
/// # Safety
/// Both buffers must be reserved, disjoint ranges of `len` bytes. Writes must
/// not violate Rust aliasing. `rendezvous` must outlive all revoker/watchdog
/// threads. A watchdog must call [`Rendezvous::cancel`] if the revoker stalls.
pub unsafe fn copy(
    src: *const u8,
    dest: *mut u8,
    len: usize,
    prefix: usize,
    width: Width,
    rendezvous: &Rendezvous,
) -> Result<(), CopyError> {
    if prefix == 0
        || prefix >= len
        || !prefix.is_multiple_of(32)
        || !len.is_multiple_of(32)
        || src.addr().abs_diff(dest.addr()) < len
    {
        return Err(CopyError::InvalidShape);
    }
    crate::initialize_try_copy();
    let progress = rendezvous.progress.as_ptr();
    let decision = rendezvous.decision.as_ptr();
    macro_rules! scalar {
        ($load:literal, $store:literal, $step:literal, $scratch:tt) => {
            // SAFETY: buffers and owned atomic controls satisfy the contract.
            unsafe {
                core::arch::asm!(
                    "2:",
                    $load, $store, $step,
                    "cmp {index}, {prefix}",
                    "jne 5f",
                    "mov qword ptr [{progress}], {index}",
                    "4:",
                    "cmp qword ptr [{decision}], 0",
                    "jne 6f",
                    "pause",
                    "jmp 4b",
                    "6:",
                    "cmp qword ptr [{decision}], 1",
                    "jne {cancel}",
                    "5:",
                    "cmp {index}, {len}",
                    "jne 2b",
                    "3:",
                    recovery_descriptor!("2b", "3b", "{fault}"),
                    scratch = out($scratch) _,
                    index = inout(reg) 0usize => _,
                    src = in(reg) src,
                    dest = in(reg) dest,
                    len = in(reg) len,
                    prefix = in(reg) prefix,
                    progress = in(reg) progress,
                    decision = in(reg) decision,
                    fault = label {
                        return Err(CopyError::Fault(MemoryError::from_last_failure(
                            Some(src), dest, len,
                        )));
                    },
                    cancel = label { return Err(CopyError::Cancelled); },
                    options(nostack),
                );
            }
        };
    }
    match width {
        Width::Byte => scalar!(
            "mov {scratch}, byte ptr [{src} + {index}]",
            "mov byte ptr [{dest} + {index}], {scratch}",
            "inc {index}",
            reg_byte
        ),
        Width::Word => scalar!(
            "mov {scratch}, qword ptr [{src} + {index}]",
            "mov qword ptr [{dest} + {index}], {scratch}",
            "add {index}, 8",
            reg
        ),
        Width::Simd => {
            // SAFETY: same reserved buffers and owned controls as above.
            unsafe {
                core::arch::asm!(
                    "2:",
                    "movdqu {a}, xmmword ptr [{src} + {index}]",
                    "movdqu {b}, xmmword ptr [{src} + {index} + 16]",
                    "movdqu xmmword ptr [{dest} + {index}], {a}",
                    "movdqu xmmword ptr [{dest} + {index} + 16], {b}",
                    "add {index}, 32",
                    "cmp {index}, {prefix}",
                    "jne 5f",
                    "mov qword ptr [{progress}], {index}",
                    "4:",
                    "cmp qword ptr [{decision}], 0",
                    "jne 6f",
                    "pause",
                    "jmp 4b",
                    "6:",
                    "cmp qword ptr [{decision}], 1",
                    "jne {cancel}",
                    "5:",
                    "cmp {index}, {len}",
                    "jne 2b",
                    "3:",
                    recovery_descriptor!("2b", "3b", "{fault}"),
                    a = out(xmm_reg) _,
                    b = out(xmm_reg) _,
                    index = inout(reg) 0usize => _,
                    src = in(reg) src,
                    dest = in(reg) dest,
                    len = in(reg) len,
                    prefix = in(reg) prefix,
                    progress = in(reg) progress,
                    decision = in(reg) decision,
                    fault = label {
                        return Err(CopyError::Fault(MemoryError::from_last_failure(
                            Some(src), dest, len,
                        )));
                    },
                    cancel = label { return Err(CopyError::Cancelled); },
                    options(nostack),
                );
            }
        }
        Width::Rep => {
            let tail = len - prefix;
            // SAFETY: same reserved buffers; rsi/rdi remain advanced at pause.
            unsafe {
                core::arch::asm!(
                    "2:",
                    "rep movsb",
                    "mov qword ptr [{progress}], {prefix}",
                    "4:",
                    "cmp qword ptr [{decision}], 0",
                    "jne 5f",
                    "pause",
                    "jmp 4b",
                    "5:",
                    "cmp qword ptr [{decision}], 1",
                    "jne {cancel}",
                    "mov rcx, {tail}",
                    "rep movsb",
                    "3:",
                    recovery_descriptor!("2b", "3b", "{fault}"),
                    inout("rsi") src => _,
                    inout("rdi") dest => _,
                    inout("rcx") prefix => _,
                    prefix = in(reg) prefix,
                    tail = in(reg) tail,
                    progress = in(reg) progress,
                    decision = in(reg) decision,
                    fault = label {
                        return Err(CopyError::Fault(MemoryError::from_last_failure(
                            Some(src), dest, len,
                        )));
                    },
                    cancel = label { return Err(CopyError::Cancelled); },
                    options(nostack),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use test_with_tracing::test;

    #[test]
    fn all_widths_keep_prefix_guards_and_resume() {
        for width in [Width::Byte, Width::Word, Width::Simd, Width::Rep] {
            let src = [0x71u8; 256];
            let mut dest = [0xa5u8; 320];
            let rendezvous = Arc::new(Rendezvous::default());
            let revoker = rendezvous.clone();
            let thread = std::thread::spawn(move || {
                assert_eq!(revoker.wait_for_prefix(Duration::from_secs(2)).unwrap(), 64);
                revoker.resume();
            });
            // SAFETY: disjoint owned arrays with 32-byte outer guards.
            unsafe {
                copy(
                    src.as_ptr(),
                    dest.as_mut_ptr().add(32),
                    256,
                    64,
                    width,
                    &rendezvous,
                )
                .unwrap();
            }
            thread.join().unwrap();
            assert_eq!(&dest[..32], &[0xa5; 32]);
            assert_eq!(&dest[32..288], &src);
            assert_eq!(&dest[288..], &[0xa5; 32]);
        }
    }

    #[test]
    fn cancellation_preserves_tail_and_cannot_be_resumed() {
        for width in [Width::Byte, Width::Word, Width::Simd, Width::Rep] {
            let src = [0x71u8; 256];
            let mut dest = [0xa5u8; 256];
            let rendezvous = Arc::new(Rendezvous::default());
            let revoker = rendezvous.clone();
            let thread = std::thread::spawn(move || {
                revoker.wait_for_prefix(Duration::from_secs(2)).unwrap();
                revoker.cancel();
                revoker.resume();
            });
            // SAFETY: disjoint owned arrays remain live until copy returns.
            let result =
                unsafe { copy(src.as_ptr(), dest.as_mut_ptr(), 256, 64, width, &rendezvous) };
            thread.join().unwrap();
            assert!(matches!(result, Err(CopyError::Cancelled)));
            assert_eq!(&dest[..64], &src[..64]);
            assert_eq!(&dest[64..], &[0xa5; 192]);
        }
    }

    #[test]
    fn missing_copy_times_out() {
        let rendezvous = Rendezvous::default();
        assert!(matches!(
            rendezvous.wait_for_prefix(Duration::ZERO),
            Err(CopyError::Cancelled)
        ));
    }

    #[test]
    fn all_widths_recover_cross_page_read_and_write_faults() {
        // This uses Linux PTE protection only to validate the new assembly's
        // recovery descriptors and partial-copy accounting. It is not an
        // MSHV/Hyper-V permission experiment.
        for width in [Width::Byte, Width::Word, Width::Simd, Width::Rep] {
            for write in [false, true] {
                // SAFETY: reserve two private pages owned solely by this test.
                let mapping = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        8192,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                assert_ne!(mapping, libc::MAP_FAILED);
                let guest = mapping.cast::<u8>();
                // SAFETY: freshly mapped private pages have no other aliases.
                unsafe { std::ptr::write_bytes(guest, 0x71, 8192) };
                let mut owned = [0xa5u8; 8192];
                let rendezvous = Arc::new(Rendezvous::default());
                let revoker_control = rendezvous.clone();
                let address = guest.addr();
                let revoker = std::thread::spawn(move || {
                    assert_eq!(
                        revoker_control
                            .wait_for_prefix(Duration::from_secs(2))
                            .unwrap(),
                        64
                    );
                    // SAFETY: this is the retained second page of this test's
                    // mapping; the copy has paused after its first-page prefix.
                    let result = unsafe {
                        libc::mprotect((address + 4096) as *mut libc::c_void, 4096, libc::PROT_NONE)
                    };
                    if result == 0 {
                        revoker_control.resume();
                    } else {
                        revoker_control.cancel();
                    }
                    assert_eq!(result, 0);
                });
                let (src, dest) = if write {
                    (owned.as_ptr().wrapping_add(64), guest.wrapping_add(64))
                } else {
                    (
                        guest.wrapping_add(64).cast_const(),
                        owned.as_mut_ptr().wrapping_add(64),
                    )
                };
                // SAFETY: disjoint owned ranges; revocation changes only PTE
                // access. Both ranges stay reserved until the copy returns.
                let result = unsafe { copy(src, dest, 8064, 64, width, &rendezvous) };
                revoker.join().unwrap();
                let Err(CopyError::Fault(error)) = result else {
                    panic!("expected annotated fault: {result:?}");
                };
                assert_eq!(error.offset(), 4032);
                assert_eq!(error.is_write, write);
                assert_eq!(error.source.signal, libc::SIGSEGV);
                assert_eq!(error.source.exception, 14);
                assert_eq!(
                    // SAFETY: restore the same test-owned reserved mapping.
                    unsafe { libc::mprotect(mapping, 8192, libc::PROT_READ | libc::PROT_WRITE) },
                    0
                );
                // SAFETY: access has been restored, and the copy has returned.
                let guest_bytes = unsafe { std::slice::from_raw_parts(guest, 8192) };
                let (dest_bytes, prefix, original) = if write {
                    (guest_bytes, 0xa5, 0x71)
                } else {
                    (&owned[..], 0x71, 0xa5)
                };
                assert!(dest_bytes[..64].iter().all(|b| *b == original));
                assert!(dest_bytes[64..4096].iter().all(|b| *b == prefix));
                assert!(dest_bytes[4096..].iter().all(|b| *b == original));
                let retry = Rendezvous::default();
                retry.resume();
                // SAFETY: the same live buffers, now fully accessible.
                unsafe { copy(src, dest, 8064, 64, width, &retry).unwrap() };
                // SAFETY: no copy or revoker still uses the mapping.
                assert_eq!(unsafe { libc::munmap(mapping, 8192) }, 0);
            }
        }
    }
}
