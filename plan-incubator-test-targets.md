# Explicit Execution Targets for Petri Tests

## Status and scope

Status: implementation deferred at the user's request on 2026-09-11.
The design passed the custom plan review, but requires user review before
work resumes. No implementation changes.

This plan and its linked note are **local planning material**. Their commit
must retain a `local:` prefix and must not enter the final upstream stack.

Until this work resumes, select the qualified FVP test explicitly using
[Manual FVP Test Selection](note-fvp-manual-test-selection.md).
The proposed target annotations, direct-only default, test renaming, and
metadata-based CI selection are not implemented.

Implementation workspace:
`/home/coo/ai/jolteon/openvmm-cca-initrd-upstream`.
Base: completed FVP integration, change ID `vurkqulm`.
Use jj, not Git. Do not push.

This is a separate follow-up to the CCA v15 incubator work. It does not
reopen FVP firmware qualification, change the published dependencies, or
implement persistent incubators or fast runnable-test enumeration.

## Decisions agreed with the user

1. No target annotation means **direct only**. Ordinary direct tests need
   no extra annotation.
2. An explicit target list replaces the default completely. It does not
   implicitly include direct execution.
3. Existing TCG tests explicitly opt into `qemu_tcg`.
4. The base CCA test explicitly opts into `qemu_cca` and `fvp_cca`.
5. A future FVP-only test can opt into `fvp_cca` alone.
6. Execution eligibility must not depend on `_tcg` or similar name suffixes.

`direct` means execution without an OpenVMM incubator runner. It does not
mean bare metal, a native instruction set, a CPU vendor, or a particular
OpenVMM hypervisor backend. A direct test can run on a nested host.

## Existing behavior and the gap

The following code establishes the current behavior:

| Surface | Current implementation |
|---|---|
| Capabilities | `petri/petri_artifacts_common/src/lib.rs`, `capabilities` |
| Host checks | `petri/src/requirements.rs`, `HostContext` and `TestRequirement` |
| Registration, listing, execution | `petri/src/test.rs`, `RunTest`, `SimpleTest`, `Test::trial`, `test_main` |
| Macro expansion | `vmm_tests/vmm_test_macros/src/lib.rs`, `ArgsWithOverrides`, `make_vmm_test`, `build_requirements` |
| Initial artifact discovery | `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs` |
| Runner wiring | `flowey/flowey_lib_hvlite/src/write_incubator_target_runner.rs` |
| Archived-test execution | `flowey/flowey_lib_hvlite/src/_jobs/consume_and_test_nextest_vmm_tests_archive.rs` |
| TCG CI selection | `flowey/flowey_hvlite/src/pipelines/checkin_gates.rs` |

`requires(cca)` is a positive prerequisite, not an exclusive test allowlist.
Requirements start with `Any` and add predicates with AND. Consequently,
advertising only `cca` does not exclude tests with no capability requirement.
The ordinary UEFI TCG tests currently have no QEMU-versus-FVP restriction.

Current unmet host requirements become libtest-mimic ignored flags. They
are not an execution-target boundary. In particular, forced ignored-test
execution must not become a way around the new target policy.

Both CCA incubators expose KVM. Host vendor, `/dev/kvm`, and the existing
`ExecutionEnvironment::{Baremetal,Nested}` cannot identify the runner.

CI currently selects its AArch64 incubator tests with
`test(aarch64_tcg)`. The suffix is a filter convention, not an enforced
execution requirement.

## Required policy

### Target vocabulary

Introduce one shared `ExecutionTarget` enum in
`petri_artifacts_common::execution_targets`:

| Rust variant | Canonical external spelling |
|---|---|
| `Direct` | `direct` |
| `QemuTcg` | `qemu_tcg` |
| `QemuCca` | `qemu_cca` |
| `FvpCca` | `fvp_cca` |

Use exact, case-sensitive spellings. Reject empty values, aliases, and
unknown values with an explicit error. Do not provide an `all` wildcard.
Adding another backend must not opt existing tests into it.

Keep the Rust target triple and `cargo xflowey --target` unchanged. They
select compilation architecture/OS, not this execution policy.

Add a compact, nonempty `ExecutionTargets` set in the same module.
Its default is `{Direct}`. Provide typed construction from a single target,
union, and membership checking; do not expose an empty-set constructor.
String parsing and display share the enum's canonical mapping.
No new external dependency is needed.

### Registration metadata

Execution targets are test metadata, not named hardware capabilities.
Do not add `fvp` or `qemu` to `PETRI_CAPABILITIES`.

Add an `execution_targets()` accessor to `RunTest` with a default returning
direct-only. Forward it through `DynRunTest`. Store the set in `SimpleTest`,
defaulting to direct-only, with a fluent setter. Re-export the shared types
from Petri for generated code and custom test registrations.

This covers tests registered without the VMM macros as well as all VMM
macro entry points. Do not put target policy inside an optional
`TestCaseRequirements`: a missing requirements object must not mean
unrestricted target eligibility.

Existing vendor, isolation, device, and capability predicates remain
independent and continue to determine default runnable/ignored status.
This change hardens target eligibility only. It does not globally change
the existing override behavior of capability-derived ignored flags.

### Macro syntax

Add `targets(...)` to `vmm_test_with`, using its existing attributes-before-
`configs(...)` grammar:

```rust
#[vmm_test_with(
    openvmm,
    targets(qemu_cca, fvp_cca),
    requires(cca),
    configs(linux_direct_aarch64)
)]
```

The parser stores an optional explicit set. Only absence produces the
direct-only default. Emit compile errors for `targets()`, duplicate names,
unknown names, or a second `targets(...)` clause.

An explicit set applies to every config expanded by that attribute.
Per-config target overrides are out of scope. If one function needs
different sets, use separate `vmm_test_with` attributes with disjoint
config lists. Existing ignore/unstable overrides retain their behavior.

The plain `vmm_test`, `openvmm_test`, and `openvmm_test_no_agent` entry points
keep their existing syntax and produce direct-only metadata. Use
`vmm_test_with(openvmm, ...)` when an explicit set is needed; do not invent
an `openvmm_test_with` macro or require normal direct tests to migrate.

Attach the set to every generated `SimpleTest`. Preserve generated VMM,
guest-architecture, firmware, and guest-image name prefixes.

## Execution identity and enforcement

### Runner-owned identity

Use `PETRI_EXECUTION_TARGET` for the actual execution target. Absence means
direct. Parse it once into an immutable typed value at test-binary entry.
An empty, invalid-UTF-8, or unknown value is an error, not a fallback.

QEMU and FVP derive this value from their parsed backend enum, never from
profile filenames, test names, CPU properties, or user-supplied guest env.
Set it for the guest command after normal backend readiness checks.
FVP must retain tuple validation, boot, and pipette readiness before
publishing its existing runtime capabilities.

Treat both `PETRI_EXECUTION_TARGET` and the discovery variable below as
reserved in both incubator backends. Reject guest-env/forwarding attempts
to override them. Do not pass a discovery declaration into an actual guest.
Direct Flowey execution explicitly selects direct rather than inheriting
a stale execution-target declaration.

Flowey also sets the reserved `PETRI_EXPECTED_EXECUTION_TARGET` to its
selected target for actual execution. The harness rejects a mismatch
between this expectation and actual identity. The incubator checks any
expectation against its parsed profile before boot, then supplies matching
actual/expected identity to the guest. This catches accidental redirection
of a direct workflow through a configured OpenVMM incubator, and a missing
incubator runner that would otherwise execute as direct.
An absent expectation remains valid for ordinary hand-run direct tests.

This is a trusted-runner configuration contract, not attestation against
an operator who can modify binaries or fabricate their environment.

### Listing and execution

Normal and ignored listing must expose only target-eligible tests.
Within that set, preserve current host requirement and explicit-ignore
classification. Continue to query `HostContext` inside the real incubator.

Execution must check the same immutable target against the test's set
before resolving runtime artifacts or entering test code. Place this
check outside the unstable-test wrapper: neither `--ignored`,
`--include-ignored`, nor `PETRI_IGNORE_UNSTABLE_FAILURES` may turn a target
violation into a passing test.

Filter target-ineligible trials out of broad invocations. For an explicit
`--exact` request naming a known incompatible test, return an error naming
the test, actual target, and allowed targets. Honor exact `--skip` exclusions
before producing that error. Unknown test names retain existing behavior.
Add a focused name-selection helper rather than changing libtest-mimic.
Test it against the pinned library's exact/substring/skip semantics.

Retain the target guard in the trial callback as defense against a future
selection-path mistake. A target rejection must occur before the unstable
failure conversion and before actual artifact resolution.

## Static artifact discovery

Runnable enumeration and static artifact discovery remain distinct.
The latter must not pretend that the outer host has FVP/KVM/CCA properties.

Introduce `PETRI_DISCOVERY_TARGET` as a **discovery-only** declaration.
It is valid only with `--list` or `--list-required-artifacts`; reject it
for execution before any early return, artifact work, or host querying.
Reject simultaneous actual-target and discovery-target declarations.
Reject ambiguous combinations of protocol, artifact, and libtest listing
modes.
Static discovery clears both actual and expected execution identity before
setting its declaration. It does not run the actual/expected execution
comparison. An incubator must not accept a discovery declaration as an
execution expectation.

In discovery-list mode:

- Filter by the declared target using static metadata.
- Do not construct `HostContext` or evaluate hardware/capability predicates.
- Retain explicit ignore flags; Flowey requests ignored candidates too
  for incubator discovery and build-only mode.
- Emit the unchanged libtest-mimic listing format for nextest to parse.

For `--list-required-artifacts`, a discovery target restricts the eligible
set before applying exact stdin names. Reject requested names that are
unknown or ineligible; do not silently return a partial artifact set.
Treat stdin read/UTF-8 errors as errors instead of truncating the request.
Keep the existing `ArtifactListOutput { required, optional }` JSON shape.
Without a discovery target, preserve legacy all-target artifact aggregation.
An artifact query still never runs a test or advertises verified capability.

Flowey derives the declared target from the selected typed incubator profile.
For incubator or build-only discovery, set the declaration only on the
initial nextest-list subprocess and exact artifact-query subprocesses;
remove any inherited actual-target declaration there.

Direct, non-build-only discovery keeps current host-sensitive enumeration,
now restricted to direct-eligible tests. Direct build-only discovery uses
the static direct target and includes ignored candidates.

Drop empty suites after target/name filtering. If no eligible tests remain
for a requested workflow, fail before dependency downloads or model launch
with a message naming the selected target and filter.

Continue using native execution or the existing user-mode list runner for
static discovery. Support the installed `qemu-aarch64` /
`qemu-aarch64-static` names without treating them as QEMU TCG incubators.
An actual incubator runner must reject discovery declarations before boot,
so a stale `CARGO_TARGET_*_RUNNER` cannot silently turn discovery into a
model launch. Preserve command argument/path handling.
Do not select an inherited `CARGO_TARGET_*_RUNNER` as the discovery
transport. Resolve the supported native/user-mode transport explicitly.

Do not add a nextest `target(...)` predicate: nextest does not know this
metadata. The test harness supplies an eligible list, and nextest continues
to apply the user's existing filter expression.

## Version skew and archive consumers

Environment variables alone are not enough: an old binary can ignore them.
Introduce a side-effect-free `--petri-target-protocol-version` query in both
the Petri test harness and the incubator CLI. Its stdout is exactly one
JSON document, `{"version":1}`, followed by a newline. It must not require a
profile, query host properties, resolve artifacts, or boot a VM.
Reject combined protocol-query/execution arguments. Clear discovery env
when performing the query.

Flowey checks selected test binaries after obtaining their paths from the
initial nextest list and before artifact queries/dependency resolution.
Run the query through the same native/user-mode transport as discovery.
Reject absent, malformed, unsupported, or future protocol responses.

Add `--petri-test-runner` / `INCUBATOR_PETRI_TEST_RUNNER` to the incubator
CLI/configuration. Flowey enables it whenever the incubator is a VMM-test
target runner, for local and archived-test workflows.
Before accepting the runner binary, Flowey checks its protocol query too.

In that mode the incubator checks the invoked guest test binary's protocol
before forwarding listing or test execution. Capture probe stdout privately.
Use the already booted, validated host and existing pipette connection:
do not add a second boot. The probe shares the command phase's existing
deadline; do not reset the budget. Reject an old binary before executing
its test body and perform normal owned cleanup.

Generic standalone incubator commands remain supported without this flag.
They still get truthful runner identity, but commands such as
`openvmm --help` are not required to implement the Petri protocol.

The archive producer, consumer, runner, and tests should remain
source-matched. New incubator workflows reject old runner/test combinations.
Unmodified old runners cannot enforce a policy they do not understand;
mixing new archives with old orchestration is unsupported. Rollback must
rebuild runner and archive together. Updated Flowey entry points reject old
test archives in direct mode too. Running old binaries with old tooling
remains legacy behavior, not coverage of the new policy.

### Archive preflight and graph ordering

Add `flowey_lib_hvlite::preflight_vmm_test_archive`. Its inputs are the
archive, selected profile/target, nextest filter and configuration, and
the native/user-mode discovery transport. It extracts/lists with the
existing nextest archive facilities, checks test-binary protocol versions,
applies static target/filter selection, and returns a
`ReadVar<ValidatedTestSelection>` plus a successful-preflight side effect.
The result identifies the target, filter, and selected suites/names.
Discard the preflight extraction only after its probes/queries finish.

In `consume_and_test_nextest_vmm_tests_archive`, this is a real prerequisite
of consumer-side VM dependency downloads, dependency installation, and prep
steps. Thread its side effect into the actual operation steps in the
affected dependency nodes, adding optional prerequisite inputs where needed.
They must claim the barrier before performing their work. Adding the barrier
only to final nextest `pre_run_deps`, or merely declaring the preflight first,
does not satisfy this requirement.

Acquiring the archive, runner/profile, nextest, and discovery transport
needed for preflight is allowed before the barrier. Independently shared
CI build jobs that produce these artifacts are not consumer downloads and
are not retroactively gated by one consumer's selection.

Use the same selection/protocol helpers for local construction-time
discovery and archive preflight. The latter cannot rely on the former
having run. Final archive execution still goes through actual-backend
nextest enumeration; a validated static selection is not a runnable list.
Test the graph with instrumented operations: failed/empty preflight must
leave consumer VM-download, install, prep, and model-launch counters at zero.

### Explicit subprocess environment policy

Extend `flowey_lib_common::gen_cargo_nextest_run_cmd::Script` with an
explicit environment-removal set, threaded from the run requests. Preserve
empty-default behavior for unrelated workflows. Apply removals with
`Command::env_remove` in `flowey_lib_common::run_cargo_nextest_run` before
applying the owned environment map. Removing a key from `BTreeMap` alone
does not remove an inherited variable.

Portable Bash/PowerShell command renderers must emit matching unsets before
assignments. Update WSLENV forwarding so removed policy/runner keys are not
reintroduced by Windows interop.

For VMM workflows, enumerate and remove inherited `CARGO_TARGET_*_RUNNER`
environment keys at subprocess launch. Then install only the selected
discovery transport or actual incubator runner. Do this on native AArch64
too, before `cross_target_list_runner`'s native fast path.
Do not infer execution identity from a runner executable name.

Apply these policies at every local/archive boundary:

| Operation | Remove inherited values | Set owned values |
|---|---|---|
| Static discovery/list/artifact query | Actual target, expected target, discovery target, Cargo runner env keys | Discovery target and selected user-mode runner if needed |
| Protocol probe | All three target variables and Cargo runner env keys | Only its explicit transport, if any |
| Direct execution | All three target variables and inherited Cargo runner env keys | Actual target `direct`, expected target `direct` |
| Incubator nextest execution | All three target variables and inherited Cargo runner env keys | Expected target from profile, selected incubator runner and Petri-runner mode |
| Guest test/probe dispatch | Discovery declaration and user overrides of reserved keys | Matching actual/expected target from the validated runner |

Retain supported non-incubator platform transports, including the checked-in
macOS signing runner and Windows interop. A configured OpenVMM incubator that
receives an incompatible expected target must reject it before boot.
Arbitrary external wrappers that falsify or strip the context are outside
the trusted-runner contract; do not claim attestation against them.

### Empty selection versus no tests run

Treat these as different failures:

1. **No static target/filter candidates.** Local discovery or archive
   preflight fails before consumer VM dependencies and model launch.
2. **Static candidates exist, but none run on the actual backend.**
   Preserve real host checks. Explicitly pass `--no-tests=fail` to VMM
   nextest runs, rather than relying on configuration/environment defaults.
   The installed nextest documents this as an error with exit code 4.
   Surface that result with target/filter diagnostics; do not convert it to
   a successful result or an ordinary ignored/unstable test success.

Thread a VMM-specific `require_executed_tests` setting through the shared
nextest command/result nodes, defaulting off for unrelated callers. Enable
it for local and archive VMM execution. Emit the explicit CLI option and
treat nextest's no-tests exit as a configuration failure in that mode,
including when ordinary test failures are configured as nonfatal.

Test target-empty and actual ignored-only/unmet-host-requirement fixtures
separately using the pinned nextest. Explicit execution of an eligible
ignored test must still be able to pass by genuinely running its body.
Retain the CCA inner one-executed-test oracle; an outer successful command
alone is not evidence that a Realm test body ran.

## Exact migration

The current incubator subset is five Rust functions producing six QEMU TCG
tests, plus one shared CCA test. Do not opt additional ordinary tests into
incubators merely because they have historically been callable there.

| Current function | New function | Explicit targets |
|---|---|---|
| `boot_no_vmbus_pcie_aarch64_tcg` | `boot_no_vmbus_pcie` | `qemu_tcg` |
| `boot_no_vmbus_pcie_smmu_accel_aarch64_tcg` | `boot_no_vmbus_pcie_smmu_accel` | `qemu_tcg` |
| `assigned_device_peer_to_peer_dma_aarch64_tcg` | `assigned_device_peer_to_peer_dma` | `qemu_tcg` |
| `assigned_device_smmu_accel_fault_aarch64_tcg` | `assigned_device_smmu_accel_fault` | `qemu_tcg` |
| `boot_no_hv_uefi_aarch64_tcg` (Alpine and Ubuntu configs) | `boot_no_hv_uefi` | `qemu_tcg` |
| `boot_linux_direct_cca` | unchanged | `qemu_cca`, `fvp_cca` |

Keep all existing `requires(...)` clauses. Convert the two plain OpenVMM
UEFI attributes to `vmm_test_with(openvmm, targets(qemu_tcg), configs(...))`.
Keep `_cca` in the CCA name: it describes the behavior, not a routing tag.
Do not add `direct` to any of these migrated cases without a separate
explicit decision. The user selected incubator-only behavior for them.

Audit all references to the old full names: documentation, Rust links,
nextest configuration, CI exclusions, and scripts. Rename only test-routing
suffixes, not useful architecture names, profile names, or CI job labels.
A renamed test has a new test-history identity; do not retain duplicate
executable aliases that would run a test twice.

Change the AArch64 TCG CI job's `test(aarch64_tcg)` selector to `all()`,
retaining its other check-in exclusions and selected QEMU profile.
Petri target metadata now limits the runnable set to the six opted-in tests.
Do not replace the suffix with another test-name allowlist.

Wire both local and archive-consuming Flowey paths. Audit direct paths so
they cannot inherit stale target/discovery declarations.
Regenerate `.github/workflows/` and `ci-flowey/` with `cargo xflowey regen`;
never edit generated YAML manually.

The focused CCA command retains
`--filter 'binary(=tests) & test(boot_linux_direct_cca)'`.
That filter reduces unrelated enumeration boots; it is not an eligibility
or safety boundary. Automatic empty-binary boot optimization is out of scope.

## Implementation sequence and rollback

Use new jj changes above `vurkqulm`; do not rewrite the completed FVP stack.

1. **Additive metadata substrate.** Add the shared types, registration
   fields, macro parsing/expansion, and private protocol parsing/serialization
   helpers. Leave current scheduling active in this change. Do not expose
   a successful protocol-v1 query yet: this code cannot enforce v1.
   Existing tests and CI must still compile and run.
2. **Activate the policy atomically.** Land Petri listing/execution guards,
   discovery mode, runner identity/protocol checks, Flowey wiring, and the
   explicit annotations for the seven current test instances together.
   Publish protocol v1 in runner and harness only in this change, after their
   enforcement obligations are active.
   Keep the existing test names and CI suffix filter temporarily, so this
   intermediate change remains runnable.
3. **Remove name-based selection.** Rename the five TCG functions, update
   references, switch CI to metadata-driven eligibility, regenerate YAML,
   and finish the Guide changes.

Do not enable direct-only enforcement in a commit that has neither the
runner identity wiring nor the existing incubator annotations.
Do not remove the suffix selector before the metadata policy is active.

Rollback proceeds in reverse order. Revert the rename/CI change first,
then the policy activation, then the additive substrate if required.
Rebuild runner and test archive together. Do not work around a version
mismatch by treating unknown metadata as unrestricted.
Activated consumers must reject phase-1 artifacts. During rollback, removal
of enforcement also removes successful v1 advertisement; do not leave an
unenforcing binary that reports v1.

## Validation and acceptance criteria

Use existing testing tools. Unit/parser tests need no VMs. VMM executions
must use `cargo xflowey vmm-tests-run`.

### Unit and process tests

- Omitted annotation is direct-only across all macro entry points and custom
  `RunTest`/`SimpleTest` registrations.
- Explicit lists have exactly their declared members, with no implicit direct.
- Empty, duplicate, repeated-clause, and unknown target syntax is rejected.
  Use `syn` parser/expansion tests in the macro crate; no new test framework
  dependency is required. Test real generated code through harness fixtures.
- Every expanded config receives the same explicit list.
- Test the full four-target membership matrix, including a synthetic
  FVP-only case. Do not add a real FVP-only feature just for this migration.
- Unknown/empty/non-UTF-8 execution and discovery declarations fail cleanly.
- Expected/actual target mismatch fails, including accidental direct-run
  redirection through an OpenVMM incubator and missing runner wiring.
- Capability availability cannot override target exclusion.
- Normal and ignored lists contain only eligible tests. Explicit ignores
  remain ignored and can run when explicitly requested on an eligible target.
- Incompatible exact execution fails before artifact resolution or test-body
  side effects, including forced ignored and unstable-failure suppression.
  Exact skip exclusions and unknown-name behavior remain correct.
- Discovery includes host-incompatible but target-eligible candidates and
  performs no host query. A fixture must detect accidental host probing.
- Discovery declarations cannot execute tests or cross into an actual
  incubator launch. Reject conflicting declarations and malformed modes.
- Contaminated parent environments are removed at actual subprocess spawn,
  not just from generated maps. Cover native AArch64, cross execution,
  local/archive paths, portable scripts, and Windows/WSL forwarding.
- Exact artifact queries reject unknown/ineligible names and input errors;
  required/optional artifact union and the existing JSON schema are preserved.
- Protocol probes are side-effect-free and silent except for their JSON.
  Missing/old/future binaries and runners fail closed in incubator workflows.
- Both runner families set identity after readiness, reject guest overrides,
  and keep protocol-probe output out of nextest stdout.
- Test zero-eligible selection before dependency/model work, complex existing
  nextest filters, binary exclusions, and local/archive execution paths.

### VMM and workflow tests

1. Direct listing and a representative ordinary direct test work without
   target annotations. Compare inventory changes: only the seven deliberately
   migrated incubator instances disappear from direct eligibility.
2. The QEMU TCG profile lists and runs the six renamed tests with a broad
   filter. The shared CCA test is not eligible there.
3. QEMU CCA lists and runs the unchanged base CCA test, not ordinary TCG tests.
4. FVP with a broad filter exposes only the base CCA test. Run it end to end
   through the existing licensed backend, preserving actual-backend
   enumeration, readiness, Realm/L1 teardown, and owned-resource cleanup.
5. Attempt a known direct-only or QEMU-only exact test on FVP, including an
   ignored-test override. It must not execute the test body or report a
   one-test pass. A workflow selecting no eligible tests must fail clearly.
6. Exercise a fake/synthetic FVP-only test against all four target contexts.
7. `--build-only --incubator ...` selects the appropriate static artifacts
   without booting the incubator. Direct build-only selects direct metadata.
8. Test the source-matched archive path used by CI, plus deliberate old-runner
   and old-test-binary protocol rejection.

Preserve the one-executed-test oracle for CCA. Enumeration-only, zero-test,
or ignored-only success does not count as a Realm pass.
SNP hardware coverage remains the previously recorded follow-up; this work
does not change its capability rules or claim that deferred testing passed.

### Checks before each implementation commit

Identify all modified packages. At minimum these are expected:
`petri_artifacts_common`, `petri`, `vmm_test_macros`, `vmm_tests`,
`incubator`, `flowey_lib_hvlite`, and `flowey_hvlite`.
The explicit command-environment and zero-test handling changes also modify
`flowey_lib_common`.

Run package-scoped check, clippy, docs, and unit tests. Use
`cargo nextest run --profile agent` for non-VMM packages; use xflowey for
`vmm_tests`. Run `cargo xtask fmt --fix` last and resolve its failures.
Do not push. Keep any unrelated pre-existing warning separate from new ones.

Obtain the established standard, adversarial, and different-model-family
rubber-duck reviews before committing the implementation. Review target
bypass paths, default changes, static versus runnable discovery, protocol
skew, and accidental CI coverage loss explicitly.

## Documentation

Update:

- `Guide/src/dev_guide/tests/vmm.md`: direct-only default, target annotations,
  target versus capability semantics, discovery distinction, and migration.
- `Guide/src/dev_guide/tests/vmm/qemu_cca.md`: the CCA target pair and why broad
  FVP selection no longer exposes unrelated tests.
- VMM macro rustdoc: exact syntax/default/error rules and examples.
- Existing references to renamed tests and suffix-based CI selection.

Use only implemented CLI syntax. Do not suggest that nextest has a custom
target predicate. Update the guide-maintenance mapping if new documented
surfaces need entries. No new Guide page is required for this feature.

## Review

**Final verdict: Approved.**

The initial review returned Minor revisions. It confirmed the
default/explicit-list policy, six-TCG-plus-
one-CCA migration, guards outside unstable-failure handling, and reverse
rollback. Four required corrections are incorporated:

1. Protocol v1 is published only with active enforcement, not in the additive
   substrate change. Skew and rollback tests cover this boundary.
2. Archive consumers have their own static preflight and a real dependency
   barrier before VM dependency work.
3. Environment removal occurs at subprocess launch and in portable scripts,
   including native AArch64 and Windows/WSL. Expected/actual target checks
   catch unintended OpenVMM-incubator redirection.
4. Static-empty selection and actual zero-runnable outcomes are separate
   failures, with explicit nextest no-tests handling and distinct fixtures.

The bounded confirmation review approved the corrected plan with no remaining
corrections. It confirmed protocol activation, archive dependency ordering,
subprocess environment removal, distinct zero-test failures, expected/actual
target checks, and the implementation/rollback sequence.
