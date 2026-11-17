# ilex: a personal Fedora bootc image. Build with `cargo xtask build`.

# First, so a bad manifest fails in seconds, before anything compiles.
FROM quay.io/fedora/fedora-bootc:45 AS compose
# Empty: image.toml decides. `none`: no shell. Otherwise comma-separated.
ARG ILEX_SHELLS=""
ARG ILEX_DEFAULT_SHELL=""
# COPY inputs: buildah does not reliably invalidate RUN bind mounts.
# One linked layer avoids committing the large bootc base per input directory;
# --parents keeps the manifest layout intact. Both work with buildah 1.42.
COPY --link --parents image.toml packages.toml build/ shells/ design-systems/ users/ machines/generic/ /ctx/
RUN python3 -I -S -c 'import runpy,sys; sys.path.insert(0,"/ctx/build"); runpy.run_path("/ctx/build/compose.py",run_name="__main__")' image --context /ctx --out /out \
        --shells "${ILEX_SHELLS}" --default-shell "${ILEX_DEFAULT_SHELL}"

# Fedora's homed enumeration counts all shadow entries against a 50-human
# limit. Build only its daemon with the one-line fix from the exact Fedora
# SRPM. Runtime libraries/data remain Fedora's; installation below refuses
# another RPM version so a future update cannot silently retain this backport.
FROM quay.io/fedora/fedora:45 AS accountsservice-build
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    dnf -y install --setopt=keepcache=1 gcc meson ninja-build gettext-devel \
        glib2-devel polkit-devel systemd-devel json-c-devel dbus-devel \
        libxcrypt-devel rpm-build cpio bzip2 patch
WORKDIR /src
ARG ACCOUNTS_SRPM_SHA256=394b5016ef1a895b2c8ecf348be692830b483fe6f4612a771c0d6b764f2171e6
RUN curl -fsSL -o accountsservice.src.rpm \
        https://kojipkgs.fedoraproject.org/packages/accountsservice/26.27.3/2.fc45/src/accountsservice-26.27.3-2.fc45.src.rpm && \
    echo "${ACCOUNTS_SRPM_SHA256}  accountsservice.src.rpm" | sha256sum -c - && \
    rpm2cpio accountsservice.src.rpm | cpio -idm --no-absolute-filenames && \
    tar -xjf accountsservice-26.27.3.tar.bz2
COPY build/accountsservice-homed.patch /src/
RUN cd accountsservice-26.27.3 && \
    patch --batch --fuzz=0 -p1 < /src/accountsservice-homed.patch && \
    CFLAGS="$(rpm --eval '%{build_cflags}')" \
    CXXFLAGS="$(rpm --eval '%{build_cxxflags}')" \
    LDFLAGS="$(rpm --eval '%{build_ldflags}')" \
    meson setup _build --buildtype=plain --prefix=/usr --libexecdir=/usr/libexec \
        --sysconfdir=/etc --localstatedir=/var -Dadmin_group=wheel \
        -Dtests=false -Dintrospection=false -Dvapi=false -Dgtk_doc=false -Ddocbook=false && \
    ninja -C _build src/accounts-daemon && \
    install -Dm755 _build/src/accounts-daemon /out/accounts-daemon && \
    strip /out/accounts-daemon && \
    install -Dm644 /src/accountsservice-homed.patch /out/accountsservice-homed.patch

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
# Fedora's cargo is used directly; rust-toolchain.toml configures rustup on
# the host, and .cargo/config.toml contains only the xtask alias. All embedded
# source files (including tool-build.sh) live below crates/; there are no Cargo
# build scripts.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
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
cargo build --release --locked -p ilexctl --no-default-features
cargo build --release --locked -p drupe
kache daemon stop || true
kache stats --last-build --root /src
install -Dm755 target/release/ilexctl /out/usr/bin/ilexctl
for shell in bash zsh fish; do
    case "$shell" in
        bash) completion=/usr/share/bash-completion/completions/ilexctl ;;
        zsh) completion=/usr/share/zsh/site-functions/_ilexctl ;;
        fish) completion=/usr/share/fish/vendor_completions.d/ilexctl.fish ;;
    esac
    mkdir -p "/out$(dirname "$completion")"
    target/release/ilexctl completion "$shell" > "/out$completion"
    chmod 0644 "/out$completion"
done
install -Dm755 target/release/drupe /out/usr/bin/drupe
install -Dm755 target/release/drupe-enter /out/usr/libexec/drupe/drupe-enter
EOF

FROM quay.io/fedora/fedora-bootc:45

# Base packages: the same transaction under every shell selection. The dnf
# cache mount keeps /var/cache/libdnf5 out of the image, and a tmpfs keeps
# what scriptlets write to /run out of it too. Then, as bootc requires: drop
# caches and log files, and declare every /var directory the packages
# created in tmpfiles.d (bootc's linter reports exactly which ones lack an
# entry, with their modes and owners), so /var is recreated on boot,
# SELinux-labeled, instead of shipped. Log directories are kept for that
# reason: a daemon that has to create its own (tuned) gets var_log_t.
COPY --from=compose /out/build/base/ /tmp/ilex-build/
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    --mount=type=tmpfs,target=/run \
    bash /tmp/ilex-build/install-packages.sh ilex-var.conf

# Shells share one transaction and its excludes, in a separate image layer.
# With no shell this payload is empty: no dnf and no tmpfiles file.
COPY --from=compose /out/build/shells/ /tmp/ilex-build/
RUN --mount=type=cache,id=ilex-dnf,target=/var/cache/libdnf5,sharing=locked \
    --mount=type=tmpfs,target=/run \
    bash /tmp/ilex-build/install-packages.sh ilex-var-shells.conf

COPY rootfs/ /
COPY --from=build /out/ /
COPY --from=accountsservice-build /out/ /tmp/ilex-accountsservice/
# Ordinary COPY preserves existing ancestor modes (in particular /tmp's 1777)
# and still keys the payload by content. --link would replace those ancestors.
COPY --from=compose /out/install/ /tmp/ilex-compose/
# Generated files precede configuration; overlays are applied only afterward.
# Both helpers use only stdlib: -I -S skips overlay modules and site hooks.
RUN python3 -I -S /tmp/ilex-compose/install-files.py --payload /tmp/ilex-compose --base --phase generated

# Boot to a graphical target when a shell is installed; let bootc's updater
# stage new images daily (see the drop-ins in rootfs), and offer a restart
# from every user's session once one is staged.
# ilex-sigpolicy pins a machine that tracks ghcr.io/reidond/ilex to signed
# images at boot, however it was installed (ilexctl system enforce-sigpolicy).
# chmod: checkouts on Windows filesystems carry no exec bits.
# drupe (developer tools from signed disk images): /app is where a tool's
# image appears in the mount namespace of its own processes, an empty
# directory everywhere else; drupe-enter is setuid root because it makes that
# namespace and mount (and nothing else: see crates/drupe/src/enter.rs);
# drupe.service mounts the installed images at boot.
# Authselect's image checksum belongs in its supported vendor location;
# /var/lib/authselect/checksum is mutable state and cannot ship in bootc.
RUN chmod 0755 /usr/libexec/ilex/update-notify && \
    mkdir /app && \
    chmod 4755 /usr/libexec/drupe/drupe-enter && \
    systemctl set-default "$(python3 -I -S -c 'import json; print("graphical.target" if json.load(open("/usr/share/ilex/image.json"))["shells"] else "multi-user.target")')" && \
    systemctl preset bootc-fetch-apply-updates.timer ilex-sigpolicy.service drupe.service systemd-homed.service && \
    systemctl --global enable ilex-profile.service && \
    authselect enable-feature with-systemd-homed && \
    mv /var/lib/authselect/checksum /usr/share/authselect/checksum && \
    authselect check && \
    semodule -n -i /usr/share/selinux/packages/ilex-homed.cil && \
    if rpm -q accountsservice >/dev/null 2>&1; then \
        test "$(rpm -q --qf '%{VERSION}-%{RELEASE}' accountsservice)" = 26.27.3-2.fc45 && \
        install -m755 /tmp/ilex-accountsservice/accounts-daemon /usr/libexec/accounts-daemon && \
        install -Dm644 /tmp/ilex-accountsservice/accountsservice-homed.patch \
            /usr/share/doc/ilex/accountsservice-homed.patch; \
    fi && \
    rm -rf /tmp/ilex-accountsservice && \
    if [ "$(systemctl get-default)" = graphical.target ]; then \
        systemctl --global enable ilex-update-notify.path ilex-update-notify.service; \
    fi && \
    for directory in /tmp /var/tmp; do \
        actual="$(stat -c '%a %u:%g' "$directory")"; \
        if [ "$actual" != '1777 0:0' ]; then \
            echo "image metadata: $directory: expected 1777 0:0, found $actual" >&2; \
            exit 1; \
        fi; \
    done

# The final mutation applies additions against the real image and removes its
# own staging in the same interpreter. Only bootc lint runs after overlays.
RUN python3 -I -S /tmp/ilex-compose/install-files.py --payload /tmp/ilex-compose --phase overlays --remove-payload

RUN bootc container lint --fatal-warnings
