# Phase-1a software networking and vsock bounce review

## Scope

These changes reuse the existing vsock fallback paths and add an owned
backend-facing packet pool to virtio-net. They are opt-in and do not change
MSHV host-access acquire/release, kernel memory registration, or private-memory
enforcement.

The logical changes are shared completion support, default-off resources,
in-process vsock, software networking, and CLI/documentation.

| Change ID | Logical chunk |
|---|---|
| `uvoxtzxv` | Fallible ordered completion and backend address namespace |
| `tqyswumk` | Default-off resource flags and explicit interim denial |
| `opxpvzmv` | Bounded in-process vsock staging and fresh reset recovery |
| `oymvrkom` | Owned software network packet arenas |
| `otkzxzss` | Opt-in CLI, Guide coverage, and validation record |

## Review

Claude Opus 5.5 performed an independent rubber-duck review of each chunk.
The separate formal agent was cancelled without results; the parent
GPT-6.1 Sol reviewed the source and staged resource bridge directly.

Resolved findings:

- Fresh vsock queue generations can recover after all retired queues and
  connections close. Restoring retired queues is refused. Normal stop/resume
  retains cursors; bounce mode rejects saved queue state.
- Normal connections retain lazy allocation after partial writes instead of
  reserving a full ring on every packet.
- Normal network RX memory failures drop the packet with zero valid bytes.
  Owned-copy failures and invalid backend IDs remain explicit fatal errors.
- Endpoint restart drops old queues and awaits endpoint stop before completing
  or dropping old TX once, releasing scratch, and refreshing RX IDs. Interrupted
  quiescence retains ownership and prevents a new consumer.
- Resolver admission tests are at module scope and actually execute.
- Owned TX snapshots, RX copyback, and segmented RX buffers use checked,
  fallible payload reservation.
- The resource-only intermediate rejects a true bounce flag explicitly; its
  constructors remain false until the CLI chunk lands.

No host kernel or host network configuration change is part of this work.

## Local validation

The final component/CLI run passed 349 tests. One pre-existing ignored test,
`partial_submit_multi_packet`, remains ignored; new tests cover whole-packet
partial acceptance and retained unsent prefixes.

Coverage includes backend memory identity, delayed TX consumption and guest
mutation, bounded pools and backpressure, RX copyback, stale IDs, endpoint
restart, split/packed publication errors, vsock credits, partial writes,
WouldBlock, malformed packets, queue reset, and resource/CLI admission.

Modified-package all-targets clippy and no-dependency rustdoc pass. Formatting
runs last before each commit. The full static MSHV/Consomme/TAP binary builds.

## Existing-kernel SNP acceptance on chris-mshv

With a new explicit one-time authorization, baseline and bounce configurations
booted the existing SNP kernel and used Consomme port forwarding bound only to
host loopback. No TAP interface, host network change, host reboot, or kernel
replacement was made.

Both runs verified 28 framed TCP transfers, totaling 1,853,464 bytes in each
direction. Payload sizes ranged from one byte to 256 KiB, with small fragmented
host sends and a deterministic transformed response checked on the host. The
candidate emitted owned TX and RX path events. Both guests powered off normally;
the host partition set returned to its baseline and no new host kernel errors
were recorded.

The initially reused guest returned errno 97 when creating AF_VSOCK sockets in
both configurations. That was a limitation of the old cached fixture, not the
published upstream SNP guest. The published-guest retest below resolves it.

Full logs, exact commands, framed traffic results, helper source, captured host
logs, and input hashes are retained in the session's `files/net-vsock-hardware/`
directory. Temporary remote test files are isolated under
`/home/chris/nv-bounce-5e10ff8d`.

## Published upstream SNP guest retest, 2026-10-09

At the user's request, fetched the SNP guest artifact from
`microsoft/openvmm-deps` release `0.3.0-155`:
`openvmm-test-linux-snp-guest.x86_64.0.3.0-155.tar.gz`.
Its manifest identifies Linux `6.18.53`. The extracted final configuration
enables `CONFIG_VSOCKETS=y`, `CONFIG_VIRTIO_VSOCKETS=y`, and
`CONFIG_VIRTIO_VSOCKETS_COMMON=y`. The kernel image SHA-256 matches the release
manifest:
`1ac6e1ed682af6fd6cb464bdd135b742a7efaee71587abd4d9e152b992273512`.

Kept the same static test helper/initrd and committed OpenVMM candidate. The
host kernel, old fixture, and host network configuration were unchanged. A
private Unix relay socket created by the root-run VMM needed ownership assigned
to the test client; only that socket inside the private test directory changed.

| Configuration | Transport | Verified frames | Bytes each direction | Exit |
|---|---|---:|---:|---:|
| Baseline | Consomme TCP | 28 | 1,853,464 | 0 |
| Baseline | In-process virtio-vsock | 28 | 1,853,464 | 0 |
| Bounce enabled | Consomme TCP | 28 | 1,853,464 | 0 |
| Bounce enabled | In-process virtio-vsock | 28 | 1,853,464 | 0 |

Both guests confirmed SNP encryption and powered off normally. Their partition
sets returned to the baseline, and neither kernel-log interval contained new
host errors. The bounce run emitted owned-buffer events in all four paths:
network TX/RX and vsock TX/RX. Payloads ranged from one byte to 256 KiB, using
fragmented writes and deterministic verified responses.

**Real SNP vsock hardware acceptance now passes.** The earlier blocked result
is superseded, not counted as a pass. Release archive, final configuration,
manifest, exact commands, input hashes, full logs, and path-event counts are
saved in the session's `files/net-vsock-upstream/` directory. Remote inputs/logs
are isolated under `/home/chris/nv-bounce-upstream-5e10ff8d`.

## Limits

Network mode admits Consomme, Linux TAP, and null endpoints, not hardware DMA,
vhost, or assigned NICs. Each pair reserves 4.25 MiB for 32 RX and 32 TX slots;
packets are capped at 64 KiB and 256 descriptors. Metadata and backends require
additional memory. Live packet data never uses the actual guest backing as the
backend's memory namespace.

In-process vsock uses owned send/receive buffers and credit-aware limits.
Neither bounce mode supports save/restore in this milestone. Kernel host
access may still remain granted after a userspace copy; phase 1b and kernel MM
enforcement remain separate work.
