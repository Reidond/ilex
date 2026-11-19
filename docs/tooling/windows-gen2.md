# Windows Gen2 security qualification

Native Windows uses Hyper-V Generation 2 for the security tier. The Fedora
builder prepares the installed ilex disk through the existing T3 executor.
The test guest boots directly on Windows using a differencing VHDX, the
Microsoft UEFI CA template, and a local per-VM vTPM. The Windows backend
uses the same `VmSpec`, `VmProvider`, `VmControl` and exact-profile
`SecurityQualification` as Linux.

```powershell
$env:ILEX_ISO = 'C:\fixtures\ilex-installer.iso'
$env:ILEX_ISO_SHA256 = '<SHA-256 from the matching release>'
$env:ILEX_VM_SLOTS = '1'
cargo xtask security --self-check --memory 6G
cargo xtask security --require-qualified --memory 6G
cargo xtask test security::firmware ephemeral:: -- --test-threads 1
```

A matching `ilex-installer.iso.sha256` next to the ISO also supplies the
expected digest. The harness verifies the ISO byte-for-byte and boots it
against a blank 32 GiB VHDX. It stops at the ilex chooser. Encrypted
installation and enrollment are covered by the later part-4 tier.

The default Gen2 profile uses fixed 6 GiB, two vCPUs and the existing Default
Switch. It accepts 2–6 GiB and up to four vCPUs; use
`ILEX_SECURITY_MEMORY` and `ILEX_SECURITY_VCPUS` to qualify another exact
profile. Gen2 and WHPX share the native slots and the 14 GiB host memory
reserve. An interrupted Gen2 launcher leaves its slot receipt and exact UUID
for reconciliation. Use `cargo xtask security --cleanup-run <run UUID>`
from the owning checkout after the launcher exits. A missing or mismatched
allocation receipt requires inspection.

The disk-only fixture adds an unpublished SSH and keyboard probe layer to
the chosen ilex image. The test delivers a disposable US-layout key to the
guest virtual console before SSH becomes available. The guest records its
boot ID when it accepts the key. A private read-only seed ISO provides an
ephemeral client key and pinned host identity to the installed fixture.
Both private keys stay in the checkout's local VM state. A separate guest
boots the exact installer ISO from its SCSI DVD drive.

The tier reads the guest Secure Boot variables, lockdown and SHA-256 TPM
event log over pinned SSH. It reuses the Linux PCR 7 replay and ilex identity
assertions. It decrypts the same PCR-bound credential after warm and cold
boots, then verifies failure on a fresh vTPM with matching PCR 7. The
unsigned-EFI control must execute when Secure Boot is off and show a
firmware refusal with Secure Boot on, using the same boot payload.

WMI capture records the exact VM UUID, timestamp and framebuffer size in a
sidecar for each PNG. The self-check uses inbox English OCR to identify the
installer chooser, the keyboard prompt and the unsigned-EFI control. Mouse
support is qualified only after the Linux input device receives the two
absolute axes and a click. Firmware pointer, clipboard, and suspend remain
unavailable. From the owning checkout:

```powershell
cargo xtask vm --secure --memory 6G
ilex-vmctl --vm secure capabilities --json
ilex-vmctl --vm secure screenshot -o secure.png
ilex-vmctl --vm secure type ilex-gen2-recovery-410625 --enter
```

`vm --secure` requires an already qualified profile. It prints the fixture
key before opening a pinned SSH session. Add `--gui` to open VMConnect.
`ilex-vmctl` and its MCP server attach to a journaled Gen2 guest. Hyper-V's
CIM console supplies capture and keyboard input. xtask owns the lifecycle.

Evidence lands in `<state dir>/artifacts/gen2-security/<run UUID>` or the
corresponding tier under `ILEX_ARTIFACTS`. A profile qualifies only after
the installer, recovery keyboard, mouse, positive TPM, negative controls
and owned teardown pass. The immutable unenrolled disk cache and the
shared builder remain available for later tests.
