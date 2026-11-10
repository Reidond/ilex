# ilex

Personal Fedora bootc image: one `Containerfile` on `quay.io/fedora/fedora-bootc:45`
(GNOME desktop), a Rust workspace for all tooling, e2e-only tests on QEMU via bcvk.

## Build and test

```sh
cargo xtask build              # podman build → localhost/ilex:<slug>, runs bootc container lint
cargo xtask test ephemeral::   # build if needed, boot in QEMU, assert over SSH
cargo xtask vm                 # interactive ephemeral VM of this checkout's image
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

## Driving VMs with ilex-vmctl

`cargo xtask disk` writes the installed qcow2 (`bcvk to-disk`); `cargo xtask
vm --disk` boots it headless on the graphical test machine (UEFI, 4K
`virtio-vga`), and `--gui` adds a GTK window for a human (WSLg shows it on the
Windows desktop). Every disk-booted VM (also the `disk::` and `graphical::`
tests) publishes `qmp.sock`, `vnc.sock` and `vm.toml` in
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
03:00 UTC and on demand: build → `changed` → `rechunk` → e2e tests →
`publish` (push by digest, tag `:latest`, `:YYYYMMDD`, `:sha-<short>`, sign).
`changed` fingerprints the image as built, so a run that changed nothing stops
right after the build (no virt stack, no rechunk, no tests). The rechunked
image (same content, reproducible layers, its own tag `rechunked`) is what is
tested and published. Rechunking while the tests ran on the image as built was
measured: about 7 minutes faster, but the `upgrade::` tier then builds its v2
from layers that share nothing with the published v1 (71 layers, 4.9 GB to
stage instead of a few) and the VM boots v1 again after the reboot, so the
tests run after the rechunk.

```sh
cargo xtask changed <ref> --image <tag> # prints changed=/exists=/fingerprint=; $GITHUB_OUTPUT when set
cargo xtask rechunk <tag> [--output <tag>] # reproducible layers + ilex.fingerprint label; in place unless --output
cargo xtask publish <tag>              # CI only
```

- The runner is `vars.ILEX_RUNNER` (a repository variable) or `ubuntu-26.04`,
  for both jobs: moving to a larger, org-owned or self-hosted runner is one
  setting (Settings → Secrets and variables → Actions → Variables). The
  runner's root disk is 145 GB with ~110 GB free, so the workflow doesn't
  clear the preinstalled toolchains first. CI runs two VMs at a time
  (`ILEX_VM_SLOTS=2`: the default of one per 8 GiB would be one on 16 GiB).
- A manual run from another branch (`gh workflow run publish.yml --ref <branch>
  -f test=true`, and `-f iso=true` for the ISO) builds and tests but never
  publishes or pushes the ISO.
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
After each run of the publish job, `cargo xtask iso --due` decides (the `iso`
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
  `--enforce-container-sigpolicy`, and `bootc status` of an ISO-installed machine
  shows `spec.image` as just `{image, transport: registry}`, without a
  `signature` entry: it tracks ghcr but is not pinned to signed images. Run
  `sudo bootc switch --enforce-container-sigpolicy ghcr.io/reidond/ilex:latest`
  once after installing (or have the image do it on first boot).

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
