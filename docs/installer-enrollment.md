# LUKS2 installer enrollment (phase 0 PR 18)

Anaconda still owns partitioning and deployment. Select LUKS2 encryption in
its storage page and keep a separate maintenance administrator. The chooser
handles public composition data only. ESP and `/boot` remain outside LUKS;
root and `/var` must have the same single encrypted ancestor, with no disk
swap or crypttab keyfile. Unsupported or ambiguous encrypted layouts stop
completion instead of guessing a partition.

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
The later `ilexctl` rename is outside this change.

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

The base growth gate is 160 MiB (previously 128 MiB). Against Fedora base
`81ada60d…`, the first build measured 2069.31 MiB versus 1917.62 MiB, growth
151.69 MiB: the rebuilt generic initramfs contributes a new approximately
111 MiB layer over the inherited Fedora archive. The required crypto RPMs
were already installed. Security support belongs in the bare base; it cannot
move to a desktop overlay. The 160 MiB limit stays enforced, and the ISO
keeps its separate 2 GiB limit.

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
```

`security::encrypted_install` builds/reuses `ILEX_ISO`, installs this candidate
from an owned disposable local registry with a throwaway signing authority,
then detaches the ISO and provisioning medium. Only the live test installer
gains that authority, scoped to the test registry; the shipped default reject
and production signature policy stay intact. The installed digest must equal
the signed candidate. This qualifies the produced ISO/candidate integration;
it does not claim that a public GHCR composition or released ISO was tested.

The fixture requires direct TPM evidence on the first installed boot, no
password input, matching/replayed ISO and installed PCR 7, encrypted mount
topology, crypttab/kargs, SELinux enforcing, no failed units, a cold restart
and a separate no-TPM passphrase fallback. That guest also retries local
completion, records passphrase-only mode and proves that recovery acknowledgement
and all slots/tokens are retained. `ilex-enroll --self-check` in the
booted guest creates one disposable loopback LUKS2 volume and exercises real
inherited-FD authentication, recovery, retained passphrase and wrong-password
rejection. There are no unit tests.

The guest's private virtual keyboard drives the normal local completion UI.
It waits at two key-free screens for an explicit screenshot acknowledgement;
the recovery key is never photographed or returned to the host. Both test
secret values are scanned against installer/target logs, journal and public
evidence without printing matches. Only public metadata, event logs, serial
diagnostics and key-free screenshots enter `artifacts/encrypted-install/`.
Disks, firmware/TPM state, private answers and signing keys are removed.

The install deadline defaults to three hours for explicit TCG and can be set
with positive `ILEX_ENCRYPTED_INSTALL_TIMEOUT` seconds. Missing qualification,
missing evidence, failure and timeout remain **unqualified**, with a failed
stage. The CI `encrypted_install=true` input selects the 6 GiB firmware profile
and runs the exact encrypted gate; it is opt-in while being qualified.
Physical fTPM, firmware changes and actual kernel-upgrade/rollback behavior
remain later qualification, never inferred from this fixture.
