# Security firmware probes

PR 17 consumes T6's `VmSpec`, `VmCapabilities`, `SecurityProfile`,
`SecurityQualification` and `qualification::Run`. It adds guest assertions,
not another capability or qualification contract. Encryption, enrollment,
installer integration and recovery are later work.

## Running and interpreting the probes

Run `cargo xtask info` first. Only use this checkout's image, disk and VM
namespace. On the existing WSL executor:

```sh
cargo xtask build --shell none
ILEX_SHELLS=none cargo xtask security --self-check --memory 2G
cargo xtask security --require-qualified --memory 2G
# A different resource profile must have its own passing T6 record:
cargo xtask security --require-qualified --memory 2049M
# Explicit emulation, with independent qualification:
ILEX_SHELLS=none cargo xtask security --self-check --accelerator tcg --memory 4G
```

`security` alone is an inventory and explicitly says no guest was qualified.
`--require-qualified` prints the provider's existing capability/qualification
record and exits nonzero unless both firmware enforcement and persistent TPM
are effective for that exact accelerator/RAM/two-vCPU profile. It launches
nothing and does not upgrade provisioning availability into qualification.
It checks backend evidence; it does not certify any newly selected image.
`--self-check` explicitly starts a qualification fixture, invalidates an old
pass before measurement, and only records a pass after all assertions finish.
Native Windows reports unsupported/unqualified JSON and exits nonzero until
the tooling T5 provider is delivered. Run Linux diagnostics inside Linux/WSL.

The tier boots the actual selected ilex image through its installed Fedora
shim/GRUB/kernel chain. A direct kernel boot or Fedora cloud image is not an
equivalent test. At first boot, warm reboot and cold restart it requires:

- Guest `SecureBoot=1`, `SetupMode=0`, TPM version 2 and `/dev/tpmrm0`.
- An active integrity/confidentiality kernel lockdown policy.
- Nonzero SHA-256 PCR 7 and T6 event-log replay matching the live PCR, including
  meaningful policy and authority events. No fixed PCR value is prescribed.
- The exact selected OCI manifest digest in `bootc status`, and ilex's
  generated composition matching all five image selection labels. A stale
  `ILEX_DISK` is a failure, including when it still boots successfully.
- A public TPM-bound test credential decrypting after warm/cold restart,
  proving persistent TPM identity as well as PCR continuity.

Identical generated unsigned EFI bytes must execute with Secure Boot off,
then receive explicit firmware rejection with it on. T6 records common CODE
and enrolled VARS-template hashes and the private enable-boolean difference.
A missing execution marker alone never proves rejection. Profile evidence
and live paused/running checks use T6's capability contract unchanged.

`artifacts/security/result.json` stays `qualification=unqualified` for every
incomplete or failed run. Its `stage`, `disposition` and error identify platform
provisioning (`platform_unsupported_or_unqualified`), guest boot/assertion/
unsigned-control failure (`guest_failed`), or another unqualified harness/image
failure. These labels are diagnostics; the provider's typed qualification
record is authoritative. None is an ignored/green security pass. The existing
CI wrapper fails on missing evidence and uploads the public security directory.
An unchanged image's unrun tier is explicitly UNQUALIFIED in the CI summary.

Evidence includes the host ioctl and runtime versions, firmware descriptor and
CODE/template fingerprints, guest RPM/kernel versions and key-variable hashes,
booted image/configuration, raw EFI variables, lockdown, TPM/PCR state, binary
event logs/replay, serial/startup logs, timings and cleanup. It excludes TPM
state and credentials. PCR 7 measures accepted Secure Boot policy, not complete
root integrity: this legacy separate-initramfs chain is not a signed UKI or
sealed-root design. No disk-unlock result is implied by these probes.

## Backend requirements and Windows seam

| Backend | Required provisioning and qualification |
| --- | --- |
| Linux QEMU/KVM | Probe `KVM_CAP_X86_SMM` on the actual host. Use q35/SMM, secure pflash, a matching Microsoft-enrolled 4 MiB OVMF CODE/VARS pair and manufactured SHA-256 swtpm. Own disk/vars/TPM as a coherent bundle. Pass actual guest and unsigned controls for the requested resource profile. |
| Linux QEMU/TCG | Explicit `--accelerator tcg`, emulated CPU and the same firmware/TPM assertions, with its own timing budget and profile. Disk construction currently needs KVM or an already delivered owned disk. No silent retry after a KVM guest failure. |
| Windows Hyper-V Gen 2 (tooling T5 dependency) | `MicrosoftUEFICertificateAuthority`, persistent vTPM/key protector and checkout-owned UEFI/GPT VHDX; record host/Hyper-V versions, firmware/template identity and protected-state lifecycle. Management access, disk delivery, guest SSH, serial/enforcement diagnostics and unsigned on/off fixtures must be provided by T5. This PR contains no Hyper-V implementation or support stub. |
| Windows QEMU/WHPX | Not qualified for security: the measured Windows build lacks the TPM backend and rejects SMM. GPU development availability does not imply security support. |

Guest assertions in `crates/ilex-e2e/src/security/probes.rs` accept the existing
`VmControl` interface, independent of QMP and host filesystem paths. T5 can
reuse them once it provides `guest_exec` (including binary stdout), qualified
`VmProvider` lifecycle and its firmware/persistent-state evidence. The Linux
fixture adapter remains in `security.rs`; it is not a cross-platform provider.
Hyper-V's PCR bytes need not match OVMF's. Neither conversion to VHDX nor an
enabled Windows feature proves guest qualification. Screenshots/input and
installer boot capabilities must be supplied and tested by their owning
tooling/installer work before those later tiers can run.

WSL KVM at 2 GiB passed the earlier T6 measurements, while its 4 GiB guest
failed at SMM entry despite ioctl=1. TCG passed at 4/6 GiB, and hosted KVM
passed at 4 GiB (run 44091377698). These are host/profile observations, not a
universal RAM ceiling. The test uses the explicitly requested profile and
retains its qualification independently. Do not promote 2 GiB evidence to
4 GiB or infer hosted support from WSL results.

## ms-7c35 and module signing

The declared RX 9070 XT (`1002:7550`) uses Fedora's in-tree `amdgpu`; the
machine manifest adds userspace gaming packages and no out-of-tree driver.
The image-level assertion records `modinfo`, the owning RPM and firmware RPM,
requires `intree=Y`, a module signer and the GPU's PCI alias, then actually
loads `amdgpu` while the guest is under lockdown and checks there is no
unsigned-module taint. Unexpected akmod/kmod driver packages or files in the
external module directories require a separate signing review and fail it.

This establishes that ilex's Fedora driver needs no owner module-signing key
for the current image. It does not test a passed-through RX 9070 XT, physical
GPU initialization, board firmware/fTPM, or rendering. Those remain physical
ms-7c35 qualification. A future NVIDIA/akmods composition needs its own reviewed
signing/enrollment design and cannot inherit this result. OCI signatures,
Secure Boot and future encrypted storage remain separate trust boundaries.
