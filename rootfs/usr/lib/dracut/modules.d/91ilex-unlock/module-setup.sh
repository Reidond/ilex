#!/bin/bash
check() { require_binaries ilex-enroll cryptsetup dmsetup lsblk findmnt || return 1; return 0; }
depends() { echo "systemd-cryptsetup crypt tpm2-tss"; }
install() {
    inst_multiple /usr/bin/ilex-enroll /usr/sbin/cryptsetup /usr/sbin/dmsetup /usr/bin/lsblk /usr/bin/findmnt /usr/bin/true
    # Keep the original only inside this initramfs; the image's systemd binary
    # and ordinary installed-system cryptsetup services stay untouched.
    # Fedora 45 moved the executable to /usr/bin and kept the old lib/systemd
    # path as a compatibility symlink. Save the ELF, then cover both generator
    # paths; copying the relative symlink would bypass our method recorder.
    inst "$(readlink -f /usr/lib/systemd/systemd-cryptsetup)" /usr/libexec/ilex/systemd-cryptsetup-original
    rm -f "$initdir/usr/lib/systemd/systemd-cryptsetup" "$initdir/usr/bin/systemd-cryptsetup"
    inst_simple "$moddir/cryptsetup-wrapper.sh" /usr/lib/systemd/systemd-cryptsetup
    inst_simple "$moddir/cryptsetup-wrapper.sh" /usr/bin/systemd-cryptsetup
    inst_hook pre-pivot 90 "$moddir/record-root.sh"
    inst_libdir_file 'cryptsetup/libcryptsetup-token-systemd-tpm2.so'
}
