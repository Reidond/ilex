#!/bin/bash
# Build a drupe tool image from Fedora packages. Runs inside a fedora
# container started by `cargo xtask tool` (tool.rs sends this script on
# stdin), which sets:
#
#   NAME      tool name, also the image's file name (/out/$NAME.raw)
#   PACKAGES  space-separated packages whose commands the tool exports
#   /out      where the image goes
#   /keys     the signing key pair (drupe.key, drupe.crt), created when missing
#
# The image is a discoverable disk image: an erofs root partition holding the
# tool as it appears under /app, its dm-verity hash partition and a signature
# partition. The tree is not a Fedora root: bin/ holds the commands, lib64/
# every shared library they load plus glibc's dynamic loader, so nothing in it
# depends on the libraries of the machine it runs on.
set -euo pipefail

: "${NAME:?}" "${PACKAGES:?}"
work=/work
root=$work/root
app=$work/app
loader=lib64/ld-linux-x86-64.so.2

dnf -y -q install systemd-repart erofs-utils openssl

if [ ! -f /keys/drupe.key ]; then
    openssl req -x509 -newkey rsa:4096 -sha256 -nodes -days 3650 \
        -subj "/CN=drupe development key" \
        -keyout /keys/drupe.key -out /keys/drupe.crt 2>/dev/null
    chmod 0600 /keys/drupe.key
    echo "created the signing key /keys/drupe.key"
fi

# shellcheck disable=SC2086
dnf -y -q --installroot="$root" --use-host-config --nodocs \
    --setopt=install_weak_deps=0 install $PACKAGES

mkdir -p "$app/bin" "$app/lib64"
commands=""
needs_loader=0
# shellcheck disable=SC2086
for file in $(rpm --root "$root" -ql $PACKAGES | grep -E '^/usr/s?bin/[A-Za-z0-9._+-]+$' | sort -u); do
    src=$root$file
    [ -f "$src" ] && [ -x "$src" ] || continue
    cmd=$(basename "$file")
    cp -L "$src" "$app/bin/$cmd"
    # The loader lists what it would load, as ldd does, without running a
    # shell from the install root. It fails on scripts and static binaries:
    # those run as they are.
    if libs=$("$root/usr/$loader" --library-path "$root/usr/lib64" --list "$src" 2>/dev/null); then
        needs_loader=1
        echo "$libs" | awk '$2 == "=>" && $3 ~ /^\// { print $3 }' | while read -r lib; do
            cp -Ln "$lib" "$app/lib64/"
        done
        commands="$commands$cmd:loader "
    else
        commands="$commands$cmd:direct "
    fi
done
[ -n "$commands" ] || { echo "no commands in /usr/bin or /usr/sbin of: $PACKAGES" >&2; exit 1; }
if [ "$needs_loader" = 1 ]; then
    cp -L "$root/usr/$loader" "$app/$loader"
fi

# drupe.json: names are [A-Za-z0-9._+-] only (the grep above), so they need
# no JSON escaping.
first_pkg=${PACKAGES%% *}
version=$(rpm --root "$root" -q --qf '%{VERSION}-%{RELEASE}' "$first_pkg")
{
    printf '{\n  "name": "%s",\n  "version": "%s",\n  "commands": {' "$NAME" "$version"
    sep=""
    for entry in $commands; do
        cmd=${entry%%:*}
        if [ "${entry##*:}" = loader ]; then
            printf '%s\n    "%s": {"exec": "bin/%s", "loader": "%s", "library-path": ["lib64"]}' \
                "$sep" "$cmd" "$cmd" "$loader"
        else
            printf '%s\n    "%s": {"exec": "bin/%s"}' "$sep" "$cmd" "$cmd"
        fi
        sep=","
    done
    printf '\n  }\n}\n'
} > "$app/drupe.json"
cat "$app/drupe.json"

mkdir -p $work/repart.d
cat > $work/repart.d/10-root.conf <<'EOF'
[Partition]
Type=root
Format=erofs
CopyFiles=/
Verity=data
VerityMatchKey=root
Minimize=best
EOF
cat > $work/repart.d/20-root-verity.conf <<'EOF'
[Partition]
Type=root-verity
Verity=hash
VerityMatchKey=root
Minimize=best
EOF
cat > $work/repart.d/30-root-verity-sig.conf <<'EOF'
[Partition]
Type=root-verity-sig
Verity=signature
VerityMatchKey=root
EOF

rm -f "/out/$NAME.raw"
systemd-repart --empty=create --size=auto --dry-run=no --offline=yes \
    --definitions=$work/repart.d --copy-source="$app" \
    --private-key=/keys/drupe.key --certificate=/keys/drupe.crt \
    --seed=random --no-pager "/out/$NAME.raw"
ls -l "/out/$NAME.raw"
