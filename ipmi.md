# IPMI Device Implementation Plan for OpenVMM

## Overview

Add a virtual IPMI BMC (Baseboard Management Controller) device to OpenVMM,
exposed via the KCS (Keyboard Controller Style) interface. The initial scope is
**System Event Log (SEL) support only** — the device will accept and store SEL
entries and allow the guest to read them back. No other IPMI functionality
(sensor reading, chassis control, SOL, etc.) is needed in Phase 1.

The device will be tested via a Linux Direct boot VMM test that verifies SEL
events can be written and read from inside the guest.

---

## Table of Contents

1. [Background: IPMI KCS Interface & SEL](#1-background-ipmi-kcs-interface--sel)
2. [Phase 1: IPMI KCS Device Implementation (Linux Direct)](#2-phase-1-ipmi-kcs-device-implementation-linux-direct)
3. [Phase 1a: VMM Test for SEL Events](#3-phase-1a-vmm-test-for-sel-events)
4. [Phase 2: UEFI Support via SMBIOS](#4-phase-2-uefi-support-via-smbios)
5. [Phase 3: Windows VMM Test](#5-phase-3-windows-vmm-test)
6. [Phase 4 (Optional): ACPI Support](#6-phase-4-optional-acpi-support)

---

## 1. Background: IPMI KCS Interface & SEL

### 1.1 IPMI KCS Interface

The IPMI specification (IPMI v2.0, Section 9) defines the KCS (Keyboard
Controller Style) system interface for in-band BMC communication. The KCS
interface uses two I/O ports:

| Port | Offset | Read | Write |
|------|--------|------|-------|
| Data | Base+0 (`0xCA2`) | Data_Out (BMC→host) | Data_In (host→BMC) |
| Status/Command | Base+1 (`0xCA3`) | Status register | Command register |

The default I/O base address for KCS interface 1 is **`0xCA2`** (IPMI v2.0,
Table 9-1). This is the address Linux's `ipmi_si` driver probes by default.

#### KCS Status Register (read from Base+1)

| Bit | Name | Description |
|-----|------|-------------|
| 7 | S1 | State bit 1 (part of KCS state machine) |
| 6 | S0 | State bit 0 |
| 5 | OEM2 | OEM use |
| 4 | OEM1 | OEM use |
| 3 | CD | Command/Data flag — 1 = last write was command, 0 = data |
| 2 | SMS_ATN | BMC has a message for the host |
| 1 | IBF | Input Buffer Full — host must wait until 0 before writing |
| 0 | OBF | Output Buffer Full — data available for host to read |

#### KCS State Machine

The KCS interface uses a simple state machine (IDLE → READ → WRITE → ERROR).
States are encoded in S1:S0 bits of the status register:

| S1:S0 | State | Meaning |
|-------|-------|---------|
| 00 | IDLE | Ready for new transfer |
| 01 | READ | BMC is transferring data to host |
| 10 | WRITE | Host is transferring data to BMC |
| 11 | ERROR | Error condition |

#### KCS Transfer Protocol (Write from host to BMC)

1. Host waits for IBF=0
2. Host writes `WRITE_START` (0x61) to Command register → state becomes WRITE
3. Host waits for IBF=0 and OBF=1, reads Data (dummy) to clear OBF
4. Host writes first data byte to Data register
5. Repeat steps 3–4 for remaining bytes
6. Host writes `WRITE_END` (0x62) to Command register
7. Host waits for IBF=0 and OBF=1, reads Data (dummy) to clear OBF
8. Host writes last data byte to Data register
9. BMC processes command, enters READ state

#### KCS Transfer Protocol (Read from BMC to host)

1. Host waits for IBF=0 and state=READ
2. Host reads Data register to get byte
3. Host writes `READ` (0x68) to Data register to acknowledge
4. Repeat until state=IDLE
5. Final read of Data register (dummy, status byte)

### 1.2 IPMI Messages & SEL Commands

IPMI messages follow this structure:
```
Request:  [NetFn/LUN] [Command] [Data...]
Response: [NetFn/LUN] [Command] [CompletionCode] [Data...]
```

The response NetFn is the request NetFn OR'd with `0x04` (response bit). For
example, a request with NetFn=App (`0x06`) gets a response with NetFn=`0x07`.
The NetFn/LUN byte is `(NetFn << 2) | LUN`, so a response to App/LUN0 is
`0x1C` (= `0x07 << 2`).

Standard completion codes:
- `0x00` — Command completed normally
- `0xC1` — Invalid/unsupported command
- `0xC7` — Request data length invalid
- `0xD4` — Insufficient privilege level

For SEL operations, the relevant commands (NetFn = Storage 0x0A):

| Command | Code | Description |
|---------|------|-------------|
| Get SEL Info | 0x40 | Returns SEL version, entry count, free space |
| Get SEL Entry | 0x43 | Read a specific SEL record by Record ID |
| Add SEL Entry | 0x44 | Add a new 16-byte SEL record |
| Clear SEL | 0x47 | Erase all SEL entries |
| Get SEL Time | 0x48 | Get BMC timestamp for SEL |
| Set SEL Time | 0x49 | Set BMC timestamp |

#### SEL Record Format (16 bytes, IPMI v2.0 Section 32)

| Offset | Length | Field |
|--------|--------|-------|
| 0–1 | 2 | Record ID (0x0001–0xFFFE) |
| 2 | 1 | Record Type (0x02 = System Event) |
| 3–6 | 4 | Timestamp (seconds since 1970-01-01, little-endian) |
| 7–8 | 2 | Generator ID |
| 9 | 1 | EvM Rev (0x04 for IPMI 2.0) |
| 10 | 1 | Sensor Type |
| 11 | 1 | Sensor Number |
| 12 | 1 | Event Dir / Event Type |
| 13–15 | 3 | Event Data 1–3 |

---

## 2. Phase 1: IPMI KCS Device Implementation (Linux Direct)

### 2.1 New Crates

Following the OpenVMM three-layer device pattern, create two new crates:

#### Crate 1: `vm/devices/ipmi_kcs_resources`

Resource handle (data-only crate, minimal deps):

```
vm/devices/ipmi_kcs_resources/
├── Cargo.toml
└── src/
    └── lib.rs
```

**`Cargo.toml`:**
```toml
[package]
name = "ipmi_kcs_resources"
edition.workspace = true
rust-version.workspace = true

[dependencies]
mesh.workspace = true
vm_resource.workspace = true

[lints]
workspace = true
```

**`src/lib.rs`:**
```rust
use mesh::MeshPayload;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::ResourceId;

/// Resource handle for the IPMI KCS device.
///
/// No configuration fields — the device starts with an empty SEL
/// and the guest populates it at runtime.
#[derive(MeshPayload)]
pub struct IpmiKcsHandle;

impl ResourceId<ChipsetDeviceHandleKind> for IpmiKcsHandle {
    const ID: &'static str = "ipmi_kcs";
}
```

#### Crate 2: `vm/devices/ipmi_kcs`

Device implementation:

```
vm/devices/ipmi_kcs/
├── Cargo.toml
└── src/
    ├── lib.rs        # Device logic + ChipsetDevice impl
    ├── resolver.rs   # Resource resolver
    ├── protocol.rs   # KCS state machine + IPMI message parsing
    └── sel.rs        # SEL storage and command handling
```

**`Cargo.toml`:**
```toml
[package]
name = "ipmi_kcs"
edition.workspace = true
rust-version.workspace = true

[dependencies]
chipset_device.workspace = true
chipset_device_resources.workspace = true
inspect.workspace = true
ipmi_kcs_resources.workspace = true
mesh.workspace = true
open_enum.workspace = true
thiserror.workspace = true
tracelimit.workspace = true
tracing.workspace = true
vm_resource.workspace = true
vmcore.workspace = true

[dev-dependencies]
test_with_tracing.workspace = true

[lints]
workspace = true
```

### 2.2 Device Architecture

#### `protocol.rs` — KCS State Machine

```rust
use open_enum::open_enum;

open_enum! {
    /// KCS interface states (encoded in status register S1:S0, bits 7:6).
    pub enum KcsState: u8 {
        IDLE_STATE  = 0x00, // S1:S0 = 00
        READ_STATE  = 0x40, // S1:S0 = 01
        WRITE_STATE = 0x80, // S1:S0 = 10
        ERROR_STATE = 0xC0, // S1:S0 = 11
    }
}

open_enum! {
    /// KCS commands written to the command register.
    pub enum KcsCommand: u8 {
        GET_STATUS_ABORT = 0x60,
        WRITE_START      = 0x61,
        WRITE_END        = 0x62,
        READ             = 0x68,
    }
}

/// KCS I/O port addresses.
const KCS_DATA_REG: u16 = 0xCA2;
const KCS_STATUS_CMD_REG: u16 = 0xCA3;
```

The state machine implementation must:
- Track IBF/OBF flags correctly
- Accumulate incoming write bytes into a request buffer
- Parse completed IPMI messages and dispatch to command handlers
- Queue response bytes for host read-back
- Handle GET_STATUS/ABORT for error recovery
- **Never panic on any input** — this is a trust boundary (guest-facing device)

#### `sel.rs` — SEL Storage

```rust
/// Maximum SEL entries (configurable, but 128 is a reasonable default).
const MAX_SEL_ENTRIES: usize = 128;

/// 16-byte SEL record per IPMI v2.0 Section 32.
pub struct SelEntry {
    pub record_id: u16,
    pub data: [u8; 16],
}

pub struct SelStore {
    entries: Vec<SelEntry>,
    next_record_id: u16,
    time_offset: i64, // BMC time offset from real time
}
```

Commands to implement:

| Command | Handler |
|---------|---------|
| Get SEL Info (0x40) | Return version=0x51, count, free space, timestamps |
| Get SEL Entry (0x43) | Find by Record ID, return 16-byte record + next ID |
| Add SEL Entry (0x44) | Accept 16-byte record, assign Record ID, store |
| Clear SEL (0x47) | Two-phase erase (initiate + confirm) per spec |

**Clear SEL Protocol Detail** (IPMI v2.0, Section 31.9):

The Clear SEL command uses a two-phase handshake to prevent accidental erasure:

1. **Initiate**: Host sends `Clear SEL` with data bytes `[0x43, 0x4C, 0x52, 0xAA]`
   - `0x43, 0x4C, 0x52` = ASCII `"CLR"` (reservation check)
   - `0xAA` = initiate erase action
   - Response: completion code `0x00` + erasure status byte (`0x01` = in progress)
2. **Get Status** (optional): Host sends `Clear SEL` with `[0x43, 0x4C, 0x52, 0x00]`
   - `0x00` = get erasure status
   - Response: `0x00` + status (`0x01` = in progress, `0x02` = erase complete)
3. For a virtual device, erasure completes instantly — return `0x02` immediately
   after the initiate call.

The first two bytes of the Clear SEL command data are the Reservation ID
(little-endian), followed by `"CLR"` + action byte. A valid sequence is:
```
[ResvID_lo, ResvID_hi, 0x43, 0x4C, 0x52, 0xAA]  // initiate
[ResvID_lo, ResvID_hi, 0x43, 0x4C, 0x52, 0x00]  // get status
```
| Get SEL Time (0x48) | Return current BMC time |
| Set SEL Time (0x49) | Update time offset |

Also implement `Get Device ID` (NetFn=App 0x06, Cmd=0x01) — Linux's `ipmi_si`
driver sends this during probe and won't complete initialization without a
valid response.

#### `lib.rs` — ChipsetDevice Implementation

```rust
#[derive(InspectMut)]
pub struct IpmiKcsDevice {
    // KCS protocol state
    state: KcsState,
    status: u8,
    data_out: u8,
    write_buffer: Vec<u8>,
    read_buffer: VecDeque<u8>,

    // IPMI layer
    sel: SelStore,

    // Static region for get_static_regions()
    #[inspect(skip)]
    pio_region: (&'static str, RangeInclusive<u16>),
}

impl ChipsetDevice for IpmiKcsDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PortIoIntercept for IpmiKcsDevice {
    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.pio_region)
    }

    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        // Read from Data_Out or Status register
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        // Write to Data_In or Command register
        // Drive KCS state machine
    }
}

impl ChangeDeviceState for IpmiKcsDevice {
    fn start(&mut self) {}
    async fn stop(&mut self) {}
    async fn reset(&mut self) {
        // Reset to idle, clear buffers, optionally preserve SEL
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for IpmiKcsDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }
}
```

#### `resolver.rs` — Resource Resolver

Use a sync resolver (no async deps needed):

```rust
pub struct IpmiKcsResolver;

declare_static_resolver! {
    IpmiKcsResolver,
    (ChipsetDeviceHandleKind, IpmiKcsHandle),
}

impl ResolveResource<ChipsetDeviceHandleKind, IpmiKcsHandle> for IpmiKcsResolver {
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: IpmiKcsHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        input.configure.omit_saved_state();
        Ok(IpmiKcsDevice::new().into())
    }
}
```

### 2.3 Integration Points

#### Step 1: Add workspace dependencies

In the root `Cargo.toml`, add to `[workspace.members]` and
`[workspace.dependencies]`:

```toml
[workspace.dependencies]
ipmi_kcs.path = "vm/devices/ipmi_kcs"
ipmi_kcs_resources.path = "vm/devices/ipmi_kcs_resources"
```

#### Step 2: Register the resolver

In `openvmm/openvmm_resources/src/lib.rs`, add to `register_static_resolvers!`:

```rust
#[cfg(guest_arch = "x86_64")]
ipmi_kcs::resolver::IpmiKcsResolver,
```

#### Step 3: Wire into petri test framework

In `petri/src/vm/openvmm/modify.rs`, add a helper method:

```rust
pub fn with_ipmi_kcs(mut self) -> Self {
    self.config.chipset_devices.push(ChipsetDeviceHandle {
        name: "ipmi_kcs".to_string(),
        resource: ipmi_kcs_resources::IpmiKcsHandle.into_resource(),
    });
    self
}
```

### 2.4 Implementation Order

1. **`ipmi_kcs_resources`** — Resource handle crate (tiny, no logic)
2. **`ipmi_kcs/src/protocol.rs`** — KCS state machine with unit tests
3. **`ipmi_kcs/src/sel.rs`** — SEL storage with unit tests
4. **`ipmi_kcs/src/lib.rs`** — Device wiring (ChipsetDevice + PortIoIntercept +
   SaveRestore with `NoSavedState` + `#[derive(InspectMut)]`)
5. **`ipmi_kcs/src/resolver.rs`** — Resolver registration (calls `omit_saved_state()`)
6. **Integration** — Workspace Cargo.toml, openvmm_resources, petri modify.rs

> **Note**: `SaveRestore` must be implemented from the start (even as a
> no-op using `NoSavedState`) because the `From<T> for ResolvedChipsetDevice`
> bound requires `T: ChangeDeviceState + ChipsetDevice + ProtobufSaveRestore
> + InspectMut`. The device will not compile without it. Real state
> persistence can be added later by replacing `NoSavedState` with a proper
> `SavedState` struct.

### 2.5 Key Implementation Notes

- **Trust boundary**: The guest can write arbitrary bytes. The KCS state machine
  and IPMI command handlers must never panic. Use `open_enum!` for all
  command/status values. Return appropriate error completion codes for unknown
  commands (0xC1 = invalid command, 0xC7 = request data length invalid).
- **No interrupts needed initially**: The polled KCS interface works without
  IRQ — the guest polls the status register. The `ipmi_si` Linux driver
  supports polled mode. This simplifies the implementation significantly.
- **Rate limiting**: If adding tracing for unknown commands or protocol errors,
  use `tracelimit::warn_ratelimited!` since the guest can trigger these at
  high frequency.

---

## 3. Phase 1a: VMM Test for SEL Events

### 3.1 Test Environment Decision: Shell Script via `/dev/port`

#### Background

The original plan proposed a static Rust test binary cross-compiled to
`x86_64-unknown-linux-musl`. Manual testing against a real Alpine Linux
guest revealed a simpler approach is viable:

- The Alpine `linux-virt` kernel has `CONFIG_DEVPORT=y`, so `/dev/port`
  is always available — no kernel modules needed.
- Shell-based I/O via `dd`/`od` on `/dev/port` was manually verified to
  work correctly: Get Device ID, Add SEL Entry, Get SEL Info all produce
  correct responses.
- The KCS protocol completes synchronously within the device (no async
  delays), so the shell fork overhead doesn't cause timeouts.

This eliminates the need for a new crate, musl cross-compilation, and
CI artifact pipeline changes.

**Approaches considered:**

- **Option A — Shell script via `/dev/port`** (chosen): Simple, no new
  crates or build infrastructure. Proven via manual testing. Each I/O port
  operation forks a few processes, but the virtual device responds instantly
  so there is no risk of timeouts.
- **Option B — Static Rust test binary**: More robust for real hardware,
  but overkill for a virtual device. Requires a new crate, musl
  cross-compilation target, and CI artifact wiring.
- **Option C — Custom kernel with IPMI modules**: Too heavy — requires
  changes to kernel build infrastructure. Better suited for Phase 2.

### 3.2 Test Implementation

Add the test to `vmm_tests/vmm_tests/tests/tests/x86_64.rs`. The test
uses inline shell commands via `/dev/port` to exercise the KCS protocol:

```rust
/// Test that the IPMI KCS device correctly handles SEL operations.
/// Uses /dev/port to directly access KCS I/O ports (0xCA2-0xCA3),
/// sends IPMI commands via the KCS write/read protocol, and validates
/// responses.
#[openvmm_test(linux_direct_x64)]
async fn ipmi_kcs_sel(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Upload a test script that exercises the KCS interface via /dev/port.
    // The script implements the KCS write/read protocol using dd and od,
    // sends Get Device ID, Add SEL Entry, Get SEL Entry, and Get SEL Info,
    // and prints IPMI_TEST_PASS on success.
    let test_script = r#"#!/bin/sh
set -e
KCS_DATA=0xCA2
KCS_CMD=0xCA3

inb() {
    dd if=/dev/port bs=1 skip=$(($1)) count=1 2>/dev/null | od -An -tx1 | tr -d ' \n'
}

outb() {
    printf "\\$(printf '%03o' $2)" | dd of=/dev/port bs=1 seek=$(($1)) count=1 2>/dev/null
}

wait_ibf() {
    for i in $(seq 1 1000); do
        s=$(inb $KCS_CMD)
        if [ $((0x$s & 2)) -eq 0 ]; then return 0; fi
    done
    echo "FAIL: IBF timeout"; exit 1
}

# Send a KCS command and collect the response as hex bytes.
# Usage: kcs_transfer <byte1> <byte2> ...
# Prints response bytes (space-separated hex) to stdout.
kcs_transfer() {
    local bytes="$@"
    local last=""
    local count=0

    # WRITE_START
    wait_ibf
    outb $KCS_CMD 0x61

    # Write all bytes except last
    for b in $bytes; do
        count=$((count + 1))
        if [ -n "$last" ]; then
            wait_ibf
            inb $KCS_DATA >/dev/null
            outb $KCS_DATA $last
        fi
        last=$b
    done

    # WRITE_END + last byte
    wait_ibf
    outb $KCS_CMD 0x62
    wait_ibf
    inb $KCS_DATA >/dev/null
    outb $KCS_DATA $last

    # READ phase
    local resp=""
    while true; do
        wait_ibf
        local status=$(inb $KCS_CMD)
        local byte=$(inb $KCS_DATA)
        local state=$((0x$status & 0xC0))
        if [ $state -ne 64 ]; then break; fi
        resp="$resp $byte"
        outb $KCS_DATA 0x68
    done
    echo $resp
}

# 1. Get Device ID (NetFn=App 0x06, Cmd=0x01)
resp=$(kcs_transfer 0x18 0x01)
cc=$(echo $resp | awk '{print $3}')
if [ "$cc" != "00" ]; then
    echo "FAIL: Get Device ID cc=$cc resp=$resp"; exit 1
fi
echo "Get Device ID: OK"

# 2. Add SEL Entry (NetFn=Storage 0x0A, Cmd=0x44, 16-byte record)
resp=$(kcs_transfer 0x28 0x44 0x00 0x00 0x02 0x00 0x00 0x00 0x00 0x20 0x00 0x04 0x01 0x42 0x6f 0x01 0x02 0x03)
cc=$(echo $resp | awk '{print $3}')
if [ "$cc" != "00" ]; then
    echo "FAIL: Add SEL Entry cc=$cc resp=$resp"; exit 1
fi
rec_lo=$(echo $resp | awk '{print $4}')
rec_hi=$(echo $resp | awk '{print $5}')
echo "Add SEL Entry: OK (id=${rec_hi}${rec_lo})"

# 3. Get SEL Entry (read back what we just wrote)
resp=$(kcs_transfer 0x28 0x43 0x00 0x00 $rec_lo $rec_hi 0x00 0xff)
cc=$(echo $resp | awk '{print $3}')
if [ "$cc" != "00" ]; then
    echo "FAIL: Get SEL Entry cc=$cc resp=$resp"; exit 1
fi
# Record data starts at field 6 (after NetFn, Cmd, CC, NextID_lo, NextID_hi)
rec_type=$(echo $resp | awk '{print $8}')
sensor_num=$(echo $resp | awk '{print $17}')
if [ "$rec_type" != "02" ] || [ "$sensor_num" != "42" ]; then
    echo "FAIL: SEL record mismatch type=$rec_type sensor=$sensor_num resp=$resp"; exit 1
fi
echo "Get SEL Entry: OK (verified)"

# 4. Get SEL Info (verify count = 1)
resp=$(kcs_transfer 0x28 0x40)
cc=$(echo $resp | awk '{print $3}')
count_lo=$(echo $resp | awk '{print $5}')
count_hi=$(echo $resp | awk '{print $6}')
if [ "$cc" != "00" ] || [ "$count_lo" != "01" ] || [ "$count_hi" != "00" ]; then
    echo "FAIL: SEL Info cc=$cc count=${count_hi}${count_lo} resp=$resp"; exit 1
fi
echo "Get SEL Info: OK (count=1)"

echo "IPMI_TEST_PASS"
"#;

    cmd!(sh, "cat > /tmp/ipmi_test.sh << 'SCRIPT_EOF'\n{test_script}\nSCRIPT_EOF")
        .run().await?;
    cmd!(sh, "chmod +x /tmp/ipmi_test.sh").run().await?;

    let output = cmd!(sh, "/tmp/ipmi_test.sh")
        .read()
        .await
        .context("IPMI KCS test script failed")?;

    assert!(
        output.contains("IPMI_TEST_PASS"),
        "IPMI KCS SEL test did not pass: {output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

### 3.3 Running the Test

```bash
cargo xflowey vmm-tests-run \
    --filter "test(ipmi_kcs_sel)" \
    --target windows-x64 \
    --dir /mnt/d/vmm_tests_out/
```

### 3.4 Unit Tests (already implemented in Phase 1)

The `ipmi_kcs` crate already contains 21 unit tests covering:

- KCS write/read roundtrip (Get Device ID)
- SEL add and get entry roundtrip
- SEL clear
- KCS error recovery (GET_STATUS/ABORT)
- Unknown command completion codes
- Invalid access sizes and registers
- SEL info after operations
- SEL time get/set
- Multiple entries and next-record-ID chaining

---

## 4. Phase 2: UEFI Support via SMBIOS

Phase 2 extends the IPMI device for UEFI-booted guests. Linux discovers
the IPMI KCS interface via SMBIOS Type 38, which is sufficient for
`ipmi_si` auto-detection — no ACPI tables are required at this stage.

**Design principle**: The I/O port base address (`0xCA2`) is a static,
well-known constant shared between OpenVMM and mu_msvm. OpenVMM only
needs to pass a single boolean flag (`ipmi_configured`) to the UEFI
firmware via the config blob. The firmware generates the SMBIOS Type 38
entry locally using the hardcoded address.

### 4.1 OpenVMM Changes

#### 4.1.1 Add `ipmi_configured` to UEFI Config Flags Bitfield

**File**: `vm/loader/src/uefi/config.rs` (Flags struct, ~line 292)

Add a new boolean field at bit 30 (first bit of current `_reserved`):

```rust
    pub hv_sint_enabled: bool,          // bit 29 (existing)
    pub ipmi_configured: bool,          // bit 30 (new)

    #[bits(33)]
    _reserved: u64,                     // was 34 bits, now 33
```

This field tells the UEFI firmware to generate SMBIOS Type 38 and ACPI
SPMI table entries for the IPMI KCS device.

#### 4.1.2 Add `enable_ipmi` to `LoadMode::Uefi`

**File**: `openvmm/openvmm_defs/src/config.rs` (LoadMode enum, ~line 111)

Add a new field to the `Uefi` variant:

```rust
    Uefi {
        firmware: File,
        // ... existing fields ...
        enable_vpci_boot: bool,
        enable_ipmi: bool,              // new
        uefi_console_mode: Option<UefiConsoleMode>,
        // ...
    },
```

#### 4.1.3 Add `ipmi` to `UefiLoadSettings` and Set the Flag

**File**: `openvmm/openvmm_core/src/worker/vm_loaders/uefi.rs`

Add to `UefiLoadSettings` (~line 30):

```rust
pub struct UefiLoadSettings {
    // ... existing fields ...
    pub ipmi: bool,
}
```

In `load_uefi()`, set the flag in the config blob builder (~line 90):

```rust
let flags = config::Flags::new()
    // ... existing flags ...
    .with_ipmi_configured(load_settings.ipmi);
```

#### 4.1.4 Wire Through `dispatch.rs`

**File**: `openvmm/openvmm_core/src/worker/dispatch.rs` (~line 2441)

In the `LoadMode::Uefi` arm of `load_firmware()`, pass the new field
to `UefiLoadSettings`:

```rust
let load_settings = super::vm_loaders::uefi::UefiLoadSettings {
    // ... existing fields ...
    ipmi: enable_ipmi,
};
```

The `enable_ipmi` field is destructured from `LoadMode::Uefi` at ~line 2430.

#### 4.1.5 Update petri `with_ipmi_kcs()` to Set the UEFI Flag

**File**: `petri/src/vm/openvmm/modify.rs` (~line 63)

Update `with_ipmi_kcs()` to also set the UEFI enable flag (following the
battery pattern):

```rust
pub fn with_ipmi_kcs(mut self) -> Self {
    self.config.chipset_devices.push(ChipsetDeviceHandle {
        name: "ipmi_kcs".to_string(),
        resource: ipmi_kcs_resources::IpmiKcsHandle.into_resource(),
    });
    if let LoadMode::Uefi { enable_ipmi, .. } = &mut self.config.load_mode {
        *enable_ipmi = true;
    }
    self
}
```

#### 4.1.6 Set Defaults

**File**: `petri/src/vm/openvmm/construct.rs` (~lines 698, 898)

Add `enable_ipmi: false` to each `LoadMode::Uefi` construction site (two
locations).

**File**: `openvmm/openvmm_entry/src/lib.rs` (~line 1088)

Add `enable_ipmi: opt.ipmi_kcs` to the `LoadMode::Uefi` construction.

### 4.2 UEFI Firmware Changes (mu_msvm)

All values (I/O port addresses, interface type, register spacing) are
hardcoded constants in the UEFI firmware — OpenVMM does not pass them.

> **Building**: See `building_from_wsl.md` in the root of the mu_msvm
> repo for instructions on building and validating UEFI firmware changes.

#### 4.2.1 Add `IpmiConfigured` Flag Bit

**File**: `MsvmPkg/Include/BiosInterface.h` (~line 790)

Add the flag after `MtrrsInitializedAtLoad` — note that `HvSintEnabled`
(bit 29 on the Rust side) is not consumed by UEFI, so we skip it:

```c
        UINT64 MtrrsInitializedAtLoad : 1;   // bit 28 (existing)
        UINT64 HvSintEnabled : 1;            // bit 29 (new, unused by UEFI, for alignment)
        UINT64 IpmiConfigured : 1;           // bit 30 (new)
        UINT64 Reserved:33;                  // was 35, now 33
```

#### 4.2.2 Add PCD for IPMI Configured

**File**: `MsvmPkg/MsvmPkg.dec` (~line 301)

Add a new PCD declaration (next available token ID):

```
  gMsvmPkgTokenSpaceGuid.PcdIpmiConfigured|FALSE|BOOLEAN|0x6072
```

**File**: `MsvmPkg/MsvmPkgX64.dsc` and `MsvmPkg/MsvmPkgAARCH64.dsc`

Add to the `[PcdsDynamicExDefault]` section:

```
  gMsvmPkgTokenSpaceGuid.PcdIpmiConfigured|FALSE
```

#### 4.2.3 Wire Flag in PlatformPei Config Parser

**File**: `MsvmPkg/PlatformPei/Config.c` (~line 950, in
`ConfigSetUefiConfigFlags`)

Add after the existing PCD-set calls:

```c
    PEI_FAIL_FAST_IF_FAILED(PcdSetBoolS(PcdIpmiConfigured,
        (UINT8) ConfigFlags->Flags.IpmiConfigured));
```

Add `PcdIpmiConfigured` to the PEI module's `.inf` file under `[Pcd]`.

#### 4.2.4 Add SMBIOS Type 38 — IPMI Device Information

**File**: `MsvmPkg/SmbiosPlatformDxe/SmbiosPlatform.c`

Add a new function `AddIpmiDeviceInformation()`:

```c
VOID
AddIpmiDeviceInformation(
    IN EFI_SMBIOS_PROTOCOL *Smbios
    )
{
    if (!PcdGetBool(PcdIpmiConfigured))
    {
        return;
    }

    // SMBIOS Type 38 structure per SMBIOS Specification v3.x
    #pragma pack(1)
    typedef struct {
        SMBIOS_STRUCTURE  Header;
        UINT8             InterfaceType;    // 0x01 = KCS
        UINT8             IpmiSpecRev;      // 0x20 = IPMI 2.0
        UINT8             I2CTargetAddr;    // 0x20 (BMC default)
        UINT8             NvStorageDevice;  // 0xFF = not present
        UINT64            BaseAddress;      // 0xCA2 (I/O space)
        UINT8             BaseAddrModifier; // register spacing
        UINT8             InterruptNumber;  // 0x00 = polled
    } SMBIOS_TYPE38_IPMI;
    #pragma pack()

    SMBIOS_TYPE38_IPMI record = {
        .Header = {
            .Type   = 38,
            .Length = sizeof(SMBIOS_TYPE38_IPMI),
            .Handle = 0,
        },
        .InterfaceType   = 0x01,  // KCS
        .IpmiSpecRev     = 0x20,  // IPMI 2.0
        .I2CTargetAddr   = 0x20,
        .NvStorageDevice = 0xFF,
        .BaseAddress     = 0x0000000000000CA3, // bit 0 = 1 → I/O port space; actual addr = 0xCA3 & ~1 = 0xCA2
        .BaseAddrModifier = 0x01, // 1-byte spacing, I/O space
        .InterruptNumber = 0x00,  // polled, no interrupt
    };

    AddStructure(Smbios, (SMBIOS_STRUCTURE *)&record, NULL, NULL);
}
```

Call from `AddAllStructures()` (~line 1949):

```c
    AddMemoryStructures(Smbios);
    AddSystemBootInformation(Smbios);
    AddIpmiDeviceInformation(Smbios);   // new
```

Add `PcdIpmiConfigured` to the SmbiosPlatformDxe `.inf` under `[Pcd]`.

### 4.3 Alpine UEFI VMM Test

The VMM test validates SMBIOS-based IPMI discovery using the Alpine UEFI
VHD. The test does **not** require network access — it uses only kernel
modules and the raw `/dev/port` approach from Phase 1a.

> **Warning**: The Alpine `linux-virt` kernel in the test VHD image may
> not have `ipmi_si` and `ipmi_devintf` modules built. If `modprobe`
> fails, the test falls back to raw `/dev/port` I/O for functional
> validation (same approach as Phase 1a). If the kernel does have IPMI
> modules, the test validates full SMBIOS-based discovery. The test
> approach may need to be adjusted based on what the image supports.

```rust
/// Test that the IPMI KCS device is discoverable via SMBIOS Type 38 on
/// a UEFI-booted Alpine guest. Falls back to /dev/port validation if
/// ipmi_si kernel module is not available.
#[openvmm_test(
    uefi_x64(vhd(alpine_3_23_x64)),
)]
async fn ipmi_kcs_uefi_smbios(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.unix_shell();

    // First, verify SMBIOS Type 38 is visible to the guest
    let dmidecode_check = cmd!(sh, "test -e /sys/firmware/dmi/entries/38-0")
        .run()
        .await;

    if dmidecode_check.is_ok() {
        tracing::info!("SMBIOS Type 38 entry found");
    }

    // Attempt kernel-based IPMI discovery (preferred path)
    let ipmi_si_result = cmd!(sh, "modprobe ipmi_si 2>&1").read().await;
    let has_ipmi_si = ipmi_si_result.is_ok()
        && !ipmi_si_result.as_deref().unwrap_or("").contains("not found");

    if has_ipmi_si {
        // Full SMBIOS discovery path: ipmi_si auto-detects from Type 38
        cmd!(sh, "modprobe ipmi_devintf").run().await?;
        cmd!(sh, "test -e /dev/ipmi0")
            .run()
            .await
            .context("/dev/ipmi0 not found — SMBIOS discovery may have failed")?;
    }

    // Functional validation via /dev/port (always works, no modules needed).
    // This reuses the Phase 1a approach: send Get Device ID and verify response.
    let output = cmd!(sh, r#"sh -c '
        inb() { dd if=/dev/port bs=1 skip=$(($1)) count=1 2>/dev/null | od -An -tx1 | tr -d " \n"; }
        outb() { printf "\\$(printf "%03o" $2)" | dd of=/dev/port bs=1 seek=$(($1)) count=1 2>/dev/null; }
        wait_ibf() { for i in $(seq 1 1000); do s=$(inb 0xCA3); if [ $((0x$s & 2)) -eq 0 ]; then return 0; fi; done; return 1; }
        wait_ibf && outb 0xCA3 0x61
        wait_ibf && inb 0xCA2 >/dev/null && outb 0xCA2 0x18
        wait_ibf && outb 0xCA3 0x62
        wait_ibf && inb 0xCA2 >/dev/null && outb 0xCA2 0x01
        wait_ibf
        status=$(inb 0xCA3)
        data=$(inb 0xCA2)
        state=$((0x$status & 0xC0))
        if [ $state -eq 64 ]; then echo "KCS_READ_OK"; else echo "KCS_FAIL state=$state"; fi
    '"#)
    .read()
    .await
    .context("KCS Get Device ID failed")?;

    assert!(
        output.contains("KCS_READ_OK"),
        "IPMI KCS device not responding on UEFI boot: {output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

### 4.4 Manual Validation via OpenVMM + setup-alpine.sh

Before relying on the VMM test, manually validate SMBIOS discovery using
a live Alpine VM with network access and full IPMI tooling.

#### Steps

1. **Set up an Alpine VM with networking**:
   ```bash
   # Use openvmm with UEFI boot and the IPMI device enabled
   cargo run -p openvmm -- \
       --uefi <path-to-uefi-firmware> \
       --ipmi-kcs \
       --disk <alpine-vhd> \
       --serial
   ```

2. **Inside the Alpine guest, install packages**:
   ```sh
   # If using a fresh setup-alpine.sh install:
   apk add ipmitool dmidecode
   ```

3. **Verify SMBIOS Type 38 is present**:
   ```sh
   dmidecode -t 38
   ```
   Expected output should show "IPMI Device Information" with
   Interface Type = KCS, Base Address = 0xCA2.

4. **Load IPMI kernel modules** (auto-detect from SMBIOS):
   ```sh
   modprobe ipmi_devintf
   modprobe ipmi_si    # No type= or ports= — must auto-detect
   ```
   If `modprobe ipmi_si` succeeds without explicit parameters, SMBIOS
   discovery is working.

5. **Verify `/dev/ipmi0` exists**:
   ```sh
   ls -la /dev/ipmi0
   ```

6. **Test with ipmitool**:
   ```sh
   # Get Device ID
   ipmitool mc info

   # Add a SEL entry
   ipmitool raw 0x0a 0x44 0x00 0x00 0x02 0x00 0x00 0x00 \
       0x00 0x20 0x00 0x04 0x01 0x42 0x6f 0x01 0x02 0x03

   # Check SEL
   ipmitool sel info
   ipmitool sel list

   # Clear SEL
   ipmitool sel clear
   ```

7. **Check kernel log for discovery method**:
   ```sh
   dmesg | grep -i ipmi
   ```
   Look for `ipmi_si: Trying SMBIOS-specified kcs state machine`
   to confirm the kernel found the device via SMBIOS Type 38.

> **Purpose**: This manual step confirms the end-to-end SMBIOS discovery
> chain before committing to a CI test approach. If the Alpine kernel
> lacks `ipmi_si`, this step will reveal it early.

### 4.5 Phase 2 Implementation Order

1. **OpenVMM config plumbing** — `config.rs` Flags bitfield, `LoadMode::Uefi`
   field, `UefiLoadSettings`, `dispatch.rs` wiring, petri `with_ipmi_kcs()`
   update, construct.rs defaults, CLI entry point.
2. **UEFI firmware flag** — `BiosInterface.h` flag bit, PCD declaration and
   wiring in `MsvmPkg.dec`, `.dsc` files, `PlatformPei/Config.c`.
3. **SMBIOS Type 38** — `SmbiosPlatformDxe/SmbiosPlatform.c`, gated on
   `PcdIpmiConfigured`.
4. **Manual validation** — Use `openvmm --ipmi-kcs` with Alpine +
   `setup-alpine.sh` to verify `dmidecode -t 38` and `modprobe ipmi_si`
   auto-detection (Section 4.4).
5. **Alpine UEFI VMM test** — Automated test (Section 4.3), adjusted
   based on manual validation findings.

### 4.6 Phase 2 File Checklist

#### Modified Files (OpenVMM)

| File | Change |
|------|--------|
| `vm/loader/src/uefi/config.rs` | Add `ipmi_configured` to `Flags` bitfield (bit 30) |
| `openvmm/openvmm_defs/src/config.rs` | Add `enable_ipmi: bool` to `LoadMode::Uefi` |
| `openvmm/openvmm_core/src/worker/vm_loaders/uefi.rs` | Add `ipmi: bool` to `UefiLoadSettings`, set flag in blob |
| `openvmm/openvmm_core/src/worker/dispatch.rs` | Wire `enable_ipmi` through to `UefiLoadSettings` |
| `petri/src/vm/openvmm/modify.rs` | Update `with_ipmi_kcs()` to set UEFI `enable_ipmi` flag |
| `petri/src/vm/openvmm/construct.rs` | Add `enable_ipmi: false` defaults (2 locations) |
| `openvmm/openvmm_entry/src/lib.rs` | Add `enable_ipmi: opt.ipmi_kcs` to `LoadMode::Uefi` |

#### Modified Files (mu_msvm)

| File | Change |
|------|--------|
| `MsvmPkg/Include/BiosInterface.h` | Add `HvSintEnabled` + `IpmiConfigured` bits, adjust Reserved |
| `MsvmPkg/MsvmPkg.dec` | Add `PcdIpmiConfigured` PCD declaration (token `0x6072`) |
| `MsvmPkg/MsvmPkgX64.dsc` | Add `PcdIpmiConfigured` default |
| `MsvmPkg/MsvmPkgAARCH64.dsc` | Add `PcdIpmiConfigured` default |
| `MsvmPkg/PlatformPei/Config.c` | Add `PcdSetBoolS(PcdIpmiConfigured, ...)` |
| `MsvmPkg/PlatformPei/PlatformPei.inf` | Add `PcdIpmiConfigured` to `[Pcd]` |
| `MsvmPkg/SmbiosPlatformDxe/SmbiosPlatform.c` | Add `AddIpmiDeviceInformation()`, call from `AddAllStructures()` |
| `MsvmPkg/SmbiosPlatformDxe/SmbiosPlatformDxe.inf` | Add `PcdIpmiConfigured` to `[Pcd]` |

#### Test Files

| File | Change |
|------|--------|
| `vmm_tests/vmm_tests/tests/tests/x86_64.rs` | Add `ipmi_kcs_uefi_smbios` test |

---

## 5. Phase 3: Windows VMM Test

Phase 3 adds a Windows UEFI VMM test to determine whether Windows can
discover and use the IPMI KCS device with only SMBIOS Type 38 support
(no ACPI). This is an exploratory phase — if Windows requires ACPI
device enumeration, Phase 4 adds the necessary ACPI support.

### 5.1 Background: Windows IPMI Discovery

Windows Server editions include a built-in IPMI driver stack:

- **ipmidrv.sys** — The Microsoft IPMI driver, which registers as a WMI
  provider. On Server editions, this driver is available by default.
- **WMI interface** — IPMI commands can be sent via the
  `root\WMI` namespace using `Microsoft_IPMI` class methods.

Windows typically discovers IPMI via ACPI PnP enumeration
(`ACPI\IPI0001` device node in the DSDT). It's unclear whether Windows
also synthesizes devices from SMBIOS Type 38 alone. This phase tests
that question empirically.

**Expected outcomes:**
- **Best case**: Windows discovers the device from SMBIOS Type 38,
  loads `ipmidrv.sys`, and the full test passes. Phase 4 is not needed.
- **Likely case**: Windows does not enumerate the device without an
  ACPI DSDT node. The test fails at device enumeration, confirming
  Phase 4 is required.

### 5.2 Test Strategy

The test checks for device presence and, if found, validates
functionality:

1. **Device enumeration** — Check if the IPMI device appears in the
   Windows device tree after boot.
2. **Driver loading** — If enumerated, verify `ipmidrv` service is running.
3. **Functional SEL operations** — If the driver loaded, send IPMI
   commands via WMI and validate responses.

### 5.3 Test Implementation

Add the test to `vmm_tests/vmm_tests/tests/tests/x86_64.rs`:

```rust
/// Test that the IPMI KCS device is discovered and functional on Windows.
/// Requires Phase 2 SMBIOS/ACPI support to be in place.
#[openvmm_test(
    uefi_x64(vhd(windows_datacenter_core_2025_x64_prepped)),
)]
async fn ipmi_kcs_windows(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.windows_shell();

    // 1. Verify IPMI device is enumerated via PnP
    let pnp_output = cmd!(
        sh,
        "powershell.exe -Command Get-PnpDevice -InstanceId 'ACPI\\IPI0001*' | Select-Object -ExpandProperty Status"
    )
    .read()
    .await
    .context("Failed to query IPMI PnP device")?;

    assert!(
        pnp_output.contains("OK"),
        "IPMI device not found or not OK: {pnp_output}"
    );

    // 2. Verify IPMI driver service is running
    let svc_output = cmd!(
        sh,
        "powershell.exe -Command (Get-Service ipmidrv -ErrorAction SilentlyContinue).Status"
    )
    .read()
    .await
    .context("Failed to query ipmidrv service")?;

    assert!(
        svc_output.trim() == "Running",
        "ipmidrv service not running: {svc_output}"
    );

    // 3. Send IPMI Get Device ID via WMI and verify response
    //    The Microsoft_IPMI WMI class provides RequestResponse method.
    //    Command: NetFn=App(0x06), Lun=0, Cmd=0x01 (Get Device ID)
    let wmi_output = cmd!(
        sh,
        r#"powershell.exe -Command "$ipmi = Get-WmiObject -Namespace root\WMI -Class Microsoft_IPMI; $req = $ipmi.GetMethodParameters('RequestResponse'); $req.NetworkFunction = 6; $req.Lun = 0; $req.ResponderAddress = 0x20; $req.Command = 1; $req.RequestData = @(); $resp = $ipmi.InvokeMethod('RequestResponse', $req); $resp.CompletionCode""#
    )
    .read()
    .await
    .context("Failed to send IPMI Get Device ID via WMI")?;

    assert!(
        wmi_output.trim() == "0",
        "Get Device ID failed, completion code: {wmi_output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

### 5.4 Test Alternatives

If WMI interaction proves too complex or unreliable, a simpler
enumeration-only test is viable:

```rust
/// Simplified test: just verify the IPMI device is enumerated.
#[openvmm_test(
    uefi_x64(vhd(windows_datacenter_core_2025_x64_prepped)),
)]
async fn ipmi_kcs_windows_enumeration(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.windows_shell();

    // Check for IPMI device in device tree
    let output = cmd!(
        sh,
        "pnputil.exe /enum-devices /connected"
    )
    .read()
    .await?;

    assert!(
        output.contains("IPI0001"),
        "IPMI device (IPI0001) not found in connected devices:\n{output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

### 5.5 Running the Windows Test

```bash
cargo xflowey vmm-tests-run \
    --filter "test(ipmi_kcs_windows)" \
    --target windows-x64 \
    --dir /mnt/d/vmm_tests_out/
```

### 5.6 Phase 3 Implementation Order

1. **Ensure Phase 2 is complete** — SMBIOS Type 38 and config flag
   plumbing must be in place.
2. **Add Windows test** — Implement `ipmi_kcs_windows` in
   `vmm_tests/vmm_tests/tests/tests/x86_64.rs`.
3. **Run the test** — Boot a Windows Server 2025 VHD and observe:
   - Does the IPMI device appear in Device Manager?
   - Does `ipmidrv.sys` load?
   - If yes, does WMI-based IPMI communication work?
4. **Assess results** — If Windows requires ACPI for enumeration,
   proceed to Phase 4. If SMBIOS alone works, Phase 4 can be skipped.

### 5.7 Phase 3 File Checklist

| File | Change |
|------|--------|
| `vmm_tests/vmm_tests/tests/tests/x86_64.rs` | Add `ipmi_kcs_windows` test |

---

## 6. Phase 4 (Optional): ACPI Support

Phase 4 adds ACPI-based IPMI device discovery. This is needed if Phase 3
reveals that Windows does not discover the device from SMBIOS Type 38
alone. Linux does not require this phase.

### 6.1 ACPI DSDT Device Node

Add an IPMI device node in the DSDT so Windows enumerates it via
`ACPI\IPI0001`.

**File**: `MsvmPkg/AcpiTables/Dsdt.asl`

```asl
Device (IPMI) {
    Name (_HID, "IPI0001")    // IPMI KCS device
    Name (_STR, Unicode("IPMI_KCS"))
    Name (_UID, 0)

    Name (_CRS, ResourceTemplate () {
        IO (Decode16, 0x0CA2, 0x0CA2, 0x01, 0x02)
    })

    Method (_STA, 0, NotSerialized) {
        Return (0x0F) // Present, enabled, functioning
    }
}
```

> **Note on conditionality**: Making `_STA` conditional on
> `PcdIpmiConfigured` requires injecting the PCD value into the DSDT via
> an OpRegion or external reference. A simpler approach is to always
> include the device node — the I/O port intercept only exists when the
> IPMI chipset device is configured, so the device will be non-functional
> (no response) if the flag is not set. Windows and Linux both tolerate a
> non-responsive ACPI device gracefully.

### 6.2 ACPI SPMI Table (Optional)

The SPMI (Service Processor Management Interface) table provides
redundant discovery metadata when SMBIOS Type 38 is already present.
Neither Linux nor Windows strictly requires it. Add only if a specific
OS or tool (e.g., `ipmitool`) benefits from it.

Per ACPI Specification v6.x, Section 5.2.16:

| Field | Value |
|-------|-------|
| Signature | "SPMI" |
| Interface Type | 0x01 = KCS |
| Spec Revision | 0x0200 |
| Interrupt Type | 0x00 = none (polled) |
| GPE | 0x00 |
| PCI Device Flag | 0x00 = not PCI |
| Base Address | GAS: I/O Space, 8-bit, offset 0, 0xCA2 |
| Register Spacing | 1 byte |

Implement as a new `MsvmPkg/AcpiTables/Spmi.aslc` or inline in
`AcpiPlatformDxe`, gated on `PcdIpmiConfigured`.

### 6.3 OpenVMM Linux Direct Boot — ACPI Table Updates (Optional)

For Linux direct boot, OpenVMM builds ACPI tables itself (no UEFI
firmware involved). Phase 1 already works without ACPI because the
`ipmi_si` driver probes the default KCS address. For completeness:

**File**: `vm/acpi/src/dsdt.rs` — Add `add_ipmi_kcs()` method.

**File**: `openvmm/openvmm_core/src/worker/dispatch.rs` — Add
`ipmi_enabled: bool` parameter to `add_devices_to_dsdt()`.

**File**: `vmm_core/src/acpi_builder.rs` — Add `build_spmi()` method.

### 6.4 Phase 4 Implementation Order

1. **DSDT device node** — `Dsdt.asl` with `IPI0001` device (required
   for Windows).
2. **(Optional) SPMI table** — If any OS benefits from it.
3. **(Optional) Linux direct ACPI** — `dsdt.rs`, `acpi_builder.rs`,
   `dispatch.rs` plumbing.
4. **Re-run Phase 3 Windows test** — Verify device enumeration and full
   WMI communication.

### 6.5 Phase 4 File Checklist

#### Modified Files (mu_msvm)

| File | Change |
|------|--------|
| `MsvmPkg/AcpiTables/Dsdt.asl` | Add `IPI0001` device node |

#### New Files (mu_msvm) — Optional

| File | Purpose |
|------|----------|
| `MsvmPkg/AcpiTables/Spmi.aslc` (or inline in AcpiPlatformDxe) | ACPI SPMI table for IPMI KCS |

#### Optional Modified Files (OpenVMM — Linux Direct ACPI)

| File | Change |
|------|--------|
| `vm/acpi/src/dsdt.rs` | Add `add_ipmi_kcs()` method |
| `vmm_core/src/acpi_builder.rs` | Add `build_spmi()` method |
| `openvmm/openvmm_core/src/worker/dispatch.rs` | Pass `ipmi_enabled` to `add_devices_to_dsdt()` |

---

## Appendix A: File Checklist

### New Files (Phase 1)

| File | Purpose |
|------|---------|
| `vm/devices/ipmi_kcs_resources/Cargo.toml` | Resource handle crate manifest |
| `vm/devices/ipmi_kcs_resources/src/lib.rs` | `IpmiKcsHandle` definition |
| `vm/devices/ipmi_kcs/Cargo.toml` | Device crate manifest |
| `vm/devices/ipmi_kcs/src/lib.rs` | `IpmiKcsDevice` + `ChipsetDevice` impl |
| `vm/devices/ipmi_kcs/src/resolver.rs` | Resource resolver |
| `vm/devices/ipmi_kcs/src/protocol.rs` | KCS state machine |
| `vm/devices/ipmi_kcs/src/sel.rs` | SEL storage + IPMI command handlers |
| `vmm_tests/ipmi_kcs_test_bin/Cargo.toml` | Guest-side test binary crate manifest |
| `vmm_tests/ipmi_kcs_test_bin/src/main.rs` | Static musl binary for KCS I/O port testing |

### Modified Files (Phase 1)

| File | Change |
|------|--------|
| `Cargo.toml` (root) | Add workspace members + dependencies |
| `openvmm/openvmm_resources/src/lib.rs` | Register `IpmiKcsResolver` |
| `petri/src/vm/openvmm/modify.rs` | Add `with_ipmi_kcs()` helper |
| `petri/Cargo.toml` | Add `ipmi_kcs_resources` dependency |
| `vmm_tests/vmm_tests/tests/tests/x86_64.rs` | Add `ipmi_kcs_sel` test |

### Modified Files (Phase 2) — see Section 4.6 for full details

| File | Change |
|------|--------|
| `vm/loader/src/uefi/config.rs` | Add `ipmi_configured` flag to Flags bitfield |
| `openvmm/openvmm_defs/src/config.rs` | Add `enable_ipmi` to `LoadMode::Uefi` |
| `openvmm/openvmm_core/src/worker/vm_loaders/uefi.rs` | Set flag in config blob |
| `openvmm/openvmm_core/src/worker/dispatch.rs` | Wire `enable_ipmi` through |
| `petri/src/vm/openvmm/modify.rs` | Set UEFI flag in `with_ipmi_kcs()` |
| `petri/src/vm/openvmm/construct.rs` | Add `enable_ipmi: false` defaults |
| `openvmm/openvmm_entry/src/lib.rs` | Wire CLI `--ipmi-kcs` to UEFI flag |
| `MsvmPkg/Include/BiosInterface.h` | Add `IpmiConfigured` flag bit |
| `MsvmPkg/MsvmPkg.dec` | Add `PcdIpmiConfigured` |
| `MsvmPkg/PlatformPei/Config.c` | Wire flag to PCD |
| `MsvmPkg/SmbiosPlatformDxe/SmbiosPlatform.c` | Add SMBIOS Type 38 |

### Modified Files (Phase 3)

| File | Change |
|------|--------|
| `vmm_tests/vmm_tests/tests/tests/x86_64.rs` | Add `ipmi_kcs_windows` test |

### Modified Files (Phase 4) — see Section 6.5 for full details

| File | Change |
|------|--------|
| `MsvmPkg/AcpiTables/Dsdt.asl` | Add `IPI0001` ACPI device node |

## Appendix B: IPMI Commands to Implement

### Required (Phase 1)

| NetFn | Command | Code | Notes |
|-------|---------|------|-------|
| App (0x06) | Get Device ID | 0x01 | Required for driver probe |
| Storage (0x0A) | Get SEL Info | 0x40 | Entry count, free space |
| Storage (0x0A) | Get SEL Entry | 0x43 | Read by Record ID |
| Storage (0x0A) | Add SEL Entry | 0x44 | Write 16-byte record |
| Storage (0x0A) | Clear SEL | 0x47 | Two-phase erase |
| Storage (0x0A) | Get SEL Time | 0x48 | BMC timestamp |
| Storage (0x0A) | Set SEL Time | 0x49 | Set BMC clock |

### Stub (Return 0xC1 — Invalid Command)

All other NetFn/Command combinations should return completion code `0xC1`.

## Appendix C: Reference Specifications

- **IPMI v2.0 Specification** (DMTF DSP0136) — Sections 9 (KCS), 31-32 (SEL)
- **SMBIOS Specification v3.x** (DMTF DSP0134) — Type 38 (IPMI Device Info)
- **ACPI Specification v6.x** — SPMI table (Section 5.2.16)
- **Linux kernel** — `drivers/char/ipmi/ipmi_si_intf.c` (KCS driver)
