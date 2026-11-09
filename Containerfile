# ilex: a personal Fedora bootc image. Build with `cargo xtask build`.

# Builder: compile the Rust binaries on Fedora 45 so they link against the
# same glibc as the image.
FROM quay.io/fedora/fedora:45 AS build
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    dnf -y install --setopt=keepcache=1 cargo gcc

# kache: a compiler cache keyed by the content of each rustc invocation's
# inputs, so a crate built in any worktree (or on the host) is restored here
# instead of rebuilt. kache has no official container image, so take the
# release tarball for x86_64 Linux (static musl, runs on any glibc) and pin it
# by sha256. v2 is two executables, `kache` and `kache-engine`, which must sit
# side by side. To bump: change the version and the sha256
# (`gh release view vX.Y.Z -R kunobi-ninja/kache --json assets`).
# Checked with sha256sum rather than `ADD --checksum`, which the podman 4.9 on
# CI's Ubuntu runners rejects.
ARG KACHE_VERSION=2.0.0
ARG KACHE_SHA256=6c59665faf1b14d0944391e6f2a1b510195e6be92c63060a9d1770ed00676824
RUN curl -fsSL -o /tmp/kache.tar.gz \
      "https://github.com/kunobi-ninja/kache/releases/download/v${KACHE_VERSION}/kache-x86_64-unknown-linux-musl.tar.gz" && \
    echo "${KACHE_SHA256}  /tmp/kache.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/kache.tar.gz -C /usr/local/bin kache kache-engine && \
    rm /tmp/kache.tar.gz && \
    kache --version

WORKDIR /src
COPY . .
# Every compile goes through kache, whose store is a podman cache mount shared
# by all worktrees. There is no shared target directory any more: each build
# compiles into a fresh /src/target and kache restores what an earlier build
# (or the host) already compiled, by content rather than by mtime. `xtask
# build` also mounts the host's own kache store at /kache-host, read-only; it
# is used when present, so a plain `podman build` and CI work without it. The
# daemon is stopped before the step ends so it flushes what it queued.
RUN --mount=type=cache,id=ilex-cargo,target=/root/.cargo/registry \
    --mount=type=cache,id=ilex-kache,target=/root/.cache/kache,sharing=locked <<'EOF'
set -euo pipefail
export RUSTC_WRAPPER=kache
if [ -f /kache-host/index.db ]; then
    export KACHE_READONLY_STORE=/kache-host
fi
cargo build --release --locked -p ilex-agent
kache daemon stop || true
kache stats --last-build --root /src
install -Dm755 target/release/ilex-agent /out/usr/bin/ilex-agent
EOF

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
