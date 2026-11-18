#!/bin/bash
# Only in the disposable owned WHPX research overlay, after provisioning the
# ilexdev test user. Never a production image recipe or host/distro installer.
set -euo pipefail
test "${1:-}" = --disposable-research-vm
shift
stage_only=false
small_bootstrap=false
for option in "$@"; do
    case "$option" in
        --stage-only) stage_only=true ;;
        --small-bootstrap-mode) small_bootstrap=true ;;
        *) printf 'Unknown research option: %s\n' "$option" >&2; exit 2 ;;
    esac
done
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
cat >/var/home/ilexdev/.config/hypr/hyprland.lua <<'EOF'
hl.monitor({ output = "", mode = "preferred", position = "auto", scale = 1 })
hl.config({
    input = { kb_layout = "us" },
    misc = { disable_hyprland_logo = true, disable_splash_rendering = true },
})
hl.bind("SUPER + E", hl.dsp.exec_cmd("/var/home/ilexdev/.local/bin/ilex-t4-super-e"))
hl.on("hyprland.start", function()
    hl.exec_cmd("dbus-update-activation-environment --systemd WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE XDG_CURRENT_DESKTOP")
end)
EOF
chown ilexdev:ilexdev /var/home/ilexdev/.config/hypr/hyprland.lua
if "$small_bootstrap"; then
    sed -i 's/mode = "preferred"/mode = "1280x800@74.99"/' /var/home/ilexdev/.config/hypr/hyprland.lua
fi
cat >/usr/share/wayland-sessions/ilex-t4-hyprland.desktop <<'EOF'
[Desktop Entry]
Name=ilex T4 Hyprland research
Comment=Disposable Fedora test overlay
Exec=env XDG_CURRENT_DESKTOP=Hyprland Hyprland
Type=Application
DesktopNames=Hyprland
EOF
account_path="/org/freedesktop/Accounts/User$(id -u ilexdev)"
gdbus call --system --dest org.freedesktop.Accounts --object-path "$account_path" \
    --method org.freedesktop.Accounts.User.SetSessionType wayland
for method in SetSession SetXSession; do
    gdbus call --system --dest org.freedesktop.Accounts --object-path "$account_path" \
        --method "org.freedesktop.Accounts.User.$method" ilex-t4-hyprland
done
printf '[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin=ilexdev\nDefaultSession=ilex-t4-hyprland.desktop\n' >/etc/gdm/custom.conf
rpm -q hyprland qt6-qtbase qt6-qtdeclarative qt6-qtwayland wl-clipboard
if "$stage_only"; then
    printf 'Session staged; restart GDM through the session regression fixture\n'
else
    systemctl restart gdm
fi
