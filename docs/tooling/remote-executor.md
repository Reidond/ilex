# Windows remote executor (T3)

Run `cargo xtask build`, `test`, `disk`, `iso`, `tool` and `rechunk` directly in
PowerShell. These commands stage the selected Windows checkout, call the T2
builder's idempotent `up`, and execute Linux tooling there. Native Linux commands
remain local. `vm` and its existing WSL/viewer path are retained for T4/T7.
For that compatibility path, build in WSL first: it uses WSL's image store.
The builder and WSL stores remain separate.

Install a changed builder recipe with `cargo xtask builder update`; a recipe
mismatch fails rather than running a different controller. The update retains
T2's recovery checkpoint until `builder accept-update`. Infrastructure remains
owned by its exact journal UUID, not by this checkout or a name prefix.

```powershell
cargo xtask builder up --memory-gib 16 --cpus 8 --wait-for-memory-seconds 10800
cargo xtask builder update --wait-for-memory-seconds 10800
cargo xtask build
cargo xtask test ephemeral:: disk:: upgrade:: graphical:: tool:: -- --test-threads 1
cargo xtask rechunk
cargo xtask disk --out 'C:\output with spaces\ilex.qcow2'
cargo xtask tool strace
cargo xtask iso --testing-tag latest
cargo xtask remote snapshot
cargo xtask remote export --image <owned-image> --out 'C:\outputs\ilex.oci.tar'
cargo xtask remote self-check
cargo xtask remote self-check --hold-seconds 120 # Ctrl-C cancellation exercise
cargo xtask remote resume                     # reconnect to this checkout's last job
cargo xtask remote cancel --job <UUID>
cargo xtask info --json
```

`iso --due` is a read-only native operation. `iso --check-registry` uses the
builder's registry tools and preserves its exit status, including exit 3 for a
missing publication. The ISO
remains a network installer; `--testing-tag latest` tests the explicitly labelled
published legacy payload. No remote operation publishes an image or release.
`security::`, `homed::` and the native security harness belong to T5. Explicit
native-only filters fail before starting a job. Existing filter unions, `--build`
and trailing harness arguments are carried as typed fields to the Linux harness.

## Source and ownership

The Windows host inventories index entries, HEAD names (including staged
deletions), current working bytes and nonignored untracked inputs. It preserves
index executable bits, actual Linux modes and symlink targets, including Git's
Windows symlink-file representation. LF-attributed text containing CRLF fails
with its path; token exports and binary bytes are retained unchanged. Source
changes between inventory, copying and the second inventory fail the snapshot.

Git internals, root target/state, `.claude` and `.t3` directories are excluded.
Private-named credentials and ignored files in build contexts fail before their
contents are read. Symlink/reparse parents, unsupported reparse points and links
escaping the checkout fail. Submodules and unresolved index conflicts fail.
The transport does not copy the Windows worktree's `.git` pointer, hooks, config,
credentials or parent history. A small Git object pack carries only the original
HEAD commit and its tree; Linux imports it with that original shallow boundary.
Tracked state/cache directories at HEAD are refused before making that pack,
so an excluded directory cannot enter the transfer through Git objects.

Files use SHA-256 chunks of at most 4 MiB. Missing chunks travel in bounded binary
batches; unchanged chunks stay in the builder. Linux rechecks every chunk and
file, creates source directories as 0755 and regular files before links, applies
file modes, verifies the manifest,
and atomically renames a unique `.partial` directory into the snapshot cache.
An existing snapshot must have the same provenance and still match its contents.
It is never rsync-deleted or rewritten beneath a job.

The job's source context explicitly carries the original repository, native
checkout path, owner UUID, slug, revision/dirty flag and source digest. Workspace
discovery in that snapshot returns this identity. It cannot discover the copied
directory as `main`. Image transactions and existing ownership receipts remain
in force. Remote fingerprints cover selected working bytes, modes, links and
deletions. Local Linux fingerprint behavior is retained.

## Protocol and authority

The maintenance SSH key remains pinned by T2. SSH uses no user config, agent,
password authentication, forwarding or remote command string. A finite versioned
stdin protocol selects missing-chunk lookup, chunk upload, job submission,
inspection, artifact fetch and cancellation. Rust command/transfer enums carry
arguments; the Linux worker deserializes the command again. Unknown protocol
versions fail before mutation. Protocol stdout is separate from job stdout and
stderr files. No user value becomes a shell fragment.

Only the explicit `ILEX_IMAGE`, `ILEX_UPGRADE_FROM`, shell/machine selection,
legacy test controls, ISO assertion controls and slot cap are forwarded.
`ILEX_ISO` uploads both the ISO and its adjacent `compositions.json` as input
chunks. `ILEX_ARTIFACTS` and explicit output paths are Windows destinations;
they are not Linux paths. Cargo targets, runtime/state roots, PATH, HOME and
kache paths are selected inside the builder. CI, GitHub, registry, SSH-agent and
production signing credentials are not forwarded.

The maintenance key already has guest-root authority through T2 recipe updates;
the controller is not a sandbox against its holder or the executable source that
holder builds. It grants no Windows shell or reverse host-control service.

## Jobs, capacity and cancellation

The host installation lock fences registration against T2 maintenance. Queued
and running `ilex-job-<UUID>.service` units and a shared guest lifecycle lock make
the builder nonidle. Each checkout has an exclusive job lease. A guest scheduler
reserves memory-weighted slots and CPU across checkout jobs, retaining leases
while their exact systemd units are active and reconciling stale unit leases.
`ILEX_VM_SLOTS` can further reduce the cap. Existing nested VM slots are capped
at one within a job. The builder's fixed memory is charged once on Windows;
nested memory comes out of that allocation, not a second physical allocation.
T2 still requires the allocation plus a 14 GiB host reserve before starting.
Commands wait up to three hours for capacity rather than lowering the reserve.

Ctrl-C after registration sends SIGTERM only to this job's systemd cgroup. The
wrapper collects outputs and exact created Podman/container and libvirt/domain
IDs authorize leftover cleanup. Names alone never authorize removal. Native
QEMU children remain subject to the existing parent-lifetime guard. Cancellation
never calls builder down, stops another Hyper-V VM or broadly kills QEMU/libvirt.
Interrupted source transfer has no submitted job. Loss of SSH leaves the remote
job running; `remote resume --job <UUID>` reconnects, streams remaining log bytes
and retrieves its outputs. A missing/killed service reports an interrupted
failure rather than a passing job.

## Persistent caches and outputs

The builder keeps rootful Podman/libvirt storage on its T2 data disk. Container
builds retain the shared `ilex-kache` mount and optional read-only host-store
import, where the host is now the builder. Linux Cargo targets are separated by
checkout owner, architecture and rustc toolchain identity. Windows target trees
are never imported. No store pruning or wholesale WSL migration occurs.

Jobs freeze returned state outputs with XFS reflinks (a sparse copy fallback)
before releasing the checkout lease. Later jobs can update their caches without
changing an earlier job's exports. ISO/drupe files, certificates, disk metadata,
build request/results, screenshots, console logs and stdout/stderr are hashed
and returned even after command failure. Development private keys stay inside
the builder. Disk outputs are format/size checked with qemu-img and have no
backing file; existing QEMU/bcvk fixtures keep their SSH credential recipe.
Persistent development-disk enrollment and native providers are later layers.

Windows state defaults to `%LOCALAPPDATA%/ilex/checkouts/<owner-qualified slug>`.
The state receipt also fences `main`. It contains:

- `remote/chunks`: the source transfer cache;
- `remote/jobs/<UUID>`: request, endpoint metadata, transfer measurements, streamed
  logs, result and a `delivered.json` map;
- `artifacts`, `iso`, `tools/images`, public certificates and build manifests;
- `exports`: OCI archives requested explicitly with `remote export`;
- `disks/<SHA256>.qcow2|raw`: immutable downloaded disk parents with ownership
  receipts. Explicit `--out` also receives a verified atomic copy.

Downloads are verified into `.partial` files before rename; a matching local
hash avoids another transfer. Space checks retain 30 GiB on each output volume.
`info` reports the original owner, builder UUID/state/root, image reference and
last result without starting the builder. `transfer.json` records source/upload
bytes and staging time; results record operation time and actual tool versions.

See [remote-executor-evidence.md](remote-executor-evidence.md) for measured
validation and outstanding merge gates. No unit-test suite is added.
