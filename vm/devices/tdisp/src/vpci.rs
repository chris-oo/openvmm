// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VPCI protocol policy and responses over the shared host lifecycle engine.

use crate::host::lifecycle::AdmissionError;
use crate::host::lifecycle::ConfirmedState;
use crate::host::lifecycle::DeviceState;
use crate::host::lifecycle::Lifecycle;
use crate::host::lifecycle::Mutation;
use crate::host::lifecycle::MutationError;
use anyhow::Context;
use std::future::Future;
use std::pin::Pin;
pub use tdisp_proto::GuestToHostCommand;
pub use tdisp_proto::GuestToHostCommandExt;
pub use tdisp_proto::GuestToHostResponse;
pub use tdisp_proto::GuestToHostResponseExt;
pub use tdisp_proto::TdispCommandResponseBind;
pub use tdisp_proto::TdispCommandResponseGetDeviceInterfaceInfo;
pub use tdisp_proto::TdispCommandResponseGetTdiReport;
pub use tdisp_proto::TdispCommandResponseModifyMmioRange;
pub use tdisp_proto::TdispCommandResponseStartTdi;
pub use tdisp_proto::TdispCommandResponseUnbind;
pub use tdisp_proto::TdispDeviceInterfaceInfo;
pub use tdisp_proto::TdispGuestOperationError;
pub use tdisp_proto::TdispGuestOperationErrorCode;
pub use tdisp_proto::TdispGuestProtocolType;
pub use tdisp_proto::TdispGuestUnbindReason;
pub use tdisp_proto::TdispMmioRangeAction;
pub use tdisp_proto::TdispReportType;
pub use tdisp_proto::TdispTdiState;
pub use tdisp_proto::guest_to_host_command::Command;
pub use tdisp_proto::guest_to_host_response::Response;

use tracing::instrument;

mod access;
pub use access::TdispAccess;
pub use access::TdispAccessGate;

/// Callback for receiving TDISP commands from the guest.
pub type TdispCommandCallback = dyn Fn(&GuestToHostCommand) -> anyhow::Result<()> + Send + Sync;

/// Describes the interface that host software should implement to provide TDISP
/// functionality for a device. These interfaces might dispatch to a physical
/// device, or might be implemented by a software emulator.
///
/// The state machine owns this interface exclusively. Do not retain aliases
/// that can issue mutations outside it. Negotiation and report reads must not
/// mutate device lifecycle or access. All emulated MMIO paths must check the
/// gate of the [`TdispAccess`] transferred with this interface.
pub trait TdispHostDeviceInterface: Send + Sync {
    /// Request versioning and protocol negotiation from the host.
    fn tdisp_negotiate_protocol(
        &mut self,
        _requested_guest_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo>;

    /// Bind a tdi device to the current partition. Transitions device to the Locked
    /// state from Unlocked.
    fn tdisp_bind_device(&mut self) -> anyhow::Result<()>;

    /// Start a bound device by transitioning it to the Run state from the Locked state.
    /// This allows attestation and resources to be accepted into the guest context.
    fn tdisp_start_device(&mut self) -> anyhow::Result<()>;

    /// Unbind a tdi device from the current partition.
    fn tdisp_unbind_device(&mut self) -> anyhow::Result<()>;

    /// Get a device interface report for the device.
    fn tdisp_get_device_report(&mut self, _report_type: TdispReportType)
    -> anyhow::Result<Vec<u8>>;

    /// Block or unblock an MMIO range in the guest's private context.
    ///
    /// The TDI is guaranteed to be Locked or Run; every other state is
    /// rejected before this is reached.
    ///
    /// * `action` - Whether the range is being blocked or unblocked. Never
    ///   [`TdispMmioRangeAction::Invalid`].
    /// * `range_id` - Identifies which MMIO range is being modified (the PCI
    ///   BAR index).
    /// * `gpa_base` - The guest physical base address of the range.
    /// * `range_len_bytes` - The length of the range, in bytes.
    fn tdisp_modify_mmio_range(
        &mut self,
        action: TdispMmioRangeAction,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()>;
}

/// Trait added to host virtual devices to dispatch TDISP commands from guests.
pub trait TdispHostDeviceTarget: Send + Sync {
    /// Dispatch a TDISP command received from a guest.
    fn tdisp_handle_guest_command(
        &mut self,
        _command: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse>;
}

/// Isolation classification for a single VPCI resource (a BAR or DMA).
///
/// This mirrors the `VPCI_RESOURCE_ISOLATION` values used on the wire by
/// `VpciMsgQueryIsolatedResources`, but is defined here so that
/// `chipset_device` and `tdisp` can expose an isolation-reporter trait
/// without taking a dependency on `vpci_protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TdispResourceIsolation {
    /// Host-visible and modifiable by the host.
    Shared,
    /// Host-inaccessible after TDI validation and private to the guest.
    Private,
    /// There is no resource here to classify. Either the BAR is invalid or part
    /// of a 64-bit BAR.
    Invalid,
}

/// Classification of a device's BAR and DMA isolation for the VPCI
/// `QueryIsolatedResources` message, reported by the guest-facing VPCI
/// server.
#[derive(Debug, Clone, Copy)]
pub enum TdispIsolationReport {
    /// The chipset device wraps a non-TDISP device.
    NotTdispCapable,
    /// The TDI is not in a state that it can respond to the isolation report
    /// request.
    NotReady,
    /// The TDI has attested and parsed its report successfully. The inner
    /// arrays give the six per-BAR classifications and the DMA classification.
    /// Guaranteed to contain only `Shared` / `Private`.
    Ready {
        /// Per-BAR isolation. Index `i` corresponds to BAR `i`.
        bars: [TdispResourceIsolation; 6],
        /// DMA path isolation.
        dma: TdispResourceIsolation,
    },
    /// An internal paravisor error prevented reading the isolation state. The
    /// paravisor should answer with an error status and log the event.
    Error,
}

/// Trait added to chipset devices that want to relay TDISP on behalf of the
/// guest-facing virtual bus.
pub trait TdispRelayedDeviceTarget: Send + Sync {
    /// Return a snapshot of the current isolation state containing what
    /// resources were isolated or shared by the TDISP relay and attestation
    /// flow.
    fn tdisp_isolation_report(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TdispIsolationReport> + Send + 'static>>;
}

/// An emulator which runs the TDISP state machine for a synthetic device.
pub struct TdispHostDeviceTargetEmulator {
    machine: TdispHostStateMachine,
    debug_device_id: String,
}

impl TdispHostDeviceTargetEmulator {
    /// Create a new emulator which runs the TDISP state machine for a synthetic device.
    pub fn new(
        host_interface: impl TdispHostDeviceInterface + 'static,
        access: TdispAccess,
        debug_device_id: &str,
    ) -> Self {
        Self {
            machine: TdispHostStateMachine::new(host_interface, access),
            debug_device_id: debug_device_id.to_owned(),
        }
    }

    /// Set the debug device ID string.
    pub fn set_debug_device_id(&mut self, debug_device_id: &str) {
        self.machine.set_debug_device_id(debug_device_id.to_owned());
        self.debug_device_id = debug_device_id.to_owned();
    }

    /// Reset the emulator's transport. This does not recover quarantine.
    pub fn reset(&self) {}
}

impl TdispHostDeviceTarget for TdispHostDeviceTargetEmulator {
    /// Main entry point for handling a guest command sent to the host.
    /// Dispatches relevant trait interface methods to handle the command.
    /// Formats and returns a response packet.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn tdisp_handle_guest_command(
        &mut self,
        command: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse> {
        let mut error = TdispGuestOperationError::Success;
        let mut response: Option<Response> = None;
        let state_before = self.machine.state();
        match &command.command {
            _ if self.machine.ensure_healthy().is_err() => {
                error = TdispGuestOperationError::HostFailedToProcessCommand;
            }
            Some(Command::GetDeviceInterfaceInfo(req)) => {
                let protocol_type = TdispGuestProtocolType::from_i32(req.guest_protocol_type);

                match protocol_type {
                    Some(protocol_type) => {
                        let interface_info = self.machine.tdisp_negotiate_protocol(protocol_type);
                        match interface_info {
                            Ok(interface_info) => {
                                response = Some(Response::GetDeviceInterfaceInfo(
                                    TdispCommandResponseGetDeviceInterfaceInfo {
                                        interface_info: Some(interface_info),
                                    },
                                ));
                            }
                            Err(err) => {
                                error = err;
                            }
                        }
                    }
                    None => {
                        error = TdispGuestOperationError::InvalidGuestProtocolRequest;
                    }
                }
            }
            Some(Command::Bind(_)) => {
                let bind_res = self.machine.request_lock_device_resources();
                if let Err(err) = bind_res {
                    error = err;
                } else {
                    response = Some(Response::Bind(TdispCommandResponseBind {}));
                }
            }
            Some(Command::StartTdi(_)) => {
                let start_tdi_res = self.machine.request_start_tdi();
                if let Err(err) = start_tdi_res {
                    error = err;
                } else {
                    response = Some(Response::StartTdi(TdispCommandResponseStartTdi {}));
                }
            }
            Some(Command::Unbind(cmd)) => {
                let unbind_reason = TdispGuestUnbindReason::from_i32(cmd.unbind_reason);

                match unbind_reason {
                    Some(reason) => {
                        let unbind_res = self.machine.request_unbind(reason);
                        if let Err(err) = unbind_res {
                            error = err;
                        }
                        response = Some(Response::Unbind(TdispCommandResponseUnbind {}));
                    }
                    None => {
                        error = TdispGuestOperationError::InvalidGuestUnbindReason;
                    }
                }
            }
            Some(Command::GetTdiReport(cmd)) => {
                let report_type = TdispReportType::from_i32(cmd.report_type);
                match report_type {
                    Some(report_type) => {
                        let report_buffer = self.machine.request_attestation_report(report_type);

                        match report_buffer {
                            Ok(report_buffer) => {
                                response = Some(Response::GetTdiReport(
                                    TdispCommandResponseGetTdiReport {
                                        report_type: cmd.report_type,
                                        report_buffer,
                                    },
                                ));
                            }
                            Err(err) => {
                                error = err;
                            }
                        }
                    }
                    None => {
                        error = TdispGuestOperationError::InvalidGuestAttestationReportType;
                    }
                }
            }
            Some(Command::ModifyMmioRange(cmd)) => {
                let action = TdispMmioRangeAction::from_i32(cmd.action);

                // `range_id` is a BAR index here, but not necessarily for all devices.
                // Future platforms might support sub-BAR ranges by the TDISP spec.
                match (action, u16::try_from(cmd.range_id)) {
                    (Some(action), Ok(range_id)) => {
                        let modify_res = self.machine.request_modify_mmio_range(
                            action,
                            range_id,
                            cmd.gpa_base,
                            cmd.range_len_bytes,
                        );
                        if let Err(err) = modify_res {
                            error = err;
                        } else {
                            response = Some(Response::ModifyMmioRange(
                                TdispCommandResponseModifyMmioRange {},
                            ));
                        }
                    }
                    (None, _) => {
                        tracing::error!(
                            action = cmd.action,
                            "ModifyMmioRange action is not a valid TdispMmioRangeAction"
                        );
                        error = TdispGuestOperationError::InvalidGuestCommandId;
                    }
                    (_, Err(_)) => {
                        tracing::error!(
                            range_id = cmd.range_id,
                            "ModifyMmioRange range_id does not fit in a u16"
                        );
                        error = TdispGuestOperationError::InvalidGuestCommandId;
                    }
                }
            }
            _ => {
                error = TdispGuestOperationError::InvalidGuestCommandId;
            }
        }
        let state_after = self.machine.state();
        let error_code: TdispGuestOperationErrorCode = error.into();
        let resp = GuestToHostResponse {
            result: error_code.into(),
            tdi_state_before: state_before.into(),
            tdi_state_after: state_after.into(),
            response,
        };

        match error {
            TdispGuestOperationError::Success => {
                tracing::info!(?resp, "tdisp_handle_guest_command success");
            }
            _ => {
                tracing::error!(?resp, "tdisp_handle_guest_command error");
            }
        }

        Ok(resp)
    }
}

/// Trait implemented by TDISP-capable devices on the client side. This includes devices that
/// are assigned to isolated partitions other than the host.
pub trait TdispClientDevice: Send + Sync {
    /// Send a TDISP command to the host for this device.
    /// TODO TDISP: Async? Better handling of device_id in GuestToHostCommand?
    fn tdisp_command_to_host(&self, command: GuestToHostCommand) -> anyhow::Result<()>;
}

/// Maximum retained protocol cleanup reasons, independent of lifecycle history.
const TDISP_UNBIND_HISTORY_LEN: usize = 10;

/// The reason for an `Unbind` call. This can be guest or host initiated.
/// `Unbind` can be called any time during the assignment flow.
/// This is used for telemetry and debugging.
#[derive(Debug)]
pub enum TdispUnbindReason {
    /// Unknown reason.
    Unknown(anyhow::Error),

    /// The device was unbound manually by the guest or host for a non-error reason.
    GuestInitiated(TdispGuestUnbindReason),

    /// The device attempted to perform an invalid state transition.
    ImpossibleStateTransition(anyhow::Error),

    /// The guest tried to transition the device to the Locked state while the device was not
    /// in the Unlocked state.
    InvalidGuestTransitionToLocked,

    /// The guest tried to transition the device to the Run state while the device was not
    /// in the Locked state.
    InvalidGuestTransitionToRun,

    /// The guest tried to retrieve the attestation report while the device was not in the
    /// Locked or Run state.
    InvalidGuestGetAttestationReportState,

    /// The guest tried to accept the attestation report while the device was not in the
    /// Locked or Run state.
    InvalidGuestAcceptAttestationReportState,

    /// The guest tried to unbind the device while the device with an unbind reason that is
    /// not recognized as a valid guest unbind reason. The unbind still succeeds but the
    /// recorded reason is discarded.
    InvalidGuestUnbindReason(anyhow::Error),
}

/// The state machine for the TDISP assignment flow for a device on the host. Both the guest and host
/// synchronize this state machine with each other as they move through the assignment flow.
pub struct TdispHostStateMachine {
    lifecycle: Lifecycle,
    access: TdispAccess,
    /// The device ID of the device being assigned.
    debug_device_id: String,
    /// A record of the last unbind reasons for the device.
    unbind_reason_history: Vec<TdispUnbindReason>,
    /// Calls back into the host to perform TDISP actions.
    host_interface: Box<dyn TdispHostDeviceInterface>,
    /// The guest protocol type that was negotiated with the host interface.
    guest_protocol_type: TdispGuestProtocolType,
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct BackendError(anyhow::Error);

impl TdispHostStateMachine {
    /// Create a new TDISP state machine with the `Unlocked` state.
    ///
    /// Transfer exclusive callback and access ownership together. All frontend
    /// MMIO paths must already use the read-only gate from `access`.
    pub fn new(
        host_interface: impl TdispHostDeviceInterface + 'static,
        access: TdispAccess,
    ) -> Self {
        Self {
            lifecycle: Lifecycle::new(ConfirmedState::Unlocked),
            access,
            debug_device_id: "".to_owned(),
            unbind_reason_history: Vec::new(),
            host_interface: Box::new(host_interface),
            guest_protocol_type: TdispGuestProtocolType::Invalid,
        }
    }

    /// Set the debug device ID string.
    pub fn set_debug_device_id(&mut self, debug_device_id: String) {
        self.debug_device_id = debug_device_id;
    }

    /// Get the current state of the TDI.
    pub(crate) fn state(&self) -> TdispTdiState {
        match self.lifecycle.state() {
            DeviceState::Confirmed(ConfirmedState::Unlocked) => TdispTdiState::Unlocked,
            DeviceState::Confirmed(ConfirmedState::Locked) => TdispTdiState::Locked,
            DeviceState::Confirmed(ConfirmedState::Running) => TdispTdiState::Run,
            DeviceState::Quarantined { .. } | DeviceState::TornDown => TdispTdiState::Uninitialized,
        }
    }

    fn ensure_healthy(&self) -> Result<(), TdispGuestOperationError> {
        self.lifecycle
            .confirmed()
            .map(|_| ())
            .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)
    }

    fn ensure_negotiated_protocol(&self) -> anyhow::Result<()> {
        if self.guest_protocol_type == TdispGuestProtocolType::Invalid {
            tracing::error!(
                "Guest tried to perform a state transition without negotiating a protocol with the host!"
            );
            return Err(anyhow::anyhow!(
                "Guest tried to perform a state transition without negotiating a protocol with the host!"
            ));
        }
        Ok(())
    }

    fn execute(
        &mut self,
        operation: Mutation,
        call: impl FnOnce(&mut dyn TdispHostDeviceInterface) -> anyhow::Result<()>,
    ) -> Result<(), MutationError<BackendError>> {
        self.lifecycle
            .execute(operation, || {
                let permit = self.access.begin().map_err(BackendError)?;
                call(self.host_interface.as_mut()).map_err(BackendError)?;
                permit.complete().map_err(BackendError)
            })
            .inspect_err(|error| {
                if let MutationError::Backend(error) = error {
                    // A failed mutation is logged once: quarantine rejects retries.
                    tracing::error!(?error, "TDISP backend mutation failed");
                }
            })
    }

    fn transition(
        &mut self,
        to: ConfirmedState,
        call: impl FnOnce(&mut dyn TdispHostDeviceInterface) -> anyhow::Result<()>,
        invalid_reason: TdispUnbindReason,
    ) -> Result<(), TdispGuestOperationError> {
        match self.execute(Mutation::SetState(to), call) {
            Ok(()) => Ok(()),
            Err(MutationError::Admission(AdmissionError::InvalidTransition { .. })) => {
                self.unbind_all(invalid_reason)?;
                Err(TdispGuestOperationError::InvalidDeviceState)
            }
            Err(_) => Err(TdispGuestOperationError::HostFailedToProcessCommand),
        }
    }

    /// Unbind a healthy device. Failed cleanup cannot recover quarantine.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn unbind_all(&mut self, reason: TdispUnbindReason) -> Result<(), TdispGuestOperationError> {
        tracing::info!("Unbind called with reason {:?}", reason);

        self.execute(Mutation::Unbind, |host| host.tdisp_unbind_device())
            .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;

        // Record the unbind reason
        if self.unbind_reason_history.len() == TDISP_UNBIND_HISTORY_LEN {
            self.unbind_reason_history.remove(0);
        }
        self.unbind_reason_history.push(reason);

        Ok(())
    }
}

/// Represents an interface by which guest commands can be dispatched to a
/// backing TDISP state handler in the host. This could be an emulated TDISP device or an
/// assigned TDISP device that is actually connected to the guest.
pub trait TdispGuestRequestInterface {
    /// Before a guest can communicate with the host, the guest must negotiate a
    /// protocol with the host. This is done by calling this function with the
    /// guest's desired protocol type. The host responds with the protocol that
    /// it will use to communicate with the guest and includes information about
    /// the TDISP capabilities of the device.
    ///
    /// If the host reports that this device not TDISP capable,
    /// [`TdispDeviceInterfaceInfo::guest_protocol_type`] will be
    /// [`TdispGuestProtocolType::Invalid`].
    fn tdisp_negotiate_protocol(
        &mut self,
        requested_guest_protocol: TdispGuestProtocolType,
    ) -> Result<TdispDeviceInterfaceInfo, TdispGuestOperationError>;

    /// Transition the device from the Unlocked to Locked state. This takes place after the
    /// device has been assigned to the guest partition and the resources for the device have
    /// been configured by the guest by not yet validated.
    /// The device will in the `Locked` state can still perform unencrypted operations until it has
    /// been transitioned to the `Run` state. The device will be attested and moved to the `Run` state.
    ///
    /// Attempting to transition the device to the `Locked` state while the device is not in the
    /// `Unlocked` state will cause an error and unbind the device.
    fn request_lock_device_resources(&mut self) -> Result<(), TdispGuestOperationError>;

    /// Transition the device from the Locked to the Run state. This takes place after the
    /// device has been assigned resources and the resources have been locked to the guest.
    /// The device will then transition to the `Run` state, where it will be non-functional
    /// until the guest undergoes attestation and resources are accepted into the guest context.
    ///
    /// Attempting to transition the device to the `Run` state while the device is not in the
    /// `Locked` state will cause an error and unbind the device.
    fn request_start_tdi(&mut self) -> Result<(), TdispGuestOperationError>;

    /// Block or unblock an MMIO range in the guest's private context. The
    /// device must be in the `Locked` or `Run` state.
    ///
    /// Unlike the transitions above, requesting this in the wrong state returns
    /// an error *without* unbinding the device: the guest may legitimately
    /// retry as BARs are reprogrammed. This does not transition the device.
    fn request_modify_mmio_range(
        &mut self,
        action: TdispMmioRangeAction,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> Result<(), TdispGuestOperationError>;

    /// Retrieves the attestation report for the device when the device is in the `Locked` or
    /// `Run` state. The device resources will not be functional until the
    /// resources have been accepted into the guest while the device is in the
    /// `Run` state.
    ///
    /// Attempting to retrieve the attestation report while the device is not in
    /// the `Locked` or `Run` state will cause an error and unbind the device.
    ///
    /// [`TdispReportType::GuestDeviceId`] is exempt from that state
    /// requirement and can be requested in any healthy state, since it identifies the
    /// device rather than describing attestation state.
    fn request_attestation_report(
        &mut self,
        report_type: TdispReportType,
    ) -> Result<Vec<u8>, TdispGuestOperationError>;

    /// Guest initiates a graceful unbind of the device. The guest might
    /// initiate an unbind for a variety of reasons:
    ///  - Device is being detached/deactivated and is no longer needed in a functional state
    ///  - Device is powering down or entering a reset
    ///
    /// The device will transition to the `Unlocked` state. The guest can call
    /// this function in any healthy state to return to `Unlocked`. It cannot
    /// recover a quarantined interface.
    fn request_unbind(
        &mut self,
        reason: TdispGuestUnbindReason,
    ) -> Result<(), TdispGuestOperationError>;
}

impl TdispGuestRequestInterface for TdispHostStateMachine {
    /// Request versioning and protocol negotiation from the host.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn tdisp_negotiate_protocol(
        &mut self,
        requested_guest_protocol: TdispGuestProtocolType,
    ) -> Result<TdispDeviceInterfaceInfo, TdispGuestOperationError> {
        self.ensure_healthy()?;
        if self.guest_protocol_type != TdispGuestProtocolType::Invalid
            && self.guest_protocol_type != requested_guest_protocol
        {
            tracing::error!(
                "Guest tried to negotiate a protocol with the host while a protocol was already negotiated!"
            );
            return Err(TdispGuestOperationError::InvalidGuestProtocolRequest);
        }

        if requested_guest_protocol == TdispGuestProtocolType::Invalid {
            tracing::error!("Guest tried to negotiate Invalid as a protocol");
            return Err(TdispGuestOperationError::InvalidGuestProtocolRequest);
        }

        // Call back into the host to negotiate protocol information.
        let res = self
            .host_interface
            .tdisp_negotiate_protocol(requested_guest_protocol)
            .context("failed to call to negotiate protocol");

        match res {
            Ok(interface_info) => {
                match TdispGuestProtocolType::from_i32(interface_info.guest_protocol_type) {
                    Some(guest_protocol_type) => {
                        if guest_protocol_type == TdispGuestProtocolType::Invalid {
                            tracing::error!(
                                ?guest_protocol_type,
                                "Guest protocol negotiated with invalid value"
                            );
                            Err(TdispGuestOperationError::InvalidGuestProtocolRequest)
                        } else {
                            self.guest_protocol_type = guest_protocol_type;
                            tracing::info!(
                                ?interface_info,
                                "Guest protocol negotiated successfully to"
                            );
                            Ok(interface_info)
                        }
                    }
                    None => {
                        tracing::error!(
                            ?interface_info,
                            "Guest protocol negotiated with none value"
                        );
                        Err(TdispGuestOperationError::InvalidGuestProtocolRequest)
                    }
                }
            }
            Err(e) => {
                tracing::error!(?e, "Failed to negotiate protocol with host interface");
                Err(TdispGuestOperationError::HostFailedToProcessCommand)
            }
        }
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_lock_device_resources(&mut self) -> Result<(), TdispGuestOperationError> {
        self.ensure_healthy()?;
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        self.transition(
            ConfirmedState::Locked,
            |host| host.tdisp_bind_device(),
            TdispUnbindReason::InvalidGuestTransitionToLocked,
        )
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_start_tdi(&mut self) -> Result<(), TdispGuestOperationError> {
        self.ensure_healthy()?;
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        self.transition(
            ConfirmedState::Running,
            |host| host.tdisp_start_device(),
            TdispUnbindReason::InvalidGuestTransitionToRun,
        )
    }

    /// Block or unblock an MMIO range in the guest's private context.
    ///
    /// Unlike the other state-gated commands, a request in the wrong state is
    /// treated as recoverable: it returns an error without unbinding, since the
    /// guest may legitimately retry as BARs are reprogrammed. Does not
    /// transition the TDI.
    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_modify_mmio_range(
        &mut self,
        action: TdispMmioRangeAction,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> Result<(), TdispGuestOperationError> {
        self.ensure_healthy()?;
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        if action == TdispMmioRangeAction::Invalid {
            tracing::error!("ModifyMmioRange requested with an invalid action.");
            return Err(TdispGuestOperationError::InvalidGuestCommandId);
        }

        tracing::info!(
            ?action,
            range_id,
            gpa_base,
            range_len_bytes,
            "Modifying MMIO range in the guest context"
        );

        self.execute(Mutation::ModifyMmio, |host| {
            host.tdisp_modify_mmio_range(action, range_id, gpa_base, range_len_bytes)
        })
        .map_err(|error| match error {
            MutationError::Admission(_) => TdispGuestOperationError::InvalidDeviceState,
            MutationError::Backend(_) => TdispGuestOperationError::HostFailedToProcessCommand,
        })
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_attestation_report(
        &mut self,
        report_type: TdispReportType,
    ) -> Result<Vec<u8>, TdispGuestOperationError> {
        self.ensure_healthy()?;
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        // The guest device ID identifies the TDI to the host and is retrieved
        // as a "report", though it does not need to be Locked or Run to retrieve the device id.
        //
        // All other report types require the TDI to be in the Locked or Run state.
        if report_type != TdispReportType::GuestDeviceId
            && self.lifecycle.locked_or_running().is_err()
        {
            tracing::error!(
                "Request to retrieve attestation report called while device was not in Locked or Run state."
            );
            self.unbind_all(TdispUnbindReason::InvalidGuestGetAttestationReportState)
                .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;

            return Err(TdispGuestOperationError::InvalidGuestAttestationReportState);
        }

        if report_type == TdispReportType::Invalid {
            tracing::error!("Invalid report type TdispReportId::INVALID requested");
            return Err(TdispGuestOperationError::InvalidGuestAttestationReportType);
        }

        let report_buffer = self
            .host_interface
            .tdisp_get_device_report(report_type)
            .context("failed to call to get device report from host");

        match report_buffer {
            Ok(report_buffer) => {
                tracing::info!("Retrieve attestation report called successfully");
                Ok(report_buffer)
            }
            Err(e) => {
                tracing::error!("Failed to get device report from host: {e:?}");
                Err(TdispGuestOperationError::HostFailedToProcessCommand)
            }
        }
    }

    #[instrument(fields(device_id = %self.debug_device_id), skip(self))]
    fn request_unbind(
        &mut self,
        reason: TdispGuestUnbindReason,
    ) -> Result<(), TdispGuestOperationError> {
        self.ensure_healthy()?;
        // Ensure the guest protocol is negotiated.
        self.ensure_negotiated_protocol()
            .map_err(|_| TdispGuestOperationError::InvalidDeviceState)?;

        // The guest can provide a reason for the unbind. If the unbind reason isn't valid for a guest (such as
        // if the guest says it is unbinding due to a host-related error), the reason is discarded and InvalidGuestUnbindReason
        // is recorded in the unbind history.
        let reason = match reason {
            TdispGuestUnbindReason::Graceful
            | TdispGuestUnbindReason::DeviceTeardown
            | TdispGuestUnbindReason::ResourceSetupFailure
            | TdispGuestUnbindReason::AttestationFailure
            | TdispGuestUnbindReason::StartupFailure => TdispUnbindReason::GuestInitiated(reason),
            _ => {
                tracing::error!(
                    "Invalid guest unbind reason {} requested",
                    reason.as_str_name()
                );
                TdispUnbindReason::InvalidGuestUnbindReason(anyhow::anyhow!(
                    "Invalid guest unbind reason {} requested",
                    reason.as_str_name()
                ))
            }
        };

        tracing::info!(
            "Guest request to unbind succeeds while device is in {:?} (reason: {:?})",
            self.state(),
            reason
        );

        self.unbind_all(reason)
            .map_err(|_| TdispGuestOperationError::HostFailedToProcessCommand)?;

        Ok(())
    }
}
