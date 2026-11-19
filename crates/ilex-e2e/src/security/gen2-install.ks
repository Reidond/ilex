# Unmodified release ISO and its signed network payload.
%include /usr/share/anaconda/interactive-defaults.ks
graphical
lang en_US.UTF-8
keyboard us
timezone Etc/UTC --utc
rootpw --lock
zerombr
clearpart --all --initlabel
autopart
poweroff

%post --nochroot --erroronfail
set -eu
umask 077
root=/mnt/sysroot
seed=$(mktemp -d /run/ilex-gen2-install.XXXXXXXX)
trap 'if mountpoint -q "$seed"; then umount "$seed"; fi; rmdir "$seed"' EXIT
mount -t iso9660 -o ro,nosuid,nodev,noexec LABEL=ILEX_GEN2 "$seed"
install -d -m 0700 "$root/var/lib/ilex-gen2"
# Bind this transition control to the TPM identity. The separate firmware
# fixture covers PCR7 policy, and PR18 exercises PCR-bound LUKS enrollment.
printf ilex-gen2-installer-proof |
    systemd-creds encrypt --name=ilex-gen2-install --with-key=tpm2 --tpm2-device=auto --tpm2-pcrs= - "$root/var/lib/ilex-gen2/installer.cred"
cat /proc/sys/kernel/random/boot_id > "$root/var/lib/ilex-gen2/installer-boot"
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
