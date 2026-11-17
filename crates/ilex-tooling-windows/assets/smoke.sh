#!/bin/bash
# Boot the builder's installed Fedora kernel/initramfs in real nested KVM.
set -euo pipefail
exec 7>/run/lock/ilex-builder-smoke.lock
flock -n 7
work=$(mktemp -d /var/lib/ilex-data/artifacts/ilex-builder-smoke.XXXXXX)
cleanup() {
    result=$?
    if [ -f "$work/serial.log" ]; then
        cp "$work/serial.log" /var/lib/ilex-data/artifacts/ilex-builder-smoke-last.log
        if [ "$result" -ne 0 ]; then tail -80 "$work/serial.log" >&2; fi
    fi
    rm -rf -- "$work"
    exit "$result"
}
trap cleanup EXIT
kernel=$(find /boot -maxdepth 1 -name 'vmlinuz-*' | sort -V | tail -1)
version=${kernel#/boot/vmlinuz-}
# dracut creates an isolated initramfs; /init proves the nested kernel ran
# userspace and powers down. No guest rootfs or registry is needed.
mkdir -p "$work/overlay/dev"
cat >"$work/overlay/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
echo ILEX_NESTED_KVM_BOOTED
/usr/bin/systemctl poweroff --force --force
EOF
chmod +x "$work/overlay/init"
mknod -m 0600 "$work/overlay/dev/console" c 5 1
# This fixture needs no block/network drivers. Keep its userspace initramfs
# small enough for 512 MiB; the installed Fedora kernel still boots on real KVM.
dracut --force --no-compress --no-hostonly --no-kernel --modules base --kver "$version" --install '/bin/sh /usr/bin/mount /usr/bin/systemctl' "$work/initrd"
# Append an explicit newc overlay: dracut may already have an /init symlink,
# so --include alone does not guarantee replacement of its normal root search.
(cd "$work/overlay"; printf 'init\0dev\0dev/console\0' | cpio --null --quiet -o -H newc --owner=0:0) >>"$work/initrd"
timeout --kill-after=10 120 qemu-system-x86_64 -accel kvm -machine q35 -cpu host -m 512 -smp 1 \
  -nodefaults -display none -monitor none -serial stdio -no-reboot \
  -kernel "$kernel" -initrd "$work/initrd" -append 'console=ttyS0 panic=-1' >"$work/serial.log" 2>&1
grep -q ILEX_NESTED_KVM_BOOTED "$work/serial.log"
podman info --format '{{.Host.OCIRuntime.Name}}' >&2
/usr/local/bin/bcvk --version >&2
qemu-system-x86_64 --version >&2
ILEX_NESTED_KERNEL="$version" python3 - <<'PY'
import fcntl,json,os
fd=os.open('/dev/kvm',os.O_RDWR)
assert fcntl.ioctl(fd,0xae00,0)==12
record=json.load(open('/var/lib/ilex-builder/provision.json'))
memory=int(next(line.split()[1] for line in open('/proc/meminfo') if line.startswith('MemTotal:')))*1024
record.update(nested_kvm_boot=True,kvm_api=12,guest_memory_bytes=memory,guest_cpus=os.cpu_count(),guest_kernel=os.uname().release,nested_kernel=os.environ['ILEX_NESTED_KERNEL'])
print(json.dumps(record))
PY
