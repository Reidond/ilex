# Shared package transaction and bootc /var clean-up. Run through bash.
set -euo pipefail

# An empty install list (including one containing only excludes) is no
# transaction: do not ask dnf to install nothing or emit an empty tmpfiles file.
if ! grep -q '^[^-]' /tmp/ilex-build/packages.args; then
    rm -rf /tmp/ilex-build
    exit 0
fi

xargs -r dnf -y install --setopt=keepcache=1 --setopt=localpkg_gpgcheck=1 < /tmp/ilex-build/packages.args

find /var/cache -mindepth 1 -maxdepth 1 ! -name libdnf5 -exec rm -rf {} +
find /tmp /var/tmp -mindepth 1 -maxdepth 1 -exec rm -rf {} +
if [ -d /var/log ]; then
    find /var/log -mindepth 1 ! -type d -delete
fi
rm -rf /var/lib/dnf/repos
{ bootc container lint --no-truncate 2>&1 || true; } \
  | sed -n 's|^  \(d /var/.*\)$|\1|p' > "/usr/lib/tmpfiles.d/$1"
cat "/usr/lib/tmpfiles.d/$1"
