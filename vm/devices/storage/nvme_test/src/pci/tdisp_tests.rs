// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Containment checks through both controller MMIO dispatch paths.

use super::*;
use crate::tdisp::MutationFault;
use crate::tests::test_helpers::TestNvmeMmioRegistration;
use mesh::CellUpdater;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::msi::MsiConnection;
use tdisp::Command;
use tdisp::GuestToHostCommand;
use tdisp::GuestToHostResponse;
use tdisp::GuestToHostResponseExt;
use tdisp::TdispGuestOperationErrorCode;
use tdisp::TdispMmioRangeAction;
use tdisp::TdispTdiState;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp_proto::TdispCommandRequestBind;
use tdisp_proto::TdispCommandRequestGetDeviceInterfaceInfo;
use tdisp_proto::TdispCommandRequestModifyMmioRange;
use tdisp_proto::TdispCommandRequestStartTdi;
use tdisp_proto::TdispCommandRequestUnbind;
use tdisp_proto::TdispGuestUnbindReason;
use vmcore::vm_task::SingleDriverBackend;

const BAR4: u64 = 0x10_0000;
const INTMS: u64 = 0x0c;

fn controller(driver: DefaultDriver) -> NvmeFaultController {
    let mut registration = TestNvmeMmioRegistration {};
    let msi = MsiConnection::new();
    let mut controller = NvmeFaultController::new(
        &VmTaskDriverSource::new(SingleDriverBackend::new(driver)),
        GuestMemory::allocate(0x1000),
        &msi.target(),
        &mut registration,
        NvmeFaultControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
        },
        FaultConfiguration::new(CellUpdater::new(false).cell()),
        true,
    );
    for (offset, value) in [(0x10, 0), (0x20, BAR4 as u32), (4, 6)] {
        controller
            .pci_cfg_write(offset, ByteEnabledDwordWrite::with_all_bytes_enabled(value))
            .unwrap();
    }
    controller
}

fn command(controller: &mut NvmeFaultController, command: Command) -> GuestToHostResponse {
    controller
        .supports_tdisp_host()
        .unwrap()
        .tdisp_handle_guest_command(GuestToHostCommand {
            device_id: 0,
            command: Some(command),
        })
        .unwrap()
}

fn success(controller: &mut NvmeFaultController, request: Command) {
    assert_eq!(
        command(controller, request).error_code(),
        Some(TdispGuestOperationErrorCode::Success)
    );
}

fn negotiate() -> Command {
    Command::GetDeviceInterfaceInfo(TdispCommandRequestGetDeviceInterfaceInfo {
        guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
    })
}

fn unblock() -> Command {
    Command::ModifyMmioRange(TdispCommandRequestModifyMmioRange {
        action: TdispMmioRangeAction::UnblockMmioRange as i32,
        range_id: 0,
        gpa_base: 0,
        range_len_bytes: BAR0_LEN,
    })
}

fn bind(controller: &mut NvmeFaultController) {
    success(controller, negotiate());
    success(controller, Command::Bind(TdispCommandRequestBind {}));
    success(
        controller,
        Command::StartTdi(TdispCommandRequestStartTdi {}),
    );
}

fn read(controller: &mut NvmeFaultController, addr: u64) -> u32 {
    let mut bytes = [0; 4];
    controller.mmio_read(addr, &mut bytes).unwrap();
    u32::from_ne_bytes(bytes)
}

fn write(controller: &mut NvmeFaultController, addr: u64, value: u32) {
    controller.mmio_write(addr, &value.to_ne_bytes()).unwrap();
}

#[async_test]
async fn callback_failure_and_unwind_deny_bar0_and_shared_msix(driver: DefaultDriver) {
    for after_effect in [false, true] {
        for panic in [false, true] {
            let mut controller = controller(driver.clone());
            bind(&mut controller);
            // Shared MSI-X remains usable without a TDI range acceptance.
            write(&mut controller, BAR4, 0xfee0_0000);
            assert_eq!(read(&mut controller, BAR4), 0xfee0_0000);
            let msix_before = controller.msix.read_u32(0);
            let mask_before = controller.registers.interrupt_mask;
            let ranges = controller.tdisp_mmio_ranges.as_ref().unwrap().clone();
            ranges.inject_fault(MutationFault {
                after_effect,
                panic,
            });

            if panic {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        command(&mut controller, unblock())
                    }))
                    .is_err()
                );
            } else {
                let response = command(&mut controller, unblock());
                assert_eq!(
                    response.error_code(),
                    Some(TdispGuestOperationErrorCode::HostFailedToProcessCommand)
                );
                assert_eq!(response.tdi_state_before_enum(), Some(TdispTdiState::Run));
                assert_eq!(
                    response.tdi_state_after_enum(),
                    Some(TdispTdiState::Uninitialized)
                );
            }
            assert_eq!(ranges.recorded_unblocked(BAR0_RANGE_ID), after_effect);
            assert!(!ranges.is_allowed());
            assert_eq!(read(&mut controller, 0), u32::MAX);
            assert_eq!(read(&mut controller, BAR4), u32::MAX);
            write(&mut controller, INTMS, 0x55);
            write(&mut controller, BAR4, 0x1234_5000);
            assert_eq!(controller.registers.interrupt_mask, mask_before);
            assert_eq!(controller.msix.read_u32(0), msix_before);

            // Device reset, negotiation, Unbind, and Unblock cannot reopen it.
            controller.reset().await;
            for retry in [
                negotiate(),
                unblock(),
                Command::Unbind(TdispCommandRequestUnbind {
                    unbind_reason: TdispGuestUnbindReason::Graceful as i32,
                }),
            ] {
                let response = command(&mut controller, retry);
                assert_eq!(
                    response.error_code(),
                    Some(TdispGuestOperationErrorCode::HostFailedToProcessCommand)
                );
                assert_eq!(
                    response.tdi_state_after_enum(),
                    Some(TdispTdiState::Uninitialized)
                );
            }
            assert_eq!(read(&mut controller, 0), u32::MAX);
            assert_eq!(read(&mut controller, BAR4), u32::MAX);
        }
    }
}

#[async_test]
async fn healthy_unbind_rebind_keeps_shared_msix_and_reopens_accepted_bar0(driver: DefaultDriver) {
    let mut controller = controller(driver);
    write(&mut controller, BAR4, 0xfee0_0000);
    assert_eq!(read(&mut controller, BAR4), 0xfee0_0000);
    bind(&mut controller);
    success(&mut controller, unblock());
    write(&mut controller, INTMS, 0x12);
    assert_eq!(read(&mut controller, INTMS), 0x12);
    for _ in 0..2 {
        success(
            &mut controller,
            Command::Unbind(TdispCommandRequestUnbind {
                unbind_reason: TdispGuestUnbindReason::Graceful as i32,
            }),
        );
        assert_eq!(read(&mut controller, 0), u32::MAX);
        write(&mut controller, BAR4, 0xfee0_1000);
        assert_eq!(read(&mut controller, BAR4), 0xfee0_1000);
    }
    bind(&mut controller);
    success(&mut controller, unblock());
    assert_ne!(read(&mut controller, 0), u32::MAX);
    write(&mut controller, INTMS, 0x40);
    assert_eq!(read(&mut controller, INTMS), 0x52);
}
