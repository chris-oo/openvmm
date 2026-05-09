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

References in the Linux tree used for this check:

- `include/linux/psp-sev.h`
- `arch/x86/include/uapi/asm/kvm.h`
- `arch/x86/kvm/svm/sev.c`
- `Documentation/virt/kvm/x86/amd-memory-encryption.rst`
