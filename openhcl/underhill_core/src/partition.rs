// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition abstraction allowing different virtualization backends within
//! OpenHCL.
//!
//! This abstraction is analogous to [`HvlitePartition`] in `openvmm_core`,
//! but includes OpenHCL-specific methods (reference time, guest VSM
//! revocation, PM timer assist, etc.) that don't apply to the regular
//! openvmm host path.
//!
//! Currently, the only implementation is for [`UhPartition`], but this trait
//! exists to enable alternative backends such as KVM for nested
//! virtualization.
//!
//! [`HvlitePartition`]: openvmm_core::partition::HvlitePartition
//! [`UhPartition`]: virt_mshv_vtl::UhPartition

use core::ops::RangeInclusive;
use hvdef::Vtl;
use inspect::Inspect;
use std::sync::Arc;
use virt::PartitionCapabilities;
use virt::Synic;
use virt::irqcon::MsiRequest;

/// The VM partition, abstracting over the virtualization backend.
pub trait OpenhclPartition: Send + Sync + Inspect {
    /// The current paravisor reference time.
    fn reference_time(&self) -> u64;

    /// The current VTL0 guest OS ID.
    fn vtl0_guest_os_id(&self) -> anyhow::Result<hvdef::hypercall::HvGuestOsId>;

    /// Registers the range to be intercepted by the host directly, without the
    /// exit flowing through the paravisor.
    ///
    /// This is best effort. Exits for this range may still flow through the
    /// paravisor.
    fn register_host_io_port_fast_path(&self, range: RangeInclusive<u16>) -> Box<dyn Send>;

    /// Revokes support for guest VSM (i.e., VTL1) after the guest has started
    /// running.
    fn revoke_guest_vsm(&self) -> anyhow::Result<()>;

    /// Requests an MSI be delivered to the guest interrupt controller.
    fn request_msi(&self, vtl: Vtl, request: MsiRequest);

    /// Returns the partition's capabilities.
    fn caps(&self) -> &PartitionCapabilities;

    /// Sets the port to use for the PM timer assist. Reads of this port will be
    /// implemented by the hypervisor, using the reference time scaled to the
    /// appropriate frequency.
    ///
    /// This is best effort. Exits for reads of this port may still flow through
    /// the paravisor.
    fn set_pm_timer_assist(&self, port: Option<u16>) -> anyhow::Result<()>;

    /// Asserts a debug interrupt on the given VTL.
    fn assert_debug_interrupt(&self, vtl: u8);

    /// Returns the trait object for accessing the synic.
    fn into_synic(self: Arc<Self>) -> Arc<dyn Synic>;

    /// Gets a line set target to trigger local APIC LINTs.
    ///
    /// The line number is the VP index times 2, plus the LINT number (0 or 1).
    #[cfg(guest_arch = "x86_64")]
    fn into_lint_target(
        self: Arc<Self>,
        vtl: Vtl,
    ) -> Arc<dyn vmcore::line_interrupt::LineSetTarget>;

    /// Returns the interface for IO APIC routing.
    #[cfg(guest_arch = "x86_64")]
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting>;

    /// Returns the interface for GIC control.
    #[cfg(guest_arch = "aarch64")]
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic>;
}

impl OpenhclPartition for virt_mshv_vtl::UhPartition {
    fn reference_time(&self) -> u64 {
        self.reference_time()
    }

    fn vtl0_guest_os_id(&self) -> anyhow::Result<hvdef::hypercall::HvGuestOsId> {
        Ok(self.vtl0_guest_os_id()?)
    }

    fn register_host_io_port_fast_path(&self, range: RangeInclusive<u16>) -> Box<dyn Send> {
        Box::new(self.register_host_io_port_fast_path(range))
    }

    fn revoke_guest_vsm(&self) -> anyhow::Result<()> {
        self.revoke_guest_vsm()?;
        Ok(())
    }

    fn request_msi(&self, vtl: Vtl, request: MsiRequest) {
        virt::Partition::request_msi(self, vtl, request)
    }

    fn caps(&self) -> &PartitionCapabilities {
        virt::Partition::caps(self)
    }

    fn set_pm_timer_assist(&self, port: Option<u16>) -> anyhow::Result<()> {
        self.set_pm_timer_assist(port)?;
        Ok(())
    }

    fn assert_debug_interrupt(&self, vtl: u8) {
        self.assert_debug_interrupt(vtl)
    }

    fn into_synic(self: Arc<Self>) -> Arc<dyn Synic> {
        self
    }

    #[cfg(guest_arch = "x86_64")]
    fn into_lint_target(
        self: Arc<Self>,
        vtl: Vtl,
    ) -> Arc<dyn vmcore::line_interrupt::LineSetTarget> {
        Arc::new(vmm_core::emuplat::apic::ApicLintLineTarget::new(self, vtl))
    }

    #[cfg(guest_arch = "x86_64")]
    fn ioapic_routing(&self) -> Arc<dyn virt::irqcon::IoApicRouting> {
        virt::X86Partition::ioapic_routing(self)
    }

    #[cfg(guest_arch = "aarch64")]
    fn control_gic(&self, vtl: Vtl) -> Arc<dyn virt::irqcon::ControlGic> {
        virt::Aarch64Partition::control_gic(self, vtl)
    }
}
