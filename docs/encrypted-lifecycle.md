# Encrypted upgrade and rollback qualification

Qualify one exact installer ISO, a supported rollback image (v1), and a distinct
update image (v2). Keep the same installed disk,
firmware identity, and TPM. Fedora shim, Secure Boot, LUKS2 root, automatic
SHA-256 PCR 7 unlock without a PIN, and a homed LUKS/btrfs home are required.
The normal comparison expects equivalent PCR 7 measurements on each backend.
A genuine firmware-policy change requires its own recorded recovery and
authenticated repair result.

## Linux QEMU and CI

Use one VM slot, 6 GiB, and two vCPUs. Use the checkout's own images and state.
The fixture needs the OpenSSL CLI, cosign v2 and the ext4 tools.
Pull a supported baseline by digest beforehand, or supply an explicitly built
local baseline. Both images need the encrypted boot and homed implementation.
Check the host's memory and active guests before starting a test. In WSL also
check Windows free physical memory and drop the distro's page cache after the
guest is off.

```sh
free -h
export ILEX_VM_SLOTS=1
export ILEX_ISO=/absolute/path/to/ilex-installer.iso
export ILEX_IMAGE=localhost/ilex:checkout-candidate
export ILEX_ENCRYPTED_UPGRADE_FROM='ghcr.io/reidond/ilex@sha256:<v1-digest>'
podman pull "$ILEX_ENCRYPTED_UPGRADE_FROM"
# Requires a positive KVM SMM probe and successful guest qualification.
cargo ilexctl doctor security
cargo ilexctl doctor security --self-check --accelerator kvm --memory 6G
cargo ilexctl doctor security --require-qualified --accelerator kvm --memory 6G
cargo ilexctl test security::upgrade_rollback -- --test-threads 1
```

`encrypted-upgrade-rollback/result.json` must say `passed`. Inspect
`iso-sha256.txt`, the manifest identities, the kernel and initramfs
hashes on the actual boot entries, and PCR readings and replay at baseline,
upgrade and rollback. Boot evidence must match each image's payloads, including
when a kernel or initramfs is shared by both deployments. The
`payload-transition.json` receipt records which fields changed. The fixture
requires a rejection for the unsigned update
and stages the same manifest after applying a disposable test signature. It
preserves the production branch of the installed verification policy.
A separate recovery boot at each deployment changes Secure Boot on the
disposable VM, enters the original recovery key and checks the boot ID, root
UUID, unlock method, header metadata, home checksum, selected profile and
an authenticated user edit to a profile-managed file. It restores firmware
policy and requires another unattended unlock before advancing.

The `publish` workflow's `encrypted_install=true` input includes the lifecycle
and the earlier installer cases. Its default v1 comes from the published
installer composition captured before promotion. For an ISO-only refresh it
uses the previous distinct signed digest from retained composition history.
The workflow verifies the pin with the production key and records its source
in `security/encrypted-baseline.json`. If no predecessor exists it records the
reason and still qualifies a fresh encrypted install. Optional overrides are
`encrypted_upgrade_from` and then `ILEX_ENCRYPTED_UPGRADE_FROM`; both take a
signed `ghcr.io/reidond/ilex@sha256:…` reference.

A CI dispatch can request a distinct Fedora RPM kernel via
`encrypted_update_kernel`. This disposable update fixture requires a changed
kernel release, kernel hash and regenerated initramfs hash. The release job
always tests a fresh encrypted install from the verified production digest
under the shipped signature policy. When a baseline exists, the job also tests
the lifecycle through that unmodified published target. It seals the required
results before releasing the ISO. The ordinary network install test also checks
the production registry and composition. The lifecycle imports v1 and v2 into
an isolated registry with a throwaway verification key. Its manifest digests
identify those delivered test images. The release scope is the default
installer composition.

For a focused retry, dispatch with both `encrypted_install=true` and
`encrypted_lifecycle_only=true`. It still installs the exact ISO and seals
the firmware and lifecycle evidence. The ephemeral suite and other encrypted
installer cases retain their separate verdicts in the full workflow.

The hosted runner must pass the firmware and 6 GiB lifecycle budget with KVM
plus SMM. If it cannot, set `ILEX_RUNNER` to a dedicated Linux runner
with secure OVMF, swtpm, and enough RAM, storage and runtime for the complete
suite. A failed probe, timeout or failed lifecycle is unqualified. A fresh-only
result qualifies that install and records why no lifecycle could run. Retain
the run URL and sealed evidence for a passing run.

## Windows Hyper-V

Use the native [Gen2 provider](tooling/windows-gen2.md) with
`MicrosoftUEFICertificateAuthority` and its existing Default Switch. Run
the firmware self-check before the operator-assisted lifecycle. Inspect
Windows free memory before both runs and check WSL with `free -h`.
Allow at least 30 GiB Windows free before work that starts the Hyper-V builder.

```powershell
Get-CimInstance Win32_OperatingSystem | Select-Object FreePhysicalMemory
wsl -d FedoraLinux-45 -u root -e free -h
$env:ILEX_VM_SLOTS = '1'
$env:ILEX_ISO = 'C:\fixtures\ilex-installer.iso'
$env:ILEX_ISO_SHA256 = '<SHA-256 from the matching asset>'
cargo ilexctl doctor security --self-check --memory 6G
cargo ilexctl doctor security --require-qualified --memory 6G
cargo ilexctl doctor security --encrypted-console --memory 6G
```

Open the printed VM in VMConnect. For automation, use the owning checkout's
`cargo ilexctl vm` commands and the configured TryCua session. After Anaconda reports successful
completion, power off. If the interface only offers reboot, use
`sudo systemctl poweroff` from the installer console. The launcher ejects the ISO and starts the installed disk
with the original disk, VM ID and vTPM. Use guest reboots for the remaining
sequence below. Save the public measurements outside the guest before the
final shutdown, which destroys the disposable fixture. Do not take enrollment
screenshots. Record the VM ID, ISO hash, profile and firmware evidence.

## Owner checklist for the physical MS-7C35

- [ ] Record motherboard name, BIOS version/date and vendor-reported AGESA,
  fTPM firmware version, Secure Boot mode and signing authority. Use the
  intended MS-7C35 composition and the exact ISO checksum. Qualify on a
  sacrificial disk with unrelated disks disconnected. Back up data and verify
  offline access to the machine's existing disk recovery material first.
- [ ] Resolve the installer's selected published composition to the expected
  v1 digest before installation, and confirm that digest on first disk boot.
  Keep a separately signed v2 digest with a different kernel ready for the
  controlled switch. The installer always follows its selected tag, so
  record and reject tag movement before installation.
- [ ] Verify the ISO with `sha256sum --check ilex-installer.iso.sha256`.
  Boot it through the UEFI entry. From the live console, save the result
  of `sudo ilex-enroll probe --json` and the binary TPM event log. Record
  the installer and installed shim/GRUB package versions and the shim SBAT and
  signer. Where available use `objdump -s -j .sbat shimx64.efi` and
  `pesign --show-signature --in shimx64.efi` on the actual EFI binary.
- [ ] In Anaconda select encrypted automatic storage on the sacrificial disk,
  create a separate local maintenance administrator, and select the intended
  home profile. Re-enter the disk passphrase during completion and verify the
  distinct recovery key stored offline. Keep both offline. Disable capture
  and recording while handling credentials.
- [ ] On first disk boot, wait for the OS without entering a disk credential.
  Verify baseline digest, token policy, root unlock evidence and PCR. Log in
  as the homed user using GDM and create the data witness below. Log out
  and confirm the home becomes inactive. Repeat the home login and checksum
  after each recovery, upgrade, and rollback boot.
- [ ] Back up the LUKS header to owner-controlled encrypted offline media.
  Keep the retained v1 deployment and the verified recovery ISO.
- [ ] Test recovery on v1. Restore normal boot and confirm TPM unlock. Stage
  the signed v2 digest, power off and boot v2. Confirm that its running kernel
  and boot-entry initramfs hashes changed. Test the recovery key on v2,
  then restore normal boot. Run `sudo bootc rollback`, power off and boot the
  retained v1. Confirm both original payload hashes and then test the recovery
  key and normal TPM unlock again.
- [ ] Review the table below. Stop before using this as the daily system
  if an image does not boot, a credential fails, marker data changes, or
  verification or unlock evidence is missing.

The same guest-side checks apply on Hyper-V. Use a temporary GRUB edit to
append `module_blacklist=tpm_crb,tpm_tis,tpm_tis_core` on a recovery boot if
the guest TPM drivers are modular. It must produce a real disk prompt and
`/dev/tpmrm0` must be absent afterward. Enter only the saved recovery key;
never put it in GRUB or an argument. Remove the one-boot override at the next
boot. On the dedicated physical fixture, an owner-approved change to the
Secure Boot enable setting can also exercise the PCR change. Restore the
original setting afterward. Keep the enrolled keys and the TPM intact.
If recovery fails or policy restoration changes PCR 7, use the offline
passphrase and the retained deployment. Record the changed PCR, unlock
method, and token metadata before considering authenticated repair. Use
[the installer enrollment and repair guide](installer-enrollment.md) for that flow.

## Guest evidence and exact lifecycle commands

Run from the maintenance account. Keep the evidence directory private and
export only the listed public outputs. Copy
`crates/ilex-e2e/src/encrypted/lifecycle-boot.py` from the qualifying revision.
Set `phase` to `v1-auto`, `v1-recovery`, `v2-auto`, `v2-recovery`,
`rollback-auto`, or `rollback-recovery` on each boot.

```sh
umask 077
phase=v1-auto
out="$HOME/qualification/$phase"
mkdir -p "$out"
sudo systemctl mask --now bootc-fetch-apply-updates.timer
sudo bootc status --json > "$out/bootc.json"
sudo python3 -I -S ./lifecycle-boot.py > "$out/boot-payload.json"
sudo ilex-enroll probe --json > "$out/probe.json"
sudo cat /run/ilex/root-unlock.json > "$out/root-unlock.json"
cat /proc/sys/kernel/random/boot_id > "$out/boot-id"
sudo ilex-enroll layout --root / > "$out/layout.json"
uuid=$(jq -er .uuid "$out/layout.json")
sudo ilex-enroll status --json --device "/dev/disk/by-uuid/$uuid" > "$out/tokens.json"
sudo cryptsetup luksDump --dump-json-metadata "/dev/disk/by-uuid/$uuid" |
  sha256sum > "$out/header.sha256"
if test -c /dev/tpmrm0; then
  sudo cat /sys/kernel/security/tpm0/binary_bios_measurements > "$out/eventlog.bin"
  cat /sys/class/tpm/tpm0/pcr-sha256/7 > "$out/pcr7.txt"
  if jq -e '.secure_boot == true' "$out/probe.json" >/dev/null; then
    jq -e --arg measured "$(tr '[:upper:]' '[:lower:]' < "$out/pcr7.txt")" \
      '.verdict == "eligible" and .pcr7 == $measured and .replay.sha256 == $measured' \
      "$out/probe.json"
  fi
fi
rpm -q shim-x64 grub2-efi-x64 systemd cryptsetup > "$out/versions.txt"
getenforce > "$out/selinux.txt"
findmnt --target /var > "$out/var.txt"
```

Back up the LUKS header once. Substitute the mount point of the designated
offline encrypted volume and disconnect it after verifying the backup exists.
The header can restore key slots and must stay private.

```sh
sudo cryptsetup luksHeaderBackup "/dev/disk/by-uuid/$uuid" \
  --header-backup-file /run/media/maintenance/RECOVERY/ilex-root.luks-header
```

From the authenticated **homed** session, run the first line once and the
rest on each deployment, including recovery boots. Record the checksum,
numeric UID, LUKS storage type and btrfs mount. Check any selected profile
state and user edits from this home after upgrade and rollback.

```sh
umask 077
dd if=/dev/urandom of="$HOME/lifecycle-witness" bs=1M count=4 status=none conv=excl
sync -f "$HOME/lifecycle-witness"
sha256sum "$HOME/lifecycle-witness"
id -u
homectl inspect "$USER"
findmnt --target "$HOME"
```

Back in the maintenance account after logout, verify the home is inactive with
`homectl inspect <homed-user>` and `mountpoint /home/<homed-user>`. To upgrade
and later roll back use the following, checking the staged and booted digests
between commands. On Hyper-V substitute `sudo systemctl reboot` for poweroff
until the final exit. The installed origin must report `containerPolicy`.

```sh
v2='ghcr.io/reidond/ilex@sha256:<v2-digest>'
sudo bootc switch --enforce-container-sigpolicy "$v2"
sudo bootc status --json
sudo systemctl poweroff
# Run after validating v2 and its recovery boot.
sudo bootc rollback
sudo bootc status --json
sudo systemctl poweroff
```

| Stage | Expected deployment | Root evidence | Comparison to record |
| --- | --- | --- | --- |
| v1 normal | v1 | `tpm2` | Installer and installed PCR 7 and replay |
| v1 recovery | v1 | `fallback` | Same header and home data, TPM absence or changed PCR |
| v2 normal | v2, retained v1 | `tpm2` | New kernel release and both hashes, compare PCR 7 to v1 |
| v2 recovery | v2 | `fallback` | Same header and home data, TPM absence or changed PCR |
| rollback normal | v1 | `tpm2` | Original hashes restored, compare PCR 7 to v1 |
| rollback recovery | v1 | `fallback` | Same header and home data, TPM absence or changed PCR |

Verify root unlock records identify the current boot ID and original LUKS UUID.
Record firmware recovery restoration and the last unattended unlock too.
Attach the sanitized result matrix, ISO and image digests, phase timings,
kernel and initramfs hashes, firmware measurements and home checksum.
Keep recovery input, credentials, raw disks, headers, and private TPM and
homed state out of the artifacts.
