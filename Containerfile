# ilex: a personal Fedora bootc image. Build with `cargo xtask build`.

# Builder: compile the Rust binaries on Fedora 45 so they link against the
# same glibc as the image.
FROM quay.io/fedora/fedora:45 AS build
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    dnf -y install --setopt=keepcache=1 cargo gcc
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=ilex-cargo,target=/root/.cargo/registry \
    --mount=type=cache,id=ilex-target,target=/src/target,sharing=locked \
    cargo build --release --locked -p ilex-agent && \
    install -Dm755 target/release/ilex-agent /out/usr/bin/ilex-agent

FROM quay.io/fedora/fedora-bootc:45

# Packages: GNOME desktop plus the ilex set. `#` comments are allowed in
# packages.txt. The dnf cache mount keeps /var/cache/libdnf5 out of the image.
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    --mount=type=bind,source=packages.txt,target=/tmp/packages.txt \
    sed 's/#.*//' /tmp/packages.txt | xargs dnf -y install --setopt=keepcache=1 && \
    rm -rf /var/log/* /var/lib/dnf/repos && \
    find /tmp /var/tmp -mindepth 1 -maxdepth 1 ! -path /tmp/packages.txt -exec rm -rf {} +

COPY rootfs/ /
COPY --from=build /out/ /

RUN systemctl set-default graphical.target

RUN bootc container lint --fatal-warnings
