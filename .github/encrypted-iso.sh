#!/bin/bash
# Build job uses rootless storage for the candidate. osbuild needs the same
# rootful, privileged Fedora builder as the ISO job, in separate state. Only the
# GNOME edition's ISO (ilex Glabra) is built: the encrypted gates install
# through it.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true ]] || { echo 'hosted CI only' >&2; exit 1; }
if [[ -e /etc/apparmor.d/bwrap-userns-restrict ]]; then
    sudo -n apparmor_parser -R /etc/apparmor.d/bwrap-userns-restrict
fi
for knob in kernel.apparmor_restrict_unprivileged_userns kernel.apparmor_restrict_unprivileged_unconfined; do
    if sudo -n sysctl -n "$knob" >/dev/null 2>&1; then sudo -n sysctl -w "$knob=0"; fi
done
root="$RUNNER_TEMP/ilex-encrypted-iso"
sudo -n mkdir -p "$root/containers" "$root/cache" "$root/state/tmp"
sudo -n podman run --rm --privileged --security-opt label=disable \
    --security-opt apparmor=unconfined --security-opt seccomp=unconfined \
    -v "$PWD:/src" -w /src \
    -v "$root/containers:/var/lib/containers" \
    -v "$root/cache:/var/cache/image-builder" \
    -v "$root/state:/mnt/ilex-state" \
    -e ILEX_STATE_DIR=/mnt/ilex-state -e TMPDIR=/mnt/ilex-state/tmp \
    quay.io/fedora/fedora:45 bash -euc '
        dnf -y install git cargo gcc podman skopeo image-builder
        git config --global --add safe.directory /src
        cargo ilexctl iso --edition gnome --output-dir /mnt/ilex-state/iso
    '
echo "ILEX_ISO_NEXT=$root/state/iso/ilex-glabra-installer.iso" >> "$GITHUB_ENV"
