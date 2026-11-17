# T2 validation — 2026-11-16/17

Host: Windows x86_64, existing non-elevated Hyper-V Administrators token.
Checkout branch: `tooling/02-runtime-builder`, rebased with `gh stack rebase` on
T1 `37eb731`, including the phase0 CI fix, image-builder cache and automatic
primary claim fixes.
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
authorized capacity polling for up to about three hours while other sessions'
WSL VM load varies. No other VM or WSL resource is stopped or changed. The
default 16 GiB/eight-vCPU profile needs 30 GiB available and is checked if capacity
appears in the same window.
Initial polling was every two minutes; it was shortened to 30 seconds after
brief qualifying memory peaks disappeared between checks. The reserve and
original maintenance wait deadline were preserved.

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

The updated development VM recorded Podman **6.1.3-1.fc45**, QEMU **11.1.3-1.fc45**, libvirt
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
An ACL inspection confirmed that the exact VM SID has the disk-directory grant
and no grant on the builder root or maintenance client key.

The forced rollback probe applied provisioning revision
`bd4c9f446aace5bc0dfe662766855c7b3ead121918b2755638b60f50d64e1c78`, then deliberately
failed. Exact checkpoint `eba2c8a2-460e-450e-a032-853c72784960` restored revision
`394c3afec5f83c5c9cf8e94cec6c59e6fd29614612555a1eba0e516045342482`. Pinned SSH,
the same data UUID and actual nested boot passed afterward. Journal state was
`ready`/`rolled-back`. Elapsed **316.92 s** includes memory polling, update and
recovery. Explicit acceptance removed that retained recovery point.

Normal update then applied revision
`b9a0f1c5e2585927d1bdddc17fe304aa5da912f7f155b7672ff5a8b2ec24a27e`, rebooted and
passed nested smoke. Both running builder and nested kernel reported
**7.3.4-300.fc45.x86_64**. Checkpoint
`9a4ec62d-0290-4b93-aea9-3a13ee440ea2` separately recorded the old recipe revision.
Explicit `rollback` restored that distinct old revision and passed pinned SSH,
data UUID and nested boot checks. Explicit acceptance removed the checkpoint.

Interrupted-update recovery was also exercised: the waiting parent was stopped
only after confirming the owned VM was off and guest updating had not started.
Journal state remained `updating`, checkpoint
`45897800-1ccb-4ef7-a578-0e5319cb221c` remained recorded, and `builder rollback
--wait-for-memory-seconds ...` recovered and smoke-tested the original recipe.
New updates now wait for memory before creating the maintenance transaction;
rollback supports the same bounded capacity wait.

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
cargo xtask builder rollback --wait-for-memory-seconds 10800
cargo xtask builder accept-update
```

Build/test command execution and actual ilex desktop qualification remain T3/T4.

Local evidence directory: `C:/Users/reido/Documents/ilex-tooling-t2-2026-11-16/`.
Operational logs and the owned resource journal are under
`%LOCALAPPDATA%/ilex/builder/`. Seed/private-key bytes are never PR evidence.

## CI coordination

At the first check, publish workflow run **43927637994** for T1 was queued.
No T2 branch workflow was dispatched while the shared publishing queue was busy.
When the queue cleared, T2 branch test run
[44098798793](https://github.com/Reidond/ilex/actions/runs/44098798793) was dispatched
on pre-rebase head `b2b4ea2`. **Build passed; upgrade failed** before updating to
the candidate. The published v1 image
`sha256:f68e8dda514e1a0c9dbc35d9e6f531ad03283355315315c3fe8823d880077126`
reported an active `/boot` autofs mount, and the shipped boot-mount protection
assertion failed. This digest is outside the fixture's two explicit legacy
exceptions. T2 does not change that Linux fixture or relax its assertion.

After rebasing onto the T1 follow-up, T1 run
[44102368753](https://github.com/Reidond/ilex/actions/runs/44102368753) was in
progress at handoff preparation. The current T2 head still needs a fresh Linux
branch run; the final PR records the latest queue/dispatch state. The Windows
acceptance above passed independently; CI is not recorded as green.

## Retained infrastructure

After update qualification, development VM
`d8a13170-bd38-4dba-bf12-fe6e67fa0202` was shut down gracefully, its accepted
checkpoints were removed, and an exact-UUID/owner/run/data-UUID development
cleanup removed its disks/config/identities. No name-based cleanup was used.

A final fresh bootstrap created VM **`b521170d-a2e0-4791-8b3e-71819084203d`**,
name **`ilex-builder-3ccbf5231e80`**, with new disks/seed/client/host identities and
data UUID **`3c09e083-3b2b-4ab0-bf23-e96b1dc57322`**. It reused checksum-verified
host runtime/Fedora downloads. Provisioning needed no repairs or resume recipe;
cloud-init finished at **165.22 s**, and `builder up` returned ready at
**203.17 s** including creation/conversion/provisioning/smoke. This is a fresh VM
bootstrap with a warm host download cache, not an all-downloads-cold benchmark.

Fresh `builder --self-check` passed real nested userspace boot, pinned identity,
four CPUs and fixed 4 GiB RAM. Its Fedora Cloud kernel and nested kernel were
**7.2.11-300.fc45.x86_64**; bootstrap installs dependencies, while explicit
`update` performs the full system upgrade tested above. Other recorded tool
versions match the development VM. Fresh RPM manifest SHA256:
`82e77a2cb4b59e0bfa7cee18cb3c90b141efed9ab0ccc06113fdaad6157e9c66`.
Provisioning revision:
`b9a0f1c5e2585927d1bdddc17fe304aa5da912f7f155b7672ff5a8b2ec24a27e`.

The final seed/seed-key directory/ISO are gone. The exact new VM SID has disk
access and no builder-root/client-key grant. Exact retired UUID checks confirmed
that the interrupted and development VMs listed above are absent. The design
retains this one ready builder as shared per-user infrastructure. Final graceful
stop/restart, pinned identity and nested self-check passed. It is left **off**,
phase **ready**, **4 GiB/four vCPUs**, with **zero checkpoints**, two non-sparse,
uncompressed, unencrypted VHDX files, Gen2 Secure Boot on and vTPM enabled. It
consumes no running VM RAM. The default remains 16 GiB/eight vCPUs; restore it
with `builder up --memory-gib 16 --cpus 8 --wait-for-memory-seconds 10800` when
30 GiB host capacity is available.

## Review follow-up — exact interruption boundaries

Review `5940511608` covered head `1ece404`. The four P2 fixes were first qualified
on T1 `1c82e48`, then rebased onto `37eb7319d553da579495e5c47606ecceedd03f3a`
after the owner confirmed its phase0 CI fix. No unit tests were added or run.

Reproduction on native Windows, with the retained ready builder off:

```powershell
cargo check -p xtask --locked
cargo clippy -p xtask --locked -- -D warnings
cargo xtask builder recovery-self-check --wait-for-memory-seconds 10800
cargo xtask runtime activation-self-check
```

The recovery command uses instrumented copies of the embedded production helper:
one identity salt for the disposable fixture, exact single-match abrupt-exit
anchors, and a start witness that prevents accidental startup during negative
admission checks. The production recovery/admission logic is unchanged in those
copies. The real installation lock covers the retained builder checks. The
maintenance SSH key remains guest-root authority, as clarified in the usage doc.

1. **Interrupted RAM/CPU resize:** the real retained UUID
   `b521170d-a2e0-4791-8b3e-71819084203d` was off at 4 GiB/four CPUs. Abrupt exit
   immediately after `Set-VMMemory` left actual startup RAM at 16 GiB while the
   journal still recorded 4 GiB. Ordinary `up` rejected resource drift before
   admission or the start witness. Explicit original options repaired actual
   settings even though they already matched the journal; interruption after
   journal commit kept it off. A separate interruption after `Set-VMProcessor`
   before journal commit exercised CPU drift and the same repair. The UUID,
   data UUID, provisioning revision and SSH pin stayed unchanged. The original
   4 GiB/four-CPU profile was restored without starting the VM.
2. **Allocated candidate:** passed. The command exited immediately after flushing
   `journal.json.partial` in phase `allocated`, before rename and before VM
   Notes were written. It then recovered against the complete prior
   creation intent, UUID/resource and actual configuration/disk paths. A wrong
   resource run and an ordinary ready record with empty Notes both failed closed.
3. **Cleanup after unregister:** passed. The same disposable Gen2 fixture exited
   immediately after `Remove-VM`, leaving phase `deleting` and its validated
   path manifest while seed keys/disks still existed. Retry finished those
   paths with the exact UUID absent. An outside path and a new separately owned
   replacement UUID using the same name were rejected before deletion. The
   replacement survived the guard check and was then removed using its own
   creation UUID/Notes. All fixture resources and keys were removed. These
   allocation/deletion cases waited until the host exceeded 18 GiB available;
   the 14 GiB reserve was never reduced.
4. **First runtime update:** passed in a fresh process-local `LOCALAPPDATA`,
   using copied checksum-verified compressed archives and fresh extraction.
   An unavailable SDL video driver caused real QEMU SDL initialization failure
   after readiness, leaving both `active.json` and `previous.json` absent.
   A subsequent actual WHPX/SDL/GL firmware smoke created the first active
   pointer; the normal installation's pointer stayed unchanged and the entire
   isolated runtime fixture was removed. An initial positive run exceeded the
   old five-second QMP budget; the bounded 30-second budget passed.

Final allocation/deletion fixture UUID:
`ee5a90e1-b8cd-4fac-be88-41b0007052eb`; protected replacement UUID:
`f035d7a1-86ce-49f4-a2cc-f2900f313e72`. Both were removed by exact recorded
identity. The final production-helper SHA256 was
`8ecc1cd60651b0e1da678d4eaa76fdf0655ae3aeb136c27a8029454b097f93ad`, identical
across the rebase. Resource observations use a fresh helper process after child
mutations, rather than reuse a parent Hyper-V settings object. Status now reports
`actual_resources` alongside the journal, making such drift observable.

Post-rebase locked clippy, runtime checksum/archive self-check and fresh-install
activation self-check passed. No T2 changes conflicted: gh-stack initially tried
replaying inherited phase0 history, so that attempt was aborted, only T2's ten
commits were replayed with GPG signing, and `gh stack rebase` then refreshed the
stack successfully. The Windows recovery helper carried across byte-for-byte.

Locked native check/clippy and all PowerShell parsers passed. Local result files
are under `C:/Users/reido/Documents/ilex-tooling-t2-2026-11-17-review/`; they contain
resource identities and booleans, not seed/private-key bytes.

[Run 44105654095](https://github.com/Reidond/ilex/actions/runs/44105654095), on
reviewed head `1ece404`, completed **failure**: `build` passed and
`upgrade::upgrade_and_rollback` failed `checking shipped boot mount protection`
on the published v1 before candidate update, with active `/boot` autofs. This
does not qualify the newer review-fix head. The newer phase0/T1 fix is now
included through the rebase. The final PR records the latest shared CI queue
check and whether a fresh branch run was dispatched.

## Re-review 5941791417 — maintenance and lookup failures

The two additional P2s were tested with the new isolated command:

```powershell
cargo xtask builder maintenance-self-check --wait-for-memory-seconds 10800
cargo clippy -p xtask --locked -- -D warnings
```

This diagnostic has private helper/request/recipe files and its own installation
lock. It neither takes the real builder's lock nor overwrites that installation's
scripts. The shared UUID `b521170d-a2e0-4791-8b3e-71819084203d` was observed off at
**16 GiB/eight CPUs**, with a matching journal, at the start and finish. Its
configuration, checkpoints, disks and keys were not modified by these checks;
T3 can continue using that shared installation.

The private Gen2 fixture remained **off** throughout. It uses explicit host-only
readiness/acceptance preconditions and a start witness; **no guest update or
guest smoke is claimed by this diagnostic**. Existing real guest evidence above
is separate. The complete final result is
`C:/Users/reido/Documents/ilex-tooling-t2-2026-11-17-rereview/maintenance-self-check-final.json`.

- Actual 16 GiB versus journal 4 GiB was injected immediately after
  `Set-VMMemory`. `update` rejected drift before checkpoint creation or recovery
  phase mutation. Zero checkpoints and an unchanged journal/file inventory were
  required; ordinary explicit original-profile repair remained available.
- A second drift was injected immediately after the real capacity wait.
  Revalidation rejected it before creating a checkpoint or intent. The unchanged
  14 GiB reserve and one overall wait budget were enforced; the fixture's 4 GiB
  admission gate was not reduced.
- Abrupt exits after checkpoint-intent publication, checkpoint API return,
  updating-journal publication, restoring-journal publication, restore API return,
  and checkpoint acceptance before/after removal all recovered. The exact
  reserved name lets rollback find a checkpoint whose UUID was not yet committed;
  an unfulfilled intent can be abandoned only after successful lookup/idle checks.
- A real legacy bad checkpoint captured 16 GiB while the journal retained 4 GiB.
  Rollback left durable `restored` on resource rejection. Explicit original-profile
  `up` options repaired it with the checkpoint retained, and retry did not restore
  the bad settings again. Interrupted older acceptance with an already absent
  snapshot also completed after positive lookup.
- Cleanup was interrupted after the `deleting` journal, before unregistering.
  Both a terminating access error and a nonterminating provider error were injected
  at the inventory query. The VM remained registered and **every tracked file,
  directory and journal remained unchanged**. The same two faults were injected
  after a real unregister interruption, with the same preservation result. Only
  an unmodified successful inventory allowed final file/journal cleanup. Inventory
  comparison covers all paths/sizes and hashes the journal, seed and key files;
  Hyper-V-managed files are not opened for hashing while locked by VMMS.

Final fixture UUID **`57874542-1373-448d-b7cd-7d65da577038`** was removed; the final
helper SHA256 was
`2df4e3dc563dedf833cc2a385e779ed8d3831e2d489ca70661eb59ee80a7ed35`.
Its result flags all recovery and preservation gates true, with
`host_only=true` and `guest_update_sent=false`. The earlier complete fixture
`be334739-210d-4177-b52a-271bff442d2a` is also absent. Successful checks removed
their private installation directories.

The initial observation attempt tried to hash a VMMS-locked configuration file
and failed. Its `finally` cleanup removed UUID
`31770c62-d612-437e-80e3-48628ffc3667`, disks, seed, identities and journal; these
absences were verified with a successful inventory. Automatic approval review
rejected a later recursive deletion of its diagnostic-only directory with
“blocked by policy”, so helper scripts/requests remain privately at
`%LOCALAPPDATA%/ilex/builder-checks/maintenance-34136-1794932166491462100.partial/`.
No VM, disk, seed or key remains there. No unit tests were added or run.
