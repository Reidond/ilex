# LUKS2 installer enrollment (phase 0 PR 18)

Anaconda still owns partitioning and deployment. Select LUKS2 encryption in
its storage page and keep a separate maintenance administrator. The chooser
handles public composition data only. ESP and `/boot` remain outside LUKS;
root must use single-device Btrfs, and root and `/var` must share one encrypted
ancestor, with no disk swap or crypttab keyfile. Unsupported or ambiguous encrypted layouts stop
completion instead of guessing a partition.

Btrfs mount major/minor numbers identify anonymous superblocks, not block
devices. Discovery uses the evaluated backing source without its subvolume
suffix and checks the kernel's Btrfs device count before walking block ancestry.
The initramfs root-method recorder uses the same discovery path.
The kernel mount ID of an opened directory selects its visible mount, so
Anaconda's stacked bind mounts cannot select a hidden mount or make an
otherwise valid encrypted layout ambiguous.

The final `%post --nochroot` starts local completion on reserved **tty8**,
then returns to Anaconda's GUI on VT6. The helper rejects a graphics-mode VT.
This uses the plan's explicit no-echo re-entry alternative: there is no
Anaconda D-Bus password broker. Re-enter the disk passphrase, save the new
recovery key offline, then privately enter that key to confirm it. Only this
local console displays the key. The terminal and scrollback are cleared;
Anaconda's logs and remote WebUI receive no recovery material. Do not capture
the key screen. `inst.nosave=all_ks` prevents copying input/output kickstarts
to the target. Runtime log/diagnostic serialization remains an exact-ISO
qualification gate, including when supplied private unattended answers are used.

The completion hook retains every existing slot. An interrupted recovery
display is recorded as incomplete; normal retries refuse to generate another
key. An administrator can explicitly rerun `ilex-enroll complete --root
/mnt/sysroot --tty /dev/tty8 --reissue-recovery` after switching to VT8.
This adds a replacement before any possible later retirement of old slots;
PR 18 never removes slots. The original passphrase always remains usable.

`crates/ilex-enroll` is the reusable library and CLI. `recovery` accepts owned
unlock/output descriptors, captures cryptenroll output privately and returns
only token/slot ids. `tpm` accepts an owned unlock descriptor, authenticates
it, checks eligibility and the target tool versions, and reads the token back.
The future native installer can link the library; PR 19 can use this CLI.

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

No TPM, Secure Boot off, unmeasured PCR or version/stack mismatch leaves
passphrase-only encryption and a nonsecret reason. `/etc/ilex/setup.json`
retains schema 1 and reports `enrolled-unverified` or `passphrase-only`.
Runtime state under encrypted `/var/lib/ilex/security/enrollment.json` stores
UUID, slot identities, acknowledgement, tool versions and probe results.
`pending` is not proof that a boot used TPM.

Every image carries a generic dracut unlock stack and
`rd.luks.options=tpm2-device=auto`. The initramfs wrapper first tries one
validated TPM token using cryptsetup's token-only operation. Creating the
mapping through that operation records `method=tpm2`. Stock systemd handles
fallback prompts; a successful path without direct method evidence records
`unknown`. Before pivot, the matching root UUID record becomes
`/run/ilex/root-unlock.json` (schema 1, UUID, boot ID, method, token/slot).
PR 19 owns later verification and fallback repair. No boot service in PR 18
automatically reseals or generates recovery material.

The base growth gate increases **128 → 192 MiB** against this stack's base:
64 MiB more allowance (50%). The 160 MiB gate was an intermediate PR 18 change.
After rebasing onto
main's homed/ilexctl base, the bare image measured 2090.87 MiB versus Fedora's
1917.62 MiB, growth 173.26 MiB. The earlier pre-rebase image grew 151.84 MiB.
The measured growth exceeds the original 128 MiB allowance by 45.26 MiB.
Layer accounting for this checkout's bare build is approximately:

| Layer group | MiB |
| --- | ---: |
| Rebuilt generic dracut crypt/TPM initramfs and tool manifest | 110.92 |
| Complete base package transaction, including existing main support | 37.52 |
| Configuration, including SELinux/homed | 20.69 |
| Copied helpers, generated files and build plans | 4.14 |
| Total growth over Fedora bootc | 173.26 |

The replacement initramfs includes cryptsetup fallback, the TPM token plugin,
tpm2-tss/libcrypto, TPM drivers and ilex-enroll, over the inherited Fedora
archive. This is layer accounting, not a claim of 173 MiB of new RPMs. The
explicit security requests are cryptsetup, tpm2-tss, systemd-udev, dracut and
kbd; some were already present, including systemd-udev through homed.
Security and homed support belong in the bare base. The combined footprint
requires the bounded increase; the
192 MiB limit stays enforced, and the ISO keeps its separate 2 GiB limit.

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
boot, unknown unlock method and no automatic reseal. A separate no-TPM
passphrase fallback also retries local
completion, records passphrase-only mode and proves that recovery acknowledgement
and all slots/tokens are retained. `ilex-enroll --self-check` in the
booted guest creates one disposable loopback LUKS2 volume and exercises real
inherited-FD authentication, recovery, retained passphrase and wrong-password
rejection. There are no unit tests.

The guest's private virtual keyboard drives the normal local completion UI.
It waits at two key-free screens for an explicit screenshot acknowledgement;
the recovery key is never photographed. The guest encrypts its test-only copy
to an ephemeral RSA-OAEP key whose private half stays in the host's private
fixture. Decryption feeds locked, dump-disabled memory; private console input with
that exact saved key must unlock the installed initramfs on a different TPM.
No plaintext recovery file is exported. Fingerprints of these high-entropy
random fixture credentials let guest, host and CI scan both credentials
without printing matches. Matching paths, lines and exception text are
suppressed. Scans cover live/target kickstart locations, logs, state and
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
agent selected by the fixture's console arguments. Local tty8 completion keeps
its private keyboard and screenshot handshake. Both credentials still pass the
final recursive log/export scan.
Evidence is staged privately until the installer privacy gate and recursive
artifact scan both pass. A failed run publishes its unqualified result and,
when available, strictly validated fixed stage/error codes and booleans.
Raw failure logs, screen cells and exception text remain private.
Disks, firmware/TPM state, private answers and signing keys are removed.

The install deadline defaults to three hours for explicit TCG and can be set
with positive `ILEX_ENCRYPTED_INSTALL_TIMEOUT` seconds. Missing qualification,
missing evidence, failure and timeout remain **unqualified**, with a failed
stage. The interrupted trial kills the shipped completion while its first
recovery key is visible, before acknowledgement/TPM enrollment. It boots with
the passphrase and the unacknowledged recovery key, checks the incomplete
state and intact slots, verifies an ordinary retry refuses implicit reissue,
then performs authenticated explicit reissue and boots with the new key. No
test helper replaces the enrollment executable or the ISO.

The CI `encrypted_install=true` input forces build, rechunk, staging, VM setup
and exact 6 GiB firmware qualification even with `test=false` and unchanged
inputs. Missing backend/firmware/TPM evidence fails the job under T6's contract;
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
