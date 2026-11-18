#!/bin/bash
# Only in the disposable owned WHPX research overlay, after provisioning the
# ilexdev test user. Never a production image recipe or host/distro installer.
set -euo pipefail
test "${1:-}" = --disposable-research-vm
test "$(id -u)" = 0
test "$(rpm -E %fedora)" = 45
test -f /sys/firmware/qemu_fw_cfg/by_name/opt/ilex/ssh-host-key/raw
test -x /usr/libexec/ilex-dev-session
test -d /var/home/ilexdev
bootc usr-overlay
curl --fail --location --silent --show-error \
    https://copr.fedorainfracloud.org/coprs/lionheartp/Hyprland/repo/fedora-45/lionheartp-Hyprland-fedora-45.repo \
    --output /etc/yum.repos.d/ilex-t4-hyprland.repo
dnf -y install hyprland qt6-qtdeclarative-devel qt6-qtwayland wl-clipboard glx-utils
install -d -o ilexdev -g ilexdev /var/home/ilexdev/.config/hypr
install -d -o ilexdev -g ilexdev /var/home/ilexdev/.local/bin
cat >/var/home/ilexdev/.local/bin/ilex-t4-super-e <<'EOF'
#!/bin/sh
printf '1\n' >> "$XDG_RUNTIME_DIR/ilex-t4-super-e"
EOF
chmod 0755 /var/home/ilexdev/.local/bin/ilex-t4-super-e
chown ilexdev:ilexdev /var/home/ilexdev/.local/bin/ilex-t4-super-e
cat >/var/home/ilexdev/.config/hypr/hyprland.conf <<'EOF'
monitor = , preferred, auto, 1
exec-once = dbus-update-activation-environment --systemd WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE XDG_CURRENT_DESKTOP
bind = SUPER, E, exec, /var/home/ilexdev/.local/bin/ilex-t4-super-e
input {
    kb_layout = us
}
misc {
    disable_hyprland_logo = true
    disable_splash_rendering = true
}
EOF
chown ilexdev:ilexdev /var/home/ilexdev/.config/hypr/hyprland.conf
cat >/usr/share/wayland-sessions/ilex-t4-hyprland.desktop <<'EOF'
[Desktop Entry]
Name=ilex T4 Hyprland research
Comment=Disposable Fedora test overlay
Exec=env XDG_CURRENT_DESKTOP=Hyprland Hyprland
Type=Application
DesktopNames=Hyprland
EOF
install -d /var/lib/AccountsService/users
printf '[User]\nSession=ilex-t4-hyprland\nXSession=ilex-t4-hyprland\nSystemAccount=false\n' >/var/lib/AccountsService/users/ilexdev
printf '[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin=ilexdev\nDefaultSession=ilex-t4-hyprland.desktop\n' >/etc/gdm/custom.conf
rpm -q hyprland qt6-qtbase qt6-qtdeclarative qt6-qtwayland wl-clipboard
systemctl restart gdm
