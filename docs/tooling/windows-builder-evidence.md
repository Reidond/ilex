# T2 validation — 2026-11-16/17

Host: Windows x86_64, existing non-elevated Hyper-V Administrators token.
Checkout branch: `tooling/02-runtime-builder`, rebased on T1 `b34be7c`.
No WSL commands, Windows feature changes, new switches, system installs,
registry publication or release creation were used.

## Completed checks

- `cargo check -p xtask` and `cargo clippy -p xtask -- -D warnings` passed
  natively. Shell recipes pass `bash -n` using Git's parser. PowerShell's AST
  parser accepts the provisioning adapter. No unit tests were added or run.
- Clean `%LOCALAPPDATA%/ilex/runtimes` acquisition downloaded all 158 locked
  official MSYS2 archives (271,185,572 bytes compressed). SHA256 and size checks
  preceded extraction. QEMU and qemu-img both report **11.1.3** from the new
  per-user directory, without the research runtime or a system QEMU installation.
- A terminated acquisition left unpublished staging. The subsequent fetch
  discarded staging and reused the already verified archives. The final runtime
  lock SHA256 is `0b77976331754a55f27e9c6b9da99374810a692be6cbc35c710d345f60309e1f`.
- `cargo xtask runtime self-check` loaded real qemu-img, rejected corrupted
  SHA256, traversal/device names and an escaping archive link, and removed its
  owned fixture. A separate PowerShell corrupt-cache fixture command was rejected
  by automatic approval review with only “blocked by policy”; it did not execute.
- `cargo xtask runtime update` booted the disposable q35/WHPX/SDL GL/virgl firmware
  fixture, queried running status with correlated loopback QMP, quit it and
  activated the runtime. This proves host runtime initialization; it is not the
  T4 actual-ilex rendering/input qualification.
- Helper lifetime recovery was exercised by terminating a waiting xtask after
  verifying that no VM journal existed. Its PowerShell child exited automatically
  when the owning Windows Job Object closed. No broad process kill was used.
- The Fedora signed CHECKSUM validates in process against the pinned Fedora 45
  key; the downloaded 606,732,288-byte Cloud image matches its pinned SHA256.
- An actual Gen2 creation exposed a PowerShell/.NET null-backup-name issue in
  atomic journal replacement. Replacement now uses `MoveFileExW` with replace
  and write-through. `builder cleanup-incomplete` recovered the flushed candidate
  only after checking creation intent, exact VM UUID, owner and matching Notes,
  then removed the owned registered VM, seed, identities, config and both VHDX
  files. Status returned `absent`; the verified Fedora download was retained.

## Live guest checks

The 4 GiB/four-vCPU validation profile waits for at least **18 GiB available**:
4 GiB fixed builder RAM plus 12 GiB host reserve and 2 GiB margin. The owner
authorized two-minute polling for up to about three hours while other sessions'
WSL VM load varies. No other VM or WSL resource is stopped or changed. The
default 16 GiB/eight-vCPU profile needs 30 GiB available and is checked if capacity
appears in the same window.

Both resource profiles passed. The following results are from this
implementation's VM, not the earlier research VM:

- Cloud-init automatically installed the guest dependencies. Maintenance SSH
  matched the pinned host key, owner, revision and data UUID. A deliberately
  wrong host key failed with exit 255. The disposable bootstrap key was revoked
  (also exit 255), and the seed/key directory/ISO were detached and deleted.
- Default **16 GiB/eight-vCPU** self-check passed, with live guest memory
  **16,753,336,320 bytes** and eight CPUs. Fixed memory, nested extensions and
  disabled automatic checkpoints were verified; Secure Boot/vTPM were configured.
- Graceful shutdown, resource change to **4 GiB/four vCPUs**, restart and
  self-check passed with the same VM UUID, data UUID and pinned SSH identity.
  Live guest memory was **4,087,316,480 bytes**, four CPUs. The combined run took
  **287.59 s including memory polling**; this is not a boot-latency measurement.
- Both profiles booted a real nested QEMU/KVM VM using the installed Fedora
  kernel and an isolated initramfs. PID 1 emitted `ILEX_NESTED_KVM_BOOTED` and
  powered off. KVM API **12**, rootful Podman, bcvk **0.22.0** and QEMU were
  probed. The nested fixture was removed, retaining its serial diagnostic.

Validated VM UUID: `d8a13170-bd38-4dba-bf12-fe6e67fa0202`; creation run:
`45a805d3-4923-4d24-86ee-48b23070acd9`; XFS data UUID:
`6f99c09a-772b-452b-9cdf-4870ede77b64`. Address discovery used this exact VM's
Default Switch KVP/NIC and exact-MAC neighbor lookup.

Recorded packages: Podman **6.1.3-1.fc45**, QEMU **11.1.3-1.fc45**, libvirt
**12.6.0-4.fc45**, Rust/Cargo **1.99.1-2.fc45**, image-builder **89.0.0-1.fc45**,
Hyper-V daemons **6.10-4.fc45**, kernels **7.2.11-300.fc45** and
**7.3.4-300.fc45**. Full RPM manifest SHA256:
`579a9d95548ec0858ccec0ae9a276207453b3e75089a8165cce8cc789b0fe695`.
DNF repositories move; this records actual versions, not reproducible RPM bytes.

Development provisioning exposed indented image-builder YAML, PID 1 after a
systemd package update, Windows CRLF stdin framing and dracut's `/init` symlink.
Fixes use whitespace-tolerant version parsing, daemon reexec, explicit UTF-8/LF
framing and an appended newc overlay. An unfinished guest whose controller was
installed resumed the reviewed recipe over pinned SSH before marking readiness.
This was iterative development, not a clean-bootstrap timing benchmark.

## Interrupted bootstrap and the real host reboot

All cleanup checked exact journal UUID, owner, run, Hyper-V Notes and bounded
owned paths. A name/prefix alone never authorized deletion. No other session's
VM, disk, image, checkout or WSL resource was modified.

- Intentional xtask termination during provisioning left VM
  `335d460c-4617-4d9c-b464-86243af89086` running with its journal. Its PowerShell
  helper exited automatically when the parent's Job Object closed. Subsequent
  `cleanup-incomplete` removed precisely that VM and its bootstrap resources.
- Internet loss and the owner's real host reboot interrupted VM
  `1d3169ef-0b0a-41f2-ada7-c7e887c6694d`. On resumption it was **off**, journal
  phase **provisioning**, with 40/240 GiB disks and its seed still attached.
  Serial output stopped at package download **266/534**; the guest controller
  was not installed and the data disk was unformatted. After inspecting the
  exact UUID/Notes/paths and saving diagnostics, `cleanup-incomplete` removed
  that VM, disks, seed ISO and identities. The verified Fedora download remained
  available for the next bootstrap. Private keys were not copied into evidence.

## Update recovery

The initial checkpoint attempt revealed that the VM SID also needs directory
access to create differencing VHDX files. Disks now live under `builder/disks`
with an inheritable grant only for the exact owned VM SID. Seed and SSH keys
remain outside that grant. Existing owned disks migrated while off with no
checkpoints; the next real checkpoint succeeded without elevation.

The forced rollback probe applied provisioning revision
`bd4c9f446aace5bc0dfe662766855c7b3ead121918b2755638b60f50d64e1c78`, then deliberately
failed. Exact checkpoint `eba2c8a2-460e-450e-a032-853c72784960` restored revision
`394c3afec5f83c5c9cf8e94cec6c59e6fd29614612555a1eba0e516045342482`. Pinned SSH,
the same data UUID and actual nested boot passed afterward. Journal state was
`ready`/`rolled-back`. Elapsed **316.92 s** includes memory polling, update and
recovery. Explicit acceptance removed that retained recovery point.

Normal update and explicit rollback after successful verification are being
qualified before this evidence is finalized.

Reproducible active commands (the rollback probe intentionally returns failure
only after recovery passes):

```powershell
cargo xtask runtime fetch
cargo xtask runtime self-check
cargo xtask runtime update
cargo xtask builder up --wait-for-memory-seconds 10800
cargo xtask builder --self-check
cargo xtask builder down
cargo xtask builder up --memory-gib 4 --cpus 4 --wait-for-memory-seconds 10800
cargo xtask builder --self-check
cargo xtask builder update --rollback-probe --wait-for-memory-seconds 10800
cargo xtask builder status --json
cargo xtask builder accept-update
cargo xtask builder update --wait-for-memory-seconds 10800
```

Build/test command execution and actual ilex desktop qualification remain T3/T4.

Local evidence directory: `C:/Users/reido/Documents/ilex-tooling-t2-2026-11-16/`.
Operational logs and the owned resource journal are under
`%LOCALAPPDATA%/ilex/builder/`. Seed/private-key bytes are never PR evidence.

## CI coordination

At the first check, publish workflow run **43927637994** for T1 was queued.
No T2 branch workflow was dispatched while the shared publishing queue was busy.
The final PR records whether a branch run could be dispatched later.

## Retained infrastructure

The design expects one ready builder, `ilex-builder-3ccbf5231e80`, to persist as
shared per-user infrastructure. Incomplete development builders are removed by
their exact journal identity. The final validation entry records whether a ready
builder was left running/off, or whether no task-created builder remains.
