# ilex

Personal Fedora bootc image: one `Containerfile` on `quay.io/fedora/fedora-bootc:45`
(GNOME desktop), a Rust workspace for all tooling, e2e-only tests on QEMU via bcvk.

## Build and test

```sh
cargo xtask build              # podman build → localhost/ilex:<slug>, runs bootc container lint
cargo xtask test ephemeral::   # build if needed, boot in QEMU, assert over SSH
cargo xtask vm                 # interactive ephemeral VM of this checkout's image (--gui: a window)
                               # on Windows: run `cargo xtask vm` in PowerShell, not in WSL
cargo xtask info               # slug, image tag, VM prefix, state dirs, VM slots
```

- The slug comes from the checkout path: the main checkout is `main`
  (`localhost/ilex:main`, VMs `ilex-main-*`); a worktree at
  `.claude/worktrees/<name>/` is `<name>` (`localhost/ilex:wt-<name>`, VMs
  `ilex-<name>-*`). Never use another checkout's image or VMs.
- VM boots take one of N machine-wide slots (`ILEX_VM_SLOTS`, default one per
  8 GiB RAM, max 4) and wait when none is free.
- `xtask test` only builds the image when it doesn't exist yet: after changing
  anything that ships in the image, pass `--build`. Filters are substrings and
  several run their union (`xtask test ephemeral:: upgrade::`); arguments after
  `--` go to the test binary. `ILEX_IMAGE=<tag or ref>` tests another image.
- An ephemeral VM has no ostree deployment, so `bootc status` there names no
  image; `ephemeral::` identifies the image through the container bcvk started.
- `upgrade::` installs a real libvirt VM (`bcvk libvirt run`, a few minutes,
  then cached per image under `/var/lib/libvirt/images`), so run it when
  touching the update path. v1 is the image under test unless
  `ILEX_UPGRADE_FROM=<ref>` names an older one (CI: the published `:latest`);
  with `CI` set and no `ILEX_UPGRADE_FROM` the tier is skipped. v2 is the
  candidate plus a marker layer, tagged `<image tag>-upgrade`, never the
  checkout's own tag. The VM uses `uefi-insecure` firmware: Secure Boot needs
  SMM, which KVM can't do under WSL2's nested virtualization.
  On v2 the same VM checks signature enforcement: v2's `ilex-sigpolicy.service`
  left the containers-storage origin alone at boot; the origin is pointed at
  `ghcr.io/reidond/ilex:latest` unverified (the state an ISO install leaves),
  and after a reboot the unit must have added the enforcement (a second run
  is a no-op). Then the rollback to v1. One VM, one install: installing is
  most of the tier's time. `ILEX_UPGRADE_PREPARE=1` only installs and boots v1
  (filling bcvk's base-disk cache) and stops; CI runs that ahead of time and
  caches the disk (see CI publishing).
- `iso::` is opt-in and slow (the ISO takes ~8 minutes to build once the
  installer image is cached, the install and boot ~8 more), so it only exists
  when a filter names it: `cargo xtask test iso::` builds the installer ISO
  (`ILEX_ISO=<path>` reuses one), installs it unattended in QEMU/UEFI onto a
  blank disk over QEMU's user networking, boots the result and asserts over SSH
  that `bootc status` shows the booted image tracking
  `ghcr.io/reidond/ilex:latest` at the digest the registry serves, that the
  system is `running` with no failed units and that GDM is up. The installer
  is a network installer: what it installs is the *published* image, not the
  image under test. Screenshots of Anaconda's welcome screen and of the first
  boot land in `<state dir>/artifacts/iso/`.
- Publishing (`xtask publish`) and the installer release only happen in CI.
  Never push to ghcr or create releases from a dev machine.

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
- `cargo xtask vm` is the exception: run it on Windows itself (PowerShell or
  Git Bash, in the checkout). It hands itself to the distro (`wsl.exe -d
  FedoraLinux-45 -u root --cd <checkout> -e bash -lc 'cargo xtask vm …'`;
  `ILEX_WSL_DISTRO` names another). With `--gui` it first builds
  `ilex-viewer.exe` (the VM's window, see "The VM window on Windows") and
  starts it when the WSL side prints its `ilex-viewer: --connect …` line.
  WSL can't start it itself: `/mnt/c` is mounted with `fmask=133`, so no
  `.exe` there has the execute bit interop needs.
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
- Clipboard (text, both ways): `--gui` installs the checkout's image plus
  `vm/Containerfile` (`spice-vdagent`; tagged `<image tag>-vm`, never
  published, rebuilt only when the base image or the Containerfile changes,
  per its `ilex.vm-key` label, because every rebuild's new digest makes bcvk
  install the disk again), QEMU puts the guest's SPICE agent port on
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

`.github/workflows/publish.yml` runs on every merge to `main`, nightly at
03:00 UTC and on demand. Three jobs start together, so the slow parts overlap
on separate runners:

- `build`: build → `changed` → `rechunk` → stage the candidate in ghcr as
  `:candidate` (`xtask publish --stage`: unsigned, never `:latest`) →
  `ephemeral::`. In the background from its first minute (`.github/vm-prep.sh
  ephemeral`): the VM stack, and a pull of `:latest`, which records the
  registry's compressed blob for each layer, so staging uploads only the
  layers that changed and machines download only those (`--stage` prints how
  many: "N of M layers are new against :latest").
- `upgrade`: the upgrade tier's v1 (the published `:latest`, by digest)
  already installed to a disk, restored from the Actions cache; from its first
  minute (`vm-prep.sh upgrade`) the VM stack, a pull of v1 and v1's disk
  booted once with `ILEX_UPGRADE_PREPARE=1`. It polls the jobs API until the
  build job's "Stage the candidate" step succeeds (or the build job ends
  without it: nothing to test), pulls `:candidate` by digest and runs
  `upgrade::`.
- `installer-due` → `iso` (see Installer ISO).

Then `publish` (main only, when the image changed) signs the staged digest,
after checking it is the one the upgrade tier tested, and moves `:sha-<short>`,
`:YYYYMMDD` and `:latest` to it (`xtask publish --digest`: tags in the
registry, nothing pushed twice). Runs of the workflow never overlap (one
concurrency group), so `:candidate` is always the current run's.

Installing v1 to a disk takes ~14 minutes on a runner (11 of them bcvk's
install VM importing the image's 64 layers into ostree), more than everything
else in a run, so it happens ahead of time: after each publish the `publish`
job starts the workflow again with `v1_disk`, a run of just the `v1-disk` job
in a concurrency group of its own (nothing waits for it; a newer one cancels
it). It installs the new `:latest` the way the upgrade tier does and saves
bcvk's base disk (~5 GB, 1.9 GiB in the cache) in the Actions cache under the
image digest and the bcvk version (`v1-disk-<bcvk>-<digest>`), with the two
`user.bootc.*` xattrs bcvk recognizes a base disk by in a file next to it (the
cache drops xattrs); older disks are deleted. The next run's upgrade job
restores it (~40 s) and puts it back into bcvk's pool, and bcvk boots it
instead of installing. A run that finds no disk (the v1-disk run hasn't
finished, or the cache evicted it) or one bcvk rejects (other install options)
installs v1 itself, ~10 minutes slower. `gh workflow run publish.yml --ref
<branch> -f v1_disk=true` fills the cache for runs on that branch.

`changed` fingerprints the image as built, so a run that changed nothing stops
right after the build (no rechunk, no tests, and the upgrade job stops when it
sees that), unless a manual run forces the tests (`test`), which rechunks too.
The rechunked image (same content, reproducible layers, its own tag
`rechunked`) is what is always tested and published. Rechunking while the
tests ran on the image as built was measured: about 7 minutes faster, but the
`upgrade::` tier then builds its v2 from layers that share nothing with the
published v1 (71 layers, 4.9 GB to stage instead of a few) and the VM boots v1
again after the reboot, so the tests run after the rechunk. The rechunk
streams chunkah's OCI archive straight into `podman load`
(`--log-driver=passthrough`): through conmon, which also logs everything a
container prints, the 4.7 GB stream took 7.5 of its 9.5 minutes.

```sh
cargo xtask changed <ref> --image <tag> # prints changed=/exists=/fingerprint=; $GITHUB_OUTPUT when set
cargo xtask rechunk <tag> [--output <tag>] # reproducible layers + ilex.fingerprint label; in place unless --output
cargo xtask publish <tag> --stage      # CI only: push as unsigned :candidate, print digest=
cargo xtask publish --digest <digest>  # CI only: sign what is staged, move :latest to it
```

- The background parts (`vm-prep.sh`) log every line with the time it was
  printed; the "Wait for" steps print those logs.
- The VM stack's Ubuntu packages (qemu, libvirt, virtiofsd, passt: 62.9 MB)
  come from the Actions cache (`apt-debs-<runner>-<hash of their names>`, the
  newest one restored by prefix): apt downloads only what it lacks. Ubuntu's
  Azure mirror stalls on single requests at times; the same download took 9 s
  in one job and 14 minutes in the other. The build and v1-disk jobs save a
  new entry when apt downloaded something; `autoclean` drops superseded
  versions first.
- The runner is `vars.ILEX_RUNNER` (a repository variable) or `ubuntu-26.04`,
  for every job: moving to a larger, org-owned or self-hosted runner is one
  setting (Settings → Secrets and variables → Actions → Variables). The
  runner's root disk is 145 GB with ~110 GB free, so the workflow doesn't
  clear the preinstalled toolchains first. Each runner boots one VM at a
  time (the default `ILEX_VM_SLOTS` on 16 GiB).
- A manual run from another branch (`gh workflow run publish.yml --ref <branch>
  -f test=true`, and `-f iso=true` for the ISO) builds, stages `:candidate`
  and tests, but never signs, moves `:latest` or releases the ISO.
- `publish` refuses to run unless `CI=true`/`GITHUB_ACTIONS=true`. The one
  override is a registry on this machine, which can never be ghcr:
  `podman run -d -p 5000:5000 docker.io/library/registry:2`, then
  `cargo xtask publish <tag> --allow-local-registry --repository localhost:5000/ilex`
  (needs `cosign` v2, `COSIGN_PRIVATE_KEY` and `COSIGN_PASSWORD`).
- Signatures are cosign **v2** key-pair signatures in the legacy `.sig` tag
  format, which bootc/containers-image verify (keyless and cosign v3's
  default format don't work). Machines check them via
  `rootfs/etc/containers/{policy.json,registries.d/ilex.yaml}` and the public
  key `rootfs/etc/pki/containers/ilex.pub`. Verify a published image with
  `cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true ghcr.io/reidond/ilex:latest`.
- The private key is not in the repo. It was generated on the WSL box into
  `/root/ilex-secrets/` (`cosign.key`, `cosign.password`, `cosign.pub`); keep a
  backup. Losing it means every machine needs a new `ilex.pub` before it can
  upgrade (machines only accept the baked-in key).

### Installer ISO

The installer is a **network installer** published as a GitHub Release: it
carries no ilex image, so one small ISO stays current by itself and fits
GitHub's limit of 2 GiB per release asset (an ISO with the ~5 GB image in it
was 2.6 GiB). Download it, write it to a USB stick and boot it; Anaconda asks
for the disk and the user, then installs the newest `ghcr.io/reidond/ilex:latest`
over a (wired) network connection and the machine updates itself from there:

```
https://github.com/Reidond/ilex/releases/latest/download/ilex-installer.iso
https://github.com/Reidond/ilex/releases/latest/download/ilex-installer.iso.sha256
```

Because it always installs the newest image it doesn't follow every publish.
On every run, `cargo xtask iso --due` decides, in the `installer-due` job (the `iso`
job reads its `iso_due=`): a new ISO is due when there is no `installer-*`
release yet, when `installer/` or `crates/xtask/src/iso.rs` changed since the
latest one (its notes carry an `installer-fingerprint:` line), or when the
latest one is 7 days old (Fedora and Anaconda fixes). A manual run with `iso`
checked (`-f iso=true`) builds one regardless. On `main` the `iso` job creates
the release `installer-YYYYMMDD` (`-N` suffix if one exists that day) with
`ilex-installer.iso`, its `.sha256` and notes, marked latest, using
`GITHUB_TOKEN` (the job has `contents: write`). From any other branch it builds
the ISO and uploads it as the workflow artifact `installer-iso` instead, so it
can be inspected; nothing is released.

```sh
cargo xtask iso                # build; the ISO, its .sha256 and release-notes.md land in <state dir>/iso/
cargo xtask iso --due          # is a new ISO due? (reads the latest installer release; needs gh)
```

- `installer/Containerfile` is `fedora-bootc:45` plus Anaconda; its kickstart
  (`installer/interactive-defaults.ks`) installs `registry:ghcr.io/reidond/ilex:latest`
  (`--source-imgref`; pykickstart requires the transport) and tracks
  `ghcr.io/reidond/ilex:latest` (`--target-imgref`). The installer drops GPU
  firmware, Vulkan drivers and CJK fonts before it builds its initramfs: a stock
  build was 1.98 GiB, 16 MiB under the limit. `xtask iso` fails if the ISO
  reaches 2 GiB.
- The installer base is not cached between runs (an `ilex-installer-base`
  package was tried, and saved ~11 of 22 minutes per build): the weekly rebuild
  exists to pick up Fedora and Anaconda fixes, which a cached base would pin,
  and the build is now rare.
- `image-builder` needs root, the `generic-iso` type needs **image-builder
  ≥ 89** (Fedora 45 `updates-testing` for now; 87 only has the unattended,
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
  container against this host's container storage.
- The installer is branded by `/.buildstamp`, the file Anaconda takes its
  product name, version and "final release" flag from (`Product=ilex`,
  `Version=45`, `IsFinal=True`). Without it the installer calls itself
  "anaconda bluesky" with a red pre-release banner. The Fedora logo in its
  sidebar is Fedora's own artwork; ilex has none yet.
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
- Kickstart's `bootc` command (pykickstart) can't pass
  `--enforce-container-sigpolicy`, so an ISO-installed machine tracks ghcr
  *unverified*: `bootc status` shows `spec.image` as just `{image, transport:
  registry}` with no `signature`, and bootc ignores `policy.json` for it (a
  plain `bootc switch` leaves the same state). The image fixes this itself:
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
  it would do. `/sysroot` is mounted read-only on a running system and bootc
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
   `iso` job asks for `contents: write`; if Settings → Actions → General →
   Workflow permissions is read-only by default, a job-level `permissions:`
   still raises it). The old `ilex-iso` package, if it exists, is unused and can
   be deleted, and so can `ilex-installer-base`.

## Conventions

- **No unit tests.** Every test boots the image (`crates/ilex-e2e`, one
  libtest-mimic binary, tiers `ephemeral::`, `disk::`, `upgrade::`,
  `graphical::`). Anything worth testing in a shipped binary must be
  observable from inside the VM: give it a `--self-check` mode.
- **bootc rules for `rootfs/`** (copied to `/`): defaults go in `/usr`; keep
  `/etc` to files that must live there (it is 3-way merged on upgrade); never
  ship content in `/var` (use tmpfiles.d); create users with sysusers.d.
- `bootc container lint --fatal-warnings` runs last in the build: clean up
  what it flags in the same `RUN` step that created it.
- Shared host-side plumbing (slug, paths, VM slots, git) lives in
  `crates/ilex-devkit`; each `xtask` subcommand is its own module with its
  own `clap::Args`.
- Shell out to the packaged `bcvk` CLI, not the `bcvk-qemu` library.
- Files are LF everywhere (`.gitattributes`); scripts shipped in `rootfs/`
  need the exec bit in git (`git update-index --chmod=+x`).
