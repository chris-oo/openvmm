// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Host and guest interfaces for trusted device assignment (TDISP).
//!
//! The VPCI host facade dispatches guest commands through
//! [`TdispHostDeviceTargetEmulator`] to an exclusively owned
//! [`TdispHostDeviceInterface`]. The native [`host::Coordinator`] owns native
//! backend operations and evidence snapshots. Both use the same lifecycle
//! engine for admission, confirmed completion, and quarantine.
//!
//! Backends must contain device access on uncertain completion. Emulated
//! frontends use [`TdispAccessGate`] on every MMIO path. The gate does not
//! withdraw physical DMA or replace native access containment.

/// Protobuf serialization of guest commands and responses.
pub mod serialize_proto;

/// Serialization code from PCI standard structures reported from the TDISP device directly.
pub mod devicereport;

/// Transport-independent native host operations and snapshot coordination.
pub mod host;

mod vpci;
pub use vpci::*;

#[cfg(test)]
mod tests;

/// Mocks for the host interface and the emulator.
pub mod test_helpers;
