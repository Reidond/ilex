# Portable Windows runtime and Fedora builder

T2 adds infrastructure commands. Existing image build/test commands keep their
current routing until T3 supplies snapshot transfer and the remote executor.
Use native PowerShell; neither command calls WSL. Linux hosts keep local tooling.

```powershell
cargo xtask runtime fetch
cargo xtask runtime status
cargo xtask runtime self-check
cargo xtask runtime update
cargo xtask builder up
cargo xtask builder status --json
cargo xtask builder self-check
cargo xtask builder down
cargo xtask builder update
```

The host needs enabled Hyper-V/WHPX, the Hyper-V PowerShell module, an authorized
Hyper-V token, and inbox OpenSSH/curl. Missing prerequisites produce errors; the
tool does not enable features, elevate, install system packages, change PATH,
create a switch, or reboot Windows. This owner's existing token is authorized.

## Acquisition and trust

`assets/runtime-lock.json` records all 158 measured MSYS2 UCRT64 archives:
name, version, official HTTPS URL, byte length and SHA256. The untrimmed closure
includes QEMU 11.1.3-2, ANGLE, SDL, virglrenderer, firmware, DLLs and licenses.
Archive hashes originate in the measured repository metadata; this does not
claim independent MSYS2 OpenPGP verification.

Archives are downloaded under `%LOCALAPPDATA%/ilex/runtimes/archives`. A damaged
cache entry fails closed; remove that exact archive before retrying. Downloads
use `.download`, check length and SHA256, then rename. No unpinned fallback is
attempted. Extraction occurs in a new `.partial` directory. Absolute paths,
parent components, NTFS streams, Windows device names, special entries and
escaping links fail. Safe hard links become independent regular-file copies;
safe non-runtime documentation/development symlinks are omitted and executable
symlinks fail. Colliding regular files must have identical
bytes. Package metadata and licenses are retained.

The installation directory includes the lock hash. Both real executables must
load before `ready.json` is flushed and the directory is renamed. An OS file
lock serializes installation and is released after a crash. The next fetch
cleans unpublished extraction and reuses checksum-verified archives. Each child
uses a private DLL/PATH search; the parent environment is untouched.

`runtime update` uses only the lock shipped in this checkout. It boots a small
WHPX/q35/virtio-vga-gl/SDL GL firmware fixture and quits it through QMP before
atomically replacing `active.json`; `previous.json` retains the former pointer.
Old directories are retained for running processes and rollback. This is a host
GL initialization check; actual ilex desktop rendering/input qualification is T4.
There is no automatic old-version eviction or online version discovery.

The builder image is Fedora Cloud Base Generic 45-1.4 x86_64, SHA256
`50bdc5e70f6a24f540932d15411c1c6ae9fcfa40fb990d876d83c5cc823b2ad0`.
The committed signed CHECKSUM is verified in process against Fedora 45 key
`4F50A6114CD5C6976A7F1179655A4B02F577861E`, published on
<https://fedoraproject.org/en/security/>. No GPG installation or trust-store
modification is needed. The image is verified before qemu-img reads it.

## Identity, storage and resources

There is one persistent builder per Windows user/repository installation, named
`ilex-builder-<sid-hash>`. Its state lives in `%LOCALAPPDATA%/ilex/builder` with
current-user/SYSTEM ACLs. The journal records schema, SID-derived owner, random
creation run, exact VM UUID, matching Hyper-V Notes and a core `OwnedResource`
record. Every mutation resolves the UUID and verifies those fields. A matching
name without a matching journal is never adopted or removed.

Default resources are eight vCPUs and fixed 16 GiB RAM, with nested virtualization,
Gen2 Secure Boot using MicrosoftUEFICertificateAuthority, a local key protector,
vTPM, and automatic checkpoints disabled. OS/data are expanding 40/240 GiB VHDX.
The converter's sparse flag is explicitly cleared and sparse/compressed/encrypted
attributes are rejected. A new XFS data filesystem holds containers, libvirt,
snapshots, Cargo targets, kache, jobs and artifacts. Its UUID is recorded.

Before downloads/conversion/update, check free space including a 30 GiB reserve.
Before starting, require fixed RAM plus 14 GiB available for Windows/apps
(12 GiB reserve plus 2 GiB margin). Current VM consumption is already reflected
in live available memory. Nested jobs consume the builder's allocation, not
another host allocation. T3 adds shared weighted guest/job scheduling.

For a constrained validation run:

```powershell
cargo xtask builder up --memory-gib 4 --cpus 4 --wait-for-memory-seconds 10800
```

The wait polls live available memory every two minutes without touching other
VMs. The minimum is 4 GiB. Explicit resource changes require the builder to be
off and no retained checkpoint; `builder up --memory-gib 16` restores the default
memory profile. Status is read-only, including when no installation exists.

## Provisioning and SSH

IMAPI generates a NoCloud seed containing a disposable bootstrap key, a generated
known SSH host key, and a separate restricted maintenance key. Cloud-init creates
dedicated users and provisions the guest automatically. The host pins the supplied
host key before its first connection, with strict checking, no user SSH config,
no global known-hosts file, no agent forwarding and no password authentication.
Provisioning identity must also match the SID-derived owner and recipe revision.
After successful provisioning and nested smoke, detach/delete the seed and host
private key/bootstrap key. The guest revokes the bootstrap authorized key.

Address discovery reads KVP addresses from this VM's single Default Switch NIC,
then exact-MAC neighbors on the matching switch interface. It retries for up to
15 minutes, with bounded SSH connects. No subnet scanning, assumed subnet or
manual static-IP step. Failure names UUID, MAC, switch, serial pipe and log paths.
Addresses are rediscovered after every start; previous IPs are not trusted.

The maintenance key is restricted to `/usr/local/libexec/ilex-builder-control`.
That user has sudo permission only for this argument-free controller. A bounded
stdin operation selects `probe`, `idle`, `smoke` or `update`; arbitrary SSH shell
commands are unavailable. Update receives a versioned recipe frame and checks
its SHA256 before installing fixed script paths. T3 extends the finite protocol
with job operations and must hold `/run/lock/ilex-builder-jobs.lock` for active
jobs and use `ilex-job-*.service` units. Maintenance refuses active jobs.

The recipe installs rootful Podman, Rust/Cargo, the packaged bcvk CLI v0.22.0
(SHA256 checked), QEMU/KVM, libvirt, OVMF, swtpm, Hyper-V KVP and ISO dependencies.
It requires image-builder >=89 and uses Fedora updates-testing for that package
when necessary. RPM signature checks remain enabled. A recipe digest,
provisioning owner, data UUID, probe and actual RPM versions are recorded under
`/var/lib/ilex-builder`; moving DNF repositories are not byte-reproducible.

## Lifecycle, recovery and acceptance

`builder self-check` verifies fixed-memory/nested/checkpoint settings and pinned
SSH identity, then boots the installed Fedora kernel with an isolated initramfs
in a nested KVM VM. A marker from PID 1 and guest poweroff prove real userspace
boot. It also probes rootful Podman, bcvk and KVM API 12. The disposable nested
fixture is removed, with no registry or image build. Build/test execution is T3.

`builder down` requires idle, requests integration-service shutdown, and retains
the VM, disks, host key and vTPM. `up` restarts the same UUID and authenticates it.
`cleanup-incomplete` explicitly removes only incomplete journal-owned bootstrap
resources after checking exact UUID/Notes and all filesystem paths. It refuses a
ready builder. If interruption occurred in the tiny gap between Hyper-V returning
the UUID and recording it, an unjournaled VM fails closed for manual inspection;
the tool cannot safely authorize name-only deletion.

Updates stop the idle builder, create an explicit powered-off Standard checkpoint
covering VM/OS/data/vTPM, start, update, reboot and run the nested smoke. On failure,
restore that exact checkpoint and verify boot again. `builder update
--rollback-probe` deliberately fails after a real package update to exercise this
path. Interrupted updates retain `updating` and require explicit `builder rollback`.
The recovery checkpoint remains until `builder accept-update` explicitly removes
it. No data disk is attached to a second writable VM, and no prune is performed.

Bootstrap/update logs and the last SSH diagnostic are retained in the builder
state directory. `builder status --json` exposes provisioning/update phase,
recovery UUID, owned paths and resource settings. A ready builder is intentionally
persistent shared infrastructure; ordinary command cleanup does not destroy it.

See `windows-builder-evidence.md` for the exact validation run and outstanding
acceptance gates. No unit-test suite is introduced.
