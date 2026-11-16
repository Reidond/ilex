#!/bin/bash
# Boot the builder's installed Fedora kernel/initramfs in real nested KVM.
set -euo pipefail
exec 7>/run/lock/ilex-builder-smoke.lock
flock -n 7
work=$(mktemp -d /var/lib/ilex-data/artifacts/ilex-builder-smoke.XXXXXX)
trap 'rm -rf -- "$work"' EXIT
kernel=$(find /boot -maxdepth 1 -name 'vmlinuz-*' | sort -V | tail -1)
version=${kernel#/boot/vmlinuz-}
# dracut creates an isolated initramfs; /init proves the nested kernel ran
# userspace and powers down. No guest rootfs or registry is needed.
mkdir -p "$work/overlay"
cat >"$work/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
echo ILEX_NESTED_KVM_BOOTED
/usr/bin/systemctl poweroff --force --force
EOF
chmod +x "$work/init"
dracut --force --no-hostonly --kver "$version" --install '/bin/sh /usr/bin/mount /usr/bin/systemctl' --include "$work/init" /init "$work/initrd"
timeout 120 qemu-system-x86_64 -accel kvm -machine q35 -cpu host -m 512 -smp 1 \
  -nodefaults -display none -monitor none -serial stdio -no-reboot \
  -kernel "$kernel" -initrd "$work/initrd" -append 'console=ttyS0 panic=-1' >"$work/serial.log" 2>&1
grep -q ILEX_NESTED_KVM_BOOTED "$work/serial.log"
podman info --format '{{.Host.OCIRuntime.Name}}' >&2
bcvk --version >&2
qemu-system-x86_64 --version | head -1 >&2
python3 - <<'PY'
import fcntl,json,os
fd=os.open('/dev/kvm',os.O_RDWR)
assert fcntl.ioctl(fd,0xae00,0)==12
record=json.load(open('/var/lib/ilex-builder/provision.json'))
record.update(nested_kvm_boot=True,kvm_api=12)
print(json.dumps(record))
PY
