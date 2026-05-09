# SNP debug policy and KVM support notes

- SNP debug is enabled by setting `SNP_POLICY_MASK_DEBUG`, which is `BIT_ULL(19)` in the Linux PSP/SEV headers.
- Linux KVM accepts that bit for SNP launch policy: `KVM_SNP_POLICY_MASK_VALID` includes `SNP_POLICY_MASK_DEBUG` in `arch/x86/kvm/svm/sev.c`.
- KVM exposes SEV debug ioctls through `KVM_MEMORY_ENCRYPT_OP`: `KVM_SEV_DBG_DECRYPT` and `KVM_SEV_DBG_ENCRYPT`.
- The KVM docs for those ioctls say they fail if guest policy does not allow debugging.
- For SNP guests, KVM uses the SNP firmware debug command `SEV_CMD_SNP_DBG_DECRYPT` when it needs to decrypt protected VMSA state for debug/error reporting.
- Setting the policy bit does not automatically make normal protected guest state available through ordinary APIs such as `KVM_GET_REGS`, nor does it make private guest memory directly readable. Debug tooling needs to use KVM paths that invoke the SEV/SNP debug decrypt operation.

References in the Linux tree used for this check:

- `include/linux/psp-sev.h`
- `arch/x86/kvm/svm/sev.c`
- `Documentation/virt/kvm/x86/amd-memory-encryption.rst`
