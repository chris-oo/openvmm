// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[test]
fn invalid_measurement_format_does_not_issue_an_ioctl() {
    let error = execute(
        1,
        CcaTsmRequest::RegenerateMeasurements {
            flags: 2,
            nonce: &[0; 32],
        },
        &mut [],
        |_| panic!("invalid flags must not reach ioctl"),
    )
    .unwrap_err();
    assert!(matches!(error, TsmRequestError::MeasurementFlags(2)));
}
use std::mem::align_of;
use std::mem::offset_of;
use test_with_tracing::test;

#[test]
fn pinned_request_layouts() {
    assert_eq!(
        nix::request_code_none!(b';', 0x96),
        IOMMU_VDEVICE_TSM_REQ as _
    );
    assert_eq!(size_of::<TsmRequest>(), 48);
    assert_eq!(align_of::<TsmRequest>(), 8);
    assert_eq!(offset_of!(TsmRequest, size), 0);
    assert_eq!(offset_of!(TsmRequest, vdevice_id), 4);
    assert_eq!(offset_of!(TsmRequest, op), 8);
    assert_eq!(offset_of!(TsmRequest, tvm_arch), 12);
    assert_eq!(offset_of!(TsmRequest, req_len), 16);
    assert_eq!(offset_of!(TsmRequest, resp_len), 20);
    assert_eq!(offset_of!(TsmRequest, req_uptr), 24);
    assert_eq!(offset_of!(TsmRequest, resp_uptr), 32);
    assert_eq!(offset_of!(TsmRequest, tsm_code), 40);
    assert_eq!(size_of::<ValidateMmio>(), 24);
    assert_eq!(align_of::<ValidateMmio>(), 8);
    assert_eq!(offset_of!(ValidateMmio, gpa_base), 0);
    assert_eq!(offset_of!(ValidateMmio, gpa_top), 8);
    assert_eq!(offset_of!(ValidateMmio, pa_base), 16);
    assert_eq!(size_of::<ScalarRequest>(), 4);
    assert_eq!(size_of::<ReadObject>(), 16);
    assert_eq!(align_of::<ReadObject>(), 8);
    assert_eq!(offset_of!(ReadObject, reserved), 4);
    assert_eq!(offset_of!(ReadObject, offset), 8);
    assert_eq!(size_of::<RegenerateObject>(), 24);
    assert_eq!(align_of::<RegenerateObject>(), 8);
    assert_eq!(offset_of!(RegenerateObject, reserved), 4);
    assert_eq!(offset_of!(RegenerateObject, flags), 8);
    assert_eq!(offset_of!(RegenerateObject, nonce), 16);
}

#[test]
fn request_encoding_uses_native_states_and_zero_offset() {
    for (object, expected) in [
        (CcaObject::Vca, 0u32),
        (CcaObject::Certificate, 1),
        (CcaObject::Measurement, 2),
        (CcaObject::InterfaceReport, 3),
    ] {
        let (op, payload) = CcaTsmRequest::ObjectSize(object).encode();
        assert_eq!(op, 7);
        assert_eq!(payload.as_bytes(), expected.to_ne_bytes());
        let (op, payload) = CcaTsmRequest::ReadObject(object).encode();
        assert_eq!(op, 5);
        assert_eq!(&payload.as_bytes()[..4], expected.to_ne_bytes());
        assert_eq!(&payload.as_bytes()[4..], &[0; 12]);
    }
    for (state, expected) in [
        (CcaTdiState::Unlocked, 0u32),
        (CcaTdiState::Locked, 1),
        (CcaTdiState::Run, 2),
    ] {
        let (op, payload) = CcaTsmRequest::SetState(state).encode();
        assert_eq!(op, 2);
        assert_eq!(payload.as_bytes(), expected.to_ne_bytes());
    }
}

#[test]
fn regeneration_uses_borrowed_nonce_and_zero_reserved_fields() {
    let nonce = [0x59; 32];
    let (op, payload) = CcaTsmRequest::RegenerateMeasurements {
        flags: 1,
        nonce: &nonce,
    }
    .encode();
    assert_eq!(op, 6);
    let Payload::Regenerate(value) = payload else {
        panic!("wrong payload")
    };
    assert_eq!(value.object_type, 2);
    assert_eq!(value.reserved, 0);
    assert_eq!(value.flags, 1);
    assert_eq!(value.nonce, nonce.as_ptr() as u64);

    let (op, payload) = CcaTsmRequest::RegenerateInterfaceReport.encode();
    assert_eq!(op, 6);
    assert_eq!(&payload.as_bytes()[..4], 3u32.to_ne_bytes());
    assert_eq!(&payload.as_bytes()[4..], &[0; 20]);
}

#[test]
fn mmio_request_preserves_address_domains() {
    let (op, payload) = CcaTsmRequest::ValidateMmio {
        gpa_base: 0x4000,
        gpa_top: 0x6000,
        pa_base: 0x8000,
    }
    .encode();
    assert_eq!(op, 1);
    let Payload::Validate(value) = payload else {
        panic!("wrong payload")
    };
    assert_eq!(value.gpa_base, 0x4000);
    assert_eq!(value.gpa_top, 0x6000);
    assert_eq!(value.pa_base, 0x8000);
}

#[test]
fn outer_request_and_completion_preserve_all_result_channels() {
    let mut response = [0; 8];
    let pointer = response.as_mut_ptr() as u64;
    let result = execute(
        71,
        CcaTsmRequest::ReadObject(CcaObject::Vca),
        &mut response,
        |cmd| {
            assert_eq!(cmd.size, 48);
            assert_eq!(cmd.vdevice_id, 71);
            assert_eq!(cmd.op, 5);
            assert_eq!(cmd.tvm_arch, 1);
            assert_eq!(cmd.req_len, 16);
            assert_eq!(cmd.resp_len, 8);
            assert_ne!(cmd.req_uptr, 0);
            assert_eq!(cmd.resp_uptr, pointer);
            assert_eq!(cmd.tsm_code, 0);
            cmd.tsm_code = 0xabc;
            Ok(3)
        },
    )
    .unwrap();
    assert_eq!(
        result,
        TsmCompletion {
            residue: 3,
            tsm_code: 0xabc
        }
    );

    let error = execute(
        71,
        CcaTsmRequest::SetState(CcaTdiState::Locked),
        &mut [],
        |cmd| {
            assert_eq!(cmd.resp_len, 0);
            assert_eq!(cmd.resp_uptr, 0);
            cmd.tsm_code = 9;
            Err(Errno::EFAULT)
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TsmRequestError::Ioctl {
            errno: Errno::EFAULT,
            tsm_code: 9
        }
    ));
}

#[test]
fn residue_is_bounded_by_response_or_request() {
    for residue in [0, 1, 4] {
        let result = execute(
            1,
            CcaTsmRequest::SetState(CcaTdiState::Unlocked),
            &mut [],
            |_| Ok(residue),
        )
        .unwrap();
        assert_eq!(result.residue, residue as u32);
    }
    let error = execute(
        1,
        CcaTsmRequest::SetState(CcaTdiState::Unlocked),
        &mut [],
        |_| Ok(5),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TsmRequestError::InvalidResidue {
            residue: 5,
            capacity: 4,
            ..
        }
    ));
    let error = execute(
        1,
        CcaTsmRequest::ReadObject(CcaObject::Certificate),
        &mut [0; 8],
        |cmd| {
            cmd.tsm_code = 7;
            Ok(9)
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TsmRequestError::InvalidResidue {
            residue: 9,
            capacity: 8,
            tsm_code: 7
        }
    ));
}

#[test]
fn invalid_response_shapes_never_issue_a_request() {
    for (request, length) in [
        (CcaTsmRequest::ObjectSize(CcaObject::Certificate), 0),
        (CcaTsmRequest::ObjectSize(CcaObject::Certificate), 3),
        (CcaTsmRequest::ObjectSize(CcaObject::Certificate), 5),
        (CcaTsmRequest::ReadObject(CcaObject::Certificate), 0),
        (CcaTsmRequest::SetState(CcaTdiState::Run), 4),
        (CcaTsmRequest::RegenerateInterfaceReport, 4),
    ] {
        let error = execute(1, request, &mut vec![0; length], |_| {
            panic!("ioctl must not run")
        })
        .unwrap_err();
        assert!(matches!(error, TsmRequestError::ResponseLength { .. }));
    }
}

#[test]
fn kernel_errno_is_not_a_successful_tsm_code() {
    let ctx = IommufdCtx::from_file(std::fs::File::open("/dev/null").unwrap());
    let error = ctx
        .cca_tsm_request(1, CcaTsmRequest::ObjectSize(CcaObject::Vca), &mut [0; 4])
        .unwrap_err();
    assert!(matches!(
        error,
        TsmRequestError::Ioctl {
            errno: Errno::ENOTTY,
            tsm_code: 0
        }
    ));
}

#[test]
fn invalid_signed_completion_preserves_raw_value() {
    let error = execute(
        1,
        CcaTsmRequest::SetState(CcaTdiState::Run),
        &mut [],
        |cmd| {
            cmd.tsm_code = 8;
            Ok(-2)
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TsmRequestError::InvalidReturn {
            value: -2,
            tsm_code: 8
        }
    ));
}
