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

# Packages: GNOME desktop plus the ilex set. `#` comments are allowed in
# packages.txt. The dnf cache mount keeps /var/cache/libdnf5 out of the image,
# and a tmpfs keeps what scriptlets write to /run out of it too. Then, as
# bootc requires: drop caches and logs, and declare every /var directory the
# packages created in tmpfiles.d (bootc's linter reports exactly which ones
# lack an entry, with their modes and owners), so /var is recreated on boot
# instead of shipped.
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    --mount=type=tmpfs,target=/run \
    --mount=type=bind,source=packages.txt,target=/tmp/packages.txt \
    sed 's/#.*//' /tmp/packages.txt | xargs dnf -y install --setopt=keepcache=1 && \
    find /var/cache -mindepth 1 -maxdepth 1 ! -name libdnf5 -exec rm -rf {} + && \
    find /tmp /var/tmp -mindepth 1 -maxdepth 1 ! -path /tmp/packages.txt -exec rm -rf {} + && \
    rm -rf /var/log/* /var/lib/dnf/repos && \
    { bootc container lint --no-truncate 2>&1 || true; } \
      | sed -n 's|^  \(d /var/.*\)$|\1|p' > /usr/lib/tmpfiles.d/ilex-var.conf && \
    cat /usr/lib/tmpfiles.d/ilex-var.conf

COPY rootfs/ /
COPY --from=build /out/ /

RUN systemctl set-default graphical.target

RUN bootc container lint --fatal-warnings
