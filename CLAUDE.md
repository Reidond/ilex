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
