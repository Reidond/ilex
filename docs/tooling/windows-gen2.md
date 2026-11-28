# Windows Gen2 security qualification

Native Windows uses Hyper-V Generation 2 for the security tier. The Fedora
builder prepares the installed ilex disk through the existing T3 executor.
The test guest boots directly on Windows using a differencing VHDX, the
Microsoft UEFI CA template, and a local per-VM vTPM. The Windows backend
uses the same `VmSpec`, `VmProvider`, `VmControl` and exact-profile
`SecurityQualification` as Linux.

> The Gen2 install trials still drive the Anaconda installer (its chooser and
> kickstart) and have not been ported to the live installers, which replaced
> Anaconda. `ILEX_ISO` needs an Anaconda ISO built before that swap; the
> release no longer carries one under that name (`ilex-installer.iso` is now
> the GNOME live installer's file).

```powershell
$env:ILEX_ISO = 'C:\fixtures\ilex-installer.iso'
$env:ILEX_ISO_SHA256 = '<SHA-256 from the matching release>'
$env:ILEX_VM_SLOTS = '1'
cargo ilexctl doctor security --self-check --memory 6G
cargo ilexctl doctor security --require-qualified --memory 6G
cargo ilexctl test security::firmware ephemeral:: -- --test-threads 1
```

A matching `ilex-installer.iso.sha256` next to the ISO also supplies the
expected digest. The harness verifies the ISO byte-for-byte and boots it
against a blank 32 GiB VHDX. One trial reaches the ilex chooser. A separate
trial installs the selected image and checks that the vTPM survives the
disk boot. The part-4 tier covers encrypted installation and enrollment.
The writable OEMDRV trial uses native `openssl.exe` on `PATH`. The Fedora
builder creates the ext4 fixture and returns it as a checked job artifact.
The guest runs directly on Hyper-V. After detachment, the builder reads the
guest-written ext4 image in memory over pinned SSH. Windows keeps the
private key and decrypts the witness locally.

Hyper-V's local vTPM protector uses the machine's signing and encryption
certificates in the **Shielded VM Local Certificates** store. First use
can create this host-wide pair. The provider checks for the usable local pair
before allocating a vTPM and reports a missing guardian as a host prerequisite.
Prepared hosts reuse the pair across VMs. Fixture teardown removes its own
protector and vTPM state. The shared certificates belong to the host.

The default Gen2 profile uses fixed 6 GiB, two vCPUs and the existing Default
Switch. It accepts 2–6 GiB and up to four vCPUs; use
`ILEX_SECURITY_MEMORY` and `ILEX_SECURITY_VCPUS` to qualify another exact
profile. Gen2 and WHPX share the native slots and the 14 GiB host memory
reserve. An interrupted Gen2 launcher leaves its creation intent and slot
receipt for reconciliation. Use `cargo ilexctl doctor security --cleanup-run <run UUID>`
from the owning checkout after the launcher exits. If registration finished
before the UUID receipt was written, cleanup checks the private VM path,
name, generation, and target disk. An ambiguous or mismatched allocation
requires inspection.

The disk-only fixture adds an unpublished SSH and keyboard probe layer to
the chosen ilex image. The test delivers a disposable US-layout key to the
guest virtual console before SSH becomes available. The guest records its
boot ID when it accepts the key. A private read-only seed ISO provides an
ephemeral client key and pinned host identity to the installed fixture.
Both private keys stay in the checkout's local VM state. A separate guest
boots the exact installer ISO from its SCSI DVD drive. The installer test uses
the same consumer API as PR 18. Call `Provider::create_installer` with the
unchanged ISO on the spec, a blank VHDX target and OEMDRV as an ISO or a VHDX.
For a raw OEMDRV image, use `gen2::vhdx(raw, destination, "raw")` first. The
Gen2 target is the first Hyper-V SCSI disk (`sda` in the installer).
After Anaconda powers off, call `Provider::boot_installed` on the original
handle. The provider verifies the powered-off UUID and attached disks, ejects
installation media and sets the original VHDX as the sole boot device.
The optional SSH seed can remain attached. The test seals a TPM-bound credential
in Anaconda's live system, writes it to the target, and unseals it after booting
the installed system on that same VM.

For writable OEMDRV, Gen2 attaches a private writable copy of the supplied
standalone VHDX. After the powered-off transition detaches OEMDRV and before
starting the installed disk, call `Provider::with_installer_evidence` with
the original VM and a reader callback. It receives a raw snapshot inside
the VM's protected state for the duration of the callback. The provider
removes the snapshot afterward and preserves the input VHDX. PR 18 reads
`/recovery.oaep` through its private OAEP handling and selects evidence from
`/public` under its existing privacy gate. Keep the raw volume and recovery
envelope in private fixture state.

Private keyboard events travel over an anonymous pipe. The controller latches
screenshot redaction before sending any sensitive event, including when the
send fails. Keep the generation returned by `Vm::send_sensitive_text` or
`Vm::conceal_console`, for example before a guest displays a newly generated
recovery key. Pass that generation and its authenticated, public guest marker
to `Vm::acknowledge_sensitive_input`. A superseded generation leaves capture
withheld. Live capability reports reflect that state. `ilexctl vm type --stdin`
sends private text and starts its own redaction generation. The normal text
command accepts public input in argv.

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
cargo ilexctl vm run --secure --memory 6G
cargo ilexctl vm --vm secure capabilities --json
cargo ilexctl vm --vm secure type ilex-gen2-recovery-410625 --enter
cargo ilexctl vm --vm secure screenshot -o secure.png
```

`vm run --secure` requires an already qualified profile. The example above
sends the public fixture response before the pinned SSH session starts. Wait
for the session before taking a screenshot.
Add `--gui` to open VMConnect.
`ilexctl vm` and the MCP server attach to a journaled Gen2 guest. Hyper-V's
CIM console supplies capture and keyboard input. `ilexctl vm run` owns the lifecycle.

Evidence lands in `<state dir>/artifacts/gen2-security/<run UUID>` or the
corresponding tier under `ILEX_ARTIFACTS`. A profile qualifies only after
the installer, recovery keyboard, mouse, positive TPM, negative controls
and owned teardown pass. The immutable unenrolled disk cache and the
shared builder remain available for later tests.
