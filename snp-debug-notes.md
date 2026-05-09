# SNP debug policy and KVM support notes

- SNP debug is enabled by setting `SNP_POLICY_MASK_DEBUG`, which is `BIT_ULL(19)` in the Linux PSP/SEV headers.
- Linux KVM accepts that bit for SNP launch policy: `KVM_SNP_POLICY_MASK_VALID` includes `SNP_POLICY_MASK_DEBUG` in `arch/x86/kvm/svm/sev.c`.
- KVM exposes SEV debug ioctls through `KVM_MEMORY_ENCRYPT_OP`: `KVM_SEV_DBG_DECRYPT` and `KVM_SEV_DBG_ENCRYPT`.
- The KVM docs for those ioctls say they fail if guest policy does not allow debugging.
- In upstream Linux KVM, `KVM_SEV_DBG_DECRYPT` is not usable for SNP guests. SNP userspace commands start at `KVM_SEV_SNP_LAUNCH_START = 100`, and KVM rejects older command IDs for SNP VMs with `-EPERM` after `KVM_SEV_INIT2`.
- There is no upstream `KVM_SEV_SNP_DBG_DECRYPT` userspace command in `arch/x86/include/uapi/asm/kvm.h`, so OpenVMM cannot decrypt arbitrary SNP private pages through KVM UAPI.
- For SNP guests, KVM uses the SNP firmware debug command `SEV_CMD_SNP_DBG_DECRYPT` internally when it needs to decrypt protected VMSA state for debug/error reporting.
- Setting the policy bit does not automatically make normal protected guest state available through ordinary APIs such as `KVM_GET_REGS`, nor does it make private guest memory directly readable. Debug policy enables KVM's internal SNP debug path, not a general userspace private-memory decrypt API.

## Private host-kernel debug action items

If userspace-visible launch logs and serial output are not enough, the next useful step is to boot the SNP host with a private/debug kernel built from the local upstream tree and add temporary KVM instrumentation. These changes should stay out of upstream/product code unless they are redesigned as an acceptable debug interface.

1. Dump protected VMSA state when an SNP guest triple faults.
   - Add a debug-only call site on the `KVM_EXIT_SHUTDOWN` / triple-fault path, e.g. near `KVM_REQ_TRIPLE_FAULT` handling in `arch/x86/kvm/x86.c` or the SVM shutdown interception path in `arch/x86/kvm/svm/svm.c`.
   - For SNP guests with `SNP_POLICY_MASK_DEBUG`, call KVM's internal VMSA access path in `arch/x86/kvm/svm/sev.c` that decrypts the VMSA via `SEV_CMD_SNP_DBG_DECRYPT`.
   - Print the decoded VMSA fields needed for bring-up: RIP, RSP, RFLAGS, CR0, CR3, CR4, EFER, segment bases/limits/attributes, and relevant control/intercept state. A raw hex dump is useful as a fallback, but decoded fields are much easier to compare against the expected loader state.

2. Add a narrow debug-only SNP private-page decrypt hook.
   - Upstream does not expose `KVM_SEV_SNP_DBG_DECRYPT` as a userspace command, but the PSP firmware command exists as `SEV_CMD_SNP_DBG_DECRYPT`.
   - Add a temporary KVM debugfs file or private ioctl that accepts a GPA and length, resolves the GPA to the backing private page/RMP-owned page, invokes the SNP firmware debug decrypt command, and copies the plaintext into a kernel bounce page or userspace buffer.
   - Use this to inspect the launch-critical pages after failure: initial page tables, Linux zero page / boot params, CC blob setup-data chain, SNP CPUID page, secrets page, and the kernel entry page.
   - Gate it on SNP debug policy and a local debug config/static key so it cannot be accidentally enabled for normal guests.

3. Add KVM SNP launch/update trace points or temporary logs.
   - Log launch policy, ASID/context creation, VMSA GPA/physical address, page-type counts, and the GPA ranges updated as normal, zero, unmeasured, secrets, CPUID, and VMSA pages.
   - Log PSP/RMP failures with both Linux return codes and firmware error codes.
   - For special pages, log checksums or first bytes before encryption so OpenVMM launch metadata can be correlated with what KVM handed to firmware.

4. Add shutdown/exception context logging around the triple fault.
   - Capture the SVM exit code / exit info that led to shutdown when available.
   - For SNP, expect normal architectural register APIs to be insufficient after launch; prefer VMSA decrypt output over `KVM_GET_REGS`-style state.

## OpenVMM debug action items

OpenVMM still needs changes to consume the private KVM debug hooks and to make its launch inputs easier to correlate with kernel-side dumps.

1. Replace the exploratory `KVM_SEV_DBG_DECRYPT` SNP path.
   - The old SEV `KVM_SEV_DBG_DECRYPT` command returns `-EPERM` for SNP guests on upstream KVM, so OpenVMM should not keep using it as the primary SNP debug path.
   - If the private host kernel exposes a temporary SNP debug decrypt ioctl/debugfs file, add an explicitly debug-only OpenVMM wrapper for that interface instead.
   - Keep the wrapper out of normal launch flow and gate it behind a local debug option/environment variable so production SNP launch does not depend on private kernel patches.

2. Add OpenVMM-side debug requests for launch-critical GPAs.
   - On triple fault or `KVM_EXIT_SHUTDOWN`, request decrypt/dump of the pages most likely to explain early boot failure: kernel entry page, initial page tables, Linux zero page / boot params, CC blob setup-data chain, SNP CPUID page, secrets page, and any VMSA GPA if exposed by the kernel hook.
   - Print hexdumps and decoded forms where OpenVMM already has structure definitions, especially page-table entries, Linux boot params, CC blob fields, and SNP CPUID table entries.
   - Include GPA, length, page tag, `BootPageAcceptance`, and launch page type in every dump header so output can be matched to KVM launch-update logs.

3. Improve launch metadata logging before `KVM_SEV_SNP_LAUNCH_FINISH`.
   - Log the SNP policy in hex and decoded bits.
   - Log page-type counts and total bytes for normal, zero, unmeasured, secrets, CPUID, and VMSA updates.
   - Log special page GPAs, including the CPUID page, secrets page, Linux zero page, CC blob, page tables, and entry page.
   - Log the BSP initial register state that OpenVMM intended to encode into the protected VMSA.

4. Add script support for private-kernel debug collection.
   - Extend `run-snp-openvmm-repro.sh` to preserve OpenVMM logs plus the relevant host `dmesg`/kernel log lines after the repro terminates.
   - Keep handling OpenVMM panics/aborts and triple faults as terminal conditions so the script does not hang before collecting host-side debug output.
   - Make debug collection optional when it requires privileged commands on the SNP host.

## Ubuntu SNP host private-kernel build/deploy plan

The test system is an Ubuntu physical host, so before building and deploying a private debug kernel collect enough information to avoid producing an unbootable or inconvenient kernel package.

1. Collect host facts.
   - Ubuntu release: `lsb_release -a`.
   - Current kernel: `uname -a`.
   - Bootloader and boot mode: check GRUB/EFI state, e.g. `bootctl status` when available and `/boot/grub/grub.cfg`.
   - Secure Boot state: `mokutil --sb-state`.
   - Current KVM/SNP module state and kernel log baseline: `lsmod | grep kvm`, `dmesg` snippets for SEV/SNP/KVM.

2. Collect the kernel config source.
   - Prefer `/boot/config-$(uname -r)`.
   - Use `/proc/config.gz` if available.
   - Base the private kernel config on the currently booted Ubuntu config, then enable only missing KVM/SNP/debug options needed for the temporary instrumentation.

3. Decide where to build.
   - Build directly on the SNP host if it has enough CPU, disk, and installed build dependencies.
   - Otherwise build Debian kernel packages elsewhere from the same kernel tree/config and copy the `.deb` files to the host.
   - Confirm available disk space in the build directory and `/boot`.

4. Confirm deployment permissions and recovery path.
   - Verify sudo/root access on the host.
   - Confirm the host can be rebooted for debugging.
   - Confirm console/IPMI/serial or other out-of-band recovery access in case the private kernel fails to boot.
   - Decide whether the new kernel should become the default GRUB entry or be selected for a one-time boot.

5. Choose the exact kernel source base.
   - Default to the same Ubuntu kernel series/source branch as the kernel currently running on the SNP host, then apply only the temporary KVM SNP debug patches.
   - Treat the local upstream Linux tree at `~/ai/eevee/linux` as the KVM SNP reference unless it already matches the host kernel closely.
   - If the host is running an Ubuntu HWE, OEM, cloud, or other flavor branch, patch that matching Ubuntu source branch/package rather than switching the host to an unrelated upstream kernel.
   - Confirm the selected source version is acceptable for the Ubuntu host's userspace, firmware, and drivers before deploying it.

6. Build and install flow.
   - Copy the host's current config into the chosen kernel tree.
   - Set a unique local version string so the debug kernel is easy to identify in GRUB and `uname -a`.
   - Build Debian packages for the kernel image/modules/headers.
   - Copy packages to the SNP host and install with `dpkg -i`.
   - Update GRUB if needed, reboot into the debug kernel, verify `uname -a`, and then run the SNP repro while collecting OpenVMM logs and host kernel logs.

References in the Linux tree used for this check:

- `include/linux/psp-sev.h`
- `arch/x86/include/uapi/asm/kvm.h`
- `arch/x86/kvm/svm/sev.c`
- `Documentation/virt/kvm/x86/amd-memory-encryption.rst`
