# T2 validation — 2026-11-16

Host: Windows x86_64, existing non-elevated Hyper-V Administrators token.
Checkout branch: `tooling/02-runtime-builder`, based on T1 `a0e65a6`.
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
- The Fedora signed CHECKSUM validates in process against the pinned Fedora 45
  key; the downloaded 606,732,288-byte Cloud image matches its pinned SHA256.
- An actual Gen2 creation exposed a PowerShell/.NET null-backup-name issue in
  atomic journal replacement. Replacement now uses `MoveFileExW` with replace
  and write-through. `builder cleanup-incomplete` recovered the flushed candidate
  only after checking creation intent, exact VM UUID, owner and matching Notes,
  then removed the owned registered VM, seed, identities, config and both VHDX
  files. Status returned `absent`; the verified Fedora download was retained.

## Live acceptance in progress

The 4 GiB/four-vCPU validation profile waits for at least **18 GiB available**:
4 GiB fixed builder RAM plus 12 GiB host reserve and 2 GiB margin. The owner
authorized two-minute polling for up to about three hours while other sessions'
WSL VM load varies. No other VM or WSL resource is stopped or changed. The
default 16 GiB/eight-vCPU profile needs 30 GiB available and is checked if capacity
appears in the same window.

Pending: full cloud-init/pinned-SSH provisioning, real nested KVM userspace boot,
graceful stop/restart with unchanged identity, intentional process interruption
and cleanup, real guest update with forced rollback, and the default resource
profile. These gates must not be inferred from compile checks or prior research.

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
