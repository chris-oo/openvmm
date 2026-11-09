// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed operations for the supported Linux CCA device-assignment ABI.
//! Errors can follow completed state changes; never blindly retry mutations.

use crate::iommufd::IommufdCtx;
use nix::errno::Errno;
use std::os::fd::AsRawFd;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

/// Evidence objects supported by the CCA vDEVICE backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcaObject {
    /// Virtual component attestation evidence.
    Vca,
    /// Device certificate chain.
    Certificate,
    /// Device measurements.
    Measurement,
    /// TDISP interface report.
    InterfaceReport,
}

impl CcaObject {
    fn abi_value(self) -> u32 {
        match self {
            Self::Vca => 0,
            Self::Certificate => 1,
            Self::Measurement => 2,
            Self::InterfaceReport => 3,
        }
    }
}

/// Native TDISP state targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcaTdiState {
    /// Release assignment.
    Unlocked,
    /// Lock device resources.
    Locked,
    /// Start the assigned device.
    Run,
}

impl CcaTdiState {
    fn abi_value(self) -> u32 {
        match self {
            Self::Unlocked => 0,
            Self::Locked => 1,
            Self::Run => 2,
        }
    }
}

#[repr(C, align(8))]
struct Envelope {
    size: u32,
    vdevice_id: u32,
    op: u32,
    req_len: u32,
    resp_len: u32,
    reserved: u32,
    req_uptr: u64,
    resp_uptr: u64,
}

mod ioctl {
    nix::ioctl_readwrite_bad!(vdevice_op, 0x3b96, super::Envelope);
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct Scalar {
    value: u32,
    reserved: u32,
}

#[repr(C, align(8))]
#[derive(IntoBytes, Immutable)]
struct Read {
    object_type: u32,
    reserved: u32,
    offset: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct Regen {
    object_type: u32,
    flags: u32,
    nonce: [u8; 32],
}

/// Host-owned operation parameters. No guest pointers reach the kernel.
#[derive(Debug)]
pub enum Request {
    /// Change state. An error does not prove the state stayed unchanged.
    SetState(CcaTdiState),
    /// Query the object's length as a native eight-byte unsigned integer.
    ObjectInfo(CcaObject),
    /// Read the complete remaining object at this offset.
    ReadObject {
        /// Evidence object.
        object: CcaObject,
        /// Byte offset in the current kernel snapshot.
        offset: u64,
    },
    /// Refresh the report without flags or a nonce.
    RegenerateInterfaceReport,
    /// Refresh measurements with an inline nonce.
    RegenerateMeasurements {
        /// Request the raw format rather than hashes.
        raw: bool,
        /// Host-owned nonce.
        nonce: [u8; 32],
    },
}

enum Payload {
    Scalar(Scalar),
    Read(Read),
    Regen(Regen),
}

impl Payload {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Scalar(value) => value.as_bytes(),
            Self::Read(value) => value.as_bytes(),
            Self::Regen(value) => value.as_bytes(),
        }
    }
}

/// Local validation or an uncertain kernel result.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The response does not have the shape required by this operation.
    #[error("invalid CCA response capacity {0}")]
    Capacity(usize),
    /// State-changing failures may occur after the operation completed.
    #[error("IOMMU_VDEVICE_OP failed: {0}")]
    Ioctl(#[source] Errno),
    /// The new ABI only permits zero as a successful return.
    #[error("unexpected IOMMU_VDEVICE_OP return {0}")]
    Return(i32),
    /// The kernel returned an invalid or incomplete response length.
    #[error("invalid CCA response length {actual}, capacity {capacity}")]
    Length {
        /// Copied response length.
        actual: u32,
        /// Offered buffer length.
        capacity: u32,
    },
}

fn execute(
    vdevice_id: u32,
    request: Request,
    response: &mut [u8],
    invoke: impl FnOnce(&mut Envelope) -> Result<i32, Errno>,
) -> Result<usize, Error> {
    let info = matches!(request, Request::ObjectInfo(_));
    let read = matches!(request, Request::ReadObject { .. });
    if (info && response.len() != 8) || (!info && !read && !response.is_empty()) {
        return Err(Error::Capacity(response.len()));
    }
    let capacity = u32::try_from(response.len()).map_err(|_| Error::Capacity(response.len()))?;
    let (op, payload) = match request {
        Request::SetState(state) => (
            0,
            Payload::Scalar(Scalar {
                value: state.abi_value(),
                reserved: 0,
            }),
        ),
        Request::ObjectInfo(object) => (
            1,
            Payload::Scalar(Scalar {
                value: object.abi_value(),
                reserved: 0,
            }),
        ),
        Request::ReadObject { object, offset } => (
            2,
            Payload::Read(Read {
                object_type: object.abi_value(),
                reserved: 0,
                offset,
            }),
        ),
        Request::RegenerateInterfaceReport => (
            3,
            Payload::Regen(Regen {
                object_type: CcaObject::InterfaceReport.abi_value(),
                flags: 0,
                nonce: [0; 32],
            }),
        ),
        Request::RegenerateMeasurements { raw, nonce } => (
            3,
            Payload::Regen(Regen {
                object_type: CcaObject::Measurement.abi_value(),
                flags: u32::from(raw),
                nonce,
            }),
        ),
    };
    let bytes = payload.bytes();
    let mut envelope = Envelope {
        size: size_of::<Envelope>() as u32,
        vdevice_id,
        op,
        req_len: bytes.len() as u32,
        resp_len: capacity,
        reserved: 0,
        req_uptr: bytes.as_ptr() as u64,
        resp_uptr: if response.is_empty() {
            0
        } else {
            response.as_mut_ptr() as u64
        },
    };
    let result = invoke(&mut envelope).map_err(Error::Ioctl)?;
    if result != 0 {
        return Err(Error::Return(result));
    }
    if envelope.resp_len > capacity || (info && envelope.resp_len != 8) {
        return Err(Error::Length {
            actual: envelope.resp_len,
            capacity,
        });
    }
    Ok(envelope.resp_len as usize)
}

impl IommufdCtx {
    /// Issue an operation without retries or ABI fallback.
    ///
    /// Serialize with the complete vDEVICE lifecycle. The response length does
    /// not establish unchanged device state after a syscall/copyback error.
    pub fn cca_tsm_request(
        &self,
        vdevice_id: u32,
        request: Request,
        response: &mut [u8],
    ) -> Result<usize, Error> {
        execute(vdevice_id, request, response, |envelope| {
            // SAFETY: all request and response storage remains live until this
            // synchronous ioctl returns. The kernel retains no user pointers.
            unsafe { ioctl::vdevice_op(self.as_raw_fd(), envelope) }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn rejects_legacy_completion_and_partial_info() {
        for returned in [-1, 1, 8] {
            assert!(matches!(
                execute(
                    1,
                    Request::ObjectInfo(CcaObject::Certificate),
                    &mut [0; 8],
                    |_| Ok(returned)
                ),
                Err(Error::Return(_))
            ));
        }
        assert!(matches!(
            execute(
                1,
                Request::ObjectInfo(CcaObject::Certificate),
                &mut [0; 8],
                |cmd| {
                    cmd.resp_len = 4;
                    Ok(0)
                }
            ),
            Err(Error::Length { .. })
        ));
        assert!(matches!(
            execute(1, Request::SetState(CcaTdiState::Locked), &mut [], |cmd| {
                cmd.resp_len = 1;
                Ok(0)
            }),
            Err(Error::Length { .. })
        ));
    }

    #[test]
    fn regeneration_has_inline_nonce_and_no_padding() {
        let request = Request::RegenerateMeasurements {
            raw: true,
            nonce: [0xa5; 32],
        };
        execute(2, request, &mut [], |cmd| {
            assert_eq!(cmd.size, 40);
            assert_eq!(cmd.req_len, 40);
            assert_eq!(cmd.reserved, 0);
            // SAFETY: execute owns the request for this callback.
            let bytes = unsafe { std::slice::from_raw_parts(cmd.req_uptr as *const u8, 40) };
            assert_eq!(&bytes[..8], &[2, 0, 0, 0, 1, 0, 0, 0]);
            assert_eq!(&bytes[8..], &[0xa5; 32]);
            Ok(0)
        })
        .unwrap();
    }

    #[test]
    fn read_preserves_short_length_and_errno() {
        assert_eq!(
            execute(
                1,
                Request::ReadObject {
                    object: CcaObject::Vca,
                    offset: 3
                },
                &mut [0; 8],
                |cmd| {
                    cmd.resp_len = 3;
                    Ok(0)
                }
            )
            .unwrap(),
            3
        );
        assert!(matches!(
            execute(1, Request::SetState(CcaTdiState::Unlocked), &mut [], |_| {
                Err(Errno::EBUSY)
            }),
            Err(Error::Ioctl(Errno::EBUSY))
        ));
    }
}
