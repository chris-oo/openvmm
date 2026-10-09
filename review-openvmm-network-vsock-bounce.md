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

The existing guest returns errno 97 when creating AF_VSOCK sockets in both
configurations. **Real SNP vsock acceptance is blocked by that guest artifact.**
Local virtio-vsock queue/Unix-socket integration tests pass, but they are not
presented as a real SNP vsock result. No search for a replacement kernel or
host-kernel change was pursued.

Full logs, exact commands, framed traffic results, helper source, captured host
logs, and input hashes are retained in the session's `files/net-vsock-hardware/`
directory. Temporary remote test files are isolated under
`/home/chris/nv-bounce-5e10ff8d`.

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
