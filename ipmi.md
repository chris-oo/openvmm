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

#### Recommended Approach: Static Rust Test Binary (Option B)

For Phase 1 (Linux Direct, no ACPI), the most practical approach is:

Build a **static Rust test binary** (`x86_64-unknown-linux-musl`) that exercises
the KCS protocol via direct I/O port access. This binary:
1. Calls `iopl(3)` to enable I/O port access from userspace
2. Uses inline assembly or libc `inb()`/`outb()` for port I/O
3. Implements the KCS write/read protocol
4. Sends "Get Device ID", "Add SEL Entry", "Get SEL Entry" commands
5. Validates responses and prints JSON results to stdout

This approach is preferred over shell scripting via `/dev/port` because:
- **Performance**: Direct port I/O is instant; shell `dd`/`od` in a polling
  loop forks ~3 processes per iteration, risking timeouts
- **Reliability**: No dependency on `/dev/port` (`CONFIG_DEVPORT`) being
  compiled into the test kernel
- **Correctness**: Proper byte-level control without shell quoting issues
- **Debuggability**: Rust binary can provide structured error output

The binary lives as a crate in `vmm_tests/` and is cross-compiled to
`x86_64-unknown-linux-musl`. Pipette uploads and executes it in the guest.

**Alternative approaches considered but rejected:**

- **Option A — Shell script via `/dev/port`**: Too fragile — each I/O port
  read/write forks multiple processes, making the KCS polling loop extremely
  slow. `/dev/port` availability is also not guaranteed.
- **Option C — Custom kernel with IPMI modules**: Too heavy — requires
  changes to kernel build infrastructure. Better suited for Phase 2 Alpine
  tests.

### 3.2 Recommended Test Strategy

Use the **static Rust binary** approach for Phase 1. The binary is built as a
standalone crate (`ipmi_kcs_test_bin`) that cross-compiles to
`x86_64-unknown-linux-musl` and is uploaded to the guest via pipette's
`write_file()` method.

The binary implements:
1. `iopl(3)` syscall to enable port I/O
2. KCS write/read protocol via `inb`/`outb`
3. "Get Device ID" command — validates basic KCS functionality
4. "Add SEL Entry" with a known 16-byte record
5. "Get SEL Entry" to read it back and compare
6. "Get SEL Info" to verify entry count
7. Prints `SEL_TEST_PASS` on success, or a detailed error on failure

### 3.3 Test Implementation

Add the test to `vmm_tests/vmm_tests/tests/tests/x86_64.rs`:

```rust
/// Test that the IPMI KCS device correctly handles SEL operations.
/// Uploads a static test binary that exercises the KCS interface via
/// direct I/O port access (iopl + inb/outb), adds a SEL entry, reads
/// it back, and validates the data.
#[openvmm_test(linux_direct_x64)]
async fn ipmi_kcs_sel(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .modify_backend(|b| b.with_ipmi_kcs())
        .run()
        .await?;

    let sh = agent.unix_shell();

    // Upload the pre-built static test binary to the guest.
    // The binary is built as x86_64-unknown-linux-musl and included
    // as a test artifact.
    let test_bin = std::io::Cursor::new(
        petri_artifacts_vmm_test::IPMI_KCS_TEST_BIN.read().await?
    );
    agent.write_file("/tmp/ipmi_kcs_test", test_bin).await?;
    cmd!(sh, "chmod +x /tmp/ipmi_kcs_test").run().await?;

    // Run the test binary — it exercises:
    // 1. Get Device ID (verifies KCS state machine)
    // 2. Add SEL Entry (writes a known 16-byte record)
    // 3. Get SEL Entry (reads it back and compares)
    // 4. Get SEL Info (verifies entry count = 1)
    let output = cmd!(sh, "/tmp/ipmi_kcs_test")
        .read()
        .await
        .context("IPMI KCS test binary failed")?;

    assert!(
        output.contains("SEL_TEST_PASS"),
        "IPMI KCS SEL test did not pass: {output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

> **Note on `write_file`**: The pipette `write_file()` method takes
> `impl AsyncRead`, not `&[u8]`. Wrap byte slices in `std::io::Cursor`
> before passing to `write_file()`.

#### Test Binary Crate (`vmm_tests/ipmi_kcs_test_bin/`)

The test binary is a small standalone Rust program:

```
vmm_tests/ipmi_kcs_test_bin/
├── Cargo.toml
└── src/
    └── main.rs
```

**`Cargo.toml`:**
```toml
[package]
name = "ipmi_kcs_test_bin"
edition.workspace = true
rust-version.workspace = true

[dependencies]
# Minimal deps for a static musl binary
libc.workspace = true

[lints]
workspace = true
```

**`src/main.rs`** (sketch):
```rust
//! IPMI KCS test binary for Linux Direct VMM tests.
//! Exercises the KCS interface via direct I/O port access.

use std::process::ExitCode;

const KCS_DATA_REG: u16 = 0xCA2;
const KCS_STATUS_CMD_REG: u16 = 0xCA3;

// KCS commands
const WRITE_START: u8 = 0x61;
const WRITE_END: u8 = 0x62;
const READ_ACK: u8 = 0x68;

// Status register bits
const IBF: u8 = 0x02;
const OBF: u8 = 0x01;
const STATE_MASK: u8 = 0xC0;
const STATE_READ: u8 = 0x40;

unsafe fn outb(port: u16, val: u8) {
    std::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack));
}

unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    std::arch::asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack));
    val
}

fn wait_ibf_clear() -> Result<(), &'static str> {
    for _ in 0..100_000 {
        if unsafe { inb(KCS_STATUS_CMD_REG) } & IBF == 0 {
            return Ok(());
        }
    }
    Err("IBF timeout")
}

fn wait_obf_set() -> Result<(), &'static str> {
    for _ in 0..100_000 {
        if unsafe { inb(KCS_STATUS_CMD_REG) } & OBF != 0 {
            return Ok(());
        }
    }
    Err("OBF timeout")
}

/// Send an IPMI command via KCS and return the response bytes.
fn kcs_transfer(request: &[u8]) -> Result<Vec<u8>, &'static str> {
    if request.is_empty() {
        return Err("empty request");
    }

    // WRITE_START
    wait_ibf_clear()?;
    unsafe { outb(KCS_STATUS_CMD_REG, WRITE_START) };

    // Write all bytes except the last
    for &byte in &request[..request.len() - 1] {
        wait_ibf_clear()?;
        wait_obf_set()?;
        unsafe { inb(KCS_DATA_REG) }; // dummy read to clear OBF
        unsafe { outb(KCS_DATA_REG, byte) };
    }

    // WRITE_END + last byte
    wait_ibf_clear()?;
    unsafe { outb(KCS_STATUS_CMD_REG, WRITE_END) };
    wait_ibf_clear()?;
    wait_obf_set()?;
    unsafe { inb(KCS_DATA_REG) }; // dummy read
    unsafe { outb(KCS_DATA_REG, *request.last().unwrap()) };

    // READ phase
    let mut response = Vec::new();
    loop {
        wait_ibf_clear()?;
        wait_obf_set()?;

        let status = unsafe { inb(KCS_STATUS_CMD_REG) };
        let byte = unsafe { inb(KCS_DATA_REG) };

        if status & STATE_MASK != STATE_READ {
            // IDLE — done (last byte is status, discard)
            break;
        }

        response.push(byte);
        unsafe { outb(KCS_DATA_REG, READ_ACK) };
    }

    Ok(response)
}

fn main() -> ExitCode {
    // Enable I/O port access
    if unsafe { libc::iopl(3) } != 0 {
        eprintln!("FAIL: iopl(3) failed — not running as root?");
        return ExitCode::FAILURE;
    }

    // 1. Get Device ID (NetFn=App 0x06, Cmd=0x01)
    //    NetFn/LUN = 0x06 << 2 = 0x18
    let resp = match kcs_transfer(&[0x18, 0x01]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("FAIL: Get Device ID: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Response: [NetFn/LUN, Cmd, CompletionCode, ...]
    if resp.len() < 3 || resp[2] != 0x00 {
        eprintln!("FAIL: Get Device ID bad response: {resp:?}");
        return ExitCode::FAILURE;
    }
    println!("Get Device ID: OK");

    // 2. Add SEL Entry (NetFn=Storage 0x0A, Cmd=0x44)
    //    NetFn/LUN = 0x0A << 2 = 0x28
    //    Data = 16-byte SEL record
    let sel_record: [u8; 16] = [
        0x00, 0x00, // Record ID (ignored by BMC, assigned on add)
        0x02,       // Record Type = System Event
        0x00, 0x00, 0x00, 0x00, // Timestamp (BMC fills in)
        0x20, 0x00, // Generator ID
        0x04,       // EvM Rev
        0x01,       // Sensor Type = Temperature
        0x42,       // Sensor Number
        0x6F,       // Event Dir / Event Type
        0x01, 0x02, 0x03, // Event Data 1-3
    ];
    let mut add_req = vec![0x28, 0x44];
    add_req.extend_from_slice(&sel_record);
    let resp = match kcs_transfer(&add_req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("FAIL: Add SEL Entry: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Response: [NetFn/LUN, Cmd, CC, RecordID_lo, RecordID_hi]
    if resp.len() < 5 || resp[2] != 0x00 {
        eprintln!("FAIL: Add SEL Entry bad response: {resp:?}");
        return ExitCode::FAILURE;
    }
    let record_id = u16::from_le_bytes([resp[3], resp[4]]);
    println!("Add SEL Entry: OK (Record ID = {record_id:#06x})");

    // 3. Get SEL Entry (NetFn=Storage 0x0A, Cmd=0x43)
    //    Data = [ResvID_lo, ResvID_hi, RecordID_lo, RecordID_hi, Offset, BytesToRead]
    let get_req = vec![
        0x28, 0x43,
        0x00, 0x00,         // Reservation ID (0 = no reservation)
        resp[3], resp[4],   // Record ID from Add response
        0x00,               // Offset into record
        0xFF,               // Read entire record
    ];
    let resp = match kcs_transfer(&get_req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("FAIL: Get SEL Entry: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Response: [NetFn/LUN, Cmd, CC, NextRecID_lo, NextRecID_hi, Record...]
    if resp.len() < 5 + 16 || resp[2] != 0x00 {
        eprintln!("FAIL: Get SEL Entry bad response: {resp:?}");
        return ExitCode::FAILURE;
    }
    let record_data = &resp[5..5 + 16];
    // Verify key fields match what we wrote
    if record_data[2] != 0x02 || record_data[10] != 0x42 || record_data[12] != 0x6F {
        eprintln!("FAIL: SEL record mismatch: {record_data:?}");
        return ExitCode::FAILURE;
    }
    println!("Get SEL Entry: OK (data verified)");

    println!("SEL_TEST_PASS");
    ExitCode::SUCCESS
}
```

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
