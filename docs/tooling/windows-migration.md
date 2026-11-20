# Windows development migration

Native PowerShell is the entry point for Windows builds, tests and development
VMs. The shared Fedora builder runs Linux jobs. Native QEMU/WHPX runs the
virgl desktop, and native Hyper-V Gen2 runs the security tier. Install the
Windows `ilex-vmctl` and configure the MCP server to launch `ilex-vmctl mcp`
with the intended checkout as its working directory.

## Inventory the old environment

From an existing session in the retiring distro, run
`bash /path/to/migration-inventory.sh /path/to/checkout …` once per relevant
Linux user, including the account that ran the old tooling. It reports
sessions and process names, checkout changes and the locations of known state
and caches. Keep reports private. Match the listed stores against the owners'
Podman image and volume inventories, persistent VM disks, installer outputs,
drupe images and development credentials. Review other projects, external
MCP registrations and launchers with their owners. The script only inspects
metadata and the explicitly supplied checkouts.

Keep dirty and untracked source changes and unique artifacts first. Rebuild
ordinary images in the builder. For unique images, record ownership, tags and
digests before the owner exports selected images with `podman save`. Check
destination tags before restoring. Back up volumes with their application's
consistent backup procedure. Keep persistent VM disks with their ownership
and firmware metadata. Rebuild bcvk caches unless their xattrs and installation
identity can be preserved. Cargo registry and Git caches are optional. Target
directories are disposable after any unique outputs have been recovered.

## Import a Linux kache store

Determine the source store from `KACHE_CACHE_DIR`, then `XDG_CACHE_HOME/kache`,
then `~/.cache/kache`. Confirm x86_64 Linux and the kache version in
`Containerfile`. At a coordinated quiet time, use an already quiescent store
or make a consistent export after its writers have stopped. Archive the
complete store, including the index and its companion files and blobs. Use an
uncompressed tar with these entries at its root. Keep the archive outside the
checkout and on a Windows-accessible staging volume. Exclude non-store data,
runtime sockets and credentials. Keep only regular files and directories.

```sh
tar --hard-dereference -C /path/to/quiescent/store -cf /path/to/staging/kache.tar .
```

```powershell
cargo xtask migrate kache 'C:\staging\kache.tar' --version 2.0.0
$env:ILEX_KACHE_IMPORT = '<sha256:… from the import receipt>'
cargo xtask build -- --no-cache
Remove-Item Env:ILEX_KACHE_IMPORT
```

Imports use the pinned builder transport and verify the archive digest. The
builder keeps each complete import by checkout owner and archive hash. Reusing
the same archive returns the same receipt. The importer checkpoints and checks
the builder's copy of the index so kache can open it on a read-only mount.
Podman labels the builder's copy for SELinux when mounting it.
A build with `ILEX_KACHE_IMPORT`
mounts that import read-only as `/kache-host`. The ordinary `ilex-kache` layer
cache still receives new results. Inspect the build's `kache stats --last-build`
and retain the import receipt and the build log. Compare against a fresh
compiler cache with `--no-host-kache` when attributing hits to the import. A
cached Podman layer can skip compilation entirely.

## Owner retirement checklist

- Pass Windows builder builds, tests, disk and ISO delivery, native desktop
  screenshot and clipboard checks, native MCP, and the Linux CI regression.
- Verify unique data migration and cache hits. Confirm ownership of any
  unresolved consumers. Replace external launchers and MCP registrations.
- Privately verify the existing signing backup and GitHub Actions secrets.
  Keep signing material outside the builder, checkout and evidence.
- At a quiet time, the owner may terminate only `FedoraLinux-45`. If keeping a
  full distro export, use private storage and verify its checksum and recovery.
- After the chosen rollback window and backup verification, the owner may
  unregister only `FedoraLinux-45` and verify the remaining distro list.
