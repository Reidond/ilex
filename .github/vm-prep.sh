#!/bin/bash
# Background preparation for the publish workflow, started while the image
# builds; the workflow's "Wait for" steps collect it.
#
#   vm-prep.sh ephemeral   the build job: the VM stack and, meanwhile, a pull
#                          of the published :latest (`latest-pull`). The pull
#                          records which compressed blob in the registry holds
#                          each of its layers, so staging the candidate reuses
#                          them.
#   vm-prep.sh upgrade     the upgrade job and the v1-disk run: the VM stack
#                          and, meanwhile, a pull of v1 ($ILEX_UPGRADE_FROM,
#                          `v1-pull`); then v1 installed to a disk and booted
#                          once (`ILEX_UPGRADE_PREPARE=1 cargo xtask test
#                          upgrade::`, `upgrade-prepare`). bcvk keeps the
#                          installed disk, so the real test boots v1 in
#                          seconds. When $ILEX_V1_DISK holds v1's disk from the
#                          Actions cache (see save-disk), it goes into bcvk's
#                          pool first and bcvk boots it instead of installing
#                          (if it is stale, bcvk installs as usual).
#   vm-prep.sh save-disk DIR
#                          the v1-disk run: bcvk's base disk into DIR for the
#                          Actions cache, with the xattrs bcvk recognizes it by
#                          (the cache drops xattrs) in a file next to it.
#   vm-prep.sh wait NAME   wait for NAME (vm-stack, latest-pull, v1-pull,
#                          upgrade-prepare) and print its log. A failed
#                          vm-stack fails; a failed pull or upgrade-prepare only
#                          warns: the push uploads more, the test pulls and
#                          installs v1 itself.
#
# Each part writes $RUNNER_TEMP/<name>.log, every line with the time it was
# printed, and when done <name>.status (its exit code).
set -uo pipefail
out=$RUNNER_TEMP
# bcvk's libvirt storage pool, where it keeps its base disks.
pool=/var/lib/libvirt/images

finish() { # name exit-code
    echo "$2" > "$out/$1.status.tmp" && mv "$out/$1.status.tmp" "$out/$1.status"
}

stamp() {
    local line
    while IFS= read -r line || [ -n "$line" ]; do
        printf '%(%H:%M:%S)T %s\n' -1 "$line"
    done
}

# A subshell, so its `set -e` stops only this part. Never call it (or `run`
# with it) as a condition (`if`, `&&`, `||`): bash ignores `set -e` there,
# inside subshells too, and a failed checksum would go on to install bcvk.
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
    sudo install -d -o "$USER" -g libvirt -m 0775 "$pool"
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

# bcvk recognizes a base disk by two xattrs (user.bootc.cache_hash, the hash
# of the image digest and the install options, and user.bootc.image_digest)
# and reuses it when the hash matches what it would install.
# FILE: its user.bootc.* xattrs as JSON (hex values).
xattrs_dump='
import json, os, sys
p = sys.argv[1]
print(json.dumps({k: os.getxattr(p, k).hex() for k in os.listxattr(p) if k.startswith("user.bootc.")}))
'
# FILE JSON: set them again.
xattrs_load='
import json, os, sys
with open(sys.argv[2]) as f:
    for k, v in json.load(f).items():
        os.setxattr(sys.argv[1], k, bytes.fromhex(v))
'

save_disk() { # dir
    local dir=$1 f n=0
    mkdir -p "$dir"
    for f in "$pool"/bootc-base-*.qcow2; do
        [ -e "$f" ] || continue
        sudo cp "$f" "$dir/"
        sudo python3 -I -c "$xattrs_dump" "$f" > "$dir/${f##*/}.xattrs"
        n=$((n + 1))
    done
    sudo chown -R "$(id -u):$(id -g)" "$dir"
    ls -l "$dir"
    cat "$dir"/*.xattrs 2>/dev/null
    if [ "$n" = 0 ]; then
        echo "no base disk in $pool" >&2
        return 1
    fi
}

restore_disk() {
    local dir=${ILEX_V1_DISK:-} f dest
    [ -n "$dir" ] || return 0
    for f in "$dir"/bootc-base-*.qcow2; do
        if [ ! -e "$f" ]; then
            echo "no v1 disk from the cache: installing v1"
            continue
        fi
        dest=$pool/${f##*/}
        mv "$f" "$dest" && python3 -I -c "$xattrs_load" "$dest" "$f.xattrs" &&
            echo "v1's disk from the cache: $dest"
    done
}

prepare() {
    restore_disk
    ILEX_UPGRADE_PREPARE=1 sg libvirt -c "cargo xtask test upgrade::"
}

# run NAME COMMAND...: log to NAME.log, then write NAME.status.
run() {
    local name=$1
    shift
    "$@" 2>&1 | stamp > "$out/$name.log"
    local rc=${PIPESTATUS[0]}
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
        run v1-pull podman pull --quiet "$ILEX_UPGRADE_FROM" &
        run vm-stack vm_stack
        stack=$?
        wait
        if [ "$stack" = 0 ]; then
            run upgrade-prepare prepare
        else
            skip upgrade-prepare "the VM stack failed"
        fi
        ;;
    save-disk)
        save_disk "${2:?save into where?}"
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
                v1-pull) echo "::warning::pulling v1 failed (exit $rc); the test pulls it itself" ;;
                upgrade-prepare) echo "::warning::preparing v1's disk failed (exit $rc); the upgrade test installs it itself" ;;
            esac
        fi
        ;;
    *)
        echo "usage: $0 ephemeral | upgrade | save-disk DIR | wait NAME" >&2
        exit 2
        ;;
esac
