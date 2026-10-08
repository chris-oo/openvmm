# OpenVMM phase-1a block bounce implementation review

## Scope and commit groups

This implements per-device, kernel-independent block bounce I/O. It does not
change MSHV host-access acquisition, kernel RAM registration, SNP ownership,
or private-memory enforcement.

Each group received an independent GPT-6.1 Sol formal review and Claude Opus
5.5 rubber-duck review before commitment.

| Group | Change ID | Change | Final review |
|---|---|---|---|
| 1 | `xqkktvtr` | Guest-memory policy and fallible allocation | No blocking issues |
| 2 | `pulyttxv` | Bounded owned staging at the common Disk boundary | No blocking issues |
| 3 | `qlronuvw` | Exact file transfers, EOF handling and FUA | No blocking issues |
| 4 | `xqqutoot` | Fallible completion, packed progress and GPA overflow | No blocking issues |
| 5 | `rpxxuosk` | Virtio-blk mode, closed resources and producer defaults | No blocking issues within phase 1a |
| 6 | `vyrzllzq` | Opt-in CLI, parser tests and Guide coverage | No blocking issues |

## Findings resolved before commitment

- Extracted and boxed only the copy-only staging helpers. Normal Disk requests
  stay unboxed. Debug/release SCSI and StorVSP code generation passes with the
  unchanged fixed-size StackFuture limits; a 1304-byte regression budget checks
  public disk futures on 64-bit targets.
- Kept the 1 MiB rejection specific to the opt-in mode. A legacy scattered
  2 MiB request still succeeds.
- Advanced packed used state immediately after successful publication, before
  fallible notification reads. Both cursor-wrap cases have regression coverage.
  Completion errors notify the guest and are never retried.
- Retained terminal queue failure through stop/start. An errored device must
  be recreated; this intentional fail-closed behavior also applies to normal
  virtio-blk devices. The bounce milestone does not support save/restore.
- Rejected O_APPEND and O_DIRECT descriptors in the Linux closed-file resolver.
  Tests exercise actual resolution, including generic-resource rejection.
- Matched legacy create/truncate behavior and rejected structured-file suffixes
  without case sensitivity.

Arc locking-hook forwarding corrects previously omitted calls for normal views
as well as restricted ones. Exact file FUA also improves legacy FileDisk paths
on Windows and macOS. These are intentional coupled corrections.

## Validation

Targeted nextest runs cover memory policy, storage, file I/O, queues, device
integration, CLI parsing, and resource resolution. Modified-package all-targets
clippy and no-dependency rustdoc pass. Workspace formatting runs last before
each commit.

The final cumulative run passed 299 component/CLI tests and two production
resolver tests. Memory-policy bitmap tests also passed separately.

Release and debug code generation cover the SCSI/StorVSP future-size dependency;
the related NVMe, IDE, StorVSP, common Disk and scsi-buffer tests pass. Production
resolver tests admit the reviewed file/RAM sources without dynamic registrations.

## Real SNP hardware acceptance

With explicit one-time permission, the unchanged `chris-mshv` host kernel
`6.18.34.mshv3-mshv-snp-x2apic-enabled` ran the current candidate in two
configurations: bounce disabled for the baseline, then bounce enabled.

The static candidate was built with:

```bash
cargo build -p openvmm --no-default-features \
  --features virt_mshv,vendored_crypto \
  --target x86_64-unknown-linux-musl --profile release
```

The host has glibc 2.38; the local GNU binary required glibc 2.39. The existing
guest kernel also needs `--pcie-ecam-below-4gb` to enumerate the data disk. Neither
adaptation changes the host kernel or overwrites known-good guest artifacts.

The serial-driven helper verified SNP encryption, located the unique
`BOUNCE-TEST` virtio-blk disk, checked its 256 MiB capacity and unmounted state,
and used guest O_DIRECT. It ran sequential and seeded-random writes/readback at
512 bytes, 4 KiB, and 64 KiB, with one and eight workers. Workers used disjoint
offsets; each write phase drained and flushed before verification.

| Run | Verified writes | Verified reads | Exit | Partition cleanup |
|---|---:|---:|---:|---|
| Baseline, bounce disabled | 7,680 | 7,680 | 0 | Unchanged |
| File candidate 1 | 7,680 | 7,680 | 0 | Unchanged |
| File candidate 2 | 7,680 | 7,680 | 0 | Unchanged |
| File candidate 3 | 7,680 | 7,680 | 0 | Unchanged |
| RAM candidate | 7,680 | 7,680 | 0 | Unchanged |

Each candidate showed owned-buffer read and write events for descriptors that
were eligible for the former direct path. Component backend probes separately
check that actual submitted buffers do not alias guest memory. Rate-limited
hardware events are sampled evidence, not an all-request pin-accounting trace.

All five guests powered off normally; host kernel-log intervals contained no
new errors, and the partition set returned to its baseline. This is not proof
of allocator return or absence of existing registration pins.

Inputs, SHA-256 manifests, helper source, exact commands, full serial/OpenVMM
logs, workload results, and before/after host logs are retained under the
session's `files/bounce-hardware/` directory. Temporary remote inputs are isolated
under `/home/chris/bounce-phase1-5e10ff8d-musl-ecam32`; no host reboot, kernel
replacement, existing-artifact overwrite, or remote repository write occurred.

## Limits

The initial mode admits only reviewed buffered regular-file and RAM backings.
Primary payload scratch is capped at 64 MiB per selected device, with additional
bounded backend buffers and metadata. Ordinary allocation bookkeeping can still
fail or trigger Linux overcommit OOM; fallible payload reservation is not a
promise to eliminate every process allocation failure.

Only 4 KiB-base-page guests are validated. Other devices and ordinary disks do
not inherit this mode. Scoped host-access release/conflict denial is phase 1b;
kernel MM enforcement and large pages remain separate work.
