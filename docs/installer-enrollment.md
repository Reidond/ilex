# LUKS2 installer enrollment (phase 0 PR 18)

Anaconda still owns partitioning and deployment. Select LUKS2 encryption in
its storage page and keep a separate maintenance administrator. The chooser
handles public composition data only. ESP and `/boot` remain outside LUKS;
TPM enrollment requires single-device Btrfs. Root and `/var` must share one
encrypted ancestor, with no disk swap or crypttab keyfile. A stacked layout,
such as LVM over one verified LUKS2 device, can finish with recovery and no TPM
enrollment. If device discovery fails or the root cannot be verified, retry
after the storage is visible. The helper does not guess a partition.

Btrfs mount major/minor numbers identify anonymous superblocks. Discovery uses
the mounted filesystem's kernel FSID to enumerate every member in sysfs and
checks the set against the kernel's device count. Each encrypted member must
be an active dm-crypt mapping of its verified LUKS2 backing device. A
single-member Btrfs above a linear map spanning encrypted and plaintext
storage is rejected, as is a filesystem with mixed Btrfs members.
The initramfs root-method recorder uses the same discovery path.
The kernel mount ID of an opened directory selects its visible mount, so
Anaconda's stacked bind mounts cannot select a hidden mount or make an
otherwise valid encrypted layout ambiguous.

After Anaconda writes the target configuration, the Flatpak preload runs in
private mount and PID namespaces. Its runtime binds and descendant processes
are gone when the final `%post --nochroot` starts local completion on reserved
**tty8**. Enrollment uses Anaconda's own view of the unlocked target and its
devices, then returns to the GUI on VT6. The helper rejects a graphics-mode VT.
This uses the plan's explicit no-echo re-entry alternative: there is no
Anaconda D-Bus password broker. Re-enter the disk passphrase, save the new
recovery key offline, then privately enter that key to confirm it. Only this
local console displays the key. The terminal and scrollback are cleared;
Anaconda's logs and remote WebUI receive no recovery material. Do not capture
the key screen. `inst.nosave=all_ks` prevents copying input/output kickstarts
to the target. Runtime log/diagnostic serialization remains an exact-ISO
qualification gate, including when supplied private unattended answers are used.

The completion hook retains every existing slot. A key becomes acknowledged
only after you re-enter it and the helper verifies that it unlocks the disk.
After an interruption or a mistyped confirmation, switch to VT8 and run
`ilex-enroll complete --root /mnt/sysroot --tty /dev/tty8`. Use `--root /` on the
installed system. Enter the disk passphrase to see the same pending key again.
The helper keeps a protected receipt on the encrypted volume until you confirm
the key. A credential that opens the volume can resume a current receipt.
Legacy receipts used the initiating passphrase. If an authenticated operator
cannot decrypt one, ilex keeps the encrypted receipt and all existing slots,
and marks them for manual cleanup. Ordinary TPM completion and repair can
proceed using the acknowledged recovery key.

Add `--reissue-recovery` to request a new key or resume an interrupted explicit
reissue. Reissue keeps the previously confirmed key and its acknowledgement
until you confirm the replacement. It retains the disk passphrase and earlier
slots. Completion checks for the slots it still needs before allocating any.
If eligible TPM enrollment is pending, a new recovery key needs two free
keyslot allocations and token IDs. With room for only one enrollment and a
confirmed recovery key, ordinary completion uses the remaining space for TPM.

`crates/ilex-enroll` is the reusable library and CLI. `recovery` accepts owned
unlock/output descriptors, captures cryptenroll output privately and returns
only token/slot ids. Use local completion for recovery reissue. `tpm` accepts
an owned unlock descriptor, authenticates it, checks eligibility and the
target tool versions, and reads the token back.
The native installer can link the same library used by `ilexctl`.

Credential authentication excludes external token handlers and filters out
built-in tokens before testing the supplied descriptor. An already available
TPM/keyring token must not turn an incorrect passphrase into successful
authentication. The guest self-check proves this with a working automatic
keyring token and distinct correct/incorrect credential descriptors.

Eligibility requires SecureBoot=1, SetupMode=0, TPM2, a nonzero SHA-256 PCR 7
matching event-log replay, the qualified systemd 262 format, matching live/target
RPM versions and the target unlock-stack marker. The initial allowlist is
systemd/systemd-udev `262.1-1.fc45.x86_64` and cryptsetup/cryptsetup-libs
`2.8.8-1.fc45.x86_64`; another pair stays passphrase-only until qualified.
Both builds record package versions and SHA-256 hashes of the helper, enrollment
and unlock executables, TPM token plugin, cryptsetup library and TSS ESAPI.
Runtime verifies these files against `/usr/lib/ilex/security/tools.json` before
using its version metadata. This works on the exact ISO, where image-builder
removes the RPM database. A target without metadata uses its RPM database;
missing or inconsistent metadata declines automatic enrollment.
Enrollment uses explicit
`--tpm2-pcrs=7:sha256 --tpm2-with-pin=no` and disables public-key, signature
and pcrlock discovery. Header validation rejects additional PCRs, another bank,
a PIN, signed policies and pcrlock. No `--wipe-slot=all` or production
`headless=true` is used. An existing exact token is reused only after passphrase
authentication and a successful token-only trial. Other TPM policies require
authenticated repair, not implicit replacement.

The public `/etc/ilex/setup.json` retains schema 1.

- `enrolled-unverified` means a TPM token was enrolled. Boot verification is
  still pending.
- `passphrase-only` means TPM enrollment was skipped, for example because
  Secure Boot is off, the TPM is unavailable, or the encrypted layout is
  unsupported. Use the disk passphrase or a confirmed recovery key. If a
  display was interrupted, use the passphrase to resume confirmation before
  relying on the pending key.
- `unavailable` means the inspected plaintext mapper layout has no verified
  LUKS recovery device. First boot refers you to the existing storage recovery
  procedure. No recovery key is generated in this case.

The encrypted target's `/var/lib/ilex/security/enrollment.json` binds slot
identities and acknowledgement to the verified LUKS UUID. A transient
discovery error leaves this record unchanged. Retry after the device reappears.

Every image carries a generic dracut unlock stack and
`rd.luks.options=tpm2-device=auto`. The initramfs wrapper first tries one
validated TPM token using cryptsetup's token-only operation. Creating the
mapping through that operation records `method=tpm2`. Stock systemd handles
fallback prompts; a successful path without direct method evidence records
`unknown`. Before pivot, the matching root UUID record becomes
`/run/ilex/root-unlock.json` (schema 1, UUID, boot ID, method, token/slot).
The installed verifier consumes this record. Repair requires a local operator.

The rebased shell-free build with Flatpak and enrollment measured
2,334,252,793 bytes in Podman's uncompressed `.Size` against 2,014,310,965
bytes for `quay.io/fedora/fedora-bootc:45` at digest
`sha256:a14fce7d40c31ad2f924444c2d7d3424a9f4617c561ea1ef729df9c5e3205455`.
The 319,941,828-byte increase (305.12 MiB) sets the **322 MiB** growth limit.
Rounding up to a whole MiB and adding 16 MiB leaves 16.88 MiB for Fedora
updates. That replaces the apps stack's 256 MiB limit and the earlier 192 MiB
enrollment-only limit. The ISO has a separate 2 GiB limit.

The generic initramfs layer carries cryptsetup fallback, the TPM token plugin,
tpm2-tss/libcrypto, TPM drivers and ilex-enroll alongside Fedora's inherited
archive. The base requests cryptsetup, tpm2-tss, systemd-udev, dracut and kbd.
Flatpak and its dependencies remain in the base for cleanup after a shell-free
switch.

## Verify and repair after boot

`sudo ilexctl security verify` reports the current boot's unlock result as
JSON. The installed service runs it at boot and persists the result under
`/var/lib/ilex/security/enrollment.json`. Verification requires the current
boot ID, the active encrypted root UUID and a matching enrolled token and
keyslot. The initramfs records `tpm2` from a successful token-only activation.
On the qualified Anaconda mapping it can also record `fallback` after the
stock Plymouth or console password agent opens the volume. Unrecognized
options, other token types or missing evidence leave verification pending.
Consumers of the persisted verdict must compare its boot ID with the running
system. Discovery or evidence errors clear an older verified result when the
bound state can be safely updated.

After a fallback boot, run `sudo ilexctl security repair` and switch to
tty8 with Ctrl+Alt+F8. Enter the disk passphrase or saved recovery key at
the hidden prompt. The initramfs credential is discarded after unlock, so
repair asks again in the installed system. Repair defers without a prompt
when Secure Boot is off or TPM/PCR support is unavailable. A successful
repair returns pending. Power off and start the machine, then check that
`ilexctl security verify` reports `verified`.

Completion, explicit recovery reissue and repair pass through the same
authenticated receipt reconciliation. A resumed receipt proves ownership of
its slot by unlocking that slot. An occupied reservation can be replanned
without deleting the occupant or changing the pending key. The allocator
proves space for both enrollments on a protected header copy, including the
actual keyslot areas, token IDs and serialized JSON metadata. When reissue
starts with an acknowledged key and TPM still needs an allocation, ilex
commits the proved TPM enrollment before allocating the optional key.
The TPM helper seals on a disposable LUKS2 header. Before a
write to the real header, ilex commits the planned identifiers and a
protected credential, or the fingerprints of the TPM slot and token it
will retire. The authenticated fallback then identifies an interrupted
allocation on retry, even after a change to the TPM. The disk passphrase
and acknowledged recovery slots survive replacement. A slot already
orphaned by an older unjournaled enrollment needs manual identification
before it can be reclaimed.

Automatic enrollment uses PCR 7 in the SHA-256 bank without a PIN.
Systemd supports TPM+PIN for deployments that want a boot prompt and can
manage PIN lockout and recovery. This ilex workflow qualifies the no-PIN
policy. Keep the disk passphrase and saved recovery key available for
changes to firmware, Secure Boot policy or TPM identity.

## Qualification

Run only from this checkout's namespace. First build/qualify the exact 6 GiB,
two-vCPU profile; WSL nested KVM's smaller passing profiles do not qualify the
installer. Explicit TCG is the local large-guest alternative.

The PR 17 guest probes retain a 60-second KVM deadline and allow 300 seconds
under explicit TCG for RPM queries and signed-module loading. The selected
deadline is retained in `amdgpu-deadline.json`; assertions and boot deadlines
remain mandatory, and timeouts stay unqualified.

```sh
cargo xtask build --shell none
ILEX_SHELLS=none cargo xtask security --self-check --accelerator tcg --memory 6G
ILEX_SHELLS=none cargo xtask test disk::install
ILEX_SECURITY_ACCELERATOR=tcg cargo xtask test security::encrypted_install
ILEX_SECURITY_ACCELERATOR=tcg cargo xtask test security::encrypted_existing_space
ILEX_SECURITY_ACCELERATOR=tcg cargo xtask test security::encrypted_interrupted
```

`security::encrypted_install` builds/reuses `ILEX_ISO`, installs this candidate
from an owned disposable local registry with a throwaway signing authority,
then detaches the ISO and provisioning medium. Only the live test installer
gains that authority, scoped to the test registry; the shipped default reject
and production signature policy stay intact. The installed digest must equal
the signed candidate. This qualifies the produced ISO/candidate integration;
it does not claim that a public GHCR composition or released ISO was tested.

The separate `encrypted_existing_space` trial first creates a public sentinel
partition on its owned blank disk, then installs with `clearpart --none` into
the remaining space. It repeats the encrypted gates and checks the sentinel
after first boot, cold restart and no-TPM fallback. Its VM identity and public
evidence directory are separate from the empty-disk trial. Neither trial
claims in-place conversion of an unencrypted filesystem.

The fixture requires direct TPM evidence on the first installed boot, no
password input, matching/replayed ISO and installed PCR 7, encrypted mount
topology, crypttab/kargs, SELinux enforcing, no failed units, a cold restart
and disposable first-boot PCR-mismatch and actual recovery-key unlocks. The
first-boot negative has Secure Boot disabled and a different present TPM; it
records changed PCR 7 and its raw event log, and retains fallback slots. The
strict positive replay requires Secure Boot authority events, which disabled
Secure Boot omits; the negative does not weaken that qualification contract.
A further negative
changes only the stopped positive fixture's SecureBootEnable variable while
keeping its original TPM and LUKS header. Both require a prompt, passphrase
boot, fallback evidence and unchanged TPM enrollment. A separate no-TPM
passphrase fallback also retries local
completion, records passphrase-only mode and proves that recovery acknowledgement
and all slots/tokens are retained. `ilex-enroll --self-check` in the
booted guest creates one disposable loopback LUKS2 volume and exercises real
inherited-FD authentication, recovery, retained passphrase and wrong-password
rejection. It checks a real undersized keyslot area and a nearly full token
table. The wrong-vTPM trial cuts repair after actual header writes, resumes
with the saved recovery key, and requires unattended verified cold boot. It
rejects a wrong repair input with an unchanged header, authenticates both
retained fallback slots, and checks that stale evidence clears verification.

The guest starts its private virtual keyboard when the shipped completion
process appears. The overall install deadline covers the earlier Flatpak
preload, and the prompt deadline starts with enrollment.
It waits at two key-free screens for an explicit screenshot acknowledgement;
the recovery key is never photographed. The guest encrypts its test-only copy
to an ephemeral RSA-OAEP key whose private half stays in the host's private
fixture. Decryption feeds locked, dump-disabled memory; private console input with
that exact saved key must unlock the installed initramfs on a different TPM.
No plaintext recovery file is exported. Fingerprints of these high-entropy
random fixture credentials let guest, host and CI scan both credentials
without printing credential bytes. Failure reports include scan counters,
the rule, a redacted path or journal region, and the credential kind and offset
for a fingerprint match. They suppress matching content and exception text.
Scans cover live/target kickstart locations, logs, state and
journal exports; an active test-only shutdown unit repeats the live scan after Anaconda
finalisation, and installed/cold/negative guests scan the final target files.
Recursive scans cover the staged and copied export sets and all CI artifact
tiers after all trials, including gzip/xz/bzip2/zstd/ZIP contents. Image files are
excluded with header checks and the screenshot handshake; no OCR is claimed.
Only public metadata, event logs, serial
diagnostics and key-free screenshots enter `artifacts/encrypted-install/`.
Initramfs fallback input uses a private Unix socket connected to the existing
guest UART. A separate reader records guest output only; credential bytes do
not enter QMP arguments or the log writer. This exercises the serial password
agent selected by the fixture's console arguments. Installed-guest retries
and recovery reissue use the same private UART to feed a test-only virtual
keyboard on VT8. The screenshot handshake still protects the recovery screen.
Both credentials pass the final recursive log/export scan.
Evidence is staged privately until the installer privacy gate and recursive
artifact scan both pass. The hosted upload rechecks the scan seal. A failed
run keeps its unqualified result and available redacted diagnostics on the
runner. The final scan detects exposed credential prefixes and suffixes of
eight bytes or more. Short fingerprint receipts stay in runner-private
temporary storage. The
fixture mirrors bounded tails of Anaconda's program, storage, packaging and
kickstart logs over serial. It also captures the preload and enrollment
output, LUKS header metadata and the no-TPM completion unit's journal. The
diagnostic export redacts both generated credential formats, including line
wraps, before taking bounded tails and applying the fingerprint scan. Scan
reports also travel over the installer serial
channel and record guest memory, writeback, swap and pressure counters.
Screen cells and unfiltered exception text stay private.
Disks, firmware/TPM state, private answers and signing keys are removed.

To diagnose installation and fallback locally, point `ILEX_ISO` and
`ILEX_IMAGE` at this checkout's installer and candidate and run
`ILEX_ENROLLMENT_LOCAL_DIAGNOSTICS=1 cargo xtask test security::enrollment_diagnostic_ -- --test-threads 1`.
These trials use a firmware profile with Secure Boot off and no TPM. They
retain an unqualified result and run outside the hosted filter. Use explicit
TCG qualification for a local Secure Boot run with the hosted memory profile.
The encrypted LVM diagnostic also hides the LUKS UUID link in a private mount
namespace, checks that discovery failure is retryable, then exercises legacy
state repair and interruption before recovery confirmation.

The install deadline defaults to three hours for explicit TCG and can be set
with positive `ILEX_ENCRYPTED_INSTALL_TIMEOUT` seconds. Missing qualification,
missing evidence, failure and timeout remain **unqualified**, with a failed
stage. The interrupted trial kills the shipped completion while its first
recovery key is visible, before acknowledgement/TPM enrollment, and waits for
the killed helper to exit. An interrupted installer leaves the target
mounted. Anaconda can detach its target `/var` bind independently of systemd
while the shutdown scan runs. The fixture retains the target scan parents and
orders the shutdown scan before their unmount. Scan roots resolve under those
parents even if a directory appears after the bind. Mount identity checks
cover the retained parents and their submounts before and after the scan.
The scanner also synchronizes journald after Anaconda stops and
while the journal is still available. The trial then boots with
the passphrase and the unacknowledged recovery key, checks the incomplete
state and intact slots, confirms the same key on ordinary retry, then performs
authenticated explicit reissue and boots with the new key. The automatic
storage trial also tests that reissue leaves the last free Btrfs slot for TPM
enrollment. The encrypted LVM diagnostic interrupts the last-slot display,
mistypes the confirmation, then confirms that key and boots with it. These
trials use the shipped enrollment executable and ISO.

The CI `encrypted_install=true` input forces build, rechunk, VM setup and exact
6 GiB firmware qualification even with `test=false` and unchanged inputs. The
build runner supplies the local rechunked image to the ephemeral and security
tiers. The encrypted installer fixture serves that image from its disposable
registry. This mode skips GHCR staging, upgrade and other root builds. Missing
backend/firmware/TPM evidence fails the job under T6's contract;
the cache-only `v1_disk=true` combination fails explicitly. It requires both
exact encrypted storage gates plus the interrupted trial. Public evidence for automatic
storage and existing free space is retained separately; the gate is opt-in
while being qualified. A hosted qualifying run should select branch
`phase0/18-luks-enroll`, `encrypted_install=true`, `security_accelerator=kvm`,
`v1_disk=false`, `test=true`, `iso=false`. The encrypted gate builds its own
exact ISO; `iso=true` additionally runs the independent publication ISO path.
The hosted runner's earlier 4 GiB KVM result does not qualify 6 GiB: this run
must measure the 6 GiB/two-vCPU contract before the encrypted tests. A KVM SMM
failure remains unqualified; explicit TCG is a separately measured alternative.
Physical fTPM, firmware changes and actual kernel-upgrade/rollback behavior
remain later qualification, never inferred from this fixture.

For a failed test-only finalisation scanner, an owned checkpoint taken after
the installer QEMU exited successfully can diagnose the installed boot without
repeating the import. `ILEX_ENROLLMENT_CHECKPOINT=<owned-state-checkpoint>` with
`cargo xtask test security::enrollment_checkpoint` restores the original disk,
TPM and firmware bundle only after the original controller has exited. It
requires the exact qualified 6 GiB/two-vCPU backend, authenticates the private
credential fingerprints, then runs first/cold boot and fallback gates. It
can also exercise passphrase/recovery boot and explicit reissue from an
interrupted completion checkpoint, reported separately from TPM boot proof. It
refuses foreign state and existing fixture paths. The result and exit status
always remain **unqualified** for the complete installer; any direct core boot
proof is reported separately. This opt-in diagnostic is outside CI's
`security::encrypted_` filter and cannot replace the full ISO qualification.
