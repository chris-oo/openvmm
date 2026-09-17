// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements GSI routing management for KVM VMs.

use crate::KvmPartitionInner;
use anyhow::Context;
use pal_event::Event;
use parking_lot::Mutex;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use virt::irqfd::IrqFdRoute;

const NUM_GSIS: usize = 2048;

/// The GSI routing table configured for a VM.
#[derive(Debug)]
pub struct GsiRouting {
    states: Box<[GsiState; NUM_GSIS]>,
    failure: Option<Arc<GsiRouteError>>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GsiRouteError {
    #[error("KVM interrupt route {gsi} dropped during unwind; retaining route ownership")]
    Unwinding { gsi: u32 },
    #[cfg(guest_arch = "aarch64")]
    #[error("checked interrupt routes require in-place CCA")]
    Unsupported,
    #[cfg(guest_arch = "aarch64")]
    #[error("CCA partition is fatal; interrupt ownership cannot be released")]
    PartitionFatal,
    #[error("KVM interrupt {gsi}: {operation} failed; route ownership must be retained")]
    Kernel {
        gsi: u32,
        operation: &'static str,
        #[source]
        source: kvm::Error,
    },
    #[error("invalid KVM MSI route {gsi}: address={address:#x}, data={data:#x}")]
    InvalidMessage { gsi: u32, address: u64, data: u32 },
}

trait RouteIoctls {
    fn routes(&mut self, routes: &[(u32, kvm::RoutingEntry)]) -> Result<(), kvm::Error>;
    fn irqfd(&mut self, gsi: u32, event: &Event, enable: bool) -> Result<(), kvm::Error>;
}

struct KernelRouteIoctls<'a>(&'a kvm::Partition);

impl RouteIoctls for KernelRouteIoctls<'_> {
    fn routes(&mut self, routes: &[(u32, kvm::RoutingEntry)]) -> Result<(), kvm::Error> {
        self.0.set_gsi_routes(routes)
    }

    fn irqfd(&mut self, gsi: u32, event: &Event, enable: bool) -> Result<(), kvm::Error> {
        self.0.irqfd(gsi, event.as_fd().as_raw_fd(), enable)
    }
}

impl GsiRouting {
    /// Creates a new routing table.
    pub fn new() -> Self {
        Self {
            states: Box::new([GsiState::Unallocated; NUM_GSIS]),
            failure: None,
        }
    }

    /// Claims a specific GSI.
    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    pub fn claim(&mut self, gsi: u32) {
        let gsi = gsi as usize;
        assert_eq!(self.states[gsi], GsiState::Unallocated);
        self.states[gsi] = GsiState::Disabled;
    }

    /// Allocates an unused GSI.
    pub fn alloc(&mut self) -> Option<u32> {
        let gsi = self.states.iter().position(|state| !state.is_allocated())?;
        self.states[gsi] = GsiState::Disabled;
        Some(gsi as u32)
    }

    /// Frees an allocated or claimed GSI.
    pub fn free(&mut self, gsi: u32) {
        let gsi = gsi as usize;
        assert_eq!(self.states[gsi], GsiState::Disabled);
        self.states[gsi] = GsiState::Unallocated;
    }

    /// Sets the routing entry for a GSI.
    pub fn set(&mut self, gsi: u32, entry: Option<kvm::RoutingEntry>) -> bool {
        let new_state = entry.map_or(GsiState::Disabled, GsiState::Enabled);
        let state = &mut self.states[gsi as usize];
        assert!(state.is_allocated());
        if *state != new_state {
            *state = new_state;
            true
        } else {
            false
        }
    }

    /// Updates the kernel's routing table with the contents of this table.
    pub fn update_routes(&mut self, kvm: &kvm::Partition) {
        kvm.set_gsi_routes(&self.entries())
            .expect("should not fail");
    }

    fn entries(&self) -> Vec<(u32, kvm::RoutingEntry)> {
        self.states
            .iter()
            .enumerate()
            .filter_map(|(gsi, state)| match state {
                GsiState::Unallocated | GsiState::Disabled => None,
                GsiState::Enabled(entry) => Some((gsi as u32, *entry)),
            })
            .collect()
    }

    pub(crate) fn check(&self) -> Result<(), Arc<GsiRouteError>> {
        self.failure
            .as_ref()
            .map_or(Ok(()), |error| Err(error.clone()))
    }

    fn fail(&mut self, error: GsiRouteError) -> Arc<GsiRouteError> {
        self.failure.get_or_insert_with(|| Arc::new(error)).clone()
    }

    fn set_checked(
        &mut self,
        gsi: u32,
        entry: Option<kvm::RoutingEntry>,
        ioctls: &mut impl RouteIoctls,
    ) -> Result<(), Arc<GsiRouteError>> {
        self.check()?;
        if self.set(gsi, entry) {
            if let Err(source) = ioctls.routes(&self.entries()) {
                return Err(self.fail(GsiRouteError::Kernel {
                    gsi,
                    operation: "set routes",
                    source,
                }));
            }
        }
        Ok(())
    }
}

impl KvmPartitionInner {
    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn check_assignment_interrupt_routes(&self) -> Result<(), Arc<GsiRouteError>> {
        if !self.checked_irq_routes() {
            return Err(Arc::new(GsiRouteError::Unsupported));
        }
        self.gsi_routing.lock().check()?;
        if self.cca_fatal.load(Ordering::Acquire) {
            return Err(Arc::new(GsiRouteError::PartitionFatal));
        }
        Ok(())
    }

    fn checked_irq_routes(&self) -> bool {
        #[cfg(guest_arch = "aarch64")]
        {
            self.caps.isolation == virt::IsolationType::Cca
                && self.memory_backing_mode.is_in_place()
        }
        #[cfg(guest_arch = "x86_64")]
        {
            false
        }
    }

    fn handle_irq_route_failure(&self, error: &GsiRouteError) {
        tracelimit::error_ratelimited!(
            error = error as &dyn std::error::Error,
            "KVM interrupt operation failed; retaining assignment routes"
        );
        #[cfg(guest_arch = "aarch64")]
        self.mark_cca_fatal();
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn irq_operations_allowed(&self) -> bool {
        !self.checked_irq_routes()
            || (!self.cca_fatal.load(Ordering::Acquire) && self.gsi_routing.lock().check().is_ok())
    }

    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn record_irq_delivery_failure(
        &self,
        gsi: u32,
        operation: &'static str,
        source: kvm::Error,
    ) {
        if self.checked_irq_routes() {
            let error = self.gsi_routing.lock().fail(GsiRouteError::Kernel {
                gsi,
                operation,
                source,
            });
            self.handle_irq_route_failure(&error);
        }
    }

    /// Reserves a new route, optionally with an associated irqfd event.
    fn new_route(self: &Arc<Self>, irqfd_event: Option<Event>) -> Option<GsiRoute> {
        let mut routing = self.gsi_routing.lock();
        routing.check().ok()?;
        let gsi = routing.alloc()?;
        Some(GsiRoute {
            partition: Arc::downgrade(self),
            inner: GsiRouteInner {
                gsi,
                irqfd_event,
                enabled: false.into(),
                enable_mutex: Mutex::new(()),
            },
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum GsiState {
    Unallocated,
    Disabled,
    Enabled(kvm::RoutingEntry),
}

impl GsiState {
    fn is_allocated(&self) -> bool {
        !matches!(self, GsiState::Unallocated)
    }
}

/// A GSI route.
struct GsiRoute {
    partition: Weak<KvmPartitionInner>,
    inner: GsiRouteInner,
}

struct GsiRouteInner {
    gsi: u32,
    irqfd_event: Option<Event>,
    enabled: AtomicBool,
    enable_mutex: Mutex<()>, // serializes route updates and enable/disable calls
}

impl Drop for GsiRoute {
    fn drop(&mut self) {
        if let Some(partition) = self.partition.upgrade() {
            if partition.checked_irq_routes() {
                let result = self.inner.release_checked(&partition);
                if let Err(error) = result {
                    partition.handle_irq_route_failure(&error);
                    // KVM may still reference this event and route. Retain the
                    // event, VM and its backing, and never recycle the GSI.
                    std::mem::forget((partition, self.inner.irqfd_event.take()));
                }
                return;
            }
            self.inner.disable(&partition);
            self.inner.set_entry(&partition, None);
            partition.gsi_routing.lock().free(self.inner.gsi);
        }
    }
}

impl GsiRouteInner {
    fn change_irqfd_checked(
        &self,
        routing: &mut GsiRouting,
        ioctls: &mut impl RouteIoctls,
        enable: bool,
    ) -> Result<(), Arc<GsiRouteError>> {
        routing.check()?;
        if self.enabled.load(Ordering::Relaxed) == enable {
            return Ok(());
        }
        // A failed enable can have an uncertain outcome. Only an acknowledged
        // disable permits clearing this flag.
        if enable {
            self.enabled.store(true, Ordering::Relaxed);
        }
        if let Some(event) = &self.irqfd_event {
            if let Err(source) = ioctls.irqfd(self.gsi, event, enable) {
                return Err(routing.fail(GsiRouteError::Kernel {
                    gsi: self.gsi,
                    operation: if enable {
                        "enable irqfd"
                    } else {
                        "disable irqfd"
                    },
                    source,
                }));
            }
        }
        self.enabled.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn enable_checked_inner(
        &self,
        routing: &mut GsiRouting,
        ioctls: &mut impl RouteIoctls,
        entry: kvm::RoutingEntry,
    ) -> Result<(), Arc<GsiRouteError>> {
        routing.set_checked(self.gsi, Some(entry), ioctls)?;
        self.change_irqfd_checked(routing, ioctls, true)
    }

    fn release_checked_inner(
        &self,
        routing: &mut GsiRouting,
        ioctls: &mut impl RouteIoctls,
        unwinding: bool,
    ) -> Result<(), Arc<GsiRouteError>> {
        if unwinding {
            return Err(routing.fail(GsiRouteError::Unwinding { gsi: self.gsi }));
        }
        self.change_irqfd_checked(routing, ioctls, false)?;
        routing.set_checked(self.gsi, None, ioctls)?;
        routing.free(self.gsi);
        Ok(())
    }

    fn release_checked(&self, partition: &KvmPartitionInner) -> Result<(), Arc<GsiRouteError>> {
        let _lock = self.enable_mutex.lock();
        self.release_checked_inner(
            &mut partition.gsi_routing.lock(),
            &mut KernelRouteIoctls(&partition.kvm),
            std::thread::panicking(),
        )
    }

    fn set_entry(&self, partition: &KvmPartitionInner, new_entry: Option<kvm::RoutingEntry>) {
        let mut routing = partition.gsi_routing.lock();
        if routing.set(self.gsi, new_entry) {
            routing.update_routes(&partition.kvm);
        }
    }

    /// Enables the route and associated irqfd.
    pub fn enable(&self, partition: &KvmPartitionInner, entry: kvm::RoutingEntry) {
        let _lock = self.enable_mutex.lock();
        #[cfg(guest_arch = "aarch64")]
        if !partition.irq_operations_allowed() {
            return;
        }
        if partition.checked_irq_routes() {
            let result = self.enable_checked_inner(
                &mut partition.gsi_routing.lock(),
                &mut KernelRouteIoctls(&partition.kvm),
                entry,
            );
            if let Err(error) = result {
                partition.handle_irq_route_failure(&error);
            }
            return;
        }
        self.set_entry(partition, Some(entry));
        if !self.enabled.load(Ordering::Relaxed) {
            if let Some(event) = &self.irqfd_event {
                partition
                    .kvm
                    .irqfd(self.gsi, event.as_fd().as_raw_fd(), true)
                    .expect("should not fail");
            }
            self.enabled.store(true, Ordering::Relaxed);
        }
    }

    /// Disables the associated irqfd.
    ///
    /// This actually leaves the route configured, but it disables the irqfd and
    /// clears the `enabled` flag.
    pub fn disable(&self, partition: &KvmPartitionInner) {
        let _lock = self.enable_mutex.lock();
        if partition.checked_irq_routes() {
            let result = self.change_irqfd_checked(
                &mut partition.gsi_routing.lock(),
                &mut KernelRouteIoctls(&partition.kvm),
                false,
            );
            if let Err(error) = result {
                partition.handle_irq_route_failure(&error);
            }
            return;
        }
        if self.enabled.load(Ordering::Relaxed) {
            if let Some(irqfd_event) = &self.irqfd_event {
                partition
                    .kvm
                    .irqfd(self.gsi, irqfd_event.as_fd().as_raw_fd(), false)
                    .expect("should not fail");
            }
            self.enabled.store(false, Ordering::Relaxed);
        }
    }
}

pub(crate) struct KvmIrqFdState {
    pub(crate) partition: Arc<KvmPartitionInner>,
}

impl KvmIrqFdState {
    pub fn new(partition: Arc<KvmPartitionInner>) -> Self {
        Self { partition }
    }

    pub fn new_irqfd_route<T: MsiRouteBuilder>(
        &self,
        builder: T,
    ) -> anyhow::Result<KvmIrqFdRoute<T>> {
        let event = Event::new();
        let route = self
            .partition
            .new_route(Some(event.clone()))
            .context("no free GSIs available for irqfd")?;
        Ok(KvmIrqFdRoute {
            builder,
            route,
            event,
        })
    }
}

/// A registered irqfd route backed by a KVM [`GsiRoute`].
///
/// Cleanup (disable irqfd, clear route, free GSI) is handled by
/// [`GsiRoute::drop`].
pub(crate) struct KvmIrqFdRoute<T> {
    builder: T,
    route: GsiRoute,
    event: Event,
}

pub(crate) trait MsiRouteBuilder: Send + Sync {
    fn routing_entry(
        &self,
        partition: &KvmPartitionInner,
        address: u64,
        data: u32,
        devid: Option<u32>,
    ) -> Option<kvm::RoutingEntry>;
}

impl<T: MsiRouteBuilder> IrqFdRoute for KvmIrqFdRoute<T> {
    fn event(&self) -> &Event {
        &self.event
    }

    fn enable(&self, address: u64, data: u32, devid: Option<u32>) {
        if let Some(partition) = self.route.partition.upgrade() {
            if let Some(entry) = self.builder.routing_entry(&partition, address, data, devid) {
                self.route.inner.enable(&partition, entry);
            } else if partition.checked_irq_routes() {
                let error = partition
                    .gsi_routing
                    .lock()
                    .fail(GsiRouteError::InvalidMessage {
                        gsi: self.route.inner.gsi,
                        address,
                        data,
                    });
                partition.handle_irq_route_failure(&error);
            } else {
                tracelimit::warn_ratelimited!(
                    address,
                    data,
                    "failed to build irqfd interrupt route"
                );
                self.route.inner.disable(&partition);
                self.route.inner.set_entry(&partition, None);
            }
        }
    }

    fn disable(&self) {
        if let Some(partition) = self.route.partition.upgrade() {
            self.route.inner.disable(&partition);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Routes(Vec<(u32, kvm::RoutingEntry)>),
        Irqfd(bool),
    }

    #[derive(Default)]
    struct Ioctls {
        calls: Vec<Call>,
        fail_at: Option<usize>,
    }

    impl Ioctls {
        fn record(&mut self, call: Call) -> Result<(), kvm::Error> {
            self.calls.push(call);
            if self.fail_at == Some(self.calls.len() - 1) {
                Err(kvm::Error::MissingCapability("injected IRQ ioctl failure"))
            } else {
                Ok(())
            }
        }
    }

    impl RouteIoctls for Ioctls {
        fn routes(&mut self, routes: &[(u32, kvm::RoutingEntry)]) -> Result<(), kvm::Error> {
            self.record(Call::Routes(routes.to_vec()))
        }

        fn irqfd(&mut self, _gsi: u32, _event: &Event, enable: bool) -> Result<(), kvm::Error> {
            self.record(Call::Irqfd(enable))
        }
    }

    fn route(routing: &mut GsiRouting) -> GsiRouteInner {
        GsiRouteInner {
            gsi: routing.alloc().unwrap(),
            irqfd_event: Some(Event::new()),
            enabled: AtomicBool::new(false),
            enable_mutex: Mutex::new(()),
        }
    }

    fn entry(data: u32) -> kvm::RoutingEntry {
        kvm::RoutingEntry::Msi {
            address_lo: 0x800_0040,
            address_hi: 0,
            data,
            devid: Some(0x100),
        }
    }

    #[test]
    fn checked_route_release_requires_both_irqfd_and_routing_acknowledgments() {
        let mut routing = GsiRouting::new();
        let route = route(&mut routing);
        let mut ioctls = Ioctls::default();
        route
            .enable_checked_inner(&mut routing, &mut ioctls, entry(42))
            .unwrap();
        route
            .release_checked_inner(&mut routing, &mut ioctls, false)
            .unwrap();
        assert_eq!(
            ioctls.calls,
            [
                Call::Routes(vec![(route.gsi, entry(42))]),
                Call::Irqfd(true),
                Call::Irqfd(false),
                Call::Routes(Vec::new()),
            ]
        );
        assert!(!route.enabled.load(Ordering::Relaxed));
        assert_eq!(routing.states[route.gsi as usize], GsiState::Unallocated);
        routing.check().unwrap();
    }

    #[test]
    fn every_checked_irq_ioctl_failure_latches_and_preserves_the_gsi() {
        for fail_at in 0..4 {
            let mut routing = GsiRouting::new();
            let route = route(&mut routing);
            let mut ioctls = Ioctls {
                fail_at: Some(fail_at),
                ..Default::default()
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                route.enable_checked_inner(&mut routing, &mut ioctls, entry(42))?;
                route.release_checked_inner(&mut routing, &mut ioctls, false)
            }));
            let original = result
                .expect("an IRQ ioctl error must not panic")
                .unwrap_err();
            assert_eq!(ioctls.calls.len(), fail_at + 1);
            assert!(routing.states[route.gsi as usize].is_allocated());
            let retry = route
                .release_checked_inner(&mut routing, &mut ioctls, false)
                .unwrap_err();
            assert!(Arc::ptr_eq(&original, &retry));
            assert_eq!(
                ioctls.calls.len(),
                fail_at + 1,
                "uncertain operations must not retry"
            );
            if fail_at == 1 || fail_at == 2 {
                assert!(route.enabled.load(Ordering::Relaxed));
            }
            assert!(matches!(
                &*original,
                GsiRouteError::Kernel {
                    source: kvm::Error::MissingCapability("injected IRQ ioctl failure"),
                    ..
                }
            ));
        }
    }

    #[test]
    fn failed_guest_route_reprogramming_blocks_new_updates_and_preserves_first_error() {
        let mut routing = GsiRouting::new();
        let route = route(&mut routing);
        let mut ioctls = Ioctls::default();
        route
            .enable_checked_inner(&mut routing, &mut ioctls, entry(42))
            .unwrap();
        ioctls.fail_at = Some(2);
        let first = route
            .enable_checked_inner(&mut routing, &mut ioctls, entry(43))
            .unwrap_err();
        let second = route
            .enable_checked_inner(&mut routing, &mut ioctls, entry(44))
            .unwrap_err();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(ioctls.calls.len(), 3);
        assert!(route.enabled.load(Ordering::Relaxed));
        assert!(routing.states[route.gsi as usize].is_allocated());
        let retained = routing.fail(GsiRouteError::InvalidMessage {
            gsi: route.gsi,
            address: 0,
            data: 0,
        });
        assert!(Arc::ptr_eq(&first, &retained));
    }

    #[test]
    fn checked_masking_does_not_claim_disable_after_kernel_failure() {
        let mut routing = GsiRouting::new();
        let route = route(&mut routing);
        let mut ioctls = Ioctls::default();
        route
            .enable_checked_inner(&mut routing, &mut ioctls, entry(42))
            .unwrap();
        ioctls.fail_at = Some(2);
        assert!(
            route
                .change_irqfd_checked(&mut routing, &mut ioctls, false)
                .is_err()
        );
        assert!(route.enabled.load(Ordering::Relaxed));
        assert!(routing.check().is_err());
        assert_eq!(
            routing.states[route.gsi as usize],
            GsiState::Enabled(entry(42))
        );
    }

    #[test]
    fn unwinding_route_release_latches_without_issuing_cleanup_or_freeing_gsi() {
        let mut routing = GsiRouting::new();
        let route = route(&mut routing);
        let mut ioctls = Ioctls::default();
        route
            .enable_checked_inner(&mut routing, &mut ioctls, entry(42))
            .unwrap();
        let before = ioctls.calls.len();
        let error = route
            .release_checked_inner(&mut routing, &mut ioctls, true)
            .unwrap_err();
        assert!(matches!(&*error, GsiRouteError::Unwinding { gsi } if *gsi == route.gsi));
        assert_eq!(ioctls.calls.len(), before);
        assert!(route.enabled.load(Ordering::Relaxed));
        assert!(routing.states[route.gsi as usize].is_allocated());
        assert!(Arc::ptr_eq(&error, &routing.check().unwrap_err()));
    }
}
