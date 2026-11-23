# Disposable CI update payload. Published images keep their original kernel.
ARG BASE
FROM ${BASE}
ARG KERNEL
RUN set -eux; \
    dnf -y --setopt=installonly_limit=0 install "kernel-$KERNEL" "kernel-core-$KERNEL" \
        "kernel-modules-$KERNEL" "kernel-modules-core-$KERNEL" "kernel-modules-extra-$KERNEL"; \
    test "$(rpm -q --qf '%{VERSION}-%{RELEASE}.%{ARCH}' kernel-core)" = "$KERNEL"; \
    mkdir -p "$(realpath /root)"; \
    DRACUT_NO_XATTR=1 dracut --force --zstd --reproducible --no-hostonly \
        "/usr/lib/modules/$KERNEL/initramfs.img" "$KERNEL"; \
    lsinitrd "/usr/lib/modules/$KERNEL/initramfs.img" | grep -F ilex-enroll; \
    rmdir "$(realpath /root)"; \
    dnf clean all; \
    rm -rf /run/dnf /var/lib/dnf/repos /var/lib/rpm-state/kernel; \
    rm -f "/boot/symvers-$KERNEL.xz" /var/log/dnf5.log /var/cache/ldconfig/aux-cache; \
    bootc container lint --fatal-warnings
