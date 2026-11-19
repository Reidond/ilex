# Select the release ISO's signed network payload through its own chooser
# helper. The transition fixture has no dependency on optional app preload.
%pre --erroronfail
python3 -I -S /usr/libexec/ilex-installer/ilex-installer-choice apply
%end
%include /tmp/ilex-bootc.ks
xconfig --startxonboot
graphical
lang en_US.UTF-8
keyboard us
timezone Etc/UTC --utc
rootpw --lock
zerombr
ignoredisk --only-use=sda
clearpart --all --initlabel
autopart
poweroff

%post --erroronfail
# Match the installed bootc mount configuration from interactive-defaults.ks.
sed -i -E '\|^[^#[:space:]]+[[:space:]]+/[[:space:]]|d' /etc/fstab
%end

%post --nochroot --erroronfail
set -eu
umask 077
root=/mnt/sysroot
seed=$(mktemp -d /run/ilex-gen2-install.XXXXXXXX)
answers=$(mktemp -d /run/ilex-gen2-oemdrv.XXXXXXXX)
trap 'for dir in "$seed" "$answers"; do if mountpoint -q "$dir"; then umount "$dir"; fi; rmdir "$dir"; done' EXIT
mount -t iso9660 -o ro,nosuid,nodev,noexec LABEL=ILEX_GEN2 "$seed"
mount -t ext4 -o rw,nosuid,nodev,noexec LABEL=OEMDRV "$answers"
install -d -m 0700 "$root/var/lib/ilex-gen2"
test -c /dev/tpmrm0
test "$(od -An -tu1 -j4 -N1 /sys/firmware/efi/efivars/SecureBoot-*)" -eq 1
# Bind this transition control to the TPM identity. The separate firmware
# fixture covers PCR7 policy, and PR18 exercises PCR-bound LUKS enrollment.
printf ilex-gen2-installer-proof |
    systemd-creds encrypt --name=ilex-gen2-install --with-key=tpm2 --tpm2-device=auto --tpm2-pcrs= - "$root/var/lib/ilex-gen2/installer.cred"
cat /proc/sys/kernel/random/boot_id > "$root/var/lib/ilex-gen2/installer-boot"
install -d -m 0700 "$answers/public"
cp "$root/var/lib/ilex-gen2/installer-boot" "$answers/public/installer-boot"
recovery=$(python3 -I -S -c 'from secrets import choice; print("-".join("".join(choice("cbdefghijklnrtuv") for _ in range(8)) for _ in range(8)))')
printf '%s' "$recovery" | openssl pkeyutl -encrypt -pubin -inkey "$answers/recovery.pub" -pkeyopt rsa_padding_mode:oaep -pkeyopt rsa_oaep_md:sha256 -out "$answers/recovery.oaep"
printf '%s' "$recovery" | sha256sum | cut -d' ' -f1 > "$answers/public/recovery-sha256"
unset recovery
umount "$answers"
install -m 0600 "$seed/host.key" "$root/etc/ssh/ssh_host_ed25519_key"
install -m 0600 "$seed/access.pub" "$root/etc/ssh/ilex-gen2-authorized_keys"
cat > "$root/etc/ssh/sshd_config.d/00-ilex-gen2-install.conf" <<'CONF'
AuthorizedKeysFile /etc/ssh/ilex-gen2-authorized_keys
PermitRootLogin prohibit-password
CONF
# sshd checks the file's mode and SELinux checks the supplied key types.
chcon system_u:object_r:sshd_key_t:s0 "$root/etc/ssh/ssh_host_ed25519_key"
chcon system_u:object_r:ssh_home_t:s0 "$root/etc/ssh/ilex-gen2-authorized_keys"
systemctl --root="$root" enable sshd.service
%end
