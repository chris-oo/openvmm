// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MMIO denial independent of callback locks and lifecycle state.

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

const HEALTHY: u8 = 0;
const IN_FLIGHT: u8 = 1;
const QUARANTINED: u8 = 2;

/// Access containment transferred exclusively to a TDISP emulator.
///
/// Create one per device and wire its read-only [`Self::gate`] into every MMIO
/// path before exposing the emulator. Resource-free mocks must also supply an
/// explicit owner. A new owner is the only way to start with a healthy latch.
pub struct TdispAccess(Arc<AtomicU8>);

impl TdispAccess {
    /// Create containment for a new device owner.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(HEALTHY)))
    }

    /// Obtain a read-only access check for a frontend.
    pub fn gate(&self) -> TdispAccessGate {
        TdispAccessGate(self.0.clone())
    }

    pub(super) fn begin(&self) -> anyhow::Result<Permit<'_>> {
        self.0
            .compare_exchange(HEALTHY, IN_FLIGHT, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("TDISP access is not healthy"))?;
        Ok(Permit { access: self })
    }
}

impl Default for TdispAccess {
    fn default() -> Self {
        Self::new()
    }
}

/// A read-only, shared projection of emulator access health.
///
/// False denies all MMIO, including shared MSI-X. True still requires the
/// frontend's ordinary range and device checks; it is not attestation.
#[derive(Clone)]
pub struct TdispAccessGate(Arc<AtomicU8>);

impl TdispAccessGate {
    /// Whether no mutation or uncertain completion currently denies access.
    pub fn is_allowed(&self) -> bool {
        self.0.load(Ordering::Acquire) == HEALTHY
    }
}

pub(super) struct Permit<'a> {
    access: &'a TdispAccess,
}

impl Permit<'_> {
    pub(super) fn complete(self) -> anyhow::Result<()> {
        self.access
            .0
            .compare_exchange(IN_FLIGHT, HEALTHY, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("TDISP access was quarantined during mutation"))?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.access.0.store(QUARANTINED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn permit_denies_in_flight_and_never_recovers_quarantine() {
        let access = TdispAccess::new();
        let gate = access.gate();
        let permit = access.begin().unwrap();
        assert!(!gate.is_allowed());
        permit.complete().unwrap();
        assert!(gate.is_allowed());
        let permit = access.begin().unwrap();
        access.0.store(QUARANTINED, Ordering::Release);
        assert!(permit.complete().is_err());
        assert!(!gate.is_allowed());
        assert!(access.begin().is_err());
    }

    #[test]
    fn dropped_permit_latches_denial() {
        let access = TdispAccess::new();
        let gate = access.gate();
        drop(access.begin().unwrap());
        assert!(!gate.is_allowed());
        assert!(access.begin().is_err());
    }
}
