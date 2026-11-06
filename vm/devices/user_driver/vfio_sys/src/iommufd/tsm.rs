// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed CCA TSM requests for the experimental Linux device-assignment ABI.
//!
//! These synchronous bindings retain host buffers for the entire ioctl. They
//! provide no device ownership, access policy, or state machine. Callers must
//! serialize requests with teardown and revoke access before changing state.
//! A state-changing ioctl error can follow a completed device operation.

use super::IommufdCtx;
use nix::errno::Errno;
use std::os::fd::AsRawFd;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

const IOMMU_VDEVICE_TSM_REQ: u32 = 0x3b96;
const TVM_ARCH_CCA: u32 = 1;

mod ioctl {
    nix::ioctl_readwrite_bad!(
        vdevice_tsm_req,
        super::IOMMU_VDEVICE_TSM_REQ,
        super::TsmRequest
    );
}

// Linux integration 2b68f486fdbc: include/uapi/linux/iommufd.h and
// arch/arm64/include/uapi/asm/rmi-da.h. Explicit alignment matches __aligned_u64.
#[repr(C, align(8))]
struct TsmRequest {
    size: u32,
    vdevice_id: u32,
    op: u32,
    tvm_arch: u32,
    req_len: u32,
    resp_len: u32,
    req_uptr: u64,
    resp_uptr: u64,
    tsm_code: u64,
}

#[repr(C, align(8))]
#[derive(IntoBytes, Immutable)]
struct ValidateMmio {
    gpa_base: u64,
    gpa_top: u64,
    pa_base: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable)]
struct ScalarRequest {
    value: u32,
}

#[repr(C, align(8))]
#[derive(IntoBytes, Immutable)]
struct ReadObject {
    object_type: u32,
    reserved: u32,
    offset: u64,
}

#[repr(C, align(8))]
#[derive(IntoBytes, Immutable)]
struct RegenerateObject {
    object_type: u32,
    reserved: u32,
    flags: u64,
    nonce: u64,
}

/// Objects implemented by the pinned CCA host kernel.
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

/// Native CCA state targets, not protobuf TDISP state numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcaTdiState {
    /// Release device assignment.
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

/// A CCA request with host-owned parameters, never raw guest pointers.
#[derive(Debug)]
pub enum CcaTsmRequest<'a> {
    /// Validate a protected MMIO interval. Policy and address validation belong
    /// to the device coordinator; `gpa_top` is exclusive.
    ValidateMmio {
        /// First guest physical address.
        gpa_base: u64,
        /// Exclusive last guest physical address.
        gpa_top: u64,
        /// First host physical address.
        pa_base: u64,
    },
    /// Change device state after the caller has stopped conflicting access.
    SetState(CcaTdiState),
    /// Query an object's size into an exactly four-byte response.
    ObjectSize(CcaObject),
    /// Read a whole object into the response, always at backend offset zero.
    /// The caller must query and bound the allocation first, and reject any
    /// returned length that differs from the queried size.
    ReadObject(CcaObject),
    /// Refresh the interface report.
    RegenerateInterfaceReport,
    /// Refresh measurements. The nonce is borrowed host storage; it is not a
    /// guest address. The backend validates the native measurement flags.
    RegenerateMeasurements {
        /// Native measurement format flags.
        flags: u64,
        /// The guest nonce copied into host storage.
        nonce: &'a [u8; 32],
    },
}

enum Payload {
    Validate(ValidateMmio),
    Scalar(ScalarRequest),
    Read(ReadObject),
    Regenerate(RegenerateObject),
}

impl Payload {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Validate(value) => value.as_bytes(),
            Self::Scalar(value) => value.as_bytes(),
            Self::Read(value) => value.as_bytes(),
            Self::Regenerate(value) => value.as_bytes(),
        }
    }
}

impl CcaTsmRequest<'_> {
    fn encode(&self) -> (u32, Payload) {
        match *self {
            Self::ValidateMmio {
                gpa_base,
                gpa_top,
                pa_base,
            } => (
                1,
                Payload::Validate(ValidateMmio {
                    gpa_base,
                    gpa_top,
                    pa_base,
                }),
            ),
            Self::SetState(state) => (
                2,
                Payload::Scalar(ScalarRequest {
                    value: state.abi_value(),
                }),
            ),
            Self::ReadObject(object) => (
                5,
                Payload::Read(ReadObject {
                    object_type: object.abi_value(),
                    reserved: 0,
                    offset: 0,
                }),
            ),
            Self::RegenerateInterfaceReport => (
                6,
                Payload::Regenerate(RegenerateObject {
                    object_type: CcaObject::InterfaceReport.abi_value(),
                    reserved: 0,
                    flags: 0,
                    nonce: 0,
                }),
            ),
            Self::RegenerateMeasurements { flags, nonce } => (
                6,
                Payload::Regenerate(RegenerateObject {
                    object_type: CcaObject::Measurement.abi_value(),
                    reserved: 0,
                    flags,
                    nonce: nonce.as_ptr() as u64,
                }),
            ),
            Self::ObjectSize(object) => (
                7,
                Payload::Scalar(ScalarRequest {
                    value: object.abi_value(),
                }),
            ),
        }
    }
}

/// A nonnegative ioctl return, which is not by itself TSM success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct TsmCompletion {
    /// Unused response bytes, or unconsumed request bytes if there is no response.
    pub residue: u32,
    /// Raw TSM code. The pinned backend can leave this zero on failure paths.
    pub tsm_code: u64,
}

/// Local validation, syscall failure, or an invalid kernel completion.
#[derive(Debug, thiserror::Error)]
pub enum TsmRequestError {
    /// Rejected before ioctl; only the two native measurement formats exist.
    #[error("unsupported native measurement flags {0:#x}")]
    MeasurementFlags(u64),
    /// Rejected before issuing the ioctl.
    #[error("invalid CCA TSM response length {length}")]
    ResponseLength {
        /// Offered response capacity.
        length: usize,
    },
    /// The request was issued and can have changed device state. The TSM code
    /// may be unchanged if failure happened before kernel copyback.
    #[error("IOMMU_VDEVICE_TSM_REQ failed: {errno} (TSM code {tsm_code:#x})")]
    Ioctl {
        /// Original syscall errno.
        #[source]
        errno: Errno,
        /// Raw output field, initialized to zero before the ioctl.
        tsm_code: u64,
    },
    /// The ioctl completed, but its residue cannot describe the offered buffer.
    #[error("invalid TSM residue {residue} for capacity {capacity} (TSM code {tsm_code:#x})")]
    InvalidResidue {
        /// Raw nonnegative return value.
        residue: u32,
        /// Response capacity, or request size when no response was offered.
        capacity: u32,
        /// Raw TSM output field.
        tsm_code: u64,
    },
    /// A negative return without an errno. This is not a usable completion.
    #[error("unexpected TSM ioctl return {value} (TSM code {tsm_code:#x})")]
    InvalidReturn {
        /// Raw return value.
        value: i32,
        /// Raw TSM output field.
        tsm_code: u64,
    },
}

fn execute(
    vdevice_id: u32,
    request: CcaTsmRequest<'_>,
    response: &mut [u8],
    invoke: impl FnOnce(&mut TsmRequest) -> Result<libc::c_int, Errno>,
) -> Result<TsmCompletion, TsmRequestError> {
    if let CcaTsmRequest::RegenerateMeasurements { flags, .. } = &request {
        if *flags > 1 {
            return Err(TsmRequestError::MeasurementFlags(*flags));
        }
    }
    let response_length_valid = match request {
        CcaTsmRequest::ObjectSize(_) => response.len() == 4,
        CcaTsmRequest::ReadObject(_) => !response.is_empty(),
        _ => response.is_empty(),
    };
    if !response_length_valid {
        return Err(TsmRequestError::ResponseLength {
            length: response.len(),
        });
    }
    let resp_len = u32::try_from(response.len()).map_err(|_| TsmRequestError::ResponseLength {
        length: response.len(),
    })?;
    let (op, payload) = request.encode();
    let bytes = payload.as_bytes();
    let mut cmd = TsmRequest {
        size: size_of::<TsmRequest>() as u32,
        vdevice_id,
        op,
        tvm_arch: TVM_ARCH_CCA,
        req_len: bytes.len() as u32,
        resp_len,
        req_uptr: bytes.as_ptr() as u64,
        resp_uptr: if response.is_empty() {
            0
        } else {
            response.as_mut_ptr() as u64
        },
        tsm_code: 0,
    };
    let capacity = if resp_len != 0 { resp_len } else { cmd.req_len };
    let result = invoke(&mut cmd);
    let value = result.map_err(|errno| TsmRequestError::Ioctl {
        errno,
        tsm_code: cmd.tsm_code,
    })?;
    let residue = u32::try_from(value).map_err(|_| TsmRequestError::InvalidReturn {
        value,
        tsm_code: cmd.tsm_code,
    })?;
    if residue > capacity {
        return Err(TsmRequestError::InvalidResidue {
            residue,
            capacity,
            tsm_code: cmd.tsm_code,
        });
    }
    Ok(TsmCompletion {
        residue,
        tsm_code: cmd.tsm_code,
    })
}

impl IommufdCtx {
    /// Issue a typed synchronous request for an existing CCA vdevice.
    ///
    /// The caller owns the vdevice and must serialize requests with its entire
    /// lifecycle. Do not retry state-changing errors: even a copyback `EFAULT`
    /// can follow a completed state transition. Success requires checking both
    /// residue and TSM status, plus operation-specific response contents.
    ///
    /// These bindings use the pinned experimental Linux ABI, not a capability
    /// probe. They do not enable trusted PCI assignment in OpenVMM.
    pub fn cca_tsm_request(
        &self,
        vdevice_id: u32,
        request: CcaTsmRequest<'_>,
        response: &mut [u8],
    ) -> Result<TsmCompletion, TsmRequestError> {
        execute(vdevice_id, request, response, |cmd| {
            // SAFETY: the typed request, response, and optional 32-byte nonce
            // point to live host storage for this synchronous call. The kernel
            // receives the exact buffer lengths and retains no user pointers.
            unsafe { ioctl::vdevice_tsm_req(self.as_raw_fd(), cmd) }
        })
    }
}

#[cfg(test)]
mod tests;
