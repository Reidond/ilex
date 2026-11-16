# ilex

Personal Fedora bootc image: a small base (fedora-bootc, ilex's own files and
drupe's signed-image runtime),
an ordered list of shells (GNOME by default, with desktop apps and design
systems), then one machine configuration. One `Containerfile` builds each root
image: base packages, shells' packages in their own transaction and image layer,
then ilex files and configuration, overlays, lint. A machine is one derived
layer on that root. Rust tooling, e2e-only tests on QEMU via bcvk.

## Build and test

```sh
cargo xtask build              # podman build → localhost/ilex:<slug>, runs bootc container lint
cargo xtask build --shell none # small base → localhost/ilex:<tag>.s-none (128 MiB growth budget)
cargo xtask build --machine vm # build the base, then localhost/ilex:<tag>.m-vm
cargo xtask build --machine vm --on <image> # only a machine layer on an existing image
cargo xtask test ephemeral::   # build if needed, boot in QEMU, assert over SSH
cargo xtask vm                 # interactive ephemeral VM of this checkout's image (--gui: a window)
                               # on Windows: run `cargo xtask vm` in PowerShell, not in WSL
cargo xtask vm --gui --print-image # inspect selection and show the image to boot, without building
cargo xtask info               # resource identity, selection and available configurations
```

- The main checkout reserves the slug `main` (`localhost/ilex:main`, VMs
  `ilex-main-*`). Linked git worktrees (Claude, T3, or made anywhere with
  `git worktree add`) use git's worktree name: the directory name unless git
  had to number it to make it unique. Short, valid lowercase names stay
  unchanged; long or unusual names, and the reserved name `main`, get a
  bounded readable part joined to a hash suffix with `--` (at most 48
  characters, using 16 hex digits of SHA-256 of the original git name).
  Plain slugs never contain `--`, so they cannot equal a hashed slug.
  The same slug selects `localhost/ilex:wt-<slug>`, VMs `ilex-<slug>-*`,
  and runtime and state directories. Never use another checkout's image or VMs.
- `build`, `test`, `vm` and `disk` accept repeatable `--shell <name>`: the
  first shell is the default. Flags override `ILEX_SHELLS` (comma-separated),
  which overrides `image.toml`; an empty environment variable uses the manifest.
  `none` alone selects the small base (`multi-user.target`), without desktop
  apps, enabled update notifications or design systems. Duplicates and unknown
  names fail before podman starts. With shells, design systems come from `image.toml`.
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
- `xtask test` builds an implicit image when missing or its selection is stale;
  after changing other content that ships in the image, pass `--build`.
  Without `--on`, `build --machine <name>` and `test --build --machine <name>`
  always build the generic base first, then build or reuse the machine layer.
  A fully cached base keeps its ID, so an unchanged machine layer is still skipped.
  `--on` deliberately uses the supplied existing base without rebuilding it.
  A missing or mismatched explicit reference must be built or pulled at that
  reference; building this checkout's implicit tag does not repair it.
  Filters are substrings and
  several run their union (`xtask test ephemeral:: upgrade::`); arguments after
  `--` go to the test binary. `ILEX_IMAGE=<tag or ref>` tests another image.
- Reusing an implicit image compares all five selection labels with the current
  manifests and flags. `test` rebuilds a stale selection; `vm` and `disk` refuse
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
  checkout's own tag. The VM uses `uefi-insecure` firmware: Secure Boot needs
  SMM, which KVM can't do under WSL2's nested virtualization.
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
- `iso::` is opt-in and slow: `cargo xtask test iso::` builds the network
  installer (`ILEX_ISO=<path>` reuses one with `compositions.json` beside it).
  `welcome` accepts the unknown machine's chooser default with Enter;
  `chooser` boots the MSI identity, checks rejected HTTP requests inside the
  VM, then selects generic with Shift+Tab, arrows and Tab before Enter.
  Both check the actual `%pre` choice over serial. `detect_board` checks the
  chooser's default using only the baseboard identity. `kernel_argument`
  edits GRUB with real key input on the untouched ISO and asserts the chooser
  was skipped; the OEMDRV `install` trial also asserts that it was skipped.
  GRUB needs 100 ms key holds; vmctl's fast desktop typing drops characters
  there. The chooser HTTP probes are enabled only by the harness's public
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
  Build locally with `cargo xtask iso --testing-tag latest` before composition
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
- Publishing (`xtask publish`) and the installer release only happen in CI.
  Never push to ghcr or create releases from a dev machine.

## Configurations: shells, design systems, machines and users

`image.toml` names the installed shells and design systems and one default of
each when its list is non-empty. `packages.toml` is the base's package list,
installed first in every root image; `systemd-container` supplies the
`systemd-dissect` command Drupe needs even without a shell. `shells/packages.toml`
contains Firefox, Flatpak, libnotify and the PackageKit excludes, installed only
with a shell. These and all selected shells' packages share a second transaction
and image layer, with their combined excludes. A shell's excludes cannot prevent
installation of a base package. The machine remains the last, squashed layer.

`cargo xtask build` checks every root built without shells against the local
fedora-bootc image: growth must be at most **128 MiB** (uncompressed `.Size`).
The error includes both sizes and removes only the rejected build's tag, so a
later test cannot reuse it. Packages that exceed the budget belong in a
shell or machine, or need a justified budget change in a commit. The base adds
ilex-agent, signature policy, the updater and drupe, plus the latter's
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
| Design system | per image, in `image.toml` | per image, `default-design-system` | planned (M1): state in their home, read by a shell; not built |
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
`X_sha-<7 hex>` companions; roots stage unsigned as `candidate.X`. All derived
tags, including the longest suffix, must fit 128 bytes. The default shell must
be first for an image to have a published tag. The five selection labels are
the authority for the image's name. Publications add no image label or file.

| Composition | Local tag (`T` is this checkout's tag) | Publication tag | Build | Rechunk |
| --- | --- | --- | --- | --- |
| Base | `T.s-none` | `base` (always listed) | root | yes |
| Base + GNOME | `T` | `generic.gnome` | root | yes |
| Base + GNOME + ms-7c35 | `T.m-ms-7c35` | `ms-7c35.gnome` | layer on `generic.gnome` | no |
| Base + GNOME + vm | `T.m-vm` | none (unlisted) | layer | no |
| Base + ms-7c35 | `T.s-none.m-ms-7c35` | `ms-7c35`, only if listed with `shells = []` | layer on `base` | no |
| Base + GNOME + ilex (future) | `T.s-gnome.s-ilex` | `generic.gnome.ilex`, once listed | root | yes |

`cargo xtask info --publications` validates the complete list before printing
TSV without a header: tag, kind (`root` or `layer`), parent tag (`-` for roots),
machine name, comma-separated shells (`-` for none). All roots precede layers.
`ilex_devkit::config::publications` exposes these five fields for CI, plus
machine descriptions/match rules and shell descriptions for the installer
catalogue. Its `selection`
field resolves all five expected labels from the same manifest snapshot,
including both design fields (cleared without shells).
`cargo xtask info --installer` prints the validated installer tag.

CI builds and publishes every root, including `base`, and its listed machine
layers. Removing a publication
stops that tag updating. No machine tracks a published tag yet, so names can
change freely for now. Once the first machine is installed, switch it before
removing or renaming its tag (including reordering shells). A tracked tag must
keep moving, and its visible composition must stay the one its person chose.

A shell is `shells/<name>/shell.toml`: its name and description, packages,
Wayland session file (without `.desktop`), compositor command, optional greeter
unit and theme targets. The default shell owns the greeter; GNOME uses GDM.
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
(a VM's `tmp.mount` hides the underlying `/tmp`). Generated
JSON and tokens have separate staging and use directory descriptors with
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

Design systems ship only in images with a shell; the bare base records empty
lists and null defaults and has no design-system directory or tokens.
They are data in `design-systems/<name>/`: `design-system.toml`
names the directory, describes it and names a JSON object token file (default
`tokens.json`). For `ilex`, this is the export of the owner's Claude
design-system artifact titled "ilex". Change values in that artifact and
replace `tokens.json` with the whole new export; never edit it by hand.
A hand-written design system's values are edited in its token file. Nothing
else in the repository repeats a token value. The image ships unchanged bytes
at `/usr/share/ilex/design-systems/<name>/tokens.json`, as recorded in
`image.json`. Token files are stored exactly as exported: `.gitattributes`
disables text conversion for `design-systems/*/tokens.json`, preserving their
original line endings on commit and checkout.
The first and default design system is `ilex`: Breeze frames,
Adwaita interiors and holly green, with light and dark tokens.

`default-design-system` is recorded in `image.json` and the
`ilex.default-design-system` image label; nothing reads it yet. A shell will
use it when the person has chosen nothing, or their chosen design system is
no longer installed in the image. Per-user selection is planned for M1;
neither its state format nor its reader is built.

To add another design system, drop in a directory with `design-system.toml`
(`name` equal to the directory name, `description`, optional `tokens`) and its
token file, then add the name to `design-systems` in `image.toml`. Set
`default-design-system` there to choose the image's default, and rebuild the
image; no code changes are needed. A shell asks for generated outputs with
`[theme] targets`. None does yet: targets must be empty until the M1 generator
defines them, and nothing renders these tokens today. Fonts and wallpapers
are not part of a design system directory yet.

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
cannot have match rules. `xtask info` and `iso` reject identical/subset rules
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

`build`, `test`, `vm` and `disk` accept `--machine <name>`, overriding
`ILEX_MACHINE`, then `generic`. In WSL `vm --gui` adds `vm` (spice-vdagent for
clipboard) only when the selected image is generic. An already selected machine
image is booted unchanged: `--machine vm` and `--image <own .m-vm>` reuse it;
another machine prints that the viewer's clipboard needs machine `vm`.
An explicit older image without an `ilex.machine` label also boots unchanged
with that clipboard message. It is still refused as a machine-layer base.
`--print-image` inspects labels to apply this same decision without building or
booting. On Windows it forwards that inspection to WSL without starting a viewer.
The implicit tag is `<checkout tag>[.s-<shell>...].m-<machine>`. A layer built
on an explicit base, including a digest or another repository's image, always
gets a local tag in this checkout's namespace: `<checkout tag>.b-<16 hex SHA-256
of base reference>.m-<machine>`. This prevents overwriting another checkout's
layer and lets distinct bases coexist. A base already in this checkout's own
shell-variant namespace keeps that prefix. Only an explicit build `--tag`
chooses another output name. Both suffixes count towards the 128-byte tag limit.
`xtask build --machine <name> --on <image>` builds only the layer and prints its
resulting full reference on stdout; scripts must use that result rather than
append a suffix to their input. Build subprocesses (including base builds and
pulls) send stdout to stderr, so command substitution captures only the reference:

```sh
image=$(cargo xtask build --machine "$machine" --on "$base")
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
set the configuration labels: build with `cargo xtask build` to test or rechunk
the result.
`cargo xtask info` honours `ILEX_SHELLS`, listing the resolved image, selection
and available configurations; builds
record them in the five `ilex.shells`, `ilex.default-shell`,
`ilex.design-systems`, `ilex.default-design-system` and `ilex.machine` labels.
The fingerprint covers the selected manifests, overlays and build scripts;
old images without configuration labels must be rebuilt.
Fingerprint v3 includes `shells/packages.toml` only with a shell, and excludes
`image.toml`: the five selection labels already cover its resolved selection.
An edit to an unselected shell or design system does not change the base's
fingerprint. An agent or `rootfs/` edit reruns no package transaction.

The image records schema 1 JSON in `/usr/share/ilex/image.json` and
`/usr/share/ilex/machine.json`, and selected design systems' tokens under
`/usr/share/ilex/design-systems/<name>/tokens.json`. `ilex-agent config` prints
the selection; `ilex-agent config --json` returns both documents. The agent's
self-check validates the configuration, sessions, greeter, target, tokens and
machine packages, plus the literal non-group entries of `packages.install` and
`shell-packages.install` recorded in `image.json`. With no shell, the empty
shell-package/session/token checks pass and the target must be `multi-user.target`;
the OS, bootc, install config, signature policy and drupe checks still run.
`ephemeral::configuration` compares both package lists with the checkout,
alongside labels, overlays and token hashes. A shell-free generic base must have
no sessions, display manager, Firefox or Flatpak; a machine may add those packages.
Every shell-free image must omit design systems and globally enabled notifier
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
`mangohud`. Steam itself is a Flatpak the person installs from Flathub; it is
not in the image. The host's `steam-devices` udev rules give it controller
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
flatpak install flathub \
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
The shell-free image installs no AccountsService daemon or package.

Keep Anaconda's distinctly named local wheel administrator for maintenance.
Create the daily account locally from that account:

```sh
sudo ilex-agent home-create alice --disk-size=20 --real-name='Alice'
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

`cargo xtask test homed::` is opt-in and uses the installed-disk QEMU/bcvk
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

## Public user profiles

`image.toml` selects public profiles with `users = ["reidond"]`. This is a
fourth configuration dimension; every root (including shell-free `base`) and
its machine layers carry the same catalogue. No publication name, OCI layer
selection or image label gains a user dimension. Images work without selecting
a profile: `home-create` still accepts a custom local username.

`users/<id>/user.toml` is a template, never an exported homed record. Schema 1
requires `schema`, positive `revision`, `username` equal to the directory,
`display-name`, `shell = "bash"`, `locale` such as `en_US.UTF-8`,
`storage = "luks-btrfs"`, and `disk-size-gib` (2–1048576). Optional `groups`
can name only `wheel` and `gamemode`; provisioning must explicitly authorize
administration and check group availability. Applying a profile never changes
an identity, storage size or groups. Unknown fields, including homed secret,
privileged, password, recovery, binding/status and signature fields, fail the
build. The minimal `reidond` example has no email, credentials or administrative
groups; the owner should adjust its name/settings before use.

Optional `settings.toml` accepts `[gnome]` with `color-scheme` (`default`,
`prefer-dark`, `prefer-light`), boolean `clock-show-weekday`, and `clock-format`
(`12h`, `24h`). No command strings or arbitrary dconf paths are accepted.
Optional `apps.toml` accepts `[[flatpaks]]` entries with an application `id`
and `remote = "flathub"`. Apps install only on explicit `--install-apps`,
using an already configured user remote; failures can be retried after login.
No network operation runs automatically or gates authentication.

Initial `home/` files are deliberately restricted to `.gitignore`,
`.editorconfig`, `.config/git/ignore` and `.config/ilex/<name>.txt|toml|json`.
Only small, non-executable UTF-8 regular files are supported. Symlinks,
special files, traversal, credentials, identity/keyring paths and login hooks
are refused. Extend these allowlists by review. The composer reads every
profile input through no-follow directory descriptors and stages canonical
`users.json` (schema 1), `users/<id>/user.json`, and `home/` templates through
the existing generated-file installer and `.generated.json`. Profile inputs
enter root content/cache and installer fingerprints.

After `sudo ilex-agent home-create NAME --disk-size=20`, authenticate locally
as the new daily user and run `ilex-agent profile list`, then
`ilex-agent profile apply reidond`. A public profile can seed a custom homed
username; its identity suggestions remain metadata for setup. Apply refuses
root, an inactive/non-LUKS home, or a caller/home UID mismatch. It uses NSS,
not `$HOME`, and writes with owned no-follow directory descriptors. Nothing
imports or rewrites `.identity`, `.identity-blob`, keyrings or credentials.

`~/.local/state/ilex/profile.json` stores schema, profile revision/digest and
managed file/setting hashes or values. Missing files are seeded atomically;
updates replace only the last managed contents. Existing files, edits,
deletions, symlink/hardlink paths and explicitly set desktop settings are
preserved and reported as conflicts (exit 1). Invalid/newer state, a newer
profile revision, or changed content without a revision bump declines updates
(exit 2). Never lower a revision during rollback. Removed profile files are
left in place. The per-user `ilex-profile.service` reapplies only an already
selected profile on login, without app installation; no selection is a no-op.
There are no root-run profile hooks. Installer choice/provisioning UX is PR 22.

`cargo xtask test ephemeral::user_profiles homed::profiles -- --test-threads 1`
checks catalogue/schema/path/registry rejection in a booted image and profile
application through PR 20's real creation/TTY path in an enforcing installed
guest. The latter uses 2 GiB and no UI automation, tests user-edit/deletion and
link preservation, future-state/rollback refusal, opt-in app failure, and a
successful relogin while the older image's user service declines newer state.
On a GNOME image it also checks scoped dconf updates and user-setting conflicts.

## Build sharing: kache

Image builds compile through [kache](https://github.com/kunobi-ninja/kache), a
compiler cache keyed by the content of each rustc invocation's inputs. The
Containerfile's builder stage installs a pinned release tarball (v2.0.0,
sha256-checked; v2 needs both `kache` and `kache-engine`) and runs
`cargo build` with `RUSTC_WRAPPER=kache`. The store is the podman cache mount
`ilex-kache`, shared by every worktree; the target directory is fresh per
build, so nothing trusts mtimes and one worktree's binaries can't leak into
another's image. The step ends with `kache stats --last-build`: crates
restored vs compiled.

- `xtask build` also mounts the host's own store (`$KACHE_CACHE_DIR`, else
  `~/.cache/kache`, once it holds an `index.db`) read-only at `/kache-host`,
  and the builder reads it only when present, so a plain `podman build` and CI
  work without it (`--no-host-kache` skips it). After a host `cargo build
  --release -p ilex-agent` with kache set up (`cargo install kache && kache
  init`, once per machine; inside WSL on Windows), the image build restores
  those crates instead of compiling them.
- To bump kache, change the version in the URL and the sha256 in the
  Containerfile (`gh release view vX.Y.Z -R kunobi-ninja/kache --json assets`).

## Windows: run everything in WSL2

podman, bcvk and QEMU are Linux-only. On Windows the stack lives in the
`FedoraLinux-45` WSL2 distro (nested virtualization gives it `/dev/kvm`).
Run xtask there, from the checkout's directory (WSL translates the cwd):

```sh
wsl -d FedoraLinux-45 -u root -e bash -lc 'cargo xtask build'
```

- `-e bash -lc` matters: `-e` skips WSL's outer shell (which would expand
  `$VARS` too early), and the login shell sources
  `/etc/profile.d/ilex-dev.sh`, which points `CARGO_TARGET_DIR` at the Linux
  filesystem for `/mnt/*` checkouts.
- In Git Bash, prefix with `MSYS_NO_PATHCONV=1` when passing Linux paths.
- Image builds and VM boots take minutes: run them in the background.
- `cargo check`/`clippy` work natively on Windows too, for quick feedback.
  `cargo clippy --workspace --all-targets` works on Windows and Linux; the
  `config::` fixtures that create Unix modes and symlinks are compiled on Unix
  and run in WSL on Windows.
- `cargo xtask vm` is the exception: run it on Windows itself (PowerShell or
  Git Bash, in the checkout). It hands itself to the distro (`wsl.exe -d
  FedoraLinux-45 -u root --cd <checkout> -e bash -lc 'cargo xtask vm …'`;
  `ILEX_WSL_DISTRO` names another). With `--gui` it first builds
  `ilex-viewer.exe` (the VM's window, see "The VM window on Windows") and
  starts it when the WSL side prints its `ilex-viewer: --connect …` line.
  WSL can't start it itself: `/mnt/c` is mounted with `fmask=133`, so no
  `.exe` there has the execute bit interop needs.
  `ILEX_SHELLS` and `ILEX_MACHINE` reach WSL through `WSLENV`, including without `--gui`.
  Selection is validated natively before building the viewer or entering WSL.
- Nothing here uses WSLg any more. It is unreliable on this machine: on most
  starts of WSL 2.7 it fails to open its shared memory with Windows
  (`/mnt/wslg/weston.log`: `rdp_allocate_shared_memory: Failed to open …`,
  `use_gfxredir = 0`; microsoft/WSL#40618), and then every Linux window only
  gets a `[WARN:COPY MODE] …` taskbar button and is never drawn, until WSL
  starts again (`wsl --shutdown`). It also mounts `/tmp/.X11-unix`
  read-only, so an Xvfb in WSL is only reachable on its abstract socket.

## Driving VMs with ilex-vmctl

`cargo xtask disk` writes the installed qcow2 (`bcvk to-disk`); `cargo xtask
vm --disk` boots it headless on the graphical test machine (UEFI, 4K
`virtio-vga`), and `--gui` adds a window for a human (in WSL the native
Windows viewer below, elsewhere a GTK window). Every disk-booted VM (also the
`disk::` and `graphical::` tests) publishes `qmp.sock`, `vnc.sock` and
`vm.toml` in
`$XDG_RUNTIME_DIR/ilex-vm/<slug>/<vm>/`; `ilex-vmctl` attaches to them by slug
(from the checkout you are in, or `--slug`) and never starts or stops VMs.

```sh
cargo install --path crates/ilex-vmctl   # once per machine (inside WSL on Windows)
ilex-vmctl list
ilex-vmctl screenshot -o shot.png        # read the PNG to see the screen
ilex-vmctl wait-screen --stable 3        # not blank, then unchanged for 3s
ilex-vmctl click 1920 1080               # framebuffer pixels (3840x2160)
ilex-vmctl type 'hunter2' --enter        # US layout; ilex-vmctl key ctrl+alt+t
```

Agents get the same tools (`screenshot`, `click`, `scroll`, `type_text`, `key`,
`wait_screen`, `vm_list`) as an MCP server; screenshots come back downscaled
and `click` takes coordinates in that downscaled image:

```sh
claude mcp add --transport stdio ilex-vm -- ilex-vmctl mcp
# Windows host (the server must run inside WSL, in the worktree's directory):
claude mcp add --transport stdio ilex-vm -- wsl.exe -d FedoraLinux-45 -u root -e bash -lc 'ilex-vmctl mcp'
```

- QMP serves one client at a time; vmctl connects per command, so an agent
  and a running test can share a VM. One VM per purpose and checkout: `vm`
  (`xtask vm --disk`), `gui` (`--gui`), `disk` and `graphical` (tests); pick
  with `--vm` when more than one runs.
- Tests assert over SSH, not pixels. Screenshots are evidence: the
  `graphical::` tier saves them to `<state dir>/artifacts/graphical/` (or
  `$ILEX_ARTIFACTS/graphical/`).
- `disk::` also checks headless images. `graphical::login` exercises GNOME's
  login and scaling, and fails immediately before installing or booting a VM
  when the image's default shell is not `gnome`, including `--shell none`.
- The SSH test key and the throwaway disk overlays live in the state dir;
  `cargo xtask vm --disk` prints the exact `ssh` command line.
- `--gui` gives the guest a GPU when the host has one (`/dev/dxg` in WSL, a
  DRM render node elsewhere; `--no-gl` opts out): `virtio-vga-gl` with
  `-display gtk,gl=on`, so virglrenderer renders the guest's OpenGL on the
  host (in WSL through Mesa's D3D12 driver on the Windows GPU), and half the
  host's threads as vCPUs (4 to 8). QEMU allows no VNC server next to a GL
  display, and QMP `screendump` fails on virgl output ("no surface"): in WSL
  `ilex-vmctl screenshot` takes the display bridge's frames instead
  (`screen.sock`, `vm.toml` says `display = "viewer"`); a GL GTK window on a
  Linux desktop (`display = "gtk-gl"`) takes vmctl input but no screenshots.

## The VM window on Windows

`cargo xtask vm --gui` in WSL doesn't open a Linux window; the VM's window is
`ilex-viewer.exe` (`crates/ilex-viewer`), a native Windows program, fed by a
display bridge in `xtask vm` (`crates/ilex-devkit/src/display.rs`, protocol
in `viewer.rs`):

- QEMU draws into a private Xvfb (`-display gtk,gl=on,show-menubar=off`;
  Mesa's D3D12 driver renders there too, and nothing tears on resize). The
  bridge watches the Xvfb root with XDamage and reads damaged rectangles
  straight from Xvfb's framebuffer, which `-fbdir` keeps as a memory-mapped
  XWD file on `/dev/shm` (`GetImage` took 11 ms for a 2800x1750 frame and
  stalled the single-threaded Xvfb meanwhile). It compares them in 64x64
  tiles with what the viewer has and sends only changed tiles, raw BGRX, on
  a thread of their own, over TCP to WSL's own address (Windows reaches it at
  ~2.4 GiB/s; the `localhost` relay manages ~550 MiB/s). A token printed with
  the address keeps others out.
- Measured with GNOME's overview toggled every 0.35 s through QMP, 2800x1750:
  the guest presents 75 fps, the viewer receives and draws 37-41 frames/s
  (the overview rests part of each cycle; about 55 during the motion), the
  bridge spends ~7 ms per frame; the WSLg window managed 32.5 the same way.
  The limit now is QEMU getting frames onto the Xvfb (a GPU readback and a
  20 MB `XPutImage` each). Typing and pointer work: ~50 frames/s, a few
  MiB/s. `cargo xtask` is a dev build, so the workspace's `profile.dev`
  optimizes ilex-devkit, ilex-viewer and all dependencies (unoptimized, the
  same path managed 6 frames/s).
- The viewer (winit + softbuffer) draws them 1:1: its window's size becomes
  the QEMU window's size on the Xvfb, which QEMU hands to the guest (EDID),
  so the guest's resolution follows the window. The guest's pointer image
  comes over XFixes and becomes the window's own cursor.
- Keyboard: a `WH_KEYBOARD_LL` hook takes every key while the window has
  focus and swallows it, so the Windows key, Alt+Tab, Ctrl+Esc reach GNOME
  instead of Windows (Ctrl+Alt+Del can't be hooked). The hook lives in a
  helper process (`ilex-viewer --keyboard-hook <hwnd>`, started by the
  viewer, ends with the window) that writes keys to the viewer over a pipe:
  Windows didn't call a hook in the process that owns the foreground window
  (only while another process had a hook too), and a hook on a busy UI
  thread would hit `LowLevelHooksTimeout`, after which Windows drops it for
  good. Keys that reach the window anyway are forwarded from winit's key
  events by scan code, so typing never depends on the hook. Keys travel as
  QEMU qnums, which are Windows' set-1 `scanCode` plus `0x80` for extended
  keys, so there is no keymap: the guest's layout decides. Losing focus
  releases every held key. Pointer and keys go to QEMU on a QMP socket of
  the bridge's own (`input.sock`), so `ilex-vmctl` keeps `qmp.sock`. To
  test the hook from a script, send keys with `keybd_event`/`SendInput`
  (the input stream hooks see); a tool that posts window messages bypasses
  it and only exercises the fallback.
- After a while on GNOME's lock screen the guest turns the display off and
  the window shows QEMU's "Display output is not active"; moving the
  pointer over the window or pressing a key wakes it. While the guest
  catches up with a resized window, the viewer stretches its last frame
  (and scales the pointer) instead of showing it in a corner.
- Clipboard (text, both ways): on a generic image `--gui` installs machine `vm`
  (`spice-vdagent`) through `machines/Containerfile`, tagged `<checkout tag>.m-vm` for the default
  base (explicit bases follow the local `.b-<hash>` rule above), never
  published, rebuilt only when the base or machine inputs change,
  per its `ilex.machine-key` label, because every rebuild's new digest makes bcvk
  install the disk again. Existing machine images are used unchanged; only `vm`
  supplies the viewer's clipboard agent. QEMU puts the guest's SPICE agent port on
  `vdagent.sock`, and the bridge speaks the agent protocol itself
  (`vdagent.rs`). QEMU's own `qemu-vdagent` chardev doesn't work here:
  `spice-vdagentd` reads EOF from it ("lost spice client connection") and
  drops everything, and QEMU's GTK clipboard is broken on Wayland hosts
  (qemu-project/qemu#1455). Two guest quirks the bridge works around:
  `spice-vdagentd` closes and reopens the port whenever the session agent
  changes (GDM to the user's session) and needs our capabilities again, so
  the bridge announces until it answers and re-offers on every guest
  announcement; and mutter's first fetch of a new X11 clipboard owner's
  text comes back empty, so each Windows copy is offered twice, 500 ms
  apart (else the first paste in GNOME pastes nothing).
  Old checkout-owned `...-vm` images can be removed by their own tag with
  `podman rmi`; do not prune the shared container storage.
- Closing the window quits QEMU; QEMU exiting closes the window.
  `ILEX_VIEWER_STATS=1 cargo xtask vm --gui` makes the viewer print frames
  received and drawn per second, the bridge its frames per second and
  milliseconds per step (`WSLENV` carries the variable over), and logs the
  guest agent's messages.
- GPU paravirtualization is virgl (OpenGL). Vulkan in the guest (Venus over
  the same virtio-gpu) is untried: WSL has a host Vulkan, Mesa's Dozen on
  D3D12 (`Microsoft Direct3D12 (<GPU>)`, Vulkan 1.2, external memory fds),
  and Fedora's virglrenderer includes Venus, but it needs blob resources
  (`blob=true,hostmem=…` and memfd-backed guest RAM), and Dozen isn't a
  conformant driver. VA-API video through Mesa's D3D12 driver doesn't
  initialize in WSL (no `/dev/dri`).

## Developer tools: drupe

A prototype of GNOME OS's [Toolpak](https://blogs.gnome.org/alatiera/2026/09/26/introducing-toolpak/)
idea under ilex's own name and format (Toolpak has published neither code nor
a format yet): tools like `strace` that can't run in a container and have no
place in an image-based `/usr`. A tool is one signed disk image holding its
commands and every library they load; running a command mounts that image at
`/app` for that process tree only and executes it as the calling user with
the rest of the machine untouched (`crates/drupe`).

```sh
cargo xtask tool strace                  # dev machine: <state dir>/tools/images/strace.raw + drupe.crt
cargo xtask tool net nmap tcpdump        # one tool `net` with the commands of two packages
drupe install strace.raw                 # on the machine, as root; then just `strace …`
drupe list; drupe remove strace; drupe run strace -V
```

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
  the kernel keyring). The image ships none: `cargo xtask tool` signs with a
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
  `sudo drupe run strace …`.
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
- `tool::` tier (`cargo xtask test tool::`, not run in CI): builds the strace
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
and on demand. `image.toml` supplies the installer composition (currently
`generic.gnome`) and `info --publications` supplies the validated tag, kind,
parent, machine and ordered shells of every publication. This workflow builds
the installer root first, then every other root, including `base`.
The existing legacy tags (`latest`, `candidate`, bare date and `sha-` tags)
stay in the registry: CI neither moves nor deletes them.

Three paths start together on separate runners:

- `build`: snapshot each root tag's own digest (explicit `-` for absence),
  check selected inputs → build its shell selection → `changed` against that
  immutable snapshot → rechunk → unsigned
  `candidate.generic.gnome` through `publish --stage --as` → `ephemeral::`.
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
- `upgrade`: `.github/upgrade-from.sh` resolves the installer's tag as v1;
  only explicit absence permits legacy `latest` as the first-run fallback.
  Registry/authentication errors are fatal. It records `own` (the installer
  tag's digest or `-`), v1's digest and `exists`. Its cached disk key is
  `v1-disk-boot-mask-<bcvk>-<v1 digest>`, with plain image install options.
  The earlier kernel-argument profile is not restored. It installs
  v1 itself on a cache miss, in the background with the VM stack. It waits for
  build's "Stage the candidate", pulls `candidate.<installer>` by digest, and
  runs `upgrade::` on it. With neither source present, `exists=false`, v1 `-`
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
Unsigned `candidate.X` is the sole staging namespace. Runs use one serialized
`publish` concurrency group; v1-disk has its own group and writes no image tag.

Immediately before each publisher, `.github/publish-head.sh image` reads that
**tag's own** revision and main's head and requires published → run → main
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
have published. An unchanged main root resolves its current signed tag once,
even if `test=true` staged an unsigned equivalent. Branches test the staged
parent. `machine-base.sh` requires root configuration labels, verifies the
pulled image ID equals the registry config digest and checks generated-file
and overlay metadata. There is no unlabelled legacy-parent skip: these new
parent tags can only have been created by the composition publisher.

Planning snapshots each layer's own tag and compares inputs and parent ID using
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

`publish --as` requires a listed composition and recomputes its tag from all five
selection labels against the row's complete declared selection, including both
design fields. Matching the tag spelling alone is insufficient. Roots accept `--stage` or `--digest --tested-fingerprint`; layers require
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
published parent, verifies all five parent labels, current machine inputs/key
and exactly the parent's filesystem diff IDs plus one layer. CI's ancestry,
snapshot and parent-still-current guards remain necessary too. The resolved
parent must also verify against the same permitted public key before signing
a layer.

All signing jobs pin kache-action to `ad7317540dcdd71c904f7a60a87b620a0c0e67ed`,
binary v2.1.1, empty token and PR comments disabled; cosign-installer v4.1.3 is
pinned to `756fe2d284c7292c4e9b62c8a28f1e5629c59532`. `build-publisher.sh` builds
with `--locked` and discovers the executable from Cargo metadata before the
key-bearing step. No Cargo, rustc, kache or build script runs with the key.
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
a forced test never republishes unchanged content. Shell packages/manifests,
overlays and design tokens affect only roots selecting them. Base packages,
`rootfs`, shipped crate trees and common build helpers affect both roots. A
base-only input change cannot leave the desktop unchanged when that input is
shared; independent RPM/content outcomes and snapshots can still let either
root skip while the other publishes. Neither changing `[[publish]]` alone nor
an unrelated shell changes an existing root's inputs. Fedora images are pulled
once before these checks, so nightly upstream changes invalidate both keys.

```sh
cargo xtask info --publications
cargo xtask info --installer
cargo xtask changed ghcr.io/reidond/ilex:base --root base --inputs-only
cargo xtask build --shell none
cargo xtask changed ghcr.io/reidond/ilex:base --image <base-image>
cargo xtask changed ghcr.io/reidond/ilex:generic.gnome --image <image>
cargo xtask rechunk <image> --output <rechunked>
cargo xtask publish <rechunked> --stage --as generic.gnome
cargo xtask publish --digest <staged-digest> --as generic.gnome --tested-fingerprint <fingerprint>
cargo xtask changed <ref> --machine ms-7c35 --inputs-only --base-id <config-digest>
cargo xtask prepare-publish <machine-image> --machine ms-7c35
ILEX_IMAGE=sha256:<tested-id> cargo xtask test ephemeral::
cargo xtask publish <machine-image> --as ms-7c35.gnome --tested-image-id <tested-id>
```

Publishing and releases only happen in CI. A branch dispatch with `test=true`
builds, stages every `candidate.<root>` including `candidate.base`, runs their
ephemeral tests, upgrades the installer (including legacy fallback) and
tests machine layers; it signs nothing and moves no published tag. Add `iso=true`
to build the ISO as an artifact on a branch; the installation/release job is
main-only because its network install exercises the published target, not the
branch candidate. PR/fork events cannot reach signing.

The sole local publishing override is an explicitly allowed loopback registry,
never ghcr, even with `CI=true`. Run a checkout-specific `registry:2` container
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
On every run, `cargo xtask iso --due` decides, in the `installer-due` job (the `iso`
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
and keep its credentials for administration and recovery. This is a traditional
local account. A separate daily user managed by systemd-homed and created at
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
These are nonsecret contracts only: no TPM probing/enrollment, encrypted home
setup or profile provisioning is delivered here. See `installer/setup/README.md`
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
verification, `cargo xtask iso --check-registry` checks every offered tag and
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
cache, checks disk space and the ISO checksum, then runs
`ILEX_ISO=<downloaded artifact> cargo xtask test iso::` with the verified tag and
`ILEX_ISO_DIGEST`. No ilex container is built by that invocation. The tier boots
Anaconda, installs onto a sparse blank 40 GiB disk, boots it, and asserts the
booted and tracked `generic.gnome` registry reference, the exact verified
digest, system `running`, no failed units and GDM up. It requires KVM and 6 GiB
for the installer VM, on the same runner selection as other tiers; it adds
about 15 minutes only when an ISO was built. Workflow runs are serialized
(v1-disk has a separate group but never moves tags), so this workflow cannot
move the tag between verification and installation. Before release it also
re-reads that tag, requires the same digest, verifies its signature again and
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
cargo xtask iso                # build; the ISO, its .sha256 and release-notes.md land in <state dir>/iso/
cargo xtask iso --due          # is a new ISO due? (reads the latest installer release; needs gh)
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
  (`--testing-tag latest`); `firefox-langpacks` remains installed. `xtask iso`
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
  needs ("mount: … permission denied"; WSL has no AppArmor, so local builds
  never see it). `/var/lib/containers`, `/var/cache/image-builder` and
  `TMPDIR` live on volumes under `/mnt`. `xtask iso` does the same locally: when
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
  /run/ostree-booted`, ordered before the updater) runs `ilex-agent
  enforce-sigpolicy` at every boot. When the booted deployment tracks
  `ghcr.io/reidond/ilex` (any tag or digest) over the registry transport with
  no verification, it rewrites the origin with `bootc switch --mutate-in-place
  --enforce-container-sigpolicy <same ref>` (no download, no new deployment, no
  reboot) and checks that `bootc status` now shows `"signature":
  "containerPolicy"`. It does nothing on ephemeral VMs (no deployment), other
  images or transports (test images in containers-storage, other registries,
  `ghcr.io/reidond/ilex-iso`) and machines that already verify, so there is
  nothing to run by hand; `ilex-agent enforce-sigpolicy --dry-run` shows what
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

1. Repository secrets, from Git Bash (the key and password go through the
   pipe, never to the terminal):

   ```sh
   MSYS_NO_PATHCONV=1 wsl.exe -d FedoraLinux-45 -u root -e cat /root/ilex-secrets/cosign.key \
     | gh secret set COSIGN_PRIVATE_KEY --repo Reidond/ilex
   MSYS_NO_PATHCONV=1 wsl.exe -d FedoraLinux-45 -u root -e cat /root/ilex-secrets/cosign.password \
     | gh secret set COSIGN_PASSWORD --repo Reidond/ilex
   ```

2. Make the `ilex` ghcr package public so machines and the installer need no
   credentials. The package only exists after the first publish: run the
   workflow once (`gh workflow run publish.yml --repo Reidond/ilex`), then open
   github.com → your profile → Packages → `ilex` → Package settings → Change
   visibility → Public. There is no API for this.
3. Check that Actions may publish: Settings → Actions → General → Workflow
   permissions should allow the `packages: write` the workflow requests, and
   a package created by the first run must list this repository under
   "Manage Actions access" (it does when the image carries the
   `org.opencontainers.image.source` label, which `xtask build` sets).
4. Nothing to set up for the installer: its releases use `GITHUB_TOKEN` (the
   `installer-release` job asks for `contents: write` and `packages: read`; if Settings → Actions → General →
   Workflow permissions is read-only by default, a job-level `permissions:`
   still raises it). The old `ilex-iso` package, if it exists, is unused and can
   be deleted, and so can `ilex-installer-base`.

## Conventions

- `packages.toml` belongs to the base and stays within its 128 MiB growth
  budget; desktop-wide dependencies belong in `shells/packages.toml`. Both
  transactions precede ilex files, configuration and overlays.
- **No unit tests.** Every test boots the image (`crates/ilex-e2e`, one
  libtest-mimic binary, tiers `ephemeral::`, `disk::`, `upgrade::`,
  `graphical::`, `tool::`, local opt-in `config::`). Anything worth testing in a shipped binary must be
  observable from inside the VM: give it a `--self-check` mode.
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
- Shared host-side plumbing (slug, paths, VM slots, git) lives in
  `crates/ilex-devkit`; each `xtask` subcommand is its own module with its
  own `clap::Args`.
- Shell out to the packaged `bcvk` CLI, not the `bcvk-qemu` library.
- Text files use LF (`.gitattributes`), except token exports, whose bytes are
  preserved exactly; scripts shipped in `rootfs/`
  need the exec bit in git (`git update-index --chmod=+x`).
  Scripts in `build/` run through their interpreter and need no exec bit.
