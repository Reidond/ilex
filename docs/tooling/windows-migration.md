# Windows development migration

Native PowerShell is the entry point for Windows builds, tests and development
VMs. The shared Fedora builder runs Linux jobs. Native QEMU/WHPX runs the
virgl desktop, and native Hyper-V Gen2 runs the security tier. Install the
Windows dev build of `ilexctl` and configure the MCP server to launch `ilexctl mcp`
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
then `~/.cache/kache`. For FedoraLinux-45, run the exporter from native
PowerShell with Windows Python 3.11 or newer. The distro's `setpriv` uses
Landlock to deny filesystem writes by the exporter. SQLite reads an active
WAL through its existing sidecars. A closed index without sidecars uses an
immutable connection and is checked for concurrent changes.

The exporter verifies each blob with `b3sum`. It reports damaged blobs and
omits their dependent entries from the archive and its index. If eviction
removes a referenced file before it can be opened, the export fails and
discards the partial archive. If `b3sum` is absent in the distro, place the
[x86_64 Linux release binary](https://github.com/BLAKE3-team/BLAKE3/releases)
on Windows and pass its path with `--b3sum-linux`.
Set `--distribution` and `--user` to select another source. Keep the archive
and its digest receipt in private Windows staging outside the checkout.

```powershell
python docs/tooling/export-kache.py --output 'C:\staging\kache.tar'
ilexctl migrate kache 'C:\staging\kache.tar' --version 2.0.0
$env:ILEX_KACHE_IMPORT = '<sha256:… from the import receipt>'
ilexctl build -- --no-cache
Remove-Item Env:ILEX_KACHE_IMPORT
```

An external export can also be imported as an uncompressed tar with
`index.db` and `store/` at its root. Export from a quiescent store or use a
consistent snapshot. Keep runtime sockets and credentials outside the archive.

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
