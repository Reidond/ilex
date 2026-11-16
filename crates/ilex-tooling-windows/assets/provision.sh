#!/bin/bash
# Root-only, repeatable provisioning. Fedora repositories retain gpgcheck=1.
set -euo pipefail
revision=$1
mkdir -p /var/lib/ilex-builder
exec > >(tee -a /var/lib/ilex-builder/provision.log) 2>&1
exec 9>/run/lock/ilex-builder-provision.lock
flock -n 9
dnf --setopt=max_parallel_downloads=10 install -y \
  podman cargo rust gcc gcc-c++ git qemu-kvm qemu-img libvirt-daemon-kvm \
  libvirt-client virtiofsd passt edk2-ovmf swtpm swtpm-tools tpm2-tools \
  python3-pyyaml hyperv-daemons openssl-devel pkgconf-pkg-config systemd-devel \
  gnu-efi-devel efibootmgr sbsigntools image-builder btrfs-progs xfsprogs \
  erofs-utils squashfs-tools policycoreutils-devel cpio zstd
# Fedora 45 currently ships the required generic-iso implementation in updates-testing.
if ! image-builder --version | grep -Eq '\b(89|9[0-9]|[1-9][0-9]{2,})\.'; then
    dnf --enablerepo=updates-testing upgrade -y image-builder
fi
python3 - <<'PY'
import re, subprocess
s=subprocess.check_output(['image-builder','--version'],text=True)
assert int(re.search(r'\b(\d+)',s).group(1)) >= 89, s
PY
if ! mountpoint -q /var/lib/ilex-data; then
    # The owned VM has one 40 GiB OS disk and one dedicated 240 GiB data disk.
    # No wildcard write: select one disk with the recorded serial and expected size.
    device=$(python3 - <<'PY'
import json,subprocess
d=json.loads(subprocess.check_output(['lsblk','-J','-b','-o','NAME,SIZE,TYPE,MOUNTPOINTS']))['blockdevices']
c=[x for x in d if x['type']=='disk' and x['size']==240*1024**3 and not x.get('children') and not any(x.get('mountpoints') or [])]
assert len(c)==1, 'expected exactly one blank 240 GiB data disk'
print('/dev/'+c[0]['name'])
PY
)
    signature=$(blkid -p -s TYPE -o value "$device" || true)
    if [ -z "$signature" ]; then mkfs.xfs -L ilex-data "$device"; else
        test "$signature" = xfs
        test "$(blkid -s LABEL -o value "$device")" = ilex-data
    fi
    mkdir -p /var/lib/ilex-data
    uuid=$(blkid -s UUID -o value "$device")
    grep -q '^UUID=.* /var/lib/ilex-data ' /etc/fstab || echo "UUID=$uuid /var/lib/ilex-data xfs defaults 0 2" >> /etc/fstab
    mount /var/lib/ilex-data
fi
owner=$(cat /var/lib/ilex-builder/owner)
if [ -e /var/lib/ilex-data/.ilex-builder-owner ]; then
    test "$(cat /var/lib/ilex-data/.ilex-builder-owner)" = "$owner"
else
    printf '%s\n' "$owner" >/var/lib/ilex-data/.ilex-builder-owner
fi
for sub in containers libvirt checkouts cargo kache artifacts jobs; do mkdir -p "/var/lib/ilex-data/$sub"; done
for sub in containers libvirt; do
    mkdir -p "/var/lib/$sub"
    if ! mountpoint -q "/var/lib/$sub"; then
        # Never overwrite populated storage during a repeat invocation.
        test -z "$(find "/var/lib/$sub" -mindepth 1 -maxdepth 1 -print -quit)" || cp -a -n "/var/lib/$sub/." "/var/lib/ilex-data/$sub/"
        grep -q " /var/lib/$sub " /etc/fstab || echo "/var/lib/ilex-data/$sub /var/lib/$sub none bind 0 0" >> /etc/fstab
        mount "/var/lib/$sub"
    fi
done
restorecon -RF /var/lib/containers /var/lib/libvirt
systemctl enable --now hypervkvpd.service virtqemud.socket virtnetworkd.socket virtstoraged.socket
curl --fail --location --proto '=https' --proto-redir '=https' \
  https://github.com/bootc-dev/bcvk/releases/download/v0.22.0/bcvk-x86_64-unknown-linux-gnu.tar.gz -o /var/lib/ilex-builder/bcvk.tar.gz
echo 'eb72198c0d36fab75bfcadbb6794a87afa6f496a4da6b1b34d27728243fa6474  /var/lib/ilex-builder/bcvk.tar.gz' | sha256sum -c -
tar -xzf /var/lib/ilex-builder/bcvk.tar.gz -C /var/lib/ilex-builder bcvk-x86_64-unknown-linux-gnu
install -m 0755 /var/lib/ilex-builder/bcvk-x86_64-unknown-linux-gnu /usr/local/bin/bcvk
install -m 0755 /var/lib/ilex-builder/builder-control.sh /usr/local/libexec/ilex-builder-control
install -m 0755 /var/lib/ilex-builder/smoke.sh /usr/local/libexec/ilex-builder-smoke
printf 'ilex ALL=(root) NOPASSWD: /usr/local/libexec/ilex-builder-control\n' >/etc/sudoers.d/ilex-builder
chmod 0440 /etc/sudoers.d/ilex-builder
visudo -cf /etc/sudoers.d/ilex-builder
rpm -qa --qf '%{NAME} %{VERSION}-%{RELEASE}.%{ARCH}\n' | sort > /var/lib/ilex-builder/packages.txt
python3 - "$revision" <<'PY'
import hashlib,json,os,sys,subprocess
p='/var/lib/ilex-builder/provision.json'
v={'schema':1,'revision':sys.argv[1],'owner':open('/var/lib/ilex-builder/owner').read().strip(),
   'data_uuid':subprocess.check_output(['findmnt','-n','-o','UUID','/var/lib/ilex-data'],text=True).strip(),
   'bcvk':subprocess.check_output(['bcvk','--version'],text=True).strip(),
   'package_manifest_sha256':hashlib.sha256(open('/var/lib/ilex-builder/packages.txt','rb').read()).hexdigest(),
   'packages':[line for line in open('/var/lib/ilex-builder/packages.txt').read().splitlines()
               if line.split()[0] in {'podman','qemu-kvm','qemu-system-x86-core','libvirt-daemon-kvm','image-builder','cargo','rust','kernel-core','hyperv-daemons'}]}
with open(p+'.partial','w') as f: json.dump(v,f); f.flush(); os.fsync(f.fileno())
os.replace(p+'.partial',p)
PY
# Bootstrap identity cannot log in again, even if a detached seed is recovered.
if id ilex-bootstrap &>/dev/null; then rm -f /home/ilex-bootstrap/.ssh/authorized_keys; usermod -L ilex-bootstrap; fi
