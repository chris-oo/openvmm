# Manual FVP Test Selection

## Local status

The user deferred the
[explicit execution-target plan](plan-incubator-test-targets.md) on
2026-09-11. This note and the plan require user review before implementation
resumes. Keep their `local:` planning commit out of the final upstream stack.

FVP support itself is implemented in change ID `vurkqulm`. Target metadata
is not: `targets(...)` and the proposed direct-only default are future work.
For now, use an explicit test filter rather than treating the FVP profile
or its `cca` capability as a test allowlist.

## Run the qualified base CCA test

On the currently provisioned machine:

```bash
cd /home/coo/ai/jolteon/openvmm-cca-initrd-upstream
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca.toml \
  --fvp-platform-root /home/coo/ai/jolteon/openvmm/target/cca-test \
  --shrinkwrap-package-root /home/coo/.shrinkwrap/package \
  --filter 'binary(=tests) & test(=aarch64_exclusive::openvmm_linux_aarch64_boot_linux_direct_cca)'
```

On another machine, replace the two platform/package roots with its
provisioned, supported inputs. Use the existing xflowey workflow, not a
prototype adapter or a standalone test-binary invocation.

`binary(=tests)` selects the executable containing the shared Realm test.
The exact `test(=...)` predicate selects only the currently qualified case;
it does not automatically include future tests with a similar name.
This avoids booting FVP to enumerate the unrelated `cca` and `tmks`
executables. The selected executable's normal and ignored enumeration still
runs inside the validated FVP backend.

Require one executed, passing
`aarch64_exclusive::openvmm_linux_aarch64_boot_linux_direct_cca` test.
Other filtered-out tests are expected. Listing-only, zero-test, or
ignored-only output is not a successful Realm test.

## Limits until the plan is implemented

- Do not use `all()` or `test(aarch64_tcg)` as the FVP selection policy.
  The general TCG/device suite is not the qualified FVP subset.
- `requires(cca)` enables tests that need CCA; it does not exclude ordinary
  tests with no such requirement.
- Do not use ignored-test overrides to force unsupported tests onto FVP.
- Add another test to the manual FVP selection only after reviewing its
  requirements and validating it on FVP.
- A future FVP-only test must stay out of QEMU's name-based CI selection
  until target metadata replaces that convention.

The filter is an operator selection rule, not the hard eligibility boundary
specified by the deferred plan. Existing test names and CI suffix filters
remain unchanged.

See the [CCA VMM test guide](Guide/src/dev_guide/tests/vmm/qemu_cca.md) for
the pinned platform, official payloads, logging, and cleanup contract.
This deferral does not weaken those checks or reintroduce a block rootfs.
