#!/bin/bash
# Started in the background by the publish workflow while the image builds.
# Two things the e2e tests need that don't depend on the candidate image:
#
# 1. the VM stack: KVM for everyone, libvirt, QEMU, virtiofsd, passt, bcvk;
# 2. the upgrade tier's v1 (the published :latest) installed to a disk:
#    `ILEX_UPGRADE_PREPARE=1 cargo xtask test upgrade::` stops once v1 has
#    booted, and bcvk keeps the installed base disk, so the real test boots
#    v1 in seconds. The install is most of that tier's time.
#
# Each writes <name>.log and, when done, <name>.status (the exit code) to
# $RUNNER_TEMP; the workflow's wait steps read them.
set -uo pipefail
out=$RUNNER_TEMP

finish() { # name exit-code
    echo "$2" > "$out/$1.status.tmp" && mv "$out/$1.status.tmp" "$out/$1.status"
}

vm_stack() {
    set -euxo pipefail
    echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' \
        | sudo tee /etc/udev/rules.d/99-kvm4all.rules
    sudo udevadm control --reload-rules
    sudo udevadm trigger --name-match=kvm
    sudo apt-get update
    sudo apt-get install -y qemu-system-x86 qemu-utils virtiofsd libvirt-daemon-system passt skopeo
    # The upgrade tier drives system libvirt as the `runner` user (bcvk
    # libvirt run). Ubuntu opens the libvirt socket only to the `libvirt`
    # group (the tests run under `sg libvirt`), and libvirt's AppArmor driver
    # confines QEMU and virtiofsd, which would keep the guest from the
    # virtiofs share of runner's container storage. bcvk also writes its base
    # disks straight into the pool directory, root-only on Ubuntu. Throwaway
    # VM: join the group, hand runner the pool directory and turn the driver
    # off.
    sudo usermod -aG libvirt "$USER"
    sudo install -d -o "$USER" -g libvirt -m 0775 /var/lib/libvirt/images
    echo 'security_driver = "none"' | sudo tee -a /etc/libvirt/qemu.conf
    # libvirt picks UEFI firmware from QEMU's firmware descriptors. For bcvk's
    # non-Secure-Boot VMs, Ubuntu's AMD SEV build (OVMF.amdsev.fd: stateless,
    # boots only a GRUB inside the firmware) wins, so the guest never reads
    # its disk and loops in "Boot Option Restoration". Mask the
    # confidential-computing builds: an empty file under /etc/qemu/firmware
    # hides the descriptor of the same name.
    sudo mkdir -p /etc/qemu/firmware
    for f in /usr/share/qemu/firmware/*amdsev*.json /usr/share/qemu/firmware/*inteltdx*.json; do
        if [ -e "$f" ]; then sudo touch "/etc/qemu/firmware/$(basename "$f")"; fi
    done
    ls -l /usr/share/qemu/firmware/ /etc/qemu/firmware/
    for unit in libvirtd virtqemud; do sudo systemctl try-restart "$unit" || true; done
    curl -fsSL -o "$RUNNER_TEMP/bcvk.tar.gz" \
        "https://github.com/bootc-dev/bcvk/releases/download/${BCVK_VERSION}/bcvk-x86_64-unknown-linux-gnu.tar.gz"
    echo "${BCVK_SHA256}  $RUNNER_TEMP/bcvk.tar.gz" | sha256sum --check -
    tar -xzf "$RUNNER_TEMP/bcvk.tar.gz" -C "$RUNNER_TEMP"
    sudo install -m 0755 "$RUNNER_TEMP/bcvk-x86_64-unknown-linux-gnu" /usr/local/bin/bcvk
    bcvk --version
}

(vm_stack) > "$out/vm-stack.log" 2>&1
rc=$?
finish vm-stack "$rc"
if [ "$rc" -ne 0 ]; then
    echo "not started: the VM stack failed" > "$out/upgrade-prepare.log"
    finish upgrade-prepare "$rc"
    exit "$rc"
fi

# `sg libvirt`: the group joined above, without a new login.
ILEX_UPGRADE_PREPARE=1 sg libvirt -c "cargo xtask test upgrade::" > "$out/upgrade-prepare.log" 2>&1
finish upgrade-prepare "$?"
