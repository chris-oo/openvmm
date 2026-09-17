// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use test_with_tracing::test;

#[test]
fn realm_host_ranges_check_both_resource_endpoints() {
    let absent = "0 0 0\n".repeat(5);
    let ranges = parse_physical_bar_ranges(&format!("0x10000 0x13fff 0x200\n{absent}")).unwrap();
    assert_eq!(ranges[0], Some(MemoryRange::new(0x10000..0x14000)));
    assert!(
        parse_physical_bar_ranges(&format!(
            "0xfffffffffffff000 0xffffffffffffffff 0x200\n{absent}"
        ))
        .is_err()
    );
    assert!(parse_physical_bar_ranges(&format!("0x14000 0x13fff 0x200\n{absent}")).is_err());
    assert!(parse_physical_bar_ranges("").is_err());
}

fn frontend() -> (VfioAssignedPciDevice, Arc<realm::access::AccessGate>) {
    let gate = Arc::new(realm::access::AccessGate::new(8));
    frontend_with_gate(gate)
}

fn frontend_with_gate(
    gate: Arc<realm::access::AccessGate>,
) -> (VfioAssignedPciDevice, Arc<realm::access::AccessGate>) {
    gate.state.lock().frontend_live = true;
    gate.state.lock().nonsecure.push(0x13000..0x14000);
    let bars = [0x10000, 0, 0, 0, 0, 0];
    let masks = [0xffff_c000, 0, 0, 0, 0, 0];
    let (emulator, capability) = MsixEmulator::new(0, 2, &MsiTarget::disconnected());
    let device = VfioAssignedPciDevice {
        pci_id: "test".into(),
        vfio_device: VfioPciDevice {
            device: Arc::new(
                vfio_sys::cdev::CdevDevice::from_file(std::fs::File::open("/dev/null").unwrap())
                    .into_device(),
            ),
            config_offset: 0,
            config_size: 256,
        },
        bar_masks: masks,
        bars,
        bar_flags: [0; 6],
        bar_reset_defaults: bars,
        mmio_enabled: true,
        pm_csr_offset: Some(0x44),
        pcie_flr_control_offset: Some(0x58),
        af_flr_control_offset: None,
        in_d0: true,
        active_bars: BarMappings::parse(&bars, &masks),
        bar_mmio_controls: std::array::from_fn(|_| None),
        bar_regions: [
            Some(VfioBarInfo {
                vfio_offset: 0,
                size: 0x4000,
            }),
            None,
            None,
            None,
            None,
            None,
        ],
        msix: Some(MsixEmulationState {
            emulator,
            capability: Box::new(capability),
            cap_offset: 0x60,
            vector_count: 2,
            table_bar: 0,
            table_range: 0x3000..0x3020,
            pba_bar: 0,
            pba_range: 0x3100..0x3108,
            enabled: false,
        }),
        supports_reset: true,
        bar_direct_maps: Vec::new(),
        config_patches: BTreeMap::new(),
        accel_stream: None,
        realm_release: Some(RealmFrontendRelease {
            gate: gate.clone(),
            released: false,
        }),
        realm_irq_suspended: false,
        binding: manager::VfioBinding::Realm(manager::RealmBinding {
            gate: gate.clone(),
            service: None,
        }),
    };
    (device, gate)
}

struct Admission(AtomicBool);

#[async_trait::async_trait]
impl tdisp::host::EvidenceService for Admission {
    fn close_admission(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    async fn object_size(
        &self,
        _: tdisp::host::Object,
    ) -> Result<usize, tdisp::host::EvidenceError> {
        Err(tdisp::host::EvidenceError::Unsupported)
    }

    async fn read_object(
        &self,
        _: tdisp::host::Object,
        _: u64,
        _: u64,
        _: Arc<dyn tdisp::host::EvidenceSink>,
    ) -> Result<usize, tdisp::host::EvidenceError> {
        Err(tdisp::host::EvidenceError::Unsupported)
    }

    async fn teardown(&self) -> Result<(), tdisp::host::EvidenceError> {
        Ok(())
    }
}

#[test]
fn failed_bar_and_config_transports_close_native_admission() {
    for operation in 0..3 {
        let (mut device, gate) = frontend();
        let service = Arc::new(Admission(AtomicBool::new(false)));
        device.install_realm_service(service.clone()).unwrap();
        let result = match operation {
            0 => device.mmio_read(0x10000, &mut [0; 4]),
            1 => device.mmio_write(0x10000, &[1; 4]),
            _ => device.pci_cfg_read(0, ByteEnabledDwordRead::with_all_bytes_enabled(&mut 0)),
        };
        assert!(matches!(result, IoResult::Err(IoError::NoResponse)));
        assert!(gate.state.lock().deny_all);
        assert!(gate.state.lock().io_error.is_some());
        assert!(service.0.load(Ordering::SeqCst));
        assert!(matches!(
            device.mmio_read(0x13000, &mut [0; 4]),
            IoResult::Err(_)
        ));
    }
}

#[test]
fn short_read_and_write_cannot_report_success_or_leave_access_open() {
    for operation in ["read", "write"] {
        let gate = realm::access::AccessGate::new(8);
        let service = Arc::new(Admission(AtomicBool::new(false)));
        let erased: Arc<dyn tdisp::host::EvidenceService> = service.clone();
        gate.bind_service(Arc::downgrade(&erased)).unwrap();
        let mut state = gate.state.lock();
        let result = complete_realm_bar_transfer(&gate, &mut state, Ok(2), 4, operation, 0, 0);
        assert!(matches!(result, IoResult::Err(IoError::NoResponse)));
        assert!(state.deny_all);
        assert!(service.0.load(Ordering::SeqCst));
    }
}

struct RouteChecks {
    checks: AtomicUsize,
    fail_on: usize,
}

impl pci_core::vfio::VfioVm for RouteChecks {
    fn add_file(&self, _: std::os::fd::BorrowedFd<'_>) -> Result<(), pci_core::vfio::VfioVmError> {
        unreachable!("route-check fixture")
    }

    fn remove_file(
        &self,
        _: std::os::fd::BorrowedFd<'_>,
    ) -> Result<(), pci_core::vfio::VfioVmError> {
        unreachable!("route-check fixture")
    }

    fn check_interrupt_routes(&self) -> Result<(), pci_core::vfio::VfioVmError> {
        if self.checks.fetch_add(1, Ordering::SeqCst) + 1 >= self.fail_on {
            return Err(pci_core::vfio::VfioVmError::new(std::io::Error::other(
                "KVM route cleanup failed",
            )));
        }
        Ok(())
    }
}

#[test]
fn frontend_release_requires_post_destruction_kvm_acknowledgement() {
    for fail_on in [2, 3] {
        let routes = Arc::new(RouteChecks {
            checks: AtomicUsize::new(0),
            fail_on,
        });
        let gate = Arc::new(realm::access::AccessGate::with_interrupts(
            8,
            routes.clone(),
        ));
        let (device, gate) = frontend_with_gate(gate);
        drop(device);
        let state = gate.state.lock();
        assert!(state.frontend_live);
        assert!(state.deny_all);
        assert!(state.irq_error.is_some());
        assert_eq!(routes.checks.load(Ordering::SeqCst), fail_on);
    }
}

#[test]
fn unwinding_never_acknowledges_frontend_or_route_release() {
    let routes = Arc::new(RouteChecks {
        checks: AtomicUsize::new(0),
        fail_on: usize::MAX,
    });
    let gate = Arc::new(realm::access::AccessGate::with_interrupts(
        8,
        routes.clone(),
    ));
    let (device, gate) = frontend_with_gate(gate);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _device = device;
        panic!("injected caller unwind");
    }));
    assert!(result.is_err());
    assert!(gate.state.lock().frontend_live);
    assert!(gate.state.lock().deny_all);
    assert_eq!(routes.checks.load(Ordering::SeqCst), 0);
}

#[test]
fn msi_updates_cannot_hide_a_kernel_route_failure() {
    for config in [false, true] {
        let routes = Arc::new(RouteChecks {
            checks: AtomicUsize::new(0),
            fail_on: 2,
        });
        let gate = Arc::new(realm::access::AccessGate::with_interrupts(8, routes));
        let (mut device, gate) = frontend_with_gate(gate);
        let service = Arc::new(Admission(AtomicBool::new(false)));
        device.install_realm_service(service.clone()).unwrap();
        let result = if config {
            device.pci_cfg_write(0x60, ByteEnabledDwordWrite::with_all_bytes_enabled(0))
        } else {
            device.mmio_write(0x13000, &[0; 4])
        };
        assert!(matches!(result, IoResult::Err(IoError::NoResponse)));
        assert!(gate.state.lock().irq_error.is_some());
        assert!(service.0.load(Ordering::SeqCst));
    }
}

#[test]
fn ordinary_config_failure_and_local_invalid_ranges_do_not_quarantine() {
    let (mut device, gate) = frontend();
    let mut value = 0;
    device
        .read_phys_config(
            0,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
            None,
        )
        .unwrap();
    assert_eq!(value, u32::MAX);
    assert!(!gate.state.lock().deny_all);
    assert!(matches!(
        device.pci_cfg_read(
            0x400,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
        ),
        IoResult::Err(IoError::InvalidRegister)
    ));
    assert!(matches!(
        device.mmio_read(0x13fff, &mut [0; 2]),
        IoResult::Err(IoError::InvalidRegister)
    ));
    assert!(!gate.state.lock().deny_all);
}

#[test]
fn release_marker_cannot_complete_during_later_field_unwind() {
    let gate = Arc::new(realm::access::AccessGate::new(8));
    gate.state.lock().frontend_live = true;
    let retained = gate.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _marker = RealmFrontendRelease {
            gate: retained,
            released: true,
        };
        panic!("later field destruction failed");
    }));
    assert!(result.is_err());
    assert!(gate.state.lock().frontend_live);
    assert!(gate.state.lock().deny_all);
}

#[test]
fn realm_bar_probe_never_moves_the_bound_interval() {
    let (mut device, gate) = frontend();
    device
        .pci_cfg_write(
            HeaderType00::BAR0.0,
            ByteEnabledDwordWrite::with_all_bytes_enabled(u32::MAX),
        )
        .unwrap();
    assert_eq!(device.bars[0], 0xffff_c000);
    assert_eq!(device.active_bars.get(0), Some(0x10000));
    assert!(matches!(
        device.pci_cfg_write(
            HeaderType00::BAR0.0,
            ByteEnabledDwordWrite::with_all_bytes_enabled(0x20000)
        ),
        IoResult::Err(_)
    ));
    device
        .pci_cfg_write(
            HeaderType00::BAR0.0,
            ByteEnabledDwordWrite::with_all_bytes_enabled(0x10000),
        )
        .unwrap();
    assert_eq!(device.bars[0], 0x10000);
    drop(device);
    assert!(!gate.state.lock().frontend_live);
}

#[test]
fn realm_gate_blocks_fallback_but_allows_complete_msix_accesses() {
    let (mut device, gate) = frontend();
    gate.state.lock().protected_blocked = true;
    assert!(matches!(
        device.mmio_read(0x10000, &mut [0; 4]),
        IoResult::Err(_)
    ));
    assert!(matches!(
        device.mmio_write(0x10000, &[1; 4]),
        IoResult::Err(_)
    ));
    // An unaligned eight-byte operation spans three emulated DWORDs.
    device.mmio_write(0x13001, &[0x5a; 8]).unwrap();
    let mut bytes = [0; 8];
    device.mmio_read(0x13001, &mut bytes).unwrap();
    assert_eq!(bytes, [0x5a; 8]);
    assert!(matches!(
        device.mmio_write(0x1301f, &[1; 4]),
        IoResult::Err(_)
    ));
    assert!(matches!(
        device.mmio_read(u64::MAX, &mut [0; 8]),
        IoResult::Err(_)
    ));
    gate.state.lock().deny_all = true;
    assert!(matches!(
        device.mmio_write(0x13000, &[1; 4]),
        IoResult::Err(_)
    ));
}

#[test]
fn realm_identity_and_reset_cannot_bypass_access_gate() {
    let (mut device, gate) = frontend();
    assert!(matches!(
        device.pci_cfg_write(0x58, ByteEnabledDwordWrite::with_all_bytes_enabled(0x8000)),
        IoResult::Err(_)
    ));
    assert!(matches!(
        device.pci_cfg_write_with_routing(
            PciConfigAccessType::Type0,
            PciConfigAddress::new(1, 8, 4).unwrap(),
            ByteEnabledDwordWrite::with_all_bytes_enabled(0x10000),
        ),
        IoResult::Err(_)
    ));
    assert!(gate.state.lock().deny_all);
    let (mut device, gate) = frontend();
    futures::executor::block_on(device.reset());
    assert!(gate.state.lock().deny_all);
}

#[test]
fn realm_irq_cleanup_failure_retains_frontend_lifetime_guard() {
    let (mut device, gate) = frontend();
    device.msix.as_mut().unwrap().enabled = true;
    // /dev/null rejects VFIO IRQ cleanup; it cannot acknowledge revocation.
    drop(device);
    let state = gate.state.lock();
    assert!(state.deny_all);
    assert!(state.frontend_live);
    assert!(state.irq_error.is_some());
}

#[test]
fn realm_stop_and_start_preserve_protection_without_reset() {
    let (mut device, gate) = frontend();
    gate.state.lock().protected_blocked = true;
    futures::executor::block_on(device.stop());
    assert!(gate.state.lock().stopped);
    assert!(matches!(
        device.mmio_read(0x13000, &mut [0; 4]),
        IoResult::Err(_)
    ));
    device.start();
    assert!(!gate.state.lock().stopped);
    assert!(gate.state.lock().protected_blocked);
    device.mmio_read(0x13000, &mut [0; 4]).unwrap();
    assert!(matches!(
        device.mmio_read(0x10000, &mut [0; 4]),
        IoResult::Err(_)
    ));
}

#[test]
fn explicit_realm_stop_propagates_and_retains_irq_failure() {
    let (mut device, gate) = frontend();
    device.msix.as_mut().unwrap().enabled = true;
    let error = device.stop_realm_frontend().unwrap_err();
    assert!(error.downcast_ref::<realm::access::AccessError>().is_some());
    assert!(gate.state.lock().irq_error.is_some());
    assert!(device.start_realm_frontend().is_err());
    assert!(gate.state.lock().stopped);
}
