#!/bin/bash
# Background preparation for the publish workflow, started while the image
# builds; the workflow's "Wait for" steps collect it.
#
#   vm-prep.sh ephemeral   the build job: the VM stack, then a pull of the
#                          published :latest (`latest-pull`). The pull records
#                          which compressed blob in the registry holds each of
#                          its layers, so staging the candidate reuses them.
#   vm-prep.sh upgrade     the upgrade job: the VM stack, then the upgrade
#                          tier's v1 installed to a disk and booted once
#                          (`ILEX_UPGRADE_PREPARE=1 cargo xtask test upgrade::`,
#                          `upgrade-prepare`). bcvk keeps the installed disk, so
#                          the real test boots v1 in seconds.
#   vm-prep.sh wait NAME   wait for NAME (vm-stack, latest-pull,
#                          upgrade-prepare), print its log, exit with its code.
#                          A failed latest-pull or upgrade-prepare only warns:
#                          the push uploads more, the test installs v1 itself.
#
# Each part writes $RUNNER_TEMP/<name>.log and, when done, <name>.status (its
# exit code).
set -uo pipefail
out=$RUNNER_TEMP

finish() { # name exit-code
    echo "$2" > "$out/$1.status.tmp" && mv "$out/$1.status.tmp" "$out/$1.status"
}

# A subshell, so its `set -e` stops only this part.
vm_stack() (
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
)

# run NAME COMMAND...: log to NAME.log, then write NAME.status.
run() {
    local name=$1
    shift
    "$@" > "$out/$name.log" 2>&1
    local rc=$?
    finish "$name" "$rc"
    return "$rc"
}

# skip NAME WHY: NAME never ran.
skip() {
    echo "not started: $2" > "$out/$1.log"
    finish "$1" 1
}

case "${1:-}" in
    ephemeral)
        # The pull needs nothing from the VM stack: both at once.
        run latest-pull podman pull --quiet "$ILEX_UPGRADE_FROM" &
        run vm-stack vm_stack
        wait
        ;;
    upgrade)
        if run vm-stack vm_stack; then
            # `sg libvirt`: the group joined above, without a new login.
            ILEX_UPGRADE_PREPARE=1 run upgrade-prepare sg libvirt -c "cargo xtask test upgrade::"
        else
            skip upgrade-prepare "the VM stack failed"
        fi
        ;;
    wait)
        name=${2:?wait for what?}
        until [ -e "$out/$name.status" ]; do sleep 2; done
        echo "::group::$name"
        cat "$out/$name.log"
        echo "::endgroup::"
        rc=$(cat "$out/$name.status")
        if [ "$rc" != 0 ]; then
            case "$name" in
                vm-stack) exit "$rc" ;;
                latest-pull) echo "::warning::pulling :latest failed (exit $rc); staging uploads every layer" ;;
                upgrade-prepare) echo "::warning::preparing v1's disk failed (exit $rc); the upgrade test installs it itself" ;;
            esac
        fi
        ;;
    *)
        echo "usage: $0 ephemeral | upgrade | wait NAME" >&2
        exit 2
        ;;
esac
