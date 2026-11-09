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
- Publishing (`xtask publish`, `xtask iso`) only happens in CI. Never push to
  ghcr from a dev machine.

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
tests) publishes `qmp.sock`, `vnc.sock` and `vm.info` in
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
