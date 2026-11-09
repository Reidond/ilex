# ilex: a personal Fedora bootc image. Build with `cargo xtask build`.

# Builder: compile the Rust binaries on Fedora 45 so they link against the
# same glibc as the image.
FROM quay.io/fedora/fedora:45 AS build
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    dnf -y install --setopt=keepcache=1 cargo gcc
WORKDIR /src
COPY . .
# The target cache is shared by every worktree, and cargo trusts mtimes: a
# checkout with older sources would look fresh and ship another worktree's
# binaries. Touching our own sources forces our crates to rebuild (deps
# stay cached).
RUN --mount=type=cache,id=ilex-cargo,target=/root/.cargo/registry \
    --mount=type=cache,id=ilex-target,target=/src/target,sharing=locked \
    find crates -type f -exec touch {} + && \
    cargo build --release --locked -p ilex-agent && \
    install -Dm755 target/release/ilex-agent /out/usr/bin/ilex-agent

FROM quay.io/fedora/fedora-bootc:45

# Packages: the tables in packages.toml (GNOME desktop for now). The dnf
# cache mount keeps /var/cache/libdnf5 out of the image, and a tmpfs keeps
# what scriptlets write to /run out of it too. Then, as bootc requires: drop
# caches and log files, and declare every /var directory the packages
# created in tmpfiles.d (bootc's linter reports exactly which ones lack an
# entry, with their modes and owners), so /var is recreated on boot,
# SELinux-labeled, instead of shipped. Log directories are kept for that
# reason: a daemon that has to create its own (tuned) gets var_log_t.
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    --mount=type=tmpfs,target=/run \
    --mount=type=bind,source=packages.toml,target=/tmp/packages.toml <<'EOF'
set -euo pipefail

# packages.toml -> dnf arguments: every table's `install`, then its
# `exclude` as --exclude=. Read with Python's stdlib TOML parser; unknown
# keys are an error rather than silently ignored.
python3 - /tmp/packages.toml > /tmp/dnf-args <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    tables = tomllib.load(f)
install, exclude = [], []
for name, table in tables.items():
    if not isinstance(table, dict):
        sys.exit(f"packages.toml: `{name}` must be a table like [{name}]")
    unknown = sorted(set(table) - {"install", "exclude"})
    if unknown:
        sys.exit(f"packages.toml: [{name}] has unknown keys: {', '.join(unknown)}")
    install += table.get("install", [])
    exclude += [f"--exclude={p}" for p in table.get("exclude", [])]
print("\n".join(install + exclude))
PY
xargs dnf -y install --setopt=keepcache=1 < /tmp/dnf-args

find /var/cache -mindepth 1 -maxdepth 1 ! -name libdnf5 -exec rm -rf {} +
find /tmp /var/tmp -mindepth 1 -maxdepth 1 ! -path /tmp/packages.toml -exec rm -rf {} +
find /var/log -mindepth 1 ! -type d -delete
rm -rf /var/lib/dnf/repos
{ bootc container lint --no-truncate 2>&1 || true; } \
  | sed -n 's|^  \(d /var/.*\)$|\1|p' > /usr/lib/tmpfiles.d/ilex-var.conf
cat /usr/lib/tmpfiles.d/ilex-var.conf
EOF

COPY rootfs/ /
COPY --from=build /out/ /

# Boot to GDM; let bootc's updater stage new images daily (see the drop-ins
# in rootfs), and offer a restart from every user's session once one is staged.
# chmod: checkouts on Windows filesystems carry no exec bits.
RUN chmod 0755 /usr/libexec/ilex/update-notify && \
    systemctl set-default graphical.target && \
    systemctl preset bootc-fetch-apply-updates.timer && \
    systemctl --global enable ilex-update-notify.path ilex-update-notify.service

RUN bootc container lint --fatal-warnings
