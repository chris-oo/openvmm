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
4. [Phase 2 (Future): UEFI Support via SMBIOS & ACPI](#4-phase-2-future-uefi-support-via-smbios--acpi)

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
[NetFn/LUN] [Command] [Data...]
```

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
inspect.workspace = true
ipmi_kcs_resources.workspace = true
mesh.workspace = true
open_enum.workspace = true
thiserror.workspace = true
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
    /// KCS interface states (encoded in status register S1:S0).
    pub enum KcsState: u8 {
        IDLE_STATE  = 0x00,
        READ_STATE  = 0x40, // S0=1 (bit 6)
        WRITE_STATE = 0x80, // S1=1 (bit 7)
        ERROR_STATE = 0xC0, // S1=1, S0=1
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

open_enum! {
    /// I/O port offsets from KCS base address.
    pub enum KcsPort: u16 {
        DATA_REG      = 0xCA2,
        STATUS_CMD_REG = 0xCA3,
    }
}
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
| Get SEL Time (0x48) | Return current BMC time |
| Set SEL Time (0x49) | Update time offset |

Also implement `Get Device ID` (NetFn=App 0x06, Cmd=0x01) — Linux's `ipmi_si`
driver sends this during probe and won't complete initialization without a
valid response.

#### `lib.rs` — ChipsetDevice Implementation

```rust
pub struct IpmiKcsDevice {
    // KCS protocol state
    state: KcsState,
    status: u8,
    data_out: u8,
    write_buffer: Vec<u8>,
    read_buffer: VecDeque<u8>,
    read_pos: usize,

    // IPMI layer
    sel: SelStore,
}

impl ChipsetDevice for IpmiKcsDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PortIoIntercept for IpmiKcsDevice {
    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[
            ("kcs", 0xCA2..=0xCA3),
        ]
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
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
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
4. **`ipmi_kcs/src/lib.rs`** — Device wiring (ChipsetDevice + PortIoIntercept)
5. **`ipmi_kcs/src/resolver.rs`** — Resolver registration
6. **Integration** — Workspace Cargo.toml, openvmm_resources, petri modify.rs
7. **SaveRestore** — Implement state persistence (can be deferred)

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

### 3.1 Test Environment Decision: Linux Direct vs Alpine

#### Linux Direct Test Kernel

The Linux Direct test kernel (`bzImage` + `initrd` from
`.packages/underhill-deps-private/`) is **minimal** and has busybox as its
userspace. It almost certainly does **not** include the `ipmi_si` or
`ipmi_devintf` kernel modules, which are needed for the guest to talk to the
KCS interface.

#### Alpine 3.23 (UEFI boot)

Alpine 3.23 cloud images **do** package IPMI tools and kernel modules:
- `ipmitool` — available via `apk add ipmitool`
- `ipmi_si`, `ipmi_devintf`, `ipmi_msghandler` — available as kernel modules

**However**, Alpine tests require UEFI boot (the Alpine VHD images are
UEFI-only). For UEFI boot, the IPMI device needs proper ACPI/SMBIOS tables
to be discoverable — which is deferred to Phase 2.

#### Recommended Approach: Custom Linux Direct Test Kernel

For Phase 1 (Linux Direct, no ACPI), the most practical approach is:

**Option A — Direct I/O Port Access from Pipette (Preferred)**

Instead of relying on kernel IPMI drivers, write a **small Rust helper binary**
compiled into the test initrd (or use pipette's execute capability) that:
1. Directly reads/writes the KCS I/O ports (`0xCA2`/`0xCA3`) using `iopl()`
   and `inb()`/`outb()` system calls
2. Implements the KCS write/read protocol in userspace
3. Sends an "Add SEL Entry" IPMI command
4. Sends a "Get SEL Entry" IPMI command to read it back
5. Prints the result to stdout for validation

This works because:
- Linux Direct boot runs as root (pipette is PID 1)
- `iopl(3)` grants I/O port access from userspace
- No kernel modules needed
- Busybox's `devmem` doesn't help (that's MMIO), but we can embed a small
  static binary

**Option B — Pre-built test binary in initrd**

Build a static `ipmi_kcs_test` binary (Rust, musl target) that exercises the
KCS protocol. Include it in the test initrd alongside pipette.

This is the approach used by tests like `virtio_rng_device` which check
`/dev/hwrng` — except for IPMI on Linux Direct there's no kernel driver path,
so we use raw I/O ports.

**Option C — Build a custom kernel with IPMI modules**

Rebuild the Linux Direct test kernel with `CONFIG_IPMI_HANDLER=y`,
`CONFIG_IPMI_SI=y`, `CONFIG_IPMI_DEVICE_INTERFACE=y`. Then the guest can
use standard `/dev/ipmi0` and `ipmitool`.

This is a heavier approach and requires changes to the kernel build
infrastructure, but gives the most realistic test.

### 3.2 Recommended Test Strategy

Use **Option A** for Phase 1. The test helper can be built as a standalone
Rust crate in `vmm_tests/` that cross-compiles to `x86_64-unknown-linux-musl`
and gets embedded or shipped alongside test artifacts.

Alternatively, since pipette can already execute arbitrary programs in the
guest, we can use shell commands with busybox if the kernel exposes
`/dev/port` (which allows raw I/O port access without `iopl()`):

```sh
# Check if /dev/port exists (it does on most Linux kernels with devtmpfs)
test -e /dev/port

# Write to I/O port: echo byte at offset into /dev/port
# Read KCS status register (port 0xCA3):
dd if=/dev/port bs=1 count=1 skip=$((0xCA3)) 2>/dev/null | od -A n -t x1
```

If `/dev/port` is available in the test initrd, **no custom binary is needed**
— we can script the entire KCS protocol using `dd` on `/dev/port`. This is the
simplest approach and should be tried first.

### 3.3 Test Implementation

Add the test to `vmm_tests/vmm_tests/tests/tests/x86_64.rs`:

```rust
/// Test that the IPMI KCS device correctly handles SEL operations.
/// Adds a SEL entry via the KCS interface and reads it back.
#[openvmm_test(linux_direct_x64)]
async fn ipmi_kcs_sel(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Verify /dev/port exists for raw I/O access
    cmd!(sh, "test -e /dev/port")
        .run()
        .await
        .context("/dev/port not available — cannot test KCS I/O")?;

    // Helper shell functions for KCS I/O
    // (These would be uploaded as a script or inlined)
    //
    // The test script:
    // 1. Reads KCS status register to confirm IDLE state
    // 2. Performs KCS WRITE_START → write IPMI "Add SEL Entry" command
    // 3. Reads response (completion code 0x00 + record ID)
    // 4. Performs KCS WRITE_START → write IPMI "Get SEL Entry" command
    // 5. Reads response and validates the 16-byte SEL record matches
    // 6. Asserts the record data is correct

    // Upload and run the KCS test script
    let script = include_str!("ipmi_kcs_test.sh");
    agent.write_file("/tmp/ipmi_kcs_test.sh", script.as_bytes()).await?;
    let output = cmd!(sh, "sh /tmp/ipmi_kcs_test.sh")
        .read()
        .await
        .context("IPMI KCS SEL test failed")?;

    assert!(
        output.contains("SEL_TEST_PASS"),
        "IPMI KCS SEL test did not pass: {output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

#### Test Shell Script (`ipmi_kcs_test.sh`)

The shell script implements the KCS protocol via `/dev/port`:

```bash
#!/bin/sh
# IPMI KCS SEL Test via /dev/port
#
# KCS ports: Data=0xCA2, Status/Cmd=0xCA3
# Uses dd to read/write individual I/O port bytes.

set -e

DATA_PORT=51874    # 0xCA2
CMD_PORT=51875     # 0xCA3

# Read one byte from an I/O port
read_port() {
    dd if=/dev/port bs=1 count=1 skip="$1" 2>/dev/null | od -A n -t u1 | tr -d ' '
}

# Write one byte to an I/O port
write_port() {
    printf "\\$(printf '%03o' "$2")" | dd of=/dev/port bs=1 count=1 seek="$1" 2>/dev/null
}

# Wait for IBF=0 (bit 1 of status)
wait_ibf_clear() {
    for i in $(seq 1 1000); do
        status=$(read_port $CMD_PORT)
        if [ $((status & 2)) -eq 0 ]; then
            return 0
        fi
    done
    echo "FAIL: IBF timeout"
    exit 1
}

# Wait for OBF=1 (bit 0 of status)
wait_obf_set() {
    for i in $(seq 1 1000); do
        status=$(read_port $CMD_PORT)
        if [ $((status & 1)) -ne 0 ]; then
            return 0
        fi
    done
    echo "FAIL: OBF timeout"
    exit 1
}

# Read KCS status
status=$(read_port $CMD_PORT)
echo "Initial KCS status: $status"

# Verify IDLE state (S1:S0 = 00, bits 7:6)
state=$((status & 192))
if [ "$state" -ne 0 ]; then
    echo "FAIL: KCS not in IDLE state (state=$state)"
    exit 1
fi
echo "KCS is in IDLE state"

# === Send "Get Device ID" command (NetFn=App(0x06), Cmd=0x01) ===
# IPMI message: [NetFn/LUN=0x18] [Cmd=0x01]
# NetFn=0x06 << 2 | LUN=0x00 = 0x18

# Write WRITE_START command
wait_ibf_clear
write_port $CMD_PORT 0x61

# Wait for IBF clear and OBF set, read dummy
wait_ibf_clear
wait_obf_set
read_port $DATA_PORT > /dev/null

# Write NetFn/LUN byte
write_port $DATA_PORT 0x18

# Wait, read dummy
wait_ibf_clear
wait_obf_set
read_port $DATA_PORT > /dev/null

# Write WRITE_END command
wait_ibf_clear
write_port $CMD_PORT 0x62

# Wait, read dummy
wait_ibf_clear
wait_obf_set
read_port $DATA_PORT > /dev/null

# Write command byte (last data byte after WRITE_END)
write_port $DATA_PORT 0x01

# Now read response
wait_ibf_clear
wait_obf_set

# Check state is READ (0x40, bits 7:6 = 01)
status=$(read_port $CMD_PORT)
state=$((status & 192))
echo "Response state: $state (expect 64 for READ)"

# Read response bytes until IDLE
response=""
while true; do
    wait_obf_set
    byte=$(read_port $DATA_PORT)

    status=$(read_port $CMD_PORT)
    state=$((status & 192))

    if [ "$state" -eq 0 ]; then
        # IDLE — this was the final status byte
        echo "Get Device ID response: $response"
        break
    fi

    response="$response $byte"
    # Acknowledge read
    write_port $DATA_PORT 0x68
    wait_ibf_clear
done

echo "Device ID retrieved successfully"

# === Now test SEL: Add SEL Entry ===
# NetFn=Storage(0x0A), Cmd=0x44
# NetFn/LUN = 0x0A << 2 | 0x00 = 0x28
# Data = 16 bytes of SEL record

echo "Adding SEL entry..."
# (Abbreviated — full KCS write sequence with 16 data bytes)
# Record: Type=0x02, Sensor Type=0x01, Sensor#=0x42, Event=0x6F, Data=0x01,0x02,0x03

echo "SEL_TEST_PASS"
```

> **Note**: The actual test script would be more complete. The above shows the
> pattern — the full implementation handles the complete KCS write/read
> sequence for Add SEL Entry and Get SEL Entry commands.

### 3.4 Running the Test

```bash
# Build and run the specific test using the flowey pipeline
cargo xflowey vmm-tests-run \
    --filter "test(ipmi_kcs_sel)" \
    --target windows-x64 \
    --dir /mnt/d/vmm_tests_out/
```

The `--target windows-x64` flag runs the test using the WHP (Windows Hypervisor
Platform) backend on WSL, which is compatible with the Linux Direct boot
firmware configuration.

### 3.5 Unit Tests

In addition to the VMM integration test, add unit tests within the
`ipmi_kcs` crate:

```rust
#[cfg(test)]
mod tests {
    use test_with_tracing::test;

    #[test]
    fn kcs_write_read_roundtrip() {
        // Create device, simulate KCS write sequence for "Get Device ID",
        // verify response bytes match expected format
    }

    #[test]
    fn sel_add_and_get_entry() {
        // Add a SEL entry via IPMI command, read it back,
        // verify record ID assignment and data integrity
    }

    #[test]
    fn sel_clear() {
        // Add entries, clear SEL, verify count=0
    }

    #[test]
    fn kcs_error_recovery() {
        // Send invalid sequence, verify device enters ERROR state,
        // send GET_STATUS/ABORT, verify recovery to IDLE
    }

    #[test]
    fn unknown_command_completion_code() {
        // Send unsupported NetFn/Cmd, verify 0xC1 completion code
    }
}
```

---

## 4. Phase 2 (Future): UEFI Support via SMBIOS & ACPI

Phase 2 extends the IPMI device for UEFI-booted guests (including Alpine).
UEFI firmware and modern OS kernels discover the IPMI interface through
SMBIOS and ACPI tables, not by probing hard-coded I/O ports.

### 4.1 SMBIOS Type 38 — IPMI Device Information

Per SMBIOS Specification v3.x, Type 38 structure:

| Offset | Size | Field | Value |
|--------|------|-------|-------|
| 0x00 | 1 | Type | 38 (0x26) |
| 0x01 | 1 | Length | 16 (minimum) |
| 0x02 | 2 | Handle | Auto-assigned |
| 0x04 | 1 | Interface Type | 0x01 = KCS |
| 0x05 | 1 | IPMI Spec Rev | 0x20 = IPMI 2.0 |
| 0x06 | 1 | I2C Target Addr | 0x20 (BMC default) |
| 0x07 | 1 | NV Storage Device | 0xFF = not present |
| 0x08 | 8 | Base Address | 0x0000000000000CA2 (I/O space, bit 0 = 0) |
| 0x10 | 1 | Base Addr Modifier | Register spacing, LSB address bit |
| 0x11 | 1 | Interrupt Number | 0x00 = none (polled) |

#### Where to Add

OpenVMM's SMBIOS generation lives in the firmware/UEFI path. The SMBIOS
Type 38 entry needs to be added when the IPMI device is configured. Look at
how existing SMBIOS entries (Type 0, 1, 2, 3) are generated and add a
Type 38 builder in the same pattern.

Relevant code area: `vm/devices/firmware/` — specifically the SMBIOS builder
used for UEFI boot configuration.

### 4.2 ACPI SPMI Table (Service Processor Management Interface)

The ACPI SPMI table tells the OS about the IPMI interface:

| Field | Value |
|-------|-------|
| Signature | "SPMI" |
| Interface Type | 0x01 = KCS |
| Spec Revision | 0x0200 |
| Interrupt Type | 0x00 = none (polled) |
| GPE | 0x00 |
| PCI Device Flag | 0x00 = not PCI |
| Base Address | GAS (Generic Address Structure): I/O Space, 0xCA2, byte access |
| Register Spacing | 1 byte |

#### Where to Add

OpenVMM's ACPI table generation needs the SPMI table added. Look at how
existing ACPI tables (DSDT, FADT, MADT, etc.) are built and served to the
guest firmware. The SPMI table is a static ACPI table — it doesn't need
AML bytecode.

Relevant areas:
- `vm/devices/firmware/uefi_specs/` — ACPI table definitions
- The UEFI firmware configuration path that assembles ACPI tables

### 4.3 ACPI DSDT Device Node (Optional but Recommended)

For best OS compatibility, add an IPMI device node in the DSDT:

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

This requires modifying the DSDT generation in OpenVMM's firmware path.

### 4.4 Alpine Test with UEFI Boot

Once SMBIOS + ACPI are in place, the Alpine test becomes straightforward:

```rust
#[openvmm_test(
    openvmm_uefi_x64(vhd(alpine_3_23_x64)),
)]
async fn ipmi_kcs_sel_alpine(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_memory(MemoryConfig {
            startup_bytes: SIZE_1_GB,
            ..Default::default()
        })
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Install ipmitool
    cmd!(sh, "apk add --no-cache ipmitool").run().await?;

    // Load IPMI kernel modules
    cmd!(sh, "modprobe ipmi_devintf").run().await?;
    cmd!(sh, "modprobe ipmi_si type=kcs ports=0xca2")
        .run()
        .await
        .context("Failed to load ipmi_si module")?;

    // Verify IPMI device exists
    cmd!(sh, "test -e /dev/ipmi0")
        .run()
        .await
        .context("/dev/ipmi0 not found after loading ipmi_si")?;

    // Add a SEL entry
    cmd!(sh, "ipmitool sel add 0x01 0x02 0x03")
        .run()
        .await?;

    // Read SEL entries
    let sel_output = cmd!(sh, "ipmitool sel list")
        .read()
        .await?;
    assert!(!sel_output.trim().is_empty(), "SEL should have entries");

    // Clear SEL
    cmd!(sh, "ipmitool sel clear").run().await?;

    // Verify SEL is empty
    let sel_info = cmd!(sh, "ipmitool sel info")
        .read()
        .await?;
    assert!(sel_info.contains("Entries          : 0"), "SEL should be empty after clear");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

> **Note**: Alpine UEFI tests require the UEFI firmware artifact and the
> Alpine VHD image. These are already available as test artifacts
> (`alpine_3_23_x64`). The test should work once the SMBIOS Type 38 and ACPI
> SPMI table are properly generated.

### 4.5 Phase 2 Implementation Order

1. SMBIOS Type 38 generation when IPMI device is configured
2. ACPI SPMI table generation
3. (Optional) DSDT device node for `IPI0001`
4. Alpine UEFI VMM test
5. Verify with `ipmitool` that full SEL lifecycle works end-to-end

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
| `vmm_tests/vmm_tests/tests/tests/ipmi_kcs_test.sh` | Guest-side test script |

### Modified Files (Phase 1)

| File | Change |
|------|--------|
| `Cargo.toml` (root) | Add workspace members + dependencies |
| `openvmm/openvmm_resources/src/lib.rs` | Register `IpmiKcsResolver` |
| `petri/src/vm/openvmm/modify.rs` | Add `with_ipmi_kcs()` helper |
| `vmm_tests/vmm_tests/tests/tests/x86_64.rs` | Add `ipmi_kcs_sel` test |
| `vmm_tests/vmm_tests/Cargo.toml` | Add `ipmi_kcs_resources` dependency |

### Modified Files (Phase 2)

| File | Change |
|------|--------|
| SMBIOS builder (in `vm/devices/firmware/`) | Add Type 38 entry |
| ACPI table builder | Add SPMI table |
| DSDT generation | Add `IPI0001` device node |
| `vmm_tests/vmm_tests/tests/tests/multiarch.rs` | Add Alpine IPMI test |

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
