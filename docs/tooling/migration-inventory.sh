#!/usr/bin/env bash
# Run in an existing Linux session. Report paths and process names to stdout.
set -euo pipefail

if [[ $(uname -s) != Linux ]]; then
    echo 'Run the inventory from the Linux session you are inventorying.' >&2
    exit 2
fi

show() {
    local label=$1 path=$2
    if [[ -e $path ]]; then
        printf '%s: %s\n' "$label" "$path"
    else
        printf '%s: absent or inaccessible (%s)\n' "$label" "$path"
    fi
}

cache=${XDG_CACHE_HOME:-"$HOME/.cache"}
state=${ILEX_STATE_DIR:-"$cache/ilex"}
cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
kache=${KACHE_CACHE_DIR:-"$cache/kache"}

printf 'User: %s\nArchitecture: %s\n' "$(id -un)" "$(uname -m)"
printf '\nSessions and relevant processes (names only)\n'
who
ps -eo pid,user,comm | awk 'NR == 1 || $3 ~ /^(bcvk|buildah|cargo|conmon|drupe|ilex-|kache|podman|qemu|rustc|virtqemud|libvirtd)/'

printf '\nData locations\n'
show 'ilex state (VM disks, tool data, installer and test artifacts)' "$state"
show 'libvirt images and installed-disk cache' /var/lib/libvirt/images
show 'rootful Podman storage' /var/lib/containers/storage
show 'rootful Podman image metadata' /var/lib/containers/storage/overlay-images/images.json
show 'rootful Podman volumes' /var/lib/containers/storage/volumes
show 'per-user Podman storage' "${XDG_DATA_HOME:-"$HOME/.local/share"}/containers/storage"
show 'compiler cache' "$kache"
show 'compiler cache index' "$kache/index.db"
show 'kache engine' "$(command -v kache-engine || printf '<not on PATH>')"
show 'Cargo registry cache' "$cargo_home/registry"
show 'Cargo Git cache' "$cargo_home/git"
if [[ -n ${CARGO_TARGET_DIR:-} ]]; then
    show 'Cargo target override' "$CARGO_TARGET_DIR"
fi
show 'legacy development profile' /etc/profile.d/ilex-dev.sh

for checkout in "$@"; do
    printf '\nCheckout: %s\n' "$checkout"
    GIT_OPTIONAL_LOCKS=0 git -C "$checkout" -c core.fsmonitor=false \
        status --short --branch --untracked-files=normal ||
        printf 'Inspect this checkout with Git on its owning host.\n'
    show 'checkout target directory' "$checkout/target"
done

cat <<'EOF'

Owner review
- Confirm all projects, users and active sessions with a stake in the distro.
- Reconcile Podman image names/digests/owners and named volumes.
- Account for persistent VM disks, local installer outputs and drupe data.
- Review external MCP registrations, launchers and private backups separately.
EOF
