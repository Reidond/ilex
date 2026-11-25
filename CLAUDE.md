# ilex

Personal Fedora bootc image: a small base (fedora-bootc, ilex's own files and
drupe's signed-image runtime),
an ordered list of shells (GNOME by default, with desktop apps), then one
machine configuration. One `Containerfile` builds each root
image: base packages, shells' packages in their own transaction and image layer,
then ilex files and configuration, overlays, lint. A machine is one derived
layer on that root. Rust tooling, e2e-only tests on QEMU via bcvk.

## Build and test

```sh
ilexctl build              # podman build → localhost/ilex:<slug>, runs bootc container lint
ilexctl build --shell none # small base → localhost/ilex:<tag>.s-none (322 MiB growth budget)
ilexctl build --machine ms-7c35 # build the base, then its machine layer
ilexctl build --machine ms-7c35 --on <image> # only a machine layer on an existing image
ilexctl test ephemeral::   # build if needed, boot in QEMU, assert over SSH
ilexctl vm run                 # interactive ephemeral VM of this checkout's image (--gui: a window)
                               # on Windows: ephemeral VM on the builder, native window with --gui
ilexctl vm run --gui --print-image # show the selected image without building
ilexctl test installer::       # the next installer's root helper, on its live image (no ISO)
ilexctl info               # resource identity, selection and available configurations
```

- Every ordinary checkout uses a bounded readable name plus `--` and 32 hex
  digits of SHA-256 of its canonical owner (at most 48 characters). The owner
  includes both the canonical common Git directory and checkout path, so two
  clones' primary checkouts, same-named linked worktrees, and reused worktree
  names at new paths allocate different names. Receipts fence even a hash collision.
  The first primary checkout of `github.com/Reidond/ilex` automatically claims
  `main` (`localhost/ilex:main`, VMs `ilex-main-*`) in machine-wide tooling state.
  A linked-worktree call discovers and registers its actual primary through
  Git's common directory, so the normal primary is registered without opening
  that checkout or setting configuration. Later clones keep owner-qualified
  names. Windows uses `%ProgramData%/ilex/tooling/main-claims`;
  native Linux uses `/var/lib/ilex-tooling/main-claims`. Claims and releases are
  immutable, atomically published records. A missing checkout's claim is
  released only by the explicit command below. `canonicalCheckout` is no longer used.
  If machine-wide state is inaccessible to an unprivileged process, it keeps
  its owner-qualified namespace; metadata/build commands still work without
  sudo or setup. It can use `main` only after that claim can be established or read.
  The same slug selects `localhost/ilex:wt-<slug>` and VMs `ilex-<slug>-*`.
  State/runtime paths stay owner-qualified even for `main` (`main--<owner hash>`),
  so registering main never adopts old unreceipted directories or keys.
  Never use another checkout's image or VMs.
- `build`, `test`, `vm run` and `disk` accept repeatable `--shell <name>`: the
  first shell is the default. Flags override `ILEX_SHELLS` (comma-separated),
  which overrides `image.toml`; an empty environment variable uses the manifest.
  `none` alone selects the small base (`multi-user.target`), without desktop
  apps or enabled update notifications. Duplicates and unknown
  names fail before podman starts.
  A shell list or default different from the manifest adds `.s-<shell>` for
  each shell in order, or `.s-none` for none, to `main` or `wt-<slug>`.
  Slugs can contain `--` but never dots, so variants cannot collide with another
  checkout. Tags over 128 bytes are rejected (a 48-character slug leaves 77
  bytes for shell and future machine suffixes). Explicit `--tag`, `--image`
  and `ILEX_IMAGE` references keep their supplied names; selection still
  controls composition when building. `image_ref()` remains the default image.
  Never use another checkout's `<tag>.s-*` or `.m-*` images either. VM names and state
  directories keep the slug alone; installed disks are separate per image.
- VM boots take one of N machine-wide slots (`ILEX_VM_SLOTS`, default one per
  8 GiB RAM, max 4) and wait when none is free.
- `ilexctl test` builds an implicit image when missing or its selection is stale;
  after changing other content that ships in the image, pass `--build`.
  Without `--on`, `build --machine <name>` and `test --build --machine <name>`
  always build the generic base first, then build or reuse the machine layer.
  A fully cached base keeps its ID, so an unchanged machine layer is still skipped.
  `--on` deliberately uses the supplied existing base without rebuilding it.
  A missing or mismatched explicit reference must be built or pulled at that
  reference; building this checkout's implicit tag does not repair it.
  Filters are substrings and
  several run their union (`ilexctl test ephemeral:: upgrade::`); arguments after
  `--` go to the test binary. `ILEX_IMAGE=<tag or ref>` tests another image.
- Reusing an implicit image first requires `ilex.checkout-owner` to match the
  canonical checkout owner, then compares all three selection labels with the current
  manifests and flags. `test` rebuilds a stale selection; `vm run` and `disk` refuse
  it and print the selection it was built for and the build command to run.
  Explicit image references are compared only when `--shell`, `--machine`,
  `ILEX_SHELLS` or `ILEX_MACHINE`
  requests a selection; a mismatch is refused unless `test --build` was given.
  An explicit reference without selection keeps CI's existing behaviour.
  For a mismatched `ILEX_IMAGE`, the machine-layer hint offers `--build` only
  when that image names machine `generic`; machines cannot be stacked.
- An ephemeral VM has no ostree deployment, so `bootc status` there names no
  image; `ephemeral::` identifies the image through the container bcvk started.
- `upgrade::` installs a real libvirt VM (`bcvk libvirt run`, a few minutes,
  then cached per image under `/var/lib/libvirt/images`), so run it when
  touching the update path. v1 is the image under test unless
  `ILEX_UPGRADE_FROM=<ref>` names an older one (CI: the installer composition, or legacy `:latest` until its first publication);
  with `CI` set and no `ILEX_UPGRADE_FROM` the tier is skipped. v2 is the
  candidate plus a marker layer, tagged `<image tag>-upgrade`, never the
  checkout's own tag. A lifetime lease and allocation receipt fence the stable
  upgrade fixture alias (preserving bcvk's warm-up cache); explicit legacy v1
  remains unmodified and is never implicitly adopted.
  Ordinary upgrade VMs keep `uefi-insecure` firmware.
  Secure Boot under KVM requires SMM. Probe `KVM_CAP_X86_SMM` on the Linux host.
  On Windows, the separate Gen2 security tier qualifies enforcement.
  On v2 the same VM checks signature enforcement: v2's `ilex-sigpolicy.service`
  left the containers-storage origin alone at boot; the origin is pointed at
  the installer composition unverified (the state older ISO installs leave),
  and after a reboot the unit must have added the enforcement (a second run
  is a no-op). The agent must refuse a singleton `insecureAcceptAnything`
  default in the effective policy before changing the origin, including in
  dry-run mode. The VM exercises unsafe root-user, XDG and environment-override
  policies while `/etc` stays safe, their precedence and a conjunctive default.
  After enforcement, `bootc upgrade --check` must fetch and verify the tracked
  image without staging an update; only an unreachable ghcr skips that check,
  with a printed reason. Before the installer's composition is first published, CI explicitly
  sets `ILEX_SIGPOLICY_LEGACY=1` to check signed legacy `:latest` instead (the
  same absence check that selects legacy v1); local runs can opt in too.
  Then the rollback to v1. One VM, one install: installing is
  most of the tier's time. `ILEX_UPGRADE_PREPARE=1` only installs and boots v1
  (filling bcvk's base-disk cache) and stops; CI runs that ahead of time and
  caches the disk (see CI publishing).
- Every image ships an empty `/usr/lib/systemd/system/boot.automount` (systemd's
  supported zero-length unit mask, portable across Windows and Linux checkouts).
  GPT discovery otherwise creates an ESP/XBOOTLDR automount over OSTree's
  `/sysroot/boot` bind on ordinary `bootc install to-disk` installs with empty
  fstab, including bcvk. Its teardown can precede staged finalization and leave
  the old deployment booted (`Remounting /boot read-write: Invalid argument`).
  The vendor mask outranks GPT's `generator.late` unit; OSTree's `boot.mount`
  and Anaconda's explicit fstab mounts still work. Unlike `systemd.gpt_auto=0`
  or masking the whole generator, it preserves root, swap, home, srv and `/efi`
  discovery. A unit ordering drop-in does not remove the conflicting autofs
  mount. Upstream: bootc [#2402](https://github.com/bootc-dev/bootc/issues/2402)
  and [#2622](https://github.com/bootc-dev/bootc/issues/2622), OSTree's merged
  [#3690](https://github.com/ostreedev/ostree/pull/3690) (2026-11-12); Fedora's
  tested 2026.4 build still reproduces it. Reassess the mask after that fix is
  packaged and validated; do not backport its filesystem/mount logic locally.
- No test fixture adds a GPT-disabling kernel argument. `disk::` and
  `upgrade::` require the shipped vendor mask, an active real `/boot`, no
  autofs at `/boot`, no runtime mask and no global GPT opt-out. Upgrade checks
  again on v2 after its policy reboot and on a protected v1 after rollback.
  Compatibility applies only to an explicit `ILEX_UPGRADE_FROM` in
  `ghcr.io/reidond/ilex` whose booted `/usr` has no vendor `boot.automount`
  (neither a file nor a symlink). The booted digest must equal the locally
  installed image's manifest digest and verify with cosign v2 and the checked-in
  ilex public key. Content is authoritative: nightly rebuilds change digests,
  and revision labels/ancestry cannot prove which files an image contains.
  An existing malformed/nonempty unit fails the normal assertions.
  Only that signed, unprotected v1 has both boot units stopped, then receives a runtime `boot.automount` mask
  and a started `boot.mount` before staging, because its own old finalizer
  runs before v2 can supply the fix. Stopping the bind first is necessary:
  masking an active automount alone can leave dead autofs pinned underneath.
  Before any repair, a full run checks that the candidate ships the mask,
  including when `ILEX_UPGRADE_FROM` selects the candidate itself. `/run`
  disappears at reboot and both v2 checks still require the shipped mask with
  no runtime assistance. Prepare-only runs inspect/verify v1 without needing
  the not-yet-built candidate. Legacy rollback still checks the exact original
  deployment; only a v1 that qualified for repair skips the shipped-mask check
  after rollback. CI installs cosign before preparation and testing. CI's
  `v1-disk-boot-mask-<bcvk>-<digest>` profile uses the shared
  `.github/v1-disk-key.sh` and never restores the old kernel-argument profile.
  An absent `/etc/fstab` is valid on a direct-to-disk install: GPT discovery
  still runs and can generate the conflicting automount. Its absence in the
  diagnostics is not the shipped-mask assertion failure.
  Logs retain command line and mount state, plus prior finalization journal
  on failure. The defect predates the stack: fix it with the common shipped
  image, rather than a phase-0-only fixture workaround.
- `iso::` is opt-in and slow: `ilexctl test iso::` builds the network
  installer (`ILEX_ISO=<path>` reuses one with `compositions.json` beside it).
  `welcome` accepts the unknown machine's chooser default with Enter;
  `chooser` boots the MSI identity, checks rejected HTTP requests inside the
  VM, then selects generic with Shift+Tab, arrows and Tab before Enter.
  Both check the actual `%pre` choice over serial. `detect_board` checks the
  chooser's default using only the baseboard identity. `kernel_argument`
  edits GRUB with real key input on the untouched ISO and asserts the chooser
  was skipped; the OEMDRV `install` trial also asserts that it was skipped.
  GRUB needs 100 ms key holds; vmctl's fast desktop typing drops characters
  there. The harness waits for its menu countdown, stops it before reading
  the ISO with xorriso, then waits for the editor's final command to be drawn
  before sending cursor movement and text. The earlier menu instructions
  appear while GRUB is still drawing and cannot establish input readiness.
  That trial uses a USB keyboard: Ubuntu's OVMF drops navigation/modifier
  input from `virtio-keyboard` in GRUB even though Linux accepts it normally.
  The chooser HTTP probes are enabled only by the harness's public
  `ilex.chooser.self-check=1` systemd credential (SMBIOS), then the person-facing
  form is still submitted with real key events.
  Lifecycle trials close Firefox with Ctrl+Shift+W and Enter at its quit
  confirmation, crash it, delay real
  browser/compositor teardown by four seconds, and force escalation of a hung
  teardown. The latter faults use the fixed public `ilex.chooser.lifecycle`
  SMBIOS credential (`crash`, `delay`, `hung`); there is no HTTP control API.
  Normal/fault handoffs check `ILEX-CHOOSER-HANDOFF` before `%pre`: the actual
  old PID identities and logind session are absent at Anaconda's ExecStartPre.
  `chooser_serial_unavailable` and `chooser_serial_backpressure` exercise
  the shipped diagnostic path with a missing sink and a private PTY stopped
  by `tcflow(TCOOFF)`. Fixed public SMBIOS credential
  `ilex.chooser.serial-test=unavailable|backpressure` selects these faults.
  A second virtual UART observes server/kiosk readiness, HTTP probe completion,
  the teardown gate and `%pre` independently of the faulted sink; each actual
  mirror must report ENOENT/EAGAIN and finish in under one second. QMP
  screenshots record the chooser and the resulting stock Anaconda page.
  `install` installs onto a blank disk and checks the exact registry digest,
  enforcing origin from first boot, sigpolicy's already-verified journal,
  `bootc upgrade --check`, setup handoff, system `running`, no failed units
  and GDM. These assertions test the published image, not the branch image.
  `ILEX_ISO_MACHINE=ms-7c35` supplies that machine's SMBIOS identity;
  `ILEX_ISO_EXPECT=generic.gnome` pre-answers via the kickstart choice file.
  Build locally with `ilexctl iso --testing-tag latest` before composition
  tags exist: the selected composition stays visible, while every entry
  installs signed legacy `:latest`. This flag is forbidden whenever `CI` or
  `GITHUB_ACTIONS` is set. CI supplies `ILEX_ISO_DIGEST`; registry resolution
  and exact booted/tracked comparisons are mandatory for the install trial.
  `ILEX_ISO_TARGET`, when supplied, must agree with the ISO catalogue.
  `ILEX_ISO_INSTALL_TIMEOUT` overrides the install's default 1800-second limit
  for a heavily contended host (positive seconds, e.g. `3600`); it does not
  change chooser teardown deadlines or any installed-image assertion.
  Screenshots and serial logs land in `<state dir>/artifacts/iso/`: the settled
  unknown-machine chooser (`01-chooser.png`), detected/changed choices
  (`03-chooser-detected.png`, `04-chooser-generic.png`), web welcome
  (`02-welcome.png` after accepting the chooser), graphical
  unattended progress (`06-installing.png`) and first boot (`07-gdm.png`).
  Welcome timing recognises the web page's blue header and light content;
  progress capture keeps light, nonblank pages (progress hides the header).
  Selection assertions still read serial, not pixels. The graphical kickstart
  uses `Etc/UTC`: WebUI 90 rejects bare `UTC` as an incomplete timezone.
  The frozen legacy `:latest` predates the `reject` policy default: its local
  testing-tag install boots with an enforcing origin and passes the handoff/GDM
  checks, but `bootc upgrade --check` refuses its old insecure default. Keep
  that assertion; testing the fixed published composition awaits the stack's
  first publication. A passing candidate-image CI run does not turn that
  legacy-payload installation into a passing update check.
- Publishing (`ilexctl publish`) and the installer release only happen in CI.
  Never push to ghcr or create releases from a dev machine.

## Linux security qualification

Phase 0 PR 18 adds the reusable `ilex-enroll` library/CLI, Anaconda's local
end-of-install recovery/enrollment step and generic initramfs unlock plumbing.
The implementation and exact-ISO gates are in
[`docs/installer-enrollment.md`](docs/installer-enrollment.md). Enrollment
stays pending until a later boot supplies direct method evidence; PR 19 owns
verification/repair. `security::encrypted_install` requires T6 qualification
for its exact 6 GiB/two-vCPU profile and tests a disposable signed local
candidate. It must not be reported as a passing released-image install.

`security::upgrade_rollback` uses the same exact-ISO encrypted fixture and
checks that each deployment boots the expected kernel and initramfs. It tests
unattended and recovery-key boots, the LUKS header, authenticated homed data
and an edited file from the selected home profile on each deployment. CI pins
and verifies the published installer composition from its pre-promotion
snapshot, or the previous distinct signed composition for an ISO-only refresh.
Installer release always requires a fresh encrypted install of the verified
published digest and also requires the lifecycle when a baseline exists.
The gate seals the required evidence before upload.
See the [encrypted lifecycle runbook](docs/encrypted-lifecycle.md) for the
CI baseline, native Gen2 console fixture, and owner-run MS-7C35 qualification.

The installed `ilex-security-verify.service` runs `ilexctl security verify`
to record pending, verified or error from this boot's root unlock evidence.
After passphrase or recovery-key fallback, `sudo ilexctl security repair`
requests a disk credential on local tty8. It defers without a prompt when
Secure Boot or TPM support is unavailable. A replacement is pending until
a subsequent boot proves automatic unlock. The encrypted QEMU tier also
interrupts repair between actual LUKS keyslot and token writes.

PR 17's consumer checks and backend requirements are documented in
[`docs/security-probes.md`](docs/security-probes.md). Guest probes also require
kernel lockdown, the selected manifest digest and all three ilex composition
labels after each boot. The common image's signed in-tree amdgpu module must
load under lockdown with no unsigned-module taint (including RX 9070 XT PCI
alias coverage); this does not qualify physical GPU rendering. These checks
use the existing `VmControl` interface and T6 qualification records. Native
Windows uses the [Hyper-V Gen2 security provider](docs/tooling/windows-gen2.md)
with the same profile and capability types.
`ilexctl doctor security --require-qualified --memory 2G` asserts an existing
exact-profile record without starting a VM; `--self-check --memory 2G` measures
that profile. Failures retain an unqualified result with the failing stage,
distinguishing platform provisioning from guest failure. No installer,
encryption or TPM-enrollment behavior is added.
Unsigned EFI controls remain in the platform stage through provisioning and
QEMU launch and retain startup logs on failure. CI's
`.github/security-control-startup-self-check.sh` boots real ilex through the
restart checks, then injects an EFI-control-only QEMU startup failure and
requires platform/unqualified reporting. Normal qualification runs afterward
to restore a passing profile; `on` selects the second control for local checks.

`ilexctl doctor security` reports this Linux host's KVM SMM capability and
security prerequisites. Its result is explicitly `not_yet_qualified` until
the booted-guest tier passes. `ilexctl doctor security --self-check` runs
`security::firmware` against this checkout's image (`ILEX_IMAGE` can select
the CI candidate). Secure guests require KVM with SMM; there is no software
emulation route, and a failed KVM/SMM probe is unqualified. On Windows the security
subcommand selects native Gen2 and accepts the exact signed installer via
`ILEX_ISO`.

The shared `VmSpec` remains the contract for part 4: `secure_boot=required`
and `tpm=persistent { bundle_id }`, with a writable owned disk, UEFI and no
external `firmware_state`. Native acceleration (KVM's `host` CPU) is the only
accelerator. Existing ordinary VMs
keep their nonsecure firmware and no TPM. Secure requests select a distro
firmware descriptor declaring Microsoft-enrolled keys, Secure Boot and SMM,
and require the matching CODE/VARS pair's combined 4 MiB layout. Fedora's
qcow2 pair and Ubuntu's raw pair are converted into private raw files.
Ordinary `ILEX_OVMF_CODE/VARS` overrides cannot qualify security requests.

`VmCapabilities.provisioning` reports prerequisites separately from effective
`features`. SMM/firmware/executable checks can make provisioning available;
they never make `FirmwareEnforcement` or `PersistentTpm` effective. Those
require a passing `qualification` for the exact `SecurityProfile`: accelerator,
RAM/vCPUs, CPU model, guest architecture, host boot/machine/CPU/kernel/backend
evidence and firmware/template fingerprints. Host identity is hashed. Evidence
lives under `<state>/security-qualification/<profile hash>/record.json` and
contains the run, artifact path and outcome (`unknown`, `qualifying`, `passed`
or `failed`). A new measurement replaces a prior pass with `qualifying` before
launch; interruption/failure cannot preserve that pass. This is local test
evidence, not remote attestation or an encrypted-install qualification.

Secure requests default to `security_launch=require_qualified` and fail before
launch for unknown/failed profiles. A harness explicitly sets
`security_launch=qualification_fixture` to measure any requested resource size
while still requiring provisioning support. This flag never promotes status.
There is no universal RAM ceiling: 2 GiB evidence does not qualify 4/6 GiB,
and two-vCPU evidence does not qualify another CPU count. PR17 consumes these
shared types and `ilex_devkit::qualification` records rather than a second
capability contract. Old metadata without evidence remains unqualified.

vmctl reports live QMP `runtime` separately. A paused, stopped, failed or
unreachable guest never exposes effective security capabilities as available,
even with historical passing profile evidence. An SMM `internal-error` is a
failed runtime. Resume can restore a passing profile only when QMP is running;
a stored failure remains failed until a new explicit qualification succeeds.

Disk, CODE, VARS and manufactured SHA256 swtpm state form one lifecycle:
`<state>/security/<bundle_id>/bundle.json` binds owner/run/purpose/disk and
policy, checks the CODE hash, and refuses missing TPM state or a changed
policy. A lock prevents concurrent use. `stop` preserves identity; `start`
reuses it; `destroy_owned` removes the complete bundle after stopping both
processes. Keep the persistent disk alongside it. Installer tests can retain
the same disk and bundle across boots. Policy changes/fresh TPM negatives
require separate disposable fixtures. No host TPM is passed through.
An interrupted manufacture fails closed and retains its directory for
diagnosis; remove only that disposable fixture after its owning process ends.

`security::firmware` boots Fedora shim from a real installed disk (no direct
kernel boot), checks SB=1/SetupMode=0/TPM2, and replays SHA256 PCR7 from the
binary event log. It requires meaningful policy and authority events. A
public test credential encrypted to this TPM/PCR7 must decrypt after both
warm reboot and a full QEMU/swtpm stop/start. An unsigned generated EFI
application must emit its serial marker with SB off, then receive an explicit
firmware rejection with SB required. Both controls use identical payload,
secure CODE and enrolled VARS template. The off copy changes only the
`SecureBootEnable` boolean, retaining all enrolled keys. When the factory
template omits that boolean (Ubuntu), it is added with value zero rather than
replacing the enrolled key store. Manifest hashes record the common firmware
and the private off-state change. The tier uses 4 GiB and two vCPUs; each SSH/control deadline is 420s.
This does not qualify encrypted installation,
desktop performance or the unresolved 4 GiB nested Hyper-V builder failure.
`ILEX_SECURITY_MEMORY=2G` selects an explicit smaller diagnostic fixture;
timing evidence records its actual memory. It does not qualify a 4 GiB VM.
The 16 November 2026 WSL run also reproduced a 4 GiB KVM SMM entry failure
after shim/GRUB (`hardware error 0xffffffff`, QMP `internal-error`). A host
SMM capability of 1 alone is therefore insufficient. Entry failures fail
early; neither SMM nor Secure Boot is disabled to continue the test.
Disk construction uses bcvk's KVM installer. Missing disk-construction support
fails as unqualified.
For immutable `sha256:...` image IDs, disk construction gives bcvk an
ID-verified checkout-owned local alias: its inner containers-storage transport
otherwise parses the ID as a registry name. The disk key remains the original
immutable ID. The alias retains only an additional local name for the existing
image; no registry upload or extra image contents are created.

CI runs the tier on the tested candidate, probes the actual runner each
time, requires KVM with SMM, and enforces a 45-minute guest-tier deadline.
Failures/timeouts are unqualified and fail the build; an unchanged image's
unrun tier is labelled **UNQUALIFIED** in the summary. `test=true` forces it.
A runner without KVM SMM, or a KVM entry failure, leaves the tier
unqualified; nothing retries with another accelerator.
Serial logs, firmware identity, versions, PCR replay, timing and cleanup
evidence are retained under `artifacts/security/` (CI: `security-evidence`).
Startup failure collection authorizes the fixture's runtime path independently
of a running VM handle. It copies `swtpm.log` and `swtpm-setup.log` before
cleanup/retry; manufacturing captures both stdout and stderr, and emulator
errors include their exit status. `swtpm_setup --version` is in the host probe.

On disposable GitHub runners, the security step unloads only Ubuntu's
`usr.bin.swtpm` AppArmor profile, whose allowed libvirt socket paths omit the
private ilex runtime. It retains the packaged profile, loaded-profile lists
before/after and relevant kernel denials. This CI preparation changes no local
host policy and still requires the manufactured TPM2 state and SHA-256 bank.
Before guest qualification, `.github/security-startup-self-check.sh` runs the
real `security --self-check` path with emulator/setup failure injection. The
emulator case first manufactures real SHA-256 state, then exits before creating
its socket. Both cases must retain distinctive stdout/stderr in the uploaded
logs and stay unqualified; a QEMU launch is refused by the fixture. Temporary
state stays outside the artifact tree and is removed. These startup fixtures
have a 12-minute total deadline; the alias self-check has five minutes.
TPM state and credential files are not uploaded or cached. Existing apt/disk
caches and the GPT automount workaround retain their behavior.

## ilexctl

The image ships the machine build of `ilexctl` from `crates/ilexctl`
(`cargo build --release -p ilexctl --no-default-features`). It composes the
machine command library and supersedes the previous machine commands below.
Units keep their names and switch together with the binary on upgrade/rollback.
Drupe remains its own command, with its service, setuid helper and state unchanged.

| Previous machine command | Machine command |
| --- | --- |
| `ilex-agent --self-check` | `ilexctl system --self-check` (also `system check`) |
| `ilex-agent config [--json]` | `ilexctl system config [--json]` |
| `ilex-agent enforce-sigpolicy [--dry-run]` | `ilexctl system enforce-sigpolicy [--dry-run]` |
| `ilex-agent home-create NAME ...` | `ilexctl home create NAME ...` |
| `ilex-agent home-label UID` (internal) | `ilexctl home label UID` (hidden) |
| `ilex-agent profile ...` | `ilexctl profile ...` |
| `drupe install strace.raw` | `drupe install strace.raw` (as root, then just `strace …`) |
| `drupe list` | `drupe list` |
| `drupe remove strace` | `drupe remove strace` |
| `drupe run strace -V` | `drupe run strace -V` |

Homed creation/labeling stay in Rust. Public profile operations, including
`prune-backups`, retain the internal Python implementation; clap validates the
public command tree before dispatch. The updater and notifier retain bootc's
units and their internal notification script.

Help groups the machine commands under “On an ilex machine”. `--self-check`
belongs to a group (`system --self-check`, `profile --self-check`), with `check`
as the equivalent verb. `--json` is per command; `system config --json` emits
one document with top-level `schema: 1`, preserving `image` and `machine`.
Exit codes are 0 for success/no-op/dry run, 1 for failed operations/checks,
and 2 for invalid usage. Errors use stderr. Drupe's exit-code behaviour is unchanged.
`ilexctl completion bash|zsh|fish|powershell` generates static completions;
the image installs bash, zsh and fish scripts under their usual `/usr/share`
completion directories. `ilexctl --version` identifies the machine build.

Install the default dev build with `cargo install --locked --path crates/ilexctl`.
Use it from native PowerShell on Windows. For checkout commands, the installed
`ilexctl` selects the checkout's Cargo manifest and keeps the caller's working
directory, including for relative paths. `ILEX_ROOT` can supply a checkout when
called from outside one. Inside a checkout it must match that checkout.
`cargo ilexctl` runs checkout commands without installing the dev build.
`builder`, `runtime` and `identity` also work outside a checkout. In a
checkout, `ilexctl --version` prints the dev flavour and the checkout slug.

VM controls and `mcp` run from the executable you start and can attach by
`--slug` and `--name` (also `--vm`). For `vm run`, `--slug` must match
the current checkout and `--name` selects the VM purpose. Use the installed
copy for a long-running MCP server, so Windows can relink
`target/debug/ilexctl.exe` during builds.
The MCP tool names and schemas stay the same.

| Previous dev command | Dev command |
| --- | --- |
| `cargo xtask build\|rechunk\|changed\|prepare-publish\|publish\|disk\|iso\|test …` | `ilexctl <same verb> …` |
| `cargo xtask vm …` | `ilexctl vm run …` |
| `cargo xtask security …` | `ilexctl doctor security …` |
| `cargo xtask tool NAME [PACKAGES…]` | `ilexctl drupe build NAME [PACKAGES…]` |
| `cargo xtask apps --check` | `ilexctl doctor apps` |
| `cargo xtask info\|identity\|doctor\|builder\|runtime\|remote\|migrate …` | `ilexctl <same verb> …` |
| `ilex-vmctl list\|display\|self-check\|capabilities\|screenshot\|click\|move\|scroll\|type\|key\|wait-screen …` | `ilexctl vm <same verb> …` |
| `ilex-vmctl mcp` | `ilexctl mcp` |

On non-Linux hosts machine commands explain that they require an ilex machine.
Help and completions work on both builds. A dev build on an ilex host hands
machine commands to `/usr/bin/ilexctl`.

## Configurations: shells, machines and users

`image.toml` names the installed shells and one default when the list is
non-empty. `packages.toml` is the base's package list,
installed first in every root image; `systemd-container` supplies the
`systemd-dissect` command Drupe needs even without a shell. Flatpak also stays
in the base so switching away from a shell can remove its old preinstalled
apps from persistent `/var`. The base transaction omits weak dependencies;
Flatpak's recommended desktop portal belongs to a shell. `shells/packages.toml`
contains libnotify and the PackageKit excludes, installed only
with a shell. These and all selected shells' packages share a second transaction
and image layer, with their combined excludes. A shell's excludes cannot prevent
installation of a base package. The machine remains the last, squashed layer.

`ilexctl build` checks every root built without shells against the local
fedora-bootc image using Podman's uncompressed `.Size`. The shell-free build
with headless Flatpak, regenerated SELinux policy and the crypt/TPM initramfs
measured 2,334,252,793 bytes against Fedora bootc's 2,014,310,965 bytes, an
increase of 305.12 MiB. The **322 MiB** budget rounds that growth up to the
next whole MiB and adds 16 MiB for Fedora updates. Keeping the Flatpak client
and timer allows preinstall cleanup after a shell-free switch. App payloads
remain in `/var`.
The error includes both sizes and removes only the rejected build's tag, so a
later test cannot reuse it. Packages that exceed the budget belong in a
shell or machine, or need a justified budget change in a commit. The base adds
ilexctl, signature policy, the updater and drupe, plus the latter's
`systemd-container` runtime dependency. A package-free base boots, but cannot
install or run drupe tools: `tool::` checks this on both kinds of root.
The base excludes exactly systemd-container's three QEMU recommendations:
`qemu-kvm-core`, `qemu-device-display-virtio-gpu` and
`qemu-device-display-virtio-vga`. Fedora's existing `qemu-user-static-*` packages
are unaffected; shells and machines may install the excluded VM packages in
their own transactions. GNOME does not request the file-less `qemu-kvm-core`
metapackage merely to preserve an RPM count.
Measured on 2026-11-15: the base is 1,957.11 MiB against fedora-bootc's
1,917.59 MiB (39.52 MiB growth), with 543 `rpm -qa` entries against 540:
`systemd-container`, its recommended `systemd-import-keys`, and Fedora's
imported signing key. No desktop package is requested by the base.
The notifier's three small files remain in `rootfs/`, but neither user unit is
enabled globally without a shell: no session means no notification is attempted.

| Thing | Installed | Default | Chosen by a person |
| --- | --- | --- | --- |
| Base | every image: fedora-bootc, ilex plumbing, `packages.toml` | always present | not selectable independently of ilex |
| Shell | per image, in `image.toml` | per image, `default-shell` | at sign-in, from the greeter's sessions |
| Machine configuration | per composition tag (`generic.gnome`, `ms-7c35.gnome`) | installer DMI match, otherwise `generic` | installer kernel override; later `bootc switch` |
| User profile | public catalogue in every image | no selection | explicit apply after authenticating into a daily home |

### Compositions

`image.toml` separately declares the local default composition, the installer's
composition (`installer`) and the published shell lists (`[[publish]]`). Each
entry has `shells` (ordered, first is default) and a non-empty `machines` list.
With shells, `generic` is required: it is the root of every machine layer in
that entry. Without shells, `generic` is forbidden because that root is already
the implicit `base`. Shell lists and machine names cannot repeat within their
respective lists; ordered shell lists cannot repeat across entries. `installer`
must name a published `generic` composition with at least one shell.

Published tags describe their contents: `base` for generic without shells,
`<machine>` for another machine without shells, otherwise
`<machine>.<shell>[.<shell>...]`. Every tag `X` has `X_YYYYMMDD` and
`X_sha-<7 hex>` companions; main stages roots unsigned as `candidate.X`. All derived
tags, including the longest suffix, must fit 128 bytes. The default shell must
be first for an image to have a published tag. The three selection labels are
the authority for the image's name. Publications add no image label or file.

| Composition | Local tag (`T` is this checkout's tag) | Publication tag | Build | Rechunk |
| --- | --- | --- | --- | --- |
| Base | `T.s-none` | `base` (always listed) | root | yes |
| Base + GNOME | `T` | `generic.gnome` | root | yes |
| Base + GNOME + ms-7c35 | `T.m-ms-7c35` | `ms-7c35.gnome` | layer on `generic.gnome` | no |
| Base + ms-7c35 | `T.s-none.m-ms-7c35` | `ms-7c35`, only if listed with `shells = []` | layer on `base` | no |
| Base + GNOME + ilex (future) | `T.s-gnome.s-ilex` | `generic.gnome.ilex`, once listed | root | yes |

`ilexctl info --publications` validates the complete list before printing
TSV without a header: tag, kind (`root` or `layer`), parent tag (`-` for roots),
machine name, comma-separated shells (`-` for none). All roots precede layers.
`ilex_devkit::config::publications` exposes these five fields for CI, plus
machine descriptions/match rules and shell descriptions for the installer
catalogue. Its `selection`
field resolves all three expected labels from the same manifest snapshot.
`ilexctl info --installer` prints the validated installer tag.

CI builds and publishes every root, including `base`, and its listed machine
layers. Removing a publication
stops that tag updating. No machine tracks a published tag yet, so names can
change freely for now. Once the first machine is installed, switch it before
removing or renaming its tag (including reordering shells). A tracked tag must
keep moving, and its visible composition must stay the one its person chose.

A shell is `shells/<name>/shell.toml`: its name and description, packages,
Wayland session file (without `.desktop`), compositor command, optional greeter
unit. The default shell owns the greeter; GNOME uses GDM.
To add a shell, create its directory and manifest, optionally add a `rootfs/`
overlay, then name it in `image.toml`. Overlays are add-only: they follow the
packages and common `rootfs/` in shell list order, and may share real directories
but never replace a file, symlink or other entry, even with identical content.
Directory/non-directory conflicts and paths through any symlink (including
`/bin` and `/lib`) are errors. Leaf symlinks may be added at new paths.
`/usr/share/ilex` is reserved for generated files and forbidden to overlays.
Dynamic-loader configuration is also forbidden: `/etc/ld.so.preload`,
`/etc/ld.so.conf` and `/etc/ld.so.conf.d/**` (including the directory itself).
A shell must provide a real session and compositor.

Add-only is a visibility rule, not containment. An overlay can ship a service
or other code that runs as root on the installed machine; review every added
path accordingly. New units, drop-ins and masks (including a new leaf symlink
to `/dev/null`) are additions and can change existing units' behaviour. They
require review just as executable files do; add-only only prevents replacing
existing filesystem entries. The build must not execute that content to produce changes
absent from the overlay diff. All configuration runs before overlays, and only
`bootc container lint` runs afterward.

The final stage copies each overlay to temporary staging by content and runs
`build/image_tree.py` against the real image; the interpreter removes its own
staging without launching another command. Final-stage copies use ordinary
`COPY`, since `COPY --link` replaces ancestor directory metadata. A build guard
requires `/tmp` and `/var/tmp` to remain `1777 root:root` in the image itself
(a VM's `tmp.mount` hides the underlying `/tmp`). Generated files
have separate staging and use directory descriptors with
`O_NOFOLLOW`, including at the leaf. `/usr/share/ilex/.generated.json` records
generated file hashes: a later layer may replace only an unchanged, registered
generated file, and registry keys use the same strict path normalization. Base
installation uses `--base --phase generated` and requires `/usr/share/ilex` to
be absent before generating anything; it never adopts a package's registry.
The same installer accepts machine overlay payloads and permits
replacing a registered `machine.json` without granting overlays replacement rights.
Final-stage Python runs as `python3 -I -S`: the helpers use only the standard
library, and neither added modules nor site-packages `.pth`/sitecustomize hooks
may run during image construction. Later machine builders must also run their
package transaction and configuration against an overlay-free filesystem, then
replay shell overlays followed by machine overlays last; starting dnf atop the
finished shell image does not meet this rule.
An explicit replacement list in a shell or machine manifest is the extension
point for a future need such as greetd's packaged `/etc/greetd/config.toml` at
M5. No replacement list or escape hatch exists today.

Package `install` entries are literal Fedora package names or `@` plus a stable
comps group ID: ASCII letters, digits and `-._+` (an alphanumeric or underscore
first). URLs, paths, `.rpm` filenames, options and xargs quoting are rejected.
`exclude` also permits DNF's `*`, `?` and bracket/range/negation globs. The dnf
transaction enables `localpkg_gpgcheck=1`. Human-facing manifest strings reject
all C0, C1 and DEL control characters.

The future ilex shell owns its design: tokens, colors and other visual choices
belong to its code, and its components read their own tokens. Keep that code
generic so a future design is a token change inside the shell. Compositions
do not choose or install a design system, and shells declare no theme targets.
GNOME continues to draw itself with Adwaita.

`shells/ilex/design/` holds source material for that shell, not an installed component
or an image/installer input. It includes the owner's complete Claude export
titled "ilex": Breeze frames, Adwaita interiors and holly green, with light and
dark tokens. Change values in that artifact and replace `tokens.json` with the
whole export; never edit it by hand. `.gitattributes` disables text conversion
for `shells/ilex/design/tokens.json`, preserving its exact bytes and original line endings.
Without `shell.toml`, `shells/ilex/` is not selectable and is excluded from the build context.
Shell discovery skips a manifestless directory only when its sole entry is a
real `design/` subdirectory. Any other directory without `shell.toml` is an error.

Machines live in `machines/<name>/machine.toml`; `generic` is the base image
with no packages or overlay. Add a machine by creating one
directory: `name` must equal its basename, `description` is required,
and optional `[packages] install` and `exclude` use the same
grammar as shell packages. Machine names cannot be `latest`, `candidate`, `main`,
`rechunked`, `upgrade`, `base`, or start with `candidate`, `sha`, `wt-` or a digit.
Optional `[[match]]` tables describe installer DMI detection. Each has a
complete `sys_vendor`/`product_name` pair or `board_vendor`/`board_name` pair
(all four are allowed), compared exactly and case-sensitively after stripping
firmware whitespace. Values have 1–128 characters, no control characters or
surrounding whitespace; firmware placeholders are refused. `generic` and `vm`
cannot have match rules. `ilexctl info` and `iso` reject identical/subset rules
across different machines. Runtime conflicts fall back to the default with
the conflicting names reported. Match metadata stays out of `machine.json`.
These are the only machine manifest keys; `publish` and other unknown keys
remain errors.
An optional `rootfs/` only adds paths against the **whole base image**, including
existing package/configuration files and shell overlay files. Those files cannot
be replaced; new drop-ins, masks and units remain additions a reviewer must read.
Put kernel arguments in `rootfs/usr/lib/bootc/kargs.d/20-<name>.toml`, using
bootc's `kargs = ["..."]` format; bootc applies the difference at install,
upgrade and switch. Other machine defaults (udev, sysctl, unit drop-ins) go in
`rootfs/usr/` too. No replacement list exists.

`build`, `test`, `vm run` and `disk` accept `--machine <name>`, overriding
`ILEX_MACHINE`, then `generic`. Native Windows `vm run --gui` installs the
selected composition with a disposable development overlay. `--print-image`
reports the selected reference without building or booting.
The implicit tag is `<checkout tag>[.s-<shell>...].m-<machine>`. A layer built
on an explicit base, including a digest or another repository's image, always
gets a local tag in this checkout's namespace: `<checkout tag>.b-<16 hex SHA-256
of base reference>.m-<machine>`. This prevents overwriting another checkout's
layer and lets distinct bases coexist. A base already in this checkout's own
shell-variant namespace keeps that prefix. Only an explicit build `--tag`
chooses another output name. Both suffixes count towards the 128-byte tag limit.
`ilexctl build --machine <name> --on <image>` builds only the layer and prints its
resulting full reference on stdout; scripts must use that result rather than
append a suffix to their input. Build subprocesses (including base builds and
pulls) send stdout to stderr, so command substitution captures only the reference:

```sh
image=$(ilexctl build --machine "$machine" --on "$base")
```

No image is pushed by building.

Machines do not stack. The host builder requires the base's `ilex.machine` label
to be `generic`, and preparation independently requires its unchanged registered
`machine.json` to name `generic`. Both errors name the base's machine. A future
explicit stack would need a tmpfiles file per machine, the stack recorded in
`machine.json` and a label such as `ilex.machines`, and configuration/self-check
coverage for every layer. None of that extension is implemented.

`machines/Containerfile` reconstructs an overlay-free preparation filesystem
from composer-owned, hash-registered `/usr/share/ilex/.overlays.json` provenance.
Before launching dnf it verifies inherited additions (contents, links, modes and
owners), stages them and removes only those additions. Package installation and
configuration then run without overlay code. It replaces `machine.json` only
when the generated-file registry still matches, replays inherited overlays in
order, and installs the new machine overlay last against the real filesystem.
Only lint runs afterwards. A base without registered provenance must be rebuilt.
The machine build squashes its new build steps into one layer, preserving all
base layers. `ilex.machine-key` covers the base image ID and every path, type,
mode, link target and byte of the machine directory and shared build helpers;
an unchanged key skips the build, preserving its image ID and installed disk.
Machine fingerprints include that key too, covering the actual supplied generic
base's content even when its manifests are outside this checkout.
Machine layers set `org.opencontainers.image.revision` to the commit that built
the layer, including when the generic base was published by an older commit.

The local opt-in `config::machine_fixture` tier builds and boots a throwaway
machine outside the repository with a fresh marker and a kargs file. An internal
fixture builder adds a shell-style parent overlay with the production installer
while retaining generic's labels and registered `machine.json`; it does not
relax machine admission. The guest checks the parent's new directory, file mode,
content, relative symlink and file in an existing directory after verify/remove/
replay. A colliding machine overlay must fail the build. The tier also checks
configuration, self-check, and that the machine tmpfiles file exists and passes
`systemd-tmpfiles --dry-run --create /usr/lib/tmpfiles.d/ilex-var-machine.conf`
in the guest without making filesystem changes. A deliberately invalid mode
in a separate guest tmpfiles file must make the same dry-run command fail.
Its marker lives under
`/usr/share/ilex-config-fixture`, outside the generated-file namespace. CI does
not register this tier.

`build/compose.py` is the schema authority (Python standard library only).
Unknown keys and invalid defaults fail in the first `compose` stage, before
compilation. Plain `podman build .` composes the default image too, but does not
set the configuration labels: build with `ilexctl build` to test or rechunk
the result.
`ilexctl info` honours `ILEX_SHELLS`, listing the resolved image, selection
and available configurations; builds
record them in the three `ilex.shells`, `ilex.default-shell` and `ilex.machine`
labels.
The fingerprint covers the selected manifests, overlays and build scripts;
old images without configuration labels must be rebuilt.
Fingerprint v3 includes `shells/packages.toml` only with a shell, and excludes
`image.toml`: the three selection labels already cover its resolved selection.
An edit to an unselected shell or to `shells/ilex/design/` does not change the base's
fingerprint. An agent or `rootfs/` edit reruns no package transaction.

The image records schema 1 JSON in `/usr/share/ilex/image.json` and
`/usr/share/ilex/machine.json`. `ilexctl system config` prints
the selection; `ilexctl system config --json` returns both documents. The system
self-check validates the configuration, sessions, greeter, target and
machine packages, plus the literal non-group entries of `packages.install` and
`shell-packages.install` recorded in `image.json`. With no shell, the empty
shell-package/session checks pass and the target must be `multi-user.target`;
the OS, bootc, install config, signature policy and drupe checks still run.
`ephemeral::configuration` compares both package lists with the checkout,
alongside labels and overlays, and requires design-system metadata, labels and
the `/usr/share/ilex/design-systems` directory to be absent from every image.
A shell-free generic base must have
no sessions, display manager or Firefox; a machine may add those packages.
Flatpak and the root apps timer remain available for transition cleanup.
Every shell-free image must omit globally enabled notifier
units. Images with a shell must enable both notifier units and have none of the
literal packages excluded by `shells/packages.toml` installed, even through the
base or machine transaction. Tmpfiles are per transaction: `ilex-var.conf` for base
packages, `ilex-var-shells.conf` for shells, `ilex-var-machine.conf` for a
machine; an empty transaction emits no file.

### This PC: `ms-7c35`

The name is the board's model number, lowercased: DMI's system product is
`MS-7C35` and its board product is `MEG X570 UNIFY (MS-7C35)`.

| Part | Hardware |
| --- | --- |
| Board | MSI MEG X570 UNIFY (MS-7C35) |
| CPU | AMD Ryzen 9 5900X, 12 cores, 24 threads |
| Memory | 2 x 32 GiB DDR4-3200 (64 GiB) |
| GPU | AMD Radeon RX 9070 XT, PCI `1002:7550` |
| Disk | Corsair MP400, 2 TB NVMe |
| Display | AOC U27G3X, 3840x2160 at 160 Hz, EDID size 60 x 34 cm (27.2 inches, 162 DPI) |

`machines/ms-7c35/machine.toml` adds `steam-devices`, `gamemode`, `gamescope` and
`mangohud`. `[apps] flatpak = ["com.valvesoftware.Steam"]` preinstalls Steam
system-wide from Flathub; its payload lives in `/var/lib/flatpak`, outside the
image. The host's `steam-devices` udev rules give it controller
access, and Flatpak clients reach the host's GameMode daemon through the
GameMode portal.

While an opted-in game runs, GameMode requests the performance CPU governor,
disables split-lock mitigation where supported and inhibits the screensaver.
Fedora leaves CPU niceness unchanged (`renice=0`). Its `ioprio=0` tunes clients
at the default best-effort I/O priority 4 to priority 0; it does not disable
I/O tuning. The governor and split-lock helpers need membership of `gamemode`.
Add the login user once, then sign out and back in before starting games:

```sh
sudo usermod -aG gamemode "$USER"
```

The image supplies the group and packaged polkit rule; membership remains
per-machine state in `/etc`.

The host's Gamescope and MangoHud serve native programs or wrap the whole
Flatpak Steam application; they do not supply commands or Vulkan layers inside
its sandbox. For a fullscreen wrapper on this PC's 4K 160 Hz display, with
adaptive sync and the host's `mangoapp` overlay:

```sh
gamescope -f -W 3840 -H 2160 -r 160 --adaptive-sync --mangoapp -- flatpak run com.valvesoftware.Steam
```

For per-game use inside Flatpak Steam, install the Flathub VulkanLayer
extensions. Their branch must match Steam's installed runtime, shown by
`flatpak info --show-runtime com.valvesoftware.Steam` (currently `26.08`):

```sh
flatpak install --user flathub \
  org.freedesktop.Platform.VulkanLayer.gamescope//26.08 \
  org.freedesktop.Platform.VulkanLayer.MangoHud//26.08
```

Replace `26.08` if Steam's runtime changes. Steam's wrapper exposes the matching
extensions' commands, so per-game launch options can use `mangohud %command%`
or `gamescope -- %command%`. Per-game Gamescope also requires Steam Play
compatibility tools packaged for Flatpak; see the
[Gamescope extension's notes](https://github.com/flathub/org.freedesktop.Platform.VulkanLayer.gamescope#notes).

The Radeon needs no extra driver package: the common image already contains
the kernel's amdgpu driver, `amd-gpu-firmware` and Mesa's radeonsi and RADV
drivers. This configuration has no overlay or kernel arguments. GNOME chooses
150% scaling for this display unless the person has saved another choice.

CI publishes the signed `:ms-7c35.gnome` image. The installer recognises this
PC and preselects that composition; choose another machine or desktop on its
first screen. Its two independent match tables
use vendor `Micro-Star International Co., Ltd.` with system model `MS-7C35`
or board model `MEG X570 UNIFY (MS-7C35)`. The values came from Windows CIM's
SMBIOS tables on 2026-11-15; Linux DMI values have not yet been measured on
the physical PC. `ilex.composition=generic.gnome` at boot explicitly chooses
generic instead.

## Password-encrypted daily homes

The base includes homed (`systemd-udev` and `systemd-pam` in Fedora 45) and
btrfs-progs, enables the homed service,
and enables Fedora authselect's `with-systemd-homed` feature before overlays.
NSS continues to resolve both local and homed identities through `systemd`.
Do not edit generated PAM files. Home suspension is off (the packaged PAM
module's default); screen lock does not evict the home key. Lingering is not
enabled. Root encryption and TPM enrollment are separate work.

Fedora policy 45.24 denies `passwd_t` D-Bus messages to homed, including
`chpasswd`'s PAM query. The image installs `ilex-homed.cil` before overlays:
bidirectional `dbus send_msg` between `passwd_t` and `systemd_homed_t`,
matching Fedora's `systemd_homed_dbus_chat` interface, plus `filesystem getattr`
from `systemd_homework_t` to `pidfs_t` for the fsck spawn path. Interrupted
resize recovery also needs `dir setattr` from `systemd_homework_t` to
`systemd_homed_cache_t` to restore the staging directory's attributes on
activation. D-Bus/polkit/homed still authorize each request; SELinux remains
enforcing. Reassess this small module
when Fedora ships the corresponding caller integration.

New homed filesystems also need their initial SELinux labels before user
processes create files. Creation sets a `defcontext` of `user_home_dir_t` for
initially unlabeled inodes, so login's early SELinux PAM lookup can search
the new filesystem root. This is a fallback, not a fixed mount context.
A `user@` dependency then labels the active home once,
using `restorecon -R -x` without forcing security user/role/range. A root-only
filesystem-UUID marker under `/var/lib/ilex/homed-labels` prevents later logins
from replacing custom labels or rescanning large homes. Local passwd accounts
are skipped, so maintenance login does not require an active homed home.

AccountsService 26.27.3-2.fc45 also counts every shadow entry (60 on the
tested desktop) against the homed generator's 50-user limit, hiding all
homed users from GDM. `build/accountsservice-homed.patch` changes that one
comparison to the enumerated human-user table. A separate build stage uses
the SHA-256-pinned Fedora SRPM and replaces only `/usr/libexec/accounts-daemon`
when that exact RPM is installed; its client library/data remain packaged.
Another RPM version fails the build for review/removal of this backport.
Flatpak's malcontent dependency also brings AccountsService to shell-free
images; the same package-pinned daemon patch applies there.

Keep Anaconda's distinctly named local wheel administrator for maintenance.
Create the daily account locally from that account:

```sh
sudo ilexctl home create andriishafar --disk-size=20 --real-name='Andrii Shafar'
# Add --admin only when this daily account should have password-authenticated sudo.
```

`homectl` prompts twice for the password and creates/signs the complete account
in one operation. No password is accepted in this command's arguments or
inherited `PASSWORD`/`NEWPASSWORD` variables. Usernames/UIDs already present
in NSS are rejected by homed; existing accounts/homes are never converted.
The machine generates its own homed signing key at runtime. Back it and the
home's LUKS header up privately; never commit/export private records or keys.

The creation path explicitly uses `--storage=luks --fs-type=btrfs`, with
`/var/home/<name>.home` mounted at `/home/<name>` through Fedora's `/home`
alias. Sizes are GiB capacities, not outer filesystem quotas. Creation checks
for 2 GiB of spare backing space beyond the requested size; this admission
check cannot reserve space against simultaneous writers. Automatic resize,
rebalancing and online/offline discard are off initially, avoiding sparse
overcommit. Budget concurrently active homes and keep a system reserve.
Use `homectl resize <name> <size>` for supported online resizing; never
truncate a `.home` file. Homed can clamp requests to btrfs's safe minimum or
available backing capacity, so verify actual sizes even after a successful
exit. Automatic resize/rebalancing require separate
qualification before being adopted as defaults.

Created accounts join `ilex-homed` (and `wheel` only with `--admin`). SSH
allows `root`/`wheel` for maintenance and denies `ilex-homed`, even for an
active administrative home with an authorized key. Keep that marker group
when administering these accounts with homectl. No SSH password/bootstrap
flow for daily homes is supported. Local maintenance login remains available
when a daily home cannot be unlocked. Profiles and firstboot UI are later work.

`ilexctl test homed::` is opt-in and uses the installed-disk QEMU/bcvk
backend, never the SELinux-disabled ephemeral guest. `homed::lifecycle`
checks enforcing SELinux, authselect/NSS, inactive discovery in AccountsService
and GDM, password login/logout, TTY, password change, sudo, maintenance recovery
and SSH denial. The inactive SSH check supplies a valid key outside the
encrypted home, verifies rejection by `DenyGroups` without home activation,
and verifies the maintenance administrator can still SSH in.
`homed::allocation` uses disposable outer btrfs storage to
check online 2 → 3 → 2 GiB resizing, filesystem/mapper/backing sizes, checksums,
two active homes and ENOSPC/recovery without filling the system filesystem.
`homed::crash_grow` and `homed::crash_shrink` each use a fresh disposable
installed-disk overlay with 2 GiB RAM and two CPUs. A guest ptrace probe stops
the actual homework worker at the btrfs resize ioctl: entry after lower-layer
growth, or successful exit before lower-layer shrink. QMP resets the same VM
without guest shutdown or sync. Each trial verifies a new boot, maintenance
login, password unlock, pre-existing data checksums, actual allocation,
supported resize recovery and deactivate/reactivate. Interrupted growth can
leave a safe smaller filesystem inside the larger allocation; requesting the
same size can be a no-op. The trial normalizes through the smaller size with
`homectl resize` before retrying growth, never by truncating the backing file.

All four trials audit authentication/home AVCs and retain nonsecret evidence
under `<state dir>/artifacts/homed/`. Crash trials separately annotate the
exact worker/tracer PID pair's proc-directory search denial from systemd's
optional proot detection, which the probe itself triggers. The raw audit is
retained; other home/authentication denials still fail the trial. No tracer
access is granted in policy. This tier qualifies homed integration, not
encrypted root, Secure Boot or the future Windows security backend.

## Apps

Shell manifests may declare `[apps] flatpak = ["org.example.App"]` and
`rpm = ["host-integration-package"]`. IDs are unique application IDs; Flathub
and branch `stable` are fixed. App RPMs join the shell transaction. The composer
generates lists under `/usr/share/ilex/apps`, copied before overlays into
`/usr/share/flatpak/preinstall.d/50-ilex-*.preinstall`. No app payload ships in
the OS image. `/etc/flatpak/preinstall.d` can override groups (`Install=false`).

`ilexctl apps sync [--update] [--dry-run]` wraps upstream `flatpak preinstall`.
Flatpak preserves administrator removals and manual installs, and removes old
shell preinstalls on switch while retaining home data. Unused runtimes stay
under Flatpak's normal lifecycle; ilex never runs `uninstall --unused` as root.
The root `ilex-apps.timer` starts independently of boot targets, then daily.
Incomplete work exits 75 and retries with backoff; offline installs never hold
up GDM. Initial installation reserves 8 GiB minus allocated system Flatpak
content (including interrupted downloads), with a 2 GiB minimum; later work
reserves 2 GiB. Removal-only reconciliation bypasses the space gate. Unchanged
complete lists avoid network; `--update` reaches Flathub at most every 20 hours.
`apps status --json` reports wanted/installed/declined/unavailable apps, origin,
last result and free space. `updates list [--json]` includes staged OS and the
last system Flatpak update transaction, including runtime refs and changes
accumulated across retries of a partial transaction. Session path
units announce installation, completion and applied Flatpak updates.

The committed Flathub trust file bootstraps the system remote without network.
Fedora's remote-add unit is disabled. The sync marks Flathub `--no-enumerate`;
it also disables a `fedora` remote retained in `/var` by an older deployment.
`00-ilex-flatpak.rules` denies every system-helper action for non-root,
including wheel. Users install into their own Flatpak installation. In GNOME
Software an unacquirable `appstream-update` denial disables system metadata
refresh and selects user scope for bundles; the system apps remain visible as
installed. The bundle default is user scope too. Only root manages system apps;
to decline a preinstall use a root uninstall or an `/etc` list override.
Removing/disabling Flathub or changing its URL is respected, not repaired.

Verified source for the shipped versions: Flatpak 1.18.4's
[`update_appstream`](https://github.com/flatpak/flatpak/blob/1.18.4/app/flatpak-builtins-utils.c)
skips disabled and no-enumerate remotes; its
[`add_sync_preinstalled`](https://github.com/flatpak/flatpak/blob/1.18.4/common/flatpak-transaction.c)
still considers them. GNOME Software 51.1's
[`setup_thread_cb` and `ensure_scope`](https://github.com/GNOME/gnome-software/blob/51.1/plugins/flatpak/gs-plugin-flatpak.c)
use the unacquirable `appstream-update` permission to disable system refresh
and default new bundles to user scope. Sync removes any older system Flathub
AppStream cache before marking the remote changed, including on its fast path.
No system catalogue is downloaded by ilex; this avoids Software's
no-enumerate/main-ref edge case while preserving user catalogues and app data.
No-enumerate also excludes the remote from runtime dependency search.
Before preinstall (and daily app runtime transitions), sync reads the signed
app metadata and explicitly installs missing runtime refs from Flathub with
`--no-auto-pin`. Related extensions remain Flatpak's responsibility. This
keeps the remote hidden throughout and does not permanently pin old runtimes.

`ilexctl doctor apps` checks configured IDs against Flathub's API without
installing apps, warns about EOL, trust-file changes and imminent key expiry,
and explicitly reports unreachable checks as unverified. GPG inspection uses
a temporary home inside the checkout, never the host root's GPG state.
The checked-in key expires 2027-06-14; review refreshed upstream trust data
before that date. Do not assume remote metadata refresh alone renews the key.

Disk VMs default to a systemd SMBIOS credential adding an unmet condition to
the apps timer, keeping downloads out of ordinary tests. Upgrade masks both
apps units before the first timer deadline. `ilexctl vm run --disk --apps` enables
downloads; `vm run --gui` enables them by default (`--no-apps` opts out).
Ephemeral assertions parse the lists and trust file without Flathub traffic.

`ilexctl test apps::` opts into an enforcing installed disk and real
Flathub downloads: offline timer/backoff with the internet NIC down and a
restricted SSH management NIC, automatic recovery without a manual start,
an interrupted real pull that resumes below 8 GiB free, real installation,
list additions/removals (including removal below the free-space margin),
manual declines, unavailable IDs, retained app data, reboot fast path, real
updates after rolling apps back, retained partial-update history across a
retry, polkit denial and a real per-user install. Set
`ILEX_APPS_SHELL_FREE_IMAGE` to a locally built shell-free generic image and
run `apps::shell_free_switch` separately to qualify a real `bootc switch`:
the new deployment's timer removes former preinstalls while keeping manual
system apps, the user installation and home data. Its image is transferred
into the disposable guest's local container store; nothing is published.
`graphical::apps` is registered only when explicitly named. It logs into GNOME
with a homed wheel account, checks MIME handlers, launches Text Editor, Papers
and Firefox, persists a real Flatpak setting in the encrypted home, verifies
Clocks/Weather's D-Bus integration into the shell-owned settings, and asks
GNOME Software's public search provider to require user-only Flathub results,
then its Install action to install the discovered Chess result. The opt-in
fixture downloads a legacy system catalogue first to qualify its removal.
Screenshots and SSH evidence go to the `graphical-apps` artifact directory.
Flatpaks without dconf access use their own keyfile under `~/.var/app`; that
is normal and the test records the actual backend. GNOME's host writer stays
confined for the shell-owned Clocks/Weather integration settings.
`publish.yml` runs only metadata checks by default; `apps=true` additionally
qualifies `apps::install` and reports whether it ran in the run summary.

The network installer always attempts `ilexctl apps sync`, with a 30-minute
workload deadline and a 15-second forced-kill grace period. Its `--nochroot`
`%post` uses private mount/PID namespaces with `unshare --fork --kill-child=KILL`;
PID 1 exiting kills its descendants, including detached process groups.
The chroot gets tmpfs `/dev` and `/run`, only null/zero/full/random/urandom
character devices, a fresh `/proc`, read-only `/sys` and a read-only resolver
file. No block devices or installer runtime sockets/state enter the chroot.
The payload's helper and Flatpak use its own `/var`.
A bootc post-install chroot lacks `/dev/null` (Rust's captured subprocesses
fail with ENOENT). Flatpak/OSTree also needs `/proc/sys/kernel/random/boot_id`;
the helper explicitly supplies the runtime filesystems rather than relying
on Anaconda's partial setup.
The temporary mounts disappear with the namespaces. Cleanup waits at most five
seconds to verify no processes remain in the recorded PID namespace, caller
runtime mount IDs match and the live installer's apps status is unchanged.
Snapshot reads/parses must succeed; errors cannot count as empty evidence.
It records the result under
`/var/lib/ilex/apps/installer-result` and diagnostics in Anaconda's log. A
transient failure is non-fatal and the boot timer retries. `iso::install`
checks the marker, complete state and every system app's Flathub origin with
the timer suppressed, proving preload rather than a post-boot download.
Before assertions it requires serial copies of `ilex-apps.log` from the target
and Anaconda's live `/tmp/program.log` and `/tmp/anaconda.log`, before their final
copy to the target. It saves those logs and the apps/cleanup state beside the
screenshots. Installer serial output is retained on timeouts too.
The test compares runtime mount IDs before/after preload, allowing Anaconda's
existing `/proc` mount while requiring the preload's binds to stay private.
As with all ISO assertions, this qualifies the published payload: until that
payload declares apps, it explicitly reports preload as not applicable and
does not establish the new chroot path. The native installer can invoke the
same sync command later.

Machines accept the same optional `[apps] flatpak` and `rpm` lists as shells
(generic has no machine additions). Machine app RPMs join its transaction;
Flatpak is included when the machine declares Flatpaks, even on a shell-free
base. A generated `machine-<name>.preinstall` joins the shell lists and enables
the root timer. `ms-7c35` declares Steam through this path. `ilexctl doctor apps`
checks machine IDs too; configuration/self-check assertions include them.

The apps stack must merge together in one main push: the GNOME RPM removal
alone would publish an image without a browser. The ilex shell itself is out
of scope; its future KDE application list belongs to that shell's project.

GNOME declares all 20 replacements, including Firefox, Logs, Scanner and
Boxes. Files, Ptyxis, Settings, Software, System Monitor, Disks, Help and
Sushi remain host RPMs. Papers keeps its Nautilus plugin, libraries,
thumbnailer and print previewer. The browser connector is excluded because
Flatpak Firefox cannot use its host native-messaging connector; GNOME
Extensions remains available as a user Flatpak. There is no RPM Firefox
profile migration (no deployed machine used one when this was introduced).

Measured 2026-11-18 against the same Fedora bootc digest
`81ada60d0dfec088b591672bce2945d6d7ac8e32e7f263eba188bdfa809fbc04`:
the main baseline image is 5,009,428,787 bytes and the GNOME Flatpak stack image
is 4,126,631,728 bytes (`podman image inspect .Size`, uncompressed), a reduction
of 882,797,059 bytes (841.90 MiB). Flatpak payloads/runtimes live separately
in `/var/lib/flatpak`; this is an OS image measurement, not total installed disk
usage. The planning run estimated about 5.5 GB for those payloads.

## Public user profiles

`image.toml` selects public profiles with `users = ["andriishafar"]`. This is a
fourth configuration dimension; every root (including shell-free `base`) and
its machine layers carry the same catalogue. No publication name, OCI layer
selection or image label gains a user dimension. Images work without selecting
a profile: `ilexctl home create` still accepts a custom local username.

`users/<id>/user.toml` is a template, never an exported homed record. Schema 1
requires `schema`, positive `revision`, `username` equal to the directory,
`display-name`, `shell = "bash"`, `locale` such as `en_US.UTF-8`,
`storage = "luks-btrfs"`, and `disk-size-gib` (2–1048576). Optional `groups`
can name only `wheel` and `gamemode`; provisioning must explicitly authorize
administration and check group availability. Applying a profile never changes
an identity, storage size or groups. Unknown fields, including homed secret,
privileged, password, recovery, binding/status and signature fields, fail the
build. The minimal `andriishafar` example uses the owner's requested username
and display name (Andrii Shafar), with no email, credentials, administrative
groups or additional personal data. The owner should adjust its settings before
use. Keep the maintenance administrator distinct (the fixture uses `ilexmaint`);
homed refuses creation when the requested username already exists in NSS.

Optional `settings.toml` accepts `[gnome]` with `color-scheme` (`default`,
`prefer-dark`, `prefer-light`), boolean `clock-show-weekday`, and `clock-format`
(`12h`, `24h`). No command strings or arbitrary dconf paths are accepted.
Optional `apps.toml` accepts `[[flatpaks]]` entries with an application `id`
and `remote = "flathub"`. Apps install/update only on explicit `--install-apps`,
after creating the user's Flathub remote from the committed trust file. System
apps are skipped and reported in `provided_by_system` (provided by the system),
so a profile does not duplicate them. Install failures can be retried after login.
No network operation runs automatically or gates authentication.

Initial `home/` files are deliberately restricted to `.gitignore`,
`.editorconfig`, `.config/git/ignore` and `.config/ilex/<name>.txt|toml|json`.
Each profile TOML manifest and initial file is limited to 64 KiB, checked on
the opened descriptor before allocation and enforced again by a bounded read.
Only non-executable UTF-8 regular files are supported. Symlinks,
special files, traversal, credentials, identity/keyring paths and login hooks
are refused. Extend these allowlists by review. The composer reads every
profile input through no-follow directory descriptors and stages canonical
`users.json` (schema 1), `users/<id>/user.json`, and `home/` templates through
the existing generated-file installer and `.generated.json`. Profile inputs
enter root content/cache and installer fingerprints.

After `sudo ilexctl home create NAME --disk-size=20`, authenticate locally
as the new daily user and run `ilexctl profile list`, then
`ilexctl profile apply andriishafar`. A public profile can seed a custom homed
username; its identity suggestions remain metadata for setup. Apply refuses
root, an inactive/non-LUKS home, or a caller/home UID mismatch. It uses NSS,
not `$HOME`, and writes with owned no-follow directory descriptors. Nothing
imports or rewrites `.identity`, `.identity-blob`, keyrings or credentials.

Threat model: profile apply defends against malicious/malformed profile content
and links or configuration redirects present before apply. It does not prevent
a concurrently malicious process of the same user, which already has that
user's full write access. The checks below detect changes; they are not an
authenticated writer protocol or a security boundary against another process
of the same UID.

External settings/app writers run in bubblewrap with a read-only host, masked
homes and runtime, a clean environment, and only no-follow-opened home
subdirectories bound writable. A systemd user drop-in confines the session's
actual dconf writer for homed accounts, retaining its bus, cache and desktop
notifications. Local maintenance accounts retain Fedora's writer. Profile
clients use a canonical dconf/session environment, refuse activation overrides,
and verify the bus writer belongs to the confined unit. They re-check the bus
ID, unique writer connection, PID and unit after settings operations. A changed
or unverifiable writer/bus reports an error and retains the previous managed
setting values; detection cannot undo writes already made by a malicious
replacement. Runtime IPC remains in
the user's owned runtime directory. Symlinked dconf directories/databases are
conflicts.

`~/.local/state/ilex/profile.json` schema 2 stores profile revision/digest,
managed file/setting hashes or values and the backup index. Schema 1 migrates
on apply; older helpers decline schema 2 instead of dropping recovery tracking.
Missing files are seeded atomically;
updates use Linux `renameat2(RENAME_EXCHANGE)` and verify the displaced contents
against the last managed digest. An intervening save is exchanged back and
reported as a conflict. Every exchanged-out inode, including a rollback version
and replaced `profile.json`, is moved rather than copied into a user-owned,
no-follow-opened directory on the same filesystem:
`~/.local/state/ilex/profile-backups/<UTC-timestamp>/`. The state records each
backup's original path and verified digest. An editor holding an old descriptor
can finish saving into that retained inode. Apply checks the backups at the end
and again on the next apply; a changed/unreadable/missing version reports a
conflict naming its backup path. A write after the final check is detected on
the next apply, while the backup already preserves the edit. If moving a version
fails, the error names its retained `.ilex-profile-*` path instead. Unsupported exchange
operations fail without falling back to an unchecked replacement. Existing files, edits,
deletions, symlink/hardlink paths and explicitly set desktop settings are
preserved and reported as conflicts (exit 1).

Apply never prunes backups. It prints a one-line notice above 50 MiB; ordinary
unchanged applies create no new state snapshot or backup directory. Inspect
sizes and proposed deletions with `ilexctl profile prune-backups` (dry run).
`--keep 5` retains the five newest backup applies; it still only lists by default.
After closing editors and reviewing/recovering any changed versions, explicitly
run `ilexctl profile prune-backups --keep 5 --execute` to delete older ones.
`--execute` without `--keep` removes all listed applies. Cleanup requires the
authenticated homed user, rejects links/unsafe file types, and preserves its own
previous-state snapshot in a new backup apply in addition to the requested N.
Cleanup inventories the on-disk backup tree as well as the index. Dry runs list
unrecorded files as `orphans` and already absent recorded files as `missing`;
orphan files are also reported by apply and included in size accounting. Explicit
cleanup can remove orphans even without `profile.json`. It unlinks each selected
file through a pinned directory descriptor, confirms absence and syncs deletion
before dropping that record. A partial failure retains survivor records; a
retry tolerates already removed files/directories and finishes the cleanup.
If the index write is interrupted, the older records remain safe to reconcile.
There is no safe automatic age/count policy for an editor's outstanding fd:
explicit cleanup can discard late writes, so close editors before executing it.
Setting resets count as edits: reapply writes only an initially absent setting,
or a changed profile value whose current override still equals the last
managed value. An unchanged managed setting is not rewritten.
For changed settings, read/compare/write each key back to back, with a final
pre-write read and post-write read. An observed intervening reset/save is left
untouched, or the later user value is retained, and reported as a conflict.
Dconf has no compare-and-swap: an edit in the remaining final-read/write window
can still be overwritten without detection. Settings preservation is therefore
best-effort, not transactional; do not claim all concurrent settings edits are
recoverable.
Invalid/newer state, a newer profile revision, or changed content without a
revision bump declines updates
(exit 1). Never lower a revision during rollback. Removed profile files are
left in place. The per-user `ilex-profile.service` reapplies only an already
selected profile on login, without app installation; no selection is a no-op.
There are no root-run profile hooks. Installer choice/provisioning UX is PR 22.

`ilexctl test ephemeral::user_profiles homed::profiles -- --test-threads 1`
checks catalogue/schema/path/registry rejection in a booted image and profile
application through PR 20's real creation/TTY path in an enforcing installed
guest. It creates `andriishafar` from the catalogue metadata alongside the local
`ilexmaint` administrator, verifies maintenance-name collision refusal and
maintenance recovery login. The guest uses 2 GiB and no UI automation, and tests
user-edit/deletion and link preservation, future-state/rollback refusal, opt-in app failure, and a
successful relogin while the older image's user service declines newer state.
On a GNOME image it also checks scoped dconf updates, reset preservation,
dconf directory/leaf links outside the encrypted home, inherited environment
redirects and user-setting conflicts. The compiler regression rejects sparse
512 MiB templates and all three manifests under a 128 MiB address-space limit.
Real-homed interleaving regressions use other processes of the same user to
save after the file comparison, save again during rollback, reset a setting
after comparison or during its write, and restart the checked dconf writer.
They also hold an old file descriptor across exchange and save after verification
or after apply returns, checking retained Btrfs backup content and end/next-apply
conflicts. Cleanup coverage checks dry-run sizes, explicit keep-N removal,
symlink refusal, retained state snapshots, no automatic pruning and the size notice.
They inject mid-prune and state-write failures, reproduce EACCES on an owned
0500 backup directory, and verify survivor indexing, retry, orphan discovery and
cleanup with missing files/directories or no profile state.
Measured 2026-11-17: the shell-free image is 1,977.85 MiB against Fedora
bootc's 1,917.62 MiB, a 60.24 MiB increase within the then-current 128 MiB budget.

## Build sharing: kache

Image builds compile through [kache](https://github.com/kunobi-ninja/kache), a
compiler cache keyed by the content of each rustc invocation's inputs. The
Containerfile's builder stage installs a pinned release tarball (v2.0.0,
sha256-checked; v2 needs both `kache` and `kache-engine`) and builds the
declared Rust packages (`build/image-crates.toml`, through
`build/build-image-crates.py`) with `RUSTC_WRAPPER=kache` and a scrubbed
environment (no `CARGO*`, `RUST*` or `KACHE*` variable of the caller). The store is the podman cache mount
`ilex-kache`, shared by every worktree; the target directory is fresh per
build, so nothing trusts mtimes and one worktree's binaries can't leak into
another's image. The step ends with `kache stats --last-build`: crates
restored vs compiled.

- `ilexctl build` also mounts the host's own store (`$KACHE_CACHE_DIR`, else
  `~/.cache/kache`, once it holds an `index.db`) read-only at `/kache-host`,
  and the builder reads it only when present, so a plain `podman build` and CI
  work without it (`--no-host-kache` skips it). After a host `cargo build
  --release -p ilexctl --no-default-features` with kache set up (`cargo install kache && kache
  init`, once per Linux host), the image build can restore those crates.
  On Windows, the host store is in the Linux builder. An exported Linux
  cache can be imported using the [migration guide](docs/tooling/windows-migration.md).
- To bump kache, change the version in the URL and the sha256 in the
  Containerfile (`gh release view vX.Y.Z -R kunobi-ninja/kache --json assets`).

## Tooling contracts (T1)

`ilex-tooling-core` defines architecture, checkout/source identity, selection,
build requests/results, disk artifacts, VM specs/handles, capabilities, RGB
frames/input and versioned state/remote envelopes. It has no platform path or
hypervisor policy. Host and guest architectures are separate. The current Linux
backend qualifies native x86_64 only; unsupported requests fail before launch.
The `ilex-devkit` facade re-exports the portable contracts and Linux tooling.

```sh
ilexctl info --json                 # identity, source manifest, paths, capabilities
ilexctl info --capabilities
ilexctl identity status             # automatic primary claim; no hand-written config
ilexctl identity release-main --owner <UUID> # only when that checkout no longer exists
ilexctl identity rebuild-legacy-image --expected-id <ID> # explicit rebuild, preserving old image
ilexctl doctor --self-check --json  # disposable real Git checkouts; no images/VMs
ilexctl doctor --self-check-build --json # Linux: actual gated Podman builds and ownership rejection
ilexctl disk --self-check           # Linux: disposable CLI alias faults; no real store or VM
ilexctl doctor --arch aarch64        # explicit rejection on the current Linux backend
ilexctl doctor --require firmware_enforcement # fails: ordinary defaults are nonsecure
ilexctl vm capabilities --json          # attach to a running VM; MCP: capabilities
```

Only the automatically registered canonical primary checkout gets `main` (see
the allocation policy above). All other primary/linked/T3 checkouts include their
canonical owner hash, even for short, valid Git worktree names; missing Git
metadata can never acquire `main` and cannot authorize mutations.
The canonical native checkout path plus common Git directory
produce a stable UUIDv8 owner. The Windows sender passes that identity
explicitly in each remote snapshot.

Local image builds pass typed requests to the Linux executor. A source manifest
covers Git modes, working bytes, untracked inputs and tracked deletions; it
excludes root Git internals and target/state. Private-named `.env`/`*.key`
source entries and ignored files inside build input directories are refused
before their contents are read, rather than silently omitted while Podman
copies them. Escaping symlinks are rejected. Two inventories and a post-build
check detect source changes. Builds write a job-specific temporary tag and
promote only after validation; source changes and inventory failures remove
only that alias and preserve an earlier accepted final image.
Image labels `ilex.checkout-owner`, `ilex.source-digest` and
`ilex.architecture` record provenance; `<state dir>/jobs/<job hash>/` retains
request/result JSON with the immutable image ID and local executor identity.
The remote executor preserves this provenance across source transfer.
Machine cache hits also require current owner/source labels, so documentation
changes refresh provenance even when the base and machine content are unchanged.

Immutable-ID disk inputs use a versioned local alias because bcvk's inner
transport cannot parse `sha256:...` as a local image ID. The shared ownership
helper leases that alias in the Podman store through installation, validates
the full source ID before tagging, and binds reuse to a store/owner/alias/ID
allocation receipt in `<state>/image-aliases/`. Explicit source labels may be
foreign/CI labels and are retained. An existing same-ID alias without a receipt
is refused; an inspection/store error never means absence. Earlier unreceipted
aliases are not adopted. `disk --self-check` injects inspect/exists exit 125,
unreceipted same-ID aliases, mismatched owners/IDs and cross-output concurrency
through the actual CLI with private stubs. CI retains its JSON alongside the
security evidence; real image-ID disk installation and guest boots remain e2e.

State/runtime roots carry `.ilex-owner.json` schema/owner receipts, and disk
cache outputs carry `<filename>.ilex-owner`. Receipt checks precede reuse or
replacement, including tools/keys and bcvk paths. Named VM launches hold an
exclusive lifetime lease, refuse any pre-existing container/domain, and remove
only the recorded created container ID/domain UUID. A stale resource requires
its owner to inspect and explicitly remove it; launch no longer force-replaces
by name. Immutable image-content caches internal to bcvk remain shared.
Unlabelled legacy tags are refused for implicit reuse. An explicit source build
by the registered primary rebuilds its unlabelled `main` outputs under a job
alias, validates the new source and owner, preserves the previous ID under
`localhost/ilex-legacy:<owner>.<old ID>` with an allocation receipt, and only then
promotes the freshly built image. It does not relabel or run the old image.
Other checkouts can explicitly rebuild an inspected legacy default image using
`identity rebuild-legacy-image --expected-id <ID>`; ordinary builds still refuse
unlabelled outputs there. Foreign labelled images are always refused.
Nonempty unlabelled state/disk caches fail visibly with `unlabelled legacy ...;
refusing adoption`. Old resources are never automatically deleted or imported.
Build output flags (`--tag`, `--manifest`, `--output`, `--iidfile`) are reserved
for the executor; use `ilexctl build --tag` to choose the final output.

### One-time tooling migration

No owner configuration or hand-written receipt is needed. The first tooling
call from the existing canonical primary or any of its linked worktrees
registers that primary as `main`. `identity status` shows the registered UUID
and checkout path. A second clone cannot take the claim; removing a checkout
does not silently transfer it. An agent may run `identity release-main --owner
<registered UUID>` after the checkout is gone; the command verifies absence and
appends a release receipt, leaving all images, VMs and data untouched.

Owner-qualified worktrees and the new `main--<owner hash>` data paths start
with cold disk/tool/ISO caches and newly generated development tool-signing
keys and SSH test keys. Existing tool images remain signed by their old keys;
rebuild them in the new state rather than importing unknown keys. Production
signing keys and image signatures are not changed. The ISO helper mounts its
owned host cache at `/var/cache/image-builder`, including the persistent
`store/` and `tmp/` directories; repeated ISO builds reuse that host store.
The tooling supplies an osbuild wrapper that checkpoints `build` and `os-tree`:
image-builder 89 otherwise does not retain these intermediate trees. The
native path prepends an owned wrapper directory for that command only; no
system executable or host configuration is replaced.

Old hashless images, VMs, runtime directories, disk caches and development keys
stay where they were. A new launcher refuses an existing bcvk name. For optional
cleanup, first inspect the exact legacy container/domain ID, image labels,
disk paths and PID creation identity and establish that they belong to this
checkout/session. Stop its owning launcher, or destroy only the verified
container ID/domain UUID. Remove only the inspected alias (`podman untag
<reference> <reference>`) and the specific old state/runtime directory after
confirming no VM uses its disks or keys. Never prune the store, force-replace a
name, or infer ownership from the old slug alone. If ownership cannot be
established, leave the resource in place; normal new-namespace work does not
need its removal. `legacy-images/*.json` records backups made during a rebuild.

VM metadata includes schema, owner, run, architecture and PID creation identity.
Unknown versions and foreign owner records are rejected before replacement;
stop an old VM through its owning launcher before updating tooling and relaunch
with the current version. Never adopt an old `main` image for a worktree.
QMP uses the portable protocol over Linux Unix sockets, with IDs, bounded
messages and per-command deadlines. Frame contracts include VM identity,
timestamp, viewport and resize generation; stale pointer mappings fail. The
existing CLI/MCP screenshot coordinates stay in framebuffer/downscaled pixels.
Firmware enforcement, persistent TPM and suspend remain explicitly unavailable
or unqualified for ordinary Linux defaults; no security capability is silently
weakened.

## Windows development tooling

The infrastructure commands run natively: run `ilexctl
runtime fetch|status|update|self-check` and `ilexctl builder
up|status|down|update|self-check` in PowerShell. The persistent per-user Gen2
builder, pinned acquisition, SSH identity, owned cleanup and update recovery are
documented in [Windows builder tooling](docs/tooling/windows-builder.md).
T3 also routes native Windows `build`, `test`, `disk`, `iso`, `drupe build` and `rechunk`
to that builder automatically, calling idempotent `builder up` as needed.
Run all Windows commands from native PowerShell in the checkout.
Install a changed controller with `builder update`; a recipe mismatch fails
rather than executing an older controller. Accept its verified update with
`builder accept-update`. The explicit `security::firmware` tier runs on native
Hyper-V Gen2, and legacy filters in the same invocation run in the Linux
builder. The Windows homed tier remains unavailable. Never remove the shared builder as ordinary
checkout cleanup.

```powershell
ilexctl builder up --memory-gib 16 --cpus 8 --wait-for-memory-seconds 10800
ilexctl builder update --wait-for-memory-seconds 10800
ilexctl build
ilexctl vm run
ilexctl vm run -- uname -r
ilexctl test ephemeral:: disk:: upgrade:: graphical:: tool:: -- --test-threads 1
ilexctl remote snapshot
ilexctl remote --self-check
ilexctl remote resume --job <UUID>
ilexctl remote cancel --job <UUID>
ilexctl remote export --image <owned-image> --out 'C:\outputs\ilex.oci.tar'
ilexctl remote self-check-receiver
```

Snapshots inventory HEAD/index names, dirty working bytes and nonignored
untracked inputs, including deletions, executable modes and Git's Windows
symlink-file representation. LF-attributed CRLF text, credential-named inputs,
ignored build inputs, escaping links/reparse parents, submodules and unresolved
conflicts fail before transfer. SHA-256 chunks are at most 4 MiB; Linux verifies
and atomically materializes them. A shallow original-HEAD pack carries no user
Git configuration, hooks or parent history. Owner, slug, revision/dirty flag and
source digest come from Windows; the copy cannot become `main`. Both executors
retain the existing clean Git-tree fingerprint encoding and use the same
versioned working-input encoding when dirty.

Pinned SSH carries finite versioned requests and typed command fields. Only explicit image/selection, legacy test and ISO assertion controls
are forwarded. Host PATH/HOME/state, agents, CI and registry/signing credentials
are not. Podman, kache and Cargo caches remain in the builder. Cargo outputs are
separate by owner, architecture/toolchain and immutable snapshot. OCI export
uses an archive and adds no registry push.

Jobs take owner leases and memory/CPU-weighted guest slots. Admission preserves
the 14 GiB host reserve and waits up to three hours for capacity. Detached
containers have prelaunch allocation nonces or Podman CID files; libvirt has a
prelaunch nonce and exact domain UUID. Cleanup checks identities, fails closed
on lookup errors, and retains an unreconciled lease after cleanup failure.
Console logs use libvirt's labelled log tree before artifact collection.

Ctrl-C cancels only the job's systemd cgroup. `remote cancel` sends an
owner-validated cancellation independently of the running client's lock and
returns its acknowledgement; the original client collects evidence, or use
`remote resume`. Finite jobs can continue after an SSH disconnect. An
interactive ephemeral VM follows the lifetime of its attached terminal. Logs
and frozen artifacts return even after failure.

Windows control files stay in `<state dir>/remote/jobs/<UUID>/`. All builder
data, including job request/context copies and results, is untrusted and goes
only under the separate `<state dir>/remote/received/<UUID>/`: streamed logs in
`logs/`, each collection in a fresh UUID directory. `delivered.json` maps logical artifact names to those
received files; disk metadata remapped for Windows is `disk.windows.json` in
the collection directory. No manifest path addresses checkout state. The entire
manifest is checked before writes: fixed prefixes/job filenames, strict Windows
names, regular files, case/path uniqueness, 8,192 files, 128 GiB per file and
256 GiB total. Logs are bounded at 256 MiB each and metadata at 32 MiB.
Every child open is relative to a verified directory handle with native
no-reparse flags. Ancestors remain held against replacement; changing a tag in
place cannot redirect a child open. Reparse points and hard links are refused.
Checksums and declared sizes are verified through the opened files. The receiver
self-check mutates a held directory into a junction at the child-open boundary
and verifies that no outside entry appears.

Explicit disk/archive files require owner receipts and output locks. Explicit
ISO/`ILEX_ARTIFACTS` directories require owner receipts and receive a per-job
UUID subdirectory; they cannot target tooling state/control paths. Resume
revalidates claims. Output copies are published atomically by file handle;
commands print the delivered path. `iso --due` stays a native read-only command;
registry checks retain their exit codes. The installer remains a network ISO.

For fault checks, `remote --self-check --allocation-image <owned-image>` pauses a
real ephemeral launch after container creation and before its CID receipt, for a
second terminal to cancel. `remote --self-check --fingerprint-image <owned-image>`
and Linux `doctor --self-check-fingerprint <owned-image> --json` report actual
root/rechunk fingerprints for same-tree comparisons. Acceptance logs and
measurements belong in the PR body.

Windows `ilexctl vm run --gui` and `vm run --disk` now run native QEMU/WHPX;
`--gui` uses the pinned portable SDL/virgl runtime, an immutable disk delivered
by the T3 builder and a writable overlay. Native Windows `ilexctl vm` controls and MCP
attach directly, including exact-HWND virgl screenshots. Clipboard is an
unpublished development overlay, supervised over pinned SSH only during an
unlocked user session. Right-Ctrl+G escapes SDL grab. See
[Windows GPU development](docs/tooling/windows-gpu.md) for lifecycle, memory,
resolution, capture limitations and acceptance commands.

| Windows command | Backend |
| --- | --- |
| `build`, `rechunk`, `disk`, `iso`, `drupe build` | Managed Fedora builder with rootful Podman |
| `test ephemeral::` and the ordinary Linux tiers | Managed Fedora builder with nested KVM |
| `vm run`, `vm run -- <command>` | Ephemeral bcvk guest on the builder, with SSH terminal or streamed command output |
| `vm run --gui`, `vm run --disk` | Native WHPX, with SDL and virgl for `--gui` |
| `doctor security`, `test security::firmware`, `vm run --secure` | Native [Hyper-V Gen2](docs/tooling/windows-gen2.md) |

See [Windows migration](docs/tooling/windows-migration.md) for inventory,
optional cache import and the owner's retirement checklist.

## Driving VMs: `ilexctl vm`

`ilexctl disk` writes the installed qcow2 (`bcvk to-disk`). On Windows,
`vm run --disk` boots a native headless WHPX VM and `--gui` selects SDL/virgl.
Linux `vm run --disk` uses the graphical test machine (UEFI, 4K `virtio-vga`);
`--gui` opens GTK on a Linux desktop.
Linux disk-booted VMs (also the `disk::` and `graphical::` tests) publish `qmp.sock`, `vnc.sock` and
`vm.toml` in
`$XDG_RUNTIME_DIR/ilex-vm/<slug>/<vm>/`; native Windows publishes private
`vm.json` with independent current-user/SYSTEM-only Windows AF_UNIX QMP
endpoints and PID/HWND identity in the
runtime directory printed by `ilexctl info`. `ilexctl vm` controls attach by slug
(from the checkout you are in, or `--slug`). Only `vm run` starts a VM.

```sh
cargo install --locked --path crates/ilexctl   # native Windows or Linux
ilexctl vm list
ilexctl vm screenshot -o shot.png        # read the PNG to see the screen
ilexctl vm wait-screen --stable 3        # not blank, then unchanged for 3s
ilexctl vm click 640 400                 # screenshot pixels; cropped client pixels on Windows
ilexctl vm type 'hunter2' --enter        # US layout; ilexctl vm key ctrl+alt+t
```

Agents get the same tools (`screenshot`, `click`, `scroll`, `type_text`, `key`,
`wait_screen`, `vm_list`) as an MCP server; screenshots come back downscaled
and `click` takes coordinates in that downscaled image:

If `ilex-vm` is already registered, remove it in the scope where it was added
before adding the replacement. Run these commands from the intended checkout.
The example uses the default `local` scope. For a `user` or `project`
registration, use that scope in both commands. Skip removal on a fresh setup.

```sh
claude mcp remove -s local ilex-vm
claude mcp add -s local --transport stdio ilex-vm -- ilexctl mcp
```

- QMP serves one client at a time; `ilexctl vm` connects per command, so an agent
  and a running test can share a VM. One VM per purpose and checkout: `vm`
  (`ilexctl vm run --disk`), `gui` (`--gui`), `disk` and `graphical` (tests); pick
  with `--vm` when more than one runs.
- Tests assert over SSH, not pixels. Screenshots are evidence: the
  `graphical::` tier saves them to `<state dir>/artifacts/graphical/` (or
  `$ILEX_ARTIFACTS/graphical/`).
- `disk::` also checks headless images. `graphical::login` exercises GNOME's
  login and scaling, and fails immediately before installing or booting a VM
  when the image's default shell is not `gnome`, including `--shell none`.
- The SSH test key and the throwaway disk overlays live in the state dir;
  `ilexctl vm run --disk` prints the exact `ssh` command line.
- On Linux, `--gui` selects virgl when a DRM render node is available.
  QMP input works with GTK virgl. For QMP screenshots, use `--disk` or
  `--gui --no-gl`. Windows SDL screenshots use the exact client area of
  the VM window.
- Every VM the Linux harness boots (`ilex_devkit::qemu::Vm::boot`) gets
  `-audiodev none,id=ilex-noaudio`. Without it, QEMU 11's VNC server asks for
  a default audio backend and QEMU probes its list in order, PipeWire first.
  On a host without a PipeWire daemon the probe fails and tears its thread loop
  down with `pw_thread_loop_stop`, which in libpipewire intermittently never
  returns: QEMU's main thread waits in `pthread_join` inside `qemu_init`, the
  QMP socket exists but nothing answers on it, and the guest never runs, so its
  serial log stays empty until a test's deadline (about one boot in a hundred
  on the Boat hosts; a rerun passes). No tier wants sound. If a QEMU ever hangs
  at start-up for another reason, `Vm::boot` gives it 30 s in all for its QMP
  socket to appear, accept a connection (QEMU binds the path before it listens,
  so `NotFound` and `ConnectionRefused` are retried) and send its greeting. A
  QEMU that exits meanwhile is reported with its exit status and `qemu.log`; one
  that does not answer is killed, reaped and reported with `qemu.log`.
  `ILEX_VM_FIRMWARE_LOG=1` makes every VM also
  write OVMF's debug console to `firmware.log` beside `serial.log` (about
  750 KB per boot, so opt-in). The ephemeral VMs (`ephemeral::`) run
  bcvk's own QEMU, which has no `-vnc`; this is not applied to them.

## Developer tools: drupe

A prototype of GNOME OS's [Toolpak](https://blogs.gnome.org/alatiera/2026/09/26/introducing-toolpak/)
idea under ilex's own name and format (Toolpak has published neither code nor
a format yet): tools like `strace` that can't run in a container and have no
place in an image-based `/usr`. A tool is one signed disk image holding its
commands and every library they load; running a command mounts that image at
`/app` for that process tree only and executes it as the calling user with
the rest of the machine untouched (`crates/drupe`).

```sh
ilexctl drupe build strace                  # dev machine: <state dir>/tools/images/strace.raw + drupe.crt
ilexctl drupe build net nmap tcpdump        # one tool `net` with the commands of two packages
```

Install, list, remove and run tools on the machine using the Drupe commands
in the [machine command table](#ilexctl).

- The image is a discoverable disk image (erofs root + dm-verity hash +
  signature partitions, `systemd-repart`), built in a Fedora container from
  the packages' commands in `/usr/bin` and `/usr/sbin`, the shared libraries
  the loader resolves for them, and glibc's loader itself. `drupe.json` at its
  root names the commands. An ELF command is started as `/app/lib64/ld-linux-…
  --library-path /app/lib64 --argv0 <cmd> /app/bin/<cmd>`: its libraries come
  from the image, and since nothing goes through the environment the programs
  it starts are the host's with the host's libraries.
- Trust: `drupe` only mounts through `systemd-dissect --image-policy
  root=signed:=absent+unused`, so the root hash must be signed by a
  certificate in `/etc/verity.d/` (or `/run/verity.d`, `/usr/lib/verity.d`,
  the kernel keyring). The image ships none: `ilexctl drupe build` signs with a
  per-checkout development key (`<state dir>/tools/keys/`, created on first
  use) and prints the certificate to copy to `/etc/verity.d/`. A real key and
  where tools are published are open. `install` also reads every file once, so
  a damaged image fails there and not with an I/O error later.
- Privilege: `/usr/libexec/drupe/drupe-enter` is setuid root. It unshares the
  mount namespace, binds `/run/drupe/priv/<tool>` (root-only) onto `/app`
  read-only, nosuid, nodev, drops to the caller in every id, and only then
  reads the caller's environment and executes. A user namespace would need no
  privilege but defeats the purpose: `strace -p` on a process outside it and
  `sudo` inside it both fail. The environment travels in a memfd because
  glibc strips `LD_LIBRARY_PATH`, `TMPDIR` and others from a setuid program.
- State: images in `/var/lib/drupe/images/`, mode 0600 from the first byte
  (`install` writes its own copy and never takes over the source's mode: a
  setuid file handed to root must not become root's setuid file there);
  `drupe.service` (`drupe activate`) mounts them at boot and rebuilds
  `/run/drupe/{cmd,bin,tools}`.
  `/etc/profile.d/drupe.sh` puts `/run/drupe/bin` first in `PATH` (symlinks
  to `drupe`, which runs the command it was called as). `sudo` resets `PATH`:
  use `sudo` with Drupe’s `run` verb to invoke `strace`.
- A user can't trace a drupe command (`strace rg …` with both from drupe, or
  `gdb` on one): the kernel ignores the setuid bit of a traced program, so
  `drupe-enter` can't make its namespace and says so. Root can. The image
  also sets `kernel.yama.ptrace_scope=1`, so `strace -p` on a process that is
  not a descendant needs root with any strace.
- `/app` is an empty directory in the image (`mkdir` in the Containerfile:
  the root filesystem is read-only on a machine).
- Limits of building from RPMs: a program that looks for its data under
  `/usr/share` or `/usr/libexec` (qemu's firmware, git's helpers) finds the
  host's or nothing; such tools need a build with `--prefix=/app`, which is
  what the fixed mount point is for. Libraries a tool `dlopen`s (NSS modules)
  are not collected. No tool depends on another tool.
- `tool::` tier (`ilexctl test tool::`, not run in CI): builds the strace
  tool once (`ILEX_TOOL_REBUILD=1` rebuilds), refuses it while its certificate
  is untrusted, then runs it as `nobody`: ids and capabilities, tracing a host
  program that loads the host's libc, attaching to a running process, the
  environment, `/app` private to the namespace, `PATH`, a damaged image,
  remove and reinstall. An ephemeral VM has SELinux disabled, a virtiofs root
  and no reboot, so `tool::installed_system` boots the `disk::` tier's
  installed disk as its own VM (`tool-disk`, the tool image attached as a
  second drive): install and run under SELinux enforcing on the composefs
  root, then a reboot after which `drupe.service` must have brought the tool
  back.

## Display scaling

Scaling is GNOME's own; the image ships no code or schema override for it.
mutter 51 runs in logical layout mode (fractional scales, no experimental
switch) and gives each monitor without a stored configuration the scale
nearest to `diagonal DPI / target`, with a 110 DPI target (135 for panels
under 20 inches) among the scales offered for the mode, from the panel's EDID
size: 27" 4K → 150%, 32" 4K → 125%, 24" 1080p and 27" 1440p → 100%, 13.3"
1080p → 125%, 14" 2880x1800 → 167%. What the user saved in
`~/.config/monitors.xml` (Settings) wins. GDM's greeter has its own
`~gdm/.config/monitors.xml`.

- Inspect with `gdctl show --modes --properties` (current scale, mutter's
  `Preferred scale` per mode, the scales offered). Wrong scale on one machine
  usually means a monitor whose EDID has a wrong or aspect-ratio-only size
  (mutter then falls back to 100%): store a choice in Settings.
- `graphical::login` pins this down in the VM. QEMU's virtio-gpu always
  reports ~100 DPI (and ignores DRM debugfs `edid_override`), so the VM's
  automatic scale is 1.0; the test checks that, that 1.5 is offered, and that
  a stored 150% survives a reboot (`06-scaled-desktop.png`).

## CI publishing

`.github/workflows/publish.yml` runs on merges to `main`, nightly at 03:00 UTC
and on demand. A small routing job compares the full ref in Bash and acquires
its concurrency group before calling `publish-pipeline.yml`.
`image.toml` supplies the installer composition (currently
`generic.gnome`) and `info --publications` supplies the validated tag, kind,
parent, machine and ordered shells of every publication. This workflow builds
the installer root first, then every other root, including `base`.
The existing legacy tags (`latest`, `candidate`, bare date and `sha-` tags)
stay in the registry: CI neither moves nor deletes them.

A registry job snapshots each published composition by digest before build or
upgrade starts. A missing tag is recorded as `-`. The same job records legacy
`latest` once if needed. These values select the build comparisons, upgrade v1,
layer-reuse pulls and machine inputs. Three paths run on separate runners:

- `build`: take the recorded snapshots, check selected inputs → build its
  shell selection → `changed` against that
  immutable snapshot → rechunk → unsigned
  staging through `publish --stage --as` → `ephemeral::`.
  From its first minute, `vm-prep.sh ephemeral` prepares the VM stack and pulls
  each root's current digest for compressed-blob reuse, installer first. Only while its tag is
  absent does the reuse pull fall back to legacy `latest`. Staging freezes the
  local ID and checks the pushed manifest's config digest before reporting a
  staged digest and fingerprint. Capability checks and tests boot that same
  immutable rechunked image ID. The installer stages before other roots build,
  so upgrade starts without waiting for `base`. A validated JSON job output
  records each root's tag, own snapshot, changed flag, staged digest,
  fingerprint and tested image ID, only after all ephemeral tests succeed.
  Every root runs `ephemeral::` and capability checks; only the installer runs
  `upgrade::`. The base shares its update implementation, has no installed v1
  yet, and gets no legacy-desktop upgrade. `tool::` remains a local tier.
- `upgrade`: `.github/upgrade-from.sh` selects the installer's snapshot as v1;
  only explicit absence permits legacy `latest` as the first-run fallback.
  Registry/authentication errors are fatal. It records `own` (the installer
  tag's digest or `-`), v1's digest and `exists`. Its cached disk key is
  `v1-disk-boot-mask-<bcvk>-<v1 digest>`, with plain image install options.
  The earlier kernel-argument profile is not restored. It installs
  v1 itself on a cache miss, in the background with the VM stack. It waits for
  build's "Stage the candidate", resolves this ref's staging tag, pulls by
  digest, and runs `upgrade::` on it. With neither source present, `exists=false`, v1 `-`
  and no upgrade tier is the explicit first-publish case.
- `installer-due` → `iso`: described below. Automatic due builds run on main;
  a branch requests an ISO explicitly with `iso=true`.

`publish` requires successful build and upgrade jobs, changed content, this
repository, main and a push/schedule/dispatch event. A skipped or failed test
job cannot reach signing. It first builds the publisher without the signing
key, then invokes that executable directly. `.github/publish-root.sh` checks
that upgrade tested exactly the staged digest (unless neither v1 exists),
validates all explicit outputs, including the fingerprint recorded by staging,
checks the staged registry config against the recorded ephemeral image ID,
applies ancestry to the composition's own
revision, then swaps against **build's own-tag snapshot**. Legacy v1 is never
used as the swap value. `publish --digest <staged> --as generic.gnome` signs
and verifies that immutable digest, then moves the composition and date tags;
it copies with `--preserve-digests`, without another local push.

Every composition `X` has `X_sha-<7 hex>`, `X` and `X_YYYYMMDD` companions.
The SHA reference is populated before signing; only after signature verification
are the moving composition/date tags updated. A failed/cancelled sign can leave
an unsigned SHA reference, but never moves the composition/date tags to it.
Main stages as `candidate.X` in the serialized `publish` group. Other refs use
`publish-<SHA-256 of the exact ref>`: different refs can run together,
including names that differ only by case. A new run replaces the pending
pipeline in its group.
Branch staging tags are `candidate-<ref slug>-<hash>` per ref and root,
with the full case-sensitive ref and root in the hash. Runs on the same ref
reuse those tags. Tags from deleted refs remain until a follow-up adds safe
tag-only cleanup. GHCR's package deletion API removes a whole version, which
can be shared by multiple tags. The v1-disk group writes no image tag.

Immediately before each publisher, `.github/publish-head.sh image` reads that
**tag's snapshotted** revision and main's head and requires published → run → main
ancestry using the API's `ahead`/`identical` statuses. A tested forward run may
publish behind main's head when a newer run never started or was cancelled.
A missing tag/revision permits a first publish, still within main's history.
Backward or outside-main candidates warn and skip green (exit 3). Malformed
labels, registry/API failures and published history unreachable from main fail
red. The digest swap separately requires the tag still to equal its snapshot,
including continued absence for a first publish. Same-commit stale replays skip
green with both digests and `published=false`. Missing snapshots are errors.
Only a real publication sets `published=true`; v1-disk dispatch and machines
on a changed root require it. Failed dependencies and green stale-root skips
start neither. Forced tests do not republish an unchanged fingerprint.

`publish-roots.sh` processes changed roots independently. The `publish` job
handles only the installer root; `publish-other-roots` handles the others,
including `base`, in parallel after the complete build/test inventory passes.
`base` uses its ephemeral evidence without borrowing upgrade outputs. Results
record `published=true|false` per root; v1-disk dispatch depends only on the
installer's result. The release job depends only on the installer publisher:
a failed or slow base publication cannot block it. A stale-root skip continues
to later roots in its job. A hard failure stops that job red; independent
successful publications stay signed. Build/test failures block all signing;
machines use only their own parent's positive publication result. Missing
results from a failed parent publisher skip that parent's layers. A fresh run
catches up. Different revisions on `base` and `generic.gnome` are
not an inconsistent dependency pair: both are independent root builds, and
nothing builds `FROM` the published base. A mismatched machine/parent pair
would be a layer built on a digest other than its published parent; the
per-parent result and parent-current guards forbid that.

`machines` reads the five-column publications table and each parent's row in
the tested-root inventory. Each row
retains its composition tag, parent tag, machine and shell list. The parent is
the exact staged/tested digest when changed in this run; main requires it to
have published. An unchanged main root uses its initial signed digest,
even if `test=true` staged an unsigned equivalent. Branches use the staged
digest when available and the initial snapshot otherwise. `machine-base.sh`
requires root configuration labels, verifies the
pulled image ID equals the registry config digest and checks generated-file
and overlay metadata. There is no unlabelled legacy-parent skip: these new
parent tags can only have been created by the composition publisher.

Planning uses each layer's initial snapshot and compares inputs and parent ID using
that immutable reference, without pulling the large root when unchanged. A
missed machine publication catches up through the parent ID in the machine
key. Neither unchanged inputs nor a full unchanged fingerprint republish;
`test=true` still tests. An unchanged nightly does not query DNF for
machine-only package updates: those arrive on the next parent change or forced
build. Registry reads (including root `changed`) are authenticated with the job token; only explicit
manifest/name unknown is absence. Denied/unauthorized and transport errors fail.

A machine preserves every parent layer and adds one squashed layer; no rechunk.
`prepare-publish --machine` labels its fingerprint before capability checks and
`ephemeral::`, which boots the immutable ID. All selected tests must pass before
the signing step receives its key. The tested inventory retains composition,
parent tag, parent digest, image reference, tested ID, own-tag snapshot and changed
flag. `.github/publish-machines.sh` checks each composition's own ancestry and
snapshot and requires **its parent tag** still to equal that tested digest.
An individual ancestry/snapshot skip continues to later rows and is recorded in
the inventory and summary. A moved parent fails with both digests. A whole-job
retry plans/builds/tests again; it never replays only an old signing step.

`publish --as` requires a listed composition and recomputes its tag from all three
selection labels against the row's complete declared selection. Matching the
tag spelling alone is insufficient. Roots accept `--stage` or `--digest --tested-fingerprint`; layers require
`--tested-image-id`. A layer must still have that ID and the independently
recomputed fingerprint label. Publication builds nothing. Every signed image
must carry exactly this run's clean 40-hex `GITHUB_SHA` revision; a layer checks
its own revision, while an unchanged parent may be older. Local pushes freeze
the ID and check the pushed config digest before signing. Staging independently
recomputes the root fingerprint, checks the clean CI revision and reports both
`fingerprint=` and `digest=` before tests. CI carries both through the successful
build/test jobs; the root signer compares the staged config's fingerprint with
`--tested-fingerprint` without pulling/executing the root. Each fingerprint must
be a SHA-256 digest. The layer publisher independently resolves its declared
published parent, verifies all three parent labels, current machine inputs/key
and exactly the parent's filesystem diff IDs plus one layer. CI's ancestry,
snapshot and parent-still-current guards remain necessary too. The resolved
parent must also verify against the same permitted public key before signing
a layer.

All signing jobs pin kache-action to `ad7317540dcdd71c904f7a60a87b620a0c0e67ed`,
binary v2.1.1, empty token and PR comments disabled; cosign-installer v4.1.3 is
pinned to `756fe2d284c7292c4e9b62c8a28f1e5629c59532`. `build-publisher.sh` builds
with `--locked` and discovers the executable from Cargo metadata before the
key-bearing step. No Cargo, rustc, kache or build script runs with the key.
That includes the one thing the publisher used to ask cargo for: the root
image's crate closure for the fingerprint (`cargo metadata` starts rustc, so a
`RUSTC_WRAPPER` such as kache, and can fetch from crates.io). `build-publisher.sh`
runs `ilexctl info --image-crates` in its keyless step (the build script's
three printing modes under python3, `cargo fetch --locked`, then
`cargo metadata --offline --locked`) and records the closure in
`$RUNNER_TEMP/image-crates`, bound to the checkout's source digest, exporting
`ILEX_IMAGE_CRATES`. `ilexctl publish` (anything but a loopback registry
with a throwaway key) refuses to start without that file, reads it, checks the
digest and forbids cargo for the rest of the process: the only code that
starts cargo (`fingerprint::closure` and `fetch_dependencies`) fails once the
record is in use, so `rechunk::fingerprint` cannot reach it. The log line is
`image crates (signing process): N crates read from … ; cargo, rustc and kache
are not run by this process`. `publish-root.sh` and `publish-machines.sh` also
start the publisher with `.github/no-cargo.sh`'s shims first on `PATH`
(`cargo`, `rustc`, `rustup`, `rustdoc`, `kache` exit 97 with an error), so a
regression is a red step before a signature exists. Their `signing()` helper
also unsets `CARGO`, `RUSTC`, `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER` and
`RUSTDOC`, which could point a toolchain somewhere other than the shims. `ilexctl doctor
--self-check-recorded-crates <image>` runs the same call with `ILEX_IMAGE_CRATES`
set and prints the fingerprint it gets without cargo; it must equal
`doctor --self-check-fingerprint <image>`'s `rechunk_fingerprint`.
The key/password are step environment values; the installer and cache post
steps have none. Guard subprocesses explicitly remove them. The trusted signing
shell, prebuilt publisher and required git/podman/skopeo/cosign tools retain
them; podman does not forward them into the image's RPM query. This is not
process isolation. The packages-write job token also authorizes registry reads;
action inputs are not a per-step credential boundary. Checkout credentials are
not persisted in machines. Other jobs retain their existing action settings.

<a id="published-history-recovery"></a>

**Published history recovery (owner action, never an agent publishing locally).**
A guard reporting published history outside main has no override. If the old
revision may remain in history, the owner fetches it, merges it into main with
`git merge -s ours <rev>` (preserving the tree) and pushes normally. If it must
stay purged, identify the exact digest and all its tags, then delete that ghcr
**package version**, not the whole package. The next fresh run sees the affected
tag as a first publish. Removing the installer composition may restore the
legacy upgrade fallback; only with both sources absent does upgrade skip while
ephemeral still runs. Removing a layer version does not bypass root tests.
For the installer, deleting its release and tag (`gh release delete
installer-… --cleanup-tag`) exposes the previous installer or first publication.
Repeat for every affected tag/release. Do not sign or retag locally. Ordinary
reverts keep ancestry and require no recovery.

Never re-run a pre-guard workflow: it uses its original, unguarded definition.
After anything newer stages or publishes, start a new full main run instead of
repeatedly clicking "Re-run failed jobs" on an old run. A stale root swap skips
green; a candidate mismatch fails safely with staged/tested digests; a moved
machine parent fails. A transient read/API error can justify retrying the newest
run. A machines-job retry while its parent is current re-plans and re-tests.
Partial publication is not undone: a fresh run/nightly catches up missing layers.

**First run on main.** Merge the entire stack as one push: its top PR into its
base, then each result down through #32 and the earlier stack into #25,
finally #25 into main. Non-main pushes start no publish run. Bottom-up main
merges would execute older workflow definitions and move legacy tags.

Today legacy `latest` exists with revision on main; `generic.gnome` and
`ms-7c35.gnome` are absent. In this PR, build records `generic.gnome=-`, stages
`candidate.generic.gnome` by ID/config and boots it. Upgrade logs the legacy
fallback, with `own=-`, `exists=true`, v1 equal to legacy latest's digest, and
tests that staged digest. The upgrade job verifies this explicit v1's signature
and inspects its booted `/usr`; if the vendor unit is absent, only v1 receives
the transient repair. This also covers the first post-merge main run when the
old workflow's nightly has republished `latest` at another digest. The staged
candidate and v2 must still ship the mask. Root ancestry logs `published=-` / `first-publish`;
swap logs `expected=- current=-; allow`. The publisher checks composition and
revision, copies `generic.gnome_sha-…`, signs/verifies, then creates
`generic.gnome` and `generic.gnome_YYYYMMDD`, logging `published=true` and
starting v1-disk for the new installer tag. Machines tests `ms-7c35.gnome` on
that exact digest; its own first-publish/absent-swap and parent-current guards
allow, then tested ID/config/revision checks precede sign/verify and tag moves.
ISO is due because its fingerprint changed and builds in parallel into the
`installer-iso` artifact. `installer-release` waits for successful build/upgrade
and actual default-composition publication (`published=true`), but does not
wait for `machines`: that job is not in its `needs`. On the first publication,
`generic.gnome` can be signed while `ms-7c35.gnome` is still being built/signed.
The all-composition guard runs before cosign; any missing offered tag or
signature tag, including the default's, makes `installer-target.sh` return
`available=false` successfully. Installation and release are skipped, the ISO
remains due, and the next full main run/nightly retries.
Once all offered tags/signature tags exist, the job verifies `generic.gnome`'s
resolved digest with the committed public key, installs that exact ISO with
`iso::`, and verifies the same tag/digest/signature again before the independent
installer ancestry guard and release. The first installation test therefore
waits for the full offered inventory, possibly until a later run.
An unchanged root permits the intentionally skipped publish job; a publication
guard skip, failure or failed image test prevents installer release. Missing
tags/signature tags before installation defer green; registry errors, a present
but invalid default signature or failed installation fail red. The ISO remains
an artifact and the previous release stays latest.
CI also publishes `base`: its own absent snapshot, capability check and ephemeral
boot permit `base_sha-…`, signature/verification, then `base` and
`base_YYYYMMDD`. It has no upgrade tier or v1-disk dispatch. Healthy lines are
`root build duration: base … seconds`, `publish ancestry guard: …:base
published=- … first-publish … allow`, `publish digest swap: …:base expected=-
current=-; allow` and `root publication result: base published=true`.

The second full run resolves v1 from `generic.gnome`, with no legacy read. It
snapshots that root and each layer independently. Unchanged content skips
publication; changed content swaps only from its own snapshot. If the layer
failed on the first run, planning detects its missing/stale parent key and
catches up. Healthy logs include `publish ancestry guard: …:<tag> … allow`,
`publish digest swap: …:<tag> expected=… current=…; allow`, matching pushed
config/tested ID, exact CI revision, signature verification, `root publication
result: generic.gnome published=true` and `machine publication summary: 1
published, 0 skipped`. Real-key/ghcr signing, production ancestry/swap outputs,
unchanged-main skips/recovery (including the pre-build root-input skips),
v1-disk follow-up, production installer-target verification, `iso::`
installation and guarded ISO release await
the first main execution. Branch tests cannot establish those paths.

After publication, the independent v1-disk run installs the installer composition
and caches bcvk's base disk (~5 GB, ~1.9 GiB compressed) with its two
`user.bootc.*` xattrs recorded beside it. Superseded disks are removed from the
Actions cache. A cache miss costs the next upgrade run an install (~14 minutes
on a runner). The runner is `vars.ILEX_RUNNER` or `ubuntu-26.04`; its 145 GB root
has enough space without clearing toolchains. VM packages use the apt-debs
Actions cache; background logs have timestamps. Rechunk runs before both tiers:
testing the unrechunked image breaks layer sharing in the upgrade test.

The extra base shares the Rust build and package layer locally, then adds its
own small final build, rechunk, stage and ephemeral boot. Budget: about 3–4
minutes when changed, less than a minute when unchanged. It does not delay
installer staging or the start of upgrade. Each published root records
`ilex.root-inputs`: selected source trees/selection plus both Fedora build
image digests. `changed --root <tag> --inputs-only` compares that key against
the root's own immutable snapshot before building. Missing keys force a build.
After a miss the existing v3 RPM/content fingerprint still decides publication;
a forced test never republishes unchanged content. Shell packages, manifests
and overlays affect only roots selecting them. Unshipped `shells/ilex/design/` material
affects neither root inputs nor content fingerprints. Base packages,
`rootfs`, shipped crate trees and common build helpers affect both roots. A
base-only input change cannot leave the desktop unchanged when that input is
shared; independent RPM/content outcomes and snapshots can still let either
root skip while the other publishes. Neither changing `[[publish]]` alone nor
an unrelated shell changes an existing root's inputs. Fedora images are pulled
once before these checks, so nightly upstream changes invalidate both keys.

```sh
ilexctl info --publications
ilexctl info --installer
ilexctl changed ghcr.io/reidond/ilex:base --root base --inputs-only
ilexctl build --shell none
ilexctl changed ghcr.io/reidond/ilex:base --image <base-image>
ilexctl changed ghcr.io/reidond/ilex:generic.gnome --image <image>
ilexctl rechunk <image> --output <rechunked>
ilexctl publish <rechunked> --stage --as generic.gnome
ilexctl publish --digest <staged-digest> --as generic.gnome --tested-fingerprint <fingerprint>
ilexctl changed <ref> --machine ms-7c35 --inputs-only --base-id <config-digest>
ilexctl prepare-publish <machine-image> --machine ms-7c35
ILEX_IMAGE=sha256:<tested-id> ilexctl test ephemeral::
ilexctl publish <machine-image> --as ms-7c35.gnome --tested-image-id <tested-id>
```

Publishing and releases only happen in CI. A branch dispatch with `test=true`
builds, stages each root under its branch tag, runs their ephemeral tests,
upgrades the installer (including legacy fallback) and tests machine layers.
Upgrade and machine tests use registry digests. Encrypted qualification uses
the immutable local image ID and a disposable signed loopback fixture.
Branch runs sign nothing in GHCR and move no published tag. Add `iso=true`
to build the ISO as an artifact on a branch; the installation/release job is
main-only because its network install exercises the published target, not the
branch candidate. PR/fork events cannot reach signing.

The sole local publishing override is an explicitly allowed loopback registry,
never ghcr. `--allow-local-registry` is refused whenever `CI` or
`GITHUB_ACTIONS` is set (to any non-empty value): a throwaway-registry run
derives the crate closure with cargo and must not share an environment with
signing secrets. A registry counts as loopback only for `localhost`,
`127.0.0.1` or `[::1]`, with or without a port. Run a checkout-specific `registry:2` container
on a free local port, generate a throwaway cosign v2 key pair in a scratch
directory, then use `--allow-local-registry --repository localhost:<port>/ilex`
and `--verification-key <scratch>/cosign.pub` with `publish --as`. Set only that
throwaway `COSIGN_PRIVATE_KEY`/`COSIGN_PASSWORD`. Stage before signing roots;
prepare and test layer IDs first. Stop/remove the container and scratch keys
afterwards, and remove only this checkout's image names. Never prune shared
container storage or read/use the owner's key for local tests.

Signatures use cosign **v2** key-pair legacy `.sig` tags; bootc/containers-image
cannot use keyless or cosign v3's default format. Policy and the public key live
in `rootfs/etc/containers/{policy.json,registries.d/ilex.yaml}` and
`rootfs/etc/pki/containers/ilex.pub`. The policy's default is `reject`: bootc's
import library refuses container-policy pulls when the effective policy's default
is exactly one `insecureAcceptAnything` entry, even if a repository has its own
signature rule. Requirements in an array are ANDed: adding that entry alongside
a signature requirement or `reject` does not trigger bootc's singleton check.
The explicit transport allowlist covers Fedora 45's `atomic` (deprecated
OpenShift), `docker`, `containers-storage`, `dir`, `docker-archive`,
`docker-daemon`, `oci`, `oci-archive`, `sif` and `tarball` transports through
their empty scopes. The more specific ilex and existing Red Hat
registry signature requirements still take precedence. Other podman, toolbox,
Flatpak OCI and bootc pulls keep their previous behaviour. A future transport
must be listed explicitly before it can be used.

The agent follows the policy lookup in Fedora 45's packaged bootc 1.17.1 and
1.17.4 (`crates/ostree-ext/src/container/skopeo.rs`): a non-empty
`CONTAINERS_POLICY_JSON` override, then
`$XDG_CONFIG_HOME/containers/policy.json` (or `$HOME/.config/containers/policy.json`
when XDG is unset), then `/etc/containers/policy.json`, then
`/usr/share/containers/policy.json`. An explicit override that cannot be opened
is an error; other candidates fall through on open errors. Paths are resolved
from `/`, including relative environment values. Diagnostics name the selected
policy. Check the environment used by the root updater as well as an interactive
root shell; `/root/.config/containers/policy.json` can override a fixed `/etc`.

**Recovering an existing machine.** An already enforcing machine with the old
singleton default cannot fetch this fix. Back up its effective policy, edit
that default to `[{"type":"reject"}]`, and merge the explicit empty-scope
transport allowances from `rootfs/etc/containers/policy.json` into missing
scopes. Preserve every existing local scope, empty-scope rule, signing key and
signature requirement. For a machine using the system policy:

```sh
sudo cp -a /etc/containers/policy.json /etc/containers/policy.json.before-ilex-repair
sudoedit /etc/containers/policy.json
sudo systemctl restart ilex-sigpolicy.service
sudo bootc upgrade --check
sudo bootc upgrade
```

If a higher-priority policy is selected, back up and repair that file instead.
Keep origin enforcement enabled throughout. On upgrade, OSTree replaces an
unchanged old `/etc` policy with the new default, but retains a locally edited
policy as a whole; it does not merge individual JSON keys. Compare that retained
file with the new shipped policy and merge the default and missing transport
allowances while keeping local rules. The agent's guard reports a retained
singleton insecure default when that policy is selected. The containers-storage
migration test covers an unchanged `/etc` policy; it does not establish
automatic recovery of an already blocked registry client.

Verify a published composition with
`cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true
ghcr.io/reidond/ilex:generic.gnome`. The private key is a repository secret; losing
it requires deploying a new public key to machines before they can update.

### Installer ISO

The installer is a **network installer** published as a GitHub Release: it
carries no ilex image, so one small ISO stays current by itself and fits
GitHub's limit of 2 GiB per release asset (an ISO with the ~5 GB image in it
was 2.6 GiB). Download it, write it to a USB stick and boot it; Anaconda's web
interface asks for language and keyboard, network, date and time, installation
method, storage, an account, then review and install. It installs the newest selected ilex composition
over a (wired) network connection and the machine updates itself from there:

```
https://github.com/Reidond/ilex/releases/latest/download/ilex-installer.iso
https://github.com/Reidond/ilex/releases/latest/download/ilex-installer.iso.sha256
```

Because it always installs the newest image it doesn't follow every publish.
On every run, `ilexctl iso --due` decides, in the `installer-due` job (the `iso`
job reads its `iso_due=`): a new ISO is due when there is no `installer-*`
release yet, when installer inputs differ from the latest one (its notes carry
an `installer-fingerprint:` line), or when the
latest one is 7 days old (Fedora and Anaconda fixes). A manual run with `iso`
checked (`-f iso=true`) builds one regardless. `iso` constructs it in parallel
with image CI and uploads `installer-iso` (ISO, checksum, catalogue and notes) on every
branch, including main. On main only, `installer-release` downloads that exact
artifact after successful image tests and either actual publication or an
unchanged root. It cannot run after a failed dependency, a cancelled run or a
green publication guard skip (`published=false`). An unchanged root accepts
only the intentionally skipped publisher with `changed=false`. Branches stop
at the artifact: no `iso::` tier and no release, since the tier installs the
published image rather than the branch image.

The installer generates `compositions.json` from every published composition
with at least one shell, their descriptions and the machines' match rules.
The default is `image.toml`'s `installer` (`generic.gnome`); its ordered shells
also determine the detected machine's composition. `base` and shell-free
machines are excluded. The boot menu has one entry, **Install ilex**.
Selection precedence: the last `ilex.composition=<tag>` kernel argument, then
`/run/ilex-installer/choice` (a kickstart can write it before including the
defaults), then DMI detection. An unknown override fails with the offered tags;
no match, conflict or unavailable default shell list falls back to the default.
The helper prints `ILEX-COMPOSITION`, generates a validated `bootc` include
and updates the installer's os-release, which the web interface shows in its
title and the review screen's **Operating system** line.

For a local graphical install, the first screen shows the detected computer,
machine configurations and published desktop lists. The detected machine and
the default desktop list are preselected (generic when nothing matches).
Desktop lists unavailable for a machine are disabled. Tab/Shift+Tab move
between groups and Continue, arrows select within a group, and Enter continues.
The screen closes, then Anaconda starts on the same VT: choose language,
network/time, storage and account, and review the chosen operating system
before writing the disk. To revise the machine or desktop after continuing,
restart the installer. On the MSI PC, choose **Any x86-64 machine** to install
generic instead.

`ilex-installer-chooser.service`, before both Anaconda startup units, runs
`/usr/libexec/ilex-installer/chooser-session`: a Python standard-library server
on IPv4 loopback only, a fresh GNOME Kiosk Wayland session on VT 6 and Firefox
with Anaconda's own profile. The page uses the bundled WebUI stylesheet, with
a small CSS fallback. Three exact token-prefixed routes serve the page, CSS
and form; no request supplies a filesystem path or a command. A random port,
per-session 128-bit token, Host/Origin checks, CSP and a strict 4 KiB form
limit protect the root server. Only a machine/ordered-desktop pair in the
validated generated table can be selected. The server atomically writes
`/run/ilex-installer/choice` as schema-1 JSON (`schema`, `composition`) and
exits on its first valid submission; `apply` revalidates it. Plain tag files
remain accepted for kickstarts. This extensible record contains no profile
choice, passwords, passphrases or recovery material. The chooser never collects
them; neither its URLs nor the generated table contain them.

The chooser is skipped for `ilex.composition=`, `inst.ks`, `inst.text`,
`inst.cmdline`, `inst.webui.remote`, `inst.rdp`, or an OEMDRV kickstart found at
`/run/install/ks.cfg`. Kiosk/server failure falls through to detection; `%pre`
waits for an activating chooser before reading its answer. The private URL
file is removed during session cleanup. The supervisor leaves Anaconda's
`run-in-new-session` launcher alive while its child closes the browser and
compositor, so the launcher can wait and close PAM. Teardown has a 15-second
grace period, then bounded TERM/KILL of the actual session process group and
tracked descendants, orphan reaping, and explicit retirement of that logind
session/scope. PID start times protect against signalling a reused PID.
Both Anaconda units require the chooser and recheck the old processes/session
in ExecStartPre. If teardown cannot be verified, startup fails closed rather
than racing a seat still owned by the chooser; ordinary browser failure still
cleans up and falls back to detection. Diagnostics keep their ordinary
journal/Anaconda output. Console mirrors share `diagnostics.py`: open with
`O_NONBLOCK|O_NOCTTY`, attempt at most 4096 bytes once, and skip unavailable,
backpressured or partial output without retrying or draining. Each disposable
worker gets a 200 ms wait budget and, if needed, SIGKILL plus a separate
200 ms reap budget. This also isolates a UART driver's last-close wait from
the control path; the real console's termios and queued output are untouched.
Server startup, HTTP self-check, kiosk lifecycle/error messages, the startup
gate and `apply`'s ttyS0/tty0 mirrors all use this helper. Normal journal I/O
is retained; the budgets apply to optional console workers, not every OS I/O
operation. No hardware serial port or successful serial write is required
for a handoff.
`ILEX-CHOOSER default=… offered=…` on serial records
the offered defaults without logging the capability URL.

On **Create account**, create a separate **maintenance/recovery administrator**
such as `ilexmaint`, and keep its credentials for administration and recovery.
Reserve `andriishafar` for the owner's daily homed account; never use the selected
daily username for maintenance. The maintenance user is a traditional local
account. A separate daily user managed by systemd-homed and created at
first boot is planned for a later PR; that setup is not delivered here.
The stock account page is retained without patching the web interface's bundles.

Storage offers erase-all and use-free-space installation, with a reclaim-space
dialog to shrink or delete existing partitions for dual boot. Check the storage
selection and review before starting installation. The UI exposes Anaconda's
storage options; this PR qualifies the ordinary bootc install only.

Locally, Firefox in GNOME Kiosk displays Cockpit's Anaconda application.
`webui-cockpit-ws.service` serves it on `127.0.0.1:80` with a root bridge
(`--local-session`); it is not exposed to the network by default. Remote browser
installation is opt-in with `inst.webui.remote` and
`inst.webui.remote.pin=<PIN>` on the boot command line, using HTTPS. Do not put
that PIN in shared logs or screenshots. Anaconda's network geolocation may
preselect language and time zone; `inst.geoloc=0` disables that lookup.

The installer reserves `/etc/ilex/setup.json` on the target, schema 1, with
the selected composition/image, null profile ID/schema, requested TPM mode
`unspecified`, actual capability `unknown` and status `not-configured`.
The catalogue reserves empty `profiles` and `security-capabilities` objects.
The final encrypted-install completion updates TPM capability/status and adds
private local recovery acknowledgement and PCR-7 enrollment. Encrypted home
setup and profile provisioning remain later work. See `installer/setup/README.md`
for extension rules; future helpers below `installer/` enter the fingerprint.

The fingerprint covers all of `installer/` (including the reserved setup helper
directory), the ISO/table-generation code, generated catalogue/menu and the
committed registry configuration/public key. Adding a publication or match rule
makes an ISO due. There are no `--source`/`--target` overrides; local
`--testing-tag <tag>` maps all entries to one signed ilex tag and is labelled
in both the catalogue and release notes. It is forbidden in CI.
The release job has `packages: read`, logs in using the job token and a private
auth file shared by skopeo and cosign, and installs cosign v2 through the same
pinned cosign-installer action as publication. Before default-target cosign
verification, `ilexctl iso --check-registry` checks every offered tag and
its digest's legacy cosign signature tag. Explicit manifest/name absence for
any tag or signature tag, including the default's signature tag, exits 3;
`installer-target.sh` then sets `available=false` and exits successfully,
skipping installation and release. This can happen on the first publication
because `installer-release` does not wait for `machines`. The still-due ISO is
retried on the next full main run/nightly. Denied/transport errors exit 1 and
fail the job. This is a presence guard, not cryptographic verification of every
offered tag.

Once that guard passes, the job resolves the default tag and verifies the
immutable `<repository>@<digest>` with
`cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true`.
A present but damaged or wrong-key default signature fails red at verification;
it does not take the missing-signature-tag deferral path. The installer itself
requires the ilex signature for whichever image it installs. The inventory is
checked again before release; disappearance after a successful installation
is an error rather than a deferral.

The job prepares KVM/QEMU/libvirt with `vm-prep.sh`, restores its apt package
cache (v2 also includes `xorriso`, `e2fsprogs`, `openssh-client` and `ovmf`),
checks disk space and the ISO checksum, then runs
`ILEX_ISO=<downloaded artifact> ilexctl test iso::` with the verified tag and
`ILEX_ISO_DIGEST`. No ilex container is built by that invocation. Before any
selected ISO trial runs, the harness checks all host commands (`xorriso`,
`qemu-system-x86_64`, `qemu-img`, `mkfs.ext4`, `skopeo`, `ssh`, `ssh-keygen`)
and reports missing prerequisites with package hints. The tier boots
Anaconda, installs onto a sparse blank 40 GiB disk, boots it, and asserts the
booted and tracked `generic.gnome` registry reference, the exact verified
digest, system `running`, no failed units and GDM up. It requires KVM and 6 GiB
for the installer VM, on the same runner selection as other tiers; it adds
about 15 minutes only when an ISO was built. Main runs serialize publication,
so the target tag stays stable between verification and installation. Before
release it also re-reads that tag, requires the same digest, verifies its signature again and
rechecks the ISO checksum. A changed/deleted tag cannot release the artifact.

Only a successful `iso::` step can create `installer-YYYYMMDD` (`-N` suffix if
needed), marked latest, with `GITHUB_TOKEN` and `contents: write`. A failed or
skipped installation creates no release; the previous release remains latest.
The ISO stays in `installer-iso`, and available screenshots/serial logs/journals
are retained as `installer-test-evidence` on success or failure (three days).
The next fresh full main run retries an installer still due. Before the first
published `generic.gnome`, a local production-target installation cannot run;
local testing uses an explicitly labelled `--testing-tag latest` build instead.

Immediately before creating a release, `installer-release` applies the same ancestry
guard independently of base publication. `.github/installer-revision.sh` reads
the newest published, non-prerelease `installer-*` release and resolves its
actual tag to a commit (the release was created with `--target GITHUB_SHA`).
It does not trust `target_commitish`, which could name a moving branch. The
previous installer commit must precede/equal the run, and the run must
precede/equal main. An old re-run skips green before `gh release create
--latest`; read/API errors or rewritten published history fail red. With no
installer release yet, only the run-to-main condition applies. This guards
image signing/tag movement and installer release creation, not the builds or
unsigned candidate staging that happen before them.

```sh
ilexctl iso                # build; the ISO, its .sha256 and release-notes.md land in <state dir>/iso/
ilexctl iso --due          # is a new ISO due? (reads the latest installer release; needs gh)
```

- `installer/Containerfile` is `fedora-bootc:45` plus `anaconda-core`,
  `anaconda-tui` and `anaconda-webui` (Cockpit and Firefox). The `anaconda`
  metapackage would select GTK on this base's basic Fedora identity.
  `slitherer`, a second browser with QtWebEngine, is excluded and both it and
  `anaconda-gui` are checked absent. `NetworkManager-wifi` is explicit, keeping
  the Wi-Fi support formerly pulled in by GTK; wired networking is recommended.
  Its kickstart
  (`installer/interactive-defaults.ks`) installs `registry:ghcr.io/reidond/ilex:<selected tag>`
  (`--source-imgref`; pykickstart requires the transport) and tracks
  `ghcr.io/reidond/ilex:<selected tag>` (`--target-imgref`). The installer drops GPU
  firmware and Vulkan drivers before it builds its initramfs. The local web ISO
  measured 1,953,595,392 bytes (1.82 GiB) and 694 seconds to build on 2026-11-16
  (`--testing-tag latest`); `firefox-langpacks` remains installed. `ilexctl iso`
  fails if the ISO reaches 2 GiB.
- The installer base is not cached between runs (an `ilex-installer-base`
  package was tried, and saved ~11 of 22 minutes per build): the weekly rebuild
  exists to pick up Fedora and Anaconda fixes, which a cached base would pin,
  and the build is now rare.
- `image-builder` needs root, the `generic-iso` type needs **image-builder
  ≥ 89** (Fedora 45 `updates`; 87 only has the unattended,
  disk-wiping `bootc-installer` type) and storage that isn't overlayfs. The CI
  job starts its own rootful, privileged `fedora:45` container with
  `sudo podman run` on the runner, after unloading Ubuntu's
  `bwrap-userns-restrict` AppArmor profile: it attaches to any
  `/usr/bin/bwrap` by path, even inside an unconfined container, and its
  `unpriv_bwrap` child denies the setuid/chown/sys_admin osbuild's sandbox
  needs ("mount: … permission denied"). `/var/lib/containers`, `/var/cache/image-builder` and
  `TMPDIR` live on volumes under `/mnt`. `ilexctl iso` does the same locally: when
  the `image-builder` on `PATH` is older than 89 (or missing) it builds
  `installer/image-builder/Containerfile` and runs image-builder from that
  container against this host's container storage. The installer and fallback
  image-builder helper tags both include the checkout slug.
- Anaconda is branded by `/.buildstamp`, the file it takes its
  product name, version and "final release" flag from (`Product=ilex`,
  `Version=45`, `IsFinal=True`). Without it the installer calls itself
  "anaconda bluesky" with a red pre-release banner. The web interface reads
  the installer's `/etc/os-release`: `NAME="ilex"` for its welcome heading,
  `PRETTY_NAME` for its title, about box and review. This is a real `/etc` file,
  replacing the symlink; Fedora's `ID`/`VERSION_ID` and `/usr/lib/os-release`
  stay unchanged. `%pre` refines `PRETTY_NAME` with the selected machine,
  desktops and tag. The Fedora logo is Fedora's artwork; ilex has none yet.
- Anaconda reads `interactive-defaults.ks` only when it has no kickstart: an
  unattended install has to `%include /usr/share/anaconda/interactive-defaults.ks`
  (without it Anaconda falls back to an RPM install from a mirror). The defaults
  also set `xconfig --startxonboot` (Anaconda otherwise picks `multi-user.target`
  for a bootc deployment, so no GDM) and drop the `/` line Anaconda writes into
  `/etc/fstab` (with it `systemd-remount-fs.service` fails on the ostree root and
  the machine boots `degraded`).
- The installer VM of the `iso::` tier gets 6 GiB: pulling from the registry
  streams the layers into the new root. (With the image embedded in the ISO,
  importing it from the installer's container storage unpacked layers into
  its RAM-backed `/var/tmp` and needed 16 GiB; 6 GiB failed with "no space
  left on device".) The installed system still calls itself "Fedora Linux 45"
  (its os-release) and its hostname is `fedora` until one is set.
- Kickstart's `bootc` command cannot pass `--enforce-container-sigpolicy`.
  The installer enables it through `/usr/lib/bootc/install/90-ilex-installer.toml`
  instead, so the target origin records `signature: containerPolicy` from the
  initial installation. Its own policy rejects everything except signed ilex
  images, using the same committed public key and registry signature config.
  Unverified pulls still apply `policy.json`: signed ilex images are accepted
  and unsigned ones rejected while that policy is present. Origin enforcement
  via `containerPolicy` additionally refuses a singleton insecure default in
  the selected policy. OSTree-remote verification does not apply that ban.
  The image retains a safety net for machines installed otherwise:
  `ilex-sigpolicy.service` (enabled by the preset, `ConditionPathExists=
  /run/ostree-booted`, ordered before the updater) runs `ilexctl system
  enforce-sigpolicy` at every boot. When the booted deployment tracks
  `ghcr.io/reidond/ilex` (any tag or digest) over the registry transport with
  no verification, it rewrites the origin with `bootc switch --mutate-in-place
  --enforce-container-sigpolicy <same ref>` (no download, no new deployment, no
  reboot) and checks that `bootc status` now shows `"signature":
  "containerPolicy"`. It does nothing on ephemeral VMs (no deployment), other
  images or transports (test images in containers-storage, other registries,
  `ghcr.io/reidond/ilex-iso`) and machines that already verify, so there is
  nothing to run by hand; `ilexctl system enforce-sigpolicy --dry-run` shows what
  it would do. An OSTree-remote-verified origin is a successful no-op without
  consulting container policy, including in dry-run mode. For container-policy
  origins, and before adding that enforcement, the agent resolves bootc's
  effective policy and refuses a singleton `insecureAcceptAnything` default with a non-zero
  exit and an explanation naming that file: changing the origin
  with that default would stop updates. The agent's self-check checks it too.
  `/sysroot` is mounted read-only on a running system and bootc
  doesn't remount it for `--mutate-in-place` ("Writing origin: Read-only file
  system"), so the agent runs the switch under `unshare --mount` with
  `/sysroot` remounted writable in that private namespace only.

### One-time GitHub setup (the user runs this, not an agent)

1. The owner supplies the GitHub Actions `COSIGN_PRIVATE_KEY` and
   `COSIGN_PASSWORD` secrets from their private backup. Signing credentials
   stay outside the development tooling and builder.

2. Make the `ilex` ghcr package public so machines and the installer need no
   credentials. The package only exists after the first publish: run the
   workflow once (`gh workflow run publish.yml --repo Reidond/ilex`), then open
   github.com → your profile → Packages → `ilex` → Package settings → Change
   visibility → Public. There is no API for this.
3. Check that Actions may publish: Settings → Actions → General → Workflow
   permissions should allow the `packages: write` the workflow requests, and
   a package created by the first run must list this repository under
   "Manage Actions access" (it does when the image carries the
   `org.opencontainers.image.source` label, which `ilexctl build` sets).
4. Nothing to set up for the installer: its releases use `GITHUB_TOKEN` (the
   `installer-release` job asks for `contents: write` and `packages: read`; if Settings → Actions → General →
   Workflow permissions is read-only by default, a job-level `permissions:`
   still raises it). The old `ilex-iso` package, if it exists, is unused and can
   be deleted, and so can `ilex-installer-base`.

### Installer profile setup

The pre-Anaconda chooser offers an installed public profile, a custom
daily user, or No daily profile. No daily profile is the default and uses
Anaconda's standard account and storage options. For daily setup, enable
LUKS2 in Anaconda and create a distinct local maintenance administrator
such as `ilexmaint`. The installer reserves a selected profile's username for
the daily home. On a machine without eligible TPM enrollment, unlock the
installed disk with the install passphrase or saved recovery key.

On first boot, sign in to the maintenance account and run
`sudo ilexctl home setup` in a terminal. For a custom daily user, supply
`--name NAME` and optionally `--disk-size GiB` (default 20). Supply a
distinct password at homectl's prompts. If setup is interrupted, rerun the
command. Correct the custom username or capacity while no home exists.
After creation, enter the daily password to verify the home and resume.
Setup runs offline and leaves the
maintenance login available on error. The selected public profile is applied
as the daily user on the first authenticated login. Profile apps remain opt-in.
See [the handoff contract](installer/setup/README.md).

The local acceptance trials are `iso::chooser`, `iso::profile_chooser`,
`security::installer_no_profile` and `security::installer_profile_setup`.
Run them with `cargo ilexctl test FILTER -- --test-threads 1`.
The profile setup trial uses an exact ISO and an isolated, signed candidate
selected with the installed machine and GNOME.
It exercises passphrase-only boot, a blocked setup retry, a local
username collision, password login as the installer-created maintenance
account, refusal of malformed maintenance hashes, custom capacity
correction, damaged-home refusal, transaction
reconciliation and authenticated profile application without network
access. It does not establish Secure Boot or
TPM hardware qualification.
For the profile trial, set `ILEX_PRIVATE_FINGERPRINTS` to a fresh private
directory outside the artifacts. Seal the successful export with
`encrypted/privacy.py --seal-profile-export` before attaching screenshots.

## The next installer

`installer/next/` is the live installer of ilex's own installer, which will
replace Anaconda. This version delivers the live image only: no Anaconda,
Cockpit, Firefox or Python chooser, and a placeholder interface that names
the edition and shows the machine's DMI identity. Nothing is installed. The
Anaconda network installer above is unchanged and stays the released one:
`installer/next` is excluded from its fingerprint (`ilexctl iso --due`).

```sh
ilexctl iso --next [--edition gnome] [--output-dir D]  # <state dir>/iso-next/ilex-installer-gnome.iso
ilexctl test iso::next                                  # boots it: Secure Boot, both boot entries, fallback, restarts, serial faults
```

**Linux only for now.** From native Windows, `ilexctl iso --next` and
`ilexctl test iso::next` are refused with a message before anything is sent:
the builder returns only `<state dir>/iso` (the Anaconda ISO) and stages only
`ILEX_ISO`, while this ISO is written to `<state dir>/iso-next` and read from
`ILEX_ISO_NEXT`, so it would never reach the machine. The worker refuses them
too, and an `iso::next` filter no longer makes the client stage the Anaconda
ISO. Run these two on a Linux host (or in the builder's shell). Nobody has run
the Windows paths of this change: no Windows host was available.

- **Editions.** One ISO per edition, as Fedora ships Silverblue (GNOME) and
  Kinoite (KDE). The identifier (`gnome`) names the image stage, the files
  below `installer/next/editions/<id>/` (package lists, the session script and
  other overlay files) and the ISO. What a person reads as its name is the
  `display-name` in `installer/next/editions.toml`, the only place it lives;
  the build writes it into the image as `/usr/share/ilex-installer/edition.json`.
  The owner chose **ilex Glabra** for the GNOME edition (identifier `gnome`) and
  **ilex Kingiana** for the future KDE edition (identifier `kde`), both holly species.
- **Build.** `installer/next/Containerfile` is `fedora-bootc:45`, the shared
  live-boot packages (`packages.txt`: dracut's `dmsquash-live`, and what
  image-builder's `generic-iso` needs inside the container) and the edition's,
  installed without weak dependencies, with the same GPU/audio firmware and
  Vulkan removals as the Anaconda installer's image (in the install layer,
  ended by `rpm --verifydb`: removing in a later layer left rpm's database
  corrupt on one build host). `rootfs/` and the
  edition's `rootfs/` are copied over it (ordinary `COPY`), then the initramfs
  is rebuilt generic with `dmsquash-live`. The same `image-builder … generic-iso`
  path as the Anaconda ISO builds it (`localhost/ilex-installer-next:<slug>.<edition>`
  is the image; the helper container, osbuild cache and 2 GiB check are shared).
  `bootloader-update.service` and the sleep targets are masked.
- **Interface.** `crates/ilex-installer` is built by the Containerfile's builder
  stage with kache, exactly like the OS image's crates (the pinned kache
  release must be bumped in both Containerfiles). Its GTK 4 + libadwaita
  interface (relm4) sits behind the `gui` cargo feature, off by default, so
  `cargo clippy --workspace` needs no `gtk4-devel`: lint it with
  `cargo clippy -p ilex-installer --features gui --all-targets` where
  `gtk4-devel` and `libadwaita-devel` are installed. `ilex-installer
  --self-check` reads the edition record and the DMI strings without a display
  and fails when the live image is incomplete; the session unit runs it first.
- **Session.** `ilex-installer-session.service` opens a logind session for the
  unprivileged `ilex-installer` user (sysusers.d, PAM service `ilex-installer`
  on tty1), so GNOME Kiosk (mutter) gets seat0's DRM and input devices without
  root. The PAM service admits only that user and requires `pam_systemd`: a
  session without logind would half-work, so its start must fail. The edition's
  `session` script runs `ilex-installer --self-check`, removes any Wayland
  socket left by an earlier attempt (logind keeps the runtime directory for a
  few seconds, so a compositor that died leaves `wayland-0` behind), starts
  `gnome-kiosk`, waits until the socket has a listener (`ss -xl`, not merely a
  file; up to a minute), then starts `ilex-installer` as its only window.
  Nothing may end the session: whichever process ends, with any status (a window
  closed by the compositor exits 0), the script stops the other and exits
  non-zero, and the window refuses a close request (`--windowed`, for
  development, still closes). The unit is `Restart=always`, three starts in five
  minutes. There is no `OnFailure=` (it fires after every failed attempt, not
  only the last): `ilex-installer-fallback.service`, enabled at boot, watches
  the session unit and, once, when it is `failed` for good (its start limit),
  prints on tty1 what failed and where to look (Ctrl+Alt+F2 has a root shell, an
  agetty autologin) and the `ILEX-INSTALLER-FALLBACK` marker. It polls every two
  seconds and is not subject to a start limit (`StartLimitIntervalSec=0` in its
  `[Unit]` section, the only place systemd reads it), so a slow boot cannot make
  it miss the end state. The session script bounds its `--self-check` with
  `timeout` like every other call. Fedora 45's `kmscon` is masked and the
  virtual terminals get agetty: kmscon is a DRM client of seat0 and takes the
  GPU on tty1 before the compositor can. `autovt@tty1.service` is masked too, so
  that no getty can replace the fallback message when the person presses
  Ctrl+Alt+F2 and back to a tty1 without a session (logind starts `autovt@ttyN`
  on such a switch). That is defence in depth: on this image the message
  survived the round trip with the mask removed, and with the watcher stopped
  too, and no getty started (VT 1 is always allocated, which logind reads as
  busy; only the reserved VT 2 is started regardless). While the session runs,
  logind finds it on tty1 and starts nothing; the tty2 shell is unaffected.
  (Ctrl+Alt+F2 reaches it from the fallback screen, and from the page too.
  GNOME Kiosk turns VT switching off unless started with `--enable-vt-switch`
  (without it the chord never reaches logind), which the session script
  passes; the compositor then asks logind to change VT, and the polkit rule
  below allows `org.freedesktop.login1.chvt` to the installer's own active
  local session. Both are needed.)
  Blanking, locking and sleep are off (dconf defaults, a
  logind drop-in and masked sleep targets): an installation must not stop when
  the screen would lock.
- **SSH.** The live image keeps fedora-bootc's `sshd`, but a drop-in condition,
  `ConditionFileNotEmpty=/root/.ssh/authorized_keys` (also on `sshd@.service`,
  the socket-activated template), lets it start only once something has
  provisioned root's key before it: a real boot has no key on it and an open
  port is no use. Two harnesses provision one. The `iso::next` harness passes
  the SMBIOS credential `ssh.authorized_keys.root` (systemd-tmpfiles writes
  the file from it) for diagnostics and the polkit report; bcvk, which does not
  use that credential, passes its key as a `tmpfiles.extra` credential
  (`f+~ /root/.ssh/authorized_keys`), so bcvk VMs of this image accept SSH too.
  Testing the file rather than either credential admits both and nothing else.
  `iso::next_serial_backpressure` and `_unavailable` boot without the credential
  and check that nothing answers on the forwarded port.
  **The trust boundary is the boot chain or physical access**, not the key
  file: whoever controls the firmware or hypervisor (SMBIOS credentials), the
  kernel command line (a GRUB edit can add a credential) or the machine itself
  can make root's key non-empty, or just boot a shell; sshd adds no way in for
  anyone who could not already do that, and an unprovisioned boot listens on no
  SSH port.
- **What the session can reach.** The live session is an active local session
  on seat0, which polkit's defaults treat as the person at the machine, so the
  image is cut down to what the installer needs. Removed (nothing requires
  them): `udisks2` (an active session may mount any block device without a
  password), `fwupd` and its plugins, `passim`, `avahi`. Masked: the
  `bootc-fetch-apply-updates` and `rpm-ostree` units (including the `countme`
  metrics timer), `systemd-homed`, `sssd` and `geoclue` (activatable for
  gnome-settings-daemon; nothing in the live image uses location). `systemd-resolved` answers no
  LLMNR or multicast DNS (it listened on 5355 on every address). A polkit rule,
  `00-ilex-installer.rules`, denies every action to the `ilex-installer` user
  except `org.freedesktop.login1.chvt`, which it allows for `subject.local &&
  subject.active` (Ctrl+Alt+F2 reaches the tty2 root shell from the page); a
  page that needs another action (a restart button, the network page) adds it
  to that file in its own change and to `POLKIT_ALLOWED` in the trial. `iso::next_boot`
  checks all of this on the running guest and writes `next-surface.txt` beside
  the other artifacts (for the installer's session on seat0, not the user
  manager's own `manager-early` session; its seat, VT, class and state are
  asserted, and so is that `geoclue.service` and `autovt@tty1.service` are
  masked): `pkcheck` against the session's own process for every
  action (only `login1.chvt` is authorized), a control that moves the rule away and back
  (two actions the defaults allow are then authorized, and denied again once it
  is restored), the system bus names, the listening sockets (nothing beyond
  loopback but the harness's sshd) and the running services. **What remains,
  measured on this image (2026-11-26):** the session is denied every polkit
  action; the system bus still offers `systemd1`, `login1`, `hostname1`,
  `locale1`, `timedate1`, `resolve1`, `timesync1`, `oom1`, `sysupdate1`,
  `home1` and `nm_dispatcher`/`nm_priv_helper` (all gated by polkit, and so
  denied for this user, or inert), and the names `home1` and
  `org.projectatomic.rpmostree1`, which are only activatable and whose units
  are masked, NetworkManager and `wpa_supplicant` (the
  Wi-Fi support later pages use; their polkit actions are denied until the
  network page adds its own), `PolicyKit1`, `RealtimeKit1`, `ColorManager`
  (colord, started by the compositor) and `GeoClue2` (still listed as an
  activatable name, but its unit is masked and `iso::next_boot` requires it);
  the listeners are loopback-only
  (resolver stub on 127.0.0.53 and .54, chrony on 323) plus sshd on 22 when a
  key was provisioned. The services running are chronyd, colord, dbus-broker,
  gssproxy, irqbalance, NetworkManager, polkit, rtkit, sshd (with a key),
  hostnamed, journald, localed, logind, oomd, resolved, udevd, userdbd and the
  installer's own two units. `gssproxy`, `irqbalance` and `rtkit` expose
  nothing to the session and stay as fedora-bootc ships them.
- **Boot entries.** "Install ilex" and "Install ilex (basic graphics)"
  (`nomodeset`); the second makes the session set `GSK_RENDERER=cairo` and
  `LIBGL_ALWAYS_SOFTWARE=1`. Both carry `rd.lvm=0 rd.md=0` (also baked into the
  initramfs): the live system activates no LVM volume group and assembles no MD
  array, so a disk with an old installation is offered. The ISO's volume label is
  `ilex-installer-<edition>`.
- **SELinux: the live image boots permissive and unlabeled.** The kernel
  command line is `root=live:CDLABEL=… rd.live.image quiet rhgb enforcing=0
  quiet`: image-builder's `generic-iso` hard-codes `enforcing=0` in the
  `org.osbuild.grub2.iso` stage's kernel options, and our two entries use its
  `${root}`. It cannot simply be dropped, because the squashfs carries no
  labels: the tree is the container image copied by `org.osbuild.container-deploy`
  into `os-tree` and squashed with no `org.osbuild.selinux` stage (the only
  one in the manifest labels the build root), so none of the 976 files sampled
  in `/usr/bin`, `/etc` and `/usr/lib/systemd/system` has a `security.selinux`
  xattr and the squashfs's xattr table is 69 bytes. Without labels nothing
  transitions: PID 1 and every process run as `kernel_t`, and files are
  `unlabeled_t`. The image cannot supply the labels either: `setfiles` run in a
  `podman build` step (it needs `--cap-add=sys_admin` on a host without SELinux)
  sets them, but the committed layer loses them (measured on this image:
  `/usr/bin/true` has no xattrs in the next container), since containers/storage
  does not keep `security.selinux` in layers. A fix needs a labeling step in the
  ISO pipeline between `container-deploy` and `squashfs` (an
  `org.osbuild.selinux` stage with the policy's `file_contexts`, which means
  upstream image-builder or our own osbuild manifest for the live tree), then
  `enforcing=1` appended after `${root}` (the last `enforcing=` wins) and a
  policy module for the installer's own units and the root helper. Until then
  SELinux confines nothing here: ordinary services already run as `kernel_t`.
  Measured on the 2026-11-26 image: **about 155 `avc:  denied` events per boot**
  (154 and 158 on two runs, counted in the journal's audit transport; the
  kernel prints 19 of them a second time and dbus-broker adds a few as
  `USER_AVC`), all permissive, from `systemd-sysctl`, `systemd`,
  `polkitd`, `systemd-oomd`, `systemd-journald`, `udev-worker`, `systemd-userd` (as the kernel truncates it),
  `systemd-resolved`, `irqbalance`, `rtkit-daemon`, `colord`, `chronyd` and
  `systemd-logind`, with source domain `kernel_t` (sysctl files, device nodes,
  `mounton` of `/var`, executing `unlabeled_t`). `iso::next_boot` writes the
  mode, command line, PID 1's and `/usr/bin/bash`'s labels, the event count and
  the fifteen most frequent denials to `next-surface.txt` (recorded, not
  asserted); a root helper added later will show up there too.
- **Serial markers.** The session script and the interface print marker lines
  to the journal and to `/dev/ttyS0` (the user is in `dialout`):
  `ILEX-INSTALLER-SESSION compositor=… graphics=default|basic secure-boot=0|1`,
  `ILEX-INSTALLER page=placeholder edition=… renderer=… uptime=…` once the
  window is mapped, `ILEX-INSTALLER-STATS mem_total_kib=…` fifteen seconds
  later, `ILEX-INSTALLER-SESSION ended child=compositor|installer status=…`
  when the session ends and `ILEX-INSTALLER-FALLBACK session=failed result=…`
  at the give-up. The session script prints them through `ilex-installer
  --marker LINE`, so one implementation owns the serial write. It never delays
  the interface: the port is touched only by a disposable worker thread, which
  the caller waits for 200 ms and then abandons (even a non-blocking open and
  write can end in a `close()` that waits for the UART's `closing_wait`), and
  markers are skipped, not queued, while an abandoned worker lives, as
  `installer/chooser/diagnostics.py` does for the Anaconda chooser. Two fixed
  credentials (SMBIOS, `io.systemd.credential:`) are test-only:
  `ilex.installer.fault=session` makes the session script fail, and
  `ilex.installer.serial-test=unavailable|backpressure` points the mirror at a
  missing device or at a pty with stopped output (`tcflow(TCOOFF)`), reporting
  each faulted write (`ILEX-INSTALLER-SERIAL mode=… status=… seconds=…`) on the
  second UART, `/dev/ttyS1`. They can only stop the installer from starting or
  silence its mirror.
- **Tests.** `iso::next_boot`, `iso::next_basic_graphics`, `iso::next_fallback`,
  `iso::next_restart`, `iso::next_serial_backpressure`,
  `iso::next_serial_unavailable`, `iso::next_serial_stall` and
  `iso::next_ssh_tmpfiles` are registered only when a filter names
  `iso::next` (so `iso::`, the Anaconda release gate, never boots this ISO).
  `cargo ilexctl test iso::next` builds the ISO (or uses
  `ILEX_ISO_NEXT=<path>`) and needs no OS image. Markers are matched as
  complete lines by their first word and a key, never as substrings, and a
  missing field is an error with the console in it, not a panic.
  - The two boot trials start the ISO under QEMU/KVM with Secure Boot required
    (the secure fixture path with `security_launch=qualification_fixture`: it
    requires provisioning, that is KVM with SMM and a Secure Boot firmware
    pair, and fails without it, but does not require the host to be
    *qualified* by the `security::` tier and never promotes it; a disposable
    1 GiB disk, no TPM, `ILEX_ISO_NEXT_MEMORY` defaults to 4G). They assert the
    markers (including `secure-boot=1` read by the guest and cairo for the basic
    entry), wait for a settled QMP screenshot that is libadwaita's light window
    with content, and record timing, memory and ISO size in
    `<state dir>/artifacts/iso-next/next-<entry>.json` beside the screenshots.
    The basic graphics entry is chosen in GRUB with real key input over a USB
    keyboard (`LinuxProvider::with_usb_keyboard`). The default entry also
    writes `next-surface.txt` (below) and fails on any polkit action beyond
    the allowlist in `iso_next.rs` (`login1.chvt` only). After the page it
    replaces the rule with the deny-everything one the image had before and
    presses Ctrl+Alt+F2 (the kernel's active VT must stay `tty1`: polkit
    refuses logind's `chvt`), restores the rule and presses it again (the
    active VT, read from the guest, must be `tty2`), types a command into tty2's root shell that only a live root shell
    can answer (`echo ILEX-TTY2-ANSWER-$((6*7))-$(id -un)`), reads the answer
    back from `/dev/vcs2` over SSH and checks a QMP screenshot of light text on
    a dark screen, then presses Ctrl+Alt+F1 and requires the page again, the
    session unit `active` and no getty on tty1. It then has root run `chvt 2`
    and `chvt 1` over SSH with the same requirements.
  - `iso::next_fallback` makes the session fail (fixed credential), requires
    exactly three failures, then exactly one `ILEX-INSTALLER-FALLBACK` (it
    waits eight seconds for a second), and reads the message back from the
    VT's own text, `/dev/vcs1`, over the harness's SSH key; a QMP screenshot
    must be light text on a dark screen, not a drawn page and not black. It then
    sends Ctrl+Alt+F2 and Ctrl+Alt+F1 (the kernel's active VT, read from the
    guest, must be `tty2` then `tty1`) and requires the same message, no login
    prompt, no getty on tty1 and no second `ILEX-INSTALLER-FALLBACK`.
  - `iso::next_restart` sends Alt+F4 (the page stays, no restart), kills the
    compositor with SIGKILL (its socket stays behind; the next attempt must
    report no stale socket and bring the page up, `NRestarts=1`), then kills
    the client (`NRestarts=2`), and requires that the fallback never fires:
    three starts in five minutes are the unit's limit.
  - `iso::next_ssh_tmpfiles` boots without the harness's credential and gives
    its key the way bcvk does (a `tmpfiles.extra` credential), and requires
    sshd to start and accept it.
  - `iso::next_serial_backpressure`, `_unavailable` and `_stall` boot with
    `ilex.installer.serial-test`; the first two without the SSH key. The page
    comes up (readiness is read from `/dev/ttyS1`) and every faulted marker
    write is reported there, in under a second, as a complete
    `ILEX-INSTALLER-SERIAL` line with `message=ILEX-INSTALLER-SESSION` and
    `message=ILEX-INSTALLER` among them, as the injected failure (a pty with
    stopped output, no device) or, for `stall` (the worker hangs for three
    seconds, as a UART that never drains would in `close()`), as having met the
    200 ms deadline (`budget`) or been skipped behind the stuck worker
    (`busy`); the first three never reach the faulted console, and without a
    key the forwarded SSH port sends no banner. `stall` has the harness's key
    and, over it, runs `ilex-installer --marker` with two lines in one process
    under the same credential (`systemd-run -p ImportCredential=…`): the audit
    lines must be `budget` then `busy`: the skip-while-stuck path, which the
    page's own markers (fifteen seconds apart) never reach. What no trial
    reproduces is a real UART's `closing_wait`: `stall` stands in for it.
- **CI.** `publish.yml`'s `iso_next` input starts only the `installer-next` job
  and skips the image pipeline and the Anaconda ISO jobs. The run has a
  concurrency group of its own (`iso-next-<hash of the ref>`, also on main: it
  neither waits for nor replaces a real publish), is passed `main=false`, and
  receives no signing secrets: the route job's `sign` output is true only for
  runs that can sign (push and nightly on main, or a manual run of main that is
  neither `iso_next` nor `v1_disk`). The workflow has no `pull_request` trigger,
  and the job additionally requires `workflow_dispatch`, has `contents: read`
  and reads no secret, so PRs and forks reach neither it nor the key. It builds
  the ISO in the same privileged Fedora 45 container as the `iso` job, keeps it
  as the `installer-next-iso` artifact, boots it with Secure Boot (the eight
  `iso::next` trials, with the same provisioning-only requirement as locally:
  a runner without KVM SMM fails them) and keeps `installer-next-evidence`.
  Nothing releases or publishes it. Runs without `iso_next` are unchanged.
- **Fingerprints.** The root image's fingerprint (the early `ilex.root-inputs`
  key and the content fingerprint, `ilex_tooling_linux::fingerprint`) hashes
  the crates in the dependency closure of the packages the root image builds,
  not all of `crates/`. Those packages are **declared in one place**,
  `build/image-crates.toml` (`[[build]]` entries: `packages`, and
  `no-default-features`; any other key or a package name that is not a plain
  name is an error). The Containerfile's builder stage builds them by running
  one script, `python3 -I -S build/build-image-crates.py
  build/image-crates.toml`, which reads that file, runs exactly those `cargo
  build --release --locked` commands with `RUSTC_WRAPPER=kache` (in a scrubbed
  environment, see below) and stages the binaries and completions under `/out`. `image_crates` reads the same
  declaration (no Containerfile parsing) and walks `cargo metadata --offline
  --locked --filter-platform x86_64-unknown-linux-gnu` from the named packages
  over normal and build dependencies (`cargo fetch --locked` runs first: the
  only step that may use the network).
  Every package without a registry or git source, workspace member or `path =`
  dependency, is hashed by its directory; one outside the checkout, or at its
  root, fails the fingerprint. Registry and git packages are covered by
  `Cargo.lock`. Today that is `ilexctl` (built
  with `--no-default-features`), `ilex-agent`, `ilex-enroll` and `drupe`; the
  installer, the e2e harness, `xtask`, the devkit, the vmctl and the tooling
  crates are not inputs, so editing them no longer rebuilds, re-signs and
  republishes every root. Nothing is listed by hand but the declaration: a new
  dependency of a built package enters the set by itself. The declaration, the
  script and the Containerfile are inputs themselves (`build/` and
  `Containerfile` are hashed whole), so editing the declaration changes every
  root's key. A crate added in a dirty working tree (not at HEAD yet) is covered
  by the dirty entries. **`Cargo.toml` and `Cargo.lock` stay hashed whole**:
  any dependency change in any workspace crate (adding `ilex-installer` added
  708 lines, a GTK bump would again) edits `Cargo.lock` and therefore changes
  every root's fingerprint once, although the shipped binaries are unchanged.
  The Anaconda ISO fingerprint (`INSTALLER_INPUTS` in `xtask/src/iso.rs`) also
  hashes `Cargo.lock`, `Cargo.toml` and `crates/xtask/src/iso.rs`, so those
  make that ISO due the same way; `installer/next` and the new crate do not.
  The first main run after this change rebuilds every root and the ISO once
  (the input set itself changed).
  **What keeps the build and the closure the same.** Nothing parses shell: the
  root image's Rust build is kept so narrow that a text check sees all of it,
  and what a text check cannot see is checked by running the script. Every
  keyless derivation of the closure (`ilexctl changed`, `build`,
  `info --image-crates`, `build-publisher.sh`, not only `doctor --self-check`)
  runs the three guards, the declaration's parser and the script itself (below)
  before `cargo fetch`; each refusal names the line (drift detection, not
  adversary-proofing). The signing process reads the recorded closure and
  never derives it, so it runs neither the script nor cargo.
  - `check_containerfile` splits the Containerfile into stages (`\`
    continuations joined, full-line comments dropped). A stage **builds the
    Rust packages** when any logical line names `cargo`, `rustc`, `rustup`,
    `rustdoc`, a word that starts with `rust` (`RUSTFLAGS`,
    `rust-toolchain.toml`; not `ca-trust`) or `image-crates`. Such a stage may
    contain only: its `FROM quay.io/fedora/fedora:45` (the builder image the
    root key hashes), `ARG KACHE_*`, `WORKDIR /src`, three pinned `COPY`s
    (`Cargo.toml Cargo.lock`, `crates/`, the script with its declaration),
    `RUN [--mount=type=cache…] dnf -y install …` of plain words, the pinned
    kache download, and the one `RUN [--mount=type=cache…] python3 -I -S
    build/build-image-crates.py build/image-crates.toml`, exactly once. The
    mounts are cache mounts at the dnf, cargo registry and kache store targets
    only. Anything else is refused without being understood: a second `cargo
    build`, `RUN sh build/x.sh`, `ENV`/`ARG RUSTFLAGS` (or any `CARGO_*`), a
    `RUN … >> build/image-crates.toml`, a heredoc `COPY`, `COPY .cargo/`, `COPY
    rust-toolchain.toml`, a `config.toml` copied into cargo's home, a bind
    mount, `WORKDIR`, `USER`, `SHELL`, `ONBUILD`, another builder image, a
    second stage that installs cargo, an exec-form or heredoc `RUN`.
  - `check_build_script` pins the script. From the line `PACKAGE =
    re.compile(...)` to the end it equals
    `crates/ilex-tooling-linux/src/build-image-crates.pinned.py` (code lines;
    comments and blank lines are free; edit both files together). That part
    reads the declaration (`declared()`: which keys it accepts, that
    `no-default-features` is read under that name), makes every cargo argv in
    one function (`cargo_commands`: `--release --locked`, `-p` for each
    declared package, `--no-default-features` when declared, nothing else),
    runs its output at one place, starts cargo with `cargo_environment()`,
    refuses to build while `cargo_configs()` lists any file, and holds every
    other program the script runs (kache, `install`, the completions). Above it
    stay the two lists a routine edit changes (`BINARIES`, `COMPLETIONS`), with
    six imports and nothing that runs a program (`subprocess`, `os.system`,
    `os.exec*`, `os.spawn*`, `os.popen`), loads or evaluates code (`runpy`,
    `importlib`, `exec`, `eval`, `__import__`, `compile`, `ctypes`), changes the
    environment or the directory (`environ`, `putenv`, `chdir`) or names the
    toolchain outside comments.
  - `check_build_files` scans every file under `build/` (the declaration and
    the script have their own checks) and refuses the toolchain's name outside
    comments, except the one message in `build/image_tree.py` that tells a
    developer to run `cargo ilexctl build`.
  - What no line can show (a line above the pinned part that rebinds what the
    pinned part calls, such as `tomllib.load = ...`) is checked by running the
    script, in `derived_image_builds` on every derivation. `--commands
    DECLARATION` prints the argv lists `main` will run (from the same function);
    the derivation compares them with what the declaration implies. `--environment`
    prints what cargo and kache start with: the script drops every `CARGO*`,
    `RUST*` and `KACHE*` variable of the caller (so `RUSTFLAGS`,
    `CARGO_BUILD_TARGET`, `RUSTUP_TOOLCHAIN` cannot turn on a cfg, a target or
    a toolchain the closure does not cover) and sets only `RUSTC_WRAPPER=kache`
    and the read-only host store. `--configs` prints every `.cargo/config` and
    `.cargo/config.toml` cargo would read (the working directory, each parent,
    cargo's home), and the build stops when it lists any: `[build]`
    target/rustflags, `[env]` and `[patch]` there would change what is built
    and nothing the fingerprint hashes. The tracked `.cargo/config.toml` (the
    `ilexctl` alias) is not copied into the builder stage.
  - The closure derivation (`cargo fetch` and `cargo metadata`) runs with the
    `*RUSTFLAGS`, `*RUSTDOCFLAGS` and `CARGO_BUILD_TARGET` variables removed,
    as the build does, so a developer's shell resolves the closure CI does.
  **Out of scope** (this finds drift, not an adversary): a spelling that never
  writes `cargo`, `rustc`, `rustup`, `rustdoc` or a word starting with `rust`
  (`c""argo`, a variable that holds the name, a script generated at build time),
  a base image that already has cargo configuration or another toolchain, and
  `build.rs` scripts of the crates. Review any script a `RUN` adds.
  `ilexctl doctor --self-check` runs the fixtures, each refused for the exact
  line it changes: the Containerfile's (4 accepted, 62 refused: every shell
  bypass the earlier parser's reviews found, a `RUN sh build/x.sh`, `RUSTFLAGS`
  and its relatives in `ENV`/`ARG`, edits of the declaration, copies of
  `.cargo`/`rust-toolchain`/`config.toml`, mounts, another base image, the
  script run twice or never or on another declaration), the script's (2
  accepted, 44 refused: an extra `-p`, `--workspace`, `--features`, `--target`,
  `--config`, a second `subprocess.run`, another helper, `os.system`/`exec*`/
  `spawn*`, `runpy`/`importlib`/`exec`/`eval`, an unscrubbed or wrongly
  scrubbed environment, the declaration's flag read under `no_default_features`,
  a build appended while reading it, another key allowed, a looser package
  pattern, a reader rebound above the pinned part, a configuration check that
  misses a parent or cargo's home), the derivation's (2 accepted, 4 refused:
  the checks `changed`, `build`, `info --image-crates` and `build-publisher.sh`
  run, on a scratch checkout: a binary added to `BINARIES` passes; the
  underscore key and an appended build are refused by the pinned text, a
  rebound `tomllib.load` and `os.path.lexists` only by running the script), the
  `build/` scan's (3 accepted, 6 refused) and the declaration's (1 accepted, 12
  refused). It checks that the checkout's own Containerfile, script and
  `build/` pass the same derivation: the real script's `--commands`,
  `--environment` (with `RUSTFLAGS`, `CARGO_BUILD_TARGET` and 14 other
  variables planted) and `--configs` (a configuration planted in the working
  directory, a parent and cargo's home), and it checks the derivation's scrub. It also runs the closure
  fixtures (a path dependency in the checkout, one outside, one at the root,
  registry and git packages, a dev edge) and the recorded-closure fixtures; a
  regression is a failed check.
- **A VM that never starts.** About one boot in a hundred on the Boat hosts
  (2 of 208 trial boots over 13 runs of the whole `iso::next` set, in a
  different trial each time) never ran the guest: the serial log stayed empty
  for the whole deadline and a rerun passed. The cause was QEMU, not the ISO:
  its VNC server asks for a default audio backend, QEMU 11 probes PipeWire
  first, and libpipewire's `pw_thread_loop_stop` intermittently never returns
  when no daemon is there (main thread in `pthread_join` inside `qemu_init`,
  QMP socket present but silent, vCPU threads at 0 s CPU). The `-audiodev none`
  that removes the probe belongs to the VM harness (`ilex_devkit::qemu`) and is
  described with it, not here. The trials only report such a hang quickly and
  say what QEMU was doing:
  a guest whose serial console stays empty for 60 s after QEMU created it fails
  with "the guest wrote nothing on the serial console … it never started"
  (OVMF prints within about five seconds), instead of waiting out the 420 s
  marker deadline; `iso::next_ssh_tmpfiles` waits for that first byte before
  its 300 s SSH wait; and any failed `iso::next` trial writes
  `next-<trial>-qemu-state.txt` beside its screenshot: QMP status, each CPU's
  instruction pointer, per-thread CPU time over two seconds and kernel wait
  points (plus a gdb backtrace when gdb is installed), the size and age of the
  serial files (and the tail of `firmware.log`, which exists when the VM was
  started with `ILEX_VM_FIRMWARE_LOG=1`), `qemu.log`, and the host's load,
  pressure and steal time. Each trial prints its duration. The
  `seconds_power_on_to_page` in `next-<entry>.json` starts when QEMU has
  started, not before the wait for a VM slot (it used to include that wait:
  40 to 220 s for a boot that takes 30).
- **Measured** 2026-11-25 on a Boat sandbox (16 vCPU, 32 GiB, nested KVM, the
  GNOME edition). After the review fixes (2026-11-26) the ISO is 1,494,851,584
  bytes (1.39 GiB): 438 MiB under the Anaconda network ISO's 1.82 GiB and 0.61
  GiB under the 2 GiB limit, and the image has 698 packages, 1,826 MiB
  installed (the first build: 1,494,847,488 bytes, 706 packages, 1,850 MiB;
  largest: llvm-libs 146 MiB,
  python3-botocore 121, kernel-core 102, kernel-modules 80, mesa-dri-drivers 53,
  podman 53; `largest-packages.txt` lists ten). `iso --next` took 14 minutes
  with the base images and RPMs already local, mostly image-builder; the
  interface's builder stage took 1 minute 42 seconds with a cold kache (0 of 136
  crates restored, `gtk4` the slowest at 33 seconds). Booted under QEMU/KVM with
  Secure Boot, 2 vCPUs and 4 GiB, the window was up 5 to 25 seconds after the
  kernel started (6 to 8 on a quiet host; GRUB's 10 second countdown comes
  before that, and the basic entry's key press skips it). GTK draws with
  `GskGLRenderer` (llvmpipe) on the default entry and `GskCairoRenderer` on the
  basic one. Fifteen seconds after the window the guest used 595 to 626 MiB
  (default) or 503 to 520 MiB (basic), with 3.2 GiB available; at 2 GiB it used
  513 and 410 MiB and had 1.3 GiB available. GNOME Kiosk was not compared with
  cage.

## The next installer's root helper

The next installer (a Rust interface per shell on its own live ISO, replacing
Anaconda) is built beside the released Anaconda ISO. Its live image is
`installer/next/`; this section covers the shared model and the root helper
only. Nothing here installs: the helper probes and plans, and writes to no disk.

`crates/ilex-installer-model` holds what the host that writes the composition
table and the installer that reads it must agree on: the table types
(`Table`, `Machine`, `Shell`, `Composition`, `Profile`) and their validator,
editions, DMI detection and the choice order, and the probe, plan, answers and
state schemas. The types moved out of `ilex-tooling-core`; `ilex_devkit::installer`
still re-exports them and `ilex_devkit::installer_model` is the crate. The host
still builds the table with `installer::table`, and its serialization is
byte-identical (field order and names are the file format), so the Anaconda ISO's
`compositions.json`, `iso.yaml` and release notes do not change. The installer
fingerprint does change once, because its inputs include `ilex-tooling-core`'s
`installer.rs` and `config.rs`, the model crate and the Cargo files.

- **Editions.** One ISO installs one shell. `edition.json` (written into the live
  image from `installer/next/editions.toml`, where the display name lives) names
  it; the identifier is the shell. `Table::for_edition` keeps the compositions
  whose shell list is exactly the edition default's, in order: the table's default
  when it starts with the edition's shell, else the edition's first generic
  composition. A composition that merely starts with the edition's shell
  (`generic.gnome.ilex` for the GNOME edition) belongs to another edition and is
  not offered. There is no desktop choice; a machine gets the default's shell
  list or nothing. A table with neither an edition record nor `--edition` is the
  Anaconda path's: its default composition's shell (GNOME). `--table` brings the
  `edition.json` beside it; a record that exists but cannot be read stops the
  helper, only an absent one means the Anaconda path.
- **Detection** is `installer/chooser/ilex-installer-choice` ported with the same
  meaning: exact, case-sensitive comparison of the four SMBIOS strings (stripped
  as Python's `strip` does); a vendor or model alone never matches; two
  matching machines fall back to the default and say so; a recognised machine
  without a composition falls back with its own reason. `Table::detect` is the
  chooser's own rule on a whole table; the helper uses `detect_for_edition`, which
  runs it on the whole catalogue and maps the result into the edition, so two
  machines matching the same computer are a conflict even when one has no
  composition in this edition, and a machine that has none here is reported
  as such. The choice order is the last `ilex.composition=<tag>` kernel argument,
  then `/run/ilex-installer/choice` (a tag or the schema-1 record), then
  detection. An unknown tag is an error that lists the offers. `Selected::serial_line`
  keeps `ILEX-COMPOSITION <tag> (<reason>) installs <ref>`; the interface prints it.
- **`ilex-installerd`** is a root service on the system bus, `org.ilex.Installer1`
  at `/org/ilex/Installer1` (zbus), started at boot by `ilex-installerd.service`
  (`Type=dbus`, `Restart=on-failure` within a start limit, no core files,
  `PrivateMounts=yes`, a 0700 runtime directory) and by bus activation if a
  caller gets there first. Its bus policy (`org.ilex.Installer1.conf`) lets root
  own the name and call it and `ilex-installer` call it; anyone else, including
  introspection, gets `Access denied`. The shell on tty2 is a root autologin, so
  it is allowed like root. With the helper stopped, the installer's call starts
  it and a refused user is denied (the tier checks both); dbus-broker starts an
  activatable service for any call to its name before it applies the policy, so
  a refused call still starts the helper, which is harmless: it is started at
  boot anyway and answers only the allowed. Every method takes and returns JSON text and none takes
  a path, an image reference, a kernel argument or a command. A crashed helper
  restarts and forgets its plans (a later Apply must record its stage before it
  writes anything and must not resume by itself). `Probe()` reports the helper
  and edition, firmware (UEFI, Secure Boot, setup mode), `ilex-enroll`'s probe
  verbatim, the DMI identity and detection, the boot-argument selection (or its
  error), this edition's catalogue, the disks and memory. A disk has a stable id
  (`/dev/disk/by-id`: `nvme-eui.*`, `wwn-*`, any other, then the bus path, then
  the kernel name), model, serial, size, bus, `removable`, the partition table,
  partitions, free extents (whole MiB between partitions, inside GPT's usable
  range) and other systems found: EFI loader directories, Windows, BitLocker,
  Linux, LUKS and Apple filesystems. Zram, loop, optical, read-only and
  network-attached devices (nbd, iSCSI, Fibre Channel, SRP, NVMe over fabrics)
  are not disks. An NVMe disk is local only when sysfs shows every controller
  behind it on PCIe: with native multipath (Fedora's default) `device` is the
  NVMe subsystem, so the helper reads the `transport` of its `nvme*` controllers
  and of `multipath/*/device`, and a fabric, an unreadable transport or a
  namespace with no controller found counts as the network. Two flags keep the
  running system's own disks out of reach:
  - `live_medium`: the disk the live system came from. Every mount at or under
    `/run/initramfs` and every iso9660 or udf mount is followed through the kernel,
    not through the text of its source: its device number (`/sys/dev/block/M:m`),
    else its canonicalised `/dev` source, then up from a partition to its disk,
    down through `slaves/` (device-mapper, MD: Ventoy's `/dev/mapper/ventoy`) and
    through a loop device's `backing_file` to the device the file lives on
    (dracut's iso-scan, with the host disk at `/run/initramfs/isoscan`). That
    file is looked at itself, never placed by the mount table's longest prefix
    (`/` contains every path, and a file whose mount was detached is named below
    it): its `st_dev` must be readable from the helper, a number with a major is
    the block device sysfs lists for it, and an anonymous number counts as no disk
    (tmpfs, overlay, virtiofs) only when a mount of that very number says so (a
    btrfs subvolume's own number falls back to the btrfs mount around it). Anything
    else (a file that is gone, a deleted one, a path that cannot be read, a
    device sysfs does not list, a live mount that names or numbers a device, or an
    iso9660 or udf mount, that cannot be found) marks every disk in use rather
    than guessing, as does a swap file that cannot be placed, and so does a mount
    table or `/proc/swaps` that cannot be read (the probe then fails).
  - `in_use`: why the running system is using the disk otherwise, one sentence
    each: a mounted partition, an entry of `/proc/swaps` (device or file), or a
    device stacked on it or a partition (non-empty `holders/`: LVM, MD, dm-crypt).
    A disk that carries an old Linux installation is therefore offered only while
    the live system has not activated its volume group or array, so the live
    image activates none: `rd.lvm=0 rd.md=0` is on both boot entries and baked
    into the initramfs (dracut's `--kernel-cmdline`, for a bootloader that brings
    its own command line), and `/etc/lvm/lvmlocal.conf` (`event_activation = 0`,
    an empty `auto_activation_volume_list`) and `/etc/mdadm.conf` (`AUTO -all`) quiet
    udev once the system is up. Activating one by hand (`vgchange -ay`) makes its
    disks `in-use` again until it is deactivated from the tty2 shell; the helper
    never deactivates anything.
  An ESP is mounted read-only (`noexec,nodev,nosuid`) by a child of the helper
  (`--scan-esp`) inside `unshare --mount`, at a per-call mount point in a
  private 0700 directory the helper made under its runtime directory (never a
  symlink, owner-checked, removed empty): the mount exists in that child's mount
  namespace only, so neither the host, the daemon, another probe nor the
  `--self-check` shares it, and an unmount failure is reported (in `notes`, and as
  a plan warning) rather than ignored. An ESP that cannot be read says so and the
  list of other systems may be incomplete.
  `Plan(request)` takes `composition`, `disk`, `mode`, and
  optionally `encrypt` (on), `tpm` (`auto` with encryption, else `off`),
  `keyboard` (`us`), `language` (`en_US.UTF-8`), `time_zone` (`Etc/UTC`),
  `host_name` (`ilex`) and `administrator`; unknown fields are refused. It
  probes afresh and refuses, with a JSON `{code, field, message}` as the error
  message: a BIOS boot (`unsupported-firmware`: the installer is UEFI only), a
  composition outside the edition (`vm` is named as never installed
  on hardware), `free-space` mode (a later PR), a disk id not in the probe, the
  live medium, a disk in use (`in-use`), a disk under `MIN_DISK_BYTES` (512 MiB ESP
  + 1 GiB `/boot` + 30 GiB root + alignment, to be set from a measured install),
  TPM without encryption, an administrator name that is reserved, and a malformed
  or unavailable language, keymap, zone or host name. An administrator name is
  at most 31 bytes (as systemd and `build/compose.py` limit them) and may not be
  any name the live image defines in `/etc/passwd`, `/etc/group` or `sysusers.d`,
  a `systemd-` name, one of the installed image's own (`ilex-homed`, `gamemode`,
  `sshd`, `tss`, `chrony`, ...) or a daily-profile name. A time zone is a zone
  file of the zoneinfo tree that starts with the TZif magic (so not `zone.tab` or
  `leapseconds`), outside `posix/` and `right/` and not `posixrules`. A language
  is a UTF-8 glibc locale name (`ll[l]_TT.UTF-8`, `C.UTF-8`, an optional `@modifier`);
  that is a syntax check only, because the live image carries no list of the
  locales the installed system can generate (glibc's `SUPPORTED` is not in it), so
  a well-formed name for a locale the system lacks falls back to the C locale there.
  A refusal names its field and may repeat that field's own (non-secret) value
  (`unknown time zone X`); a request that is not valid JSON of the schema is
  refused with a message that never quotes a value of the document, only a
  position and the name of a missing or unknown field, because a caller may have
  put anything, a secret included, in a mistyped field. The tier sends a secret in
  an unknown field, a mistyped field and an unknown mode and requires that it
  never comes back. `Plan` resolves the tag with `skopeo inspect --raw` to the
  manifest digest (it hashes exactly the returned bytes), reads the download
  size and `org.opencontainers.image.version`/`revision` from the config, and runs
  `systemd-repart --dry-run=yes --empty=force --seed=<plan seed>` on the shipped
  layout directory itself: `/usr/lib/ilex-installer/repart/whole-disk` or
  `whole-disk-encrypted`, each a complete layout (one root partition) that
  repart is pointed at as it is, never assembled or copied. The registry is
  untrusted until the signature is checked at pull, so its answers are checked
  before use: a digest must be `sha256:` and 64 lower-case hex digits before it
  reaches skopeo, a child manifest fetched by digest must hash to that digest, an
  image manifest must have a config and layers with sizes (added with checked
  arithmetic), only `manifest unknown` and `name unknown` (any case) mean the image
  is absent, text shown from it has no control characters, and every tool's output
  is bounded (a tool that prints more is stopped and the call fails). The plan holds
  a 128-bit id, the seed that makes Apply create the same UUIDs, the normalised
  request, the image (`target` tag and pinned `source@digest`), the partitions, what
  is erased and the other systems it destroys, the planned unlock method with the
  probe's reason code, and warnings. The registry answer is metadata: signatures
  are enforced when the image is pulled. `State()` reports the stage (`idle` until
  Apply exists), the latest plan id and a record at `/run/ilex-installer/state.json`.
  The helper keeps the last eight plans in memory. `ilex-installerd --self-check`
  prints `ILEX-INSTALLERD-CHECK OK: …` after exercising the model's rules and
  the parsers on fixtures (manifests a registry could send, the live medium and
  disks in use in a sysfs tree of the kernel's shape, scratch directories, output
  limits, zone and name files), the shipped definitions against the model's sizes,
  `systemd-repart` on a sparse file (both layouts, repeatable with a seed) and
  a real probe of the system. `--table`, `--edition`, `--definitions` and
  `--run-dir` are for development; `--dmi-dir`, `--efi-dir` and `--cmdline` are
  hidden and exist for the `installer::` tier.
- **Answers** (`answers.toml` on a volume labelled `ILEX-ANSWERS`) have their
  schema and syntax checks in the model: `[install]` (`composition`, `disk`,
  `mode`, `confirm-erase` repeating the disk id), `[encryption]` (`enabled`,
  `tpm`, `passphrase`), `[system]` and `[administrator]` (`name`, `password`).
  Secrets never print, and a TOML error names the line and column but never the
  text (toml's own message quotes the line). The administrator's password may not
  contain a line break or NUL, the passphrase no NUL.
  Reading the volume and applying the answers are later work.
- **The live image** gains the helper and `ilex-enroll` (built in the same
  builder stage), `compositions.json` generated like the Anaconda ISO's, the
  bus policy, the unit and the repart definitions under
  `installer/next/rootfs`, `ilex-enroll tool-manifest` as
  `/usr/lib/ilex/security/tools.json` (image-builder drops the RPM database)
  and the packages the helper runs (`dosfstools`, `e2fsprogs`, `btrfs-progs`,
  `skopeo`, `kbd`, `tzdata`, `cryptsetup`, `tpm2-tss`; fedora-bootc has them all,
  so they add no size, and listing them keeps a base change from dropping one). `ilexctl iso --next`
  shares its image stage with the tier (`iso_next::build_image`).

`ilexctl test installer::` boots that image (`ILEX_INSTALLER_IMAGE` names one;
otherwise it is built, without the OS image or an ISO) as an ephemeral VM with
an SSH server added in a layer of the harness's own (tagged
`localhost/ilex-installer-next-test:<slug>.<image id>` and labelled with the
checkout's owner, like the images the tooling builds), the kiosk session masked
and eight disks: a blank 40 GiB disk, a 48 GiB disk carrying a fake dual-boot
layout (an ESP with Windows and Ubuntu loader directories, a reserved partition,
a LUKS volume and a labelled ext4 partition), a 36 GiB disk carrying an old Linux
installation (`/boot`, an LVM volume group with two volumes, an MD RAID 1 array),
an 8 GiB disk, and four the running
system uses: an ISO mounted by its `/dev/disk/by-label` name as given, an ISO file
on a mounted partition attached to a loop device and mounted where dracut mounts
the medium (iso-scan), a device-mapper device over a partition holding an ISO
(Ventoy), and a disk with a mounted partition, active swap and a device stacked
on a partition. It calls the helper with `busctl` as `ilex-installer`. The tests
share the VM and one of them restarts the helper with fixtures, so the group runs
them one at a time; `--test-threads 1` is not needed. `installer::probe` runs the
self-check beside a loop of probes, compares the probe with what the guest
reports, checks the disks (free extents and other systems included, the three
live media and the disk in use), shows that the event of the old installation's
disk appearing activates neither its volume group nor its array, then that the
same event does activate both once the image's LVM and MD configuration is moved
aside (so the first check proves something) and that the disk, now held, is
refused (`in-use`) and offered again once the configuration is back, shows that a
loop device whose file's mount was lazily unmounted (the kernel then names the
file `/lost.iso`, which `/` contains) marks every disk in use, points the unit at SMBIOS and command-line
fixtures to exercise detection through the bus (MS-7C35 by system and by board
alone, case, a vendor alone, the boot override and an unknown one), requires
`Access denied` for `nobody` and `daemon` on every method and on introspection,
and stops the helper to check that the installer's call activates it and a refused
user is denied. `installer::plan` first points the helper at an absent
firmware directory (a BIOS boot is refused), then at a present one: whole-disk
plans for the blank disk plain and encrypted, the pinned digest against an
independent resolution in the guest, the layout disk and its five systems in the
warning, the old installation's disk (plannable, its volumes among what would
be erased, nothing activated), a refusal with its code and field for every way a request can be
refused (the live media, the disk in use, the reserved and system names, the
language, zone and keymap, the malformed requests, and the rest), a secret never
quoted back, no disk changed and no scratch file left behind. The registry,
`systemd-repart` and internal errors cannot be provoked by a request; the
registry's classification is exercised in the self-check on fixtures of what a
registry answers. Planning needs the registry (`ghcr.io/reidond/ilex:generic.gnome`).
A VM cannot carry another board's firmware identity, and libblkid only takes a
real BitLocker header for BitLocker: the `--self-check` covers conflicting
machines, the BitLocker and NTFS classification, a live medium that cannot be
followed and a live ISO's own mount layout; the live ISO's own boot is
`iso::next…`.

## Conventions

- `packages.toml` belongs to the base and stays within its 322 MiB growth
  budget; desktop-wide dependencies belong in `shells/packages.toml`. Both
  transactions precede ilex files, configuration and overlays.
- **No unit tests.** Every test boots the image (`crates/ilex-e2e`, one
  libtest-mimic binary, tiers `ephemeral::`, `disk::`, `upgrade::`,
  `graphical::`, `tool::`, local opt-in `config::`). Anything worth testing in a shipped binary must be
  observable from inside the VM: give its command group a `--self-check` mode.
- **bootc rules for `rootfs/`** (copied to `/`): defaults go in `/usr`; keep
  `/etc` to files that must live there (it is 3-way merged on upgrade); never
  ship content in `/var` (use tmpfiles.d); create users with sysusers.d.
- Overlays under `shells/` and `machines/` follow the same bootc rules as
  `rootfs/` and only add new files or leaf symlinks. Real directories may be
  shared; replacements, paths through symlinks and `/usr/share/ilex` are
  forbidden, as are `/etc/ld.so.preload`, `/etc/ld.so.conf` and
  `/etc/ld.so.conf.d/**`. Add-only makes added paths visible for review; it does
  not contain root code on installed machines. Run all image configuration
  before overlays, use `python3 -I -S` for build helpers, and run only lint after
  installing overlays. Preserve parent directory metadata with ordinary final
  `COPY`; `/tmp` and `/var/tmp` must be `1777 root:root` in the image.
  Replacements need a future explicit manifest list, not a build
  escape hatch. Manifest inputs enter the build with `COPY`, because buildah's
  cache does not reliably notice changes to `RUN` bind mounts. Package entries
  name Fedora packages or comps IDs; only excludes accept DNF globs.
- `bootc container lint --fatal-warnings` runs last in the build: clean up
  what it flags in the same `RUN` step that created it.
- Portable contracts and selection live in `crates/ilex-tooling-core`; QMP
  framing and keyboard mapping in `crates/ilex-tooling-qmp`; Linux execution,
  paths and slots in `crates/ilex-tooling-linux`. `ilex-devkit` is the
  compatibility facade. Each `ilexctl` group has its own module in the
  crate that owns it. Add a command to the machine build only if a machine
  needs it.
- `drupe-enter` is the only setuid binary we ship and takes no new dependency.
- Shell out to the packaged `bcvk` CLI, not the `bcvk-qemu` library.
- Text files use LF (`.gitattributes`), except token exports, whose bytes are
  preserved exactly; scripts shipped in `rootfs/`
  need the exec bit in git (`git update-index --chmod=+x`).
  Scripts in `build/` run through their interpreter and need no exec bit.
