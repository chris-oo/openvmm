// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::TdispAccess;
use crate::TdispAccessGate;
use crate::TdispGuestRequestInterface;
use crate::TdispHostDeviceInterface;
use crate::TdispHostDeviceTargetEmulator;
use crate::TdispHostStateMachine;
use crate::test_helpers::TDISP_MOCK_DEVICE_ID;
use crate::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use crate::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;
use parking_lot::Mutex;
use std::sync::Arc;
use tdisp_proto::TdispDeviceInterfaceInfo;
use tdisp_proto::TdispGuestProtocolType;
use tdisp_proto::TdispMmioRangeAction;
use tdisp_proto::TdispReportType;

#[derive(Debug, PartialEq, Clone)]
pub enum LastCall {
    NegotiateProtocol,
    BindDevice,
    StartDevice,
    UnbindDevice,
    GetDeviceReport(TdispReportType),
    ModifyMmioRange {
        action: TdispMmioRangeAction,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    },
}

pub struct TrackingHostInterface {
    last_call: Arc<Mutex<Option<LastCall>>>,
    report_buffer: Arc<Mutex<Vec<u8>>>,
    control: Arc<Mutex<FaultControl>>,
    gate: TdispAccessGate,
}

#[derive(Default)]
pub struct FaultControl {
    pub fail: Option<LastCall>,
    pub after_effect: bool,
    pub panic: bool,
    pub calls: Vec<LastCall>,
    pub effects: usize,
}

impl TrackingHostInterface {
    fn call(&mut self, call: LastCall, mutation: bool) -> anyhow::Result<()> {
        assert_eq!(self.gate.is_allowed(), !mutation);
        *self.last_call.lock() = Some(call.clone());
        let mut control = self.control.lock();
        control.calls.push(call.clone());
        let fail = control.fail == Some(call);
        if !fail || control.after_effect {
            control.effects += usize::from(mutation);
        }
        if fail {
            assert!(!control.panic, "injected callback unwind");
            anyhow::bail!("injected callback failure");
        }
        Ok(())
    }
}

impl TdispHostDeviceInterface for TrackingHostInterface {
    fn tdisp_bind_device(&mut self) -> anyhow::Result<()> {
        self.call(LastCall::BindDevice, true)
    }

    fn tdisp_start_device(&mut self) -> anyhow::Result<()> {
        self.call(LastCall::StartDevice, true)
    }

    fn tdisp_unbind_device(&mut self) -> anyhow::Result<()> {
        self.call(LastCall::UnbindDevice, true)
    }

    fn tdisp_modify_mmio_range(
        &mut self,
        action: TdispMmioRangeAction,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        self.call(
            LastCall::ModifyMmioRange {
                action,
                range_id,
                gpa_base,
                range_len_bytes,
            },
            true,
        )
    }

    /// Returns a mock report buffer that is configurable.
    fn tdisp_get_device_report(&mut self, report_type: TdispReportType) -> anyhow::Result<Vec<u8>> {
        self.call(LastCall::GetDeviceReport(report_type), false)?;
        match report_type {
            TdispReportType::InterfaceReport => Ok(self.report_buffer.lock().clone()),
            // The guest device ID is served in any TDI state, so the mock has
            // to answer it too. The wire format is a little-endian u64.
            TdispReportType::GuestDeviceId => Ok(TDISP_MOCK_DEVICE_ID.to_le_bytes().to_vec()),
            _ => Err(anyhow::anyhow!(
                "mock test checks only InterfaceReport and GuestDeviceId requests"
            )),
        }
    }

    fn tdisp_negotiate_protocol(
        &mut self,
        _requested_guest_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        self.call(LastCall::NegotiateProtocol, false)?;
        Ok(TdispDeviceInterfaceInfo {
            guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
            supported_features: TDISP_MOCK_SUPPORTED_FEATURES,
            tdisp_device_id: TDISP_MOCK_DEVICE_ID,
        })
    }
}

/// Mock host emulator that records calls and provides a report buffer that is configurable.
pub struct MockHostEmulator {
    pub emulator: TdispHostDeviceTargetEmulator,
    pub last_call: Arc<Mutex<Option<LastCall>>>,
    pub report_buffer: Arc<Mutex<Vec<u8>>>,
    pub control: Arc<Mutex<FaultControl>>,
    pub gate: TdispAccessGate,
}

pub fn new_emulator() -> MockHostEmulator {
    let last_call: Arc<Mutex<Option<LastCall>>> = Arc::new(Mutex::new(None));
    let report_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    let control = Arc::new(Mutex::new(FaultControl::default()));
    let access = TdispAccess::new();
    let gate = access.gate();
    let interface = TrackingHostInterface {
        last_call: last_call.clone(),
        report_buffer: report_buffer.clone(),
        control: control.clone(),
        gate: gate.clone(),
    };
    let emulator = TdispHostDeviceTargetEmulator::new(interface, access, "test-device");
    MockHostEmulator {
        emulator,
        last_call,
        report_buffer,
        control,
        gate,
    }
}

pub struct MockTdiStateMachine {
    pub machine: TdispHostStateMachine,
    pub last_call: Arc<Mutex<Option<LastCall>>>,
    pub control: Arc<Mutex<FaultControl>>,
    pub gate: TdispAccessGate,
}

/// Returns a fresh state machine paired with a handle for inspecting which
/// host-interface method was called most recently.
pub fn new_machine() -> MockTdiStateMachine {
    let last_call: Arc<Mutex<Option<LastCall>>> = Arc::new(Mutex::new(None));
    let report_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    let control = Arc::new(Mutex::new(FaultControl::default()));
    let access = TdispAccess::new();
    let gate = access.gate();
    let interface = TrackingHostInterface {
        last_call: last_call.clone(),
        report_buffer: report_buffer.clone(),
        control: control.clone(),
        gate: gate.clone(),
    };

    let mut machine = TdispHostStateMachine::new(interface, access);

    // Forcibly negotiate any protocol to avoid the need for a test to do it.
    // This otherwise doesn't affect test behavior right now.
    machine
        .tdisp_negotiate_protocol(TDISP_MOCK_GUEST_PROTOCOL)
        .unwrap();

    // Reset last_call to avoid interference from the emulator.
    *last_call.lock() = None;

    MockTdiStateMachine {
        machine,
        last_call,
        control,
        gate,
    }
}
